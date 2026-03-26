use std::collections::HashMap;

use anyhow::Result;

use super::callback::move_with_retry;
use super::pr::{auto_create_pr, post_pr_comment};
use crate::db::Db;
use crate::linear::LinearApi;
use crate::models::{Effect, EffectType};
use crate::traits::{CommandRunner, Notifier};

/// Result of a single `process_effects()` call.
// Used by Task 5 daemon integration; allow dead_code until daemon/mod.rs is updated.
#[allow(dead_code)]
pub struct ProcessResult {
    pub processed: usize,
    pub failed: usize,
}

/// Drain the effects outbox: fetch pending effects, execute them, mark done/failed.
// Used by Task 5 daemon integration; allow dead_code until daemon/mod.rs is updated.
#[allow(dead_code)]
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
            match execute_effect(effect, linear, cmd, notifier) {
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
// Used by process_effects and tests; allow dead_code in non-test builds until Task 5.
#[allow(dead_code)]
pub fn execute_effect(
    effect: &Effect,
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
            let working_dir = payload_str(payload, "working_dir")?;
            // Returns Option<String> — we log but don't fail if no PR created.
            if let Some(url) = auto_create_pr(cmd, working_dir, issue_id, task_id)? {
                eprintln!("[effects] CreatePr: created PR {url} for {issue_id}");
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
            let working_dir = payload
                .get("working_dir")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp");
            // Returns bool — we log but don't fail if no PR found.
            if !post_pr_comment(cmd, working_dir, body)? {
                eprintln!(
                    "[effects] PostPrComment: no PR found in {working_dir}, skipping comment"
                );
            }
            Ok(())
        }

        EffectType::Notify => {
            let channel = payload_str(payload, "channel")?;
            let msg = payload_str(payload, "message")?;
            notifier.notify_slack(channel, msg);
            Ok(())
        }
    }
}

/// Extract a string field from an effect payload, returning `Err` if absent or not a string.
#[allow(dead_code)]
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

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].1, "in_progress");
    }

    #[test]
    fn execute_effect_post_comment() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-pc-t",
            "EFF-PC",
            EffectType::PostComment,
            serde_json::json!({ "body": "Hello from processor" }),
        );

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        let comments = linear.comment_calls.borrow();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].1.contains("Hello from processor"));
    }

    #[test]
    fn execute_effect_add_label() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-al-t",
            "EFF-AL",
            EffectType::AddLabel,
            serde_json::json!({ "label": "spec:done" }),
        );

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        let labels = linear.add_label_calls.borrow();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].1, "spec:done");
    }

    #[test]
    fn execute_effect_remove_label() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-rl-t",
            "EFF-RL",
            EffectType::RemoveLabel,
            serde_json::json!({ "label": "analyze" }),
        );

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        let labels = linear.remove_label_calls.borrow();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].1, "analyze");
    }

    #[test]
    fn execute_effect_update_estimate() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-ue-t",
            "EFF-UE",
            EffectType::UpdateEstimate,
            serde_json::json!({ "estimate": 5 }),
        );

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        let estimates = linear.estimate_calls.borrow();
        assert_eq!(estimates.len(), 1);
        assert_eq!(estimates[0].1, 5);
    }

    #[test]
    fn execute_effect_attach_url() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-au-t",
            "EFF-AU",
            EffectType::AttachUrl,
            serde_json::json!({ "url": "https://github.com/org/repo/pull/99", "title": "PR #99" }),
        );

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        let attaches = linear.attach_calls.borrow();
        assert_eq!(attaches.len(), 1);
        assert!(attaches[0].1.contains("pull/99"));
    }

    #[test]
    fn execute_effect_create_pr_skips_gracefully() {
        // FakeCommandRunner returns empty stdout for git commands, so auto_create_pr
        // will detect empty branch name and return None (graceful skip, not error).
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-cp-t",
            "EFF-CP",
            EffectType::CreatePr,
            serde_json::json!({ "working_dir": "/tmp" }),
        );

        // Should succeed even when no PR is created (graceful skip)
        let result = execute_effect(&effect, &linear, &cmd, &notifier);
        assert!(
            result.is_ok(),
            "CreatePr should not fail on graceful skip: {result:?}"
        );
    }

    #[test]
    fn execute_effect_post_pr_comment_no_pr_skips_gracefully() {
        // FakeCommandRunner returns empty stdout, so post_pr_comment returns Ok(false).
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-ppc-t",
            "EFF-PPC",
            EffectType::PostPrComment,
            serde_json::json!({ "body": "Review posted.", "working_dir": "/tmp" }),
        );

        let result = execute_effect(&effect, &linear, &cmd, &notifier);
        assert!(
            result.is_ok(),
            "PostPrComment with no PR should not fail: {result:?}"
        );
    }

    #[test]
    fn execute_effect_notify() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        let effect = make_effect(
            "eff-ntf-t",
            "EFF-NTF",
            EffectType::Notify,
            serde_json::json!({ "channel": "#alerts", "message": "Task done" }),
        );

        execute_effect(&effect, &linear, &cmd, &notifier).unwrap();

        let notifications = notifier.slack_calls.borrow();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].0, "#alerts");
        assert!(notifications[0].1.contains("Task done"));
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
}
