use std::path::Path;

use anyhow::Result;

use crate::db::Db;

use super::log_daemon;
use super::{GitHubClient, LinearMergeApi};

/// Check for merged PRs on issues in "ready" status.
/// When a PR is merged, move the issue to Done in Linear.
/// Returns `true` if at least one merge was detected (caller should trigger update).
pub fn check_merged_prs(
    _db: &Db,
    werma_dir: &Path,
    linear: &impl LinearMergeApi,
    github: &impl GitHubClient,
) -> Result<bool> {
    let log_path = werma_dir.join("logs/daemon.log");
    let mut any_merged = false;

    let ready_issues = match linear.get_issues_by_status("ready") {
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

        if let Err(e) = linear.move_issue_by_name(issue_id, "done") {
            log_daemon(
                &log_path,
                &format!("failed to move {identifier} to Done: {e}"),
            );
            continue;
        }

        linear
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
    use serde_json::json;

    struct FakeLinear {
        issues: Vec<serde_json::Value>,
        move_calls: std::cell::RefCell<Vec<(String, String)>>,
        comment_calls: std::cell::RefCell<Vec<(String, String)>>,
    }

    impl FakeLinear {
        fn new(issues: Vec<serde_json::Value>) -> Self {
            Self {
                issues,
                move_calls: std::cell::RefCell::new(vec![]),
                comment_calls: std::cell::RefCell::new(vec![]),
            }
        }
    }

    impl LinearMergeApi for FakeLinear {
        fn get_issues_by_status(&self, _status_name: &str) -> Result<Vec<serde_json::Value>> {
            Ok(self.issues.clone())
        }

        fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
            self.move_calls
                .borrow_mut()
                .push((issue_id.to_string(), status_name.to_string()));
            Ok(())
        }

        fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
            self.comment_calls
                .borrow_mut()
                .push((issue_id.to_string(), body.to_string()));
            Ok(())
        }
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
        let db = crate::db::Db::open_in_memory().unwrap();

        let linear = FakeLinear::new(vec![]);
        let github = FakeGitHub { merged_prs: vec![] };

        let merged = check_merged_prs(&db, dir.path(), &linear, &github).unwrap();
        assert!(!merged);
    }

    #[test]
    fn no_merged_pr_skips_issue() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();

        let linear = FakeLinear::new(vec![json!({
            "id": "issue-1",
            "identifier": "RIG-100"
        })]);
        let github = FakeGitHub { merged_prs: vec![] };

        let merged = check_merged_prs(&db, dir.path(), &linear, &github).unwrap();
        assert!(!merged);
        assert!(linear.move_calls.borrow().is_empty());
    }

    #[test]
    fn merged_pr_moves_issue_to_done() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();

        let linear = FakeLinear::new(vec![json!({
            "id": "issue-1",
            "identifier": "RIG-100"
        })]);
        let github = FakeGitHub {
            merged_prs: vec!["RIG-100".to_string()],
        };

        let merged = check_merged_prs(&db, dir.path(), &linear, &github).unwrap();
        assert!(merged);

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].0, "issue-1");
        assert_eq!(moves[0].1, "done");

        let comments = linear.comment_calls.borrow();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].1.contains("PR merged"));
    }

    #[test]
    fn empty_issue_id_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();

        let linear = FakeLinear::new(vec![json!({
            "id": "",
            "identifier": "RIG-100"
        })]);
        let github = FakeGitHub {
            merged_prs: vec!["RIG-100".to_string()],
        };

        let merged = check_merged_prs(&db, dir.path(), &linear, &github).unwrap();
        assert!(!merged);
        assert!(linear.move_calls.borrow().is_empty());
    }

    #[test]
    fn multiple_issues_only_merged_ones_processed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();

        let linear = FakeLinear::new(vec![
            json!({"id": "issue-1", "identifier": "RIG-100"}),
            json!({"id": "issue-2", "identifier": "RIG-101"}),
            json!({"id": "issue-3", "identifier": "RIG-102"}),
        ]);
        let github = FakeGitHub {
            merged_prs: vec!["RIG-101".to_string()],
        };

        let merged = check_merged_prs(&db, dir.path(), &linear, &github).unwrap();
        assert!(merged);

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].0, "issue-2");
    }

    struct FailLinear;

    impl LinearMergeApi for FailLinear {
        fn get_issues_by_status(&self, _status_name: &str) -> Result<Vec<serde_json::Value>> {
            Err(anyhow::anyhow!("no API key"))
        }

        fn move_issue_by_name(&self, _issue_id: &str, _status_name: &str) -> Result<()> {
            Ok(())
        }

        fn comment(&self, _issue_id: &str, _body: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn linear_api_failure_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();

        let github = FakeGitHub { merged_prs: vec![] };

        let merged = check_merged_prs(&db, dir.path(), &FailLinear, &github).unwrap();
        assert!(!merged);
    }

    struct FailMoveLinear;

    impl LinearMergeApi for FailMoveLinear {
        fn get_issues_by_status(&self, _status_name: &str) -> Result<Vec<serde_json::Value>> {
            Ok(vec![json!({"id": "issue-1", "identifier": "RIG-100"})])
        }

        fn move_issue_by_name(&self, _issue_id: &str, _status_name: &str) -> Result<()> {
            Err(anyhow::anyhow!("move failed"))
        }

        fn comment(&self, _issue_id: &str, _body: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn move_failure_skips_to_next_issue() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();

        let github = FakeGitHub {
            merged_prs: vec!["RIG-100".to_string()],
        };

        let merged = check_merged_prs(&db, dir.path(), &FailMoveLinear, &github).unwrap();
        // Move failed → not counted as merged
        assert!(!merged);

        // Should have logged the error
        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(log_content.contains("failed to move"));
    }
}
