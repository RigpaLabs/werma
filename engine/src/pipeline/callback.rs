use std::collections::HashMap;

use anyhow::{Result, anyhow};
use rusqlite::{Connection, params};

use super::config::PipelineConfig;
use super::helpers::{infer_working_dir_from_issue, truncate_lines};
use super::loader::{load_default, resolve_prompt};
use super::pr::{auto_create_pr, get_pr_review_verdict, has_open_pr_for_issue, pr_title_from_url};
use super::prompt::{build_vars, render_prompt};
use super::verdict::{
    extract_rejection_feedback, extract_review_body, is_heavy_track, is_max_turns_exit,
    parse_comments, parse_estimate, parse_pr_url, parse_verdict,
};
use crate::db::Db;
use crate::linear::LinearApi;
use crate::models::{Effect, EffectStatus, EffectType};
use crate::traits::CommandRunner;

/// The decision produced by `decide_callback()`: internal DB changes + outbox effects.
pub struct CallbackDecision {
    pub internal: InternalChanges,
    pub effects: Vec<Effect>,
}

/// Internal DB changes to apply atomically in the callback transaction.
pub struct InternalChanges {
    /// A new pipeline task to spawn (stored in DB, no filesystem write).
    pub spawn_task: Option<crate::models::Task>,
    /// Update the task's estimate field: `(task_id, estimate)`.
    pub update_estimate: Option<(String, i32)>,
}

/// Insert a task using a raw `&Connection` — for use inside `Db::transaction()` closures.
fn insert_task_with_conn(conn: &Connection, task: &crate::models::Task) -> Result<()> {
    let depends_on = serde_json::to_string(&task.depends_on)?;
    let context_files = serde_json::to_string(&task.context_files)?;
    let linear_pushed: i32 = if task.linear_pushed { 1 } else { 0 };

    conn.execute(
        "INSERT INTO tasks (
            id, status, priority, created_at, started_at, finished_at,
            type, prompt, output_path, working_dir, model, max_turns,
            allowed_tools, session_id, linear_issue_id, linear_pushed,
            pipeline_stage, depends_on, context_files, repo_hash, estimate,
            retry_count, retry_after, cost_usd, turns_used, handoff_content
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21,
            ?22, ?23, ?24, ?25, ?26
        )",
        params![
            task.id,
            task.status.to_string(),
            task.priority,
            task.created_at,
            task.started_at,
            task.finished_at,
            task.task_type,
            task.prompt,
            task.output_path,
            task.working_dir,
            task.model,
            task.max_turns,
            task.allowed_tools,
            task.session_id,
            task.linear_issue_id,
            linear_pushed,
            task.pipeline_stage,
            depends_on,
            context_files,
            task.repo_hash,
            task.estimate,
            task.retry_count,
            task.retry_after,
            task.cost_usd,
            task.turns_used,
            task.handoff_content,
        ],
    )?;
    Ok(())
}

/// Helper: build a `Vec<Effect>` entry with deterministic dedup_key.
fn make_effect(
    task_id: &str,
    issue_id: &str,
    effect_type: EffectType,
    key_suffix: &str,
    payload: serde_json::Value,
) -> Effect {
    Effect {
        id: 0,
        dedup_key: format!("{task_id}:{key_suffix}"),
        task_id: task_id.to_string(),
        issue_id: issue_id.to_string(),
        effect_type,
        payload,
        blocking: true,
        status: EffectStatus::Pending,
        attempts: 0,
        max_attempts: 5,
        created_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        next_retry_at: None,
        executed_at: None,
        error: None,
    }
}

/// Max retries for Linear status move operations.
// Used by Task 4 effect processor; allow dead_code until effects.rs is implemented.
#[allow(dead_code)]
const CALLBACK_MAX_RETRIES: u32 = 3;
/// Backoff delays in milliseconds between retries: 50ms, 100ms, 200ms.
#[allow(dead_code)]
const CALLBACK_BACKOFF_MS: [u64; 3] = [50, 100, 200];

/// Default maximum review cycles when not configured in YAML.
pub(crate) const DEFAULT_MAX_REVIEW_ROUNDS: u32 = 3;

/// Move a Linear issue to a new status with retry + backoff + reconciliation.
///
/// Retries up to `CALLBACK_MAX_RETRIES` times with exponential backoff.
/// After a successful move, performs a read-after-write check to verify
/// the status actually changed. Returns an error only if all retries
/// are exhausted or reconciliation fails.
// Used by Task 4 effect processor; allow dead_code until effects.rs is implemented.
#[allow(dead_code)]
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

/// Decision service: read DB/cmd, return what should happen — no external mutations.
///
/// Reads the pipeline config, parses verdicts, checks review round limits, and
/// builds the set of outbox effects + internal changes. Does NOT call Linear API
/// mutating methods or write files. Calling code commits everything atomically.
#[allow(clippy::too_many_arguments)]
pub fn decide_callback(
    db: &Db,
    task_id: &str,
    stage: &str,
    result: &str,
    linear_issue_id: &str,
    working_dir: &str,
    cmd: &dyn CommandRunner,
) -> Result<CallbackDecision> {
    let config = load_default()?;
    let mut effects: Vec<Effect> = Vec::new();
    let mut internal = InternalChanges {
        spawn_task: None,
        update_estimate: None,
    };

    // Guard: if output is empty, attempt fallback for reviewer stage (RIG-309),
    // otherwise queue a comment effect and return.
    if result.trim().is_empty() {
        eprintln!("callback: empty output for task {task_id} (stage={stage}), checking fallback");

        // RIG-309: For reviewer stage, check if the agent posted a review via tool calls
        // even though the final text result was empty. Claude Code --output-format json
        // only captures the final assistant text — tool calls are not in `result`.
        if stage == "reviewer" {
            if let Some(gh_verdict) = get_pr_review_verdict(cmd, working_dir, linear_issue_id) {
                eprintln!(
                    "[CALLBACK] {linear_issue_id}: empty result but GitHub PR has review \
                     decision={gh_verdict} — using as fallback verdict (task {task_id})"
                );
                let synthesized_result = format!(
                    "Reviewer agent produced empty output but GitHub PR review found.\n\
                     Fallback verdict from GitHub review decision.\n\n\
                     REVIEW_VERDICT={gh_verdict}"
                );
                // Recurse with synthesized result
                return decide_callback(
                    db,
                    task_id,
                    stage,
                    &synthesized_result,
                    linear_issue_id,
                    working_dir,
                    cmd,
                );
            }
            eprintln!(
                "[CALLBACK] {linear_issue_id}: reviewer empty result and no GitHub review \
                 decision found — treating as failed (task {task_id})"
            );
        }

        effects.push(make_effect(
            task_id,
            linear_issue_id,
            EffectType::PostComment,
            "empty_output_comment",
            serde_json::json!({
                "body": format!(
                    "**Werma task `{task_id}`** (stage: {stage}) produced empty output. \
                     Task marked as failed. Re-trigger needed."
                )
            }),
        ));
        return Ok(CallbackDecision { internal, effects });
    }

    // RIG-252 + RIG-202: Detect error_max_turns — agent ran out of turns without completing.
    // After repeated soft failures for the same issue+stage, escalate to blocked
    // to prevent infinite retry loops (observed: RIG-186 had 5 consecutive reviewer failures).
    if is_max_turns_exit(result) {
        let failed_count = db
            .count_failed_tasks_for_issue_stage(linear_issue_id, stage)
            .unwrap_or(0);
        let max_soft_failures = config
            .stage(stage)
            .and_then(super::config::StageConfig::review_round_limit)
            .unwrap_or(DEFAULT_MAX_REVIEW_ROUNDS) as i64;

        if failed_count >= max_soft_failures {
            // Escalate: too many max_turns failures → move to blocked
            let escalation_status = config
                .stage(stage)
                .and_then(|s| s.transition_for("blocked"))
                .map(|t| t.status.as_str())
                .unwrap_or("backlog")
                .to_string();
            eprintln!(
                "[CALLBACK] {linear_issue_id}: task {task_id} (stage={stage}) hit max_turns — \
                 {failed_count} prior failures >= limit {max_soft_failures}, escalating to {escalation_status}"
            );
            effects.push(make_effect(
                task_id,
                linear_issue_id,
                EffectType::MoveIssue,
                &format!("max_turns_escalate:{escalation_status}"),
                serde_json::json!({ "target_status": escalation_status }),
            ));
            effects.push(make_effect(
                task_id,
                linear_issue_id,
                EffectType::PostComment,
                "max_turns_escalation_comment",
                serde_json::json!({
                    "body": format!(
                        "**max_turns failure limit reached** ({failed_count} failures, stage: {stage}). \
                         Moving to {escalation_status} — manual intervention required.\n\n\
                         Task `{task_id}` was the latest attempt."
                    )
                }),
            ));
        } else {
            eprintln!(
                "[CALLBACK] {linear_issue_id}: task {task_id} (stage={stage}) hit max_turns — \
                 soft failure ({failed_count}/{max_soft_failures}), no transition"
            );
            effects.push(make_effect(
                task_id,
                linear_issue_id,
                EffectType::PostComment,
                "max_turns_soft_comment",
                serde_json::json!({
                    "body": format!(
                        "**Werma task `{task_id}`** (stage: {stage}) hit `max_turns` — agent ran out of \
                         turns without completing. Soft failure ({}/{max_soft_failures}). Will be retried.",
                        failed_count + 1,
                    )
                }),
            ));
        }
        return Ok(CallbackDecision { internal, effects });
    }

    let stage_cfg = if let Some(s) = config.stage(stage) {
        s
    } else {
        return Err(anyhow::anyhow!("unknown pipeline stage: {stage}"));
    };

    // Queue comment blocks from agent output (non-critical)
    let comments = parse_comments(result);
    for (idx, comment_body) in comments.iter().enumerate() {
        effects.push(make_effect(
            task_id,
            linear_issue_id,
            EffectType::PostComment,
            &format!("comment_block:{idx}"),
            serde_json::json!({ "body": comment_body }),
        ));
    }

    let verdict = parse_verdict(result);

    // For stages that require a verdict (reviewer, qa, devops), warn if missing.
    let has_explicit_transitions = !stage_cfg.transitions.is_empty();

    if verdict.is_none() && has_explicit_transitions && stage != "engineer" && stage != "analyst" {
        eprintln!(
            "warning: no verdict found for task {task_id} (stage={stage}), keeping current state"
        );
        effects.push(make_effect(
            task_id,
            linear_issue_id,
            EffectType::PostComment,
            "no_verdict_comment",
            serde_json::json!({
                "body": format!(
                    "**Werma task `{task_id}`** (stage: {stage}) completed but no verdict found. \
                     Manual review needed."
                )
            }),
        ));
        return Ok(CallbackDecision { internal, effects });
    }

    // Compute effective verdict once — analyst/engineer default to "done" when missing.
    let verdict_str = verdict
        .as_deref()
        .unwrap_or(if stage == "engineer" || stage == "analyst" {
            "done"
        } else {
            ""
        })
        .to_lowercase();

    // RIG-227: Fallback spec posting for analyst stage.
    if stage == "analyst" && verdict_str == "done" {
        let spec_body = extract_spec_from_output(result);
        let min_lines = if comments.is_empty() { 1 } else { 5 };
        if spec_body.lines().count() >= min_lines {
            let truncated = truncate_lines(&spec_body, 200);
            effects.push(make_effect(
                task_id,
                linear_issue_id,
                EffectType::PostComment,
                "analyst_spec_comment",
                serde_json::json!({ "body": truncated }),
            ));
        }
    }

    // Parse estimate from analyst output for adaptive track routing
    let estimate = if stage == "analyst" {
        let est = parse_estimate(result);
        if est > 0 {
            effects.push(make_effect(
                task_id,
                linear_issue_id,
                EffectType::UpdateEstimate,
                &format!("update_estimate:{est}"),
                serde_json::json!({ "estimate": est }),
            ));
            // Internal DB update (synchronous, in the callback transaction)
            internal.update_estimate = Some((task_id.to_string(), est));
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
                "callback: blocking ALREADY_DONE→done for {linear_issue_id} — open PR exists."
            );
            effects.push(make_effect(
                task_id,
                linear_issue_id,
                EffectType::PostComment,
                "already_done_blocked_comment",
                serde_json::json!({
                    "body": format!(
                        "**Analyst ALREADY_DONE blocked** (task: `{task_id}`): open PR exists for this issue. \
                         An open PR means work is in progress, not done. Issue stays in current state."
                    )
                }),
            ));
            // RIG-274: still add spec:done even when blocked by open PR.
            if stage == "analyst" {
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::AddLabel,
                    "add_label:spec:done",
                    serde_json::json!({ "label": "spec:done" }),
                ));
            }
            return Ok(CallbackDecision { internal, effects });
        }

        // Queue the issue move as an effect — processor calls move_with_retry.
        effects.push(make_effect(
            task_id,
            linear_issue_id,
            EffectType::MoveIssue,
            &format!("move_issue:{}", t.status),
            serde_json::json!({
                "target_status": t.status,
                // Notify on failure: processor can read this payload
                "alert_on_failure": true,
            }),
        ));

        // RIG-300: Analyst label swap.
        if stage == "analyst" {
            if let Some(ref label) = stage_cfg.linear_label {
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::RemoveLabel,
                    &format!("remove_label:{label}"),
                    serde_json::json!({ "label": label }),
                ));
                let suffix = if verdict_str == "blocked" {
                    "blocked"
                } else {
                    "done"
                };
                let result_label = format!("{label}:{suffix}");
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::AddLabel,
                    &format!("add_label:{result_label}"),
                    serde_json::json!({ "label": result_label }),
                ));
            }
            // RIG-274: Add spec:done for done/already_done.
            if verdict_str == "done" || verdict_str == "already_done" {
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::AddLabel,
                    "add_label:spec:done",
                    serde_json::json!({ "label": "spec:done" }),
                ));
            }
        }

        // Auto-create PR for engineer stage completion
        let pr_url = if stage == "engineer" && verdict_str == "done" {
            let url_from_output = parse_pr_url(result);
            let url = url_from_output.or_else(|| {
                match auto_create_pr(cmd, working_dir, linear_issue_id, task_id) {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("auto-PR error: {e}");
                        None
                    }
                }
            });

            // RIG-232: engineer DONE without PR still spawns reviewer.
            if url.is_none() {
                eprintln!(
                    "callback: engineer DONE but no PR_URL found for {linear_issue_id} (task {task_id}). \
                     Spawning reviewer anyway."
                );
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::PostComment,
                    "no_pr_comment",
                    serde_json::json!({
                        "body": format!(
                            "**Engineer task `{task_id}` DONE but no PR created.** \
                             The agent did not include `PR_URL=` in output. \
                             Proceeding to reviewer — reviewer will verify or request a PR."
                        )
                    }),
                ));
            }

            // Attach PR URL to Linear issue
            if let Some(ref pr) = url {
                let pr_title = pr_title_from_url(pr);
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::AttachUrl,
                    &format!("attach_url:{pr}"),
                    serde_json::json!({ "url": pr, "title": pr_title }),
                ));
            }
            url
        } else {
            None
        };

        // RIG-281: Post reviewer's review as a PR comment.
        if stage == "reviewer" {
            if let Some(review_body) = extract_review_body(result) {
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::PostPrComment,
                    "reviewer_pr_comment",
                    serde_json::json!({ "body": review_body }),
                ));
            }
        }

        // Queue the callback summary comment.
        let comment = format_callback_comment(
            task_id,
            stage,
            &verdict_str,
            t.spawn.as_deref(),
            pr_url.as_deref(),
        );
        effects.push(make_effect(
            task_id,
            linear_issue_id,
            EffectType::PostComment,
            "callback_summary_comment",
            serde_json::json!({ "body": comment }),
        ));

        // Spawn next stage if configured
        if let Some(ref next_stage) = t.spawn {
            // Check review cycle limit.
            if stage == "reviewer" && next_stage == "engineer" {
                let review_count =
                    db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?;
                let max_rounds = stage_cfg
                    .review_round_limit()
                    .unwrap_or(DEFAULT_MAX_REVIEW_ROUNDS) as i64;
                if review_count >= max_rounds {
                    let escalation_status = stage_cfg
                        .transition_for("blocked")
                        .map(|t| t.status.as_str())
                        .unwrap_or("backlog")
                        .to_string();
                    eprintln!(
                        "review cycle limit ({max_rounds}) reached for issue {linear_issue_id}, \
                         escalating to {escalation_status}"
                    );
                    effects.push(make_effect(
                        task_id,
                        linear_issue_id,
                        EffectType::MoveIssue,
                        &format!("cycle_limit_escalate:{escalation_status}"),
                        serde_json::json!({ "target_status": escalation_status }),
                    ));
                    effects.push(make_effect(
                        task_id,
                        linear_issue_id,
                        EffectType::PostComment,
                        "cycle_limit_comment",
                        serde_json::json!({
                            "body": format!(
                                "**Review cycle limit reached** ({max_rounds} rounds). \
                                 Moving to {escalation_status} — manual review required."
                            )
                        }),
                    ));
                    return Ok(CallbackDecision { internal, effects });
                }
            }

            // Build the next-stage task with handoff in DB column (no file write).
            let spawn = build_next_stage_task(
                db,
                &config,
                linear_issue_id,
                next_stage,
                result,
                task_id,
                stage,
                working_dir,
                estimate,
                pr_url.as_deref(),
            )?;
            internal.spawn_task = spawn;
        }
    } else {
        eprintln!("stage '{stage}': no transition for verdict '{verdict_str}' — no action taken");
    }

    Ok(CallbackDecision { internal, effects })
}

/// Thin wrapper: dedup guard + decide_callback + atomic DB transaction.
///
/// Signature kept identical to the old callback() so callers and tests need
/// minimal changes. The `linear` and `notifier` params are now used only by
/// the effect *processor* (Task 4), not here.
#[allow(clippy::too_many_arguments)]
pub fn callback(
    db: &Db,
    task_id: &str,
    stage: &str,
    result: &str,
    linear_issue_id: &str,
    working_dir: &str,
    cmd: &dyn CommandRunner,
) -> Result<()> {
    // Dedup guard: if callback SUCCEEDED recently, skip to prevent duplicate
    // effects from overlapping daemon ticks.
    if db.is_callback_recently_fired(task_id, 60)? {
        eprintln!("callback: skipping duplicate for task {task_id} (fired <60s ago)");
        return Ok(());
    }

    let decision = decide_callback(
        db,
        task_id,
        stage,
        result,
        linear_issue_id,
        working_dir,
        cmd,
    )?;

    // Atomically write all internal changes + outbox effects in one transaction.
    // callback_fired_at is set here so it's part of the same atomic write.
    db.transaction(|conn| {
        // Spawn next pipeline task if the decision requires it.
        if let Some(ref task) = decision.internal.spawn_task {
            let exists: bool = conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE id = ?1",
                params![task.id],
                |row| row.get::<_, i32>(0),
            )? > 0;
            if !exists {
                insert_task_with_conn(conn, task)?;
                eprintln!(
                    "  + pipeline task: {} stage={} type={}",
                    task.id, task.pipeline_stage, task.task_type
                );
            }
        }

        // Apply estimate update.
        if let Some((ref tid, est)) = decision.internal.update_estimate {
            conn.execute(
                "UPDATE tasks SET estimate = ?1 WHERE id = ?2",
                params![est, tid],
            )?;
        }

        // Insert outbox effects (INSERT OR IGNORE via dedup_key).
        crate::db::Db::insert_effects_with_conn(conn, &decision.effects)?;

        // Mark callback as fired to prevent re-processing.
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        conn.execute(
            "UPDATE tasks SET callback_fired_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;
        Ok(())
    })?;

    // Log for observability: what will the effect processor execute?
    if !decision.effects.is_empty() {
        eprintln!(
            "[CALLBACK] {linear_issue_id}: queued {} effects for task {task_id}",
            decision.effects.len()
        );
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

pub(crate) fn extract_spec_from_output(output: &str) -> String {
    let mut lines = Vec::new();
    let mut in_comment_block = false;
    let mut unclosed_block_start = 0;

    for (idx, line) in output.lines().enumerate() {
        let trimmed = line.trim();

        // Skip ---COMMENT---/---END COMMENT--- blocks (already posted)
        if trimmed == "---COMMENT---" {
            in_comment_block = true;
            unclosed_block_start = idx;
            continue;
        }
        if trimmed == "---END COMMENT---" {
            in_comment_block = false;
            continue;
        }
        if in_comment_block {
            continue;
        }

        // Skip verdict and estimate metadata lines
        let upper = trimmed.to_uppercase();
        if upper.starts_with("VERDICT=") || upper.starts_with("ESTIMATE=") {
            continue;
        }

        lines.push(line);
    }

    // If a ---COMMENT--- was never closed, treat it as plain text.
    // This handles non-conforming agent output where markers are malformed.
    if in_comment_block {
        lines.clear();
        for (idx, line) in output.lines().enumerate() {
            if idx < unclosed_block_start {
                let trimmed = line.trim();
                let upper = trimmed.to_uppercase();
                if upper.starts_with("VERDICT=") || upper.starts_with("ESTIMATE=") {
                    continue;
                }
                lines.push(line);
            } else {
                // Include unclosed block content as-is (skip the marker line itself)
                if idx == unclosed_block_start {
                    continue;
                }
                let trimmed = line.trim();
                let upper = trimmed.to_uppercase();
                if upper.starts_with("VERDICT=") || upper.starts_with("ESTIMATE=") {
                    continue;
                }
                lines.push(line);
            }
        }
    }

    // Trim leading/trailing blank lines
    let result = lines.join("\n");
    result.trim().to_string()
}

/// Build a Task for the next pipeline stage with handoff content stored in `task.handoff_content`.
///
/// Unlike `create_next_stage_task()`, this function:
/// - Does NOT write any files (no `~/.werma/logs/*-handoff.md`)
/// - Does NOT insert into DB (caller does that atomically via `insert_task_with_conn`)
/// - Does NOT call Linear API for issue metadata (no `&dyn LinearApi` param)
/// - Returns `None` if an active task already exists for the issue + stage
#[allow(clippy::too_many_arguments)]
fn build_next_stage_task(
    db: &Db,
    config: &PipelineConfig,
    linear_issue_id: &str,
    next_stage: &str,
    previous_output: &str,
    prev_task_id: &str,
    prev_stage: &str,
    working_dir: &str,
    estimate: i32,
    pr_url: Option<&str>,
) -> Result<Option<crate::models::Task>> {
    // Guard: don't spawn if an active task already exists for this issue + stage.
    let existing = db.tasks_by_linear_issue(linear_issue_id, Some(next_stage), true)?;
    if !existing.is_empty() {
        eprintln!(
            "skip spawn: active task already exists for {linear_issue_id} stage={next_stage}"
        );
        return Ok(None);
    }

    let stage_cfg = config
        .stage(next_stage)
        .ok_or_else(|| anyhow::anyhow!("no config for stage '{next_stage}'"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let review_round = if next_stage == "reviewer" {
        db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?
    } else {
        0
    };

    let max_turns = if let Some(t) = stage_cfg.max_turns {
        t as i32
    } else if next_stage == "engineer" {
        if is_heavy_track(estimate) { 45 } else { 30 }
    } else {
        crate::default_turns(&stage_cfg.agent)
    };
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
    let effective_model = stage_cfg
        .effective_model(estimate, review_round)
        .to_string();

    // Build the prompt without issue metadata (no Linear API call).
    let prompt = build_handoff_prompt(
        config,
        next_stage,
        prev_stage,
        linear_issue_id,
        "", // issue_title: unknown without Linear API call
        "", // issue_description: unknown without Linear API call
        previous_output,
    );

    let pr_section = pr_url.map(|url| format!("PR: {url}\n")).unwrap_or_default();

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

    let effective_working_dir = if working_dir.is_empty() || working_dir == "~/projects/ar" {
        infer_working_dir_from_issue(db, linear_issue_id)
    } else {
        working_dir.to_string()
    };

    use crate::models::{Status, Task};
    let task = Task {
        id: task_id,
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
        linear_issue_id: linear_issue_id.to_string(),
        linear_pushed: false,
        pipeline_stage: next_stage.to_string(),
        depends_on: vec![],
        context_files: vec![], // no filesystem dependency — handoff in DB column
        repo_hash: crate::runtime_repo_hash(),
        estimate,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content,
    };

    Ok(Some(task))
}

/// Parameters for creating the next pipeline stage task.
// Used only in tests; the production path uses build_next_stage_task() via decide_callback().
#[cfg_attr(not(test), allow(dead_code))]
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
// Used only in tests; the production path uses build_next_stage_task() via decide_callback().
#[cfg_attr(not(test), allow(dead_code))]
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
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content: String::new(),
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
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
        // RIG-232: verify that the "no PR created" warning comment effect is queued.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

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
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // PostComment effect about missing PR should be queued.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("no PR created"))
            }),
            "should queue PostComment effect about missing PR, got: {effects:?}"
        );

        // MoveIssue effect to "review" should be queued.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("review")
            }),
            "should queue MoveIssue effect to review, got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_done_swaps_labels() {
        // RIG-253: analyst callback should queue RemoveLabel(analyze), AddLabel(analyze:done), AddLabel(spec:done) effects.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

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
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // RemoveLabel effect for "analyze".
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::RemoveLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze")
            }),
            "should queue RemoveLabel(analyze), got: {effects:?}"
        );

        // AddLabel effect for "analyze:done".
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze:done")
            }),
            "should queue AddLabel(analyze:done), got: {effects:?}"
        );

        // AddLabel effect for "spec:done".
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "should queue AddLabel(spec:done), got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_already_done_adds_spec_done() {
        // RIG-274: ALREADY_DONE verdict should queue AddLabel(spec:done) effect.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

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
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "ALREADY_DONE should queue AddLabel(spec:done), got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_already_done_with_open_pr_still_adds_spec_done() {
        // RIG-274: ALREADY_DONE + open PR path should still queue AddLabel(spec:done).
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

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
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "ALREADY_DONE with open PR should still queue AddLabel(spec:done), got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_blocked_adds_analyze_blocked_not_spec_done() {
        // RIG-300: BLOCKED verdict should queue AddLabel(analyze:blocked), NOT AddLabel(spec:done)
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

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
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // No AddLabel(spec:done) for BLOCKED.
        assert!(
            !effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "BLOCKED should NOT queue AddLabel(spec:done), got: {effects:?}"
        );

        // AddLabel(analyze:blocked).
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze:blocked")
            }),
            "BLOCKED should queue AddLabel(analyze:blocked), got: {effects:?}"
        );

        // No AddLabel(analyze:done).
        assert!(
            !effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze:done")
            }),
            "BLOCKED should NOT queue AddLabel(analyze:done), got: {effects:?}"
        );

        // RemoveLabel(analyze).
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::RemoveLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze")
            }),
            "BLOCKED should queue RemoveLabel(analyze), got: {effects:?}"
        );
    }

    #[test]
    fn review_cycle_escalation_uses_config_status() {
        // When review cycle limit is reached, the callback should queue MoveIssue("backlog"),
        // not "blocked" (config-driven, not hardcoded).
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
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
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // MoveIssue("backlog") should be queued (config-driven escalation).
        let has_backlog = effects.iter().any(|e| {
            e.effect_type == crate::models::EffectType::MoveIssue
                && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("backlog")
        });
        let has_blocked = effects.iter().any(|e| {
            e.effect_type == crate::models::EffectType::MoveIssue
                && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("blocked")
        });
        assert!(
            has_backlog,
            "escalation should queue MoveIssue('backlog'), got: {effects:?}"
        );
        assert!(
            !has_blocked,
            "escalation should NOT queue MoveIssue('blocked'), got: {effects:?}"
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
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
            &cmd,
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

        let mut task = crate::db::make_test_task("20260325-252a");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252a".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        let result = r#"{"type":"result","subtype":"error_max_turns","is_error":false,"result":"partial work"}"#;

        callback(
            &db,
            "20260325-252a",
            "engineer",
            result,
            "RIG-252a",
            "~/projects/rigpa/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // No MoveIssue effects — max_turns should not transition.
        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == crate::models::EffectType::MoveIssue),
            "max_turns exit should not queue MoveIssue effects, got: {effects:?}"
        );

        // PostComment effect about max_turns should be queued.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("max_turns"))
            }),
            "should queue PostComment about max_turns, got: {effects:?}"
        );
    }

    #[test]
    fn callback_max_turns_in_text_output_does_not_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let mut task = crate::db::make_test_task("20260325-252b");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252b".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        let result = "Partial implementation done.\nerror_max_turns\nSome more text";

        callback(
            &db,
            "20260325-252b",
            "engineer",
            result,
            "RIG-252b",
            "~/projects/rigpa/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();
        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == crate::models::EffectType::MoveIssue),
            "max_turns text should not queue MoveIssue effects, got: {effects:?}"
        );
    }

    #[test]
    fn callback_normal_engineer_done_still_works() {
        // Sanity check: normal DONE output should queue MoveIssue("review") effect.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let mut task = crate::db::make_test_task("20260325-252c");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252c".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        let result = "All work done.\nPR_URL=https://github.com/org/repo/pull/1\nVERDICT=DONE";

        callback(
            &db,
            "20260325-252c",
            "engineer",
            result,
            "RIG-252c",
            "~/projects/rigpa/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // MoveIssue("review") should be queued.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("review")
            }),
            "normal DONE should queue MoveIssue('review'), got: {effects:?}"
        );
    }

    // ─── RIG-202: max_turns escalation after N soft failures ─────────────

    #[test]
    fn callback_max_turns_escalates_after_repeated_failures() {
        // RIG-202: After N failed reviewer tasks, max_turns should queue
        // MoveIssue("backlog") + PostComment escalation effects.
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        for i in 0..3 {
            let mut prev = crate::db::make_test_task(&format!("20260326-f{i}"));
            prev.status = Status::Completed;
            prev.linear_issue_id = "RIG-202a".to_string();
            prev.pipeline_stage = "reviewer".to_string();
            db.insert_task(&prev).unwrap();
            db.set_task_status(&format!("20260326-f{i}"), Status::Failed)
                .unwrap();
        }

        let mut task = crate::db::make_test_task("20260326-202a");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-202a".to_string();
        task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&task).unwrap();

        let result = "Partial review done.\nerror_max_turns";

        callback(
            &db,
            "20260326-202a",
            "reviewer",
            result,
            "RIG-202a",
            "~/projects/rigpa/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // MoveIssue("backlog") escalation effect.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("backlog")
            }),
            "should queue MoveIssue('backlog') escalation, got: {effects:?}"
        );

        // PostComment with escalation message.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("failure limit reached"))
            }),
            "should queue PostComment about failure limit, got: {effects:?}"
        );

        // callback_fired_at should be set.
        assert!(
            db.is_callback_recently_fired("20260326-202a", 60).unwrap(),
            "callback_fired_at should be set after escalation"
        );
    }

    #[test]
    fn callback_max_turns_soft_failure_below_limit() {
        // RIG-202: Below the failure limit, max_turns should queue only a PostComment
        // (soft failure — no MoveIssue escalation).
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let mut prev = crate::db::make_test_task("20260326-f0b");
        prev.status = Status::Completed;
        prev.linear_issue_id = "RIG-202b".to_string();
        prev.pipeline_stage = "reviewer".to_string();
        db.insert_task(&prev).unwrap();
        db.set_task_status("20260326-f0b", Status::Failed).unwrap();

        let mut task = crate::db::make_test_task("20260326-202b");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-202b".to_string();
        task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&task).unwrap();

        let result = "Partial review.\nerror_max_turns";

        callback(
            &db,
            "20260326-202b",
            "reviewer",
            result,
            "RIG-202b",
            "~/projects/rigpa/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        // No MoveIssue effects — soft failure does not escalate.
        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == crate::models::EffectType::MoveIssue),
            "soft failure should not queue MoveIssue, got: {effects:?}"
        );

        // PostComment with "Soft failure" message.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("Soft failure"))
            }),
            "should queue PostComment about soft failure, got: {effects:?}"
        );
    }

    // -------------------------------------------------------------------------
    // decide_callback() unit tests
    // -------------------------------------------------------------------------

    /// Helper: create a minimal analyst task in `db` and return its id.
    fn insert_analyst_task(db: &crate::db::Db, task_id: &str, issue_id: &str) -> String {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "analyst".to_string();
        t.task_type = "pipeline-analyst".to_string();
        t.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&t).unwrap();
        task_id.to_string()
    }

    /// Helper: create a minimal reviewer task in `db`.
    fn insert_reviewer_task(db: &crate::db::Db, task_id: &str, issue_id: &str) {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "reviewer".to_string();
        t.task_type = "pipeline-reviewer".to_string();
        t.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&t).unwrap();
    }

    #[test]
    fn decide_analyst_done_produces_correct_effects() {
        // Analyst "done" transition in default.yaml has status=todo but NO spawn.
        // Engineer is spawned later by the poll step when it sees the issue moved to "todo".
        // So decide_callback for analyst must produce:
        //   - MoveIssue (→ todo)
        //   - RemoveLabel (analyze) + AddLabel (analyze:done)
        //   - AddLabel (spec:done)
        //   - UpdateEstimate (when ESTIMATE= present)
        //   - PostComment (spec body)
        //   - PostComment (callback summary)
        // And NO spawn_task.
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-100";
        insert_analyst_task(&db, "decide-100-a", issue_id);

        // Analyst output with estimate and spec body (>= 1 line since comments vec is empty)
        let result = "## Spec\nImplement feature X\n- req 1\n- req 2\n\nESTIMATE=5";

        let decision = decide_callback(
            &db,
            "decide-100-a",
            "analyst",
            result,
            issue_id,
            "~/projects/rigpa/werma",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

        // Must have a MoveIssue effect (→ todo)
        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "analyst done should queue MoveIssue, got: {effects:?}"
        );

        // Must have spec:done label added
        assert!(
            effects.iter().any(|e| {
                e.effect_type == EffectType::AddLabel
                    && e.payload
                        .get("label")
                        .and_then(|v| v.as_str())
                        .is_some_and(|l| l.contains("spec:done"))
            }),
            "analyst done should add spec:done label, got: {effects:?}"
        );

        // Must have UpdateEstimate for estimate=5
        assert!(
            effects.iter().any(|e| {
                e.effect_type == EffectType::UpdateEstimate
                    && e.payload
                        .get("estimate")
                        .and_then(|v| v.as_i64())
                        .is_some_and(|est| est == 5)
            }),
            "analyst done should queue UpdateEstimate=5, got: {effects:?}"
        );

        // InternalChanges: update_estimate set
        assert_eq!(
            decision.internal.update_estimate,
            Some(("decide-100-a".to_string(), 5)),
            "internal.update_estimate should be (task_id, 5)"
        );

        // InternalChanges: NO spawn_task — analyst stage has no spawn in config;
        // engineer is created by the poll step after MoveIssue(todo) is processed.
        assert!(
            decision.internal.spawn_task.is_none(),
            "analyst done must NOT spawn a task (poll handles it after status move)"
        );
    }

    #[test]
    fn decide_reviewer_rejected_spawns_engineer() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-101";
        insert_reviewer_task(&db, "decide-101-r", issue_id);

        let result = "## Review\n- blocker: missing tests\n- blocker: type errors\n\nREVIEW_VERDICT=REJECTED";

        let decision = decide_callback(
            &db,
            "decide-101-r",
            "reviewer",
            result,
            issue_id,
            "~/projects/rigpa/werma",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

        // Must move to in_progress (or equivalent rejection status)
        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "reviewer rejected should queue MoveIssue, got: {effects:?}"
        );

        // Must spawn engineer
        let spawned = decision.internal.spawn_task.as_ref();
        assert!(
            spawned.is_some(),
            "reviewer rejected should spawn engineer task"
        );
        let spawned = spawned.unwrap();
        assert_eq!(spawned.pipeline_stage, "engineer");
        // Handoff must contain rejection feedback
        assert!(
            spawned.handoff_content.contains("blocker") || spawned.prompt.contains("blocker"),
            "spawned engineer must carry rejection feedback: handoff={}, prompt={}",
            spawned.handoff_content,
            spawned.prompt
        );
    }

    #[test]
    fn decide_unknown_stage_returns_error() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        // Insert task for an unknown stage
        let mut t = crate::db::make_test_task("decide-unk-1");
        t.status = Status::Completed;
        t.linear_issue_id = "DECIDE-UNK".to_string();
        t.pipeline_stage = "unicorn".to_string();
        db.insert_task(&t).unwrap();

        let result = decide_callback(
            &db,
            "decide-unk-1",
            "unicorn", // not in pipeline config
            "some output",
            "DECIDE-UNK",
            "/tmp",
            &cmd,
        );

        assert!(result.is_err(), "unknown stage must return Err, got Ok");
    }

    #[test]
    fn decide_empty_output_returns_comment_effect() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        insert_analyst_task(&db, "decide-empty-1", "DECIDE-EMPTY");

        let decision = decide_callback(
            &db,
            "decide-empty-1",
            "analyst",
            "   ", // whitespace-only = empty
            "DECIDE-EMPTY",
            "/tmp",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

        // Must queue exactly one PostComment about empty output
        assert!(
            effects.iter().any(|e| {
                e.effect_type == EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("empty output"))
            }),
            "empty output should queue PostComment, got: {effects:?}"
        );

        // Must NOT queue MoveIssue
        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "empty output must not queue MoveIssue, got: {effects:?}"
        );

        // No spawn
        assert!(
            decision.internal.spawn_task.is_none(),
            "empty output must not spawn next task"
        );
    }

    #[test]
    fn decide_max_turns_escalation() {
        // After repeated failures, max_turns should escalate to blocked.
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-MAX";

        // Insert 3 prior failed reviewer tasks (meets DEFAULT_MAX_REVIEW_ROUNDS = 3)
        for i in 0..3 {
            let mut t = crate::db::make_test_task(&format!("decide-max-prev-{i}"));
            t.linear_issue_id = issue_id.to_string();
            t.pipeline_stage = "reviewer".to_string();
            t.task_type = "pipeline-reviewer".to_string();
            db.insert_task(&t).unwrap();
            db.set_task_status(&format!("decide-max-prev-{i}"), Status::Failed)
                .unwrap();
        }

        // The current task
        insert_reviewer_task(&db, "decide-max-cur", issue_id);

        let result = "Partial review.\nerror_max_turns";

        let decision = decide_callback(
            &db,
            "decide-max-cur",
            "reviewer",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

        // Must queue MoveIssue (escalation to blocked/backlog)
        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "max_turns escalation should queue MoveIssue, got: {effects:?}"
        );

        // Must queue PostComment about escalation
        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::PostComment),
            "max_turns escalation should queue PostComment, got: {effects:?}"
        );

        // No spawn — escalation blocks further automation
        assert!(
            decision.internal.spawn_task.is_none(),
            "max_turns escalation must not spawn next task"
        );
    }

    #[test]
    fn decide_dedup_keys_are_deterministic() {
        // Calling decide_callback twice with identical inputs must produce effects
        // with identical dedup_keys — ensuring INSERT OR IGNORE deduplication works.
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-DEDUP";
        let task_id = "decide-dedup-t1";
        insert_analyst_task(&db, task_id, issue_id);

        let result = "## Spec\nSome content\n\nESTIMATE=3";

        let d1 = decide_callback(&db, task_id, "analyst", result, issue_id, "/tmp", &cmd).unwrap();
        let d2 = decide_callback(&db, task_id, "analyst", result, issue_id, "/tmp", &cmd).unwrap();

        // Same number of effects
        assert_eq!(
            d1.effects.len(),
            d2.effects.len(),
            "identical inputs must produce same number of effects"
        );

        // All dedup_keys match
        let keys1: Vec<&str> = d1.effects.iter().map(|e| e.dedup_key.as_str()).collect();
        let keys2: Vec<&str> = d2.effects.iter().map(|e| e.dedup_key.as_str()).collect();
        assert_eq!(
            keys1, keys2,
            "dedup_keys must be deterministic across identical calls"
        );

        // Dedup keys must contain the task_id prefix
        for key in &keys1 {
            assert!(
                key.starts_with(task_id),
                "dedup_key must start with task_id prefix: {key}"
            );
        }
    }
}
