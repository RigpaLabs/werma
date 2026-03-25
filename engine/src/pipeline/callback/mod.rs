mod comments;
mod retry;
mod spawn;

pub(crate) use comments::{extract_spec_from_output, format_callback_comment};
pub(crate) use retry::move_with_retry;
pub(crate) use spawn::{NextStageParams, create_next_stage_task};

use anyhow::Result;

use super::helpers::truncate_lines;
use super::loader::load_default;
use super::pr::{auto_create_pr, has_open_pr_for_issue, post_pr_comment, pr_title_from_url};
use super::verdict::{
    extract_review_body, is_max_turns_exit, parse_comments, parse_estimate, parse_pr_url,
    parse_verdict,
};
use crate::db::Db;
use crate::linear::LinearApi;
use crate::traits::{CommandRunner, Notifier};

/// Default maximum review cycles when not configured in YAML.
const DEFAULT_MAX_REVIEW_ROUNDS: u32 = 3;

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
                .unwrap_or("backlog");
            eprintln!(
                "[CALLBACK] {linear_issue_id}: task {task_id} (stage={stage}) hit max_turns — \
                 {failed_count} prior failures >= limit {max_soft_failures}, escalating to {escalation_status}"
            );
            if let Err(e) = move_with_retry(linear, linear_issue_id, escalation_status) {
                eprintln!(
                    "[CALLBACK] {linear_issue_id}: escalation move to '{escalation_status}' failed: {e}"
                );
            }
            if let Err(e) = linear.comment(
                linear_issue_id,
                &format!(
                    "**max_turns failure limit reached** ({failed_count} failures, stage: {stage}). \
                     Moving to {escalation_status} — manual intervention required.\n\n\
                     Task `{task_id}` was the latest attempt."
                ),
            ) {
                eprintln!("callback: failed to post escalation comment on {linear_issue_id}: {e}");
            }
            if let Err(e) = db.set_callback_fired_at(task_id) {
                eprintln!("warn: failed to set callback_fired_at for {task_id}: {e}");
            }
        } else {
            eprintln!(
                "[CALLBACK] {linear_issue_id}: task {task_id} (stage={stage}) hit max_turns — \
                 soft failure ({failed_count}/{max_soft_failures}), no transition"
            );
            if let Err(e) = linear.comment(
                linear_issue_id,
                &format!(
                    "**Werma task `{task_id}`** (stage: {stage}) hit `max_turns` — agent ran out of \
                     turns without completing. Soft failure ({}/{max_soft_failures}). Will be retried.",
                    failed_count + 1,
                ),
            ) {
                eprintln!("callback: failed to post max_turns comment on {linear_issue_id}: {e}");
            }
        }
        return Ok(());
    }

    let stage_cfg = if let Some(s) = config.stage(stage) {
        s
    } else {
        eprintln!("unknown pipeline stage: {stage}");
        return Ok(());
    };

    // Post any comment blocks from agent output (non-critical)
    let comments = parse_comments(result);
    for comment_body in &comments {
        if let Err(e) = linear.comment(linear_issue_id, comment_body) {
            eprintln!("[CALLBACK] {linear_issue_id}: failed to post comment: {e}");
        }
    }

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

    // Compute effective verdict once — analyst/engineer default to "done" when missing.
    // Used for both fallback spec posting and transition routing.
    let verdict_str = verdict
        .as_deref()
        .unwrap_or(if stage == "engineer" || stage == "analyst" {
            "done"
        } else {
            ""
        })
        .to_lowercase();

    // RIG-227: Fallback spec posting for analyst stage.
    // Post substantive plain-text output as a comment so the spec reaches Linear.
    // Only fire for "done" — ALREADY_DONE means no new spec was written,
    // and BLOCKED shouldn't post a partial spec.
    // When COMMENT blocks were posted, require substantial plain-text content
    // (>= 5 lines) to avoid posting trivial preamble alongside proper blocks.
    if stage == "analyst" && verdict_str == "done" {
        let spec_body = extract_spec_from_output(result);
        let min_lines = if comments.is_empty() { 1 } else { 5 };
        if spec_body.lines().count() >= min_lines {
            let truncated = truncate_lines(&spec_body, 200);
            if let Err(e) = linear.comment(linear_issue_id, &truncated) {
                eprintln!(
                    "[CALLBACK] {linear_issue_id}: failed to post fallback spec comment: {e}"
                );
            }
        }
    }

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
                     failed to move to '{}' after retries: {e}",
                t.status
            );
            notifier.notify_macos("Werma Callback Failed", &alert_msg, "Basso");
            notifier.notify_slack("#werma-alerts", &alert_msg);
            return Err(e);
        }

        // RIG-300: Analyst label swap — remove trigger label, add verdict-specific label.
        // done/already_done → "analyze:done" + "spec:done"
        // blocked → "analyze:blocked" (no spec:done — spec wasn't completed)
        if stage == "analyst" {
            if let Some(ref label) = stage_cfg.linear_label {
                if let Err(e) = linear.remove_label(linear_issue_id, label) {
                    eprintln!("callback: failed to remove '{label}' from {linear_issue_id}: {e}");
                }
                let suffix = if verdict_str == "blocked" {
                    "blocked"
                } else {
                    "done"
                };
                let result_label = format!("{label}:{suffix}");
                if let Err(e) = linear.add_label(linear_issue_id, &result_label) {
                    eprintln!(
                        "callback: failed to add '{result_label}' label to {linear_issue_id}: {e}"
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

        // RIG-281: Post reviewer's review as a PR comment (engine-side, not agent-side).
        // Agents no longer call `gh pr comment` directly — the engine handles it.
        if stage == "reviewer" {
            if let Some(review_body) = extract_review_body(result) {
                match post_pr_comment(cmd, working_dir, &review_body) {
                    Ok(true) => {
                        eprintln!(
                            "[CALLBACK] {linear_issue_id}: posted review as PR comment (task {task_id})"
                        );
                    }
                    Ok(false) => {
                        eprintln!(
                            "[CALLBACK] {linear_issue_id}: no open PR found for review comment \
                             (task {task_id})"
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "[CALLBACK] {linear_issue_id}: failed to post review as PR comment: {e}"
                        );
                    }
                }
            }
        }

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

#[cfg(test)]
mod tests;
