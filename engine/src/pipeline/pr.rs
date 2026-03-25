use anyhow::{Context, Result};

use crate::traits::CommandRunner;

use super::helpers::resolve_home;

/// Check if a merged PR exists for the given Linear issue identifier in the repo.
/// Uses `gh pr list --search` to find merged PRs mentioning the issue.
pub(crate) fn is_pr_merged_for_issue(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    identifier: &str,
) -> bool {
    pr_exists_for_issue(cmd, working_dir, identifier, "merged")
}

/// Check if an open (unmerged) PR exists for the given Linear issue identifier.
pub(crate) fn has_open_pr_for_issue(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    identifier: &str,
) -> bool {
    pr_exists_for_issue(cmd, working_dir, identifier, "open")
}

/// Check if a PR exists for the given Linear issue identifier in a specific state.
///
/// Uses `--head` to filter by branch name containing the identifier, avoiding stale
/// PRs from other issues that happen to mention the same identifier in their body (RIG-234).
pub(crate) fn pr_exists_for_issue(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    identifier: &str,
    state: &str,
) -> bool {
    let working_dir = resolve_home(working_dir);

    // First, try to get the branch name that would match this issue.
    // Branch naming convention: type/RIG-XX-short-name or feat/RIG-XX-...
    // We search for PRs whose headRefName contains the identifier.
    let output = cmd.run(
        "gh",
        &[
            "pr",
            "list",
            "--search",
            identifier,
            "--state",
            state,
            "--json",
            "number,headRefName",
            "--limit",
            "10",
        ],
        Some(&working_dir),
    );

    match output {
        Ok(o) if o.success => {
            let text = o.stdout_str();
            if text == "[]" || text.is_empty() {
                return false;
            }
            // Filter: only count PRs where headRefName contains the identifier.
            // This prevents stale PR matches from other issues (RIG-234 fix).
            // If headRefName is absent (API didn't include it), assume match (safe fallback).
            let identifier_lower = identifier.to_lowercase();
            if let Ok(prs) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                prs.iter().any(|pr| {
                    match pr["headRefName"].as_str() {
                        Some(branch) => branch.to_lowercase().contains(&identifier_lower),
                        // headRefName absent → fall back to non-empty check (backward compat)
                        None => true,
                    }
                })
            } else {
                // JSON parse failed — fall back to non-empty check (old behavior)
                true
            }
        }
        _ => false,
    }
}

/// Automatically create a GitHub PR from the engineer's worktree branch.
///
/// Returns the PR URL if successful, or None if:
/// - On main/master branch (safety)
/// - No commits ahead of main (nothing to PR)
/// - PR creation fails (logged but non-fatal)
pub(crate) fn auto_create_pr(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    linear_issue_id: &str,
    task_id: &str,
) -> Result<Option<String>> {
    let working_dir = resolve_home(working_dir);

    // 1. Get current branch
    let branch_output = cmd
        .run("git", &["branch", "--show-current"], Some(&working_dir))
        .context("git branch --show-current")?;
    let branch_name = branch_output.stdout_str();

    // 2. Safety: never PR from main/master or empty branch
    if branch_name.is_empty() || branch_name == "main" || branch_name == "master" {
        return Ok(None);
    }

    // 3. Check if there are commits ahead of main
    let log_output = cmd
        .run(
            "git",
            &["log", "origin/main..HEAD", "--oneline"],
            Some(&working_dir),
        )
        .context("git log origin/main..HEAD")?;
    let log_text = log_output.stdout_str();
    if log_text.is_empty() {
        eprintln!("auto-PR: no commits ahead of main on branch {branch_name}, skipping");
        return Ok(None);
    }

    // 4. Push branch (ignore errors if already up-to-date)
    let push_output = cmd
        .run(
            "git",
            &["push", "-u", "origin", &branch_name],
            Some(&working_dir),
        )
        .context("git push")?;
    if !push_output.success {
        let stderr = push_output.stderr_str();
        eprintln!("auto-PR: push failed: {stderr}");
        return Ok(None);
    }

    // 5. Check if PR already exists for this branch
    let existing_pr = cmd
        .run(
            "gh",
            &["pr", "view", "--json", "url", "-q", ".url"],
            Some(&working_dir),
        )
        .context("gh pr view")?;
    if existing_pr.success {
        let url = existing_pr.stdout_str();
        if !url.is_empty() {
            return Ok(Some(url));
        }
    }

    // 6. Create PR
    let pr_title = format!("{linear_issue_id} feat: implementation");
    let pr_body = format!(
        "## Summary\nPipeline engineer task `{task_id}`.\n\n\
         Linear: https://linear.app/rigpa/issue/{linear_issue_id}",
    );

    let output = cmd
        .run(
            "gh",
            &[
                "pr",
                "create",
                "--title",
                &pr_title,
                "--body",
                &pr_body,
                "--label",
                "ai-generated",
            ],
            Some(&working_dir),
        )
        .context("gh pr create")?;

    if output.success {
        let url = output.stdout_str();
        Ok(Some(url))
    } else {
        let stderr = output.stderr_str();
        eprintln!("auto-PR failed: {stderr}");
        Ok(None)
    }
}

/// Post a comment on a GitHub PR for the given working directory.
///
/// Finds the PR number from the current branch, then posts the comment body.
/// Returns Ok(true) if comment was posted, Ok(false) if no PR found, Err on failure.
pub(crate) fn post_pr_comment(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    comment_body: &str,
) -> Result<bool> {
    let working_dir = resolve_home(working_dir);

    // Find PR number for the current branch
    let pr_output = cmd
        .run(
            "gh",
            &["pr", "view", "--json", "number", "-q", ".number"],
            Some(&working_dir),
        )
        .context("gh pr view")?;

    if !pr_output.success {
        return Ok(false);
    }

    let pr_num = pr_output.stdout_str();
    if pr_num.is_empty() {
        return Ok(false);
    }

    // Post comment
    let result = cmd
        .run(
            "gh",
            &["pr", "comment", &pr_num, "--body", comment_body],
            Some(&working_dir),
        )
        .context("gh pr comment")?;

    if !result.success {
        let stderr = result.stderr_str();
        eprintln!("post_pr_comment: gh pr comment failed: {stderr}");
        return Ok(false);
    }

    Ok(true)
}

/// Find the open PR number for a given Linear issue identifier.
///
/// Returns `Some(number)` if an open PR is found whose branch contains the identifier.
pub(crate) fn find_pr_number_for_issue(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    identifier: &str,
) -> Option<String> {
    let working_dir = resolve_home(working_dir);

    let output = cmd.run(
        "gh",
        &[
            "pr",
            "list",
            "--search",
            identifier,
            "--state",
            "open",
            "--json",
            "number,headRefName",
            "--limit",
            "10",
        ],
        Some(&working_dir),
    );

    match output {
        Ok(o) if o.success => {
            let text = o.stdout_str();
            if text == "[]" || text.is_empty() {
                return None;
            }
            let identifier_lower = identifier.to_lowercase();
            if let Ok(prs) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                prs.iter().find_map(|pr| {
                    let matches = match pr["headRefName"].as_str() {
                        Some(branch) => branch.to_lowercase().contains(&identifier_lower),
                        None => true,
                    };
                    if matches {
                        pr["number"].as_i64().map(|n| n.to_string())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Merge a PR by number using squash merge with branch deletion.
///
/// Uses `gh pr merge --squash --delete-branch --auto` to wait for CI.
/// Returns Ok(true) if merge succeeded, Ok(false) if merge failed (e.g., conflicts).
pub(crate) fn merge_pr(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    pr_number: &str,
) -> Result<bool> {
    let working_dir = resolve_home(working_dir);

    let result = cmd
        .run(
            "gh",
            &[
                "pr",
                "merge",
                pr_number,
                "--squash",
                "--delete-branch",
                "--auto",
            ],
            Some(&working_dir),
        )
        .context("gh pr merge")?;

    if result.success {
        eprintln!("[DEPLOY] PR #{pr_number} merge initiated (--auto, waiting for CI)");
        Ok(true)
    } else {
        let stderr = result.stderr_str();
        eprintln!("[DEPLOY] PR #{pr_number} merge failed: {stderr}");
        Ok(false)
    }
}

/// Derive a short title from a GitHub PR URL (e.g. "PR #42").
pub(crate) fn pr_title_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(|n| format!("PR #{n}"))
        .unwrap_or_else(|| "Pull Request".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::fakes::FakeCommandRunner;

    #[test]
    fn pr_title_from_url_extracts_number() {
        assert_eq!(
            pr_title_from_url("https://github.com/org/repo/pull/42"),
            "PR #42"
        );
    }

    #[test]
    fn pr_title_from_url_fallback() {
        assert_eq!(
            pr_title_from_url("https://github.com/org/repo/pull/"),
            "Pull Request"
        );
    }

    #[test]
    fn pr_exists_filters_by_branch_name_when_present() {
        let cmd = FakeCommandRunner::new();
        // Return a PR whose headRefName is present but does NOT contain the identifier
        cmd.push_success(r#"[{"number":1,"headRefName":"feat/RIG-999-other-issue"}]"#);

        let found = pr_exists_for_issue(&cmd, "/tmp", "RIG-100", "open");
        assert!(!found, "should not match PR from a different issue branch");
    }

    #[test]
    fn pr_exists_falls_back_when_no_branch_name() {
        let cmd = FakeCommandRunner::new();
        // Return a PR without headRefName — should fall back to non-empty check (match)
        cmd.push_success(r#"[{"number":42}]"#);

        let found = pr_exists_for_issue(&cmd, "/tmp", "RIG-100", "open");
        assert!(
            found,
            "should match when headRefName is absent (backward compat)"
        );
    }

    #[test]
    fn pr_exists_matches_correct_branch() {
        let cmd = FakeCommandRunner::new();
        // Return a PR whose headRefName contains the identifier
        cmd.push_success(r#"[{"number":5,"headRefName":"feat/rig-100-my-feature"}]"#);

        let found = pr_exists_for_issue(&cmd, "/tmp", "RIG-100", "open");
        assert!(
            found,
            "should match PR whose branch contains the identifier"
        );
    }

    #[test]
    fn pr_exists_empty_result() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("[]");

        let found = pr_exists_for_issue(&cmd, "/tmp", "RIG-100", "open");
        assert!(!found);
    }

    // ─── post_pr_comment ─────────────────────────────────────────────────

    #[test]
    fn post_pr_comment_success() {
        let cmd = FakeCommandRunner::new();
        // gh pr view returns PR number
        cmd.push_success("42");
        // gh pr comment succeeds
        cmd.push_success("");

        let result = post_pr_comment(&cmd, "/tmp", "Great code!").unwrap();
        assert!(result, "should return true on success");

        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "gh");
        assert_eq!(calls[1].0, "gh");
        assert!(calls[1].1.contains(&"comment".to_string()));
        assert!(calls[1].1.contains(&"42".to_string()));
        assert!(calls[1].1.contains(&"Great code!".to_string()));
    }

    #[test]
    fn post_pr_comment_no_pr() {
        let cmd = FakeCommandRunner::new();
        // gh pr view fails (no PR for current branch)
        cmd.push_failure("no pull requests found");

        let result = post_pr_comment(&cmd, "/tmp", "Review text").unwrap();
        assert!(!result, "should return false when no PR found");
    }

    #[test]
    fn post_pr_comment_empty_pr_number() {
        let cmd = FakeCommandRunner::new();
        // gh pr view returns empty (edge case)
        cmd.push_success("");

        let result = post_pr_comment(&cmd, "/tmp", "Review text").unwrap();
        assert!(!result, "should return false on empty PR number");
    }

    // ─── find_pr_number_for_issue ───────────────────────────────────────

    #[test]
    fn find_pr_number_matches_branch() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(r#"[{"number":42,"headRefName":"feat/rig-100-my-feature"}]"#);

        let result = find_pr_number_for_issue(&cmd, "/tmp", "RIG-100");
        assert_eq!(result, Some("42".to_string()));
    }

    #[test]
    fn find_pr_number_no_match() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(r#"[{"number":99,"headRefName":"feat/rig-999-other"}]"#);

        let result = find_pr_number_for_issue(&cmd, "/tmp", "RIG-100");
        assert!(result.is_none());
    }

    #[test]
    fn find_pr_number_empty_list() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("[]");

        let result = find_pr_number_for_issue(&cmd, "/tmp", "RIG-100");
        assert!(result.is_none());
    }

    #[test]
    fn find_pr_number_fallback_no_branch() {
        let cmd = FakeCommandRunner::new();
        // No headRefName → fallback to match
        cmd.push_success(r#"[{"number":7}]"#);

        let result = find_pr_number_for_issue(&cmd, "/tmp", "RIG-100");
        assert_eq!(result, Some("7".to_string()));
    }

    // ─── merge_pr ───────────────────────────────────────────────────────

    #[test]
    fn merge_pr_success() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("Merged");

        let result = merge_pr(&cmd, "/tmp", "42").unwrap();
        assert!(result, "should return true on successful merge");

        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "gh");
        assert!(calls[0].1.contains(&"merge".to_string()));
        assert!(calls[0].1.contains(&"42".to_string()));
        assert!(calls[0].1.contains(&"--squash".to_string()));
        assert!(calls[0].1.contains(&"--auto".to_string()));
        // Must never use --admin
        assert!(
            !calls[0].1.contains(&"--admin".to_string()),
            "merge must use --auto, never --admin"
        );
    }

    #[test]
    fn merge_pr_failure() {
        let cmd = FakeCommandRunner::new();
        cmd.push_failure("merge conflicts");

        let result = merge_pr(&cmd, "/tmp", "42").unwrap();
        assert!(!result, "should return false on merge failure");
    }
}
