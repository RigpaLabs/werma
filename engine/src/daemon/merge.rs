use std::path::Path;

use anyhow::Result;

use super::{GitHubClient, log_daemon};
use crate::linear::LinearApi;

/// Real GitHub CLI implementation via `gh pr list`.
pub struct RealGitHub;

impl GitHubClient for RealGitHub {
    fn find_merged_pr(&self, identifier: &str) -> bool {
        let check_cmd = std::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--search",
                identifier,
                "--state",
                "merged",
                "--json",
                "number,title,mergedAt",
                "--limit",
                "1",
            ])
            .output();

        match check_cmd {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let json: serde_json::Value =
                    serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null);
                json.as_array().is_some_and(|arr| !arr.is_empty())
            }
            _ => false,
        }
    }
}

/// Check for merged PRs on issues in "ready" status.
/// When a PR is merged, move the issue to Done via the tracker adapter.
/// Returns `true` if at least one merge was detected (caller should trigger update).
///
/// Accepts any `LinearApi` implementation — works with both the Linear client
/// and the `GitHubIssueClient` adapter, eliminating the now-removed `LinearMergeApi` shim.
pub fn check_merged_prs(
    werma_dir: &Path,
    tracker: &dyn LinearApi,
    github: &impl GitHubClient,
) -> Result<bool> {
    let log_path = werma_dir.join("logs/daemon.log");
    let mut any_merged = false;

    let ready_issues = match tracker.get_issues_by_status("ready") {
        Ok(issues) => issues,
        Err(_) => return Ok(false),
    };

    for issue in &ready_issues {
        let issue_id = issue["id"].as_str().unwrap_or("");
        let identifier = issue["identifier"].as_str().unwrap_or("");

        if issue_id.is_empty() {
            continue;
        }

        if !github.find_merged_pr(identifier) {
            continue;
        }

        log_daemon(
            &log_path,
            &format!("merge detected: {identifier} — moving to Done"),
        );

        if let Err(e) = tracker.move_issue_by_name(issue_id, "done") {
            log_daemon(
                &log_path,
                &format!("failed to move {identifier} to Done: {e}"),
            );
            continue;
        }

        tracker
            .comment(
                issue_id,
                "**PR merged** — issue moved to Done automatically by werma daemon.",
            )
            .ok();

        any_merged = true;
    }

    Ok(any_merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::fakes::FakeLinearApi;
    use serde_json::json;

    fn make_issue(id: &str, identifier: &str) -> serde_json::Value {
        json!({ "id": id, "identifier": identifier })
    }

    struct FakeGitHub {
        merged_prs: Vec<String>,
    }

    impl GitHubClient for FakeGitHub {
        fn find_merged_pr(&self, identifier: &str) -> bool {
            self.merged_prs.iter().any(|p| p == identifier)
        }
    }

    #[test]
    fn no_ready_issues_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        let github = FakeGitHub { merged_prs: vec![] };

        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        assert!(!merged);
    }

    #[test]
    fn no_merged_pr_skips_issue() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        tracker.set_issues_for_status("ready", vec![make_issue("issue-1", "RIG-100")]);
        let github = FakeGitHub { merged_prs: vec![] };

        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        assert!(!merged);
        assert!(tracker.move_calls.borrow().is_empty());
    }

    #[test]
    fn merged_pr_moves_issue_to_done() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        tracker.set_issues_for_status("ready", vec![make_issue("issue-1", "RIG-100")]);
        let github = FakeGitHub {
            merged_prs: vec!["RIG-100".to_string()],
        };

        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        assert!(merged);

        let moves = tracker.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].0, "issue-1");
        assert_eq!(moves[0].1, "done");

        let comments = tracker.comment_calls.borrow();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].1.contains("PR merged"));
    }

    #[test]
    fn empty_issue_id_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        tracker.set_issues_for_status("ready", vec![make_issue("", "RIG-100")]);
        let github = FakeGitHub {
            merged_prs: vec!["RIG-100".to_string()],
        };

        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        assert!(!merged);
        assert!(tracker.move_calls.borrow().is_empty());
    }

    #[test]
    fn multiple_issues_only_merged_ones_processed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        tracker.set_issues_for_status(
            "ready",
            vec![
                make_issue("issue-1", "RIG-100"),
                make_issue("issue-2", "RIG-101"),
                make_issue("issue-3", "RIG-102"),
            ],
        );
        let github = FakeGitHub {
            merged_prs: vec!["RIG-101".to_string()],
        };

        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        assert!(merged);

        let moves = tracker.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].0, "issue-2");
    }

    #[test]
    fn tracker_api_failure_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        tracker.fail_next_n_status_fetches(1);
        let github = FakeGitHub { merged_prs: vec![] };
        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        assert!(!merged);
    }

    #[test]
    fn move_failure_skips_to_next_issue() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        tracker.set_issues_for_status("ready", vec![make_issue("issue-1", "RIG-100")]);
        tracker.fail_next_n_moves(1);
        let github = FakeGitHub {
            merged_prs: vec!["RIG-100".to_string()],
        };

        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        // Move failed → not counted as merged
        assert!(!merged);

        // Should have logged the error
        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(log_content.contains("failed to move"));
    }

    /// Verify that GitHub-style identifiers (repo#N format) are passed through correctly.
    #[test]
    fn github_identifier_passed_to_find_merged_pr() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let tracker = FakeLinearApi::new();
        tracker.set_issues_for_status("ready", vec![make_issue("45", "werma#45")]);

        // FakeGitHub checks for the exact identifier string
        let github = FakeGitHub {
            merged_prs: vec!["werma#45".to_string()],
        };

        let merged = check_merged_prs(dir.path(), &tracker, &github).unwrap();
        assert!(merged);

        let moves = tracker.move_calls.borrow();
        assert_eq!(moves[0].0, "45");
        assert_eq!(moves[0].1, "done");
    }
}
