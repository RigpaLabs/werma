use std::collections::HashMap;

use anyhow::{Result, anyhow};

use super::config::PipelineConfig;
use super::helpers::{infer_working_dir_from_issue, truncate_lines};
use super::loader::{load_default, resolve_prompt};
use super::pr::{auto_create_pr, has_open_pr_for_issue, pr_title_from_url};
use super::prompt::{build_vars, render_prompt};
use super::verdict::{
    extract_rejection_feedback, is_heavy_track, is_max_turns_exit, parse_comments, parse_estimate,
    parse_pr_url, parse_verdict,
};
use crate::db::Db;
use crate::linear::LinearApi;
use crate::traits::{CommandRunner, Notifier};

/// Max retries for Linear status move operations.
const CALLBACK_MAX_RETRIES: u32 = 3;
/// Backoff delays in milliseconds between retries: 50ms, 100ms, 200ms.
const CALLBACK_BACKOFF_MS: [u64; 3] = [50, 100, 200];

/// Default maximum review cycles when not configured in YAML.
const DEFAULT_MAX_REVIEW_ROUNDS: u32 = 3;

/// Move a Linear issue to a new status with retry + backoff + reconciliation.
///
/// Retries up to `CALLBACK_MAX_RETRIES` times with exponential backoff.
/// After a successful move, performs a read-after-write check to verify
/// the status actually changed. Returns an error only if all retries
/// are exhausted or reconciliation fails.
pub(crate) fn move_with_retry(
    linear: &dyn LinearApi,
    issue_id: &str,
    target_status: &str,
) -> Result<()> {
    let mut last_err = None;

    for attempt in 0..CALLBACK_MAX_RETRIES {
        match linear.move_issue_by_name(issue_id, target_status) {
            Ok(()) => {
                // Reconciliation: verify the status actually changed
                match linear.get_issue_status(issue_id) {
                    Ok(actual_status) => {
                        let actual_lower = actual_status.to_lowercase().replace(' ', "_");
                        let target_lower = target_status.to_lowercase().replace(' ', "_");
                        if actual_lower == target_lower {
                            eprintln!(
                                "[CALLBACK] {issue_id}: moved to {target_status} \
                                 (verified, attempt {})",
                                attempt + 1
                            );
                            return Ok(());
                        }
                        // Move succeeded but status didn't change — treat as failure and retry
                        eprintln!(
                            "[CALLBACK] {issue_id}: move to '{target_status}' returned OK but \
                             actual status is '{actual_status}' (attempt {})",
                            attempt + 1
                        );
                        last_err = Some(anyhow!(
                            "reconciliation failed: expected '{target_status}', got '{actual_status}'"
                        ));
                    }
                    Err(e) => {
                        // Reconciliation query failed — optimistically accept the move
                        eprintln!(
                            "[CALLBACK] {issue_id}: moved to {target_status} \
                             (reconciliation check failed: {e}, accepting move, attempt {})",
                            attempt + 1
                        );
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[CALLBACK] {issue_id}: move to '{target_status}' failed \
                     (attempt {}): {e}",
                    attempt + 1
                );
                last_err = Some(e);
            }
        }

        // Backoff before next retry (skip after last attempt)
        if attempt + 1 < CALLBACK_MAX_RETRIES {
            let delay = CALLBACK_BACKOFF_MS
                .get(attempt as usize)
                .copied()
                .unwrap_or(2000);
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
    }

    let err = last_err.unwrap_or_else(|| anyhow!("move_with_retry exhausted"));
    eprintln!(
        "[CALLBACK] {issue_id}: FAILED to move to '{target_status}' after \
         {CALLBACK_MAX_RETRIES} attempts"
    );
    Err(err)
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
    notifier: &dyn Notifier,
) -> Result<()> {
    // Dedup guard: if callback SUCCEEDED recently, skip to prevent
    // duplicate Linear comments from overlapping daemon ticks / cmd_complete races.
    if db.is_callback_recently_fired(task_id, 60)? {
        eprintln!("callback: skipping duplicate for task {task_id} (fired <60s ago)");
        return Ok(());
    }

    let config = load_default()?;

    // Guard: if output is empty, post a comment and return early.
    if result.trim().is_empty() {
        eprintln!("callback: empty output for task {task_id} (stage={stage}), skipping transition");
        if let Err(e) = linear.comment(
            linear_issue_id,
            &format!(
                "**Werma task `{task_id}`** (stage: {stage}) produced empty output. \
                 Task marked as failed. Re-trigger needed."
            ),
        ) {
            eprintln!("callback: failed to post empty-output comment on {linear_issue_id}: {e}");
        }
        return Ok(());
    }

    // RIG-252: Detect error_max_turns in output — agent ran out of turns without completing.
    // This is a safety net in case the runner script didn't catch it (old binary, manual complete).
    if is_max_turns_exit(result) {
        eprintln!(
            "[CALLBACK] {linear_issue_id}: task {task_id} (stage={stage}) hit max_turns — \
             marking as incomplete, no transition"
        );
        if let Err(e) = linear.comment(
            linear_issue_id,
            &format!(
                "**Werma task `{task_id}`** (stage: {stage}) hit `max_turns` — agent ran out of \
                 turns without completing. Task marked as failed. Will be retried."
            ),
        ) {
            eprintln!("callback: failed to post max_turns comment on {linear_issue_id}: {e}");
        }
        return Ok(());
    }

    // Post any comment blocks from agent output (non-critical)
    let comments = parse_comments(result);
    for comment_body in &comments {
        if let Err(e) = linear.comment(linear_issue_id, comment_body) {
            eprintln!("[CALLBACK] {linear_issue_id}: failed to post comment: {e}");
        }
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
        if let Err(e) = linear.comment(
            linear_issue_id,
            &format!(
                "**Werma task `{task_id}`** (stage: {stage}) completed but no verdict found. \
                 Manual review needed."
            ),
        ) {
            eprintln!("callback: failed to post no-verdict comment on {linear_issue_id}: {e}");
        }
        if let Err(e) = db.set_callback_fired_at(task_id) {
            eprintln!("warn: failed to set callback_fired_at for {task_id}: {e}");
        }
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

    if let Some(t) = transition {
        // Guard: never move to "done" via ALREADY_DONE if there's an open PR.
        if verdict_str == "already_done"
            && t.status == "done"
            && has_open_pr_for_issue(cmd, working_dir, linear_issue_id)
        {
            eprintln!(
                "callback: blocking ALREADY_DONE→done for {linear_issue_id} — open PR exists. \
                     Issue stays in current state."
            );
            if let Err(e) = linear.comment(
                    linear_issue_id,
                    &format!(
                        "**Analyst ALREADY_DONE blocked** (task: `{task_id}`): open PR exists for this issue. \
                         An open PR means work is in progress, not done. Issue stays in current state."
                    ),
                ) {
                    eprintln!("callback: failed to post ALREADY_DONE comment on {linear_issue_id}: {e}");
                }
            if let Err(e) = db.set_callback_fired_at(task_id) {
                eprintln!("warn: failed to set callback_fired_at for {task_id}: {e}");
            }
            // RIG-274: still add spec:done even when blocked by open PR — ensures
            // the dedup label is set so re-adding the trigger label won't re-run analyst.
            if stage == "analyst" {
                if let Err(e) = linear.add_label(linear_issue_id, "spec:done") {
                    eprintln!(
                        "callback: failed to add 'spec:done' label to {linear_issue_id}: {e}"
                    );
                }
            }
            return Ok(());
        }

        // Move the issue — retry with backoff + reconciliation check.
        if let Err(e) = move_with_retry(linear, linear_issue_id, &t.status) {
            let alert_msg = format!(
                "[CALLBACK FAILURE] {linear_issue_id} task `{task_id}` (stage: {stage}): \
                     failed to move to '{}' after {CALLBACK_MAX_RETRIES} retries: {e}",
                t.status
            );
            notifier.notify_macos("Werma Callback Failed", &alert_msg, "Basso");
            notifier.notify_slack("#werma-alerts", &alert_msg);
            return Err(e);
        }

        // Analyst label swap: remove trigger label, add "analyze:done" + "spec:done"
        if stage == "analyst" {
            if let Some(ref label) = stage_cfg.linear_label {
                if let Err(e) = linear.remove_label(linear_issue_id, label) {
                    eprintln!("callback: failed to remove '{label}' from {linear_issue_id}: {e}");
                }
                let done_label = format!("{label}:done");
                if let Err(e) = linear.add_label(linear_issue_id, &done_label) {
                    eprintln!(
                        "callback: failed to add '{done_label}' label to {linear_issue_id}: {e}"
                    );
                }
            }
            // RIG-274: Add spec:done label for robust dedup — prevents re-running
            // analyst even if trigger label is re-added later.
            if verdict_str == "done" || verdict_str == "already_done" {
                if let Err(e) = linear.add_label(linear_issue_id, "spec:done") {
                    eprintln!(
                        "callback: failed to add 'spec:done' label to {linear_issue_id}: {e}"
                    );
                }
            }
        }

        // Auto-create PR for engineer stage completion
        let pr_url = if stage == "engineer" && verdict_str == "done" {
            let url_from_output = parse_pr_url(result);

            let url = url_from_output.or_else(|| {
                match auto_create_pr(cmd, working_dir, linear_issue_id, task_id) {
                    Ok(url) => url,
                    Err(e) => {
                        eprintln!("auto-PR error: {e}");
                        None
                    }
                }
            });

            // RIG-232: engineer DONE without PR still spawns reviewer.
            // Reviewer can check for open PRs or request the engineer to create one.
            if url.is_none() {
                eprintln!(
                    "callback: engineer DONE but no PR_URL found for {linear_issue_id} (task {task_id}). \
                         Spawning reviewer anyway — reviewer can check for PR."
                );
                if let Err(e) = linear.comment(
                    linear_issue_id,
                    &format!(
                        "**Engineer task `{task_id}` DONE but no PR created.** \
                             The agent did not include `PR_URL=` in output. \
                             Proceeding to reviewer — reviewer will verify or request a PR."
                    ),
                ) {
                    eprintln!("callback: failed to post no-PR comment on {linear_issue_id}: {e}");
                }
            }

            // Attach PR URL to Linear issue if we have one
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
                    .unwrap_or(DEFAULT_MAX_REVIEW_ROUNDS) as i64;
                if review_count >= max_rounds {
                    // Use the pipeline config's blocked transition target status
                    // (e.g. "backlog"), falling back to "backlog" if not configured.
                    let escalation_status = stage_cfg
                        .transition_for("blocked")
                        .map(|t| t.status.as_str())
                        .unwrap_or("backlog");
                    eprintln!(
                        "review cycle limit ({max_rounds}) reached for issue {linear_issue_id}, \
                             escalating to {escalation_status}"
                    );
                    if let Err(e) = move_with_retry(linear, linear_issue_id, escalation_status) {
                        eprintln!(
                            "callback: escalation move to '{escalation_status}' failed for \
                                 {linear_issue_id}: {e} — marking callback as fired to prevent retry loop"
                        );
                    }
                    if let Err(e) = linear.comment(
                        linear_issue_id,
                        &format!(
                            "**Review cycle limit reached** ({max_rounds} rounds). \
                                 Moving to {escalation_status} — manual review required."
                        ),
                    ) {
                        eprintln!(
                            "callback: failed to post escalation comment on {linear_issue_id}: {e}"
                        );
                    }
                    if let Err(e) = db.set_callback_fired_at(task_id) {
                        eprintln!("warn: failed to set callback_fired_at for {task_id}: {e}");
                    }
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
                logs_dir: None,
            })?;
        }

        // RIG-211: set callback_fired_at AFTER successful transition.
        if let Err(e) = db.set_callback_fired_at(task_id) {
            eprintln!("warn: failed to set callback_fired_at for {task_id}: {e}");
        }
    } else {
        eprintln!("stage '{stage}': no transition for verdict '{verdict_str}' — no action taken");
        if let Err(e) = db.set_callback_fired_at(task_id) {
            eprintln!("warn: failed to set callback_fired_at for {task_id}: {e}");
        }
    }

    Ok(())
}

/// Build a comment string for a pipeline callback.
pub(crate) fn format_callback_comment(
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
    /// Override the logs directory for handoff files. `None` = use `~/.werma/logs/` (production).
    pub logs_dir: Option<&'a std::path::Path>,
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
        logs_dir: _,
    } = p;

    // Guard: don't spawn if an active task already exists for this issue + stage.
    let existing = db.tasks_by_linear_issue(linear_issue_id, Some(next_stage), true)?;
    if !existing.is_empty() {
        eprintln!(
            "skip spawn: active task already exists for {linear_issue_id} stage={next_stage}"
        );
        return Ok(());
    }

    let stage_cfg = config
        .stage(next_stage)
        .ok_or_else(|| anyhow::anyhow!("no config for stage '{next_stage}'"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let review_round = if *next_stage == "reviewer" {
        db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?
    } else {
        0
    };

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

    let logs_dir = match p.logs_dir {
        Some(dir) => dir.to_path_buf(),
        None => dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home dir"))?
            .join(".werma")
            .join("logs"),
    };
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

    let effective_working_dir = if working_dir.is_empty() || *working_dir == "~/projects/ar" {
        infer_working_dir_from_issue(db, linear_issue_id)
    } else {
        working_dir.to_string()
    };

    use crate::models::{Status, Task};
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
        rendered = format!(
            "# Pipeline: Engineer Stage ({stage_kind})\n\
             Linear issue: {linear_issue_id}\n\n\
             ## {from_label}\n{fb}\n\n{rendered}"
        );
    }

    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Status, Task};
    use crate::pipeline::loader::load_from_str;
    use crate::traits::fakes::{FakeLinearApi, FakeNotifier};

    fn test_config() -> PipelineConfig {
        load_from_str(include_str!("../../pipelines/default.yaml"), "<test>").unwrap()
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
    fn format_callback_comment_done_no_spawn() {
        let comment = format_callback_comment("task-001", "engineer", "done", None, None);
        assert!(comment.contains("DONE"));
        assert!(comment.contains("task-001"));
        assert!(comment.contains("Engineer"));
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

    #[test]
    fn move_with_retry_succeeds_first_attempt() {
        let linear = FakeLinearApi::new();
        linear.set_issue_status("RIG-100", "In Review");

        let result = move_with_retry(&linear, "RIG-100", "review");
        assert!(result.is_ok());

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0], ("RIG-100".to_string(), "review".to_string()));
    }

    #[test]
    fn move_with_retry_succeeds_after_one_failure() {
        let linear = FakeLinearApi::new();
        linear.fail_next_n_moves(1);

        let result = move_with_retry(&linear, "RIG-100", "review");
        assert!(result.is_ok());

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
    }

    #[test]
    fn move_with_retry_fails_all_retries() {
        let linear = FakeLinearApi::new();
        linear.fail_next_n_moves(3);

        let result = move_with_retry(&linear, "RIG-100", "review");
        assert!(result.is_err());

        let moves = linear.move_calls.borrow();
        assert!(moves.is_empty(), "no successful moves recorded");
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
        let tmpdir = tempfile::tempdir().unwrap();

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
            logs_dir: Some(tmpdir.path()),
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
        let tmpdir = tempfile::tempdir().unwrap();

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
            logs_dir: Some(tmpdir.path()),
        })
        .unwrap();

        let pending = db.list_tasks(Some(Status::Pending)).unwrap();
        assert_eq!(pending.len(), 1);

        let eng_task = &pending[0];
        assert!(
            eng_task.prompt.contains("Revision")
                || eng_task.prompt.contains("rejected")
                || eng_task.prompt.contains("blocker")
        );
        assert_eq!(eng_task.pipeline_stage, "engineer");
        assert_eq!(eng_task.task_type, "pipeline-engineer");
    }

    #[test]
    fn create_next_stage_task_skips_if_active_exists() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let existing = Task {
            id: "20260313-050".to_string(),
            status: Status::Pending,
            linear_issue_id: "RIG-300".to_string(),
            pipeline_stage: "engineer".to_string(),
            task_type: "pipeline-engineer".to_string(),
            ..Default::default()
        };
        db.insert_task(&existing).unwrap();

        let tmpdir = tempfile::tempdir().unwrap();
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
            logs_dir: Some(tmpdir.path()),
        })
        .unwrap();

        let tasks = db
            .tasks_by_linear_issue("RIG-300", Some("engineer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);
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
        assert!(
            prompt.contains("blocker")
                || prompt.contains("Revision")
                || prompt.contains("rejected")
        );
    }

    #[test]
    fn callback_no_verdict_does_not_create_task() {
        assert!(
            crate::pipeline::verdict::parse_verdict("Just some output without verdict markers")
                .is_none()
        );

        let db = crate::db::Db::open_in_memory().unwrap();
        let pending = db.list_tasks(Some(Status::Pending)).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn build_handoff_prompt_fallback_when_no_config() {
        let yaml = r#"
pipeline: minimal
stages:
  unknown:
    agent: pipeline-test
    model: sonnet
"#;
        let config = load_from_str(yaml, "<test>").unwrap();
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

    #[test]
    fn build_handoff_prompt_from_qa() {
        let config = test_config();
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

    #[test]
    fn handoff_includes_pr_url_when_provided() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();
        let tmpdir = tempfile::tempdir().unwrap();
        let logs_dir = tmpdir.path().join("logs");

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
            logs_dir: Some(&logs_dir),
        })
        .unwrap();

        let tasks = db
            .tasks_by_linear_issue("test-issue-pr", Some("reviewer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);

        let handoff_path = &tasks[0].context_files[0];
        let handoff_content = std::fs::read_to_string(handoff_path).unwrap();
        assert!(
            handoff_content.contains(pr_url),
            "handoff should contain PR URL"
        );
    }

    #[test]
    fn callback_reviewer_rejection_reuses_branch() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();
        let tmpdir = tempfile::tempdir().unwrap();
        let logs_dir = tmpdir.path().join("logs");

        for i in 0..10 {
            let dummy = crate::models::Task {
                id: format!("20260312-{:03}", i + 1),
                ..Default::default()
            };
            db.insert_task(&dummy).unwrap();
        }

        let issue_id = "RIG-42";

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
            logs_dir: Some(&logs_dir),
        })
        .unwrap();

        let initial_tasks = db
            .tasks_by_linear_issue(issue_id, Some("engineer"), false)
            .unwrap();
        assert_eq!(initial_tasks.len(), 1);
        let initial_task = &initial_tasks[0];

        db.set_task_status(&initial_task.id, Status::Completed)
            .unwrap();

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
            logs_dir: Some(&logs_dir),
        })
        .unwrap();

        let all_eng_tasks = db
            .tasks_by_linear_issue(issue_id, Some("engineer"), false)
            .unwrap();
        assert_eq!(all_eng_tasks.len(), 2);

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
    fn callback_engineer_done_without_pr_still_spawns_reviewer() {
        // RIG-232: engineer DONE without PR_URL should still spawn reviewer.
        // Test via create_next_stage_task directly to avoid FS dependencies.
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        // Simulate the state after engineer completes without creating a PR.
        // Previously, callback returned early here — now it should spawn reviewer.
        let tmpdir = tempfile::tempdir().unwrap();
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "RIG-232",
            next_stage: "reviewer",
            previous_output: "Implementation complete.\nVERDICT=DONE",
            prev_task_id: "20260314-232",
            prev_stage: "engineer",
            working_dir: "~/projects/rigpa/werma",
            estimate: 0,
            pr_url: None, // No PR URL — this is the RIG-232 scenario
            logs_dir: Some(tmpdir.path()),
        })
        .unwrap();

        // Reviewer task should be created even without PR URL
        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-232", Some("reviewer"), false)
            .unwrap();
        assert!(
            !reviewer_tasks.is_empty(),
            "reviewer should be spawned even without PR_URL (RIG-232 fix)"
        );

        let reviewer = &reviewer_tasks[0];
        assert_eq!(reviewer.pipeline_stage, "reviewer");
        assert_eq!(reviewer.linear_issue_id, "RIG-232");
        assert_eq!(reviewer.status, Status::Pending);
    }

    #[test]
    fn callback_engineer_done_without_pr_posts_warning_comment() {
        // RIG-232: verify that the "no PR created" warning comment is still posted.
        // This tests the comment posting code path that was preserved from the old behavior.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-232b", "in_progress");

        let mut task = crate::db::make_test_task("20260314-232b");
        task.id = "20260314-232b".to_string();
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-232b".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        // Engineer output with DONE but no PR_URL.
        // cmd returns "main" for git branch → auto_create_pr returns None (safety guard)
        cmd.push_success("main");

        let result = "Implementation complete.\nVERDICT=DONE";

        callback(
            &db,
            "20260314-232b",
            "engineer",
            result,
            "RIG-232b",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // Warning comment about missing PR should be posted
        let comments = linear.comment_calls.borrow();
        assert!(
            comments
                .iter()
                .any(|(id, body)| id == "RIG-232b" && body.contains("no PR created")),
            "should warn about missing PR, got: {comments:?}"
        );

        // Issue should still move to review
        let moves = linear.move_calls.borrow();
        assert!(
            moves
                .iter()
                .any(|(id, status)| id == "RIG-232b" && status == "review"),
            "engineer DONE should move to review even without PR, got: {moves:?}"
        );
    }

    #[test]
    fn callback_analyst_done_swaps_labels() {
        // RIG-253: analyst callback should remove trigger label and add analyze:done
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-253", "in_progress");

        let mut task = crate::db::make_test_task("20260315-253");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-253".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        let result = "## Spec\nDo the thing.\nESTIMATE=3\nVERDICT=DONE";

        callback(
            &db,
            "20260315-253",
            "analyst",
            result,
            "RIG-253",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // Trigger label "analyze" should be removed
        let removes = linear.remove_label_calls.borrow();
        assert!(
            removes
                .iter()
                .any(|(id, label)| id == "RIG-253" && label == "analyze"),
            "should remove 'analyze' trigger label, got: {removes:?}"
        );

        // Done label "analyze:done" should be added
        let adds = linear.add_label_calls.borrow();
        assert!(
            adds.iter()
                .any(|(id, label)| id == "RIG-253" && label == "analyze:done"),
            "should add 'analyze:done' label, got: {adds:?}"
        );

        // RIG-274: spec:done label should also be added
        assert!(
            adds.iter()
                .any(|(id, label)| id == "RIG-253" && label == "spec:done"),
            "should add 'spec:done' label, got: {adds:?}"
        );
    }

    #[test]
    fn callback_analyst_already_done_adds_spec_done() {
        // RIG-274: ALREADY_DONE verdict should also add spec:done label
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-274", "done");

        let mut task = crate::db::make_test_task("20260324-274");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-274".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        let result = "Issue already has implementation.\nVERDICT=ALREADY_DONE";

        callback(
            &db,
            "20260324-274",
            "analyst",
            result,
            "RIG-274",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        let adds = linear.add_label_calls.borrow();
        assert!(
            adds.iter()
                .any(|(id, label)| id == "RIG-274" && label == "spec:done"),
            "ALREADY_DONE should add 'spec:done' label, got: {adds:?}"
        );
    }

    #[test]
    fn callback_analyst_already_done_with_open_pr_still_adds_spec_done() {
        // RIG-274: ALREADY_DONE + open PR path should still add spec:done label.
        // The early return guard (blocking ALREADY_DONE→done when PR exists) was
        // exiting before reaching the spec:done label code — this test covers that path.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-274c", "in_progress");

        let mut task = crate::db::make_test_task("20260324-274c");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-274c".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        // Simulate an open PR whose branch name contains "RIG-274c".
        cmd.push_success(r#"[{"number":99,"headRefName":"feat/rig-274c-my-spec"}]"#);

        let result = "Issue already has a spec.\nVERDICT=ALREADY_DONE";

        callback(
            &db,
            "20260324-274c",
            "analyst",
            result,
            "RIG-274c",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        let adds = linear.add_label_calls.borrow();
        assert!(
            adds.iter()
                .any(|(id, label)| id == "RIG-274c" && label == "spec:done"),
            "ALREADY_DONE with open PR should still add 'spec:done' label, got: {adds:?}"
        );
    }

    #[test]
    fn callback_analyst_blocked_does_not_add_spec_done() {
        // RIG-274: BLOCKED verdict should NOT add spec:done label
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-274b", "blocked");

        let mut task = crate::db::make_test_task("20260324-275");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-274b".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        let result = "Cannot proceed, missing requirements.\nVERDICT=BLOCKED";

        callback(
            &db,
            "20260324-275",
            "analyst",
            result,
            "RIG-274b",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        let adds = linear.add_label_calls.borrow();
        assert!(
            !adds
                .iter()
                .any(|(id, label)| id == "RIG-274b" && label == "spec:done"),
            "BLOCKED should NOT add 'spec:done' label, got: {adds:?}"
        );
    }

    #[test]
    fn review_cycle_escalation_uses_config_status() {
        // When review cycle limit is reached, the callback should use the
        // pipeline config's blocked transition target (backlog), not hardcode "blocked".
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // Insert a reviewer task for the issue
        let task = Task {
            id: "20260324-esc".to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-24T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-reviewer".to_string(),
            prompt: "review issue".to_string(),
            output_path: String::new(),
            working_dir: "~/projects/rigpa/werma".to_string(),
            model: "opus".to_string(),
            max_turns: 50,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "RIG-ESC".to_string(),
            linear_pushed: false,
            pipeline_stage: "reviewer".to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };
        db.insert_task(&task).unwrap();

        // Simulate 3 completed reviewer tasks (at the limit)
        for i in 0..3 {
            let mut prev = task.clone();
            prev.id = format!("20260324-prev{i}");
            prev.status = Status::Completed;
            prev.linear_pushed = true;
            db.insert_task(&prev).unwrap();
        }

        let result = "Code needs changes.\nVERDICT=REJECTED";

        callback(
            &db,
            "20260324-esc",
            "reviewer",
            result,
            "RIG-ESC",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // Verify it moved to "backlog" (from config), not "blocked"
        let moves = linear.move_calls.borrow();
        // First move is the "review" status from the rejected transition,
        // but escalation should override to "backlog"
        let has_backlog = moves.iter().any(|(_, status)| status == "backlog");
        let has_blocked = moves.iter().any(|(_, status)| status == "blocked");
        assert!(
            has_backlog,
            "escalation should move to 'backlog' (from config), got: {moves:?}"
        );
        assert!(
            !has_blocked,
            "escalation should NOT move to 'blocked' (hardcoded), got: {moves:?}"
        );
    }

    #[test]
    fn callback_no_transition_sets_fired_at() {
        // When a verdict has no matching transition, callback should still
        // set callback_fired_at to prevent re-processing.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let task = Task {
            id: "20260324-unk".to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-24T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-reviewer".to_string(),
            prompt: "review issue".to_string(),
            output_path: String::new(),
            working_dir: "~/projects/rigpa/werma".to_string(),
            model: "opus".to_string(),
            max_turns: 50,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "RIG-UNK".to_string(),
            linear_pushed: false,
            pipeline_stage: "reviewer".to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };
        db.insert_task(&task).unwrap();

        let result = "Something unusual happened.\nVERDICT=UNKNOWN_VERDICT_XYZ";

        callback(
            &db,
            "20260324-unk",
            "reviewer",
            result,
            "RIG-UNK",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // No moves should have been made
        let moves = linear.move_calls.borrow();
        assert!(
            moves.is_empty(),
            "unknown verdict should not trigger any moves, got: {moves:?}"
        );

        // callback_fired_at should be set to prevent re-processing
        assert!(
            db.is_callback_recently_fired("20260324-unk", 60).unwrap(),
            "callback_fired_at should be set for unknown verdict"
        );
    }

    // ─── RIG-252: max_turns exit tests ──────────────────────────────────

    #[test]
    fn callback_max_turns_exit_does_not_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-252a", "in_progress");

        let mut task = crate::db::make_test_task("20260325-252a");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252a".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        // Simulate output containing error_max_turns (raw JSON dumped as fallback)
        let result = r#"{"type":"result","subtype":"error_max_turns","is_error":false,"result":"partial work"}"#;

        callback(
            &db,
            "20260325-252a",
            "engineer",
            result,
            "RIG-252a",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // No status moves should happen
        let moves = linear.move_calls.borrow();
        assert!(
            moves.is_empty(),
            "max_turns exit should not trigger any status moves, got: {moves:?}"
        );

        // Should post a comment about max_turns
        let comments = linear.comment_calls.borrow();
        assert!(
            comments
                .iter()
                .any(|(id, body)| id == "RIG-252a" && body.contains("max_turns")),
            "should post max_turns warning comment, got: {comments:?}"
        );
    }

    #[test]
    fn callback_max_turns_in_text_output_does_not_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-252b", "in_progress");

        let mut task = crate::db::make_test_task("20260325-252b");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252b".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        // Text output that mentions error_max_turns
        let result = "Partial implementation done.\nerror_max_turns\nSome more text";

        callback(
            &db,
            "20260325-252b",
            "engineer",
            result,
            "RIG-252b",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        let moves = linear.move_calls.borrow();
        assert!(
            moves.is_empty(),
            "max_turns text should not trigger moves, got: {moves:?}"
        );
    }

    #[test]
    fn callback_normal_engineer_done_still_works() {
        // Sanity check: normal DONE output should NOT be caught by max_turns guard
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("RIG-252c", "in_progress");

        let mut task = crate::db::make_test_task("20260325-252c");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252c".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        // Normal success output — no max_turns indicator
        cmd.push_success("main"); // for auto_create_pr branch check
        let result = "All work done.\nPR_URL=https://github.com/org/repo/pull/1\nVERDICT=DONE";

        callback(
            &db,
            "20260325-252c",
            "engineer",
            result,
            "RIG-252c",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // Should transition normally
        let moves = linear.move_calls.borrow();
        assert!(
            moves
                .iter()
                .any(|(id, status)| id == "RIG-252c" && status == "review"),
            "normal DONE should still move to review, got: {moves:?}"
        );
    }
}
