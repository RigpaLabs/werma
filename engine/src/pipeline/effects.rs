use std::collections::HashMap;

use anyhow::Result;

use super::callback::move_with_retry;
use super::pr::{auto_create_pr, post_pr_review, pr_title_from_url};
use crate::db::Db;
use crate::linear::LinearApi;
use crate::models::{Effect, EffectType};
use crate::traits::{CommandRunner, Notifier};

/// Result of a single `process_effects()` call.
pub struct ProcessResult {
    pub processed: usize,
    pub failed: usize,
}

/// Drain the effects outbox: fetch pending effects, execute them, mark done/failed.
///
/// Groups effects by task_id and processes them in id order. If a blocking effect
/// fails, the remaining effects for that task are skipped until next processor run.
/// After all of a task's effects succeed, marks `linear_pushed = true` on the task.
pub fn process_effects(
    db: &Db,
    linear: &dyn LinearApi,
    cmd: &dyn CommandRunner,
    notifier: &dyn Notifier,
) -> Result<ProcessResult> {
    let batch = db.pending_effects(20)?;

    // Group effects by task_id, preserving id order (batch is already ASC by id).
    let mut by_task: HashMap<String, Vec<Effect>> = HashMap::new();
    let mut task_order: Vec<String> = Vec::new();

    for effect in batch {
        let tid = effect.task_id.clone();
        if !by_task.contains_key(&tid) {
            task_order.push(tid.clone());
        }
        by_task.entry(tid).or_default().push(effect);
    }

    let mut processed = 0usize;
    let mut failed = 0usize;

    for task_id in task_order {
        let effects = by_task.remove(&task_id).unwrap_or_default();

        for effect in &effects {
            match execute_effect(effect, db, linear, cmd, notifier) {
                Ok(()) => {
                    db.mark_effect_done(effect.id)?;
                    processed += 1;
                }
                Err(e) => {
                    let msg = e.to_string();
                    db.mark_effect_failed(effect.id, &msg)?;
                    failed += 1;

                    // Blocking effects halt the chain for this task.
                    if effect.blocking {
                        eprintln!(
                            "[effects] blocking effect {} (type={:?}) failed for task {}: {msg}",
                            effect.id, effect.effect_type, task_id
                        );
                        break;
                    }
                }
            }
        }

        // After processing this task's batch: if all blocking effects are done → mark pushed.
        if db.blocking_effects_done(&task_id)? {
            db.set_linear_pushed(&task_id, true)?;
        }
    }

    Ok(ProcessResult { processed, failed })
}

/// Execute a single effect, dispatching on its type.
///
/// All EffectType variants are matched explicitly — no catch-all.
/// RIG-355: accepts `&Db` so CreatePr/PostPrComment can re-read the task's current
/// working_dir (the payload may have been created before the runner updated the DB).
pub fn execute_effect(
    effect: &Effect,
    db: &Db,
    linear: &dyn LinearApi,
    cmd: &dyn CommandRunner,
    notifier: &dyn Notifier,
) -> Result<()> {
    let issue_id = &effect.issue_id;
    let task_id = &effect.task_id;
    let payload = &effect.payload;

    match effect.effect_type {
        EffectType::MoveIssue => {
            let target = payload_str(payload, "target_status")?;
            move_with_retry(linear, issue_id, target)
        }

        EffectType::PostComment => {
            let body = payload_str(payload, "body")?;
            linear.comment(issue_id, body)
        }

        EffectType::AddLabel => {
            let label = payload_str(payload, "label")?;
            linear.add_label(issue_id, label)
        }

        EffectType::RemoveLabel => {
            let label = payload_str(payload, "label")?;
            linear.remove_label(issue_id, label)
        }

        EffectType::UpdateEstimate => {
            let estimate = payload["estimate"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("effect payload missing 'estimate'"))?
                as i32;
            linear.update_estimate(issue_id, estimate)
        }

        EffectType::CreatePr => {
            // RIG-355: Re-read working_dir from task DB record. The payload may have
            // been created before the runner updated task.working_dir to the worktree path.
            let payload_wd = payload_str(payload, "working_dir")?;
            let working_dir = db
                .task(task_id)
                .ok()
                .flatten()
                .map(|t| t.working_dir.clone())
                .filter(|wd| !wd.is_empty())
                .unwrap_or_else(|| payload_wd.to_string());
            if working_dir != payload_wd {
                eprintln!(
                    "[effects] CreatePr: using DB working_dir '{working_dir}' \
                     (payload had '{payload_wd}') for {issue_id}"
                );
            }
            if let Some(url) = auto_create_pr(cmd, &working_dir, issue_id, task_id)? {
                eprintln!("[effects] CreatePr: created PR {url} for {issue_id}");
                let title = pr_title_from_url(&url);
                linear.attach_url(issue_id, &url, &title)?;
            } else {
                eprintln!(
                    "[effects] CreatePr: no PR created for {issue_id} (skipped — already exists or on main)"
                );
            }
            Ok(())
        }

        EffectType::AttachUrl => {
            let url = payload_str(payload, "url")?;
            let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or(url);
            linear.attach_url(issue_id, url, title)
        }

        EffectType::PostPrComment => {
            let body = payload_str(payload, "body")?;
            let payload_wd = payload
                .get("working_dir")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp");
            // RIG-355: Re-read working_dir from task DB record for the same reason
            // as CreatePr. For reviewer tasks (read-only, no worktree), also try to
            // find the engineer task's worktree for the same issue — that's where the
            // PR's feature branch lives.
            let working_dir = resolve_pr_working_dir(db, task_id, issue_id, payload_wd);
            let review_event = payload
                .get("review_event")
                .and_then(|v| v.as_str())
                .unwrap_or("comment");
            // RIG-318: post a proper PR review (not an issue comment).
            // Errors propagate so the outbox retries (e.g. PR not yet created).
            post_pr_review(cmd, &working_dir, body, review_event)
        }

        EffectType::Notify => {
            let channel = payload_str(payload, "channel")?;
            let msg = payload_str(payload, "message")?;
            notifier.notify_slack(channel, msg);
            Ok(())
        }
    }
}

/// RIG-355: Resolve the working_dir for PR operations (CreatePr, PostPrComment).
///
/// For reviewer tasks (read-only, no worktree), the task's own working_dir is the base
/// repo (on main branch) — useless for finding the PR. Instead, look up the most recent
/// completed engineer task for the same issue — its working_dir is the worktree where
/// the feature branch lives.
fn resolve_pr_working_dir(db: &Db, task_id: &str, issue_id: &str, fallback: &str) -> String {
    // First: try the current task's working_dir from DB
    if let Ok(Some(task)) = db.task(task_id) {
        if !task.working_dir.is_empty() && task.working_dir.contains(".trees/") {
            return task.working_dir;
        }
    }

    // Second: look for the engineer task's worktree for the same issue
    if let Ok(tasks) = db.tasks_by_linear_issue(issue_id, None, false) {
        // Find the most recent completed engineer task with a worktree path
        if let Some(eng_task) = tasks
            .iter()
            .rev()
            .find(|t| t.pipeline_stage == "engineer" && t.working_dir.contains(".trees/"))
        {
            eprintln!(
                "[effects] PostPrComment: using engineer worktree '{}' for {issue_id} \
                 (reviewer task {task_id} has no worktree)",
                eng_task.working_dir
            );
            return eng_task.working_dir.clone();
        }
    }

    fallback.to_string()
}

/// Extract a string field from an effect payload, returning `Err` if absent or not a string.
fn payload_str<'a>(payload: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    payload[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("effect payload missing '{key}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::models::{Effect, EffectStatus, EffectType, Status};
    use crate::traits::fakes::{FakeCommandRunner, FakeLinearApi, FakeNotifier};

    fn make_effect(
        task_id: &str,
        issue_id: &str,
        effect_type: EffectType,
        payload: serde_json::Value,
    ) -> Effect {
        Effect {
            id: 0,
            dedup_key: format!("{task_id}:{effect_type:?}"),
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

    fn insert_task(db: &Db, task_id: &str, issue_id: &str) {
        let mut t = crate::db::make_test_task(task_id);
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "analyst".to_string();
        db.insert_task(&t).unwrap();
    }

    #[test]
    fn process_effects_executes_pending() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();
        let issue_id = "EFF-100";
        let task_id = "eff-100-t";

        insert_task(&db, task_id, issue_id);
        linear.set_issue_status(issue_id, "Todo");

        let effect = make_effect(
            task_id,
            issue_id,
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress", "alert_on_failure": true }),
        );
        db.insert_effects(&[effect]).unwrap();

        let result = process_effects(&db, &linear, &cmd, &notifier).unwrap();
        assert_eq!(result.processed, 1);
        assert_eq!(result.failed, 0);

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].0, issue_id);
        assert_eq!(moves[0].1, "in_progress");
    }

    #[test]
    fn process_effects_marks_done_on_success() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();
        let issue_id = "EFF-101";
        let task_id = "eff-101-t";

        insert_task(&db, task_id, issue_id);
        linear.set_issue_status(issue_id, "Todo");

        let effect = make_effect(
            task_id,
            issue_id,
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress" }),
        );
        db.insert_effects(&[effect]).unwrap();

        process_effects(&db, &linear, &cmd, &notifier).unwrap();

        // No pending effects remain
        let pending = db.pending_effects(100).unwrap();
        assert!(
            pending.is_empty(),
            "all effects should be done, got: {pending:?}"
        );
    }

    #[test]
    fn process_effects_marks_failed_on_error() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();
        let issue_id = "EFF-102";
        let task_id = "eff-102-t";

        insert_task(&db, task_id, issue_id);
        linear.set_issue_status(issue_id, "Todo");
        // Make all move calls fail
        linear.fail_next_n_moves(99);

        let effect = make_effect(
            task_id,
            issue_id,
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress" }),
        );
        db.insert_effects(&[effect]).unwrap();

        let result = process_effects(&db, &linear, &cmd, &notifier).unwrap();
        assert_eq!(result.failed, 1);
        assert_eq!(result.processed, 0);

        // Effect has attempts=1 and is still retryable (max=5)
        // After failure it gets next_retry_at set, so won't appear in pending_effects yet.
        // But we can inspect db directly via a raw query or check counts.
        // Use a direct check: blocking_effects_done should return false.
        assert!(
            !db.blocking_effects_done(task_id).unwrap(),
            "blocking effects not yet done after failure"
        );
    }

    #[test]
    fn process_effects_stops_blocking_chain_on_failure() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();
        let issue_id = "EFF-103";
        let task_id = "eff-103-t";

        insert_task(&db, task_id, issue_id);
        linear.set_issue_status(issue_id, "Todo");
        linear.fail_next_n_moves(99);

        // First effect: MoveIssue (will fail — all moves disabled)
        let e1 = make_effect(
            task_id,
            issue_id,
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress" }),
        );
        // Second effect: PostComment (would succeed but should be skipped)
        let e2 = make_effect(
            task_id,
            issue_id,
            EffectType::PostComment,
            serde_json::json!({ "body": "should not be posted" }),
        );
        db.insert_effects(&[e1, e2]).unwrap();

        let result = process_effects(&db, &linear, &cmd, &notifier).unwrap();

        // Only 1 effect attempted (MoveIssue failed, chain halted)
        assert_eq!(result.failed, 1, "only the first effect should have failed");
        assert_eq!(result.processed, 0, "no effects should have succeeded");

        // No comments posted
        let comments = linear.comment_calls.borrow();
        assert!(
            comments.is_empty(),
            "PostComment should not have been called after blocking failure"
        );
    }

    #[test]
    fn process_effects_sets_linear_pushed_when_all_done() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();
        let issue_id = "EFF-104";
        let task_id = "eff-104-t";

        insert_task(&db, task_id, issue_id);
        linear.set_issue_status(issue_id, "Todo");

        let effect = make_effect(
            task_id,
            issue_id,
            EffectType::PostComment,
            serde_json::json!({ "body": "All done!" }),
        );
        db.insert_effects(&[effect]).unwrap();

        process_effects(&db, &linear, &cmd, &notifier).unwrap();

        let task = db.task(task_id).unwrap().unwrap();
        assert!(
            task.linear_pushed,
            "task.linear_pushed should be true after all effects done"
        );
    }

    #[test]
    fn execute_effect_move_issue_uses_move_with_retry() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.set_issue_status("EFF-105", "Todo");

        let effect = make_effect(
            "eff-105-t",
            "EFF-105",
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress" }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].1, "in_progress");
    }

    #[test]
    fn execute_effect_post_comment() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-pc-t",
            "EFF-PC",
            EffectType::PostComment,
            serde_json::json!({ "body": "Hello from processor" }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let comments = linear.comment_calls.borrow();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].1.contains("Hello from processor"));
    }

    #[test]
    fn execute_effect_add_label() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-al-t",
            "EFF-AL",
            EffectType::AddLabel,
            serde_json::json!({ "label": "spec:done" }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let labels = linear.add_label_calls.borrow();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].1, "spec:done");
    }

    #[test]
    fn execute_effect_remove_label() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-rl-t",
            "EFF-RL",
            EffectType::RemoveLabel,
            serde_json::json!({ "label": "analyze" }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let labels = linear.remove_label_calls.borrow();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].1, "analyze");
    }

    #[test]
    fn execute_effect_update_estimate() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-ue-t",
            "EFF-UE",
            EffectType::UpdateEstimate,
            serde_json::json!({ "estimate": 5 }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let estimates = linear.estimate_calls.borrow();
        assert_eq!(estimates.len(), 1);
        assert_eq!(estimates[0].1, 5);
    }

    #[test]
    fn execute_effect_attach_url() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-au-t",
            "EFF-AU",
            EffectType::AttachUrl,
            serde_json::json!({ "url": "https://github.com/org/repo/pull/99", "title": "PR #99" }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let attaches = linear.attach_calls.borrow();
        assert_eq!(attaches.len(), 1);
        assert!(attaches[0].1.contains("pull/99"));
    }

    #[test]
    fn execute_effect_create_pr_returns_error_on_empty_branch() {
        // RIG-355: FakeCommandRunner returns empty stdout → empty branch name
        // → auto_create_pr now returns Err (not Ok(None)), so effect retries.
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-cp-t",
            "EFF-CP",
            EffectType::CreatePr,
            serde_json::json!({ "working_dir": "/tmp" }),
        );

        // RIG-355: empty branch name → Err (not silent skip)
        let result = execute_effect(&effect, &db, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "CreatePr should return Err on empty branch (effect retries): {result:?}"
        );
    }

    #[test]
    fn execute_effect_create_pr_attaches_url_to_linear() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        cmd.push_success("feat/rig-315-fix-create-pr"); // git branch --show-current
        cmd.push_success("abc1234 RIG-315 feat: implementation"); // git log origin/main..HEAD
        cmd.push_success(""); // git push
        cmd.push_success(""); // gh pr view (no existing PR)
        cmd.push_success("https://github.com/org/repo/pull/99"); // gh pr create

        let effect = make_effect(
            "eff-cp-url-t",
            "EFF-CP-URL",
            EffectType::CreatePr,
            serde_json::json!({ "working_dir": "/tmp" }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let attaches = linear.attach_calls.borrow();
        assert_eq!(attaches.len(), 1, "attach_url should be called once");
        assert_eq!(attaches[0].0, "EFF-CP-URL");
        assert!(
            attaches[0].1.contains("pull/99"),
            "attached URL should be the PR URL"
        );
        assert_eq!(
            attaches[0].2, "PR #99",
            "title should be derived via pr_title_from_url"
        );
    }

    // ─── RIG-318: PostPrComment uses proper PR review, propagates errors ──

    #[test]
    fn execute_effect_post_pr_review_success() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // gh pr view → PR number
        cmd.push_success("42");
        // gh pr review → success
        cmd.push_success("");

        let effect = make_effect(
            "eff-ppc-ok-t",
            "EFF-PPC-OK",
            EffectType::PostPrComment,
            serde_json::json!({
                "body": "Review findings here.",
                "working_dir": "/tmp",
                "review_event": "approve",
            }),
        );

        let result = execute_effect(&effect, &db, &linear, &cmd, &notifier);
        assert!(
            result.is_ok(),
            "should succeed when PR review is posted: {result:?}"
        );

        // Verify gh pr review was called with --approve
        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].1.contains(&"review".to_string()));
        assert!(calls[1].1.contains(&"--approve".to_string()));
    }

    #[test]
    fn execute_effect_post_pr_review_no_pr_returns_error() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // FakeCommandRunner returns empty stdout → no PR found
        let effect = make_effect(
            "eff-ppc-nopr-t",
            "EFF-PPC-NOPR",
            EffectType::PostPrComment,
            serde_json::json!({ "body": "Review posted.", "working_dir": "/tmp" }),
        );

        let result = execute_effect(&effect, &db, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "PostPrComment with no PR should return Err for retry"
        );
    }

    #[test]
    fn execute_effect_post_pr_review_api_error_returns_error() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // gh pr view → PR number
        cmd.push_success("42");
        // gh pr review → API failure
        cmd.push_failure("HTTP 422: Validation Failed");

        let effect = make_effect(
            "eff-ppc-err-t",
            "EFF-PPC-ERR",
            EffectType::PostPrComment,
            serde_json::json!({
                "body": "Review findings.",
                "working_dir": "/tmp",
                "review_event": "comment",
            }),
        );

        let result = execute_effect(&effect, &db, &linear, &cmd, &notifier);
        assert!(result.is_err(), "PostPrComment should fail on API error");
    }

    #[test]
    fn execute_effect_post_pr_review_defaults_to_comment_event() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // gh pr view → PR number
        cmd.push_success("42");
        // gh pr review → success
        cmd.push_success("");

        // No review_event in payload → should default to "comment"
        let effect = make_effect(
            "eff-ppc-def-t",
            "EFF-PPC-DEF",
            EffectType::PostPrComment,
            serde_json::json!({ "body": "Review text.", "working_dir": "/tmp" }),
        );

        let result = execute_effect(&effect, &db, &linear, &cmd, &notifier);
        assert!(result.is_ok(), "should succeed: {result:?}");

        let calls = cmd.calls.borrow();
        assert!(calls[1].1.contains(&"--comment".to_string()));
    }

    #[test]
    fn execute_effect_notify() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-ntf-t",
            "EFF-NTF",
            EffectType::Notify,
            serde_json::json!({ "channel": "#alerts", "message": "Task done" }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        let notifications = notifier.slack_calls.borrow();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].0, "#alerts");
        assert!(notifications[0].1.contains("Task done"));
    }

    // ─── RIG-355: CreatePr re-reads working_dir from DB ─────────────────

    #[test]
    fn create_pr_prefers_db_working_dir_over_payload() {
        // Payload has stale base repo path, but DB has updated worktree path.
        // execute_effect should use the DB value.
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // Insert task with worktree path in DB (simulates RIG-351 runner update)
        let mut t = crate::db::make_test_task("rig355-eng-t");
        t.status = Status::Completed;
        t.linear_issue_id = "RIG-355-TEST".to_string();
        t.pipeline_stage = "engineer".to_string();
        t.working_dir =
            "/home/user/projects/repo/.trees/feat--RIG-355-pipeline-engineer-stage".to_string();
        db.insert_task(&t).unwrap();

        // Script auto_create_pr to succeed from the worktree path
        cmd.push_success("feat/RIG-355-pipeline-engineer-stage"); // git branch --show-current
        cmd.push_success("abc1234 RIG-355 feat: fix"); // git log origin/main..HEAD
        cmd.push_success(""); // git push
        cmd.push_success(""); // gh pr view (no existing PR)
        cmd.push_success("https://github.com/org/repo/pull/42"); // gh pr create

        // Effect payload has STALE base repo path (before runner updated DB)
        let effect = make_effect(
            "rig355-eng-t",
            "RIG-355-TEST",
            EffectType::CreatePr,
            serde_json::json!({
                "working_dir": "~/projects/repo",
                "issue_id": "RIG-355-TEST",
                "task_id": "rig355-eng-t",
            }),
        );

        execute_effect(&effect, &db, &linear, &cmd, &notifier).unwrap();

        // Verify PR was created (attach_url was called)
        let attaches = linear.attach_calls.borrow();
        assert_eq!(attaches.len(), 1, "PR should be attached to Linear issue");

        // Verify the command ran from the DB worktree path, not the payload path.
        // The first git command should have been executed with the worktree path.
        let calls = cmd.calls.borrow();
        assert!(!calls.is_empty(), "auto_create_pr should have been called");
        // The working_dir passed to commands should be the worktree path from DB
        let first_call_dir = &calls[0].2;
        assert!(
            first_call_dir
                .as_ref()
                .is_some_and(|d| d.contains(".trees/")),
            "command should run from worktree path (DB value), not base repo: {:?}",
            first_call_dir
        );
    }

    #[test]
    fn create_pr_returns_error_on_main_branch() {
        // RIG-355: auto_create_pr must return Err (not Ok(None)) when on main.
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // git branch --show-current returns "main"
        cmd.push_success("main");

        let effect = make_effect(
            "rig355-main-t",
            "RIG-355-MAIN",
            EffectType::CreatePr,
            serde_json::json!({ "working_dir": "/tmp" }),
        );

        let result = execute_effect(&effect, &db, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "CreatePr on main branch must return Err for retry: {result:?}"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("expected worktree feature branch"),
            "error message should explain the problem"
        );
    }

    #[test]
    fn post_pr_comment_resolves_engineer_worktree() {
        // RIG-355: PostPrComment for reviewer tasks should find the engineer's worktree.
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let issue_id = "RIG-355-REV";

        // Insert engineer task with worktree path
        let mut eng = crate::db::make_test_task("rig355-eng");
        eng.status = Status::Completed;
        eng.linear_issue_id = issue_id.to_string();
        eng.pipeline_stage = "engineer".to_string();
        eng.working_dir =
            "/home/user/projects/repo/.trees/feat--RIG-355-pipeline-engineer-stage".to_string();
        db.insert_task(&eng).unwrap();

        // Insert reviewer task with base repo path (no worktree)
        let mut rev = crate::db::make_test_task("rig355-rev");
        rev.status = Status::Completed;
        rev.linear_issue_id = issue_id.to_string();
        rev.pipeline_stage = "reviewer".to_string();
        rev.working_dir = "~/projects/repo".to_string();
        db.insert_task(&rev).unwrap();

        // Script gh pr view + gh pr review to succeed
        cmd.push_success("42"); // gh pr view → PR number
        cmd.push_success(""); // gh pr review → success

        let effect = make_effect(
            "rig355-rev",
            issue_id,
            EffectType::PostPrComment,
            serde_json::json!({
                "body": "LGTM!",
                "working_dir": "~/projects/repo",
                "review_event": "approve",
            }),
        );

        let result = execute_effect(&effect, &db, &linear, &cmd, &notifier);
        assert!(result.is_ok(), "PostPrComment should succeed: {result:?}");

        // Verify the command ran from the ENGINEER's worktree path
        let calls = cmd.calls.borrow();
        let first_call_dir = &calls[0].2;
        assert!(
            first_call_dir
                .as_ref()
                .is_some_and(|d| d.contains(".trees/")),
            "PostPrComment should resolve to engineer's worktree: {:?}",
            first_call_dir
        );
    }

    #[test]
    fn payload_str_returns_error_for_missing_key() {
        let payload = serde_json::json!({ "other_key": "value" });
        let result = payload_str(&payload, "target_status");
        assert!(result.is_err(), "missing key should return Err");
        assert!(
            result.unwrap_err().to_string().contains("target_status"),
            "error message should name the missing key"
        );
    }

    #[test]
    fn payload_str_returns_error_for_non_string_value() {
        let payload = serde_json::json!({ "target_status": 42 });
        let result = payload_str(&payload, "target_status");
        assert!(result.is_err(), "non-string value should return Err");
    }

    #[test]
    fn process_effects_multi_task_independent() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let issue_a = "EFF-200";
        let task_a = "eff-200-t";
        let issue_b = "EFF-201";
        let task_b = "eff-201-t";

        insert_task(&db, task_a, issue_a);
        insert_task(&db, task_b, issue_b);
        linear.set_issue_status(issue_a, "Todo");
        linear.set_issue_status(issue_b, "Todo");

        // Fail the first 3 move calls (exhausts all move_with_retry attempts for Task A).
        // Task B's MoveIssue call comes after and succeeds.
        linear.fail_next_n_moves(3);

        // Task A: MoveIssue (will fail, blocking) + PostComment (should be skipped)
        let a1 = make_effect(
            task_a,
            issue_a,
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress" }),
        );
        let a2 = make_effect(
            task_a,
            issue_a,
            EffectType::PostComment,
            serde_json::json!({ "body": "task A comment — should not post" }),
        );

        // Task B: MoveIssue (will succeed) + PostComment (will succeed)
        let b1 = make_effect(
            task_b,
            issue_b,
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress" }),
        );
        let b2 = make_effect(
            task_b,
            issue_b,
            EffectType::PostComment,
            serde_json::json!({ "body": "task B comment" }),
        );

        // Insert Task A effects first so they come first in the batch.
        db.insert_effects(&[a1, a2]).unwrap();
        db.insert_effects(&[b1, b2]).unwrap();

        let result = process_effects(&db, &linear, &cmd, &notifier).unwrap();

        // Task A: 1 failed (MoveIssue), 0 processed. Task B: 2 processed (Move + Comment).
        assert_eq!(result.failed, 1, "only Task A's MoveIssue should fail");
        assert_eq!(result.processed, 2, "Task B's two effects should succeed");

        // Task A's PostComment must NOT have been called.
        let comments = linear.comment_calls.borrow();
        assert_eq!(
            comments.len(),
            1,
            "only Task B's comment should have been posted"
        );
        assert!(
            comments[0].1.contains("task B comment"),
            "the posted comment should be from Task B"
        );
        drop(comments);

        // Task B: all blocking effects done → linear_pushed = true.
        let task_b_row = db.task(task_b).unwrap().unwrap();
        assert!(
            task_b_row.linear_pushed,
            "Task B should have linear_pushed=true after all effects done"
        );

        // Task A: blocking effect failed → linear_pushed = false.
        let task_a_row = db.task(task_a).unwrap().unwrap();
        assert!(
            !task_a_row.linear_pushed,
            "Task A should have linear_pushed=false because MoveIssue failed"
        );
    }

    // ─── RIG-353: CreatePr effect with worktree scenarios ─────────────────

    #[test]
    fn create_pr_effect_with_worktree_working_dir() {
        // RIG-351 pattern: engineer runs in worktree, working_dir points to .trees/feat--X
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // Simulate auto_create_pr flow from worktree path
        cmd.push_success("feat/rig-351-fix"); // git branch --show-current
        cmd.push_success("abc1234 RIG-351 feat: fix"); // git log
        cmd.push_success(""); // git push
        cmd.push_success(""); // gh pr view (no existing PR)
        cmd.push_success("https://github.com/org/repo/pull/88"); // gh pr create

        let effect = make_effect(
            "eff-wt-t",
            "EFF-WT",
            EffectType::CreatePr,
            // This is the key: working_dir is a worktree path, not base repo
            serde_json::json!({ "working_dir": "/Users/dev/projects/werma/.trees/feat--RIG-351" }),
        );

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        // Verify the working_dir was passed through to git commands
        let calls = cmd.calls.borrow();
        assert!(
            !calls.is_empty(),
            "commands should have been called for worktree PR creation"
        );
        // First call (git branch) should use the worktree dir
        let first_dir = &calls[0].2;
        assert!(
            first_dir
                .as_ref()
                .is_some_and(|d| d.contains(".trees/feat--RIG-351")),
            "git commands must use the worktree working_dir, got: {first_dir:?}"
        );
        drop(calls);

        // PR URL should be attached to Linear
        let attaches = linear.attach_calls.borrow();
        assert_eq!(attaches.len(), 1, "should attach PR URL after creation");
        assert!(attaches[0].1.contains("pull/88"));
    }

    #[test]
    fn create_pr_effect_empty_working_dir_returns_error() {
        // Empty working_dir in payload should cause payload_str to fail
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-empty-wd-t",
            "EFF-EMPTY-WD",
            EffectType::CreatePr,
            // Missing working_dir key entirely
            serde_json::json!({ "issue_id": "EFF-EMPTY-WD" }),
        );

        let result = execute_effect(&effect, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "CreatePr with missing working_dir should return Err"
        );
        assert!(
            result.unwrap_err().to_string().contains("working_dir"),
            "error should mention missing working_dir"
        );
    }

    #[test]
    fn process_effects_nonblocking_failure_continues() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let issue_id = "EFF-202";
        let task_id = "eff-202-t";

        insert_task(&db, task_id, issue_id);
        linear.set_issue_status(issue_id, "Todo");

        // Effect 1: non-blocking PostComment that will fail (missing 'body' key).
        // Use a unique dedup_key so it doesn't collide with e3 below.
        let mut e1 = Effect {
            dedup_key: format!("{task_id}:PostComment:fail"),
            ..make_effect(
                task_id,
                issue_id,
                EffectType::PostComment,
                serde_json::json!({ "WRONG_KEY": "will fail" }),
            )
        };
        e1.blocking = false; // non-blocking — failure must not halt the chain

        // Effect 2: blocking MoveIssue — should still execute despite e1 failing.
        let e2 = make_effect(
            task_id,
            issue_id,
            EffectType::MoveIssue,
            serde_json::json!({ "target_status": "in_progress" }),
        );

        // Effect 3: non-blocking PostComment — should still execute.
        let mut e3 = Effect {
            dedup_key: format!("{task_id}:PostComment:ok"),
            ..make_effect(
                task_id,
                issue_id,
                EffectType::PostComment,
                serde_json::json!({ "body": "continuation comment" }),
            )
        };
        e3.blocking = false;

        db.insert_effects(&[e1, e2, e3]).unwrap();

        let result = process_effects(&db, &linear, &cmd, &notifier).unwrap();

        assert_eq!(
            result.failed, 1,
            "only the non-blocking PostComment should fail"
        );
        assert_eq!(
            result.processed, 2,
            "MoveIssue and second PostComment should succeed"
        );

        // The MoveIssue (e2) must have been called.
        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1, "MoveIssue should have been executed");
        drop(moves);

        // The second PostComment (e3) must have been called.
        let comments = linear.comment_calls.borrow();
        assert_eq!(
            comments.len(),
            1,
            "the continuation PostComment should have been posted"
        );
        assert!(comments[0].1.contains("continuation comment"));
        drop(comments);

        // All blocking effects are done → linear_pushed = true.
        let task_row = db.task(task_id).unwrap().unwrap();
        assert!(
            task_row.linear_pushed,
            "blocking_effects_done=true so linear_pushed should be set"
        );
    }
}
