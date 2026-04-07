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
/// Ignores untracked (`??`) and ignored (`!!`) files — only modified/staged files count.
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
            // Filter out untracked (??) and ignored (!!) files — only
            // modified/staged/deleted files indicate real contamination.
            let tracked_dirty: Vec<&str> = stdout
                .lines()
                .filter(|line| !line.starts_with("??") && !line.starts_with("!!"))
                .collect();
            if !tracked_dirty.is_empty() {
                let dirty_files: Vec<&str> = tracked_dirty.iter().take(5).copied().collect();
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

    /// Create a temp git repo and return its path.
    fn init_temp_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        dir
    }

    /// Insert a running task with the given working_dir and task_type.
    fn insert_running_task(db: &crate::db::Db, working_dir: &str) {
        let task = crate::models::Task {
            id: "20260331-001".to_string(),
            status: crate::models::Status::Running,
            priority: 1,
            created_at: "2026-03-31T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "code".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: working_dir.to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            issue_identifier: String::new(),
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
    }

    #[test]
    fn untracked_files_do_not_trigger_warning() {
        let repo = init_temp_git_repo();
        // Create an untracked file — should NOT trigger contamination.
        std::fs::write(repo.path().join("untracked.txt"), "hello").unwrap();

        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("daemon.log");
        let db = crate::db::Db::open_in_memory().unwrap();
        insert_running_task(&db, &repo.path().to_string_lossy());

        let mut notified = HashMap::new();
        let fake_notifier = crate::traits::fakes::FakeNotifier::new();

        check_main_branch_cleanliness(&db, &log_path, &mut notified, 300, &fake_notifier).unwrap();

        // No macOS notification should have been sent.
        assert!(
            fake_notifier.macos_calls.borrow().is_empty(),
            "untracked files should not trigger contamination warning"
        );
    }

    #[test]
    fn modified_tracked_file_triggers_warning() {
        let repo = init_temp_git_repo();
        // Create and commit a file, then modify it — should trigger contamination.
        let file_path = repo.path().join("tracked.txt");
        std::fs::write(&file_path, "original").unwrap();
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add tracked file"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        // Modify the tracked file.
        std::fs::write(&file_path, "modified").unwrap();

        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("daemon.log");
        let db = crate::db::Db::open_in_memory().unwrap();
        insert_running_task(&db, &repo.path().to_string_lossy());

        let mut notified = HashMap::new();
        let fake_notifier = crate::traits::fakes::FakeNotifier::new();

        check_main_branch_cleanliness(&db, &log_path, &mut notified, 300, &fake_notifier).unwrap();

        // Should have triggered a notification.
        assert_eq!(
            fake_notifier.macos_calls.borrow().len(),
            1,
            "modified tracked file should trigger contamination warning"
        );
    }

    #[test]
    fn staged_file_triggers_warning() {
        let repo = init_temp_git_repo();
        // Stage a new file (not committed) — should trigger contamination.
        let file_path = repo.path().join("staged.txt");
        std::fs::write(&file_path, "staged content").unwrap();
        std::process::Command::new("git")
            .args(["add", "staged.txt"])
            .current_dir(repo.path())
            .output()
            .unwrap();

        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("daemon.log");
        let db = crate::db::Db::open_in_memory().unwrap();
        insert_running_task(&db, &repo.path().to_string_lossy());

        let mut notified = HashMap::new();
        let fake_notifier = crate::traits::fakes::FakeNotifier::new();

        check_main_branch_cleanliness(&db, &log_path, &mut notified, 300, &fake_notifier).unwrap();

        assert_eq!(
            fake_notifier.macos_calls.borrow().len(),
            1,
            "staged file should trigger contamination warning"
        );
    }

    #[test]
    fn mixed_untracked_and_modified_only_warns_for_modified() {
        let repo = init_temp_git_repo();
        // Commit a file, then modify it.
        let tracked = repo.path().join("tracked.txt");
        std::fs::write(&tracked, "original").unwrap();
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add file"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        std::fs::write(&tracked, "modified").unwrap();

        // Also create an untracked file.
        std::fs::write(repo.path().join("untracked.txt"), "ignore me").unwrap();

        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("daemon.log");
        let db = crate::db::Db::open_in_memory().unwrap();
        insert_running_task(&db, &repo.path().to_string_lossy());

        let mut notified = HashMap::new();
        let fake_notifier = crate::traits::fakes::FakeNotifier::new();

        check_main_branch_cleanliness(&db, &log_path, &mut notified, 300, &fake_notifier).unwrap();

        // Should warn (because of modified tracked file), but the warning
        // should not include the untracked file.
        let calls = fake_notifier.macos_calls.borrow();
        assert_eq!(calls.len(), 1, "should warn about modified file");
        let (_, body, _) = &calls[0];
        assert!(
            body.contains("tracked.txt"),
            "warning should mention tracked.txt"
        );
        assert!(
            !body.contains("untracked.txt"),
            "warning should not mention untracked.txt"
        );
    }
}
