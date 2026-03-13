use std::path::Path;

use anyhow::Result;

use crate::db::Db;
use crate::runner;

use super::TmuxSession;
use super::log_daemon;

/// Drain pending tasks into tmux sessions, respecting pipeline max_concurrent.
pub fn drain_queue(
    db: &Db,
    werma_dir: &Path,
    max_concurrent: usize,
    tmux: &impl TmuxSession,
) -> Result<()> {
    let active = tmux.count_werma_sessions();
    if active >= max_concurrent {
        return Ok(());
    }

    let slots = max_concurrent - active;
    for _ in 0..slots {
        match runner::run_next(db, werma_dir) {
            Ok(Some(id)) => {
                let log_path = werma_dir.join("logs/daemon.log");
                log_daemon(&log_path, &format!("launched: {id}"));
            }
            Ok(None) => break,
            Err(e) => {
                let log_path = werma_dir.join("logs/daemon.log");
                log_daemon(&log_path, &format!("launch error: {e}"));
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTmux {
        active_sessions: usize,
    }

    impl TmuxSession for FakeTmux {
        fn has_session(&self, _name: &str) -> bool {
            false
        }

        fn count_werma_sessions(&self) -> usize {
            self.active_sessions
        }
    }

    #[test]
    fn drain_queue_no_tasks_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        drain_queue(&db, dir.path(), 3, &tmux).unwrap();
    }

    #[test]
    fn drain_queue_at_capacity_skips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 3 };

        // At max_concurrent=3 with 3 active — should do nothing
        drain_queue(&db, dir.path(), 3, &tmux).unwrap();
    }

    #[test]
    fn drain_queue_over_capacity_skips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 5 };

        // Over capacity — should do nothing
        drain_queue(&db, dir.path(), 3, &tmux).unwrap();
    }

    #[test]
    fn drain_queue_with_available_slots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 1 };

        // 1 active, max 3 — should try to launch (but no pending tasks)
        drain_queue(&db, dir.path(), 3, &tmux).unwrap();
    }

    #[test]
    fn drain_queue_zero_max_concurrent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // Zero max = no slots
        drain_queue(&db, dir.path(), 0, &tmux).unwrap();
    }
}
