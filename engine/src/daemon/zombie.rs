use std::path::Path;

use anyhow::Result;

use crate::db::TaskRepository;
use crate::traits::Notifier;

use super::TmuxSession;
use super::log_daemon;

/// Detect zombie tasks: status is `running` but either:
/// 1. The tmux session has died entirely, OR
/// 2. The tmux session exists but the process inside has exited (silent crash).
///
/// Case 2 catches the bug where claude exits silently but tmux keeps the session
/// alive (e.g., due to `remain-on-exit` or process tree issues).
///
/// `notified_tasks` tracks task IDs that were recently notified, keyed to the time
/// of last notification. Notifications within `notification_cooldown_secs` are suppressed
/// to prevent duplicate macOS/Slack alerts for the same task across consecutive polls.
pub fn check_zombie_tasks(
    db: &dyn TaskRepository,
    werma_dir: &Path,
    tmux: &impl TmuxSession,
    notifier: &dyn Notifier,
    notified_tasks: &mut std::collections::HashMap<String, std::time::Instant>,
    notification_cooldown_secs: u64,
) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let running = db.list_tasks(Some(crate::models::Status::Running))?;

    for task in &running {
        let session_name = format!("werma-{}", task.id);

        if !tmux.has_session(&session_name) {
            // Case 1: session gone entirely
            mark_zombie(
                db,
                &log_path,
                task,
                "tmux session died unexpectedly",
                notifier,
                notified_tasks,
                notification_cooldown_secs,
            );
            continue;
        }

        // Activity detection: warn if worktree files haven't changed in >5min
        check_worktree_activity(task, werma_dir, notifier);

        // Case 2: session exists but process inside is dead
        if !tmux.is_pane_process_alive(&session_name) {
            // Capture pane content for diagnostics before killing
            let pane_content = tmux.capture_pane(&session_name, 20);
            let diag = match &pane_content {
                Some(text) => format!(" | last output: {}", truncate_for_log(text, 200)),
                None => " | pane empty (no output)".to_string(),
            };

            // Save diagnostic info to the task's log file
            let task_log = werma_dir.join("logs").join(format!("{}.log", task.id));
            let diag_entry = format!(
                "{}: ZOMBIE (dead process in live session){diag}\n",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S")
            );
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&task_log)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(diag_entry.as_bytes())
                })
            {
                log_daemon(
                    &log_path,
                    &format!(
                        "[ZOMBIE] failed to write diagnostic log for {}: {e}",
                        task.id
                    ),
                );
            }

            // Kill the orphaned tmux session
            if let Err(e) = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &session_name])
                .output()
            {
                log_daemon(
                    &log_path,
                    &format!("[ZOMBIE] failed to kill tmux session {session_name}: {e}"),
                );
            }

            mark_zombie(
                db,
                &log_path,
                task,
                &format!("process died in live tmux session{diag}"),
                notifier,
                notified_tasks,
                notification_cooldown_secs,
            );
        }
    }

    Ok(())
}

/// Mark a task as zombie (failed) and send notifications.
///
/// Notifications are suppressed if the task was already notified within
/// `notification_cooldown_secs` to prevent duplicate alerts across polls.
fn mark_zombie(
    db: &dyn TaskRepository,
    log_path: &Path,
    task: &crate::models::Task,
    reason: &str,
    notifier: &dyn Notifier,
    notified_tasks: &mut std::collections::HashMap<String, std::time::Instant>,
    notification_cooldown_secs: u64,
) {
    log_daemon(
        log_path,
        &format!("ZOMBIE detected: {} — {reason}", task.id),
    );

    if let Err(e) = db.set_task_status(&task.id, crate::models::Status::Failed) {
        log_daemon(
            log_path,
            &format!("[ZOMBIE] failed to set status for {}: {e}", task.id),
        );
    }
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    if let Err(e) = db.update_task_field(&task.id, "finished_at", &now) {
        log_daemon(
            log_path,
            &format!("[ZOMBIE] failed to set finished_at for {}: {e}", task.id),
        );
    }

    let within_cooldown = notified_tasks.get(&task.id).is_some_and(|last| {
        last.elapsed() < std::time::Duration::from_secs(notification_cooldown_secs)
    });

    if !within_cooldown {
        let label =
            crate::notify::format_notify_label(&task.id, &task.task_type, &task.linear_issue_id);
        notifier.notify_macos(
            "werma: zombie task detected",
            &format!("{label} — {reason}"),
            "Basso",
        );
        notifier.notify_slack("#werma-alerts", &format!(":zombie: *{label}* — {reason}"));
        notified_tasks.insert(task.id.clone(), std::time::Instant::now());
    }
}

/// Check worktree for recent file modifications and warn if idle >5 min.
fn check_worktree_activity(task: &crate::models::Task, werma_dir: &Path, _notifier: &dyn Notifier) {
    let working_dir = std::path::Path::new(&task.working_dir);
    if !working_dir.exists() {
        return;
    }

    // Use `find` with stat-based approach (macOS compatible)
    let output = std::process::Command::new("find")
        .args([
            ".", "-not", "-path", "./.git/*", "-type", "f", "-newer",
            // Compare against a reference: files modified in last 5 min
            ".",
        ])
        .current_dir(working_dir)
        .output();

    // Simpler approach: check git status for any changes
    let has_recent_changes = std::process::Command::new("git")
        .args(["diff", "--stat", "HEAD"])
        .current_dir(working_dir)
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    // Check started_at to compute elapsed time
    let idle_minutes = task
        .started_at
        .as_deref()
        .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok())
        .map(|started| {
            let now = chrono::Local::now().naive_local();
            (now - started).num_minutes()
        });

    // Only flag if running >5 min and no git changes detected
    if let Some(mins) = idle_minutes {
        if mins > 5 && !has_recent_changes {
            let log_path = werma_dir.join("logs/daemon.log");
            log_daemon(
                &log_path,
                &format!(
                    "[ACTIVITY] {} running {}m with no worktree changes — may be idle",
                    task.id, mins
                ),
            );

            // Don't spam notifications — only log. The zombie detector handles
            // truly dead sessions. This is informational.
            let _ = output; // suppress unused warning
        }
    }
}

/// Truncate a string for log output, replacing newlines with ` | `.
fn truncate_for_log(s: &str, max_len: usize) -> String {
    let flat = s.replace('\n', " | ");
    if flat.chars().count() > max_len {
        let truncated: String = flat.chars().take(max_len).collect();
        format!("{truncated}...")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Status, Task};
    use crate::traits::fakes::FakeNotifier;

    struct FakeTmux {
        alive_sessions: Vec<String>,
        /// Sessions where tmux exists but the process inside is dead.
        dead_process_sessions: Vec<String>,
    }

    impl FakeTmux {
        fn new(alive: Vec<String>) -> Self {
            Self {
                alive_sessions: alive,
                dead_process_sessions: vec![],
            }
        }

        fn with_dead_process(mut self, sessions: Vec<String>) -> Self {
            self.dead_process_sessions = sessions;
            self
        }
    }

    impl super::super::TmuxSession for FakeTmux {
        fn has_session(&self, name: &str) -> bool {
            self.alive_sessions.iter().any(|s| s == name)
                || self.dead_process_sessions.iter().any(|s| s == name)
        }

        fn count_werma_sessions(&self) -> usize {
            self.alive_sessions.len() + self.dead_process_sessions.len()
        }

        fn is_pane_process_alive(&self, name: &str) -> bool {
            // Only truly alive if in alive_sessions (not dead_process_sessions)
            self.alive_sessions.iter().any(|s| s == name)
        }

        fn capture_pane(&self, _name: &str, _lines: u32) -> Option<String> {
            None
        }
    }

    fn make_running_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            status: Status::Running,
            priority: 1,
            created_at: "2026-03-13T10:00:00".to_string(),
            started_at: Some("2026-03-13T10:00:00".to_string()),
            finished_at: None,
            task_type: "pipeline-engineer".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "RIG-210".to_string(),
            linear_pushed: false,
            pipeline_stage: "engineer".to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
        }
    }

    #[test]
    fn marks_dead_sessions_as_failed() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260313-999");
        db.insert_task(&task).unwrap();

        // No alive sessions — zombie detected
        let tmux = FakeTmux::new(vec![]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        let updated = db.task("20260313-999").unwrap().unwrap();
        assert_eq!(updated.status, Status::Failed);
        assert!(updated.finished_at.is_some());
    }

    #[test]
    fn ignores_non_running() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = Task {
            id: "20260313-998".to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-13T10:00:00".to_string(),
            started_at: Some("2026-03-13T10:00:00".to_string()),
            finished_at: Some("2026-03-13T10:05:00".to_string()),
            task_type: "research".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
        };
        db.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        let updated = db.task("20260313-998").unwrap().unwrap();
        assert_eq!(updated.status, Status::Completed);
    }

    #[test]
    fn skips_alive_sessions() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260313-997");
        db.insert_task(&task).unwrap();

        // Session is alive with process running — should not be marked as zombie
        let tmux = FakeTmux::new(vec!["werma-20260313-997".to_string()]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        let updated = db.task("20260313-997").unwrap().unwrap();
        assert_eq!(updated.status, Status::Running);
        assert!(updated.finished_at.is_none());
    }

    #[test]
    fn detects_dead_process_in_live_session() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260313-995");
        db.insert_task(&task).unwrap();

        // Session exists but process inside is dead (the core bug scenario)
        let tmux = FakeTmux::new(vec![]).with_dead_process(vec!["werma-20260313-995".to_string()]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        let updated = db.task("20260313-995").unwrap().unwrap();
        assert_eq!(updated.status, Status::Failed);
        assert!(updated.finished_at.is_some());
    }

    #[test]
    fn multiple_tasks_mixed_states() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let alive_task = make_running_task("20260313-001");
        let dead_task = make_running_task("20260313-002");
        db.insert_task(&alive_task).unwrap();
        db.insert_task(&dead_task).unwrap();

        let tmux = FakeTmux::new(vec!["werma-20260313-001".to_string()]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        // 001 should still be running
        let t1 = db.task("20260313-001").unwrap().unwrap();
        assert_eq!(t1.status, Status::Running);

        // 002 should be marked failed (zombie)
        let t2 = db.task("20260313-002").unwrap().unwrap();
        assert_eq!(t2.status, Status::Failed);
        assert!(t2.finished_at.is_some());
    }

    #[test]
    fn no_running_tasks_is_ok() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let tmux = FakeTmux::new(vec![]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();
    }

    #[test]
    fn zombie_sets_finished_at_timestamp() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260313-996");
        db.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        let updated = db.task("20260313-996").unwrap().unwrap();
        let finished = updated.finished_at.unwrap();
        // Should be a valid timestamp format
        assert!(finished.contains("T"));
        assert!(finished.len() >= 19); // "2026-03-13T10:00:00"
    }

    #[test]
    fn mixed_alive_dead_process_and_dead_session() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        // 3 tasks: alive, dead process in live session, dead session
        let t1 = make_running_task("20260313-010"); // alive
        let t2 = make_running_task("20260313-011"); // dead process, live session
        let t3 = make_running_task("20260313-012"); // dead session
        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();
        db.insert_task(&t3).unwrap();

        let tmux = FakeTmux::new(vec!["werma-20260313-010".to_string()])
            .with_dead_process(vec!["werma-20260313-011".to_string()]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        assert_eq!(
            db.task("20260313-010").unwrap().unwrap().status,
            Status::Running
        );
        assert_eq!(
            db.task("20260313-011").unwrap().unwrap().status,
            Status::Failed
        );
        assert_eq!(
            db.task("20260313-012").unwrap().unwrap().status,
            Status::Failed
        );
    }

    #[test]
    fn dead_process_writes_diagnostic_log() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260313-994");
        db.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]).with_dead_process(vec!["werma-20260313-994".to_string()]);

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &db,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        // Check that a diagnostic entry was written to the task log
        let task_log = werma_dir.path().join("logs/20260313-994.log");
        let content = std::fs::read_to_string(&task_log).unwrap();
        assert!(content.contains("ZOMBIE (dead process in live session)"));
    }

    // ─── Tests using FakeTaskRepo (no SQLite) ────────────────────────────

    use crate::db::fakes::FakeTaskRepo;

    #[test]
    fn fake_repo_marks_dead_sessions_as_failed() {
        let repo = FakeTaskRepo::new();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260325-001");
        repo.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]);
        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &repo,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        let updated = repo.task("20260325-001").unwrap().unwrap();
        assert_eq!(updated.status, Status::Failed);
        assert!(updated.finished_at.is_some());
    }

    #[test]
    fn fake_repo_skips_alive_sessions() {
        let repo = FakeTaskRepo::new();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260325-002");
        repo.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec!["werma-20260325-002".to_string()]);
        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &repo,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        let updated = repo.task("20260325-002").unwrap().unwrap();
        assert_eq!(updated.status, Status::Running);
    }

    #[test]
    fn fake_repo_mixed_alive_and_dead() {
        let repo = FakeTaskRepo::new();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let alive = make_running_task("20260325-010");
        let dead = make_running_task("20260325-011");
        repo.insert_task(&alive).unwrap();
        repo.insert_task(&dead).unwrap();

        let tmux = FakeTmux::new(vec!["werma-20260325-010".to_string()]);
        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(
            &repo,
            werma_dir.path(),
            &tmux,
            &FakeNotifier::new(),
            &mut nt,
            300,
        )
        .unwrap();

        assert_eq!(
            repo.task("20260325-010").unwrap().unwrap().status,
            Status::Running
        );
        assert_eq!(
            repo.task("20260325-011").unwrap().unwrap().status,
            Status::Failed
        );
    }

    // ─── Dedup / cooldown tests ───────────────────────────────────────────

    #[test]
    fn notification_suppressed_within_cooldown() {
        // Pre-populate notified_tasks with the task ID to simulate a recent notification.
        // The DB work (mark as Failed) must still happen, but the notification must be skipped.
        let repo = FakeTaskRepo::new();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260380-dup1");
        repo.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]); // session gone → zombie
        let notifier = FakeNotifier::new();

        let mut nt = std::collections::HashMap::new();
        // Simulate: this task was notified just now (within 300s cooldown)
        nt.insert("20260380-dup1".to_string(), std::time::Instant::now());

        check_zombie_tasks(&repo, werma_dir.path(), &tmux, &notifier, &mut nt, 300).unwrap();

        // DB work still happened
        let updated = repo.task("20260380-dup1").unwrap().unwrap();
        assert_eq!(updated.status, Status::Failed);

        // Notification suppressed — duplicate prevented
        assert!(
            notifier.macos_calls.borrow().is_empty(),
            "notification must be suppressed within cooldown"
        );
    }

    #[test]
    fn notification_fires_after_cooldown_expires() {
        // cooldown_secs = 0 means any elapsed duration satisfies the condition.
        let repo = FakeTaskRepo::new();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260380-dup2");
        repo.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]);
        let notifier = FakeNotifier::new();

        let mut nt = std::collections::HashMap::new();
        // Simulate prior notification, but with 0-second cooldown → always fires again
        nt.insert("20260380-dup2".to_string(), std::time::Instant::now());

        check_zombie_tasks(&repo, werma_dir.path(), &tmux, &notifier, &mut nt, 0).unwrap();

        assert_eq!(
            notifier.macos_calls.borrow().len(),
            1,
            "notification must fire when cooldown is 0"
        );
    }

    #[test]
    fn first_notification_always_fires() {
        // Empty notified_tasks → no cooldown history → notification must fire.
        let repo = FakeTaskRepo::new();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260380-dup3");
        repo.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]);
        let notifier = FakeNotifier::new();

        let mut nt = std::collections::HashMap::new();
        check_zombie_tasks(&repo, werma_dir.path(), &tmux, &notifier, &mut nt, 300).unwrap();

        assert_eq!(
            notifier.macos_calls.borrow().len(),
            1,
            "first notification must always fire"
        );
        // Map should be updated with the task ID
        assert!(
            nt.contains_key("20260380-dup3"),
            "notified_tasks must be updated after notification"
        );
    }
}
