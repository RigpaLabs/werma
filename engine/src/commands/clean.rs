use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::NaiveDateTime;

use crate::db::Db;
use crate::models::Status;

/// Minimum age (in hours) before a terminal task's worktree is eligible for pruning.
const STALE_THRESHOLD_HOURS: i64 = 24;

/// A worktree directory that is a candidate for pruning.
#[derive(Debug)]
struct StaleWorktree {
    path: PathBuf,
    task_id: Option<String>,
    reason: String,
}

/// `werma clean` — prune orphaned/stale worktrees from completed/failed tasks.
///
/// Default (dry-run): lists worktrees eligible for removal.
/// With --force: actually deletes them.
pub fn cmd_clean_worktrees(db: &Db, force: bool) -> Result<()> {
    let stale = find_stale_worktrees(db)?;

    if stale.is_empty() {
        println!("no stale worktrees found");
        return Ok(());
    }

    if force {
        let mut removed = 0;
        let mut failed = 0;
        for entry in &stale {
            match remove_worktree(&entry.path) {
                Ok(()) => {
                    println!("removed: {}", entry.path.display());
                    removed += 1;
                }
                Err(e) => {
                    eprintln!("failed to remove {}: {e}", entry.path.display());
                    failed += 1;
                }
            }
        }
        println!("\ncleaned {removed} worktrees ({failed} failed)");
    } else {
        println!("stale worktrees (use --force to delete):\n");
        for entry in &stale {
            let task_info = entry
                .task_id
                .as_deref()
                .map(|id| format!(" (task {id})"))
                .unwrap_or_default();
            println!("  {}{task_info} — {}", entry.path.display(), entry.reason);
        }
        println!("\n{} worktrees would be removed", stale.len());
    }

    Ok(())
}

/// Find all stale worktrees by cross-referencing DB tasks with filesystem `.trees/` entries.
fn find_stale_worktrees(db: &Db) -> Result<Vec<StaleWorktree>> {
    let now = chrono::Local::now().naive_local();
    let mut stale = Vec::new();

    // Collect all terminal tasks with working_dir in .trees/
    let mut known_worktree_paths: HashSet<PathBuf> = HashSet::new();
    let mut repo_roots: HashSet<PathBuf> = HashSet::new();

    for status in [Status::Completed, Status::Failed, Status::Canceled] {
        let tasks = db.list_all_tasks_by_finished(status)?;
        for task in &tasks {
            if !is_trees_path(&task.working_dir) {
                continue;
            }

            let working_path = resolve_path(&task.working_dir);
            known_worktree_paths.insert(working_path.clone());

            // Extract the repo root (parent of .trees/)
            if let Some(repo_root) = find_repo_root(&working_path) {
                repo_roots.insert(repo_root);
            }

            // Check if finished >24h ago
            if let Some(ref finished_at) = task.finished_at {
                if let Some(age_hours) = hours_since(finished_at, now) {
                    if age_hours >= STALE_THRESHOLD_HOURS && working_path.exists() {
                        stale.push(StaleWorktree {
                            path: working_path,
                            task_id: Some(task.id.clone()),
                            reason: format!("terminal for {age_hours}h"),
                        });
                    }
                }
            }
        }
    }

    // Also collect working_dirs from non-terminal tasks to avoid pruning active worktrees
    let mut active_worktree_paths: HashSet<PathBuf> = HashSet::new();
    for status in [Status::Pending, Status::Running] {
        let tasks = db.list_tasks(Some(status))?;
        for task in &tasks {
            if is_trees_path(&task.working_dir) {
                let working_path = resolve_path(&task.working_dir);
                active_worktree_paths.insert(working_path.clone());

                if let Some(repo_root) = find_repo_root(&working_path) {
                    repo_roots.insert(repo_root);
                }
            }
        }
    }

    // Scan filesystem for orphan worktrees (exist on disk but no matching task in DB)
    let stale_paths: HashSet<PathBuf> = stale.iter().map(|s| s.path.clone()).collect();
    for repo_root in &repo_roots {
        let trees_dir = repo_root.join(".trees");
        if !trees_dir.is_dir() {
            continue;
        }

        let entries = match std::fs::read_dir(&trees_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Skip if already identified as stale or if it belongs to an active task
            if stale_paths.contains(&path) || active_worktree_paths.contains(&path) {
                continue;
            }

            // Skip if it belongs to a known terminal task that's not yet 24h old
            if known_worktree_paths.contains(&path) {
                continue;
            }

            // Orphan: on disk but no DB record at all — safe to prune
            stale.push(StaleWorktree {
                path,
                task_id: None,
                reason: "orphan (no matching task in DB)".to_string(),
            });
        }
    }

    Ok(stale)
}

/// Remove a worktree directory.
/// First tries `git worktree remove`, falls back to `rm -rf` if git fails.
fn remove_worktree(path: &Path) -> Result<()> {
    // Find the repo root to run git commands from
    if let Some(repo_root) = find_repo_root(path) {
        let output = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", &path.to_string_lossy()])
            .current_dir(&repo_root)
            .output();

        if let Ok(ref out) = output {
            if out.status.success() {
                return Ok(());
            }
        }
    }

    // Fallback: direct removal if git worktree remove fails
    std::fs::remove_dir_all(path)?;

    // Run git worktree prune to clean up stale refs
    if let Some(repo_root) = find_repo_root(path) {
        let _ = std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&repo_root)
            .output();
    }

    Ok(())
}

/// Check if a path string contains `.trees/` (indicating it's a worktree).
fn is_trees_path(path: &str) -> bool {
    path.contains("/.trees/") || path.contains("\\.trees\\")
}

/// Find the repo root from a worktree path by looking for the `.trees/` component.
/// e.g. `/home/user/projects/werma/.trees/feat--RIG-42` → `/home/user/projects/werma`
fn find_repo_root(worktree_path: &Path) -> Option<PathBuf> {
    let path_str = worktree_path.to_string_lossy();
    path_str
        .find("/.trees/")
        .map(|idx| PathBuf::from(&path_str[..idx]))
}

/// Resolve ~ to home directory.
fn resolve_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest.strip_prefix('/').unwrap_or(rest));
        }
    }
    PathBuf::from(path)
}

/// Parse a finished_at timestamp and return hours elapsed since then.
fn hours_since(finished_at: &str, now: NaiveDateTime) -> Option<i64> {
    NaiveDateTime::parse_from_str(finished_at, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|finished| (now - finished).num_hours())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::models::Task;

    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn make_task(id: &str, status: Status, working_dir: &str, finished_at: Option<&str>) -> Task {
        Task {
            id: id.into(),
            status,
            task_type: "code".into(),
            prompt: "test".into(),
            working_dir: working_dir.into(),
            model: "sonnet".into(),
            finished_at: finished_at.map(String::from),
            ..Default::default()
        }
    }

    // --- is_trees_path ---

    #[test]
    fn is_trees_path_positive() {
        assert!(is_trees_path("/home/user/project/.trees/feat--RIG-42"));
        assert!(is_trees_path("~/projects/werma/.trees/fix--RIG-99-hotfix"));
    }

    #[test]
    fn is_trees_path_negative() {
        assert!(!is_trees_path("/home/user/project"));
        assert!(!is_trees_path("/tmp"));
        assert!(!is_trees_path("~/projects/werma"));
    }

    // --- find_repo_root ---

    #[test]
    fn find_repo_root_from_worktree() {
        let path = Path::new("/home/user/projects/werma/.trees/feat--RIG-42");
        assert_eq!(
            find_repo_root(path),
            Some(PathBuf::from("/home/user/projects/werma"))
        );
    }

    #[test]
    fn find_repo_root_no_trees() {
        let path = Path::new("/home/user/projects/werma");
        assert_eq!(find_repo_root(path), None);
    }

    // --- hours_since ---

    #[test]
    fn hours_since_recent() {
        let now =
            NaiveDateTime::parse_from_str("2026-03-31T20:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let hours = hours_since("2026-03-31T18:00:00", now);
        assert_eq!(hours, Some(2));
    }

    #[test]
    fn hours_since_old() {
        let now =
            NaiveDateTime::parse_from_str("2026-03-31T20:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let hours = hours_since("2026-03-30T10:00:00", now);
        assert_eq!(hours, Some(34));
    }

    #[test]
    fn hours_since_invalid_timestamp() {
        let now =
            NaiveDateTime::parse_from_str("2026-03-31T20:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(hours_since("not-a-date", now), None);
    }

    // --- resolve_path ---

    #[test]
    fn resolve_path_absolute() {
        assert_eq!(resolve_path("/tmp/foo"), PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn resolve_path_tilde() {
        let resolved = resolve_path("~/projects/werma");
        // Should start with home dir, not literal ~
        assert!(!resolved.to_string_lossy().starts_with('~'));
        assert!(resolved.to_string_lossy().ends_with("projects/werma"));
    }

    // --- find_stale_worktrees (DB-level) ---

    #[test]
    fn find_stale_no_tasks() {
        let db = test_db();
        let stale = find_stale_worktrees(&db).unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn find_stale_ignores_non_trees_tasks() {
        let db = test_db();
        let task = make_task(
            "20260331-001",
            Status::Completed,
            "/tmp/regular-dir",
            Some("2026-03-29T10:00:00"),
        );
        db.insert_task(&task).unwrap();

        let stale = find_stale_worktrees(&db).unwrap();
        assert!(
            stale.is_empty(),
            "tasks without .trees/ path should be ignored"
        );
    }

    #[test]
    fn find_stale_ignores_recent_terminal() {
        let db = test_db();
        // Finished 1 hour ago — not yet stale
        let now = chrono::Local::now();
        let recent = (now - chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();

        let task = make_task(
            "20260331-001",
            Status::Completed,
            "/nonexistent/.trees/feat--RIG-99",
            Some(&recent),
        );
        db.insert_task(&task).unwrap();

        let stale = find_stale_worktrees(&db).unwrap();
        // The directory doesn't exist, so even if it's "stale" by time, it won't be listed
        // (existence check filters it out)
        assert!(stale.is_empty());
    }

    #[test]
    fn find_stale_with_tempdir_worktree() {
        let db = test_db();
        let tmp = tempfile::tempdir().unwrap();
        let trees_dir = tmp.path().join(".trees");
        std::fs::create_dir_all(&trees_dir).unwrap();

        // Create a fake worktree directory
        let wt_dir = trees_dir.join("feat--RIG-42-old-feature");
        std::fs::create_dir_all(&wt_dir).unwrap();

        // Task finished 48 hours ago, pointing to that worktree
        let task = make_task(
            "20260331-001",
            Status::Completed,
            &wt_dir.to_string_lossy(),
            Some("2026-03-29T10:00:00"),
        );
        db.insert_task(&task).unwrap();

        let stale = find_stale_worktrees(&db).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].path, wt_dir);
        assert_eq!(stale[0].task_id, Some("20260331-001".to_string()));
        assert!(stale[0].reason.contains("terminal for"));
    }

    #[test]
    fn find_stale_detects_orphan_worktrees() {
        let db = test_db();
        let tmp = tempfile::tempdir().unwrap();
        let trees_dir = tmp.path().join(".trees");
        std::fs::create_dir_all(&trees_dir).unwrap();

        // Orphan directory — no task in DB points here
        let orphan_dir = trees_dir.join("feat--RIG-99-orphan");
        std::fs::create_dir_all(&orphan_dir).unwrap();

        // We need at least one task pointing to this repo root so the scanner finds it
        let known_wt = trees_dir.join("feat--RIG-100-known");
        std::fs::create_dir_all(&known_wt).unwrap();
        let task = make_task(
            "20260331-002",
            Status::Completed,
            &known_wt.to_string_lossy(),
            Some("2026-03-29T10:00:00"),
        );
        db.insert_task(&task).unwrap();

        let stale = find_stale_worktrees(&db).unwrap();

        // Should find both: the known stale worktree + the orphan
        let orphan = stale.iter().find(|s| s.path == orphan_dir);
        assert!(orphan.is_some(), "orphan worktree should be detected");
        assert!(
            orphan.unwrap().reason.contains("orphan"),
            "reason should indicate orphan"
        );
        assert!(
            orphan.unwrap().task_id.is_none(),
            "orphan should have no task_id"
        );
    }

    #[test]
    fn find_stale_skips_active_task_worktrees() {
        let db = test_db();
        let tmp = tempfile::tempdir().unwrap();
        let trees_dir = tmp.path().join(".trees");
        std::fs::create_dir_all(&trees_dir).unwrap();

        // Active (running) task worktree
        let active_wt = trees_dir.join("feat--RIG-50-active");
        std::fs::create_dir_all(&active_wt).unwrap();
        let active_task = make_task(
            "20260331-001",
            Status::Running,
            &active_wt.to_string_lossy(),
            None,
        );
        db.insert_task(&active_task).unwrap();

        // Stale completed task so the repo root gets scanned
        let stale_wt = trees_dir.join("feat--RIG-51-stale");
        std::fs::create_dir_all(&stale_wt).unwrap();
        let stale_task = make_task(
            "20260331-002",
            Status::Completed,
            &stale_wt.to_string_lossy(),
            Some("2026-03-29T10:00:00"),
        );
        db.insert_task(&stale_task).unwrap();

        let stale = find_stale_worktrees(&db).unwrap();

        // Active worktree must NOT be listed
        assert!(
            !stale.iter().any(|s| s.path == active_wt),
            "active task worktree must not be pruned"
        );
        // Stale one should be listed
        assert!(stale.iter().any(|s| s.path == stale_wt));
    }

    // --- cmd_clean_worktrees dry-run ---

    #[test]
    fn cmd_clean_worktrees_dry_run_empty() {
        let db = test_db();
        cmd_clean_worktrees(&db, false).unwrap();
    }

    // --- cmd_clean_worktrees force ---

    #[test]
    fn cmd_clean_worktrees_force_removes_directory() {
        let db = test_db();
        let tmp = tempfile::tempdir().unwrap();
        let trees_dir = tmp.path().join(".trees");
        std::fs::create_dir_all(&trees_dir).unwrap();

        let wt_dir = trees_dir.join("feat--RIG-42-removeme");
        std::fs::create_dir_all(&wt_dir).unwrap();
        // Put a file in it to verify recursive removal
        std::fs::write(wt_dir.join("test.txt"), "content").unwrap();

        let task = make_task(
            "20260331-001",
            Status::Completed,
            &wt_dir.to_string_lossy(),
            Some("2026-03-29T10:00:00"),
        );
        db.insert_task(&task).unwrap();

        cmd_clean_worktrees(&db, true).unwrap();

        assert!(!wt_dir.exists(), "worktree directory should be removed");
    }

    #[test]
    fn find_stale_includes_all_terminal_statuses() {
        let db = test_db();
        let tmp = tempfile::tempdir().unwrap();
        let trees_dir = tmp.path().join(".trees");
        std::fs::create_dir_all(&trees_dir).unwrap();

        for (i, status) in [Status::Completed, Status::Failed, Status::Canceled]
            .iter()
            .enumerate()
        {
            let wt_dir = trees_dir.join(format!("feat--RIG-{i}-terminal"));
            std::fs::create_dir_all(&wt_dir).unwrap();

            let task = make_task(
                &format!("20260331-00{}", i + 1),
                *status,
                &wt_dir.to_string_lossy(),
                Some("2026-03-29T10:00:00"),
            );
            db.insert_task(&task).unwrap();
        }

        let stale = find_stale_worktrees(&db).unwrap();
        assert_eq!(
            stale.len(),
            3,
            "all three terminal statuses should be found"
        );
    }
}
