use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::db::Db;
use crate::traits::Notifier;

use super::log_daemon;

const MAX_LOG_SIZE_BYTES: u64 = 5 * 1024 * 1024;

/// Rotate log files larger than MAX_LOG_SIZE_BYTES.
pub fn rotate_logs(werma_dir: &Path) -> Result<()> {
    let logs_dir = werma_dir.join("logs");
    if !logs_dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(&logs_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "log")
            && let Ok(meta) = entry.metadata()
            && meta.len() > MAX_LOG_SIZE_BYTES
        {
            // Truncate to last 1000 lines.
            if let Ok(content) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = content.lines().collect();
                let keep = if lines.len() > 1000 {
                    &lines[lines.len() - 1000..]
                } else {
                    &lines
                };
                let _ = std::fs::write(&path, keep.join("\n"));
            }
        }
    }

    Ok(())
}

/// Check that main branch checkouts are clean (no staged/unstaged changes).
/// Collects unique repo root dirs from running write tasks and checks `git status`.
/// Uses per-repo cooldown to avoid spamming notifications every tick.
pub fn check_main_branch_cleanliness(
    db: &Db,
    log_path: &Path,
    notified: &mut HashMap<PathBuf, Instant>,
    cooldown_secs: u64,
    notifier: &dyn Notifier,
) -> Result<()> {
    let running = db.list_tasks(Some(crate::models::Status::Running))?;

    let mut checked = std::collections::HashSet::new();
    for task in &running {
        if !crate::worktree::needs_worktree(&task.task_type) {
            continue;
        }

        let working_dir = crate::runner::resolve_home(&task.working_dir);
        let working_dir_str = working_dir.to_string_lossy();
        let repo_dir = if let Some(trees_pos) = working_dir_str.find("/.trees/") {
            PathBuf::from(&working_dir_str[..trees_pos])
        } else {
            working_dir
        };

        if !checked.insert(repo_dir.clone()) {
            continue;
        }

        if let Some(last) = notified.get(&repo_dir)
            && last.elapsed() < Duration::from_secs(cooldown_secs)
        {
            continue;
        }

        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo_dir)
            .output();

        if let Ok(out) = output
            && out.status.success()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !stdout.trim().is_empty() {
                let dirty_files: Vec<&str> = stdout.lines().take(5).collect();
                log_daemon(
                    log_path,
                    &format!(
                        "WARNING: main checkout dirty at {} — possible agent contamination: {}",
                        repo_dir.display(),
                        dirty_files.join(", ")
                    ),
                );
                notifier.notify_macos(
                    "werma: main branch contamination detected",
                    &format!(
                        "Dirty files in {}: {}",
                        repo_dir.display(),
                        dirty_files.join(", ")
                    ),
                    "Basso",
                );
                notified.insert(repo_dir, Instant::now());
            } else {
                notified.remove(&repo_dir);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── rotate_logs ────────────────────────────────────────────────────

    #[test]
    fn rotate_logs_skips_small_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let log_file = logs_dir.join("test.log");
        std::fs::write(&log_file, "small log content\n").unwrap();

        rotate_logs(dir.path()).unwrap();

        let content = std::fs::read_to_string(&log_file).unwrap();
        assert_eq!(content, "small log content\n");
    }

    #[test]
    fn rotate_logs_truncates_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let log_file = logs_dir.join("daemon.log");

        let mut content = String::new();
        for i in 0..10000 {
            content.push_str(&format!("{i:04}: {}\n", "X".repeat(590)));
        }
        std::fs::write(&log_file, &content).unwrap();

        let meta = std::fs::metadata(&log_file).unwrap();
        assert!(meta.len() > MAX_LOG_SIZE_BYTES);

        rotate_logs(dir.path()).unwrap();

        let result = std::fs::read_to_string(&log_file).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 1000);
        assert!(lines[0].starts_with("9000:"));
        assert!(lines[999].starts_with("9999:"));
    }

    #[test]
    fn rotate_logs_skips_non_log_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let md_file = logs_dir.join("output.md");
        let content = "X".repeat(6 * 1024 * 1024);
        std::fs::write(&md_file, &content).unwrap();

        rotate_logs(dir.path()).unwrap();

        let result = std::fs::read_to_string(&md_file).unwrap();
        assert_eq!(result.len(), 6 * 1024 * 1024);
    }

    #[test]
    fn rotate_logs_nonexistent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = rotate_logs(dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn rotate_logs_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        rotate_logs(dir.path()).unwrap();
    }

    #[test]
    fn rotate_logs_exactly_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let log_file = logs_dir.join("test.log");
        // Create file exactly at the threshold — should NOT be rotated (> not >=)
        let content = "X".repeat(MAX_LOG_SIZE_BYTES as usize);
        std::fs::write(&log_file, &content).unwrap();

        rotate_logs(dir.path()).unwrap();

        let result = std::fs::read_to_string(&log_file).unwrap();
        assert_eq!(result.len(), MAX_LOG_SIZE_BYTES as usize);
    }

    #[test]
    fn rotate_logs_file_with_fewer_than_1000_lines() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let log_file = logs_dir.join("daemon.log");

        // Create a file > 5MB but with only 500 lines (each line very long)
        let mut content = String::new();
        for i in 0..500 {
            content.push_str(&format!("{i:03}: {}\n", "Y".repeat(12000)));
        }
        std::fs::write(&log_file, &content).unwrap();

        let meta = std::fs::metadata(&log_file).unwrap();
        assert!(meta.len() > MAX_LOG_SIZE_BYTES);

        rotate_logs(dir.path()).unwrap();

        // All 500 lines should be preserved (fewer than 1000)
        let result = std::fs::read_to_string(&log_file).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 500);
    }

    // ─── check_main_branch_cleanliness ──────────────────────────────────

    #[test]
    fn no_running_tasks_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");
        let db = crate::db::Db::open_in_memory().unwrap();
        let mut notified = HashMap::new();
        let fake_notifier = crate::traits::fakes::FakeNotifier::new();

        check_main_branch_cleanliness(&db, &log_path, &mut notified, 300, &fake_notifier).unwrap();
    }
}
