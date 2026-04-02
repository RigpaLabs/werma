mod client;
mod config;

// Re-exports — callers import from `crate::linear::*`
pub use client::LinearClient;
#[allow(unused_imports)]
pub use client::is_after_timestamp;
pub use config::{
    configured_team_keys, infer_working_dir, is_linear_identifier, is_manual_issue,
    validate_working_dir,
};

use anyhow::Result;
use serde_json::Value;

// ─── LinearApi trait ─────────────────────────────────────────────────────────

/// Trait abstracting Linear API operations for testability.
/// Covers all methods called by pipeline/executor, pipeline/mod, daemon, and runner.
pub trait LinearApi {
    fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>>;
    fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<Value>>;
    fn get_issue(&self, issue_id: &str) -> Result<(String, String)>;
    fn get_issue_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<(String, String, String, String, Vec<String>)>;
    fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()>;
    fn comment(&self, issue_id: &str, body: &str) -> Result<()>;
    fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> Result<()>;
    fn update_estimate(&self, issue_id: &str, estimate: i32) -> Result<()>;
    fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()>;
    fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()>;
    /// Get the current status name of an issue (for read-after-write reconciliation).
    fn get_issue_status(&self, issue_id: &str) -> Result<String>;
    /// Get issue state type (e.g. "canceled", "completed") and team key (e.g. "RIG", "FAT").
    /// Used by cancel detection to identify canceled issues or issues moved to another team.
    fn get_issue_state_and_team(&self, issue_id: &str) -> Result<(String, String)>;
    /// Fetch comments on an issue, optionally filtered to those created after `after_iso`.
    /// Returns vec of (author_name, created_at_iso, body).
    fn list_comments(
        &self,
        issue_id: &str,
        after_iso: Option<&str>,
    ) -> Result<Vec<(String, String, String)>>;

    /// Fetch child (sub) issues of a parent issue.
    /// Returns vec of (identifier, title, status_name, description).
    /// Returns empty vec if the issue has no children.
    fn get_sub_issues(&self, identifier: &str) -> Result<Vec<(String, String, String, String)>>;
}

impl LinearApi for LinearClient {
    fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>> {
        self.get_issues_by_status(status_name)
    }

    fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<Value>> {
        self.get_issues_by_label(label_name)
    }

    fn get_issue(&self, issue_id: &str) -> Result<(String, String)> {
        self.get_issue(issue_id)
    }

    fn get_issue_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<(String, String, String, String, Vec<String>)> {
        self.get_issue_by_identifier(identifier)
    }

    fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
        self.move_issue_by_name(issue_id, status_name)
    }

    fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
        self.comment(issue_id, body)
    }

    fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> Result<()> {
        self.attach_url(issue_id, url, title)
    }

    fn update_estimate(&self, issue_id: &str, estimate: i32) -> Result<()> {
        self.update_estimate(issue_id, estimate)
    }

    fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        self.remove_label(issue_id, label_name)
    }

    fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        self.add_label(issue_id, label_name)
    }

    fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        self.get_issue_status(issue_id)
    }

    fn get_issue_state_and_team(&self, issue_id: &str) -> Result<(String, String)> {
        self.get_issue_state_and_team(issue_id)
    }

    fn list_comments(
        &self,
        issue_id: &str,
        after_iso: Option<&str>,
    ) -> Result<Vec<(String, String, String)>> {
        self.list_comments(issue_id, after_iso)
    }

    fn get_sub_issues(&self, identifier: &str) -> Result<Vec<(String, String, String, String)>> {
        self.get_sub_issues(identifier)
    }
}
