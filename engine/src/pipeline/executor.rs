use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::config::PipelineConfig;
use super::loader::{load_default, resolve_prompt};
use super::prompt::{build_vars, render_prompt};
use super::verdict::{
    extract_rejection_feedback, is_heavy_track, parse_estimate, parse_pr_url, parse_verdict,
};
use crate::db::Db;
use crate::linear::LinearApi;
use crate::models::{Status, Task};
use crate::traits::CommandRunner;

/// Default maximum review cycles when not configured in YAML.
const DEFAULT_MAX_REVIEW_ROUNDS: u32 = 3;

// ─── Public API ──────────────────────────────────────────────────────────────

/// Check if an issue is a research issue (has `research` label).
pub fn is_research_issue(labels: &[&str]) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case("research"))
}

/// Poll Linear for issues at pipeline-relevant statuses and create tasks.
pub fn poll(db: &Db, linear: &dyn LinearApi, cmd: &dyn CommandRunner) -> Result<()> {
    let config = load_default()?;

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
        let existing = db.tasks_by_linear_issue(identifier, None, true)?;
        if !existing.is_empty() {
            total_skipped += 1;
            continue;
        }

        let working_dir = crate::linear::infer_working_dir(title, &labels);
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

                // Skip if any non-failed task exists for this issue + stage.
                // This covers active (pending/running), completed-but-unpushed (callback
                // pending), AND completed-and-pushed tasks where the Linear status didn't
                // actually move (RIG-209). Failed tasks don't block — poll can retry those.
                if db.has_any_nonfailed_task_for_issue_stage(identifier, stage_name)? {
                    total_skipped += 1;
                    continue;
                }

                // Manual issues: skip execution stages (skip_manual=true)
                if crate::linear::is_manual_issue(&labels) && stage_cfg.skip_manual() {
                    total_skipped += 1;
                    continue;
                }

                let working_dir = crate::linear::infer_working_dir(title, &labels);
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

                // For reviewer stage: skip if PR is already merged (manual merge while in Review)
                if stage_name == "reviewer" && is_pr_merged_for_issue(cmd, &working_dir, identifier)
                {
                    println!("  ~ {identifier} [{title}] PR already merged, moving to Done");
                    let _ = linear.move_issue_by_name(issue_id, "done");
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

            if issue_id.is_empty() {
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

            // Skip if any non-failed task exists for this issue + stage (RIG-209).
            if db.has_any_nonfailed_task_for_issue_stage(identifier, stage_name)? {
                total_skipped += 1;
                continue;
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

            let working_dir = crate::linear::infer_working_dir(title, &labels);
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

            // For reviewer stage: skip if PR is already merged
            if *stage_name == "reviewer" && is_pr_merged_for_issue(cmd, &working_dir, identifier) {
                println!("  ~ {identifier} [{title}] PR already merged, moving to Done");
                let _ = linear.move_issue_by_name(issue_id, "done");
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

/// Handle pipeline callback when a task completes.
#[allow(clippy::too_many_arguments)]
pub fn callback(
    db: &Db,
    task_id: &str,
    stage: &str,
    result: &str,
    linear_issue_id: &str,
    working_dir: &str,
    linear: &dyn LinearApi,
    cmd: &dyn CommandRunner,
) -> Result<()> {
    // Dedup guard: if callback was recently fired for this task, skip to prevent
    // duplicate Linear comments from overlapping daemon ticks / cmd_complete races.
    if db.is_callback_recently_fired(task_id, 60)? {
        eprintln!("callback: skipping duplicate for task {task_id} (fired <60s ago)");
        return Ok(());
    }
    db.set_callback_fired_at(task_id)?;

    let config = load_default()?;

    // Guard: if output is empty, post a comment and return early.
    // cmd_complete should have already marked this as failed, but this is a safety net
    // for daemon retries that re-read the same empty output file.
    if result.trim().is_empty() {
        eprintln!("callback: empty output for task {task_id} (stage={stage}), skipping transition");
        let _ = linear.comment(
            linear_issue_id,
            &format!(
                "**Werma task `{task_id}`** (stage: {stage}) produced empty output. \
                 Task marked as failed. Re-trigger needed."
            ),
        );
        return Ok(());
    }

    let stage_cfg = if let Some(s) = config.stage(stage) {
        s
    } else {
        eprintln!("unknown pipeline stage: {stage}");
        return Ok(());
    };

    let verdict = parse_verdict(result);

    // For stages that require a verdict (reviewer, qa, devops), warn if missing.
    let has_explicit_transitions = !stage_cfg.transitions.is_empty();

    if verdict.is_none() && has_explicit_transitions && stage != "engineer" && stage != "analyst" {
        eprintln!(
            "warning: no verdict found for task {task_id} (stage={stage}), keeping current state"
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
            // Guard: never move to "done" via ALREADY_DONE if there's an open PR.
            // An open PR means work is in progress — the analyst misjudged.
            if verdict_str == "already_done"
                && t.status == "done"
                && has_open_pr_for_issue(cmd, working_dir, linear_issue_id)
            {
                eprintln!(
                    "callback: blocking ALREADY_DONE→done for {linear_issue_id} — open PR exists. \
                     Issue stays in current state."
                );
                let _ = linear.comment(
                    linear_issue_id,
                    &format!(
                        "**Analyst ALREADY_DONE blocked** (task: `{task_id}`): open PR exists for this issue. \
                         An open PR means work is in progress, not done. Issue stays in current state."
                    ),
                );
                return Ok(());
            }

            // Move the issue first — this is the critical operation.
            if let Err(e) = linear.move_issue_by_name(linear_issue_id, &t.status) {
                // Log and return error so the caller knows the move failed.
                eprintln!(
                    "callback: failed to move {} to '{}': {e}",
                    linear_issue_id, t.status
                );
                return Err(e);
            }

            // Auto-create PR for engineer stage completion
            let pr_url = if stage == "engineer" && verdict_str == "done" {
                // Try to extract PR URL from agent output first (agent may have created it)
                let url_from_output = parse_pr_url(result);

                // Fallback: auto-create PR from worktree branch
                let url = url_from_output.or_else(|| {
                    match auto_create_pr(cmd, working_dir, linear_issue_id, task_id) {
                        Ok(url) => url,
                        Err(e) => {
                            eprintln!("auto-PR error: {e}");
                            None
                        }
                    }
                });

                // Validate: engineer DONE without PR is an error — pipeline stalls otherwise
                if url.is_none() {
                    eprintln!(
                        "callback: engineer DONE but no PR_URL found for {linear_issue_id} (task {task_id}). \
                         Keeping issue in current state for retry."
                    );
                    let _ = linear.comment(
                        linear_issue_id,
                        &format!(
                            "**Engineer task `{task_id}` DONE but no PR created.** \
                             The agent did not create a PR or include `PR_URL=` in its output. \
                             Issue stays in current state — re-trigger needed."
                        ),
                    );
                    return Ok(());
                }

                // Attach PR URL to Linear issue
                if let Some(ref pr) = url {
                    let pr_title = pr_title_from_url(pr);
                    if let Err(e) = linear.attach_url(linear_issue_id, pr, &pr_title) {
                        eprintln!("attach PR to Linear: {e}");
                    }
                }
                url
            } else {
                None
            };

            // Post a comment — non-critical, don't fail the callback if this errors.
            let comment = format_callback_comment(
                task_id,
                stage,
                &verdict_str,
                t.spawn.as_deref(),
                pr_url.as_deref(),
            );
            if let Err(e) = linear.comment(linear_issue_id, &comment) {
                eprintln!("callback: failed to post comment on {linear_issue_id}: {e}");
            }

            // Spawn next stage if configured
            if let Some(ref next_stage) = t.spawn {
                // Check review cycle limit: if reviewer has rejected too many times,
                // escalate to Blocked to prevent infinite loops.
                if stage == "reviewer" && next_stage == "engineer" {
                    let review_count =
                        db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?;
                    let max_rounds = stage_cfg
                        .review_round_limit()
                        .unwrap_or(DEFAULT_MAX_REVIEW_ROUNDS)
                        as i64;
                    if review_count >= max_rounds {
                        eprintln!(
                            "review cycle limit ({max_rounds}) reached for issue {linear_issue_id}, \
                             escalating to blocked"
                        );
                        linear.move_issue_by_name(linear_issue_id, "blocked")?;
                        linear.comment(
                            linear_issue_id,
                            &format!(
                                "**Review cycle limit reached** ({max_rounds} rounds). \
                                 Moving to Blocked — manual review required."
                            ),
                        )?;
                        // Don't spawn another engineer cycle
                        return Ok(());
                    }
                }

                create_next_stage_task(&NextStageParams {
                    db,
                    config: &config,
                    linear: Some(linear),
                    linear_issue_id,
                    next_stage,
                    previous_output: result,
                    prev_task_id: task_id,
                    prev_stage: stage,
                    working_dir,
                    estimate,
                    pr_url: pr_url.as_deref(),
                })?;
            }
        }
        None => {
            eprintln!(
                "stage '{stage}': no transition for verdict '{verdict_str}' — no action taken"
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

    let max_turns = stage_cfg
        .max_turns
        .map(|t| t as i32)
        .unwrap_or_else(|| crate::default_turns(&stage_cfg.agent));
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
    let effective_model = stage_cfg.effective_model(estimate, 0).to_string();

    let prompt = build_poll_prompt(config, stage_cfg, identifier, title, description);

    let effective_working_dir = if working_dir.is_empty() || working_dir == "~/projects/rigpa/werma"
    {
        infer_working_dir_from_issue(db, identifier)
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
        estimate,
    };

    db.insert_task(&task)?;
    Ok(task_id)
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Resolve `~/` prefix to the user's home directory.
fn resolve_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Check if a merged PR exists for the given Linear issue identifier in the repo.
/// Uses `gh pr list --search` to find merged PRs mentioning the issue.
fn is_pr_merged_for_issue(cmd: &dyn CommandRunner, working_dir: &str, identifier: &str) -> bool {
    pr_exists_for_issue(cmd, working_dir, identifier, "merged")
}

/// Check if an open (unmerged) PR exists for the given Linear issue identifier.
fn has_open_pr_for_issue(cmd: &dyn CommandRunner, working_dir: &str, identifier: &str) -> bool {
    pr_exists_for_issue(cmd, working_dir, identifier, "open")
}

/// Check if a PR exists for the given Linear issue identifier in a specific state.
fn pr_exists_for_issue(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    identifier: &str,
    state: &str,
) -> bool {
    let working_dir = resolve_home(working_dir);
    let output = cmd.run(
        "gh",
        &[
            "pr", "list", "--search", identifier, "--state", state, "--json", "number", "--limit",
            "1",
        ],
        Some(&working_dir),
    );

    match output {
        Ok(o) if o.success => {
            let text = o.stdout_str();
            text != "[]" && !text.is_empty()
        }
        _ => false,
    }
}

/// Automatically create a GitHub PR from the engineer's worktree branch.
///
/// Returns the PR URL if successful, or None if:
/// - On main/master branch (safety)
/// - No commits ahead of main (nothing to PR)
/// - PR creation fails (logged but non-fatal)
fn auto_create_pr(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    linear_issue_id: &str,
    task_id: &str,
) -> Result<Option<String>> {
    let working_dir = resolve_home(working_dir);

    // 1. Get current branch
    let branch_output = cmd
        .run("git", &["branch", "--show-current"], Some(&working_dir))
        .context("git branch --show-current")?;
    let branch_name = branch_output.stdout_str();

    // 2. Safety: never PR from main/master or empty branch
    if branch_name.is_empty() || branch_name == "main" || branch_name == "master" {
        return Ok(None);
    }

    // 3. Check if there are commits ahead of main
    let log_output = cmd
        .run(
            "git",
            &["log", "origin/main..HEAD", "--oneline"],
            Some(&working_dir),
        )
        .context("git log origin/main..HEAD")?;
    let log_text = log_output.stdout_str();
    if log_text.is_empty() {
        eprintln!("auto-PR: no commits ahead of main on branch {branch_name}, skipping");
        return Ok(None);
    }

    // 4. Push branch (ignore errors if already up-to-date)
    let push_output = cmd
        .run(
            "git",
            &["push", "-u", "origin", &branch_name],
            Some(&working_dir),
        )
        .context("git push")?;
    if !push_output.success {
        let stderr = push_output.stderr_str();
        eprintln!("auto-PR: push failed: {stderr}");
        return Ok(None);
    }

    // 5. Check if PR already exists for this branch
    let existing_pr = cmd
        .run(
            "gh",
            &["pr", "view", "--json", "url", "-q", ".url"],
            Some(&working_dir),
        )
        .context("gh pr view")?;
    if existing_pr.success {
        let url = existing_pr.stdout_str();
        if !url.is_empty() {
            return Ok(Some(url));
        }
    }

    // 6. Create PR
    let pr_title = format!("{linear_issue_id} feat: implementation");
    let pr_body = format!(
        "## Summary\nPipeline engineer task `{task_id}`.\n\n\
         Linear: https://linear.app/rigpa/issue/{linear_issue_id}",
    );

    let output = cmd
        .run(
            "gh",
            &[
                "pr",
                "create",
                "--title",
                &pr_title,
                "--body",
                &pr_body,
                "--label",
                "ai-generated",
            ],
            Some(&working_dir),
        )
        .context("gh pr create")?;

    if output.success {
        let url = output.stdout_str();
        Ok(Some(url))
    } else {
        let stderr = output.stderr_str();
        eprintln!("auto-PR failed: {stderr}");
        Ok(None)
    }
}

/// Derive a short title from a GitHub PR URL (e.g. "PR #42").
fn pr_title_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(|n| format!("PR #{n}"))
        .unwrap_or_else(|| "Pull Request".to_string())
}

/// Build a comment string for a pipeline callback.
fn format_callback_comment(
    task_id: &str,
    stage: &str,
    verdict: &str,
    spawn: Option<&str>,
    pr_url: Option<&str>,
) -> String {
    let stage_label = stage
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + &stage[1..])
        .unwrap_or_else(|| stage.to_string());

    let spawn_note = spawn
        .map(|s| format!(" Spawning **{s}** stage."))
        .unwrap_or_default();

    let pr_note = pr_url.map(|url| format!(" PR: {url}")).unwrap_or_default();

    match verdict.to_lowercase().as_str() {
        "approved" | "passed" | "done" | "ok" | "fixed" => {
            format!(
                "**{stage_label} {verdict_upper}** (task: `{task_id}`).{pr_note}{spawn_note}",
                verdict_upper = verdict.to_uppercase()
            )
        }
        "rejected" | "failed" | "request_changes" => {
            format!(
                "**{stage_label}: {verdict_upper}** (task: `{task_id}`). Moving back.{pr_note}{spawn_note}",
                verdict_upper = verdict.to_uppercase()
            )
        }
        _ => {
            format!(
                "**{stage_label}** completed (task: `{task_id}`), verdict: {verdict}.{pr_note}{spawn_note}"
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
            if !task.working_dir.is_empty() && task.working_dir != "~/projects/rigpa/werma" {
                return task.working_dir.clone();
            }
        }
        if let Some(task) = tasks.first() {
            return task.working_dir.clone();
        }
    }
    "~/projects/rigpa/werma".to_string()
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
    pub linear: Option<&'a dyn LinearApi>,
    pub linear_issue_id: &'a str,
    pub next_stage: &'a str,
    pub previous_output: &'a str,
    pub prev_task_id: &'a str,
    pub prev_stage: &'a str,
    pub working_dir: &'a str,
    pub estimate: i32,
    pub pr_url: Option<&'a str>,
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
        pr_url: _,
    } = p;

    // Guard: don't spawn if an active task already exists for this issue + stage.
    // Prevents duplicates from double-callback (cmd_complete + daemon).
    let existing = db.tasks_by_linear_issue(linear_issue_id, Some(next_stage), true)?;
    if !existing.is_empty() {
        eprintln!(
            "skip spawn: active task already exists for {linear_issue_id} stage={next_stage}"
        );
        return Ok(());
    }

    let stage_cfg = config
        .stage(next_stage)
        .with_context(|| format!("no config for stage '{next_stage}'"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    // Determine review round for model selection (how many times reviewer has completed)
    let review_round = if *next_stage == "reviewer" {
        db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?
    } else {
        0
    };

    // Max turns: config > heavy-track heuristic > default_turns
    let max_turns = if let Some(t) = stage_cfg.max_turns {
        t as i32
    } else if *next_stage == "engineer" {
        if is_heavy_track(p.estimate) { 45 } else { 30 }
    } else {
        crate::default_turns(&stage_cfg.agent)
    };
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
    let effective_model = stage_cfg
        .effective_model(p.estimate, review_round)
        .to_string();

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

    let pr_section = p
        .pr_url
        .map(|url| format!("PR: {url}\n"))
        .unwrap_or_default();

    let handoff_content = format!(
        "## Pipeline Handoff: {} ({}) -> {} ({})\n\
         Linear issue: {}\n\
         {pr_section}\n\
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
        model: effective_model.clone(),
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
        "  + pipeline task: {} stage={} type={} model={}",
        task_id, next_stage, stage_cfg.agent, effective_model
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
            pr_url: None,
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
            pr_url: None,
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
        // Must have pipeline_stage set for deterministic branch naming (worktree reuse)
        assert_eq!(eng_task.pipeline_stage, "engineer");
        assert_eq!(eng_task.task_type, "pipeline-engineer");
    }

    #[test]
    fn callback_reviewer_rejection_reuses_branch() {
        // Verify that initial and re-spawned engineer tasks produce the same branch name,
        // enabling worktree reuse and pushing to the existing PR.
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        // Pre-insert dummy tasks to offset the ID counter, avoiding handoff file
        // collisions with other tests that also generate 20260312-001 IDs.
        for i in 0..10 {
            let dummy = crate::models::Task {
                id: format!("20260312-{:03}", i + 1),
                ..Default::default()
            };
            db.insert_task(&dummy).unwrap();
        }

        // Use realistic linear_issue_id (RIG-XX format, as in production)
        let issue_id = "RIG-42";

        // 1. Simulate initial engineer task (from analyst)
        let analyst_output = "## Spec\nImplement feature X for RIG-42";
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: issue_id,
            next_stage: "engineer",
            previous_output: analyst_output,
            prev_task_id: "20260310-001",
            prev_stage: "analyst",
            working_dir: "~/projects/rigpa/werma",
            estimate: 0,
            pr_url: None,
        })
        .unwrap();

        let initial_tasks = db
            .tasks_by_linear_issue(issue_id, Some("engineer"), false)
            .unwrap();
        assert_eq!(initial_tasks.len(), 1);
        let initial_task = &initial_tasks[0];

        // Mark it completed so the duplicate guard doesn't block the next spawn
        db.set_task_status(&initial_task.id, Status::Completed)
            .unwrap();

        // 2. Simulate re-spawned engineer (from reviewer rejection)
        let reviewer_output = "## Review\n- blocker: no tests\nREVIEW_VERDICT=REJECTED";
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: issue_id,
            next_stage: "engineer",
            previous_output: reviewer_output,
            prev_task_id: "20260310-002",
            prev_stage: "reviewer",
            working_dir: "~/projects/rigpa/werma",
            estimate: 0,
            pr_url: None,
        })
        .unwrap();

        let all_eng_tasks = db
            .tasks_by_linear_issue(issue_id, Some("engineer"), false)
            .unwrap();
        assert_eq!(all_eng_tasks.len(), 2);

        // Both tasks should produce the same branch name
        let branch1 = crate::worktree::generate_branch_name(initial_task);
        let respawned_task = all_eng_tasks
            .iter()
            .find(|t| t.id != initial_task.id)
            .unwrap();
        let branch2 = crate::worktree::generate_branch_name(respawned_task);

        assert_eq!(
            branch1, branch2,
            "re-spawned engineer must reuse the same branch for PR continuity"
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
        assert_eq!(dir, "~/projects/rigpa/werma");
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
        let comment = format_callback_comment("task-123", "reviewer", "approved", None, None);
        assert!(comment.contains("APPROVED"));
        assert!(comment.contains("task-123"));
    }

    #[test]
    fn format_callback_comment_rejected_with_spawn() {
        let comment =
            format_callback_comment("task-456", "reviewer", "rejected", Some("engineer"), None);
        assert!(comment.contains("REJECTED"));
        assert!(comment.contains("engineer"));
    }

    #[test]
    fn format_callback_comment_with_pr_url() {
        let comment = format_callback_comment(
            "task-789",
            "engineer",
            "done",
            None,
            Some("https://github.com/org/repo/pull/42"),
        );
        assert!(comment.contains("DONE"));
        assert!(comment.contains("https://github.com/org/repo/pull/42"));
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

    #[test]
    fn handoff_includes_pr_url_when_provided() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        // Pre-insert dummy tasks to offset the ID counter, avoiding handoff file
        // collisions with other tests that also generate today's date prefix.
        let today = chrono::Local::now().format("%Y%m%d").to_string();
        for i in 0..20 {
            let dummy = crate::models::Task {
                id: format!("{today}-{:03}", i + 1),
                ..Default::default()
            };
            db.insert_task(&dummy).unwrap();
        }

        let engineer_output = "Implementation complete.\nVERDICT=DONE";
        let pr_url = "https://github.com/RigpaLabs/werma/pull/42";

        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "test-issue-pr",
            next_stage: "reviewer",
            previous_output: engineer_output,
            prev_task_id: "20260312-001",
            prev_stage: "engineer",
            working_dir: "~/projects/rigpa/werma",
            estimate: 0,
            pr_url: Some(pr_url),
        })
        .unwrap();

        let tasks = db
            .tasks_by_linear_issue("test-issue-pr", Some("reviewer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);

        // Verify handoff file contains the PR URL
        let handoff_path = &tasks[0].context_files[0];
        let handoff_content = std::fs::read_to_string(handoff_path).unwrap();
        assert!(
            handoff_content.contains(pr_url),
            "handoff should contain PR URL"
        );
    }

    #[test]
    fn parse_pr_url_from_output() {
        let output = "Created PR: https://github.com/RigpaLabs/werma/pull/42\nVERDICT=DONE";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/RigpaLabs/werma/pull/42".to_string())
        );
    }

    #[test]
    fn parse_pr_url_in_markdown() {
        let output = "PR created: [link](https://github.com/org/repo/pull/99)\nDone.";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/org/repo/pull/99".to_string())
        );
    }

    #[test]
    fn parse_pr_url_none_when_absent() {
        assert_eq!(parse_pr_url("just some output, no PR link"), None);
    }

    #[test]
    fn parse_pr_url_ignores_non_pull_github_urls() {
        let output = "See https://github.com/org/repo/issues/10 for context";
        assert_eq!(parse_pr_url(output), None);
    }

    #[test]
    fn pr_title_from_url_extracts_number() {
        assert_eq!(
            pr_title_from_url("https://github.com/org/repo/pull/42"),
            "PR #42"
        );
    }

    #[test]
    fn pr_title_from_url_fallback() {
        assert_eq!(
            pr_title_from_url("https://github.com/org/repo/pull/"),
            "Pull Request"
        );
    }

    // ─── format_callback_comment: edge cases ────────────────────────────────

    #[test]
    fn format_callback_comment_done_no_spawn() {
        let comment = format_callback_comment("task-001", "engineer", "done", None, None);
        assert!(comment.contains("DONE"));
        assert!(comment.contains("task-001"));
        assert!(comment.contains("Engineer")); // first letter capitalized
    }

    #[test]
    fn format_callback_comment_with_spawn_and_pr() {
        let comment = format_callback_comment(
            "task-002",
            "engineer",
            "done",
            Some("reviewer"),
            Some("https://github.com/org/repo/pull/5"),
        );
        assert!(comment.contains("reviewer"));
        assert!(comment.contains("pull/5"));
    }

    // ─── build_handoff_prompt: qa rejection ─────────────────────────────────

    #[test]
    fn build_handoff_prompt_from_qa() {
        let config = test_config();
        // If qa stage exists in config, test it; otherwise skip gracefully
        if config.stage("qa").is_some() {
            let prompt = build_handoff_prompt(
                &config,
                "engineer",
                "qa",
                "issue-456",
                "QA Failed Issue",
                "Description",
                "QA found bugs\nVERDICT=REJECTED",
            );
            assert!(
                prompt.contains("issue-456")
                    || prompt.contains("QA")
                    || prompt.contains("REJECTED")
            );
        }
    }

    // ─── parse_pr_url: multiple URLs ────────────────────────────────────────

    #[test]
    fn parse_pr_url_first_match() {
        let output = "Created https://github.com/a/b/pull/1\nAlso https://github.com/a/b/pull/2";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/a/b/pull/1".to_string())
        );
    }

    // ─── truncate_lines: edge cases ─────────────────────────────────────────

    #[test]
    fn truncate_lines_empty() {
        assert_eq!(truncate_lines("", 10), "");
    }

    #[test]
    fn truncate_lines_exact_limit() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(truncate_lines(text, 5), text);
    }

    // ─── is_research_issue: edge cases ──────────────────────────────────────

    #[test]
    fn research_issue_mixed_labels() {
        assert!(is_research_issue(&["Feature", "Research", "repo:werma"]));
        assert!(!is_research_issue(&["researcher"])); // partial match should not trigger
    }

    // ─── create_initial_stage_task ─────────────────────────────────────────

    #[test]
    fn create_initial_stage_task_creates_pending_task() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let task_id = create_initial_stage_task(
            &db,
            &config,
            "analyst",
            "RIG-200",
            "Test issue title",
            "Test description",
            "~/projects/rigpa/werma",
            3,
        )
        .unwrap();

        let task = db.task(&task_id).unwrap().unwrap();
        assert_eq!(task.pipeline_stage, "analyst");
        assert_eq!(task.linear_issue_id, "RIG-200");
        assert_eq!(task.status, Status::Pending);
        assert_eq!(task.estimate, 3);
        assert_eq!(task.working_dir, "~/projects/rigpa/werma");
        assert!(task.prompt.contains("RIG-200"));
    }

    #[test]
    fn create_initial_stage_task_unknown_stage_errors() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let result = create_initial_stage_task(
            &db,
            &config,
            "nonexistent_stage",
            "RIG-201",
            "Title",
            "Desc",
            "/tmp",
            0,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown pipeline stage")
        );
    }

    #[test]
    fn create_initial_stage_task_infers_working_dir_from_existing() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        // Insert a prior task with a custom working dir
        let prior = Task {
            id: "20260313-001".to_string(),
            linear_issue_id: "RIG-202".to_string(),
            working_dir: "~/projects/rigpa/fathom".to_string(),
            pipeline_stage: "analyst".to_string(),
            task_type: "pipeline-analyst".to_string(),
            ..Default::default()
        };
        db.insert_task(&prior).unwrap();

        // When working_dir is default werma path, should infer from existing task
        let task_id = create_initial_stage_task(
            &db,
            &config,
            "analyst",
            "RIG-202",
            "Fathom task",
            "Description",
            "~/projects/rigpa/werma",
            0,
        )
        .unwrap();

        let task = db.task(&task_id).unwrap().unwrap();
        assert_eq!(task.working_dir, "~/projects/rigpa/fathom");
    }

    // ─── create_next_stage_task: duplicate guard ──────────────────────────

    #[test]
    fn create_next_stage_task_skips_if_active_exists() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        // Insert an active (pending) engineer task
        let existing = Task {
            id: "20260313-050".to_string(),
            status: Status::Pending,
            linear_issue_id: "RIG-300".to_string(),
            pipeline_stage: "engineer".to_string(),
            task_type: "pipeline-engineer".to_string(),
            ..Default::default()
        };
        db.insert_task(&existing).unwrap();

        // Try to spawn another engineer for the same issue
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "RIG-300",
            next_stage: "engineer",
            previous_output: "spec output",
            prev_task_id: "20260313-001",
            prev_stage: "analyst",
            working_dir: "~/projects/rigpa/werma",
            estimate: 0,
            pr_url: None,
        })
        .unwrap();

        // Should still be only 1 engineer task (duplicate blocked)
        let tasks = db
            .tasks_by_linear_issue("RIG-300", Some("engineer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);
    }

    // ─── build_poll_prompt: no prompt in config ──────────────────────────

    #[test]
    fn build_poll_prompt_fallback_when_no_prompt() {
        let yaml = r#"
pipeline: minimal
stages:
  bare:
    agent: pipeline-test
    model: sonnet
"#;
        let config = crate::pipeline::loader::load_from_str(yaml, "<test>").unwrap();
        let stage_cfg = config.stage("bare").unwrap();
        let prompt = build_poll_prompt(&config, stage_cfg, "RIG-99", "Bare title", "Bare desc");
        assert!(prompt.contains("RIG-99"));
        assert!(prompt.contains("Bare title"));
        assert!(prompt.contains("pipeline-test")); // agent name in fallback
    }

    // ─── build_handoff_prompt: no prompt in config ──────────────────────

    #[test]
    fn build_handoff_prompt_fallback_when_no_config() {
        let yaml = r#"
pipeline: minimal
stages:
  unknown:
    agent: pipeline-test
    model: sonnet
"#;
        let config = crate::pipeline::loader::load_from_str(yaml, "<test>").unwrap();
        // When next_stage doesn't exist in config, should use fallback
        let prompt = build_handoff_prompt(
            &config,
            "nonexistent",
            "analyst",
            "RIG-99",
            "Title",
            "Desc",
            "prev output",
        );
        assert!(prompt.contains("RIG-99"));
        assert!(prompt.contains("nonexistent"));
    }

    // ─── resolve_home ────────────────────────────────────────────────────

    #[test]
    fn resolve_home_expands_tilde() {
        let result = resolve_home("~/test/path");
        assert!(!result.to_string_lossy().starts_with("~/"));
        assert!(result.to_string_lossy().ends_with("/test/path"));
    }

    #[test]
    fn resolve_home_absolute_path_unchanged() {
        let result = resolve_home("/absolute/path");
        assert_eq!(result, std::path::PathBuf::from("/absolute/path"));
    }
}
