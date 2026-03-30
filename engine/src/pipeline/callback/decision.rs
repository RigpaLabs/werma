use anyhow::Result;

use super::super::config::StageConfig;
use super::super::helpers::truncate_lines;
use super::super::loader::load_default;
use super::super::pr::{get_pr_review_verdict, has_open_pr_for_issue, pr_title_from_url};
use super::super::verdict::{
    extract_review_body, is_max_turns_exit, parse_comments, parse_estimate, parse_pr_url,
    parse_verdict,
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
                // RIG-232: No PR_URL in output — queue CreatePr effect.
                // Effect processor calls auto_create_pr() atomically after the transaction.
                eprintln!(
                    "callback: engineer DONE but no PR_URL in output for {linear_issue_id} \
                     (task {task_id}) — queuing CreatePr effect."
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
                             Proceeding to reviewer — reviewer will verify or request a PR."
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
        let db = crate::db::Db::open_in_memory().unwrap();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let issue_id = "DECIDE-100";
        insert_analyst_task(&db, "decide-100-a", issue_id);

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
            "~/projects/rigpa/werma",
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

        let result = "## Spec\nSome content\n\nESTIMATE=3";

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

        let result =
            "## Spec\nImplement the feature.\n\nEstimate: 3 SP\n\nESTIMATE=3\n\nVERDICT=done";

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
        t.working_dir = "~/projects/rigpa/werma".to_string();
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
        t.working_dir = "~/projects/rigpa/werma".to_string();
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
}
