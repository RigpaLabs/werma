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

    // 4. Push branch — failure is retriable (network, auth), not a skip.
    let push_output = cmd
        .run(
            "git",
            &["push", "-u", "origin", &branch_name],
            Some(&working_dir),
        )
        .context("git push")?;
    if !push_output.success {
        let stderr = push_output.stderr_str();
        return Err(anyhow::anyhow!(
            "git push failed for {branch_name}: {stderr}"
        ));
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
    let pr_body = if let Ok(ws) = std::env::var("WERMA_LINEAR_WORKSPACE") {
        format!(
            "## Summary\nPipeline engineer task `{task_id}`.\n\n\
             Linear: https://linear.app/{ws}/issue/{linear_issue_id}",
        )
    } else {
        eprintln!(
            "auto-PR: WERMA_LINEAR_WORKSPACE not set — PR body will not include Linear link. \
             Set it to your Linear workspace slug (e.g. WERMA_LINEAR_WORKSPACE=myorg) to enable."
        );
        format!(
            "## Summary\nPipeline engineer task `{task_id}`.\n\n\
             Issue: {linear_issue_id}",
        )
    };

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
        Err(anyhow::anyhow!(
            "gh pr create failed for {linear_issue_id}: {stderr}"
        ))
    }
}

/// Post a pull request review on GitHub using `gh pr review`.
///
/// Uses the proper PR review endpoint (not issue comments), so the review appears
/// in the "Reviews" section on GitHub. The `review_event` parameter controls the
/// review type: "comment", "approve", or "request-changes".
///
/// Returns `Ok(())` on success. Returns `Err` if:
/// - No PR is found for the current branch (caller should retry — PR may not exist yet)
/// - The `gh pr review` command fails (API error, auth issue, etc.)
///
/// RIG-318: replaces the old `post_pr_comment` which used `gh pr comment` (issue
/// comment endpoint), causing reviews to silently not appear in GitHub's Reviews tab.
pub(crate) fn post_pr_review(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    review_body: &str,
    review_event: &str,
) -> Result<()> {
    let working_dir = resolve_home(working_dir);

    // Find PR number for the current branch
    let pr_output = cmd
        .run(
            "gh",
            &["pr", "view", "--json", "number", "-q", ".number"],
            Some(&working_dir),
        )
        .context("gh pr view")?;

    if !pr_output.success || pr_output.stdout_str().is_empty() {
        return Err(anyhow::anyhow!(
            "no PR found for branch in {}: {}",
            working_dir.display(),
            pr_output.stderr_str()
        ));
    }

    let pr_num = pr_output.stdout_str();

    // Map review_event to gh pr review flags
    let event_flag = match review_event {
        "approve" => "--approve",
        "request-changes" => "--request-changes",
        _ => "--comment", // default: COMMENT
    };

    // Post review using `gh pr review` — this hits the correct GitHub PR reviews API
    let result = cmd
        .run(
            "gh",
            &["pr", "review", &pr_num, event_flag, "--body", review_body],
            Some(&working_dir),
        )
        .context("gh pr review")?;

    if !result.success {
        return Err(anyhow::anyhow!(
            "gh pr review failed for PR #{pr_num}: {}",
            result.stderr_str()
        ));
    }

    eprintln!("[pr] posted {review_event} review on PR #{pr_num}");
    Ok(())
}

/// Check the latest review state on the open PR for a given issue.
///
/// Uses `gh pr view` to get the latest review decision. Returns the verdict
/// string ("APPROVED" or "REJECTED") if a review was posted, or None if no
/// review exists or no PR is found.
///
/// This is used as a fallback when the reviewer agent produces empty `result`
/// (RIG-309): the agent may have posted a review via tool calls but the final
/// text was empty, so we check GitHub directly.
pub(crate) fn get_pr_review_verdict(
    cmd: &dyn CommandRunner,
    working_dir: &str,
    identifier: &str,
) -> Option<String> {
    let working_dir = resolve_home(working_dir);

    // Find open PR for this issue
    let pr_output = cmd
        .run(
            "gh",
            &[
                "pr",
                "list",
                "--search",
                identifier,
                "--state",
                "open",
                "--json",
                "number,headRefName,reviewDecision",
                "--limit",
                "5",
            ],
            Some(&working_dir),
        )
        .ok()?;

    if !pr_output.success {
        return None;
    }

    let text = pr_output.stdout_str();
    let prs: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;
    let identifier_lower = identifier.to_lowercase();

    // Find the PR whose branch matches the issue identifier
    for pr in &prs {
        let branch_matches = pr["headRefName"]
            .as_str()
            .map(|b| b.to_lowercase().contains(&identifier_lower))
            .unwrap_or(true); // fallback: assume match if no branch info

        if !branch_matches {
            continue;
        }

        // reviewDecision can be: "APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", or null
        if let Some(decision) = pr["reviewDecision"].as_str() {
            return match decision {
                "APPROVED" => Some("APPROVED".to_string()),
                "CHANGES_REQUESTED" => Some("REJECTED".to_string()),
                _ => None, // REVIEW_REQUIRED or other states = no actionable verdict
            };
        }
    }

    None
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

    // ─── RIG-306: merged + open PR interaction ─────────────────────────

    #[test]
    fn merged_pr_exists_but_open_pr_also_exists_should_not_skip() {
        let cmd = FakeCommandRunner::new();
        // is_pr_merged_for_issue: returns merged PR
        cmd.push_success(r#"[{"number":155,"headRefName":"feat/rig-281-old-work"}]"#);
        // has_open_pr_for_issue: returns open PR (re-work)
        cmd.push_success(r#"[{"number":160,"headRefName":"feat/rig-281-new-work"}]"#);

        let merged = is_pr_merged_for_issue(&cmd, "/tmp", "RIG-281");
        let has_open = has_open_pr_for_issue(&cmd, "/tmp", "RIG-281");
        // Logic: merged && !has_open → should NOT skip to Done
        assert!(merged);
        assert!(has_open);
        assert!(
            !(merged && !has_open),
            "should not skip when open PR exists alongside merged PR"
        );
    }

    #[test]
    fn merged_pr_exists_and_no_open_pr_should_skip() {
        let cmd = FakeCommandRunner::new();
        // is_pr_merged_for_issue: returns merged PR
        cmd.push_success(r#"[{"number":155,"headRefName":"feat/rig-281-old-work"}]"#);
        // has_open_pr_for_issue: no open PRs
        cmd.push_success("[]");

        let merged = is_pr_merged_for_issue(&cmd, "/tmp", "RIG-281");
        let has_open = has_open_pr_for_issue(&cmd, "/tmp", "RIG-281");
        // Logic: merged && !has_open → should skip to Done
        assert!(merged);
        assert!(!has_open);
        assert!(
            merged && !has_open,
            "should skip to Done when only merged PR exists"
        );
    }

    // ─── post_pr_review (RIG-318) ─────────────────────────────────────────

    #[test]
    fn post_pr_review_success_comment() {
        let cmd = FakeCommandRunner::new();
        // gh pr view returns PR number
        cmd.push_success("42");
        // gh pr review succeeds
        cmd.push_success("");

        post_pr_review(&cmd, "/tmp", "Great code!", "comment").unwrap();

        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "gh");
        assert!(calls[1].1.contains(&"review".to_string()));
        assert!(calls[1].1.contains(&"42".to_string()));
        assert!(calls[1].1.contains(&"--comment".to_string()));
        assert!(calls[1].1.contains(&"Great code!".to_string()));
    }

    #[test]
    fn post_pr_review_success_approve() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("42");
        cmd.push_success("");

        post_pr_review(&cmd, "/tmp", "LGTM!", "approve").unwrap();

        let calls = cmd.calls.borrow();
        assert!(calls[1].1.contains(&"--approve".to_string()));
    }

    #[test]
    fn post_pr_review_success_request_changes() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("42");
        cmd.push_success("");

        post_pr_review(&cmd, "/tmp", "Needs fixes", "request-changes").unwrap();

        let calls = cmd.calls.borrow();
        assert!(calls[1].1.contains(&"--request-changes".to_string()));
    }

    #[test]
    fn post_pr_review_no_pr_returns_error() {
        let cmd = FakeCommandRunner::new();
        // gh pr view fails (no PR for current branch)
        cmd.push_failure("no pull requests found");

        let result = post_pr_review(&cmd, "/tmp", "Review text", "comment");
        assert!(result.is_err(), "should return Err when no PR found");
        assert!(
            result.unwrap_err().to_string().contains("no PR found"),
            "error should mention no PR found"
        );
    }

    #[test]
    fn post_pr_review_empty_pr_number_returns_error() {
        let cmd = FakeCommandRunner::new();
        // gh pr view returns empty (edge case)
        cmd.push_success("");

        let result = post_pr_review(&cmd, "/tmp", "Review text", "comment");
        assert!(result.is_err(), "should return Err on empty PR number");
    }

    #[test]
    fn post_pr_review_api_error_returns_error() {
        let cmd = FakeCommandRunner::new();
        // gh pr view returns PR number
        cmd.push_success("42");
        // gh pr review fails (API error)
        cmd.push_failure("HTTP 422: Validation Failed");

        let result = post_pr_review(&cmd, "/tmp", "Review text", "comment");
        assert!(result.is_err(), "should return Err on API failure");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("gh pr review failed"),
            "error should mention gh pr review failure"
        );
    }

    // ─── RIG-309: get_pr_review_verdict ────────────────────────────────

    #[test]
    fn get_pr_review_verdict_approved() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            r#"[{"number":42,"headRefName":"feat/rig-309-fix-reviewer","reviewDecision":"APPROVED"}]"#,
        );

        let verdict = get_pr_review_verdict(&cmd, "/tmp", "RIG-309");
        assert_eq!(verdict, Some("APPROVED".to_string()));
    }

    #[test]
    fn get_pr_review_verdict_changes_requested() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            r#"[{"number":42,"headRefName":"feat/rig-309-fix","reviewDecision":"CHANGES_REQUESTED"}]"#,
        );

        let verdict = get_pr_review_verdict(&cmd, "/tmp", "RIG-309");
        assert_eq!(verdict, Some("REJECTED".to_string()));
    }

    #[test]
    fn get_pr_review_verdict_no_review() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            r#"[{"number":42,"headRefName":"feat/rig-309-fix","reviewDecision":"REVIEW_REQUIRED"}]"#,
        );

        let verdict = get_pr_review_verdict(&cmd, "/tmp", "RIG-309");
        assert_eq!(verdict, None);
    }

    #[test]
    fn get_pr_review_verdict_null_decision() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            r#"[{"number":42,"headRefName":"feat/rig-309-fix","reviewDecision":null}]"#,
        );

        let verdict = get_pr_review_verdict(&cmd, "/tmp", "RIG-309");
        assert_eq!(verdict, None);
    }

    #[test]
    fn get_pr_review_verdict_no_matching_pr() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            r#"[{"number":42,"headRefName":"feat/rig-999-other","reviewDecision":"APPROVED"}]"#,
        );

        let verdict = get_pr_review_verdict(&cmd, "/tmp", "RIG-309");
        assert_eq!(verdict, None);
    }

    #[test]
    fn get_pr_review_verdict_no_prs() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("[]");

        let verdict = get_pr_review_verdict(&cmd, "/tmp", "RIG-309");
        assert_eq!(verdict, None);
    }

    #[test]
    fn post_pr_review_default_event_is_comment() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("42");
        cmd.push_success("");

        // Unknown event falls back to --comment
        post_pr_review(&cmd, "/tmp", "Review text", "unknown_event").unwrap();

        let calls = cmd.calls.borrow();
        assert!(calls[1].1.contains(&"--comment".to_string()));
    }
}
