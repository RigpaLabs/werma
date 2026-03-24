use std::path::Path;

use anyhow::Result;

use crate::db::Db;
use crate::linear::LinearApi;
use crate::models::Status;
use crate::traits::Notifier;

use super::log_daemon;

/// How long (in seconds) a task can run before being flagged as stuck.
const STUCK_THRESHOLD_SECS: i64 = 7200; // 2 hours

/// Check running/pending pipeline tasks against Linear issue status.
///
/// Detects two conditions:
/// 1. Issue was **Canceled** in Linear → cancel the werma task.
/// 2. Issue was **moved to a different team** → cancel the werma task (it no longer belongs
///    to this pipeline instance).
///
/// Also flags tasks stuck running for more than `STUCK_THRESHOLD_SECS`.
pub fn check_canceled_and_stuck(
    db: &Db,
    werma_dir: &Path,
    linear: &dyn LinearApi,
    notifier: &dyn Notifier,
    expected_team_key: &str,
) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");

    // Gather all active pipeline tasks (pending + running) with a linear_issue_id.
    let active_tasks: Vec<_> = db
        .list_tasks(Some(Status::Running))?
        .into_iter()
        .chain(db.list_tasks(Some(Status::Pending))?)
        .filter(|t| !t.linear_issue_id.is_empty() && !t.pipeline_stage.is_empty())
        .collect();

    if active_tasks.is_empty() {
        return Ok(());
    }

    // Cache Linear query results — multiple tasks can reference the same issue.
    let mut issue_cache: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

    for task in &active_tasks {
        // Only query Linear once per issue; reuse cached result for duplicates.
        let (state_type, team_key) = if let Some(cached) = issue_cache.get(&task.linear_issue_id) {
            cached.clone()
        } else {
            match linear.get_issue_state_and_team(&task.linear_issue_id) {
                Ok(result) => {
                    issue_cache.insert(task.linear_issue_id.clone(), result.clone());
                    result
                }
                Err(e) => {
                    // API errors are transient — log and skip, don't cancel.
                    log_daemon(
                        &log_path,
                        &format!(
                            "cancel-check: failed to query {} for task {}: {e}",
                            task.linear_issue_id, task.id
                        ),
                    );
                    continue;
                }
            }
        };

        // Condition 1: Issue was canceled in Linear.
        if state_type == "canceled" || state_type == "cancelled" {
            cancel_task(
                db,
                &log_path,
                task,
                &format!("Linear issue {} was Canceled", task.linear_issue_id),
                notifier,
            );
            continue;
        }

        // Condition 2: Issue moved to a different team.
        if !expected_team_key.is_empty() && !team_key.is_empty() && team_key != expected_team_key {
            cancel_task(
                db,
                &log_path,
                task,
                &format!(
                    "Linear issue {} moved to team {} (expected {})",
                    task.linear_issue_id, team_key, expected_team_key
                ),
                notifier,
            );
            continue;
        }

        // Condition 3: Stuck detection — running tasks with no progress for >2h.
        if task.status == Status::Running {
            if let Some(ref started) = task.started_at {
                if let Ok(started_dt) =
                    chrono::NaiveDateTime::parse_from_str(started, "%Y-%m-%dT%H:%M:%S")
                {
                    let now = chrono::Local::now().naive_local();
                    let elapsed = now.signed_duration_since(started_dt);
                    if elapsed.num_seconds() > STUCK_THRESHOLD_SECS {
                        let hours = elapsed.num_seconds() as f64 / 3600.0;
                        let msg = format!(
                            "task {} stuck: running for {hours:.1}h (issue {})",
                            task.id, task.linear_issue_id
                        );
                        log_daemon(&log_path, &format!("STUCK: {msg}"));

                        let label = crate::notify::format_notify_label(
                            &task.id,
                            &task.task_type,
                            &task.linear_issue_id,
                        );
                        notifier.notify_macos("werma: stuck task", &msg, "Basso");
                        notifier.notify_slack(
                            "#werma-alerts",
                            &format!(
                                ":hourglass: *{label}* — running for {hours:.1}h, may be stuck"
                            ),
                        );

                        // Kill the tmux session and mark as failed.
                        let session_name = format!("werma-{}", task.id);
                        let _ = std::process::Command::new("tmux")
                            .args(["kill-session", "-t", &session_name])
                            .output();
                        let _ = db.set_task_status(&task.id, Status::Failed);
                        let now_str = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                        let _ = db.update_task_field(&task.id, "finished_at", &now_str);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Cancel a task: kill tmux session (if running), set status to Canceled, notify.
fn cancel_task(
    db: &Db,
    log_path: &Path,
    task: &crate::models::Task,
    reason: &str,
    notifier: &dyn Notifier,
) {
    log_daemon(log_path, &format!("CANCEL: {} — {reason}", task.id));

    // Kill tmux session if the task is running.
    if task.status == Status::Running {
        let session_name = format!("werma-{}", task.id);
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &session_name])
            .output();
    }

    let _ = db.set_task_status(&task.id, Status::Canceled);
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let _ = db.update_task_field(&task.id, "finished_at", &now);

    let label =
        crate::notify::format_notify_label(&task.id, &task.task_type, &task.linear_issue_id);
    notifier.notify_macos(
        "werma: task canceled",
        &format!("{label} — {reason}"),
        "Basso",
    );
    notifier.notify_slack(
        "#werma-alerts",
        &format!(":no_entry_sign: *{label}* — {reason}"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Task;
    use crate::traits::fakes::{FakeLinearApi, FakeNotifier};

    fn make_pipeline_task(id: &str, issue_id: &str, status: Status) -> Task {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        Task {
            id: id.to_string(),
            status,
            priority: 1,
            created_at: now.clone(),
            started_at: if status == Status::Running {
                Some(now)
            } else {
                None
            },
            finished_at: None,
            task_type: "pipeline-engineer".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: issue_id.to_string(),
            linear_pushed: false,
            pipeline_stage: "engineer".to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        }
    }

    #[test]
    fn cancels_task_when_issue_canceled() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_pipeline_task("20260324-001", "RIG-270", Status::Running);
        db.insert_task(&task).unwrap();

        let linear = FakeLinearApi::new();
        linear.set_issue_status("RIG-270", "canceled");

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &FakeNotifier::new(), "RIG")
            .unwrap();

        let updated = db.task("20260324-001").unwrap().unwrap();
        assert_eq!(updated.status, Status::Canceled);
        assert!(updated.finished_at.is_some());
    }

    #[test]
    fn cancels_task_when_issue_moved_to_different_team() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_pipeline_task("20260324-002", "RIG-270", Status::Running);
        db.insert_task(&task).unwrap();

        let linear = FakeLinearApi::new();
        // Issue moved to FAT team — state is active but team differs from expected "RIG".
        linear.set_issue_state_and_team("RIG-270", "started", "FAT");

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &FakeNotifier::new(), "RIG")
            .unwrap();

        let updated = db.task("20260324-002").unwrap().unwrap();
        assert_eq!(updated.status, Status::Canceled);
        assert!(updated.finished_at.is_some());
    }

    #[test]
    fn skips_non_pipeline_tasks() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let mut task = make_pipeline_task("20260324-003", "RIG-271", Status::Running);
        task.pipeline_stage = String::new(); // Not a pipeline task
        db.insert_task(&task).unwrap();

        let linear = FakeLinearApi::new();
        linear.set_issue_status("RIG-271", "canceled");

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &FakeNotifier::new(), "RIG")
            .unwrap();

        // Should NOT be canceled because it's not a pipeline task
        let updated = db.task("20260324-003").unwrap().unwrap();
        assert_eq!(updated.status, Status::Running);
    }

    #[test]
    fn cancels_pending_task_on_canceled_issue() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_pipeline_task("20260324-004", "RIG-272", Status::Pending);
        db.insert_task(&task).unwrap();

        let linear = FakeLinearApi::new();
        linear.set_issue_status("RIG-272", "canceled");

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &FakeNotifier::new(), "RIG")
            .unwrap();

        let updated = db.task("20260324-004").unwrap().unwrap();
        assert_eq!(updated.status, Status::Canceled);
    }

    #[test]
    fn no_active_tasks_is_ok() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let linear = FakeLinearApi::new();

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &FakeNotifier::new(), "RIG")
            .unwrap();
    }

    #[test]
    fn keeps_task_alive_when_issue_is_active() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_pipeline_task("20260324-005", "RIG-273", Status::Running);
        db.insert_task(&task).unwrap();

        let linear = FakeLinearApi::new();
        // Issue is still in progress, same team
        linear.set_issue_status("RIG-273", "in_progress");

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &FakeNotifier::new(), "RIG")
            .unwrap();

        let updated = db.task("20260324-005").unwrap().unwrap();
        assert_eq!(updated.status, Status::Running);
    }

    #[test]
    fn dedup_cancels_all_tasks_sharing_same_issue() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        // Two tasks referencing the same canceled issue.
        let task1 = make_pipeline_task("20260324-010", "RIG-275", Status::Running);
        let task2 = make_pipeline_task("20260324-011", "RIG-275", Status::Pending);
        db.insert_task(&task1).unwrap();
        db.insert_task(&task2).unwrap();

        let linear = FakeLinearApi::new();
        linear.set_issue_status("RIG-275", "canceled");

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &FakeNotifier::new(), "RIG")
            .unwrap();

        // Both tasks should be canceled — the cache must apply the result to both.
        let updated1 = db.task("20260324-010").unwrap().unwrap();
        let updated2 = db.task("20260324-011").unwrap().unwrap();
        assert_eq!(updated1.status, Status::Canceled);
        assert_eq!(updated2.status, Status::Canceled);
    }

    #[test]
    fn sends_notification_on_cancel() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_pipeline_task("20260324-006", "RIG-274", Status::Running);
        db.insert_task(&task).unwrap();

        let linear = FakeLinearApi::new();
        linear.set_issue_status("RIG-274", "canceled");

        let notifier = FakeNotifier::new();

        check_canceled_and_stuck(&db, werma_dir.path(), &linear, &notifier, "RIG").unwrap();

        assert_eq!(notifier.macos_calls.borrow().len(), 1);
        assert_eq!(notifier.slack_calls.borrow().len(), 1);
        assert!(notifier.slack_calls.borrow()[0].1.contains("Canceled"));
    }
}
