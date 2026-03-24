use std::path::Path;
use std::time::{Duration, Instant};

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
/// Convenience wrapper with no stagger (used in tests and non-daemon callers).
#[allow(dead_code)]
pub fn drain_queue(
    db: &Db,
    werma_dir: &Path,
    max_concurrent: usize,
    tmux: &impl TmuxSession,
) -> Result<()> {
    drain_queue_with_stagger(db, werma_dir, max_concurrent, tmux, 0, &mut None)
}

/// Drain pending tasks into tmux sessions with stagger delay between launches.
///
/// Unlike a blocking sleep loop, this function checks `last_launch_at` and only
/// launches a task if enough time has passed since the last launch. When the
/// stagger delay hasn't elapsed, it returns immediately — the daemon tick loop
/// will call again on the next tick. This prevents blocking the entire daemon
/// (zombie detection, pipeline polling, cron, cancel checks) during stagger waits.
pub fn drain_queue_with_stagger(
    db: &Db,
    werma_dir: &Path,
    max_concurrent: usize,
    tmux: &impl TmuxSession,
    stagger_secs: u64,
    last_launch_at: &mut Option<Instant>,
) -> Result<()> {
    let active = tmux.count_werma_sessions();
    if active >= max_concurrent {
        return Ok(());
    }

    let log_path = werma_dir.join("logs/daemon.log");
    let slots = max_concurrent - active;
    for _ in 0..slots {
        // Stagger: only launch if enough time has passed since last launch.
        // Returns immediately instead of blocking — next tick will retry.
        if stagger_secs > 0 {
            if let Some(last) = last_launch_at {
                if last.elapsed() < Duration::from_secs(stagger_secs) {
                    break;
                }
            }
        }
        match runner::run_next(db, werma_dir) {
            Ok(Some(id)) => {
                log_daemon(&log_path, &format!("launched: {id}"));
                *last_launch_at = Some(Instant::now());
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

    #[test]
    fn stagger_skips_when_recent_launch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // Simulate a recent launch — stagger should prevent launching
        let mut last_launch = Some(Instant::now());
        drain_queue_with_stagger(&db, dir.path(), 3, &tmux, 10, &mut last_launch).unwrap();
        // last_launch should remain unchanged (no new launch)
        assert!(last_launch.unwrap().elapsed().as_secs() < 2);
    }

    #[test]
    fn stagger_zero_allows_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // stagger=0 should not block (no pending tasks, but function runs)
        let mut last_launch = None;
        drain_queue_with_stagger(&db, dir.path(), 3, &tmux, 0, &mut last_launch).unwrap();
    }
}
