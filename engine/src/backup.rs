use anyhow::{Context, Result};
use std::path::Path;

/// Create an atomic backup of the SQLite database via `sqlite3 .backup`.
pub fn backup_db(werma_dir: &Path) -> Result<String> {
    let db_path = werma_dir.join("werma.db");
    let backup_dir = werma_dir.join("backups");
    std::fs::create_dir_all(&backup_dir)?;

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup_path = backup_dir.join(format!("werma-{timestamp}.db"));

    let status = std::process::Command::new("sqlite3")
        .args([
            db_path.to_str().unwrap_or_default(),
            &format!(".backup '{}'", backup_path.display()),
        ])
        .status()
        .context("sqlite3 backup command failed")?;

    if !status.success() {
        anyhow::bail!("sqlite3 backup exited with {status}");
    }

    prune_backups(&backup_dir, 7)?;

    println!("backup: {}", backup_path.display());
    Ok(backup_path.to_string_lossy().to_string())
}

/// Remove oldest backups keeping only `keep` most recent.
fn prune_backups(backup_dir: &Path, keep: usize) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(backup_dir)?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "db"))
        .collect();

    // Sort by filename (which includes timestamp, so lexicographic = chronological).
    entries.sort_by_key(std::fs::DirEntry::path);

    if entries.len() > keep {
        for entry in &entries[..entries.len() - keep] {
            std::fs::remove_file(entry.path())?;
        }
    }

    Ok(())
}

/// Git auto-commit state for tracking db evolution.
#[allow(dead_code)]
pub fn git_commit_state(werma_dir: &Path) -> Result<()> {
    let werma_repo = werma_dir
        .parent()
        .and_then(|p| p.parent())
        .context("cannot find werma repo root")?;

    let git_check = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(werma_repo)
        .output();

    if let Ok(o) = git_check
        && o.status.success()
    {
        let _ = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(werma_repo)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "werma: auto-state update", "--allow-empty"])
            .current_dir(werma_repo)
            .status();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_backups_keeps_n_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let backup_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backup_dir).unwrap();

        // Create 10 fake backup files with sortable names.
        for i in 0..10 {
            let name = format!("werma-20260309-{i:06}.db");
            std::fs::write(backup_dir.join(&name), "fake db").unwrap();
        }

        let count_before: usize = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .count();
        assert_eq!(count_before, 10);

        prune_backups(&backup_dir, 3).unwrap();

        let remaining: Vec<String> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        assert_eq!(remaining.len(), 3);
        // Should keep the 3 most recent (highest numbered).
        for name in &remaining {
            let num: usize = name
                .strip_prefix("werma-20260309-")
                .and_then(|s| s.strip_suffix(".db"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            assert!(num >= 7, "unexpected file kept: {name}");
        }
    }

    #[test]
    fn prune_backups_noop_when_under_limit() {
        let dir = tempfile::tempdir().unwrap();
        let backup_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backup_dir).unwrap();

        for i in 0..3 {
            let name = format!("werma-20260309-{i:06}.db");
            std::fs::write(backup_dir.join(&name), "fake db").unwrap();
        }

        prune_backups(&backup_dir, 7).unwrap();

        let count: usize = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .count();
        assert_eq!(count, 3);
    }

    #[test]
    fn prune_backups_ignores_non_db_files() {
        let dir = tempfile::tempdir().unwrap();
        let backup_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backup_dir).unwrap();

        // Create db files and a non-db file.
        for i in 0..5 {
            std::fs::write(backup_dir.join(format!("werma-{i:06}.db")), "fake db").unwrap();
        }
        std::fs::write(backup_dir.join("readme.txt"), "not a backup").unwrap();

        prune_backups(&backup_dir, 2).unwrap();

        let db_count: usize = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "db"))
            .count();
        assert_eq!(db_count, 2);

        // Non-db file should still be there.
        assert!(backup_dir.join("readme.txt").exists());
    }

    #[test]
    fn prune_backups_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let backup_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backup_dir).unwrap();

        // Should not error on empty directory.
        prune_backups(&backup_dir, 7).unwrap();
    }
}
