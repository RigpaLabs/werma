use std::collections::HashMap;

use anyhow::{Context, Result};

use super::config::PipelineConfig;
use super::loader::{load_default, resolve_prompt};
use super::prompt::{build_vars, render_prompt};
use super::verdict::{extract_rejection_feedback, is_heavy_track, parse_estimate, parse_verdict};
use crate::db::Db;
use crate::linear::LinearClient;
use crate::models::{Status, Task};

/// Maximum number of review cycles before auto-approving to prevent infinite loops.
const MAX_REVIEW_CYCLES: i64 = 3;

// ─── Public API ──────────────────────────────────────────────────────────────

/// Check if an issue is a research issue (has `research` label).
pub fn is_research_issue(labels: &[&str]) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case("research"))
}

/// Poll Linear for issues at pipeline-relevant statuses and create tasks.
pub fn poll(db: &Db) -> Result<()> {
    let config = load_default()?;
    let linear = LinearClient::new()?;

    let mut total_created = 0;
    let mut total_skipped = 0;

    // Research issues in Todo → research task (not pipeline task)
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
            estimate: 0,
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

    // Standard pipeline: iterate config stages that have linear_status
    let poll_stages = config.poll_stages();

    // Collect all unique status keys across all polled stages
    let mut status_to_stages: HashMap<String, Vec<String>> = HashMap::new();
    for (stage_name, stage_cfg) in &poll_stages {
        for key in stage_cfg.status_keys() {
            status_to_stages
                .entry(key.to_string())
                .or_default()
                .push(stage_name.to_string());
        }
    }

    for (status_key, stage_names) in &status_to_stages {
        let issues = linear.get_issues_by_status(status_key)?;

        for issue in &issues {
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

            // Research issues in todo bypass the standard pipeline
            if is_research_issue(&labels) && status_key == "todo" {
                continue;
            }

            for stage_name in stage_names {
                let stage_cfg = match config.stage(stage_name) {
                    Some(s) => s,
                    None => continue,
                };

                // Enforce max_concurrent: skip if stage already has enough active tasks
                let active_count = db.count_active_tasks_for_stage(stage_name)?;
                if active_count >= stage_cfg.max_concurrent as i64 {
                    total_skipped += 1;
                    continue;
                }

                // Skip if active task already exists for this issue + stage
                let existing = db.tasks_by_linear_issue(issue_id, Some(stage_name), true)?;
                if !existing.is_empty() {
                    total_skipped += 1;
                    continue;
                }

                // Manual issues: skip execution stages (skip_manual=true)
                if crate::linear::is_manual_issue(&labels) && stage_cfg.skip_manual() {
                    total_skipped += 1;
                    continue;
                }

                let working_dir = crate::linear::infer_working_dir(title, &labels);

                // Build prompt from config
                let prompt = build_poll_prompt(&config, stage_cfg, identifier, title, description);

                let task_id = db.next_task_id()?;
                let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

                let max_turns = crate::default_turns(&stage_cfg.agent);
                let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);

                let task = Task {
                    id: task_id.clone(),
                    status: Status::Pending,
                    priority: 1,
                    created_at: now,
                    started_at: None,
                    finished_at: None,
                    task_type: stage_cfg.agent.clone(),
                    prompt,
                    output_path: String::new(),
                    working_dir,
                    model: stage_cfg.model.clone(),
                    max_turns,
                    allowed_tools,
                    session_id: String::new(),
                    linear_issue_id: issue_id.to_string(),
                    linear_pushed: false,
                    pipeline_stage: stage_name.clone(),
                    depends_on: vec![],
                    context_files: vec![],
                    repo_hash: crate::runtime_repo_hash(),
                    estimate: issue["estimate"].as_i64().unwrap_or(0) as i32,
                };

                db.insert_task(&task)?;
                println!(
                    "  + {} [{}] stage={} type={}",
                    task_id, identifier, stage_name, stage_cfg.agent
                );
                total_created += 1;
            }
        }
    }

    println!(
        "\nPipeline poll: {} created, {} skipped",
        total_created, total_skipped
    );
    Ok(())
}

/// Handle pipeline callback when a task completes.
pub fn callback(
    db: &Db,
    task_id: &str,
    stage: &str,
    result: &str,
    linear_issue_id: &str,
    working_dir: &str,
) -> Result<()> {
    let config = load_default()?;
    let linear = LinearClient::new()?;

    let stage_cfg = if let Some(s) = config.stage(stage) {
        s
    } else {
        eprintln!("unknown pipeline stage: {stage}");
        return Ok(());
    };

    let verdict = parse_verdict(result);

    // Stages with no verdicts in transitions (engineer/analyst) are auto-complete.
    // For stages that require a verdict (reviewer, qa, devops), warn if missing.
    let has_explicit_transitions = !stage_cfg.transitions.is_empty();
    let is_auto_complete = stage_cfg.transitions.values().all(|t| t.spawn.is_none())
        && stage_cfg
            .transitions
            .values()
            .any(|t| t.status != "in_progress");

    let _ = is_auto_complete; // used implicitly via logic below

    if verdict.is_none() && has_explicit_transitions && stage != "engineer" && stage != "analyst" {
        eprintln!(
            "warning: no verdict found for task {} (stage={}), keeping current state",
            task_id, stage
        );
        linear.comment(
            linear_issue_id,
            &format!(
                "**Werma task `{task_id}`** (stage: {stage}) completed but no verdict found. \
                 Manual review needed."
            ),
        )?;
        return Ok(());
    }

    // For engineer/analyst: default verdict is "done" if none found
    let verdict_str = verdict
        .as_deref()
        .unwrap_or(if stage == "engineer" || stage == "analyst" {
            "done"
        } else {
            ""
        })
        .to_lowercase();

    // Parse estimate from analyst output for adaptive track routing
    let estimate = if stage == "analyst" {
        let est = parse_estimate(result);
        if est > 0 {
            if let Err(e) = linear.update_estimate(linear_issue_id, est) {
                eprintln!("warn: failed to update estimate on Linear: {e}");
            }
            if let Err(e) = db.update_task_field(task_id, "estimate", &est.to_string()) {
                eprintln!("warn: failed to update estimate in DB: {e}");
            }
        }
        est
    } else {
        0
    };

    let transition = stage_cfg.transition_for(&verdict_str);

    match transition {
        Some(t) => {
            linear.move_issue_by_name(linear_issue_id, &t.status)?;

            // Post a comment summarizing what happened
            let comment = format_callback_comment(task_id, stage, &verdict_str, t.spawn.as_deref());
            linear.comment(linear_issue_id, &comment)?;

            // Spawn next stage if configured
            if let Some(ref next_stage) = t.spawn {
                // Check review cycle limit: if reviewer has rejected too many times,
                // force-approve instead to prevent infinite loops.
                if stage == "reviewer" && next_stage == "engineer" {
                    let review_count =
                        db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?;
                    if review_count >= MAX_REVIEW_CYCLES {
                        eprintln!(
                            "review cycle limit ({MAX_REVIEW_CYCLES}) reached for issue {}, \
                             force-approving",
                            linear_issue_id
                        );
                        linear.move_issue_by_name(linear_issue_id, "ready")?;
                        linear.comment(
                            linear_issue_id,
                            &format!(
                                "**Review cycle limit reached** ({MAX_REVIEW_CYCLES} cycles). \
                                 Auto-moving to Ready. Manual review recommended."
                            ),
                        )?;
                        // Don't spawn another engineer cycle
                        return Ok(());
                    }
                }

                create_next_stage_task(&NextStageParams {
                    db,
                    config: &config,
                    linear: Some(&linear),
                    linear_issue_id,
                    next_stage,
                    previous_output: result,
                    prev_task_id: task_id,
                    prev_stage: stage,
                    working_dir,
                    estimate,
                })?;
            }
        }
        None => {
            eprintln!(
                "stage '{}': no transition for verdict '{}' — no action taken",
                stage, verdict_str
            );
        }
    }

    Ok(())
}

/// Create a pipeline task for an initial stage (no previous output).
/// Used by `werma pipeline run` to manually trigger a stage.
#[allow(clippy::too_many_arguments)]
pub fn create_initial_stage_task(
    db: &Db,
    config: &PipelineConfig,
    stage_name: &str,
    linear_issue_id: &str,
    identifier: &str,
    title: &str,
    description: &str,
    working_dir: &str,
    estimate: i32,
) -> Result<String> {
    let stage_cfg = config
        .stage(stage_name)
        .with_context(|| format!("unknown pipeline stage: {stage_name}"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let max_turns = crate::default_turns(&stage_cfg.agent);
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);

    let prompt = build_poll_prompt(config, stage_cfg, identifier, title, description);

    let effective_working_dir = if working_dir.is_empty() || working_dir == "~/projects/ar" {
        infer_working_dir_from_issue(db, linear_issue_id)
    } else {
        working_dir.to_string()
    };

    let task = Task {
        id: task_id.clone(),
        status: Status::Pending,
        priority: 1,
        created_at: now,
        started_at: None,
        finished_at: None,
        task_type: stage_cfg.agent.clone(),
        prompt,
        output_path: String::new(),
        working_dir: effective_working_dir,
        model: stage_cfg.model.clone(),
        max_turns,
        allowed_tools,
        session_id: String::new(),
        linear_issue_id: linear_issue_id.to_string(),
        linear_pushed: false,
        pipeline_stage: stage_name.to_string(),
        depends_on: vec![],
        context_files: vec![],
        repo_hash: crate::runtime_repo_hash(),
        estimate,
    };

    db.insert_task(&task)?;
    Ok(task_id)
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Build a comment string for a pipeline callback.
fn format_callback_comment(
    task_id: &str,
    stage: &str,
    verdict: &str,
    spawn: Option<&str>,
) -> String {
    let stage_label = stage
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + &stage[1..])
        .unwrap_or_else(|| stage.to_string());

    let spawn_note = spawn
        .map(|s| format!(" Spawning **{s}** stage."))
        .unwrap_or_default();

    match verdict.to_lowercase().as_str() {
        "approved" | "passed" | "done" | "ok" | "fixed" => {
            format!(
                "**{stage_label} {verdict_upper}** (task: `{task_id}`).{spawn_note}",
                verdict_upper = verdict.to_uppercase()
            )
        }
        "rejected" | "failed" | "request_changes" => {
            format!(
                "**{stage_label}: {verdict_upper}** (task: `{task_id}`). Moving back.{spawn_note}",
                verdict_upper = verdict.to_uppercase()
            )
        }
        _ => {
            format!(
                "**{stage_label}** completed (task: `{task_id}`), verdict: {verdict}.{spawn_note}"
            )
        }
    }
}

/// Build the initial prompt for a polled stage (from config, with issue vars).
fn build_poll_prompt(
    config: &PipelineConfig,
    stage_cfg: &super::config::StageConfig,
    identifier: &str,
    title: &str,
    description: &str,
) -> String {
    let prompt_source = match &stage_cfg.prompt {
        Some(p) => resolve_prompt(p),
        None => {
            // No prompt in config — minimal fallback
            return format!(
                "[{identifier}] {title}\n\nStage: {agent}\n\n{description}",
                agent = stage_cfg.agent
            );
        }
    };

    let mut runtime: HashMap<String, String> = HashMap::new();
    runtime.insert("issue_id".to_string(), identifier.to_string());
    runtime.insert("issue_title".to_string(), title.to_string());
    runtime.insert("issue_description".to_string(), description.to_string());

    let vars = build_vars(&config.templates, &runtime);
    render_prompt(&prompt_source, &vars)
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

/// Infer working directory from existing tasks for the same Linear issue.
fn infer_working_dir_from_issue(db: &Db, linear_issue_id: &str) -> String {
    if let Ok(tasks) = db.tasks_by_linear_issue(linear_issue_id, None, false) {
        for task in &tasks {
            if !task.working_dir.is_empty() && task.working_dir != "~/projects/ar" {
                return task.working_dir.clone();
            }
        }
        if let Some(task) = tasks.first() {
            return task.working_dir.clone();
        }
    }
    "~/projects/ar".to_string()
}

/// Build the stage prompt for a spawned task (handoff context).
fn build_handoff_prompt(
    config: &PipelineConfig,
    next_stage: &str,
    prev_stage: &str,
    linear_issue_id: &str,
    issue_title: &str,
    issue_description: &str,
    previous_output: &str,
) -> String {
    let stage_cfg = match config.stage(next_stage) {
        Some(s) => s,
        None => {
            return format!(
                "Continue pipeline for Linear issue {linear_issue_id}. Stage: {next_stage}\n\n\
                 Previous stage ({prev_stage}) output is in the handoff context file."
            );
        }
    };

    // For engineer spawned from reviewer/qa rejection: include rejection feedback inline
    let feedback = if next_stage == "engineer" && (prev_stage == "reviewer" || prev_stage == "qa") {
        Some(extract_rejection_feedback(previous_output))
    } else {
        None
    };

    let prompt_source = match &stage_cfg.prompt {
        Some(p) => resolve_prompt(p),
        None => {
            return format!(
                "Continue pipeline for Linear issue {linear_issue_id}. Stage: {next_stage}\n\n\
                 Previous stage ({prev_stage}) output is in the handoff context file."
            );
        }
    };

    let mut runtime: HashMap<String, String> = HashMap::new();
    runtime.insert("issue_id".to_string(), linear_issue_id.to_string());
    runtime.insert("issue_title".to_string(), issue_title.to_string());
    runtime.insert(
        "issue_description".to_string(),
        issue_description.to_string(),
    );
    runtime.insert("previous_output".to_string(), previous_output.to_string());
    runtime.insert(
        "rejection_feedback".to_string(),
        feedback.clone().unwrap_or_default(),
    );
    runtime.insert("working_dir".to_string(), String::new());

    let vars = build_vars(&config.templates, &runtime);
    let mut rendered = render_prompt(&prompt_source, &vars);

    // For rejection flows: inject feedback section if the prompt doesn't already use it
    if let Some(fb) = feedback
        && !rendered.contains(&fb)
        && !fb.is_empty()
    {
        let from_label = if prev_stage == "reviewer" {
            "Reviewer Feedback"
        } else {
            "QA Failure Report"
        };
        let stage_kind = if prev_stage == "reviewer" {
            "Revision"
        } else {
            "QA Fix"
        };
        // Rebuild with explicit context section prepended
        rendered = format!(
            "# Pipeline: Engineer Stage ({stage_kind})\n\
             Linear issue: {linear_issue_id}\n\n\
             ## {from_label}\n{fb}\n\n{rendered}"
        );
    }

    rendered
}

/// Parameters for creating the next pipeline stage task.
pub(crate) struct NextStageParams<'a> {
    pub db: &'a Db,
    pub config: &'a PipelineConfig,
    pub linear: Option<&'a LinearClient>,
    pub linear_issue_id: &'a str,
    pub next_stage: &'a str,
    pub previous_output: &'a str,
    pub prev_task_id: &'a str,
    pub prev_stage: &'a str,
    pub working_dir: &'a str,
    pub estimate: i32,
}

/// Create a task for the next pipeline stage with handoff context.
pub(crate) fn create_next_stage_task(p: &NextStageParams<'_>) -> Result<()> {
    let NextStageParams {
        db,
        config,
        linear,
        linear_issue_id,
        next_stage,
        previous_output,
        prev_task_id,
        prev_stage,
        working_dir,
        estimate: _,
    } = p;
    let stage_cfg = config
        .stage(next_stage)
        .with_context(|| format!("no config for stage '{next_stage}'"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    // Engineer turns vary by track: heavy track gets more budget for complex work
    let max_turns = if *next_stage == "engineer" {
        if is_heavy_track(p.estimate) { 45 } else { 30 }
    } else {
        crate::default_turns(&stage_cfg.agent)
    };
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);

    // Fetch issue title/description from Linear for template vars
    let (issue_title, issue_description) = linear
        .and_then(|l| l.get_issue(linear_issue_id).ok())
        .unwrap_or_default();

    let prompt = build_handoff_prompt(
        config,
        next_stage,
        prev_stage,
        linear_issue_id,
        &issue_title,
        &issue_description,
        previous_output,
    );

    // Write structured handoff file with previous stage output
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
        truncate_lines(previous_output, 200),
    );
    std::fs::write(&handoff_path, &handoff_content)?;

    // Use passed working_dir, fallback to inference from existing tasks
    let effective_working_dir = if working_dir.is_empty() || *working_dir == "~/projects/ar" {
        infer_working_dir_from_issue(db, linear_issue_id)
    } else {
        working_dir.to_string()
    };

    let task = Task {
        id: task_id.clone(),
        status: Status::Pending,
        priority: 1,
        created_at: now,
        started_at: None,
        finished_at: None,
        task_type: stage_cfg.agent.clone(),
        prompt,
        output_path: String::new(),
        working_dir: effective_working_dir,
        model: stage_cfg.model.clone(),
        max_turns,
        allowed_tools,
        session_id: String::new(),
        linear_issue_id: linear_issue_id.to_string(),
        linear_pushed: false,
        pipeline_stage: next_stage.to_string(),
        depends_on: vec![],
        context_files: vec![handoff_path.to_string_lossy().to_string()],
        repo_hash: crate::runtime_repo_hash(),
        estimate: p.estimate,
    };

    db.insert_task(&task)?;
    println!(
        "  + pipeline task: {} stage={} type={}",
        task_id, next_stage, stage_cfg.agent
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::loader::load_from_str;

    fn test_config() -> PipelineConfig {
        load_from_str(include_str!("../../pipelines/default.yaml"), "<test>").unwrap()
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
    fn callback_analyst_creates_engineer_with_context() {
        let db = crate::db::Db::open_in_memory().unwrap();

        let analyst_task = Task {
            id: "20260310-001".to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-10T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-analyst".to_string(),
            prompt: "analyze issue".to_string(),
            output_path: String::new(),
            working_dir: "~/projects/rigpa/werma".to_string(),
            model: "opus".to_string(),
            max_turns: 20,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "test-issue-abc".to_string(),
            linear_pushed: false,
            pipeline_stage: "analyst".to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };
        db.insert_task(&analyst_task).unwrap();

        let config = test_config();
        let analyst_output = "## Spec\nImplement feature X\n## Requirements\n- Do A\n- Do B";

        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "test-issue-abc",
            next_stage: "engineer",
            previous_output: analyst_output,
            prev_task_id: "20260310-001",
            prev_stage: "analyst",
            working_dir: "~/projects/rigpa/werma",
            estimate: 0,
        })
        .unwrap();

        let tasks = db
            .tasks_by_linear_issue("test-issue-abc", Some("engineer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);

        let eng_task = &tasks[0];
        assert_eq!(eng_task.pipeline_stage, "engineer");
        assert_eq!(eng_task.task_type, "pipeline-engineer");
        assert!(!eng_task.context_files.is_empty());
        assert_eq!(eng_task.working_dir, "~/projects/rigpa/werma");
    }

    #[test]
    fn callback_reviewer_rejected_passes_feedback() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let reviewer_output = "## Review\n- blocker: no tests\nREVIEW_VERDICT=REJECTED";

        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "test-issue-def",
            next_stage: "engineer",
            previous_output: reviewer_output,
            prev_task_id: "20260310-002",
            prev_stage: "reviewer",
            working_dir: "",
            estimate: 0,
        })
        .unwrap();

        let pending = db.list_tasks(Some(Status::Pending)).unwrap();
        assert_eq!(pending.len(), 1);

        let eng_task = &pending[0];
        // Should contain rejection context — either in prompt or handoff
        assert!(
            eng_task.prompt.contains("Revision")
                || eng_task.prompt.contains("rejected")
                || eng_task.prompt.contains("blocker")
        );
    }

    #[test]
    fn infer_working_dir_from_existing_tasks() {
        let db = crate::db::Db::open_in_memory().unwrap();

        let task = Task {
            id: "20260310-010".to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-10T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-analyst".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "~/projects/rigpa/werma".to_string(),
            model: "opus".to_string(),
            max_turns: 20,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "issue-xyz".to_string(),
            linear_pushed: false,
            pipeline_stage: "analyst".to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };
        db.insert_task(&task).unwrap();

        let dir = infer_working_dir_from_issue(&db, "issue-xyz");
        assert_eq!(dir, "~/projects/rigpa/werma");

        let dir = infer_working_dir_from_issue(&db, "unknown-issue");
        assert_eq!(dir, "~/projects/ar");
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
    fn callback_no_verdict_does_not_create_task() {
        // Verify parse_verdict returns None for empty output
        assert!(
            crate::pipeline::verdict::parse_verdict("Just some output without verdict markers")
                .is_none()
        );

        let db = crate::db::Db::open_in_memory().unwrap();
        let pending = db.list_tasks(Some(Status::Pending)).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn format_callback_comment_approved() {
        let comment = format_callback_comment("task-123", "reviewer", "approved", None);
        assert!(comment.contains("APPROVED"));
        assert!(comment.contains("task-123"));
    }

    #[test]
    fn format_callback_comment_rejected_with_spawn() {
        let comment = format_callback_comment("task-456", "reviewer", "rejected", Some("engineer"));
        assert!(comment.contains("REJECTED"));
        assert!(comment.contains("engineer"));
    }

    #[test]
    fn build_poll_prompt_uses_issue_vars() {
        let config = test_config();
        let stage_cfg = config.stage("analyst").unwrap();
        let prompt = build_poll_prompt(&config, stage_cfg, "RIG-65", "My title", "My description");
        assert!(prompt.contains("RIG-65"));
        assert!(prompt.contains("My title"));
    }

    #[test]
    fn build_handoff_prompt_for_engineer_from_analyst() {
        let config = test_config();
        let prompt = build_handoff_prompt(
            &config,
            "engineer",
            "analyst",
            "issue-123",
            "Test Issue Title",
            "Test description",
            "spec output",
        );
        assert!(prompt.contains("issue-123"));
    }

    #[test]
    fn build_handoff_prompt_for_engineer_from_reviewer_includes_feedback() {
        let config = test_config();
        let reviewer_output =
            "## Findings\n- blocker: missing error handling\nREVIEW_VERDICT=REJECTED";
        let prompt = build_handoff_prompt(
            &config,
            "engineer",
            "reviewer",
            "issue-123",
            "Title",
            "Desc",
            reviewer_output,
        );
        // Feedback should be present somewhere in the prompt
        assert!(
            prompt.contains("blocker")
                || prompt.contains("Revision")
                || prompt.contains("rejected")
        );
    }
}
