use anyhow::{Context, Result};

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

/// Whether a stage is an execution stage (human does the work for `manual` issues).
/// Review and QA stages are NOT execution — agents should review regardless.
fn is_execution_stage(stage: &str) -> bool {
    matches!(stage, "analyst" | "engineer" | "devops")
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

/// Check if an issue has the `research` label.
fn is_research_issue(labels: &[&str]) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case("research"))
}

/// Poll Linear for issues at pipeline-relevant statuses and create tasks.
pub fn poll(db: &Db) -> Result<()> {
    let linear = LinearClient::new()?;

    let mut total_created = 0;
    let mut total_skipped = 0;

    // Research issues in Todo → create research task (not pipeline task)
    let todo_issues = linear.get_issues_by_status("todo")?;
    for issue in &todo_issues {
        let issue_id = issue["id"].as_str().unwrap_or("");
        let identifier = issue["identifier"].as_str().unwrap_or("");
        let title = issue["title"].as_str().unwrap_or("");
        let description = issue["description"].as_str().unwrap_or("");

        if issue_id.is_empty() {
            continue;
        }

        let labels: Vec<&str> = issue["labels"]["nodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if !is_research_issue(&labels) {
            continue; // Not a research issue — handled by standard pipeline below
        }

        // Skip manual research issues — human does the research
        if crate::linear::is_manual_issue(&labels) {
            total_skipped += 1;
            continue;
        }

        // Skip if active task already exists for this issue
        let existing = db.tasks_by_linear_issue(issue_id, None, true)?;
        if !existing.is_empty() {
            total_skipped += 1;
            continue;
        }

        let working_dir = crate::linear::infer_working_dir(title, &labels);
        let prompt = format!(
            "[{}] {}\n\n{}\n\nSave the research output as a markdown file in docs/research/. \
             On the last line of your output, write: OUTPUT_FILE=<path-to-saved-file>",
            identifier, title, description
        );

        let task_id = db.next_task_id()?;
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let max_turns = crate::default_turns("research");
        let allowed_tools = crate::runner::tools_for_type("research", false);

        let task = Task {
            id: task_id.clone(),
            status: Status::Pending,
            priority: 2,
            created_at: now,
            started_at: None,
            finished_at: None,
            task_type: "research".to_string(),
            prompt,
            output_path: String::new(),
            working_dir,
            model: "sonnet".to_string(),
            max_turns,
            allowed_tools,
            session_id: String::new(),
            linear_issue_id: issue_id.to_string(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: crate::runtime_repo_hash(),
        };

        db.insert_task(&task)?;
        // Move to In Progress so it doesn't get picked up again
        let _ = linear.move_issue_by_name(issue_id, "in_progress");
        println!(
            "  + {} [{}] type=research (research pipeline)",
            task_id, identifier
        );
        total_created += 1;
    }

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

            // Research issues bypass the standard pipeline — already handled above
            if is_research_issue(&labels) && stage_config.status_key == "todo" {
                continue;
            }

            // Manual issues: skip execution stages (analyst, engineer, devops)
            // but allow review/qa — agents should review human code too.
            if crate::linear::is_manual_issue(&labels) && is_execution_stage(stage_config.stage) {
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
                repo_hash: crate::runtime_repo_hash(),
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

            // Create engineer task with analyst's output as handoff
            create_next_stage_task(db, linear_issue_id, "engineer", result, task_id, stage)?;
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
                create_next_stage_task(db, linear_issue_id, "engineer", result, task_id, stage)?;
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
                create_next_stage_task(db, linear_issue_id, "engineer", result, task_id, stage)?;
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

/// Truncate text to a maximum number of lines.
fn truncate_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().take(max).collect();
    let result = lines.join("\n");
    if text.lines().count() > max {
        format!(
            "{result}\n\n[... truncated, {max} of {} lines shown]",
            text.lines().count()
        )
    } else {
        result
    }
}

/// Create a task for the next pipeline stage with handoff context.
fn create_next_stage_task(
    db: &Db,
    linear_issue_id: &str,
    next_stage: &str,
    previous_output: &str,
    prev_task_id: &str,
    prev_stage: &str,
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

    // Write structured handoff file
    let werma_dir = dirs::home_dir().context("no home dir")?.join(".werma");
    let logs_dir = werma_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let handoff_path = logs_dir.join(format!("{task_id}-handoff.md"));

    let handoff_content = format!(
        "## Pipeline Handoff: {} ({}) -> {} ({})\n\
         Linear issue: {}\n\n\
         ### Previous Stage Output\n{}\n",
        prev_task_id,
        prev_stage,
        task_id,
        next_stage,
        linear_issue_id,
        truncate_lines(previous_output, 100),
    );
    std::fs::write(&handoff_path, &handoff_content)?;

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
        context_files: vec![handoff_path.to_string_lossy().to_string()],
        repo_hash: crate::runtime_repo_hash(),
    };

    db.insert_task(&task)?;
    println!(
        "  + pipeline task: {} stage={} type={}",
        task_id, next_stage, agent_type
    );

    Ok(())
}

/// Parse OUTPUT_FILE=<path> from research task output.
pub fn parse_output_file(result: &str) -> Option<String> {
    for line in result.lines().rev() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("OUTPUT_FILE=") {
            let path = path.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Handle research task completion: create curator follow-up and move issue to Done.
pub fn handle_research_completion(db: &Db, task: &Task, output: &str) -> Result<()> {
    let linear = LinearClient::new()?;

    let output_file = parse_output_file(output).unwrap_or_default();

    // Post summary as comment on the Linear issue
    let summary = extract_tldr(output);
    if !summary.is_empty() {
        linear.comment(
            &task.linear_issue_id,
            &format!(
                "**Research completed** (task: `{}`)\n\n{}\n\n{}",
                task.id,
                summary,
                if output_file.is_empty() {
                    String::new()
                } else {
                    format!("File: `{}`", output_file)
                }
            ),
        )?;
    }

    // Create curator follow-up task if we have an output file
    if !output_file.is_empty() {
        let curator_prompt = format!(
            "# Research Curator\n\n\
             ## Input\n\
             Research file: {}\n\
             Linear issue: {}\n\n\
             ## Tasks\n\
             1. Read the research file\n\
             2. Extract key topics/entities (libraries, patterns, strategies, tools)\n\
             3. Search for related research files in docs/research/\n\
             4. Check if findings update any existing memory files in ~/.claude/projects/*/memory/\n\
             5. Output: CURATOR_VERDICT=DONE or CURATOR_VERDICT=SKIPPED (nothing to link)",
            output_file, task.linear_issue_id
        );

        let curator_id = db.next_task_id()?;
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

        let curator_task = Task {
            id: curator_id.clone(),
            status: Status::Pending,
            priority: 3, // Low priority — informational
            created_at: now,
            started_at: None,
            finished_at: None,
            task_type: "research-curator".to_string(),
            prompt: curator_prompt,
            output_path: String::new(),
            working_dir: task.working_dir.clone(),
            model: "haiku".to_string(),
            max_turns: crate::default_turns("research-curator"),
            allowed_tools: crate::runner::tools_for_type("research-curator", false),
            session_id: String::new(),
            linear_issue_id: task.linear_issue_id.clone(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![task.id.clone()],
            context_files: vec![output_file],
            repo_hash: crate::runtime_repo_hash(),
        };

        db.insert_task(&curator_task)?;
        println!("  + curator task: {} for research {}", curator_id, task.id);
    }

    // Move issue to Done
    let _ = linear.move_issue_by_name(&task.linear_issue_id, "done");

    Ok(())
}

/// Extract TL;DR section from research output.
fn extract_tldr(text: &str) -> String {
    let mut in_tldr = false;
    let mut lines = Vec::new();

    for line in text.lines() {
        if line.trim().starts_with("## TL;DR") || line.trim().starts_with("## TLDR") {
            in_tldr = true;
            continue;
        }
        if in_tldr {
            if line.starts_with("## ") {
                break; // Next section
            }
            lines.push(line);
        }
    }

    let result = lines.join("\n").trim().to_string();
    if result.is_empty() {
        // Fallback: first 5 non-empty lines
        text.lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .take(5)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        result
    }
}

/// Show pipeline status: count issues at each stage.
pub fn status(db: &Db) -> Result<()> {
    // Try to get Linear counts
    let linear_available = LinearClient::new().is_ok();

    println!("\nPipeline Status:");
    println!("================\n");

    if linear_available {
        let linear = LinearClient::new()?;
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
    } else {
        println!("  (Linear not configured — showing local pipeline tasks only)");
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

    #[test]
    fn research_issue_detection() {
        assert!(is_research_issue(&["research"]));
        assert!(is_research_issue(&["Research"]));
        assert!(is_research_issue(&["RESEARCH"]));
        assert!(is_research_issue(&["Feature", "research", "repo:ar-quant"]));
        assert!(!is_research_issue(&["Feature", "Bug"]));
        assert!(!is_research_issue(&[]));
    }

    #[test]
    fn parse_output_file_from_result() {
        let text = "Research complete.\nSaved to file.\nOUTPUT_FILE=/path/to/file.md";
        assert_eq!(
            parse_output_file(text),
            Some("/path/to/file.md".to_string())
        );

        // Last line wins
        let text2 = "OUTPUT_FILE=/old/path.md\nMore output\nOUTPUT_FILE=/new/path.md";
        assert_eq!(parse_output_file(text2), Some("/new/path.md".to_string()));

        // No output file
        assert_eq!(parse_output_file("Just some text"), None);
        assert_eq!(parse_output_file(""), None);

        // Empty path
        assert_eq!(parse_output_file("OUTPUT_FILE="), None);
    }

    #[test]
    fn extract_tldr_section() {
        let text = "# Research\n\n## TL;DR\n\n- Point 1\n- Point 2\n\n## Findings\n\nDetails...";
        let tldr = extract_tldr(text);
        assert!(tldr.contains("Point 1"));
        assert!(tldr.contains("Point 2"));
        assert!(!tldr.contains("Details"));
    }

    #[test]
    fn extract_tldr_fallback() {
        let text = "First line of findings.\nSecond line.\nThird line.";
        let tldr = extract_tldr(text);
        assert!(tldr.contains("First line"));
    }

    #[test]
    fn truncate_lines_short() {
        let text = "line 1\nline 2\nline 3";
        assert_eq!(truncate_lines(text, 10), text);
    }

    #[test]
    fn truncate_lines_long() {
        let text: String = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_lines(&text, 5);
        assert!(result.contains("line 0"));
        assert!(result.contains("line 4"));
        assert!(!result.contains("line 5"));
        assert!(result.contains("[... truncated, 5 of 20 lines shown]"));
    }

    #[test]
    fn execution_vs_review_stages() {
        // Execution stages: manual issues should be skipped
        assert!(is_execution_stage("analyst"));
        assert!(is_execution_stage("engineer"));
        assert!(is_execution_stage("devops"));

        // Review/QA stages: manual issues should NOT be skipped
        assert!(!is_execution_stage("reviewer"));
        assert!(!is_execution_stage("qa"));
        assert!(!is_execution_stage("unknown"));
    }
}
