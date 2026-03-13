use std::path::Path;

use anyhow::Result;

use crate::db::Db;

use super::TmuxSession;
use super::log_daemon;

/// Detect zombie tasks: status is `running` but the tmux session has died.
/// Marks them as failed and sends notifications.
pub fn check_zombie_tasks(db: &Db, werma_dir: &Path, tmux: &impl TmuxSession) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let running = db.list_tasks(Some(crate::models::Status::Running))?;

    for task in &running {
        let session_name = format!("werma-{}", task.id);
        if tmux.has_session(&session_name) {
            continue;
        }

        // Session is dead but task is still running — zombie detected.
        let reason = "tmux session died unexpectedly";
        log_daemon(
            &log_path,
            &format!("ZOMBIE detected: {} — {reason}", task.id),
        );

        db.set_task_status(&task.id, crate::models::Status::Failed)?;
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        db.update_task_field(&task.id, "finished_at", &now)?;

        let label =
            crate::notify::format_notify_label(&task.id, &task.task_type, &task.linear_issue_id);
        crate::notify::notify_macos(
            "werma: zombie task detected",
            &format!("{label} — {reason}"),
            "Basso",
        );
        crate::notify::notify_slack("#werma-alerts", &format!(":zombie: *{label}* — {reason}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Status, Task};

    struct FakeTmux {
        alive_sessions: Vec<String>,
    }

    impl super::super::TmuxSession for FakeTmux {
        fn has_session(&self, name: &str) -> bool {
            self.alive_sessions.iter().any(|s| s == name)
        }

        fn count_werma_sessions(&self) -> usize {
            self.alive_sessions.len()
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
        let tmux = FakeTmux {
            alive_sessions: vec![],
        };

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

        let tmux = FakeTmux {
            alive_sessions: vec![],
        };

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

        // Session is alive — should not be marked as zombie
        let tmux = FakeTmux {
            alive_sessions: vec!["werma-20260313-997".to_string()],
        };

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

        let updated = db.task("20260313-997").unwrap().unwrap();
        assert_eq!(updated.status, Status::Running);
        assert!(updated.finished_at.is_none());
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

        let tmux = FakeTmux {
            alive_sessions: vec!["werma-20260313-001".to_string()],
        };

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

        let tmux = FakeTmux {
            alive_sessions: vec![],
        };

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();
    }

    #[test]
    fn zombie_sets_finished_at_timestamp() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260313-996");
        db.insert_task(&task).unwrap();

        let tmux = FakeTmux {
            alive_sessions: vec![],
        };

        check_zombie_tasks(&db, werma_dir.path(), &tmux).unwrap();

        let updated = db.task("20260313-996").unwrap().unwrap();
        let finished = updated.finished_at.unwrap();
        // Should be a valid timestamp format
        assert!(finished.contains("T"));
        assert!(finished.len() >= 19); // "2026-03-13T10:00:00"
    }
}
