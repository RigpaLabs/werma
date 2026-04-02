use anyhow::Result;

use super::super::config::StageConfig;
use super::super::helpers::truncate_lines;
use super::super::loader::load_for_working_dir;
use super::super::pr::{get_pr_review_verdict, has_open_pr_for_issue, pr_title_from_url};
use super::super::verdict::{
    extract_review_body, is_max_turns_exit, parse_comments, parse_estimate, parse_pr_url,
    parse_verdict, validate_analyst_spec,
};
use super::effects_helper::{extract_spec_from_output, format_callback_comment, make_effect};
use super::task_builder::build_next_stage_task;
use crate::db::Db;
use crate::models::{Effect, EffectType};
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
    /// Update a task's handoff_content field: `(task_id, content)`.
    pub handoff_update: Option<(String, String)>,
}

/// Default maximum review cycles when not configured in YAML.
pub(crate) const DEFAULT_MAX_REVIEW_ROUNDS: u32 = 3;

/// Returns true if the verdict represents a forward-advancing (success) outcome.
/// Success verdicts should always advance the pipeline, even if the stage has
/// been retried many times — the cap only prevents infinite failure retries.
fn is_forward_verdict(verdict: &str) -> bool {
    matches!(
        verdict,
        "done" | "approved" | "passed" | "ok" | "already_done"
    )
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
    let config = load_for_working_dir(working_dir)?;
    let mut effects: Vec<Effect> = Vec::new();
    let mut internal = InternalChanges {
        spawn_task: None,
        update_estimate: None,
        handoff_update: None,
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
            .and_then(StageConfig::review_round_limit)
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

    // RIG-340: Validate analyst spec contains required sections before allowing transition.
    // Check the full output (including comment blocks) — the spec may be in either place.
    if stage == "analyst" && verdict_str == "done" {
        if let Err(missing) = validate_analyst_spec(result) {
            let missing_list = missing
                .iter()
                .map(|s| format!("- `{s}`"))
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!(
                "[CALLBACK] {linear_issue_id}: analyst spec missing required sections: {}",
                missing.join(", ")
            );
            effects.push(make_effect(
                task_id,
                linear_issue_id,
                EffectType::PostComment,
                "spec_validation_failed",
                serde_json::json!({
                    "body": format!(
                        "**Analyst spec validation failed** (task: `{task_id}`). \
                         The following required sections are missing:\n\n{missing_list}\n\n\
                         Please re-run the analyst stage with a complete spec."
                    )
                }),
            ));
            // No transition — issue stays in current state, task is treated as failed
            return Ok(CallbackDecision { internal, effects });
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
        // RIG-338: Stage retry cap — only fire on failure verdicts.
        // Success verdicts (done, approved, passed, ok) always advance the pipeline,
        // even if the stage has failed many times before. The cap prevents infinite
        // *retries*, not successful completions.
        let is_success = is_forward_verdict(&verdict_str);
        if !is_success {
            if let (Some(max_attempts), Some(on_max_verdict)) =
                (stage_cfg.attempt_limit(), &stage_cfg.on_max_rounds)
            {
                let attempt_count =
                    db.count_failed_tasks_for_issue_stage(linear_issue_id, stage)?;
                if attempt_count >= max_attempts as i64 {
                    let escalation_status = stage_cfg
                        .transition_for(on_max_verdict)
                        .map(|t| t.status.as_str())
                        .unwrap_or("backlog")
                        .to_string();
                    eprintln!(
                        "[CALLBACK] {linear_issue_id}: stage {stage} retry cap reached — \
                         {attempt_count} failed attempts >= limit {max_attempts}, \
                         escalating via on_max_rounds={on_max_verdict} to {escalation_status}"
                    );
                    effects.push(make_effect(
                        task_id,
                        linear_issue_id,
                        EffectType::MoveIssue,
                        &format!("retry_cap_escalate:{escalation_status}"),
                        serde_json::json!({ "target_status": escalation_status }),
                    ));
                    effects.push(make_effect(
                        task_id,
                        linear_issue_id,
                        EffectType::PostComment,
                        "retry_cap_comment",
                        serde_json::json!({
                            "body": format!(
                                "**Stage retry cap reached** (stage: {stage}, {attempt_count} failed attempts, \
                                 limit: {max_attempts}). Escalating via `{on_max_verdict}` → \
                                 {escalation_status}.\n\nTask `{task_id}` was the latest attempt."
                            )
                        }),
                    ));
                    return Ok(CallbackDecision { internal, effects });
                }
            }
        }

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

        // Engineer DONE: attach existing PR or queue CreatePr effect (outbox boundary).
        // Never call auto_create_pr() directly here — that's a GitHub side effect.
        let pr_url = if stage == "engineer" && verdict_str == "done" {
            let url_from_output = parse_pr_url(result);

            if let Some(ref pr) = url_from_output {
                // Agent already created a PR and included PR_URL= in output — just attach it.
                let pr_title = pr_title_from_url(pr);
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::AttachUrl,
                    &format!("attach_url:{pr}"),
                    serde_json::json!({ "url": pr, "title": pr_title }),
                ));
            } else {
                // RIG-334: No PR_URL in output — queue CreatePr effect (blocking).
                // Effect processor calls auto_create_pr() atomically after the transaction.
                // Reviewer spawn is DEFERRED: we do NOT spawn reviewer here because
                // there is no PR artifact yet. The poller will create the reviewer task
                // after CreatePr succeeds and the issue moves to Review status.
                eprintln!(
                    "callback: engineer DONE but no PR_URL in output for {linear_issue_id} \
                     (task {task_id}) — queuing CreatePr effect, deferring reviewer spawn."
                );
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::CreatePr,
                    "create_pr",
                    serde_json::json!({
                        "working_dir": working_dir,
                        "issue_id": linear_issue_id,
                        "task_id": task_id,
                    }),
                ));
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::PostComment,
                    "no_pr_comment",
                    serde_json::json!({
                        "body": format!(
                            "**Engineer task `{task_id}` DONE but no PR created.** \
                             The agent did not include `PR_URL=` in output. \
                             CreatePr effect queued — reviewer will be spawned by poller \
                             after PR creation."
                        )
                    }),
                ));
            }

            url_from_output
        } else {
            None
        };

        // RIG-281: Post reviewer's review as a proper GitHub PR review.
        // RIG-318: use `gh pr review` (not `gh pr comment`) with the correct review event.
        if stage == "reviewer" {
            if let Some(review_body) = extract_review_body(result) {
                let review_event = match verdict_str.as_str() {
                    "approved" => "approve",
                    "rejected" => "request-changes",
                    _ => "comment",
                };
                effects.push(make_effect(
                    task_id,
                    linear_issue_id,
                    EffectType::PostPrComment,
                    "reviewer_pr_comment",
                    serde_json::json!({
                        "body": review_body,
                        "working_dir": working_dir,
                        "review_event": review_event,
                    }),
                ));
            }
        }

        // RIG-333: Store reviewer's feedback in the reviewer task's handoff_content
        // so the next reviewer (after engineer fixes) can see what was flagged.
        // Deferred to InternalChanges — applied atomically in callback() transaction.
        if stage == "reviewer" {
            let reviewer_feedback = super::super::verdict::extract_rejection_feedback(result);
            if !reviewer_feedback.is_empty() {
                let handoff = format!(
                    "## Previous Review (REVIEW_VERDICT={verdict})\n\n{feedback}",
                    verdict = verdict_str.to_uppercase(),
                    feedback = truncate_lines(&reviewer_feedback, 150),
                );
                internal.handoff_update = Some((task_id.to_string(), handoff));
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

            // RIG-334: Do NOT spawn reviewer when engineer had no PR_URL.
            // The CreatePr effect must complete first, then the poller will
            // create the reviewer task when it sees the issue in Review.
            if stage == "engineer" && next_stage == "reviewer" && pr_url.is_none() {
                eprintln!(
                    "callback: skipping reviewer spawn for {linear_issue_id} — \
                     no PR_URL from engineer output, deferring to poller after CreatePr"
                );
            } else {
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
        }
    } else {
        eprintln!("stage '{stage}': no transition for verdict '{verdict_str}' — no action taken");
    }

    Ok(CallbackDecision { internal, effects })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Status;

    /// Helper: create a minimal analyst task in `db` and return its id.
    fn insert_analyst_task(db: &crate::db::Db, task_id: &str, issue_id: &str) -> String {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "analyst".to_string();
        t.task_type = "pipeline-analyst".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();
        task_id.to_string()
    }

    /// Helper: returns analyst output that passes spec validation (RIG-340).
    fn valid_analyst_output(estimate: i32) -> String {
        format!(
            "## Scope\nImplement feature X\n\n\
             ## Acceptance Criteria\n- req 1\n- req 2\n\n\
             ## Out of Scope\n- Not Y\n\n\
             ESTIMATE={estimate}"
        )
    }

    /// Helper: create a minimal reviewer task in `db`.
    fn insert_reviewer_task(db: &crate::db::Db, task_id: &str, issue_id: &str) {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "reviewer".to_string();
        t.task_type = "pipeline-reviewer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();
    }

    #[test]
    fn decide_analyst_done_produces_correct_effects() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-100";
        insert_analyst_task(&db, "decide-100-a", issue_id);

        let result = &valid_analyst_output(5);

        let decision = decide_callback(
            &db,
            "decide-100-a",
            "analyst",
            result,
            issue_id,
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "analyst done should queue MoveIssue, got: {effects:?}"
        );

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

        assert_eq!(
            decision.internal.update_estimate,
            Some(("decide-100-a".to_string(), 5)),
            "internal.update_estimate should be (task_id, 5)"
        );

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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "reviewer rejected should queue MoveIssue, got: {effects:?}"
        );

        let spawned = decision.internal.spawn_task.as_ref();
        assert!(
            spawned.is_some(),
            "reviewer rejected should spawn engineer task"
        );
        let spawned = spawned.unwrap();
        assert_eq!(spawned.pipeline_stage, "engineer");
        assert!(
            spawned.handoff_content.contains("blocker") || spawned.prompt.contains("blocker"),
            "spawned engineer must carry rejection feedback"
        );
    }

    #[test]
    fn decide_unknown_stage_returns_error() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let mut t = crate::db::make_test_task("decide-unk-1");
        t.status = Status::Completed;
        t.linear_issue_id = "DECIDE-UNK".to_string();
        t.pipeline_stage = "unicorn".to_string();
        db.insert_task(&t).unwrap();

        let result = decide_callback(
            &db,
            "decide-unk-1",
            "unicorn",
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
            "   ",
            "DECIDE-EMPTY",
            "/tmp",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

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

        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "empty output must not queue MoveIssue, got: {effects:?}"
        );

        assert!(
            decision.internal.spawn_task.is_none(),
            "empty output must not spawn next task"
        );
    }

    #[test]
    fn decide_max_turns_escalation() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-MAX";

        for i in 0..3 {
            let mut t = crate::db::make_test_task(&format!("decide-max-prev-{i}"));
            t.linear_issue_id = issue_id.to_string();
            t.pipeline_stage = "reviewer".to_string();
            t.task_type = "pipeline-reviewer".to_string();
            db.insert_task(&t).unwrap();
            db.set_task_status(&format!("decide-max-prev-{i}"), Status::Failed)
                .unwrap();
        }

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

        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "max_turns escalation should queue MoveIssue, got: {effects:?}"
        );

        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::PostComment),
            "max_turns escalation should queue PostComment, got: {effects:?}"
        );

        assert!(
            decision.internal.spawn_task.is_none(),
            "max_turns escalation must not spawn next task"
        );
    }

    #[test]
    fn decide_dedup_keys_are_deterministic() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-DEDUP";
        let task_id = "decide-dedup-t1";
        insert_analyst_task(&db, task_id, issue_id);

        let result = &valid_analyst_output(3);

        let d1 = decide_callback(&db, task_id, "analyst", result, issue_id, "/tmp", &cmd).unwrap();
        let d2 = decide_callback(&db, task_id, "analyst", result, issue_id, "/tmp", &cmd).unwrap();

        assert_eq!(
            d1.effects.len(),
            d2.effects.len(),
            "identical inputs must produce same number of effects"
        );

        let keys1: Vec<&str> = d1.effects.iter().map(|e| e.dedup_key.as_str()).collect();
        let keys2: Vec<&str> = d2.effects.iter().map(|e| e.dedup_key.as_str()).collect();
        assert_eq!(
            keys1, keys2,
            "dedup_keys must be deterministic across identical calls"
        );

        for key in &keys1 {
            assert!(
                key.starts_with(task_id),
                "dedup_key must start with task_id prefix: {key}"
            );
        }
    }

    #[test]
    fn decide_analyst_done_effects_have_correct_blocking() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "BLOCK-TEST";
        let task_id = "block-test-t1";
        insert_analyst_task(&db, task_id, issue_id);

        let result = &format!("{}\n\nVERDICT=done", valid_analyst_output(3));

        let decision =
            decide_callback(&db, task_id, "analyst", result, issue_id, "/tmp", &cmd).unwrap();

        let move_effects: Vec<_> = decision
            .effects
            .iter()
            .filter(|e| e.effect_type == EffectType::MoveIssue)
            .collect();
        let comment_effects: Vec<_> = decision
            .effects
            .iter()
            .filter(|e| e.effect_type == EffectType::PostComment)
            .collect();
        let estimate_effects: Vec<_> = decision
            .effects
            .iter()
            .filter(|e| e.effect_type == EffectType::UpdateEstimate)
            .collect();

        assert!(
            !move_effects.is_empty(),
            "analyst done must produce MoveIssue"
        );
        assert!(
            !comment_effects.is_empty(),
            "analyst done must produce PostComment"
        );

        for e in &move_effects {
            assert!(e.blocking, "MoveIssue must be blocking=true");
        }
        for e in &comment_effects {
            assert!(!e.blocking, "PostComment must be blocking=false");
        }
        for e in &estimate_effects {
            assert!(e.blocking, "UpdateEstimate must be blocking=true");
        }
    }

    /// Fix 1 regression: engineer DONE with no PR_URL in output must emit CreatePr effect,
    /// never call auto_create_pr() directly. FakeCommandRunner should record 0 cmd calls.
    #[test]
    fn test_engineer_done_emits_create_pr_effect_not_direct_call() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "ENG-CREATEPR";
        let task_id = "eng-createpr-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = crate::models::Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "engineer".to_string();
        t.task_type = "pipeline-engineer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();

        // No PR_URL in output → should queue CreatePr effect, not call git/gh commands.
        let result = "Implementation complete.\nAll tests pass.\nVERDICT=DONE";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        // Must have CreatePr effect
        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::CreatePr),
            "engineer DONE without PR_URL must emit CreatePr effect, got: {:?}",
            decision.effects
        );

        // Must NOT have AttachUrl (no URL to attach yet)
        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::AttachUrl),
            "engineer DONE without PR_URL must not emit AttachUrl, got: {:?}",
            decision.effects
        );

        // FakeCommandRunner must have 0 calls — no direct git/gh side effects in decide path.
        // (The PR creation happens in the effect processor, not here.)
        let calls = cmd.calls.borrow();
        assert!(
            calls.is_empty(),
            "decide_callback must not call any commands for engineer DONE (no auto_create_pr), got: {calls:?}"
        );

        // RIG-334: No PR_URL → reviewer must NOT be spawned immediately.
        // The poller will create the reviewer after CreatePr effect completes.
        assert!(
            decision.internal.spawn_task.is_none(),
            "engineer DONE without PR_URL must NOT spawn reviewer (RIG-334), got: {:?}",
            decision.internal.spawn_task
        );
    }

    /// Fix 1 variant: engineer DONE with PR_URL already in output → emit AttachUrl, no CreatePr.
    #[test]
    fn test_engineer_done_with_pr_url_emits_attach_url_not_create_pr() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "ENG-ATTACHURL";
        let task_id = "eng-attachurl-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = crate::models::Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "engineer".to_string();
        t.task_type = "pipeline-engineer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();

        // Agent included PR_URL in output → should queue AttachUrl, not CreatePr.
        let result =
            "Implementation complete.\nPR_URL=https://github.com/org/repo/pull/42\nVERDICT=DONE";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::AttachUrl),
            "engineer DONE with PR_URL must emit AttachUrl, got: {:?}",
            decision.effects
        );

        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::CreatePr),
            "engineer DONE with PR_URL must not emit CreatePr, got: {:?}",
            decision.effects
        );

        // RIG-334: With PR_URL present, reviewer SHOULD be spawned immediately.
        let spawned = decision.internal.spawn_task.as_ref();
        assert!(
            spawned.is_some(),
            "engineer DONE with PR_URL must spawn reviewer task"
        );
        assert_eq!(spawned.unwrap().pipeline_stage, "reviewer");
    }

    /// RIG-334 regression: engineer DONE without PR_URL must NOT spawn reviewer.
    /// The reviewer should only be created by the poller after the CreatePr effect
    /// succeeds and the issue moves to Review status.
    #[test]
    fn test_engineer_done_no_pr_url_defers_reviewer_spawn() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-334-TEST";
        let task_id = "rig334-eng-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = crate::models::Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "engineer".to_string();
        t.task_type = "pipeline-engineer".to_string();
        t.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&t).unwrap();

        // Engineer output: DONE but no PR_URL
        let result = "Code changes committed to branch.\ncargo test passes.\nVERDICT=DONE";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        // MoveIssue to review should still be queued
        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "engineer DONE must queue MoveIssue to review"
        );

        // CreatePr must be queued (blocking)
        let create_pr = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::CreatePr);
        assert!(create_pr.is_some(), "must queue CreatePr effect");
        assert!(create_pr.unwrap().blocking, "CreatePr must be blocking");

        // Reviewer must NOT be spawned — this is the RIG-334 fix
        assert!(
            decision.internal.spawn_task.is_none(),
            "reviewer must NOT be spawned when no PR_URL (RIG-334)"
        );
    }

    /// RIG-334 complement: engineer DONE WITH PR_URL spawns reviewer immediately.
    #[test]
    fn test_engineer_done_with_pr_url_spawns_reviewer() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-334-PR";
        let task_id = "rig334-eng-pr-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = crate::models::Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "engineer".to_string();
        t.task_type = "pipeline-engineer".to_string();
        t.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&t).unwrap();

        // Engineer output: DONE with PR_URL
        let result =
            "Code changes committed.\nPR_URL=https://github.com/org/repo/pull/55\nVERDICT=DONE";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        // Reviewer SHOULD be spawned — PR artifact exists
        let spawned = decision.internal.spawn_task.as_ref();
        assert!(
            spawned.is_some(),
            "engineer DONE with PR_URL must spawn reviewer (RIG-334)"
        );
        assert_eq!(spawned.unwrap().pipeline_stage, "reviewer");
        assert_eq!(spawned.unwrap().linear_issue_id, issue_id);

        // No CreatePr effect (PR already exists)
        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::CreatePr),
            "must not queue CreatePr when PR_URL is present"
        );
    }

    // ─── RIG-335: Engineer stage delivery guarantee tests ──────────────

    /// Engineer DONE defaults to "done" verdict even without explicit VERDICT= marker.
    /// This is by design — engineer/analyst stages auto-default to done.
    #[test]
    fn decide_engineer_defaults_to_done_without_explicit_verdict() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-335-DEF";
        let task_id = "rig335-def-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "engineer".to_string();
        t.task_type = "pipeline-engineer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();

        // No VERDICT= in output — engineer should default to "done"
        let result = "All changes committed and pushed.\nTests pass.";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        // Should still produce MoveIssue (defaults to done → review transition)
        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "engineer without explicit verdict should default to done and queue MoveIssue"
        );

        // No PR_URL → CreatePr should be queued, reviewer NOT spawned
        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::CreatePr),
            "engineer defaulting to done without PR_URL must queue CreatePr"
        );
        assert!(
            decision.internal.spawn_task.is_none(),
            "engineer without PR_URL must not spawn reviewer even when defaulting to done"
        );
    }

    /// Engineer BLOCKED verdict should NOT queue CreatePr or spawn reviewer.
    #[test]
    fn decide_engineer_blocked_does_not_create_pr() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-335-BLK";
        let task_id = "rig335-blk-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "engineer".to_string();
        t.task_type = "pipeline-engineer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();

        let result = "Cannot implement — dependency not available.\nVERDICT=BLOCKED";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        // BLOCKED should NOT queue CreatePr
        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::CreatePr),
            "engineer BLOCKED must NOT queue CreatePr"
        );

        // Should NOT spawn reviewer
        assert!(
            decision.internal.spawn_task.is_none(),
            "engineer BLOCKED must NOT spawn reviewer"
        );
    }

    /// Reviewer APPROVED with PR_URL should queue proper effects.
    #[test]
    fn decide_reviewer_approved_posts_pr_review() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-335-RAPPR";
        let task_id = "rig335-rappr-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "reviewer".to_string();
        t.task_type = "pipeline-reviewer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();

        let result = "---COMMENT---\nLGTM! Clean implementation.\n---END COMMENT---\nREVIEW_VERDICT=APPROVED";

        let decision =
            decide_callback(&db, task_id, "reviewer", result, issue_id, "/tmp", &cmd).unwrap();

        // Should queue PostPrComment with approve event
        let pr_comment = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::PostPrComment);
        assert!(
            pr_comment.is_some(),
            "reviewer APPROVED with review body must queue PostPrComment"
        );
        let payload = &pr_comment.unwrap().payload;
        assert_eq!(
            payload.get("review_event").and_then(|v| v.as_str()),
            Some("approve"),
            "APPROVED verdict must map to 'approve' review event"
        );

        // Should move issue (APPROVED → next status)
        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "reviewer APPROVED must queue MoveIssue"
        );
    }

    /// Reviewer REJECTED must map to 'request-changes' review event.
    #[test]
    fn decide_reviewer_rejected_uses_request_changes_event() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-335-RREJ";
        let task_id = "rig335-rrej-t1";

        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "reviewer".to_string();
        t.task_type = "pipeline-reviewer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();

        let result = "---COMMENT---\n- blocker: missing error handling\n---END COMMENT---\nREVIEW_VERDICT=REJECTED";

        let decision =
            decide_callback(&db, task_id, "reviewer", result, issue_id, "/tmp", &cmd).unwrap();

        let pr_comment = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::PostPrComment);
        assert!(
            pr_comment.is_some(),
            "reviewer REJECTED with review body must queue PostPrComment"
        );
        assert_eq!(
            pr_comment
                .unwrap()
                .payload
                .get("review_event")
                .and_then(|v| v.as_str()),
            Some("request-changes"),
            "REJECTED verdict must map to 'request-changes' review event (RIG-318)"
        );
    }

    /// RIG-333: Reviewer callback returns handoff_update in InternalChanges (not direct DB write).
    #[test]
    fn decide_reviewer_stores_handoff_content() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-333";
        let task_id = "decide-333-r";
        insert_reviewer_task(&db, task_id, issue_id);

        let result =
            "## Review\n- blocker: missing tests\n- nit: typo in docs\n\nREVIEW_VERDICT=REJECTED";

        let decision =
            decide_callback(&db, task_id, "reviewer", result, issue_id, "/tmp", &cmd).unwrap();

        // Verify handoff_update is set in InternalChanges (applied atomically by callback())
        let (ref tid, ref content) = decision
            .internal
            .handoff_update
            .expect("handoff_update should be set for rejected reviewer");
        assert_eq!(tid, task_id);
        assert!(
            content.contains("Previous Review"),
            "handoff should contain review summary, got: {content}",
        );
        assert!(
            content.contains("REJECTED"),
            "handoff should contain verdict, got: {content}",
        );
    }

    // ─── RIG-338: Stage retry cap tests ──────────────────────────────────

    /// Helper: insert a completed deployer task.
    fn insert_deployer_task(db: &crate::db::Db, task_id: &str, issue_id: &str) {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "deployer".to_string();
        t.task_type = "pipeline-deployer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();
    }

    /// Helper: insert a failed deployer task.
    fn insert_failed_deployer_task(db: &crate::db::Db, task_id: &str, issue_id: &str) {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Failed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "deployer".to_string();
        t.task_type = "pipeline-deployer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();
    }

    /// Helper: insert a failed reviewer task.
    fn insert_failed_reviewer_task(db: &crate::db::Db, task_id: &str, issue_id: &str) {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Failed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "reviewer".to_string();
        t.task_type = "pipeline-reviewer".to_string();
        t.working_dir = "~/projects/werma".to_string();
        db.insert_task(&t).unwrap();
    }

    /// RIG-338: When failed attempt count is below max_stage_attempts, normal transition proceeds.
    #[test]
    fn decide_retry_cap_below_limit_proceeds_normally() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-338-BELOW";

        // Only 1 prior failed deployer task (below the cap of 2)
        insert_failed_deployer_task(&db, "338-below-d1", issue_id);

        // Current attempt also fails, but total failed = 1 (below cap of 2)
        let result = "Deploy complete.\nDEPLOY_VERDICT=DONE";

        let decision = decide_callback(
            &db,
            "338-below-d1",
            "deployer",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Normal transition should proceed — MoveIssue to "done"
        let move_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::MoveIssue);
        assert!(
            move_effect.is_some(),
            "below cap: should queue MoveIssue, got: {:?}",
            decision.effects
        );
        let payload = &move_effect.unwrap().payload;
        assert_eq!(
            payload.get("target_status").and_then(|v| v.as_str()),
            Some("done"),
            "below cap: should move to 'done', not escalate"
        );
    }

    /// RIG-338: When failed attempt count reaches max_stage_attempts, escalation fires.
    #[test]
    fn decide_retry_cap_at_limit_escalates() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-338-CAP";

        // 2 prior failed deployer tasks (at the cap of 2)
        insert_failed_deployer_task(&db, "338-cap-d1", issue_id);
        insert_failed_deployer_task(&db, "338-cap-d2", issue_id);

        // 3rd attempt also fails — should be capped
        let result = "Deploy failed.\nDEPLOY_VERDICT=FAILED";

        let decision = decide_callback(
            &db,
            "338-cap-d2",
            "deployer",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Should escalate via on_max_rounds=blocked → backlog
        let move_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::MoveIssue);
        assert!(
            move_effect.is_some(),
            "at cap: should queue MoveIssue for escalation"
        );
        let payload = &move_effect.unwrap().payload;
        assert_eq!(
            payload.get("target_status").and_then(|v| v.as_str()),
            Some("backlog"),
            "at cap: should escalate to backlog via on_max_rounds"
        );

        // Should have escalation comment
        assert!(
            decision.effects.iter().any(|e| {
                e.effect_type == EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("retry cap reached"))
            }),
            "at cap: should post retry cap comment"
        );

        // Should NOT spawn any next task
        assert!(
            decision.internal.spawn_task.is_none(),
            "at cap: must not spawn next task"
        );
    }

    /// RIG-338: Successful verdict at the boundary must NOT trigger the cap.
    /// Regression test: previously the cap fired unconditionally on all verdicts,
    /// discarding completed work when a stage succeeded on the Nth attempt.
    #[test]
    fn decide_retry_cap_success_at_boundary_advances_normally() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-338-SUCC";

        // 2 prior failed deployer tasks (at the cap of 2).
        // The 3rd attempt (current) succeeds — should NOT be capped.
        insert_failed_deployer_task(&db, "338-succ-d1", issue_id);
        insert_failed_deployer_task(&db, "338-succ-d2", issue_id);
        // Current task is completed (success) — insert as completed.
        insert_deployer_task(&db, "338-succ-d3", issue_id);

        let result = "Deploy succeeded.\nDEPLOY_VERDICT=DONE";

        let decision = decide_callback(
            &db,
            "338-succ-d3",
            "deployer",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Should advance normally to "done", NOT escalate to "backlog"
        let move_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::MoveIssue);
        assert!(
            move_effect.is_some(),
            "success at boundary: should queue MoveIssue"
        );
        let payload = &move_effect.unwrap().payload;
        assert_eq!(
            payload.get("target_status").and_then(|v| v.as_str()),
            Some("done"),
            "success at boundary: should advance to 'done', not escalate to 'backlog'"
        );
    }

    /// RIG-338: Retry cap works for reviewer stage (cap of 3).
    #[test]
    fn decide_retry_cap_reviewer_at_limit() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-338-REV";

        // 3 prior failed reviewer tasks (at the cap of 3)
        insert_failed_reviewer_task(&db, "338-rev-r1", issue_id);
        insert_failed_reviewer_task(&db, "338-rev-r2", issue_id);
        insert_failed_reviewer_task(&db, "338-rev-r3", issue_id);

        let result = "## Review\n- issues found\n\nREVIEW_VERDICT=REJECTED";

        let decision = decide_callback(
            &db,
            "338-rev-r3",
            "reviewer",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Should escalate via on_max_rounds=blocked → backlog
        let move_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::MoveIssue);
        assert!(move_effect.is_some(), "reviewer at cap: should escalate");
        let payload = &move_effect.unwrap().payload;
        assert_eq!(
            payload.get("target_status").and_then(|v| v.as_str()),
            Some("backlog"),
            "reviewer at cap: should escalate to backlog"
        );

        // Must NOT spawn engineer (retry prevented)
        assert!(
            decision.internal.spawn_task.is_none(),
            "reviewer at cap: must not spawn engineer"
        );
    }

    /// RIG-338: Reviewer success at boundary must advance, not escalate.
    #[test]
    fn decide_retry_cap_reviewer_success_at_boundary_advances() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-338-REVSUCC";

        // 3 prior failed reviewer tasks (at the cap of 3).
        // Current (4th) attempt succeeds — should advance, not escalate.
        insert_failed_reviewer_task(&db, "338-rs-r1", issue_id);
        insert_failed_reviewer_task(&db, "338-rs-r2", issue_id);
        insert_failed_reviewer_task(&db, "338-rs-r3", issue_id);
        insert_reviewer_task(&db, "338-rs-r4", issue_id);

        let result = "## Review\nAll looks good.\n\nREVIEW_VERDICT=APPROVED";

        let decision =
            decide_callback(&db, "338-rs-r4", "reviewer", result, issue_id, "/tmp", &cmd).unwrap();

        // Should advance to "ready", not escalate to "backlog"
        let move_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::MoveIssue);
        assert!(
            move_effect.is_some(),
            "reviewer success at boundary: should queue MoveIssue"
        );
        let payload = &move_effect.unwrap().payload;
        assert_eq!(
            payload.get("target_status").and_then(|v| v.as_str()),
            Some("ready"),
            "reviewer success at boundary: should advance to 'ready', not 'backlog'"
        );
    }

    /// RIG-338: Stages without max_stage_attempts configured are unaffected.
    #[test]
    fn decide_retry_cap_not_configured_proceeds_normally() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-338-NOCONF";

        // Many prior analyst tasks — but analyst has no max_stage_attempts
        for i in 0..5 {
            insert_analyst_task(&db, &format!("338-nc-a{i}"), issue_id);
        }

        let result = &format!("{}\n\nVERDICT=DONE", valid_analyst_output(3));

        let decision =
            decide_callback(&db, "338-nc-a4", "analyst", result, issue_id, "/tmp", &cmd).unwrap();

        // Normal transition should proceed (no cap configured)
        let move_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::MoveIssue);
        assert!(
            move_effect.is_some(),
            "uncapped stage: should proceed normally"
        );
        let payload = &move_effect.unwrap().payload;
        assert_eq!(
            payload.get("target_status").and_then(|v| v.as_str()),
            Some("todo"),
            "uncapped stage: should follow normal transition to todo"
        );
    }

    // ─── RIG-340: Analyst spec validation gate ──────────────────────────

    #[test]
    fn decide_analyst_done_missing_sections_blocks_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-340-MISS";
        insert_analyst_task(&db, "340-miss-a1", issue_id);

        // Missing all required sections
        let result = "## Spec\nImplement feature X\n\nESTIMATE=5\nVERDICT=DONE";

        let decision = decide_callback(
            &db,
            "340-miss-a1",
            "analyst",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Must NOT have MoveIssue — validation blocks the transition
        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "missing spec sections must block MoveIssue, got: {:?}",
            decision.effects
        );

        // Must have a comment explaining what's missing
        let validation_comment = decision
            .effects
            .iter()
            .find(|e| e.dedup_key.contains("spec_validation_failed"));
        assert!(
            validation_comment.is_some(),
            "must post comment about missing sections"
        );
        let body = validation_comment
            .unwrap()
            .payload
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            body.contains("## Scope")
                && body.contains("## Acceptance Criteria")
                && body.contains("## Out of Scope"),
            "comment must list all missing sections, got: {body}"
        );

        // Must NOT spawn next task
        assert!(
            decision.internal.spawn_task.is_none(),
            "validation failure must not spawn next task"
        );
    }

    #[test]
    fn decide_analyst_done_partial_sections_blocks_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-340-PART";
        insert_analyst_task(&db, "340-part-a1", issue_id);

        // Has Scope and AC but missing Out of Scope
        let result = "## Scope\nDo X\n## Acceptance Criteria\n- AC1\n\nESTIMATE=3\nVERDICT=DONE";

        let decision = decide_callback(
            &db,
            "340-part-a1",
            "analyst",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "partially valid spec must still block transition"
        );

        let validation_comment = decision
            .effects
            .iter()
            .find(|e| e.dedup_key.contains("spec_validation_failed"));
        let body = validation_comment
            .unwrap()
            .payload
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            body.contains("## Out of Scope"),
            "comment must list only the missing section, got: {body}"
        );
        assert!(
            !body.contains("## Scope\n"),
            "comment must not list sections that are present"
        );
    }

    #[test]
    fn decide_analyst_done_valid_spec_allows_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-340-VALID";
        insert_analyst_task(&db, "340-valid-a1", issue_id);

        let result = &valid_analyst_output(5);

        let decision = decide_callback(
            &db,
            "340-valid-a1",
            "analyst",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "valid spec must allow transition, got: {:?}",
            decision.effects
        );

        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.dedup_key.contains("spec_validation_failed")),
            "valid spec must not emit validation failure comment"
        );
    }

    #[test]
    fn decide_analyst_blocked_skips_validation() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-340-BLOCK";
        insert_analyst_task(&db, "340-block-a1", issue_id);

        // BLOCKED verdict — no spec sections needed
        let result = "Need clarification on requirements.\nVERDICT=BLOCKED";

        let decision = decide_callback(
            &db,
            "340-block-a1",
            "analyst",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Blocked should still produce a transition (to backlog or similar)
        // but must NOT fail validation — validation only runs on "done"
        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.dedup_key.contains("spec_validation_failed")),
            "BLOCKED verdict must skip spec validation"
        );
    }

    // ─── RIG-353: Additional verdict path coverage ────────────────────────

    /// Reviewer empty output + no GitHub review → PostComment "empty output", no transition.
    #[test]
    fn decide_reviewer_empty_output_no_github_review() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-353-REMPTY";
        let task_id = "353-rempty-t1";
        insert_reviewer_task(&db, task_id, issue_id);

        let decision = decide_callback(
            &db,
            task_id,
            "reviewer",
            "   \n  \n  ",
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        let effects = &decision.effects;

        assert!(
            effects.iter().any(|e| {
                e.effect_type == EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("empty output"))
            }),
            "reviewer empty output should queue 'empty output' comment, got: {effects:?}"
        );

        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "reviewer empty output must not queue MoveIssue"
        );

        assert!(
            decision.internal.spawn_task.is_none(),
            "reviewer empty output must not spawn next task"
        );
    }

    #[test]
    fn decide_reviewer_fallback_to_github_verdict_approved() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-353-GHAPPR";
        let task_id = "353-ghappr-t1";
        insert_reviewer_task(&db, task_id, issue_id);

        cmd.push_success(&format!(
            r#"[{{"number":42,"headRefName":"feat/rig-353-ghappr-fix","reviewDecision":"APPROVED"}}]"#
        ));

        let decision =
            decide_callback(&db, task_id, "reviewer", "", issue_id, "/tmp", &cmd).unwrap();

        let effects = &decision.effects;

        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "GitHub APPROVED fallback should produce MoveIssue, got: {effects:?}"
        );
    }

    #[test]
    fn decide_reviewer_fallback_to_github_verdict_rejected() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-353-GHREJ";
        let task_id = "353-ghrej-t1";
        insert_reviewer_task(&db, task_id, issue_id);

        cmd.push_success(&format!(
            r#"[{{"number":42,"headRefName":"feat/rig-353-ghrej-fix","reviewDecision":"CHANGES_REQUESTED"}}]"#
        ));

        let decision =
            decide_callback(&db, task_id, "reviewer", "", issue_id, "/tmp", &cmd).unwrap();

        let effects = &decision.effects;

        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "GitHub REJECTED fallback should produce MoveIssue, got: {effects:?}"
        );

        assert!(
            decision.internal.spawn_task.is_some(),
            "GitHub REJECTED fallback should spawn engineer for fixes"
        );
        assert_eq!(
            decision
                .internal
                .spawn_task
                .as_ref()
                .unwrap()
                .pipeline_stage,
            "engineer",
            "spawned task should be engineer"
        );
    }

    #[test]
    fn decide_analyst_blocked_queues_label_swap() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-353-ABLK";
        let task_id = "353-ablk-t1";
        insert_analyst_task(&db, task_id, issue_id);

        let result = "Cannot analyze: missing requirements.\n\nVERDICT=BLOCKED";

        let decision =
            decide_callback(&db, task_id, "analyst", result, issue_id, "/tmp", &cmd).unwrap();

        let effects = &decision.effects;

        let remove_label = effects.iter().find(|e| {
            e.effect_type == EffectType::RemoveLabel
                && e.payload
                    .get("label")
                    .and_then(|v| v.as_str())
                    .is_some_and(|l| l == "analyze")
        });
        assert!(
            remove_label.is_some(),
            "analyst BLOCKED should remove 'analyze' label, got: {effects:?}"
        );

        let add_label = effects.iter().find(|e| {
            e.effect_type == EffectType::AddLabel
                && e.payload
                    .get("label")
                    .and_then(|v| v.as_str())
                    .is_some_and(|l| l == "analyze:blocked")
        });
        assert!(
            add_label.is_some(),
            "analyst BLOCKED should add 'analyze:blocked' label, got: {effects:?}"
        );
    }

    #[test]
    fn decide_review_cycle_at_max_rounds_escalates() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "RIG-353-CYCLE";

        for i in 0..DEFAULT_MAX_REVIEW_ROUNDS {
            let tid = format!("353-cycle-r{i}");
            let mut t = crate::db::make_test_task(&tid);
            t.status = Status::Completed;
            t.linear_issue_id = issue_id.to_string();
            t.pipeline_stage = "reviewer".to_string();
            t.task_type = "pipeline-reviewer".to_string();
            t.working_dir = "~/projects/werma".to_string();
            db.insert_task(&t).unwrap();
        }

        let task_id = "353-cycle-cur";
        insert_reviewer_task(&db, task_id, issue_id);

        let result = "## Review\n- still broken\n\nREVIEW_VERDICT=REJECTED";

        let decision =
            decide_callback(&db, task_id, "reviewer", result, issue_id, "/tmp", &cmd).unwrap();

        let effects = &decision.effects;

        let escalation_move = effects.iter().find(|e| {
            e.effect_type == EffectType::MoveIssue
                && e.payload
                    .get("target_status")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == "backlog")
        });
        assert!(
            escalation_move.is_some(),
            "at max_rounds, REJECTED should escalate to backlog, got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("cycle limit"))
            }),
            "escalation should include cycle limit comment, got: {effects:?}"
        );

        assert!(
            decision.internal.spawn_task.is_none(),
            "at max_rounds, must NOT spawn engineer — escalated to backlog"
        );
    }
}
