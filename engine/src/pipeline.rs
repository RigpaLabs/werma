use anyhow::Result;

use crate::db::Db;
use crate::linear::LinearClient;
use crate::models::{Status, Task};

/// Pipeline stages and their corresponding agent types.
pub fn agent_for_stage(stage: &str) -> &str {
    match stage {
        "analyst" => "pipeline-analyst",
        "engineer" => "pipeline-engineer",
        "reviewer" => "pipeline-reviewer",
        "qa" => "pipeline-qa",
        "devops" => "pipeline-devops",
        _ => "research",
    }
}

/// Model for each pipeline stage.
pub fn model_for_stage(stage: &str) -> &str {
    match stage {
        "analyst" | "engineer" => "opus",
        _ => "sonnet",
    }
}

/// Extract verdict from result text.
/// Looks for patterns like VERDICT=APPROVED, REVIEW_VERDICT=APPROVED, etc.
/// Returns None if no verdict found (critical fix from bash version).
pub fn parse_verdict(result: &str) -> Option<String> {
    // Look for explicit verdict patterns (last match wins)
    let patterns = [
        "VERDICT=",
        "REVIEW_VERDICT=",
        "QA_VERDICT=",
        "DEPLOY_VERDICT=",
        "FIX_VERDICT=",
    ];

    let mut found: Option<String> = None;

    for line in result.lines() {
        let line = line.trim();
        for pattern in &patterns {
            if let Some(rest) = line.strip_prefix(pattern).or_else(|| {
                // Also check within the line
                line.find(pattern).map(|pos| &line[pos + pattern.len()..])
            }) {
                let verdict = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                if !verdict.is_empty() {
                    found = Some(verdict.to_uppercase());
                }
            }
        }
    }

    // Also check for standalone APPROVED/REJECTED keywords in the last 10 lines
    if found.is_none() {
        let last_lines: Vec<&str> = result.lines().rev().take(10).collect();
        for line in &last_lines {
            let upper = line.trim().to_uppercase();
            if upper.contains("APPROVED") && !upper.contains("NOT APPROVED") {
                return Some("APPROVED".to_string());
            }
            if upper.contains("REJECTED") || upper.contains("REQUEST_CHANGES") {
                return Some("REJECTED".to_string());
            }
            if upper.contains("PASSED") && !upper.contains("NOT PASSED") {
                return Some("PASSED".to_string());
            }
            if upper.contains("FAILED") {
                return Some("FAILED".to_string());
            }
        }
    }

    found
}

/// Status key mappings for pipeline stages.
struct StageConfig {
    status_key: &'static str,
    stage: &'static str,
}

const POLL_STAGES: &[StageConfig] = &[
    StageConfig {
        status_key: "todo",
        stage: "analyst",
    },
    StageConfig {
        status_key: "review",
        stage: "reviewer",
    },
    StageConfig {
        status_key: "qa",
        stage: "qa",
    },
    StageConfig {
        status_key: "ready",
        stage: "devops",
    },
    StageConfig {
        status_key: "deploy",
        stage: "devops",
    },
];

/// Poll Linear for issues at pipeline-relevant statuses and create tasks.
pub fn poll(db: &Db) -> Result<()> {
    let linear = LinearClient::new()?;

    let mut total_created = 0;
    let mut total_skipped = 0;

    for stage_config in POLL_STAGES {
        let issues = linear.get_issues_by_status(stage_config.status_key)?;

        for issue in &issues {
            let issue_id = issue["id"].as_str().unwrap_or("");
            let identifier = issue["identifier"].as_str().unwrap_or("");
            let title = issue["title"].as_str().unwrap_or("");
            let description = issue["description"].as_str().unwrap_or("");

            if issue_id.is_empty() {
                continue;
            }

            // Skip if active task already exists for this issue + stage
            let existing = db.tasks_by_linear_issue(issue_id, Some(stage_config.stage), true)?;
            if !existing.is_empty() {
                total_skipped += 1;
                continue;
            }

            let agent_type = agent_for_stage(stage_config.stage);
            let model = model_for_stage(stage_config.stage);

            // Get labels for working_dir inference and filtering
            let labels: Vec<&str> = issue["labels"]["nodes"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l["name"].as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            // Skip manual issues — human-driven, agents must not pick up
            if crate::linear::is_manual_issue(&labels) {
                total_skipped += 1;
                continue;
            }

            let working_dir = crate::linear::infer_working_dir(title, &labels);

            // Build prompt
            let prompt = format!(
                "[{}] {}\n\nStage: {}\n\n{}",
                identifier, title, stage_config.stage, description
            );

            let task_id = db.next_task_id()?;
            let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

            let max_turns = crate::default_turns(agent_type);
            let allowed_tools = crate::runner::tools_for_type(agent_type, false);

            let task = Task {
                id: task_id.clone(),
                status: Status::Pending,
                priority: 1,
                created_at: now,
                started_at: None,
                finished_at: None,
                task_type: agent_type.to_string(),
                prompt,
                output_path: String::new(),
                working_dir,
                model: model.to_string(),
                max_turns,
                allowed_tools,
                session_id: String::new(),
                linear_issue_id: issue_id.to_string(),
                linear_pushed: false,
                pipeline_stage: stage_config.stage.to_string(),
                depends_on: vec![],
                context_files: vec![],
            };

            db.insert_task(&task)?;
            println!(
                "  + {} [{}] stage={} type={}",
                task_id, identifier, stage_config.stage, agent_type
            );
            total_created += 1;
        }
    }

    println!(
        "\nPipeline poll: {} created, {} skipped",
        total_created, total_skipped
    );
    Ok(())
}

/// Handle pipeline callback when a task completes.
/// Called from the task completion hook.
pub fn callback(
    db: &Db,
    task_id: &str,
    stage: &str,
    result: &str,
    linear_issue_id: &str,
) -> Result<()> {
    let linear = LinearClient::new()?;
    let verdict = parse_verdict(result);

    // CRITICAL FIX: empty verdict should NOT auto-approve
    if verdict.is_none() && stage != "engineer" && stage != "analyst" {
        eprintln!(
            "warning: no verdict found for task {} (stage={}), keeping current state",
            task_id, stage
        );
        linear.comment(
            linear_issue_id,
            &format!(
                "**Werma task `{}`** (stage: {}) completed but no verdict found. Manual review needed.",
                task_id, stage
            ),
        )?;
        return Ok(());
    }

    let verdict_str = verdict.as_deref().unwrap_or("");

    match stage {
        "analyst" => {
            // Analyst completed → move to In Progress, create engineer task
            linear.move_issue_by_name(linear_issue_id, "in_progress")?;
            linear.comment(
                linear_issue_id,
                &format!(
                    "**Analyst completed** (task: `{}`). Spec posted. Moving to In Progress.",
                    task_id
                ),
            )?;

            // Create engineer task
            create_next_stage_task(db, linear_issue_id, "engineer", result)?;
        }

        "engineer" => {
            // Engineer completed → move to In Review
            // DON'T create another engineer task (bug fix from bash version)
            linear.move_issue_by_name(linear_issue_id, "review")?;
            linear.comment(
                linear_issue_id,
                &format!(
                    "**Engineer completed** (task: `{}`). Moving to In Review.",
                    task_id
                ),
            )?;
        }

        "reviewer" => match verdict_str {
            "APPROVED" | "PASSED" => {
                linear.move_issue_by_name(linear_issue_id, "qa")?;
                linear.comment(
                    linear_issue_id,
                    &format!("**Review APPROVED** (task: `{}`). Moving to QA.", task_id),
                )?;
            }
            "REJECTED" | "REQUEST_CHANGES" => {
                linear.move_issue_by_name(linear_issue_id, "in_progress")?;
                linear.comment(
                    linear_issue_id,
                    &format!(
                        "**Review: CHANGES REQUESTED** (task: `{}`). Moving back to In Progress.",
                        task_id
                    ),
                )?;
                create_next_stage_task(db, linear_issue_id, "engineer", result)?;
            }
            _ => {
                // No verdict for reviewer — already handled above, but just in case
                eprintln!("reviewer: unexpected verdict '{}'", verdict_str);
            }
        },

        "qa" => match verdict_str {
            "APPROVED" | "PASSED" => {
                linear.move_issue_by_name(linear_issue_id, "ready")?;
                linear.comment(
                    linear_issue_id,
                    &format!(
                        "**QA PASSED** (task: `{}`). Moving to Ready for Deploy.",
                        task_id
                    ),
                )?;
            }
            "REJECTED" | "FAILED" => {
                linear.move_issue_by_name(linear_issue_id, "in_progress")?;
                linear.comment(
                    linear_issue_id,
                    &format!(
                        "**QA FAILED** (task: `{}`). Moving back to In Progress.",
                        task_id
                    ),
                )?;
                create_next_stage_task(db, linear_issue_id, "engineer", result)?;
            }
            _ => {
                eprintln!("qa: unexpected verdict '{}'", verdict_str);
            }
        },

        "devops" => {
            if verdict_str == "FAILED" {
                linear.move_issue_by_name(linear_issue_id, "failed")?;
                linear.comment(
                    linear_issue_id,
                    &format!(
                        "**DEPLOY FAILED** (task: `{}`). Moving to Deploy Failed.",
                        task_id
                    ),
                )?;
            } else {
                // OK or completed without explicit failure
                linear.move_issue_by_name(linear_issue_id, "done")?;
                linear.comment(
                    linear_issue_id,
                    &format!("**DEPLOYED** (task: `{}`). Issue complete.", task_id),
                )?;
            }
        }

        _ => {
            eprintln!("unknown pipeline stage: {}", stage);
        }
    }

    Ok(())
}

/// Create a task for the next pipeline stage.
fn create_next_stage_task(
    db: &Db,
    linear_issue_id: &str,
    next_stage: &str,
    _context: &str,
) -> Result<()> {
    let agent_type = agent_for_stage(next_stage);
    let model = model_for_stage(next_stage);
    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let max_turns = crate::default_turns(agent_type);
    let allowed_tools = crate::runner::tools_for_type(agent_type, false);

    let prompt = format!(
        "Continue pipeline for Linear issue {}. Stage: {}",
        linear_issue_id, next_stage
    );

    let task = Task {
        id: task_id.clone(),
        status: Status::Pending,
        priority: 1,
        created_at: now,
        started_at: None,
        finished_at: None,
        task_type: agent_type.to_string(),
        prompt,
        output_path: String::new(),
        working_dir: "~/projects/ar".to_string(),
        model: model.to_string(),
        max_turns,
        allowed_tools,
        session_id: String::new(),
        linear_issue_id: linear_issue_id.to_string(),
        linear_pushed: false,
        pipeline_stage: next_stage.to_string(),
        depends_on: vec![],
        context_files: vec![],
    };

    db.insert_task(&task)?;
    println!(
        "  + pipeline task: {} stage={} type={}",
        task_id, next_stage, agent_type
    );

    Ok(())
}

/// Show pipeline status: count issues at each stage.
pub fn status(db: &Db) -> Result<()> {
    println!("\nPipeline Status:");
    println!("================\n");

    let linear = match LinearClient::new() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("  WARNING: Linear not available — {e}");
            eprintln!("  Pipeline poll/sync disabled until LINEAR_API_KEY is set.\n");
            None
        }
    };

    if let Some(ref linear) = linear {
        let stages = [
            ("backlog", "Backlog"),
            ("todo", "Todo"),
            ("in_progress", "In Progress"),
            ("review", "In Review"),
            ("qa", "QA"),
            ("ready", "Ready for Deploy"),
            ("deploy", "Deploying"),
            ("done", "Done"),
            ("failed", "Deploy Failed"),
        ];

        for (key, label) in &stages {
            match linear.get_issues_by_status(key) {
                Ok(issues) => {
                    if !issues.is_empty() {
                        println!("  {} ({}): {} issues", label, key, issues.len());
                        for issue in &issues {
                            let id = issue["identifier"].as_str().unwrap_or("?");
                            let title = issue["title"].as_str().unwrap_or("?");
                            println!("    {} {}", id, title);
                        }
                    }
                }
                Err(_) => {
                    println!("  {} ({}): <error fetching>", label, key);
                }
            }
        }
    }

    // Show local pipeline tasks
    println!("\nLocal pipeline tasks:");
    let pipeline_stages = ["analyst", "engineer", "reviewer", "qa", "devops"];
    for stage in &pipeline_stages {
        let pending = db.list_tasks(Some(Status::Pending))?;
        let running = db.list_tasks(Some(Status::Running))?;

        let stage_pending: Vec<_> = pending
            .iter()
            .filter(|t| t.pipeline_stage == *stage)
            .collect();
        let stage_running: Vec<_> = running
            .iter()
            .filter(|t| t.pipeline_stage == *stage)
            .collect();

        if !stage_pending.is_empty() || !stage_running.is_empty() {
            println!(
                "  {}: {} pending, {} running",
                stage,
                stage_pending.len(),
                stage_running.len()
            );
        }
    }

    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_to_agent_mapping() {
        assert_eq!(agent_for_stage("analyst"), "pipeline-analyst");
        assert_eq!(agent_for_stage("engineer"), "pipeline-engineer");
        assert_eq!(agent_for_stage("reviewer"), "pipeline-reviewer");
        assert_eq!(agent_for_stage("qa"), "pipeline-qa");
        assert_eq!(agent_for_stage("devops"), "pipeline-devops");
        assert_eq!(agent_for_stage("unknown"), "research");
    }

    #[test]
    fn stage_to_model_mapping() {
        assert_eq!(model_for_stage("analyst"), "opus");
        assert_eq!(model_for_stage("engineer"), "opus");
        assert_eq!(model_for_stage("reviewer"), "sonnet");
        assert_eq!(model_for_stage("qa"), "sonnet");
        assert_eq!(model_for_stage("devops"), "sonnet");
        assert_eq!(model_for_stage("unknown"), "sonnet");
    }

    #[test]
    fn verdict_parsing_explicit() {
        assert_eq!(
            parse_verdict("REVIEW_VERDICT=APPROVED"),
            Some("APPROVED".to_string())
        );
        assert_eq!(
            parse_verdict("VERDICT=REJECTED"),
            Some("REJECTED".to_string())
        );
        assert_eq!(
            parse_verdict("QA_VERDICT=PASSED"),
            Some("PASSED".to_string())
        );
        assert_eq!(
            parse_verdict("QA_VERDICT=FAILED"),
            Some("FAILED".to_string())
        );
        assert_eq!(parse_verdict("DEPLOY_VERDICT=OK"), Some("OK".to_string()));
        assert_eq!(
            parse_verdict("FIX_VERDICT=FIXED"),
            Some("FIXED".to_string())
        );
    }

    #[test]
    fn verdict_parsing_within_text() {
        let text = "Some output here\nAll checks passed\nREVIEW_VERDICT=APPROVED\nDone.";
        assert_eq!(parse_verdict(text), Some("APPROVED".to_string()));
    }

    #[test]
    fn verdict_parsing_keyword_fallback() {
        let text = "Everything looks good.\nAPPROVED";
        assert_eq!(parse_verdict(text), Some("APPROVED".to_string()));

        let text2 = "Found issues.\nREJECTED";
        assert_eq!(parse_verdict(text2), Some("REJECTED".to_string()));

        let text3 = "All tests pass.\nPASSED";
        assert_eq!(parse_verdict(text3), Some("PASSED".to_string()));
    }

    #[test]
    fn verdict_parsing_empty_no_verdict() {
        // CRITICAL: empty/no verdict should return None, NOT auto-approve
        assert_eq!(parse_verdict(""), None);
        assert_eq!(
            parse_verdict("Some random output without any verdict keywords"),
            None
        );
        assert_eq!(
            parse_verdict("Task completed successfully.\nAll done."),
            None
        );
    }

    #[test]
    fn verdict_parsing_not_approved() {
        // "NOT APPROVED" should not match as APPROVED
        assert_eq!(
            parse_verdict("The changes are NOT APPROVED due to issues."),
            None
        );
    }

    #[test]
    fn verdict_last_match_wins() {
        let text = "VERDICT=FAILED\nAfter fixes:\nVERDICT=APPROVED";
        assert_eq!(parse_verdict(text), Some("APPROVED".to_string()));
    }
}
