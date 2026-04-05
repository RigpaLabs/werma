use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::traits::CommandRunner;

// ─── Labels-as-statuses convention ─────────────────────────────────────────
//
// GitHub Issues don't have workflow statuses like Linear. We use labels:
//   status:backlog, status:todo, status:in-progress, status:review,
//   status:qa, status:ready, status:done, status:canceled,
//   status:blocked, status:deploy, status:failed
//
// Story points use sp:N labels (sp:1, sp:2, sp:3, sp:5, sp:8, sp:13).
// Moving to done/canceled also closes the issue.
// ───────────────────────────────────────────────────────────────────────────

/// GitHub Issues adapter that speaks the LinearApi protocol.
///
/// Uses `gh` CLI via `CommandRunner` for all GitHub API interactions,
/// enabling unit tests with `FakeCommandRunner`.
pub struct GitHubIssueClient<'a> {
    cmd: &'a dyn CommandRunner,
    owner: String,
    repo: String,
}

impl<'a> GitHubIssueClient<'a> {
    pub fn new(cmd: &'a dyn CommandRunner, owner: String, repo: String) -> Self {
        Self { cmd, owner, repo }
    }

    fn repo_flag(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Run a `gh` command and return parsed JSON stdout.
    fn gh_json(&self, args: &[&str]) -> Result<Value> {
        let output = self
            .cmd
            .run("gh", args, None)
            .context("running gh command")?;

        if !output.success {
            let stderr = output.stderr_str();
            bail!("gh command failed: {stderr}");
        }

        let stdout = output.stdout_str();
        if stdout.is_empty() {
            return Ok(Value::Null);
        }

        serde_json::from_str(&stdout).with_context(|| {
            format!(
                "parsing gh JSON output (first 200 chars): {}",
                &stdout[..stdout.len().min(200)]
            )
        })
    }

    /// Run a `gh` command, ignoring stdout (for mutations).
    fn gh_exec(&self, args: &[&str]) -> Result<()> {
        let output = self
            .cmd
            .run("gh", args, None)
            .context("running gh command")?;

        if !output.success {
            let stderr = output.stderr_str();
            bail!("gh command failed: {stderr}");
        }
        Ok(())
    }

    /// Parse an issue number from an identifier.
    /// Accepts: "42", "#42", "repo#42", "owner/repo#42"
    fn parse_number(issue_id: &str) -> Result<u64> {
        let s = issue_id.trim();

        if let Some(hash_pos) = s.rfind('#') {
            let num_str = &s[hash_pos + 1..];
            return num_str
                .parse()
                .with_context(|| format!("invalid issue number: {issue_id}"));
        }

        s.parse()
            .with_context(|| format!("invalid issue number: {issue_id}"))
    }

    /// Build identifier in `repo#number` format.
    fn make_identifier(&self, number: u64) -> String {
        format!("{}#{number}", self.repo)
    }

    /// Map a werma status name to a GitHub status label.
    fn status_to_label(status_name: &str) -> String {
        let normalized = status_name.to_lowercase().replace(' ', "_");
        let suffix = match normalized.as_str() {
            "backlog" => "backlog",
            "todo" => "todo",
            "in_progress" => "in-progress",
            "in_review" | "review" => "review",
            "qa" => "qa",
            "ready" => "ready",
            "done" => "done",
            "canceled" => "canceled",
            "blocked" => "blocked",
            "deploy" => "deploy",
            "failed" => "failed",
            other => other,
        };
        format!("status:{suffix}")
    }

    /// Derive the Linear-compatible state type from a status label.
    fn label_to_state_type(label: &str) -> &'static str {
        match label {
            "status:backlog" | "status:blocked" => "backlog",
            "status:todo" => "unstarted",
            "status:in-progress" | "status:review" | "status:qa" | "status:deploy"
            | "status:failed" | "status:ready" => "started",
            "status:done" => "completed",
            "status:canceled" => "canceled",
            _ => "unstarted",
        }
    }

    /// Map a status label to a human-readable name (matches Linear's status names).
    fn label_to_status_name(label: &str) -> &'static str {
        match label {
            "status:backlog" => "Backlog",
            "status:todo" => "Todo",
            "status:in-progress" => "In Progress",
            "status:review" => "In Review",
            "status:qa" => "QA",
            "status:ready" => "Ready",
            "status:done" => "Done",
            "status:canceled" => "Canceled",
            "status:blocked" => "Blocked",
            "status:deploy" => "Deploy",
            "status:failed" => "Failed",
            _ => "Unknown",
        }
    }

    /// Find the first `status:*` label in a labels array.
    fn find_status_label(labels: &[Value]) -> Option<String> {
        labels
            .iter()
            .filter_map(|l| l["name"].as_str())
            .find(|name| name.starts_with("status:"))
            .map(String::from)
    }

    /// Extract story point estimate from `sp:N` labels.
    fn find_estimate(labels: &[Value]) -> i64 {
        labels
            .iter()
            .filter_map(|l| l["name"].as_str())
            .find(|name| name.starts_with("sp:"))
            .and_then(|name| name.strip_prefix("sp:"))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }

    /// Normalize a GitHub issue (from `gh --json`) into poll.rs-compatible JSON.
    ///
    /// Expected input fields: number, title, body, labels, state
    /// Output shape matches what Linear returns and poll.rs consumes.
    fn normalize_issue(&self, gh: &Value) -> Value {
        let number = gh["number"].as_u64().unwrap_or(0);
        // RIG-385: number=0 means the 'number' field was missing or wrong type in gh output.
        // Log a warning so this shows up in daemon logs for diagnosis.
        if number == 0 {
            eprintln!(
                "  ! normalize_issue: gh issue has number=0 or missing 'number' field \
                 (raw type={:?}) — identifier will be malformed",
                gh["number"]
            );
        }
        let title = gh["title"].as_str().unwrap_or("");
        let body = gh["body"].as_str().unwrap_or("");
        let labels = gh["labels"].as_array().cloned().unwrap_or_default();

        let status_label = Self::find_status_label(&labels);
        let state_type = status_label
            .as_deref()
            .map(Self::label_to_state_type)
            .unwrap_or("backlog");
        let estimate = Self::find_estimate(&labels);

        // Rebuild labels in Linear's { nodes: [{ name }] } shape
        let label_nodes: Vec<Value> = labels
            .iter()
            .filter_map(|l| {
                let name = l["name"].as_str()?;
                Some(json!({ "name": name }))
            })
            .collect();

        json!({
            "id": number.to_string(),
            "identifier": self.make_identifier(number),
            "title": title,
            "description": body,
            "priority": 0,
            "estimate": estimate,
            "state": { "type": state_type },
            "labels": { "nodes": label_nodes }
        })
    }

    /// Collect all `status:*` label names from an issue's current labels.
    fn collect_status_labels(labels: &[Value]) -> Vec<String> {
        labels
            .iter()
            .filter_map(|l| l["name"].as_str())
            .filter(|name| name.starts_with("status:"))
            .map(String::from)
            .collect()
    }

    /// Parse task-list checkboxes from a markdown body.
    /// Returns vec of (text, is_checked).
    fn parse_task_list(body: &str) -> Vec<(String, bool)> {
        body.lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if let Some(text) = trimmed
                    .strip_prefix("- [x] ")
                    .or_else(|| trimmed.strip_prefix("- [X] "))
                {
                    Some((text.to_string(), true))
                } else {
                    trimmed
                        .strip_prefix("- [ ] ")
                        .map(|text| (text.to_string(), false))
                }
            })
            .collect()
    }

    // ─── LinearApi method implementations ──────────────────────────────────

    pub fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>> {
        let label = Self::status_to_label(status_name);
        let repo = self.repo_flag();
        let json = self.gh_json(&[
            "issue",
            "list",
            "--label",
            &label,
            "--repo",
            &repo,
            "--json",
            "number,title,body,labels,state",
            "--limit",
            "100",
            "--state",
            "all",
        ])?;

        let issues = json.as_array().cloned().unwrap_or_default();
        let normalized: Vec<Value> = issues.iter().map(|i| self.normalize_issue(i)).collect();
        Ok(normalized)
    }

    pub fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<Value>> {
        let repo = self.repo_flag();
        let json = self.gh_json(&[
            "issue",
            "list",
            "--label",
            label_name,
            "--repo",
            &repo,
            "--json",
            "number,title,body,labels,state",
            "--limit",
            "100",
            "--state",
            "all",
        ])?;

        let issues = json.as_array().cloned().unwrap_or_default();
        Ok(issues.iter().map(|i| self.normalize_issue(i)).collect())
    }

    pub fn get_issue(&self, issue_id: &str) -> Result<(String, String)> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let json = self.gh_json(&[
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            &repo,
            "--json",
            "title,body",
        ])?;

        let title = json["title"].as_str().unwrap_or("").to_string();
        let body = json["body"].as_str().unwrap_or("").to_string();
        Ok((title, body))
    }

    pub fn get_issue_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<(String, String, String, String, Vec<String>)> {
        let number = Self::parse_number(identifier)?;
        let repo = self.repo_flag();
        let json = self.gh_json(&[
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            &repo,
            "--json",
            "number,title,body,labels",
        ])?;

        let num = json["number"].as_u64().unwrap_or(number);
        let title = json["title"].as_str().unwrap_or("").to_string();
        let body = json["body"].as_str().unwrap_or("").to_string();
        let labels: Vec<String> = json["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok((
            num.to_string(),
            self.make_identifier(num),
            title,
            body,
            labels,
        ))
    }

    pub fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let new_label = Self::status_to_label(status_name);

        // Fetch current labels to find existing status labels
        let json = self.gh_json(&[
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            &repo,
            "--json",
            "labels",
        ])?;

        let labels = json["labels"].as_array().cloned().unwrap_or_default();
        let old_status_labels = Self::collect_status_labels(&labels);

        // Build edit args: remove old status labels, add new one
        let num_str = number.to_string();
        let mut args: Vec<String> = vec![
            "issue".into(),
            "edit".into(),
            num_str,
            "--repo".into(),
            repo.clone(),
        ];

        for old in &old_status_labels {
            args.push("--remove-label".into());
            args.push(old.clone());
        }

        args.push("--add-label".into());
        args.push(new_label);

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.gh_exec(&arg_refs)?;

        // Close issue if moving to done or canceled
        let normalized = status_name.to_lowercase().replace(' ', "_");
        if normalized == "done" || normalized == "canceled" {
            let num_str = number.to_string();
            self.gh_exec(&["issue", "close", &num_str, "--repo", &repo])?;
        }

        Ok(())
    }

    pub fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let num_str = number.to_string();
        self.gh_exec(&[
            "issue", "comment", &num_str, "--repo", &repo, "--body", body,
        ])
    }

    pub fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> Result<()> {
        let body = format!("**{title}**\n{url}");
        self.comment(issue_id, &body)
    }

    pub fn update_estimate(&self, issue_id: &str, estimate: i32) -> Result<()> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();

        // Fetch current labels to find old sp:* labels
        let json = self.gh_json(&[
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            &repo,
            "--json",
            "labels",
        ])?;

        let labels = json["labels"].as_array().cloned().unwrap_or_default();
        let old_sp: Vec<String> = labels
            .iter()
            .filter_map(|l| l["name"].as_str())
            .filter(|name| name.starts_with("sp:"))
            .map(String::from)
            .collect();

        let new_label = format!("sp:{estimate}");
        let num_str = number.to_string();
        let mut args: Vec<String> = vec![
            "issue".into(),
            "edit".into(),
            num_str,
            "--repo".into(),
            repo,
        ];

        for old in &old_sp {
            args.push("--remove-label".into());
            args.push(old.clone());
        }

        args.push("--add-label".into());
        args.push(new_label);

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.gh_exec(&arg_refs)
    }

    pub fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let num_str = number.to_string();
        self.gh_exec(&[
            "issue",
            "edit",
            &num_str,
            "--repo",
            &repo,
            "--remove-label",
            label_name,
        ])
    }

    pub fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let num_str = number.to_string();
        self.gh_exec(&[
            "issue",
            "edit",
            &num_str,
            "--repo",
            &repo,
            "--add-label",
            label_name,
        ])
    }

    pub fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let num_str = number.to_string();
        let json = self.gh_json(&[
            "issue",
            "view",
            &num_str,
            "--repo",
            &repo,
            "--json",
            "labels,state",
        ])?;

        let labels = json["labels"].as_array().cloned().unwrap_or_default();
        let status_label = Self::find_status_label(&labels);

        if let Some(label) = status_label {
            Ok(Self::label_to_status_name(&label).to_string())
        } else {
            let state = json["state"].as_str().unwrap_or("OPEN");
            if state == "CLOSED" {
                Ok("Done".to_string())
            } else {
                Ok("Backlog".to_string())
            }
        }
    }

    pub fn get_issue_state_and_team(&self, issue_id: &str) -> Result<(String, String)> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let num_str = number.to_string();
        let json = self.gh_json(&[
            "issue",
            "view",
            &num_str,
            "--repo",
            &repo,
            "--json",
            "labels,state",
        ])?;

        let labels = json["labels"].as_array().cloned().unwrap_or_default();
        let status_label = Self::find_status_label(&labels);

        let state_type = if let Some(label) = &status_label {
            Self::label_to_state_type(label).to_string()
        } else {
            let state = json["state"].as_str().unwrap_or("OPEN");
            if state == "CLOSED" {
                "completed".to_string()
            } else {
                "unstarted".to_string()
            }
        };

        // Return empty team key — GitHub issues don't belong to Linear teams.
        // The empty-string guard in cancel_check.rs will skip the team-mismatch check.
        Ok((state_type, String::new()))
    }

    pub fn list_comments(
        &self,
        issue_id: &str,
        after_iso: Option<&str>,
    ) -> Result<Vec<(String, String, String)>> {
        let number = Self::parse_number(issue_id)?;
        let repo = self.repo_flag();
        let num_str = number.to_string();
        let json = self.gh_json(&[
            "issue", "view", &num_str, "--repo", &repo, "--json", "comments",
        ])?;

        let nodes = json["comments"].as_array().cloned().unwrap_or_default();
        let mut comments = Vec::new();

        for node in &nodes {
            let body = node["body"].as_str().unwrap_or("").to_string();
            let created_at = node["createdAt"].as_str().unwrap_or("").to_string();
            let author = node["author"]["login"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();

            if let Some(after) = after_iso {
                if !crate::linear::is_after_timestamp(&created_at, after) {
                    continue;
                }
            }

            // Skip bot/pipeline comments
            if body.starts_with("**Werma") || body.starts_with("**Pipeline") {
                continue;
            }

            comments.push((author, created_at, body));
        }

        Ok(comments)
    }

    pub fn get_sub_issues(
        &self,
        identifier: &str,
    ) -> Result<Vec<(String, String, String, String)>> {
        let number = Self::parse_number(identifier)?;
        let repo = self.repo_flag();
        let num_str = number.to_string();
        let json = self.gh_json(&["issue", "view", &num_str, "--repo", &repo, "--json", "body"])?;

        let body = json["body"].as_str().unwrap_or("");
        let tasks = Self::parse_task_list(body);

        Ok(tasks
            .into_iter()
            .enumerate()
            .map(|(i, (text, checked))| {
                let status = if checked { "Done" } else { "Todo" };
                let sub_id = format!("{}#{}-task-{}", self.repo, number, i + 1);
                (sub_id, text, status.to_string(), String::new())
            })
            .collect())
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::fakes::FakeCommandRunner;

    fn test_client(cmd: &dyn CommandRunner) -> GitHubIssueClient<'_> {
        GitHubIssueClient::new(cmd, "testowner".to_string(), "testrepo".to_string())
    }

    // ── Pure function tests ────────────────────────────────────────────

    #[test]
    fn parse_number_plain() {
        assert_eq!(GitHubIssueClient::parse_number("42").unwrap(), 42);
    }

    #[test]
    fn parse_number_hash_prefix() {
        assert_eq!(GitHubIssueClient::parse_number("#42").unwrap(), 42);
    }

    #[test]
    fn parse_number_repo_qualified() {
        assert_eq!(
            GitHubIssueClient::parse_number("owner/repo#99").unwrap(),
            99
        );
    }

    #[test]
    fn parse_number_repo_no_owner() {
        assert_eq!(GitHubIssueClient::parse_number("repo#7").unwrap(), 7);
    }

    #[test]
    fn parse_number_invalid() {
        assert!(GitHubIssueClient::parse_number("abc").is_err());
        assert!(GitHubIssueClient::parse_number("#").is_err());
        assert!(GitHubIssueClient::parse_number("").is_err());
    }

    #[test]
    fn status_to_label_mapping() {
        assert_eq!(GitHubIssueClient::status_to_label("todo"), "status:todo");
        assert_eq!(
            GitHubIssueClient::status_to_label("in_progress"),
            "status:in-progress"
        );
        assert_eq!(GitHubIssueClient::status_to_label("done"), "status:done");
        assert_eq!(
            GitHubIssueClient::status_to_label("canceled"),
            "status:canceled"
        );
        assert_eq!(
            GitHubIssueClient::status_to_label("review"),
            "status:review"
        );
    }

    #[test]
    fn label_to_state_type_mapping() {
        assert_eq!(
            GitHubIssueClient::label_to_state_type("status:todo"),
            "unstarted"
        );
        assert_eq!(
            GitHubIssueClient::label_to_state_type("status:in-progress"),
            "started"
        );
        assert_eq!(
            GitHubIssueClient::label_to_state_type("status:done"),
            "completed"
        );
        assert_eq!(
            GitHubIssueClient::label_to_state_type("status:canceled"),
            "canceled"
        );
        assert_eq!(
            GitHubIssueClient::label_to_state_type("status:backlog"),
            "backlog"
        );
        assert_eq!(
            GitHubIssueClient::label_to_state_type("status:ready"),
            "started"
        );
    }

    #[test]
    fn label_to_status_name_mapping() {
        assert_eq!(
            GitHubIssueClient::label_to_status_name("status:todo"),
            "Todo"
        );
        assert_eq!(
            GitHubIssueClient::label_to_status_name("status:in-progress"),
            "In Progress"
        );
        assert_eq!(
            GitHubIssueClient::label_to_status_name("status:done"),
            "Done"
        );
        assert_eq!(
            GitHubIssueClient::label_to_status_name("unknown"),
            "Unknown"
        );
    }

    #[test]
    fn find_status_label_present() {
        let labels = vec![
            json!({"name": "bug"}),
            json!({"name": "status:in-progress"}),
            json!({"name": "priority:high"}),
        ];
        assert_eq!(
            GitHubIssueClient::find_status_label(&labels),
            Some("status:in-progress".to_string())
        );
    }

    #[test]
    fn find_status_label_absent() {
        let labels = vec![json!({"name": "bug"}), json!({"name": "feature"})];
        assert_eq!(GitHubIssueClient::find_status_label(&labels), None);
    }

    #[test]
    fn find_estimate_present() {
        let labels = vec![
            json!({"name": "status:todo"}),
            json!({"name": "sp:5"}),
            json!({"name": "feature"}),
        ];
        assert_eq!(GitHubIssueClient::find_estimate(&labels), 5);
    }

    #[test]
    fn find_estimate_absent() {
        let labels = vec![json!({"name": "bug"})];
        assert_eq!(GitHubIssueClient::find_estimate(&labels), 0);
    }

    #[test]
    fn parse_task_list_mixed() {
        let body =
            "## Tasks\n- [x] First thing\n- [ ] Second thing\nSome text\n- [X] Third thing\n";
        let tasks = GitHubIssueClient::parse_task_list(body);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0], ("First thing".to_string(), true));
        assert_eq!(tasks[1], ("Second thing".to_string(), false));
        assert_eq!(tasks[2], ("Third thing".to_string(), true));
    }

    #[test]
    fn parse_task_list_empty() {
        assert!(GitHubIssueClient::parse_task_list("No tasks here").is_empty());
        assert!(GitHubIssueClient::parse_task_list("").is_empty());
    }

    #[test]
    fn normalize_issue_full() {
        let cmd = FakeCommandRunner::new();
        let client = test_client(&cmd);
        let gh_issue = json!({
            "number": 42,
            "title": "Fix the bug",
            "body": "Detailed description",
            "labels": [
                {"name": "status:in-progress"},
                {"name": "sp:3"},
                {"name": "bug"}
            ],
            "state": "OPEN"
        });

        let normalized = client.normalize_issue(&gh_issue);

        assert_eq!(normalized["id"], "42");
        assert_eq!(normalized["identifier"], "testrepo#42");
        assert_eq!(normalized["title"], "Fix the bug");
        assert_eq!(normalized["description"], "Detailed description");
        assert_eq!(normalized["state"]["type"], "started");
        assert_eq!(normalized["estimate"], 3);
        assert_eq!(normalized["priority"], 0);

        let label_nodes = normalized["labels"]["nodes"].as_array().unwrap();
        assert_eq!(label_nodes.len(), 3);
        assert_eq!(label_nodes[0]["name"], "status:in-progress");
    }

    #[test]
    fn normalize_issue_no_status_label() {
        let cmd = FakeCommandRunner::new();
        let client = test_client(&cmd);
        let gh_issue = json!({
            "number": 1,
            "title": "Title",
            "body": "",
            "labels": [],
            "state": "OPEN"
        });

        let normalized = client.normalize_issue(&gh_issue);
        assert_eq!(normalized["state"]["type"], "backlog");
        assert_eq!(normalized["estimate"], 0);
    }

    // ── FakeCommandRunner integration tests ────────────────────────────

    #[test]
    fn get_issues_by_status_normalizes() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!([
                {
                    "number": 10,
                    "title": "Issue A",
                    "body": "Desc A",
                    "labels": [{"name": "status:todo"}],
                    "state": "OPEN"
                }
            ]))
            .unwrap(),
        );

        let client = test_client(&cmd);
        let issues = client.get_issues_by_status("todo").unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["id"], "10");
        assert_eq!(issues[0]["identifier"], "testrepo#10");
        assert_eq!(issues[0]["state"]["type"], "unstarted");
    }

    #[test]
    fn get_issues_by_status_gh_args() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("[]");

        let client = test_client(&cmd);
        client.get_issues_by_status("in_progress").unwrap();

        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "gh");
        assert!(calls[0].1.contains(&"--label".to_string()));
        assert!(calls[0].1.contains(&"status:in-progress".to_string()));
        assert!(calls[0].1.contains(&"testowner/testrepo".to_string()));
    }

    #[test]
    fn get_issue_returns_title_body() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!({
                "title": "My Issue",
                "body": "Description text"
            }))
            .unwrap(),
        );

        let client = test_client(&cmd);
        let (title, desc) = client.get_issue("42").unwrap();

        assert_eq!(title, "My Issue");
        assert_eq!(desc, "Description text");
    }

    #[test]
    fn get_issue_by_identifier_parses_hash() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!({
                "number": 99,
                "title": "Feature X",
                "body": "Spec here",
                "labels": [{"name": "feature"}, {"name": "status:todo"}]
            }))
            .unwrap(),
        );

        let client = test_client(&cmd);
        let (id, ident, title, desc, labels) =
            client.get_issue_by_identifier("testrepo#99").unwrap();

        assert_eq!(id, "99");
        assert_eq!(ident, "testrepo#99");
        assert_eq!(title, "Feature X");
        assert_eq!(desc, "Spec here");
        assert_eq!(labels, vec!["feature", "status:todo"]);
    }

    #[test]
    fn move_issue_swaps_labels_and_closes_on_done() {
        let cmd = FakeCommandRunner::new();

        // gh issue view (fetch current labels)
        cmd.push_success(
            &serde_json::to_string(&json!({
                "labels": [{"name": "status:in-progress"}, {"name": "bug"}]
            }))
            .unwrap(),
        );
        // gh issue edit (swap labels)
        cmd.push_success("");
        // gh issue close (done triggers close)
        cmd.push_success("");

        let client = test_client(&cmd);
        client.move_issue_by_name("42", "done").unwrap();

        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 3);

        // Verify edit call removes old label, adds new
        let edit_args = &calls[1].1;
        assert!(edit_args.contains(&"--remove-label".to_string()));
        assert!(edit_args.contains(&"status:in-progress".to_string()));
        assert!(edit_args.contains(&"--add-label".to_string()));
        assert!(edit_args.contains(&"status:done".to_string()));

        // Verify close call
        assert!(calls[2].1.contains(&"close".to_string()));
    }

    #[test]
    fn move_issue_no_close_for_non_terminal() {
        let cmd = FakeCommandRunner::new();

        // gh issue view
        cmd.push_success(
            &serde_json::to_string(&json!({
                "labels": [{"name": "status:todo"}]
            }))
            .unwrap(),
        );
        // gh issue edit
        cmd.push_success("");

        let client = test_client(&cmd);
        client.move_issue_by_name("42", "in_progress").unwrap();

        let calls = cmd.calls.borrow();
        // Only 2 calls (view + edit), no close
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn comment_gh_args() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("");

        let client = test_client(&cmd);
        client.comment("#10", "Hello from werma").unwrap();

        let calls = cmd.calls.borrow();
        assert_eq!(calls[0].0, "gh");
        assert!(calls[0].1.contains(&"comment".to_string()));
        assert!(calls[0].1.contains(&"10".to_string()));
        assert!(calls[0].1.contains(&"--body".to_string()));
        assert!(calls[0].1.contains(&"Hello from werma".to_string()));
    }

    #[test]
    fn attach_url_posts_formatted_comment() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("");

        let client = test_client(&cmd);
        client
            .attach_url("42", "https://example.com/pr/1", "Pull Request")
            .unwrap();

        let calls = cmd.calls.borrow();
        let body_idx = calls[0]
            .1
            .iter()
            .position(|a| a == "--body")
            .expect("--body flag");
        let body = &calls[0].1[body_idx + 1];
        assert!(body.contains("**Pull Request**"));
        assert!(body.contains("https://example.com/pr/1"));
    }

    #[test]
    fn update_estimate_swaps_sp_labels() {
        let cmd = FakeCommandRunner::new();

        // gh issue view
        cmd.push_success(
            &serde_json::to_string(&json!({
                "labels": [{"name": "sp:3"}, {"name": "feature"}]
            }))
            .unwrap(),
        );
        // gh issue edit
        cmd.push_success("");

        let client = test_client(&cmd);
        client.update_estimate("42", 8).unwrap();

        let calls = cmd.calls.borrow();
        let edit_args = &calls[1].1;
        assert!(edit_args.contains(&"--remove-label".to_string()));
        assert!(edit_args.contains(&"sp:3".to_string()));
        assert!(edit_args.contains(&"--add-label".to_string()));
        assert!(edit_args.contains(&"sp:8".to_string()));
    }

    #[test]
    fn get_issue_status_from_label() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!({
                "labels": [{"name": "status:in-progress"}, {"name": "bug"}],
                "state": "OPEN"
            }))
            .unwrap(),
        );

        let client = test_client(&cmd);
        assert_eq!(client.get_issue_status("42").unwrap(), "In Progress");
    }

    #[test]
    fn get_issue_status_fallback_to_github_state() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!({
                "labels": [{"name": "bug"}],
                "state": "CLOSED"
            }))
            .unwrap(),
        );

        let client = test_client(&cmd);
        assert_eq!(client.get_issue_status("42").unwrap(), "Done");
    }

    #[test]
    fn get_issue_state_and_team() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!({
                "labels": [{"name": "status:review"}],
                "state": "OPEN"
            }))
            .unwrap(),
        );

        let client = test_client(&cmd);
        let (state, team) = client.get_issue_state_and_team("42").unwrap();
        assert_eq!(state, "started");
        assert_eq!(team, ""); // GitHub issues return empty team key
    }

    #[test]
    fn list_comments_filters_bot_and_old() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!({
                "comments": [
                    {
                        "body": "Human comment",
                        "createdAt": "2026-04-01T10:00:00Z",
                        "author": {"login": "alice"}
                    },
                    {
                        "body": "**Werma task** status update",
                        "createdAt": "2026-04-01T11:00:00Z",
                        "author": {"login": "werma-bot"}
                    },
                    {
                        "body": "Old comment",
                        "createdAt": "2026-03-01T10:00:00Z",
                        "author": {"login": "bob"}
                    }
                ]
            }))
            .unwrap(),
        );

        let client = test_client(&cmd);
        let comments = client
            .list_comments("42", Some("2026-03-15T00:00:00Z"))
            .unwrap();

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].0, "alice");
        assert_eq!(comments[0].2, "Human comment");
    }

    #[test]
    fn get_sub_issues_parses_checkboxes() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            &serde_json::to_string(&json!({
                "body": "## Tasks\n- [x] First done\n- [ ] Second pending\nNot a task\n"
            }))
            .unwrap(),
        );

        let client = test_client(&cmd);
        let subs = client.get_sub_issues("#42").unwrap();

        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].1, "First done");
        assert_eq!(subs[0].2, "Done");
        assert_eq!(subs[1].1, "Second pending");
        assert_eq!(subs[1].2, "Todo");
    }

    #[test]
    fn remove_label_gh_args() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("");

        let client = test_client(&cmd);
        client.remove_label("42", "old-label").unwrap();

        let calls = cmd.calls.borrow();
        assert!(calls[0].1.contains(&"--remove-label".to_string()));
        assert!(calls[0].1.contains(&"old-label".to_string()));
    }

    #[test]
    fn add_label_gh_args() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("");

        let client = test_client(&cmd);
        client.add_label("42", "new-label").unwrap();

        let calls = cmd.calls.borrow();
        assert!(calls[0].1.contains(&"--add-label".to_string()));
        assert!(calls[0].1.contains(&"new-label".to_string()));
    }

    #[test]
    fn gh_command_failure_returns_error() {
        let cmd = FakeCommandRunner::new();
        cmd.push_failure("not found");

        let client = test_client(&cmd);
        let result = client.get_issue("999");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("gh command failed")
        );
    }
}
