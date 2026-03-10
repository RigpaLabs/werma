use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::models::Task;

/// Whether this task type needs an isolated git worktree.
/// Write tasks (code, full, refactor, pipeline-engineer, pipeline-devops) get worktrees.
/// Read-only tasks (research, review, analyze, pipeline-analyst, pipeline-reviewer, pipeline-qa)
/// run directly in working_dir since they can't cause git conflicts.
pub fn needs_worktree(task_type: &str) -> bool {
    matches!(
        task_type,
        "code" | "full" | "refactor" | "pipeline-engineer" | "pipeline-devops"
    )
}

/// Generate a branch name from a task.
/// With linear_issue_id → RIG-XX/slugified-title
/// Without → werma-{task_id}
pub fn generate_branch_name(task: &Task) -> String {
    if !task.linear_issue_id.is_empty() {
        let slug = slugify_prompt(&task.prompt);
        let rig_id = extract_rig_id(&task.prompt).unwrap_or_default();
        if rig_id.is_empty() {
            format!("werma-{}/{}", task.id, slug)
        } else {
            format!("{}/{}", rig_id, slug)
        }
    } else {
        format!("werma-{}", task.id)
    }
}

/// Set up a git worktree for the given branch.
/// Creates .trees/{branch} inside working_dir.
/// If the worktree already exists (resume case), returns its path.
pub fn setup_worktree(working_dir: &Path, branch_name: &str) -> Result<PathBuf> {
    let trees_dir = working_dir.join(".trees");
    std::fs::create_dir_all(&trees_dir)
        .with_context(|| format!("creating .trees/ in {}", working_dir.display()))?;

    let dir_name = branch_name.replace('/', "--");
    let worktree_path = trees_dir.join(dir_name);

    // Resume case: worktree already exists
    if worktree_path.exists() {
        return Ok(worktree_path);
    }

    // Try creating with a new branch first
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            &worktree_path.to_string_lossy(),
            "-b",
            branch_name,
        ])
        .current_dir(working_dir)
        .output()
        .context("running git worktree add")?;

    if output.status.success() {
        return Ok(worktree_path);
    }

    // Branch might already exist (e.g. from a previous failed task) — attach to it
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already exists") {
        let output2 = Command::new("git")
            .args([
                "worktree",
                "add",
                &worktree_path.to_string_lossy(),
                branch_name,
            ])
            .current_dir(working_dir)
            .output()
            .context("running git worktree add (existing branch)")?;

        if output2.status.success() {
            return Ok(worktree_path);
        }

        let stderr2 = String::from_utf8_lossy(&output2.stderr);
        bail!(
            "git worktree add failed for existing branch '{}': {}",
            branch_name,
            stderr2.trim()
        );
    }

    bail!(
        "git worktree add failed for '{}': {}",
        branch_name,
        stderr.trim()
    );
}

/// Remove a worktree (does NOT delete the branch).
#[allow(dead_code)]
pub fn cleanup_worktree(working_dir: &Path, branch_name: &str) -> Result<()> {
    let dir_name = branch_name.replace('/', "--");
    let worktree_path = working_dir.join(".trees").join(dir_name);

    if !worktree_path.exists() {
        return Ok(());
    }

    let output = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .current_dir(working_dir)
        .output()
        .context("running git worktree remove")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree remove failed: {}", stderr.trim());
    }

    Ok(())
}

/// Extract RIG-XX from the beginning of a string.
/// Matches patterns like "RIG-42 ...", "  RIG-42 ...", "[RIG-42] ..."
pub fn extract_rig_id_prefix(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let trimmed = trimmed.strip_prefix('[').unwrap_or(trimmed);
    if let Some(digits) = trimmed.strip_prefix("RIG-") {
        // Collect digits after "RIG-"
        let digit_end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        let id = &trimmed[..4 + digit_end]; // "RIG-" + digits
        if id.len() > 4 {
            return Some(id.to_string());
        }
    }
    None
}

/// Extract RIG-XX identifier from a prompt string.
fn extract_rig_id(prompt: &str) -> Option<String> {
    let re_pattern = "RIG-";
    let start = prompt.find(re_pattern)?;
    let rest = &prompt[start..];

    // Collect RIG- followed by digits
    let end = rest
        .char_indices()
        .skip(4) // skip "RIG-"
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());

    let id = &rest[..end];
    if id.len() > 4 {
        // Must have at least one digit after RIG-
        Some(id.to_uppercase())
    } else {
        None
    }
}

/// Slugify a prompt into a short branch-name-safe string.
/// Takes first ~4 meaningful words, lowercased, joined by hyphens.
fn slugify_prompt(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt);

    // Remove bracketed prefix like "[RIG-XX]"
    let cleaned = if first_line.starts_with('[') {
        first_line
            .find(']')
            .map(|i| &first_line[i + 1..])
            .unwrap_or(first_line)
            .trim()
    } else {
        first_line.trim()
    };

    let slug: String = cleaned
        .split_whitespace()
        .filter(|w| w.len() > 2) // skip short words
        .take(4)
        .map(|w| {
            w.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_lowercase()
        })
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "task".to_string()
    } else {
        // Truncate to reasonable branch name length
        slug.chars().take(40).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Status;

    fn test_task(task_type: &str, linear_issue_id: &str, prompt: &str) -> Task {
        Task {
            id: "20260310-001".to_string(),
            status: Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: task_type.to_string(),
            prompt: prompt.to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: linear_issue_id.to_string(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        }
    }

    // --- needs_worktree ---

    #[test]
    fn needs_worktree_write_types() {
        assert!(needs_worktree("code"));
        assert!(needs_worktree("full"));
        assert!(needs_worktree("refactor"));
        assert!(needs_worktree("pipeline-engineer"));
        assert!(needs_worktree("pipeline-devops"));
    }

    #[test]
    fn needs_worktree_read_types() {
        assert!(!needs_worktree("research"));
        assert!(!needs_worktree("review"));
        assert!(!needs_worktree("analyze"));
        assert!(!needs_worktree("pipeline-analyst"));
        assert!(!needs_worktree("pipeline-reviewer"));
        assert!(!needs_worktree("pipeline-qa"));
    }

    #[test]
    fn needs_worktree_unknown_type() {
        assert!(!needs_worktree("something-random"));
    }

    // --- generate_branch_name ---

    #[test]
    fn branch_name_with_linear_issue() {
        let task = test_task(
            "code",
            "issue-abc-123",
            "[RIG-42] Add worktree support for parallel agents",
        );
        let name = generate_branch_name(&task);
        assert!(
            name.starts_with("RIG-42/"),
            "expected RIG-42/ prefix, got: {name}"
        );
        assert!(name.contains("worktree"));
    }

    #[test]
    fn branch_name_without_linear() {
        let task = test_task("code", "", "Fix something broken");
        let name = generate_branch_name(&task);
        assert_eq!(name, "werma-20260310-001");
    }

    #[test]
    fn branch_name_refactor_type() {
        let task = test_task("refactor", "", "Cleanup module structure");
        let name = generate_branch_name(&task);
        assert_eq!(name, "werma-20260310-001");
    }

    #[test]
    fn branch_name_linear_without_rig_id() {
        let task = test_task("code", "issue-abc-123", "Add feature without issue prefix");
        let name = generate_branch_name(&task);
        assert!(
            name.starts_with("werma-20260310-001/"),
            "expected werma- prefix, got: {name}"
        );
    }

    // --- extract_rig_id_prefix ---

    #[test]
    fn extract_rig_id_prefix_found() {
        assert_eq!(
            extract_rig_id_prefix("RIG-83 do stuff"),
            Some("RIG-83".to_string())
        );
        assert_eq!(
            extract_rig_id_prefix("  RIG-42 something"),
            Some("RIG-42".to_string())
        );
        assert_eq!(
            extract_rig_id_prefix("[RIG-100] title"),
            Some("RIG-100".to_string())
        );
    }

    #[test]
    fn extract_rig_id_prefix_not_at_start() {
        assert_eq!(
            extract_rig_id_prefix("fix the thing RIG-99 mentioned"),
            None
        );
        assert_eq!(extract_rig_id_prefix("no issue here"), None);
        assert_eq!(extract_rig_id_prefix("RIG- no digits"), None);
    }

    // --- extract_rig_id ---

    #[test]
    fn extract_rig_id_found() {
        assert_eq!(
            extract_rig_id("[RIG-42] Something"),
            Some("RIG-42".to_string())
        );
        assert_eq!(extract_rig_id("RIG-123 stuff"), Some("RIG-123".to_string()));
    }

    #[test]
    fn extract_rig_id_not_found() {
        assert_eq!(extract_rig_id("no issue id here"), None);
        assert_eq!(extract_rig_id("RIG- no digits"), None);
    }

    // --- slugify_prompt ---

    #[test]
    fn slugify_basic() {
        let slug = slugify_prompt("Add worktree support for parallel agents");
        assert_eq!(slug, "add-worktree-support-for");
    }

    #[test]
    fn slugify_with_prefix() {
        let slug = slugify_prompt("[RIG-42] Add worktree support");
        assert!(slug.contains("worktree"));
    }

    #[test]
    fn slugify_empty() {
        assert_eq!(slugify_prompt(""), "task");
        assert_eq!(slugify_prompt("a b"), "task"); // too short words
    }

    // --- setup_worktree + cleanup_worktree (integration) ---

    #[test]
    fn setup_worktree_creates_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path();

        // Init a git repo
        Command::new("git")
            .args(["init"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        let branch = "RIG-99/test-branch";

        // First call: creates worktree
        let path = setup_worktree(repo_dir, branch).unwrap();
        assert!(path.exists());
        assert!(path.ends_with(".trees/RIG-99--test-branch"));

        // Second call: returns same path (resume)
        let path2 = setup_worktree(repo_dir, branch).unwrap();
        assert_eq!(path, path2);

        // Cleanup
        cleanup_worktree(repo_dir, branch).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_nonexistent_worktree_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        // No error on missing worktree
        assert!(cleanup_worktree(dir.path(), "nonexistent").is_ok());
    }
}
