use std::collections::HashMap;

use anyhow::Result;

use super::config::PipelineConfig;
use super::loader::load_default;
use super::loader::resolve_prompt;
use super::prompt::{build_vars, render_prompt};
use crate::db::Db;
use crate::linear::LinearApi;
use crate::models::{Status, Task};
use crate::traits::CommandRunner;

use super::pr::{has_open_pr_for_issue, is_pr_merged_for_issue};

/// Check if an issue is a research issue (has `research` label).
pub fn is_research_issue(labels: &[&str]) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case("research"))
}

/// Poll Linear for issues at pipeline-relevant statuses and create tasks.
pub fn poll(db: &Db, linear: &dyn LinearApi, cmd: &dyn CommandRunner) -> Result<()> {
    let config = load_default()?;
    let user_cfg = crate::config::UserConfig::load();

    let mut total_created = 0;
    let mut total_skipped = 0;

    // Research issues in Todo → research task (not pipeline task)
    let todo_issues = linear.get_issues_by_status("todo")?;
    for issue in &todo_issues {
        let issue_id = issue["id"].as_str().unwrap_or("");
        let identifier = issue["identifier"].as_str().unwrap_or("");
        let title = issue["title"].as_str().unwrap_or("");
        let description = issue["description"].as_str().unwrap_or("");

        // RIG-307: skip issues with empty id or identifier to prevent ghost tasks
        if issue_id.is_empty() || identifier.is_empty() {
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
        let existing = db.tasks_by_linear_issue(identifier, None, true)?;
        if !existing.is_empty() {
            total_skipped += 1;
            continue;
        }

        let working_dir = crate::linear::infer_working_dir(title, &labels, &user_cfg);
        if crate::linear::validate_working_dir(&working_dir).is_none() {
            eprintln!(
                "  ! skipping {identifier} [{title}]: working dir '{working_dir}' does not exist"
            );
            total_skipped += 1;
            continue;
        }
        let prompt = format!(
            "[{identifier}] {title}\n\n{description}\n\nSave the research output as a markdown file in docs/research/. \
             On the last line of your output, write: OUTPUT_FILE=<path-to-saved-file>"
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
            linear_issue_id: identifier.to_string(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: crate::runtime_repo_hash(),
            estimate: 0,
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
        };

        db.insert_task(&task)?;
        // Move to In Progress so it doesn't get picked up again
        if let Err(e) = linear.move_issue_by_name(issue_id, "in_progress") {
            eprintln!("  ! research move to in_progress failed for {identifier}: {e}");
        }
        println!("  + {task_id} [{identifier}] type=research (research pipeline)");
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

            // RIG-307: skip issues with empty id or identifier to prevent ghost tasks
            if issue_id.is_empty() || identifier.is_empty() {
                continue;
            }

            // Skip issues whose state type is completed or canceled — they're done.
            let state_type = issue["state"]["type"].as_str().unwrap_or("");
            if state_type == "completed" || state_type == "canceled" || state_type == "cancelled" {
                total_skipped += 1;
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

                // Skip if an active or callback-pending task exists for this issue + stage.
                // Active (pending/running) tasks block to prevent duplicates.
                // Completed-but-unpushed tasks block until callback fires (RIG-209).
                // Completed+pushed tasks do NOT block — allows re-spawn after rejection
                // cycles where reviewer sends issue back to In Progress (RIG-277).
                if db.has_any_nonfailed_task_for_issue_stage(identifier, stage_name)? {
                    eprintln!(
                        "  ~ skipping {identifier} stage={stage_name}: \
                         active or callback-pending task exists"
                    );
                    total_skipped += 1;
                    continue;
                }

                // RIG-296: cross-stage guard — skip if another pipeline task is running
                // for this issue. Prevents spawning engineer while reviewer is still active.
                if db.has_running_pipeline_task_for_issue(identifier)? {
                    eprintln!(
                        "  ~ skipping {identifier} stage={stage_name}: \
                         another pipeline task is running for this issue"
                    );
                    total_skipped += 1;
                    continue;
                }

                // Manual issues: skip execution stages (skip_manual=true)
                if crate::linear::is_manual_issue(&labels) && stage_cfg.skip_manual() {
                    total_skipped += 1;
                    continue;
                }

                let working_dir = crate::linear::infer_working_dir(title, &labels, &user_cfg);
                if crate::linear::validate_working_dir(&working_dir).is_none() {
                    eprintln!(
                        "  ! skipping {identifier} [{title}] stage={stage_name}: working dir '{working_dir}' does not exist"
                    );
                    total_skipped += 1;
                    continue;
                }

                // RIG-135: Cross-stage dedup for reviewer — skip if any review task
                // (regardless of stage name) is already active for this issue.
                if stage_name == "reviewer" && db.has_any_review_task_for_issue(identifier)? {
                    total_skipped += 1;
                    continue;
                }

                // RIG-309: Circuit breaker — cap total reviewer spawns per issue to prevent
                // infinite loops when reviewer produces empty results (no verdict → issue stays
                // in Review → poller spawns another reviewer → repeat).
                if stage_name == "reviewer" {
                    let max_rounds = stage_cfg
                        .review_round_limit()
                        .unwrap_or(super::callback::DEFAULT_MAX_REVIEW_ROUNDS)
                        as i64;
                    let total_reviewer_tasks =
                        db.count_all_tasks_for_issue_stage(identifier, "reviewer")?;
                    if total_reviewer_tasks >= max_rounds * 2 {
                        eprintln!(
                            "  ! {identifier} circuit breaker: {total_reviewer_tasks} total reviewer \
                             tasks (limit: {}), skipping spawn",
                            max_rounds * 2
                        );
                        if total_reviewer_tasks == max_rounds * 2 {
                            // Only move to backlog on first trigger, not every poll
                            if let Err(e) = linear.move_issue_by_name(issue_id, "backlog") {
                                eprintln!(
                                    "  ! circuit breaker: failed to move {identifier} to backlog: {e}"
                                );
                            }
                            if let Err(e) = linear.comment(
                                identifier,
                                &format!(
                                    "**Reviewer circuit breaker triggered** — {total_reviewer_tasks} \
                                     reviewer tasks spawned without resolution. Moving to Backlog. \
                                     Manual intervention required."
                                ),
                            ) {
                                eprintln!(
                                    "  ! circuit breaker: failed to post comment on {identifier}: {e}"
                                );
                            }
                        }
                        total_skipped += 1;
                        continue;
                    }
                }

                // For reviewer stage: skip if PR is already merged (manual merge while in Review)
                // RIG-306: Only skip if merged AND no open PR exists (re-worked issues have both)
                if stage_name == "reviewer"
                    && is_pr_merged_for_issue(cmd, &working_dir, identifier)
                    && !has_open_pr_for_issue(cmd, &working_dir, identifier)
                {
                    println!("  ~ {identifier} [{title}] PR already merged, moving to Done");
                    if let Err(e) = linear.move_issue_by_name(issue_id, "done") {
                        eprintln!("  ! failed to move {identifier} to done: {e}");
                    }
                    total_skipped += 1;
                    continue;
                }

                // Build prompt from config
                let prompt = build_poll_prompt(&config, stage_cfg, identifier, title, description);

                let task_id = db.next_task_id()?;
                let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

                let issue_estimate = issue["estimate"].as_i64().unwrap_or(0) as i32;

                // For polled stages (first invocation), review_round=0
                let review_round: i64 = if stage_name == "reviewer" {
                    db.count_completed_tasks_for_issue_stage(identifier, "reviewer")?
                } else {
                    0
                };

                let max_turns = stage_cfg
                    .max_turns
                    .map(|t| t as i32)
                    .unwrap_or_else(|| crate::default_turns(&stage_cfg.agent));
                let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
                let effective_model = stage_cfg
                    .effective_model(issue_estimate, review_round)
                    .to_string();

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
                    model: effective_model,
                    max_turns,
                    allowed_tools,
                    session_id: String::new(),
                    linear_issue_id: identifier.to_string(),
                    linear_pushed: false,
                    pipeline_stage: stage_name.clone(),
                    depends_on: vec![],
                    context_files: vec![],
                    repo_hash: crate::runtime_repo_hash(),
                    estimate: issue_estimate,
                    retry_count: 0,
                    retry_after: None,
                    cost_usd: None,
                    turns_used: 0,
                };

                db.insert_task(&task)?;

                // on_start: move issue to a different status when task is created
                if let Some(ref on_start) = stage_cfg.on_start
                    && let Err(e) = linear.move_issue_by_name(issue_id, &on_start.status)
                {
                    eprintln!(
                        "  ! on_start move failed for {} -> {}: {e}",
                        identifier, on_start.status
                    );
                }

                println!(
                    "  + {} [{}] stage={} type={}",
                    task_id, identifier, stage_name, stage_cfg.agent
                );
                total_created += 1;
            }
        }
    }

    // Label-based polling: iterate stages with linear_label
    for (stage_name, stage_cfg) in &poll_stages {
        let label = match &stage_cfg.linear_label {
            Some(l) => l.clone(),
            None => continue,
        };

        let issues = linear.get_issues_by_label(&label)?;

        for issue in &issues {
            let issue_id = issue["id"].as_str().unwrap_or("");
            let identifier = issue["identifier"].as_str().unwrap_or("");
            let title = issue["title"].as_str().unwrap_or("");
            let description = issue["description"].as_str().unwrap_or("");

            // RIG-307: skip issues with empty id or identifier to prevent ghost tasks
            if issue_id.is_empty() || identifier.is_empty() {
                continue;
            }

            // Label-based triggers only fire on Backlog issues.
            // Issues in any other status (In Progress, Review, etc.) are ignored.
            let state_type = issue["state"]["type"].as_str().unwrap_or("");
            if state_type != "backlog" {
                total_skipped += 1;
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

            // Manual issues: never auto-process via label triggers.
            if crate::linear::is_manual_issue(&labels) {
                total_skipped += 1;
                continue;
            }

            // Skip if active or callback-pending task exists (RIG-209, RIG-277).
            if db.has_any_nonfailed_task_for_issue_stage(identifier, stage_name)? {
                eprintln!(
                    "  ~ skipping {identifier} stage={stage_name}: \
                     active or callback-pending task exists (label path)"
                );
                total_skipped += 1;
                continue;
            }

            // RIG-296: cross-stage guard (label path) — skip if another pipeline
            // task is running for this issue.
            if db.has_running_pipeline_task_for_issue(identifier)? {
                eprintln!(
                    "  ~ skipping {identifier} stage={stage_name}: \
                     another pipeline task is running for this issue (label path)"
                );
                total_skipped += 1;
                continue;
            }

            // RIG-274/RIG-300: Skip analyst if already processed (has spec:done,
            // {label}:done, or {label}:blocked). Prevents re-running analyst on
            // issues that were already analyzed or blocked.
            if *stage_name == "analyst" {
                let done_label = format!("{label}:done");
                let blocked_label = format!("{label}:blocked");
                let has_result = labels.iter().any(|l| {
                    l.eq_ignore_ascii_case("spec:done")
                        || l.eq_ignore_ascii_case(&done_label)
                        || l.eq_ignore_ascii_case(&blocked_label)
                });
                if has_result {
                    eprintln!(
                        "  ~ skipping analyst for {identifier}: already processed (has result label)"
                    );
                    // Clean up the stale trigger label
                    if let Err(e) = linear.remove_label(issue_id, &label) {
                        eprintln!("  ! failed to remove stale '{label}' from {identifier}: {e}");
                    }
                    total_skipped += 1;
                    continue;
                }
            }

            // Guard: don't re-run analyst if engineer has already started for this issue.
            // Prevents analyst from seeing an open PR and declaring ALREADY_DONE.
            if *stage_name == "analyst" {
                let engineer_tasks =
                    db.tasks_by_linear_issue(identifier, Some("engineer"), false)?;
                if !engineer_tasks.is_empty() {
                    eprintln!(
                        "  ~ skipping analyst for {identifier}: engineer already ran ({} tasks)",
                        engineer_tasks.len()
                    );
                    total_skipped += 1;
                    continue;
                }
            }

            let working_dir = crate::linear::infer_working_dir(title, &labels, &user_cfg);
            if crate::linear::validate_working_dir(&working_dir).is_none() {
                eprintln!(
                    "  ! skipping {identifier} [{title}] stage={stage_name}: working dir '{working_dir}' does not exist"
                );
                total_skipped += 1;
                continue;
            }

            // RIG-135: Cross-stage dedup for reviewer (label-based path)
            if *stage_name == "reviewer" && db.has_any_review_task_for_issue(identifier)? {
                total_skipped += 1;
                continue;
            }

            // RIG-309: Circuit breaker (label-based path) — same logic as status-based path.
            if *stage_name == "reviewer" {
                let max_rounds = stage_cfg
                    .review_round_limit()
                    .unwrap_or(super::callback::DEFAULT_MAX_REVIEW_ROUNDS)
                    as i64;
                let total_reviewer_tasks =
                    db.count_all_tasks_for_issue_stage(identifier, "reviewer")?;
                if total_reviewer_tasks >= max_rounds * 2 {
                    eprintln!(
                        "  ! {identifier} circuit breaker: {total_reviewer_tasks} total reviewer \
                         tasks (limit: {}), skipping spawn (label path)",
                        max_rounds * 2
                    );
                    total_skipped += 1;
                    continue;
                }
            }

            // For reviewer stage: skip if PR is already merged
            // RIG-306: Only skip if merged AND no open PR exists (re-worked issues have both)
            if *stage_name == "reviewer"
                && is_pr_merged_for_issue(cmd, &working_dir, identifier)
                && !has_open_pr_for_issue(cmd, &working_dir, identifier)
            {
                println!("  ~ {identifier} [{title}] PR already merged, moving to Done");
                if let Err(e) = linear.move_issue_by_name(issue_id, "done") {
                    eprintln!("  ! failed to move {identifier} to done: {e}");
                }
                total_skipped += 1;
                continue;
            }

            let prompt = build_poll_prompt(&config, stage_cfg, identifier, title, description);

            let task_id = db.next_task_id()?;
            let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

            let issue_estimate = issue["estimate"].as_i64().unwrap_or(0) as i32;

            let review_round: i64 = if *stage_name == "reviewer" {
                db.count_completed_tasks_for_issue_stage(identifier, "reviewer")?
            } else {
                0
            };

            let max_turns = stage_cfg
                .max_turns
                .map(|t| t as i32)
                .unwrap_or_else(|| crate::default_turns(&stage_cfg.agent));
            let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
            let effective_model = stage_cfg
                .effective_model(issue_estimate, review_round)
                .to_string();

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
                model: effective_model,
                max_turns,
                allowed_tools,
                session_id: String::new(),
                linear_issue_id: identifier.to_string(),
                linear_pushed: false,
                pipeline_stage: stage_name.to_string(),
                depends_on: vec![],
                context_files: vec![],
                repo_hash: crate::runtime_repo_hash(),
                estimate: issue_estimate,
                retry_count: 0,
                retry_after: None,
                cost_usd: None,
                turns_used: 0,
            };

            db.insert_task(&task)?;

            // Remove the trigger label from the issue so it doesn't get picked up again
            if let Err(e) = linear.remove_label(issue_id, &label) {
                eprintln!("  ! failed to remove label '{label}' from {identifier}: {e}");
            }

            // on_start: move issue status
            if let Some(ref on_start) = stage_cfg.on_start
                && let Err(e) = linear.move_issue_by_name(issue_id, &on_start.status)
            {
                eprintln!(
                    "  ! on_start move failed for {} -> {}: {e}",
                    identifier, on_start.status
                );
            }

            println!(
                "  + {} [{}] stage={} type={} (label: {})",
                task_id, identifier, stage_name, stage_cfg.agent, label
            );
            total_created += 1;
        }
    }

    println!("\nPipeline poll: {total_created} created, {total_skipped} skipped");
    Ok(())
}

/// Build the initial prompt for a polled stage (from config, with issue vars).
pub(crate) fn build_poll_prompt(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::loader::load_from_str;
    use crate::traits::fakes::{FakeCommandRunner, FakeLinearApi};

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
    fn research_issue_mixed_labels() {
        assert!(is_research_issue(&["Feature", "Research", "repo:werma"]));
        assert!(!is_research_issue(&["researcher"])); // partial match should not trigger
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
    fn build_poll_prompt_fallback_when_no_prompt() {
        let yaml = r#"
pipeline: minimal
stages:
  bare:
    agent: pipeline-test
    model: sonnet
"#;
        let config = load_from_str(yaml, "<test>").unwrap();
        let stage_cfg = config.stage("bare").unwrap();
        let prompt = build_poll_prompt(&config, stage_cfg, "RIG-99", "Bare title", "Bare desc");
        assert!(prompt.contains("RIG-99"));
        assert!(prompt.contains("Bare title"));
        assert!(prompt.contains("pipeline-test")); // agent name in fallback
    }

    /// Helper: build a fake issue JSON with labels and backlog state.
    fn fake_issue(id: &str, identifier: &str, title: &str, labels: &[&str]) -> serde_json::Value {
        fake_issue_with_state(id, identifier, title, labels, "backlog")
    }

    /// Helper: build a fake issue JSON with labels and a specified state type.
    fn fake_issue_with_state(
        id: &str,
        identifier: &str,
        title: &str,
        labels: &[&str],
        state_type: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "identifier": identifier,
            "title": title,
            "description": "Test description",
            "state": {"type": state_type},
            "estimate": 3,
            "labels": {
                "nodes": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>()
            }
        })
    }

    #[test]
    fn poll_skips_analyst_when_spec_done_label_present() {
        // RIG-274: issues with spec:done label should not spawn analyst tasks
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        // Issue has both "analyze" trigger label AND "spec:done" → should be skipped
        let issue = fake_issue(
            "uuid-1",
            "RIG-274",
            "Test werma issue",
            &["analyze", "spec:done", "repo:werma"],
        );
        linear.set_issues_for_label("analyze", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        // No task should be created
        let tasks = db
            .tasks_by_linear_issue("RIG-274", Some("analyst"), false)
            .unwrap();
        assert!(
            tasks.is_empty(),
            "should not create analyst task when spec:done present"
        );

        // The stale "analyze" label should be removed
        let removes = linear.remove_label_calls.borrow();
        assert!(
            removes
                .iter()
                .any(|(id, label)| id == "uuid-1" && label == "analyze"),
            "should remove stale 'analyze' label, got: {removes:?}"
        );
    }

    #[test]
    fn poll_skips_analyst_when_analyze_done_label_present() {
        // RIG-274: issues with analyze:done label should also be skipped
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        let issue = fake_issue(
            "uuid-2",
            "RIG-275",
            "Test werma issue",
            &["analyze", "analyze:done", "repo:werma"],
        );
        linear.set_issues_for_label("analyze", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        let tasks = db
            .tasks_by_linear_issue("RIG-275", Some("analyst"), false)
            .unwrap();
        assert!(
            tasks.is_empty(),
            "should not create analyst task when analyze:done present"
        );
    }

    #[test]
    fn poll_skips_analyst_when_analyze_blocked_label_present() {
        // RIG-300: issues with analyze:blocked label should be skipped
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        let issue = fake_issue(
            "uuid-blocked",
            "RIG-300",
            "Test werma issue",
            &["analyze", "analyze:blocked", "repo:werma"],
        );
        linear.set_issues_for_label("analyze", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        let tasks = db
            .tasks_by_linear_issue("RIG-300", Some("analyst"), false)
            .unwrap();
        assert!(
            tasks.is_empty(),
            "should not create analyst task when analyze:blocked present"
        );

        // The stale "analyze" trigger label should be removed
        let removes = linear.remove_label_calls.borrow();
        assert!(
            removes
                .iter()
                .any(|(id, label)| id == "uuid-blocked" && label == "analyze"),
            "should remove stale 'analyze' label when analyze:blocked present, got: {removes:?}"
        );
    }

    #[test]
    fn poll_creates_analyst_task_with_issue_id() {
        // RIG-274: analyst tasks must include linear_issue_id for visibility in `werma st`
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        let issue = fake_issue(
            "uuid-happy",
            "FAT-18",
            "Test werma issue",
            &["analyze", "repo:werma"],
        );
        linear.set_issues_for_label("analyze", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        let tasks = db
            .tasks_by_linear_issue("FAT-18", Some("analyst"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1, "should create exactly one analyst task");
        assert_eq!(
            tasks[0].linear_issue_id, "FAT-18",
            "analyst task must have linear_issue_id set"
        );
        assert_eq!(tasks[0].pipeline_stage, "analyst");
    }

    #[test]
    fn poll_circuit_breaker_blocks_excessive_reviewer_spawns() {
        // RIG-309: after max_review_rounds * 2 reviewer tasks, poller should stop spawning
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        // Insert 6 completed+pushed reviewer tasks (max_review_rounds=3, limit=3*2=6)
        for i in 0..6 {
            let mut task = crate::db::make_test_task(&format!("20260326-rev{i}"));
            task.status = crate::models::Status::Completed;
            task.linear_issue_id = "RIG-309".to_string();
            task.pipeline_stage = "reviewer".to_string();
            task.linear_pushed = true;
            db.insert_task(&task).unwrap();
        }

        // Issue in Review status
        let issue = serde_json::json!({
            "id": "uuid-309",
            "identifier": "RIG-309",
            "title": "Fix reviewer",
            "description": "Fix it",
            "state": {"type": "started"},
            "estimate": 3,
            "labels": {"nodes": [{"name": "repo:werma"}]}
        });
        linear.set_issues_for_status("review", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        // No new reviewer task should be created
        let tasks = db
            .tasks_by_linear_issue("RIG-309", Some("reviewer"), true)
            .unwrap();
        assert!(
            tasks.is_empty(),
            "circuit breaker should prevent spawning new reviewer task"
        );
    }

    #[test]
    fn poll_skips_analyst_when_engineer_already_ran() {
        // RIG-274: don't re-run analyst if engineer has already started for the issue
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        // Insert a completed engineer task for this issue
        let mut engineer_task = crate::db::make_test_task("20260324-eng");
        engineer_task.status = crate::models::Status::Completed;
        engineer_task.linear_issue_id = "RIG-280".to_string();
        engineer_task.pipeline_stage = "engineer".to_string();
        db.insert_task(&engineer_task).unwrap();

        let issue = fake_issue(
            "uuid-eng",
            "RIG-280",
            "Test werma issue",
            &["analyze", "repo:werma"],
        );
        linear.set_issues_for_label("analyze", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        let tasks = db
            .tasks_by_linear_issue("RIG-280", Some("analyst"), false)
            .unwrap();
        assert!(
            tasks.is_empty(),
            "should not create analyst task when engineer already ran"
        );
    }

    #[test]
    fn poll_creates_engineer_task_for_fat_issue_with_correct_identifier() {
        // RIG-307: FAT team issues in In Progress must get correct FAT-XX identifier
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        let issue = fake_issue_with_state(
            "uuid-fat-37",
            "FAT-37",
            "Fix fathom order book sync",
            &["Feature", "repo:werma"],
            "started",
        );
        linear.set_issues_for_status("in_progress", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        let tasks = db
            .tasks_by_linear_issue("FAT-37", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "should create exactly one engineer task for FAT-37"
        );
        assert_eq!(
            tasks[0].linear_issue_id, "FAT-37",
            "engineer task must have FAT-37 as linear_issue_id"
        );
        assert_eq!(tasks[0].pipeline_stage, "engineer");
    }

    #[test]
    fn poll_creates_engineer_task_for_rig_issue_still_works() {
        // RIG-307: RIG team issues should still work as before
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        let issue = fake_issue_with_state(
            "uuid-rig-100",
            "RIG-100",
            "Improve werma dashboard",
            &["Feature", "repo:werma"],
            "started",
        );
        linear.set_issues_for_status("in_progress", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        let tasks = db
            .tasks_by_linear_issue("RIG-100", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "should create exactly one engineer task for RIG-100"
        );
        assert_eq!(tasks[0].linear_issue_id, "RIG-100");
        assert_eq!(tasks[0].pipeline_stage, "engineer");
    }

    #[test]
    fn poll_skips_issue_with_empty_identifier() {
        // RIG-307: issues with empty identifier should be skipped to prevent ghost tasks
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        // Issue with empty identifier (malformed API response)
        let issue = fake_issue_with_state(
            "uuid-no-ident",
            "",
            "Issue with missing identifier",
            &["Feature", "repo:werma"],
            "started",
        );
        linear.set_issues_for_status("in_progress", vec![issue]);

        poll(&db, &linear, &cmd).unwrap();

        // No tasks should be created
        let all_tasks = db.list_tasks(None).unwrap();
        assert!(
            all_tasks.is_empty(),
            "should not create tasks for issues with empty identifier"
        );
    }

    #[test]
    fn poll_dedup_prevents_duplicate_fat_engineer_tasks() {
        // RIG-307: second poll cycle should not create duplicate tasks
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        let issue = fake_issue_with_state(
            "uuid-fat-42",
            "FAT-42",
            "Add per-symbol metrics",
            &["Feature", "repo:werma"],
            "started",
        );
        linear.set_issues_for_status("in_progress", vec![issue.clone()]);

        // First poll — creates task
        poll(&db, &linear, &cmd).unwrap();
        let tasks = db
            .tasks_by_linear_issue("FAT-42", Some("engineer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);

        // Second poll — should not create duplicate
        poll(&db, &linear, &cmd).unwrap();
        let tasks = db
            .tasks_by_linear_issue("FAT-42", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "second poll should not create duplicate engineer task for FAT-42"
        );
    }
}
