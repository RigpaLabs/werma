mod client;

pub use client::GitHubIssueClient;

use anyhow::Result;
use serde_json::Value;

use crate::linear::LinearApi;

impl LinearApi for GitHubIssueClient<'_> {
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
