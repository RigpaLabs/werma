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

/// Derive a git branch type prefix from task type and prompt content.
/// Pipeline stages map to: engineer→feat/fix, reviewer→review, analyst→chore, devops→chore.
/// Regular tasks: code→feat, refactor→refactor, full→feat.
/// If the prompt's first line contains "fix:" or "fix!:", overrides to "fix".
fn derive_branch_type(task: &Task) -> &'static str {
    // Check prompt for fix indicators (conventional commit prefix or keywords)
    let prompt_lower = task.prompt.to_lowercase();
    let first_line = prompt_lower.lines().next().unwrap_or("");
    let is_fix = first_line.contains("fix:") || first_line.contains("fix!:");

    match task.task_type.as_str() {
        "pipeline-engineer" | "code" | "full" => {
            if is_fix {
                "fix"
            } else {
                "feat"
            }
        }
        "refactor" => "refactor",
        "pipeline-reviewer" | "pipeline-qa" => "review",
        "pipeline-analyst" => "chore",
        "pipeline-devops" => "chore",
        _ => "feat",
    }
}

/// Generate a branch name from a task.
/// Pipeline tasks → type/RIG-XX-pipeline-{stage}-stage (deterministic, enables branch reuse on re-spawn)
/// Non-pipeline with linear_issue_id → type/RIG-XX-short-name (e.g. feat/RIG-42-add-worktree-support)
/// Without → werma-{task_id}
pub fn generate_branch_name(task: &Task) -> String {
    if !task.linear_issue_id.is_empty() {
        // Try prompt first, then linear_issue_id (which is the identifier like "RIG-42")
        let rig_id = extract_linear_id(&task.prompt)
            .or_else(|| extract_linear_id_prefix(&task.linear_issue_id))
            .unwrap_or_default();

        // Pipeline tasks: deterministic branch name based on issue + stage.
        // This ensures re-spawned tasks (e.g. engineer after reviewer rejection)
        // reuse the same branch and worktree, so they can push to the existing PR.
        let slug = if !task.pipeline_stage.is_empty() {
            format!("pipeline-{}-stage", task.pipeline_stage)
        } else {
            slugify_prompt(&task.prompt)
        };

        if rig_id.is_empty() {
            format!("werma-{}/{}", task.id, slug)
        } else {
            let branch_type = derive_branch_type(task);
            format!("{branch_type}/{rig_id}-{slug}")
        }
    } else {
        format!("werma-{}", task.id)
    }
}

/// Fetch latest origin/main so worktrees branch from current HEAD.
fn fetch_origin_main(working_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["fetch", "origin", "main", "--quiet"])
        .current_dir(working_dir)
        .output()
        .context("running git fetch origin main")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Non-fatal: log warning but don't fail (offline, no remote, etc.)
        eprintln!(
            "warning: git fetch origin main failed (branching from potentially stale ref): {}",
            stderr.trim()
        );
    }
    Ok(())
}

/// Set up a git worktree for the given branch.
/// Creates .trees/{branch} inside working_dir.
/// If the worktree already exists (resume case), returns its path.
/// Installs a pre-commit hook to enforce cargo fmt in all worktrees.
pub fn setup_worktree(working_dir: &Path, branch_name: &str) -> Result<PathBuf> {
    let trees_dir = working_dir.join(".trees");
    std::fs::create_dir_all(&trees_dir)
        .with_context(|| format!("creating .trees/ in {}", working_dir.display()))?;

    let dir_name = branch_name.replace('/', "--");
    let worktree_path = trees_dir.join(dir_name);

    if !worktree_path.exists() {
        create_worktree_dir(working_dir, branch_name, &worktree_path)?;
    }

    // Install pre-commit hook for both new and resumed worktrees (idempotent)
    install_pre_commit_hook(&worktree_path)?;

    Ok(worktree_path)
}

/// Create the git worktree directory for the given branch.
fn create_worktree_dir(working_dir: &Path, branch_name: &str, worktree_path: &Path) -> Result<()> {
    // Fetch latest origin/main before branching to avoid stale base
    fetch_origin_main(working_dir)?;

    // Try creating with a new branch from origin/main
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            &worktree_path.to_string_lossy(),
            "-b",
            branch_name,
            "origin/main",
        ])
        .current_dir(working_dir)
        .output()
        .context("running git worktree add")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Branch already exists (e.g. from a previous failed task) — attach to it
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
            return Ok(());
        }

        let stderr2 = String::from_utf8_lossy(&output2.stderr);
        bail!(
            "git worktree add failed for existing branch '{}': {}",
            branch_name,
            stderr2.trim()
        );
    }

    // origin/main ref unresolvable (network failure, no cached ref) — fall back to HEAD
    if stderr.contains("bad revision")
        || stderr.contains("invalid reference")
        || stderr.contains("pathspec")
    {
        eprintln!(
            "warning: origin/main not resolvable, branching from HEAD: {}",
            stderr.trim()
        );
        let output_head = Command::new("git")
            .args([
                "worktree",
                "add",
                &worktree_path.to_string_lossy(),
                "-b",
                branch_name,
            ])
            .current_dir(working_dir)
            .output()
            .context("running git worktree add (fallback to HEAD)")?;

        if output_head.status.success() {
            return Ok(());
        }

        let stderr_head = String::from_utf8_lossy(&output_head.stderr);
        bail!(
            "git worktree add failed (HEAD fallback) for '{}': {}",
            branch_name,
            stderr_head.trim()
        );
    }

    bail!(
        "git worktree add failed for '{}': {}",
        branch_name,
        stderr.trim()
    );
}

/// Install a pre-commit hook that enforces `cargo fmt --check` before commits.
/// Prevents agents from committing unformatted code — CI won't fail on fmt anymore.
/// Idempotent: skips if hook already exists.
fn install_pre_commit_hook(worktree_path: &Path) -> Result<()> {
    // Resolve the actual git directory (worktrees use a .git file, not a directory)
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree_path)
        .output()
        .context("resolving git dir for pre-commit hook")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse --git-dir failed: {}", stderr.trim());
    }

    let git_dir_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let git_dir = if Path::new(&git_dir_raw).is_absolute() {
        PathBuf::from(&git_dir_raw)
    } else {
        worktree_path.join(&git_dir_raw)
    };

    let hooks_dir = git_dir.join("hooks");
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating hooks dir at {}", hooks_dir.display()))?;

    let hook_path = hooks_dir.join("pre-commit");

    const HOOK_VERSION: &str = "2";
    let hook_content = format!(
        r#"#!/bin/sh
# werma-hook-version: {HOOK_VERSION}
# Auto-installed by werma — enforce cargo fmt before commit
if [ -d "engine" ]; then
    cargo fmt --check --manifest-path engine/Cargo.toml 2>&1
    if [ $? -ne 0 ]; then
        echo ""
        echo "ERROR: cargo fmt check failed. Run: cargo fmt --manifest-path engine/Cargo.toml"
        echo ""
        exit 1
    fi

    cargo clippy --manifest-path engine/Cargo.toml -- -D warnings 2>&1
    if [ $? -ne 0 ]; then
        echo ""
        echo "ERROR: cargo clippy failed. Fix all warnings before committing."
        echo ""
        exit 1
    fi

    cargo test --manifest-path engine/Cargo.toml 2>&1
    if [ $? -ne 0 ]; then
        echo ""
        echo "ERROR: cargo test failed. Fix all test failures before committing."
        echo ""
        exit 1
    fi
fi
"#
    );

    // Check if existing hook is current version — overwrite if stale
    if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path).unwrap_or_default();
        let version_marker = format!("# werma-hook-version: {HOOK_VERSION}");
        if existing.contains(&version_marker) {
            return Ok(());
        }
        // Stale or unversioned hook — overwrite with current version
    }

    std::fs::write(&hook_path, hook_content)
        .with_context(|| format!("writing pre-commit hook at {}", hook_path.display()))?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).with_context(
            || {
                format!(
                    "setting pre-commit hook executable at {}",
                    hook_path.display()
                )
            },
        )?;
    }

    Ok(())
}

/// Check if a path is inside a `.trees/` directory (i.e., a worktree, not the main checkout).
/// Used as a safety guard to prevent write tasks from running on the main repo.
pub fn is_inside_worktree(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".trees")
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

/// Extract a Linear identifier (e.g. RIG-42, FAT-36, AR-10) from the beginning of a string.
/// Matches patterns like "RIG-42 ...", "  FAT-36 ...", "[AR-10] ..."
/// The prefix must be 1+ uppercase ASCII letters followed by a hyphen and 1+ digits.
pub fn extract_linear_id_prefix(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let trimmed = trimmed.strip_prefix('[').unwrap_or(trimmed);

    // Find the uppercase prefix (e.g. "RIG", "FAT", "AR")
    let prefix_end = trimmed.find(|c: char| !c.is_ascii_uppercase()).unwrap_or(0);
    if prefix_end == 0 {
        return None;
    }

    // Must be followed by a hyphen
    let rest = &trimmed[prefix_end..];
    let rest = rest.strip_prefix('-')?;

    // Collect digits
    let digit_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digit_end == 0 {
        return None;
    }

    // prefix + "-" + digits
    let id_len = prefix_end + 1 + digit_end;
    Some(trimmed[..id_len].to_string())
}

/// Extract a Linear identifier (e.g. RIG-42, FAT-36) from anywhere in a prompt string.
/// Matches the first occurrence of `[A-Z]+-\d+`.
fn extract_linear_id(prompt: &str) -> Option<String> {
    // Scan for uppercase letters followed by '-' and digits
    let bytes = prompt.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Find start of uppercase sequence
        if !bytes[i].is_ascii_uppercase() {
            i += 1;
            continue;
        }

        let prefix_start = i;
        while i < len && bytes[i].is_ascii_uppercase() {
            i += 1;
        }
        // Must be followed by '-'
        if i >= len || bytes[i] != b'-' {
            continue;
        }
        i += 1; // skip '-'

        // Must have at least one digit
        let digit_start = i;
        while i < len && bytes[i].is_ascii_digit() {
            i += 1;
        }

        if i > digit_start {
            return Some(prompt[prefix_start..i].to_uppercase());
        }
    }

    None
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
        }
    }

    // --- is_inside_worktree ---

    #[test]
    fn is_inside_worktree_positive() {
        assert!(is_inside_worktree(Path::new(
            "/home/user/project/.trees/feat--RIG-42-thing"
        )));
        assert!(is_inside_worktree(Path::new(
            "/Users/ar/projects/rigpa/werma/.trees/fix--RIG-99-bug"
        )));
    }

    #[test]
    fn is_inside_worktree_negative() {
        assert!(!is_inside_worktree(Path::new("/home/user/project")));
        assert!(!is_inside_worktree(Path::new(
            "/home/user/project/src/main.rs"
        )));
        assert!(!is_inside_worktree(Path::new("/tmp")));
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
            name.starts_with("feat/RIG-42-"),
            "expected feat/RIG-42- prefix, got: {name}"
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

    #[test]
    fn branch_name_fix_type_from_prompt() {
        let task = test_task(
            "pipeline-engineer",
            "issue-abc-123",
            "RIG-169 fix: engineer agent creates branches from stale main",
        );
        let name = generate_branch_name(&task);
        assert!(
            name.starts_with("fix/RIG-169-"),
            "expected fix/ prefix for fix: prompt, got: {name}"
        );
    }

    #[test]
    fn branch_name_refactor_pipeline() {
        let task = test_task(
            "refactor",
            "issue-abc-123",
            "[RIG-100] Cleanup module structure",
        );
        let name = generate_branch_name(&task);
        assert!(
            name.starts_with("refactor/RIG-100-"),
            "expected refactor/ prefix, got: {name}"
        );
    }

    #[test]
    fn branch_name_reviewer_type() {
        let task = test_task(
            "pipeline-reviewer",
            "issue-abc-123",
            "[RIG-50] Review the changes",
        );
        let name = generate_branch_name(&task);
        assert!(
            name.starts_with("review/RIG-50-"),
            "expected review/ prefix, got: {name}"
        );
    }

    // --- derive_branch_type ---

    #[test]
    fn derive_type_engineer_default_feat() {
        let task = test_task("pipeline-engineer", "x", "RIG-42 Add new feature");
        assert_eq!(derive_branch_type(&task), "feat");
    }

    #[test]
    fn derive_type_engineer_fix_from_prompt() {
        let task = test_task("pipeline-engineer", "x", "RIG-42 fix: broken thing");
        assert_eq!(derive_branch_type(&task), "fix");
    }

    #[test]
    fn derive_type_refactor() {
        let task = test_task("refactor", "x", "Cleanup stuff");
        assert_eq!(derive_branch_type(&task), "refactor");
    }

    #[test]
    fn derive_type_reviewer() {
        let task = test_task("pipeline-reviewer", "x", "Review code");
        assert_eq!(derive_branch_type(&task), "review");
    }

    // --- pipeline branch naming (deterministic for re-spawn) ---

    #[test]
    fn branch_name_pipeline_engineer_deterministic() {
        let mut task = test_task(
            "pipeline-engineer",
            "issue-abc-123",
            "# Pipeline: Engineer Stage\nLinear issue: RIG-171\n\nImplement feature X",
        );
        task.pipeline_stage = "engineer".to_string();
        let name = generate_branch_name(&task);
        assert_eq!(name, "feat/RIG-171-pipeline-engineer-stage");
    }

    #[test]
    fn branch_name_pipeline_engineer_fat_prefix() {
        let mut task = test_task(
            "pipeline-engineer",
            "FAT-36",
            "# Pipeline: Engineer Stage\nLinear issue: FAT-36\n\nAdd per-symbol metrics",
        );
        task.pipeline_stage = "engineer".to_string();
        let name = generate_branch_name(&task);
        assert_eq!(name, "feat/FAT-36-pipeline-engineer-stage");
    }

    #[test]
    fn branch_name_pipeline_engineer_respawn_same_branch() {
        let mut task1 = test_task(
            "pipeline-engineer",
            "issue-abc-123",
            "# Pipeline: Engineer Stage\nLinear issue: RIG-171\n\nImplement feature X",
        );
        task1.pipeline_stage = "engineer".to_string();

        let mut task2 = test_task(
            "pipeline-engineer",
            "issue-abc-123",
            "# Pipeline: Engineer Stage (Revision)\nLinear issue: RIG-171\n\n## Reviewer Feedback\n- blocker: no tests",
        );
        task2.id = "20260310-002".to_string();
        task2.pipeline_stage = "engineer".to_string();

        assert_eq!(generate_branch_name(&task1), generate_branch_name(&task2));
    }

    #[test]
    fn branch_name_pipeline_fat_respawn_same_branch() {
        let mut task1 = test_task(
            "pipeline-engineer",
            "FAT-36",
            "# Pipeline: Engineer Stage\nLinear issue: FAT-36\n\nImplement feature",
        );
        task1.pipeline_stage = "engineer".to_string();

        let mut task2 = test_task(
            "pipeline-engineer",
            "FAT-36",
            "# Pipeline: Engineer Stage (Revision)\nLinear issue: FAT-36\n\n## Rejection",
        );
        task2.id = "20260310-002".to_string();
        task2.pipeline_stage = "engineer".to_string();

        assert_eq!(generate_branch_name(&task1), generate_branch_name(&task2));
        assert_eq!(
            generate_branch_name(&task1),
            "feat/FAT-36-pipeline-engineer-stage"
        );
    }

    // --- extract_linear_id_prefix ---

    #[test]
    fn extract_linear_id_prefix_rig() {
        assert_eq!(
            extract_linear_id_prefix("RIG-83 do stuff"),
            Some("RIG-83".to_string())
        );
        assert_eq!(
            extract_linear_id_prefix("  RIG-42 something"),
            Some("RIG-42".to_string())
        );
        assert_eq!(
            extract_linear_id_prefix("[RIG-100] title"),
            Some("RIG-100".to_string())
        );
    }

    #[test]
    fn extract_linear_id_prefix_fat() {
        assert_eq!(
            extract_linear_id_prefix("FAT-36 order book fix"),
            Some("FAT-36".to_string())
        );
        assert_eq!(
            extract_linear_id_prefix("[FAT-42] fathom feature"),
            Some("FAT-42".to_string())
        );
    }

    #[test]
    fn extract_linear_id_prefix_other_teams() {
        assert_eq!(
            extract_linear_id_prefix("AR-10 personal task"),
            Some("AR-10".to_string())
        );
        assert_eq!(
            extract_linear_id_prefix("ABC-999 something"),
            Some("ABC-999".to_string())
        );
    }

    #[test]
    fn extract_linear_id_prefix_not_at_start() {
        assert_eq!(
            extract_linear_id_prefix("fix the thing RIG-99 mentioned"),
            None
        );
        assert_eq!(extract_linear_id_prefix("no issue here"), None);
        assert_eq!(extract_linear_id_prefix("RIG- no digits"), None);
        assert_eq!(extract_linear_id_prefix("FAT- no digits"), None);
    }

    // --- extract_linear_id ---

    #[test]
    fn extract_linear_id_rig() {
        assert_eq!(
            extract_linear_id("[RIG-42] Something"),
            Some("RIG-42".to_string())
        );
        assert_eq!(
            extract_linear_id("RIG-123 stuff"),
            Some("RIG-123".to_string())
        );
    }

    #[test]
    fn extract_linear_id_fat() {
        assert_eq!(
            extract_linear_id("# Pipeline: Engineer Stage\nLinear issue: FAT-36\n"),
            Some("FAT-36".to_string())
        );
        assert_eq!(
            extract_linear_id("[FAT-42] Add per-symbol metrics"),
            Some("FAT-42".to_string())
        );
    }

    #[test]
    fn extract_linear_id_other_teams() {
        assert_eq!(
            extract_linear_id("Issue AR-10 needs work"),
            Some("AR-10".to_string())
        );
    }

    #[test]
    fn extract_linear_id_not_found() {
        assert_eq!(extract_linear_id("no issue id here"), None);
        assert_eq!(extract_linear_id("RIG- no digits"), None);
        assert_eq!(extract_linear_id("FAT- no digits"), None);
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

    /// Helper: create a test git repo with an "origin" remote.
    fn init_repo_with_origin(dir: &tempfile::TempDir) -> PathBuf {
        let repo_dir = dir.path().to_path_buf();
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        let origin_dir = dir.path().join("origin.git");
        Command::new("git")
            .args([
                "clone",
                "--bare",
                &repo_dir.to_string_lossy(),
                &origin_dir.to_string_lossy(),
            ])
            .output()
            .unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", &origin_dir.to_string_lossy()])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        repo_dir
    }

    #[test]
    fn setup_worktree_creates_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = init_repo_with_origin(&dir);

        let branch = "feat/RIG-99-test-branch";

        // First call: creates worktree
        let path = setup_worktree(&repo_dir, branch).unwrap();
        assert!(path.exists());
        assert!(path.ends_with(".trees/feat--RIG-99-test-branch"));

        // Second call: returns same path (resume)
        let path2 = setup_worktree(&repo_dir, branch).unwrap();
        assert_eq!(path, path2);

        // Cleanup
        cleanup_worktree(&repo_dir, branch).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn setup_worktree_installs_pre_commit_hook() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = init_repo_with_origin(&dir);

        let branch = "feat/RIG-200-hook-test";
        let worktree_path = setup_worktree(&repo_dir, branch).unwrap();

        // Resolve the git dir for this worktree and check hook exists
        let output = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&worktree_path)
            .output()
            .unwrap();
        let git_dir_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let git_dir = if Path::new(&git_dir_raw).is_absolute() {
            PathBuf::from(&git_dir_raw)
        } else {
            worktree_path.join(&git_dir_raw)
        };

        let hook_path = git_dir.join("hooks").join("pre-commit");
        assert!(hook_path.exists(), "pre-commit hook should be installed");

        let content = std::fs::read_to_string(&hook_path).unwrap();
        assert!(
            content.contains("cargo fmt --check"),
            "hook should run cargo fmt"
        );
        assert!(
            content.contains("Auto-installed by werma"),
            "hook should have werma marker"
        );

        // Verify executable permission
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&hook_path).unwrap().permissions();
            assert!(perms.mode() & 0o111 != 0, "hook should be executable");
        }

        // Second call should be idempotent (not overwrite)
        let content_before = std::fs::read_to_string(&hook_path).unwrap();
        setup_worktree(&repo_dir, branch).unwrap();
        let content_after = std::fs::read_to_string(&hook_path).unwrap();
        assert_eq!(
            content_before, content_after,
            "hook should not be overwritten on resume"
        );

        cleanup_worktree(&repo_dir, branch).unwrap();
    }

    #[test]
    fn setup_worktree_falls_back_to_head_without_origin() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path();

        // Init a git repo with NO origin remote — origin/main will be unresolvable
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        let branch = "feat/RIG-100-no-origin";

        // Should succeed by falling back to HEAD (no origin remote at all)
        let path = setup_worktree(repo_dir, branch).unwrap();
        assert!(path.exists());
        assert!(path.ends_with(".trees/feat--RIG-100-no-origin"));

        // Cleanup
        cleanup_worktree(repo_dir, branch).unwrap();
    }

    #[test]
    fn cleanup_nonexistent_worktree_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        // No error on missing worktree
        assert!(cleanup_worktree(dir.path(), "nonexistent").is_ok());
    }
}
