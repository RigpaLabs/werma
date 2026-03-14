use std::path::Path;

use anyhow::Result;

use crate::db::Db;

use super::TmuxSession;
use super::log_daemon;

/// Detect zombie tasks: status is `running` but either:
/// 1. The tmux session has died entirely, OR
/// 2. The tmux session exists but the process inside has exited (silent crash).
///
/// Case 2 catches the bug where claude exits silently but tmux keeps the session
/// alive (e.g., due to `remain-on-exit` or process tree issues).
pub fn check_zombie_tasks(db: &Db, werma_dir: &Path, tmux: &impl TmuxSession) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let running = db.list_tasks(Some(crate::models::Status::Running))?;

    for task in &running {
        let session_name = format!("werma-{}", task.id);

        if !tmux.has_session(&session_name) {
            // Case 1: session gone entirely
            mark_zombie(
                db,
                &log_path,
                werma_dir,
                task,
                "tmux session died unexpectedly",
            );
            continue;
        }

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
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&task_log)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(diag_entry.as_bytes())
                });

            // Kill the orphaned tmux session
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &session_name])
                .output();

            mark_zombie(
                db,
                &log_path,
                werma_dir,
                task,
                &format!("process died in live tmux session{diag}"),
            );
        }
    }

    Ok(())
}

/// Mark a task as zombie (failed) and send notifications.
fn mark_zombie(
    db: &Db,
    log_path: &Path,
    _werma_dir: &Path,
    task: &crate::models::Task,
    reason: &str,
) {
    log_daemon(
        log_path,
        &format!("ZOMBIE detected: {} — {reason}", task.id),
    );

    let _ = db.set_task_status(&task.id, crate::models::Status::Failed);
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let _ = db.update_task_field(&task.id, "finished_at", &now);

    let label =
        crate::notify::format_notify_label(&task.id, &task.task_type, &task.linear_issue_id);
    crate::notify::notify_macos(
        "werma: zombie task detected",
        &format!("{label} — {reason}"),
        "Basso",
    );
    crate::notify::notify_slack("#werma-alerts", &format!(":zombie: *{label}* — {reason}"));
}

/// Truncate a string for log output, replacing newlines with ` | `.
fn truncate_for_log(s: &str, max_len: usize) -> String {
    let flat = s.replace('\n', " | ");
    if flat.len() > max_len {
        format!("{}...", &flat[..max_len])
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Status, Task};

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

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

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
        };
        db.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]);

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

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

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

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

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

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

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

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

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();
    }

    #[test]
    fn zombie_sets_finished_at_timestamp() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260313-996");
        db.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(vec![]);

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

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

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

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

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

        // Check that a diagnostic entry was written to the task log
        let task_log = werma_dir.path().join("logs/20260313-994.log");
        let content = std::fs::read_to_string(&task_log).unwrap();
        assert!(content.contains("ZOMBIE (dead process in live session)"));
    }
}
