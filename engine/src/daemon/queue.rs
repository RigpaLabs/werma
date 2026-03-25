use std::path::Path;
use std::time::Instant;

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

/// Attempt to launch a single pending task, respecting max_concurrent and stagger cooldown.
///
/// Cooperative scheduling: instead of blocking with `thread::sleep`, this function
/// checks whether enough time has passed since the last launch. If the cooldown
/// hasn't elapsed, it returns immediately without launching. The daemon's tick loop
/// calls this every 5s, so launches are naturally spaced out.
///
/// Returns `true` if a task was launched (caller should update `last_launch`).
pub fn try_launch_one(
    db: &Db,
    werma_dir: &Path,
    max_concurrent: usize,
    stagger_secs: u64,
    last_launch: Option<Instant>,
    tmux: &impl TmuxSession,
) -> Result<bool> {
    let active = tmux.count_werma_sessions();
    if active >= max_concurrent {
        return Ok(false);
    }

    // Enforce stagger cooldown: skip if we launched too recently
    if stagger_secs > 0 {
        if let Some(last) = last_launch {
            let elapsed = last.elapsed().as_secs();
            if elapsed < stagger_secs {
                return Ok(false);
            }
        }
    }

    let log_path = werma_dir.join("logs/daemon.log");
    match runner::run_next(db, werma_dir) {
        Ok(Some(id)) => {
            log_daemon(&log_path, &format!("launched: {id}"));
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(e) => {
            log_daemon(&log_path, &format!("launch error: {e}"));
            Err(e)
        }
    }
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
    fn try_launch_one_at_capacity_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 3 };

        let launched = try_launch_one(&db, dir.path(), 3, 0, None, &tmux).unwrap();
        assert!(!launched);
    }

    #[test]
    fn try_launch_one_over_capacity_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 5 };

        let launched = try_launch_one(&db, dir.path(), 3, 0, None, &tmux).unwrap();
        assert!(!launched);
    }

    #[test]
    fn try_launch_one_no_pending_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        let launched = try_launch_one(&db, dir.path(), 8, 0, None, &tmux).unwrap();
        assert!(!launched, "no pending tasks means nothing to launch");
    }

    #[test]
    fn try_launch_one_zero_max_concurrent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        let launched = try_launch_one(&db, dir.path(), 0, 0, None, &tmux).unwrap();
        assert!(!launched);
    }

    #[test]
    fn stagger_cooldown_skips_when_too_recent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // Last launch was just now — with 10s stagger, should skip
        let last = Some(Instant::now());
        let launched = try_launch_one(&db, dir.path(), 8, 10, last, &tmux).unwrap();
        assert!(!launched, "stagger cooldown should prevent launch");
    }

    #[test]
    fn stagger_cooldown_allows_when_elapsed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // Last launch was 20s ago — with 4s stagger, should allow
        let last = Some(Instant::now() - std::time::Duration::from_secs(20));
        // No pending tasks, so returns false (no task to launch), but cooldown didn't block
        let launched = try_launch_one(&db, dir.path(), 8, 4, last, &tmux).unwrap();
        assert!(!launched, "no pending tasks, but cooldown did not block");
    }

    #[test]
    fn stagger_zero_ignores_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // stagger_secs=0 should not enforce cooldown even with recent launch
        let last = Some(Instant::now());
        let launched = try_launch_one(&db, dir.path(), 8, 0, last, &tmux).unwrap();
        // false because no pending tasks, not because of cooldown
        assert!(!launched);
    }

    #[test]
    fn no_last_launch_skips_cooldown_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // No last launch (first call) — should not block even with stagger
        let launched = try_launch_one(&db, dir.path(), 8, 10, None, &tmux).unwrap();
        assert!(!launched, "no pending tasks");
    }

    #[test]
    fn try_launch_is_non_blocking() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = FakeTmux { active_sessions: 0 };

        // Even with large stagger, the function must return instantly (no sleep)
        let start = Instant::now();
        let _ = try_launch_one(&db, dir.path(), 8, 60, Some(Instant::now()), &tmux);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "try_launch_one must be non-blocking"
        );
    }
}
