use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use super::config::{load_config, team_key_from_identifier};
use super::helpers::is_after_timestamp;

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
}

const LINEAR_API: &str = "https://api.linear.app/graphql";

/// Re-export from config module for convenience.
fn read_env_file_key(key: &str) -> Result<String, std::env::VarError> {
    crate::config::read_env_file_key(key)
}

pub struct LinearClient {
    client: Client,
    api_key: String,
}

impl LinearClient {
    pub fn new() -> Result<Self> {
        let api_key = std::env::var("LINEAR_API_KEY")
            .or_else(|_| std::env::var("WERMA_LINEAR_API_KEY"))
            .or_else(|_| read_env_file_key("LINEAR_API_KEY"))
            .context("LINEAR_API_KEY not set\n  Fix: export LINEAR_API_KEY=lin_api_...\n  Or add to ~/.werma/.env:\n    LINEAR_API_KEY=lin_api_...")?;

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("building HTTP client")?;

        Ok(Self { client, api_key })
    }

    /// Execute a GraphQL query against the Linear API.
    pub(super) fn query(&self, query: &str, variables: &Value) -> Result<Value> {
        let body = json!({
            "query": query,
            "variables": variables
        });

        let resp = self
            .client
            .post(LINEAR_API)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .context("Linear API request failed")?;

        let json: Value = resp.json().context("parsing Linear response")?;

        if let Some(errors) = json["errors"].as_array()
            && let Some(first) = errors.first()
        {
            bail!(
                "Linear API error: {}",
                first["message"].as_str().unwrap_or("unknown")
            );
        }

        Ok(json["data"].clone())
    }

    // ─── Issue operations ────────────────────────────────────────────────

    /// Resolve an issue identifier (e.g. "RIG-95") to a UUID.
    /// If already a UUID (contains no dash-digit pattern), returns as-is.
    pub(super) fn resolve_uuid(&self, issue_id: &str) -> Result<String> {
        if issue_id.contains('-')
            && issue_id
                .rsplit('-')
                .next()
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        {
            let (uuid, ..) = self.get_issue_by_identifier(issue_id)?;
            Ok(uuid)
        } else {
            Ok(issue_id.to_string())
        }
    }

    /// Move an issue to a status by state ID.
    pub(super) fn move_issue(&self, issue_id: &str, state_id: &str) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;
        self.query(
            r#"mutation($id: String!, $stateId: String!) {
                issueUpdate(id: $id, input: { stateId: $stateId }) { success }
            }"#,
            &json!({"id": uuid, "stateId": state_id}),
        )?;
        Ok(())
    }

    /// Move an issue to a status by status name (looks up in config).
    /// Resolves the correct team's status ID from the issue identifier prefix.
    pub fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
        let config = load_config()?;
        let team_key = team_key_from_identifier(issue_id);
        let state_id = config
            .status_id(&team_key, status_name)
            .with_context(|| format!("unknown status '{status_name}' for team '{team_key}'"))?;
        self.move_issue(issue_id, state_id)
    }

    /// Add a comment to an issue.
    pub fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;
        self.query(
            r#"mutation($issueId: String!, $body: String!) {
                commentCreate(input: { issueId: $issueId, body: $body }) { success }
            }"#,
            &json!({"issueId": uuid, "body": body}),
        )?;
        Ok(())
    }

    /// Attach a URL to a Linear issue (appears in the issue sidebar).
    pub fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;
        self.query(
            r#"mutation($issueId: String!, $url: String!, $title: String!) {
                attachmentCreate(input: { issueId: $issueId, url: $url, title: $title }) {
                    success
                }
            }"#,
            &json!({"issueId": uuid, "url": url, "title": title}),
        )?;
        Ok(())
    }

    /// Update the estimate (story points) of a Linear issue.
    pub fn update_estimate(&self, issue_id: &str, estimate: i32) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;
        self.query(
            r#"mutation($id: String!, $estimate: Int) {
                issueUpdate(id: $id, input: { estimate: $estimate }) { success }
            }"#,
            &json!({"id": uuid, "estimate": estimate}),
        )?;
        Ok(())
    }

    /// Fetch a single issue by ID or identifier (title + description).
    pub fn get_issue(&self, issue_id: &str) -> Result<(String, String)> {
        let uuid = self.resolve_uuid(issue_id)?;
        let data = self.query(
            r#"query($id: String!) {
                issue(id: $id) { title description }
            }"#,
            &json!({"id": uuid}),
        )?;
        let title = data["issue"]["title"].as_str().unwrap_or("").to_string();
        let description = data["issue"]["description"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok((title, description))
    }

    /// Fetch the current status name of an issue (for read-after-write reconciliation).
    pub fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        let uuid = self.resolve_uuid(issue_id)?;
        let data = self.query(
            r#"query($id: String!) {
                issue(id: $id) { state { name } }
            }"#,
            &json!({"id": uuid}),
        )?;
        let status = data["issue"]["state"]["name"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok(status)
    }

    /// Fetch issue state type (e.g. "canceled") and team key (e.g. "RIG").
    pub fn get_issue_state_and_team(&self, issue_id: &str) -> Result<(String, String)> {
        let uuid = self.resolve_uuid(issue_id)?;
        let data = self.query(
            r#"query($id: String!) {
                issue(id: $id) { state { type } team { key } }
            }"#,
            &json!({"id": uuid}),
        )?;
        let state_type = data["issue"]["state"]["type"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let team_key = data["issue"]["team"]["key"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok((state_type, team_key))
    }

    /// Fetch a single issue by identifier (e.g. "RIG-95").
    /// Returns (uuid, identifier, title, description, labels).
    pub fn get_issue_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<(String, String, String, String, Vec<String>)> {
        let config = load_config()?;
        let team_key = team_key_from_identifier(identifier);
        let team = config
            .team_by_key(&team_key)
            .or(config.primary_team())
            .context("no teams configured")?;

        // Parse "RIG-95" → number 95
        let number: i64 = identifier
            .rsplit('-')
            .next()
            .and_then(|n| n.parse().ok())
            .with_context(|| format!("invalid identifier: {identifier}"))?;

        let data = self.query(
            r#"query($teamId: ID!, $number: Float!) {
                issues(filter: {
                    team: { id: { eq: $teamId } },
                    number: { eq: $number }
                }) {
                    nodes {
                        id identifier title description
                        labels { nodes { name } }
                    }
                }
            }"#,
            &json!({"teamId": team.team_id, "number": number}),
        )?;

        let nodes = data["issues"]["nodes"]
            .as_array()
            .context("unexpected response")?;
        let issue = nodes
            .first()
            .with_context(|| format!("issue {identifier} not found"))?;

        let id = issue["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("issue {identifier} has no id"))?
            .to_string();
        let ident = issue["identifier"]
            .as_str()
            .unwrap_or(identifier)
            .to_string();
        let title = issue["title"].as_str().unwrap_or("").to_string();
        let description = issue["description"].as_str().unwrap_or("").to_string();
        let labels = issue["labels"]["nodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok((id, ident, title, description, labels))
    }

    /// Fetch comments on an issue by UUID, optionally filtering to those after `after_iso`.
    /// Returns vec of (author_name, created_at_iso, body) sorted chronologically.
    pub fn list_comments(
        &self,
        issue_id: &str,
        after_iso: Option<&str>,
    ) -> Result<Vec<(String, String, String)>> {
        let uuid = self.resolve_uuid(issue_id)?;

        let data = self.query(
            r#"query($issueId: ID!) {
                issue(id: $issueId) {
                    comments(orderBy: createdAt) {
                        nodes {
                            body
                            createdAt
                            user { name }
                        }
                    }
                }
            }"#,
            &json!({"issueId": uuid}),
        )?;

        let nodes = data["issue"]["comments"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut comments = Vec::new();
        for node in &nodes {
            let body = node["body"].as_str().unwrap_or("").to_string();
            let created_at = node["createdAt"].as_str().unwrap_or("").to_string();
            let author = node["user"]["name"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();

            // Filter by timestamp if provided — use chrono for proper comparison
            // since SQLite stores local time (%Y-%m-%dT%H:%M:%S) and Linear
            // returns UTC with fractional seconds (2026-03-24T15:30:00.000Z).
            if let Some(after) = after_iso {
                if !is_after_timestamp(&created_at, after) {
                    continue;
                }
            }

            // Skip bot/pipeline comments (werma callback comments)
            if body.starts_with("**Werma") || body.starts_with("**Pipeline") {
                continue;
            }

            comments.push((author, created_at, body));
        }

        Ok(comments)
    }
}
