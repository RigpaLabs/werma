use std::path::Path;

use anyhow::Result;

use crate::db::Db;
use crate::runner;

use super::TmuxSession;
use super::log_daemon;

/// Real tmux implementation via `std::process::Command`.
pub struct RealTmux;

impl TmuxSession for RealTmux {
    fn has_session(&self, name: &str) -> bool {
        let result = std::process::Command::new("tmux")
            .args(["has-session", "-t", name])
            .output();
        matches!(result, Ok(out) if out.status.success())
    }

    fn count_werma_sessions(&self) -> usize {
        let output = std::process::Command::new("tmux").args(["ls"]).output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout.lines().filter(|l| l.starts_with("werma-")).count()
            }
            Err(_) => 0,
        }
    }

    fn is_pane_process_alive(&self, name: &str) -> bool {
        // Get the PID of the process running in the tmux pane
        let result = std::process::Command::new("tmux")
            .args(["list-panes", "-t", name, "-F", "#{pane_pid}"])
            .output();

        match result {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let pid_str = stdout.trim();
                if pid_str.is_empty() {
                    return false;
                }
                // Use `ps -p <pid>` to check if process is still running
                let ps_result = std::process::Command::new("ps")
                    .args(["-p", pid_str])
                    .output();
                matches!(ps_result, Ok(ps_out) if ps_out.status.success())
            }
            _ => false,
        }
    }

    fn capture_pane(&self, name: &str, lines: u32) -> Option<String> {
        let start = format!("-{lines}");
        let result = std::process::Command::new("tmux")
            .args(["capture-pane", "-t", name, "-p", "-S", &start])
            .output();

        match result {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if text.is_empty() { None } else { Some(text) }
            }
            _ => None,
        }
    }
}

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

    let log_path = werma_dir.join("logs/daemon.log");
    let slots = max_concurrent - active;
    for _ in 0..slots {
        match runner::run_next(db, werma_dir) {
            Ok(Some(id)) => {
                log_daemon(&log_path, &format!("launched: {id}"));
            }
            Ok(None) => break,
            Err(e) => {
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

        fn is_pane_process_alive(&self, _name: &str) -> bool {
            false
        }

        fn capture_pane(&self, _name: &str, _lines: u32) -> Option<String> {
            None
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
