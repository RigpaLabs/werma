use std::path::Path;

use anyhow::{Context, Result};

// ─── CommandOutput ───────────────────────────────────────────────────────────

/// Output from a command execution.
/// Custom struct because `std::process::ExitStatus` has no public constructor.
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

// ─── CommandRunner trait ─────────────────────────────────────────────────────

/// Trait abstracting external command execution for testability.
/// All `Command::new("git"|"tmux"|"gh"|"osascript")` calls go through this.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<CommandOutput>;
}

/// Real implementation using std::process::Command.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<CommandOutput> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        let output = cmd.output().with_context(|| format!("running {program}"))?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

// ─── Notifier trait ──────────────────────────────────────────────────────────

/// Trait abstracting notifications (macOS + Slack) for testability.
/// Used by pipeline callback and move_with_retry for alert notifications.
pub trait Notifier {
    fn notify_macos(&self, title: &str, message: &str, sound: &str);
    fn notify_slack(&self, channel: &str, text: &str);
}

/// Real notifier using osascript and Slack API.
pub struct RealNotifier;

impl Notifier for RealNotifier {
    fn notify_macos(&self, title: &str, message: &str, sound: &str) {
        crate::notify::notify_macos(title, message, sound);
    }

    fn notify_slack(&self, channel: &str, text: &str) {
        crate::notify::notify_slack(channel, text);
    }
}

// ─── Fakes (test-only) ──────────────────────────────────────────────────────

#[cfg(test)]
pub mod fakes {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    /// Fake command runner with a FIFO queue of pre-programmed responses.
    /// Unmatched calls return success with empty output.
    pub struct FakeCommandRunner {
        responses: RefCell<VecDeque<CommandOutput>>,
        pub calls: RefCell<Vec<(String, Vec<String>, Option<String>)>>,
    }

    impl FakeCommandRunner {
        pub fn new() -> Self {
            Self {
                responses: RefCell::new(VecDeque::new()),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub fn push_success(&self, stdout: &str) {
            self.responses.borrow_mut().push_back(CommandOutput {
                success: true,
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            });
        }

        pub fn push_failure(&self, stderr: &str) {
            self.responses.borrow_mut().push_back(CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            });
        }
    }

    impl CommandRunner for FakeCommandRunner {
        fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<CommandOutput> {
            self.calls.borrow_mut().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
                dir.map(|d| d.to_string_lossy().to_string()),
            ));

            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .unwrap_or(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                }))
        }
    }

    /// Fake notifier that records calls for assertion.
    pub struct FakeNotifier {
        pub macos_calls: RefCell<Vec<(String, String, String)>>,
        pub slack_calls: RefCell<Vec<(String, String)>>,
    }

    impl FakeNotifier {
        pub fn new() -> Self {
            Self {
                macos_calls: RefCell::new(Vec::new()),
                slack_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Notifier for FakeNotifier {
        fn notify_macos(&self, title: &str, message: &str, sound: &str) {
            self.macos_calls.borrow_mut().push((
                title.to_string(),
                message.to_string(),
                sound.to_string(),
            ));
        }

        fn notify_slack(&self, channel: &str, text: &str) {
            self.slack_calls
                .borrow_mut()
                .push((channel.to_string(), text.to_string()));
        }
    }

    /// Fake Linear API that records all calls and supports configurable failures.
    pub struct FakeLinearApi {
        pub move_calls: RefCell<Vec<(String, String)>>,
        pub comment_calls: RefCell<Vec<(String, String)>>,
        pub attach_calls: RefCell<Vec<(String, String, String)>>,
        pub estimate_calls: RefCell<Vec<(String, i32)>>,
        pub remove_label_calls: RefCell<Vec<(String, String)>>,
        pub add_label_calls: RefCell<Vec<(String, String)>>,
        pub issues_by_status: RefCell<std::collections::HashMap<String, Vec<serde_json::Value>>>,
        pub issues_by_label: RefCell<std::collections::HashMap<String, Vec<serde_json::Value>>>,
        pub issue_data: RefCell<std::collections::HashMap<String, (String, String)>>,
        /// Maps issue_id -> current status name (for get_issue_status reconciliation).
        pub issue_status: RefCell<std::collections::HashMap<String, String>>,
        /// Maps issue_id -> (state_type, team_key) for get_issue_state_and_team.
        pub issue_state_and_team: RefCell<std::collections::HashMap<String, (String, String)>>,
        /// Maps issue_id -> vec of (author, created_at, body) comments.
        pub issue_comments:
            RefCell<std::collections::HashMap<String, Vec<(String, String, String)>>>,
        /// Maps identifier -> vec of (identifier, title, status, description) sub-issues.
        pub sub_issues:
            RefCell<std::collections::HashMap<String, Vec<(String, String, String, String)>>>,
        fail_next_moves: RefCell<u32>,
    }

    #[allow(dead_code)]
    impl FakeLinearApi {
        pub fn new() -> Self {
            Self {
                move_calls: RefCell::new(Vec::new()),
                comment_calls: RefCell::new(Vec::new()),
                attach_calls: RefCell::new(Vec::new()),
                estimate_calls: RefCell::new(Vec::new()),
                remove_label_calls: RefCell::new(Vec::new()),
                add_label_calls: RefCell::new(Vec::new()),
                issues_by_status: RefCell::new(std::collections::HashMap::new()),
                issues_by_label: RefCell::new(std::collections::HashMap::new()),
                issue_data: RefCell::new(std::collections::HashMap::new()),
                issue_status: RefCell::new(std::collections::HashMap::new()),
                issue_state_and_team: RefCell::new(std::collections::HashMap::new()),
                issue_comments: RefCell::new(std::collections::HashMap::new()),
                sub_issues: RefCell::new(std::collections::HashMap::new()),
                fail_next_moves: RefCell::new(0),
            }
        }

        /// Set sub-issues that will be returned by get_sub_issues for an identifier.
        pub fn set_sub_issues(
            &self,
            identifier: &str,
            children: Vec<(String, String, String, String)>,
        ) {
            self.sub_issues
                .borrow_mut()
                .insert(identifier.to_string(), children);
        }

        /// Set comments that will be returned by list_comments for an issue.
        pub fn set_issue_comments(&self, issue_id: &str, comments: Vec<(String, String, String)>) {
            self.issue_comments
                .borrow_mut()
                .insert(issue_id.to_string(), comments);
        }

        /// Set the status that get_issue_status will return for an issue.
        pub fn set_issue_status(&self, issue_id: &str, status: &str) {
            self.issue_status
                .borrow_mut()
                .insert(issue_id.to_string(), status.to_string());
        }

        /// Set state type and team key for get_issue_state_and_team.
        pub fn set_issue_state_and_team(&self, issue_id: &str, state_type: &str, team_key: &str) {
            self.issue_state_and_team.borrow_mut().insert(
                issue_id.to_string(),
                (state_type.to_string(), team_key.to_string()),
            );
        }

        /// Configure the next N move_issue_by_name calls to return Err.
        pub fn fail_next_n_moves(&self, n: u32) {
            *self.fail_next_moves.borrow_mut() = n;
        }

        /// Add issues that will be returned by get_issues_by_status.
        pub fn set_issues_for_status(&self, status: &str, issues: Vec<serde_json::Value>) {
            self.issues_by_status
                .borrow_mut()
                .insert(status.to_string(), issues);
        }

        /// Add issues that will be returned by get_issues_by_label.
        pub fn set_issues_for_label(&self, label: &str, issues: Vec<serde_json::Value>) {
            self.issues_by_label
                .borrow_mut()
                .insert(label.to_string(), issues);
        }

        /// Set issue data returned by get_issue.
        pub fn set_issue_data(&self, id: &str, title: &str, description: &str) {
            self.issue_data
                .borrow_mut()
                .insert(id.to_string(), (title.to_string(), description.to_string()));
        }
    }

    impl crate::linear::LinearApi for FakeLinearApi {
        fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<serde_json::Value>> {
            Ok(self
                .issues_by_status
                .borrow()
                .get(status_name)
                .cloned()
                .unwrap_or_default())
        }

        fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<serde_json::Value>> {
            Ok(self
                .issues_by_label
                .borrow()
                .get(label_name)
                .cloned()
                .unwrap_or_default())
        }

        fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
            let mut fail_count = self.fail_next_moves.borrow_mut();
            if *fail_count > 0 {
                *fail_count -= 1;
                return Err(anyhow::anyhow!(
                    "fake move failure: {} -> {}",
                    issue_id,
                    status_name
                ));
            }
            self.move_calls
                .borrow_mut()
                .push((issue_id.to_string(), status_name.to_string()));
            // Auto-update issue_status so reconciliation checks see the new status
            self.issue_status
                .borrow_mut()
                .insert(issue_id.to_string(), status_name.to_string());
            Ok(())
        }

        fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
            self.comment_calls
                .borrow_mut()
                .push((issue_id.to_string(), body.to_string()));
            Ok(())
        }

        fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> Result<()> {
            self.attach_calls.borrow_mut().push((
                issue_id.to_string(),
                url.to_string(),
                title.to_string(),
            ));
            Ok(())
        }

        fn update_estimate(&self, issue_id: &str, estimate: i32) -> Result<()> {
            self.estimate_calls
                .borrow_mut()
                .push((issue_id.to_string(), estimate));
            Ok(())
        }

        fn get_issue(&self, issue_id: &str) -> Result<(String, String)> {
            Ok(self
                .issue_data
                .borrow()
                .get(issue_id)
                .cloned()
                .unwrap_or_default())
        }

        fn get_issue_by_identifier(
            &self,
            identifier: &str,
        ) -> Result<(String, String, String, String, Vec<String>)> {
            let (title, desc) = self
                .issue_data
                .borrow()
                .get(identifier)
                .cloned()
                .unwrap_or_default();
            Ok((
                format!("fake-uuid-{identifier}"),
                identifier.to_string(),
                title,
                desc,
                vec![],
            ))
        }

        fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
            self.remove_label_calls
                .borrow_mut()
                .push((issue_id.to_string(), label_name.to_string()));
            Ok(())
        }

        fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
            self.add_label_calls
                .borrow_mut()
                .push((issue_id.to_string(), label_name.to_string()));
            Ok(())
        }

        fn get_issue_status(&self, issue_id: &str) -> Result<String> {
            Ok(self
                .issue_status
                .borrow()
                .get(issue_id)
                .cloned()
                .unwrap_or_default())
        }

        fn get_issue_state_and_team(&self, issue_id: &str) -> Result<(String, String)> {
            // If explicitly set, return that.
            if let Some(result) = self.issue_state_and_team.borrow().get(issue_id) {
                return Ok(result.clone());
            }
            // Derive from issue_status: map status name to Linear state type.
            let status = self
                .issue_status
                .borrow()
                .get(issue_id)
                .cloned()
                .unwrap_or_default();
            let state_type = match status.as_str() {
                "canceled" => "canceled",
                "done" => "completed",
                "backlog" => "backlog",
                "todo" => "unstarted",
                _ => "started",
            };
            // Default team: "RIG"
            Ok((state_type.to_string(), "RIG".to_string()))
        }

        fn list_comments(
            &self,
            issue_id: &str,
            after_iso: Option<&str>,
        ) -> Result<Vec<(String, String, String)>> {
            let all = self
                .issue_comments
                .borrow()
                .get(issue_id)
                .cloned()
                .unwrap_or_default();
            if let Some(after) = after_iso {
                Ok(all
                    .into_iter()
                    .filter(|(_, ts, _)| crate::linear::is_after_timestamp(ts.as_str(), after))
                    .collect())
            } else {
                Ok(all)
            }
        }

        fn get_sub_issues(
            &self,
            identifier: &str,
        ) -> Result<Vec<(String, String, String, String)>> {
            Ok(self
                .sub_issues
                .borrow()
                .get(identifier)
                .cloned()
                .unwrap_or_default())
        }
    }

    // ─── StatefulFakeLinearApi ────────────────────────────────────────────────

    /// Stateful issue representation for StatefulFakeLinearApi.
    /// `id` is the Linear UUID-like issue ID; `identifier` is the human-readable one (e.g. "RIG-42").
    #[derive(Debug, Clone)]
    pub struct IssueState {
        pub id: String,
        pub identifier: String,
        pub title: String,
        pub description: String,
        pub status: String,
        pub labels: Vec<String>,
        pub estimate: Option<i32>,
    }

    /// Call record for StatefulFakeLinearApi call tracking.
    #[derive(Debug, Clone)]
    pub enum ApiCall {
        Move {
            identifier: String,
            status: String,
        },
        Comment {
            identifier: String,
            body: String,
        },
        AddLabel {
            identifier: String,
            label: String,
        },
        RemoveLabel {
            identifier: String,
            label: String,
        },
        AttachUrl {
            identifier: String,
            url: String,
            title: String,
        },
        UpdateEstimate {
            identifier: String,
            estimate: i32,
        },
    }

    /// Status name → Linear state_type mapping.
    fn status_to_state_type(status: &str) -> &'static str {
        match status.to_lowercase().as_str() {
            "backlog" => "backlog",
            "todo" => "unstarted",
            "in_progress" => "started",
            "review" => "started",
            "done" => "completed",
            "canceled" => "canceled",
            _ => "backlog",
        }
    }

    /// A stateful fake Linear API for integration testing.
    ///
    /// Maintains real issue state (status, labels) and tracks all API calls.
    /// Supports `fail_next_n_moves(n)` for retry testing.
    /// Issues are keyed by their ID string (UUID-like); a separate index maps
    /// identifier (e.g. "RIG-42") → ID.
    pub struct StatefulFakeLinearApi {
        /// Map from issue ID → issue state
        issues: RefCell<HashMap<String, IssueState>>,
        /// Map from identifier (e.g. "RIG-42") → issue ID
        identifier_to_uuid: RefCell<HashMap<String, String>>,
        pub calls: RefCell<Vec<ApiCall>>,
        fail_next_moves: RefCell<u32>,
    }

    impl StatefulFakeLinearApi {
        pub fn new() -> Self {
            Self {
                issues: RefCell::new(HashMap::new()),
                identifier_to_uuid: RefCell::new(HashMap::new()),
                calls: RefCell::new(Vec::new()),
                fail_next_moves: RefCell::new(0),
            }
        }

        /// Seed an issue into the fake.
        pub fn add_issue(
            &self,
            id: &str,
            identifier: &str,
            title: &str,
            description: &str,
            status: &str,
            labels: Vec<String>,
        ) {
            self.add_issue_with_estimate(id, identifier, title, description, status, labels, None);
        }

        /// Seed an issue with a specific estimate (story points).
        pub fn add_issue_with_estimate(
            &self,
            id: &str,
            identifier: &str,
            title: &str,
            description: &str,
            status: &str,
            labels: Vec<String>,
            estimate: Option<i32>,
        ) {
            let state = IssueState {
                id: id.to_string(),
                identifier: identifier.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                status: status.to_string(),
                labels,
                estimate,
            };
            self.issues.borrow_mut().insert(id.to_string(), state);
            self.identifier_to_uuid
                .borrow_mut()
                .insert(identifier.to_string(), id.to_string());
        }

        /// Move an issue by identifier to a new status (bypasses fail_next_moves).
        pub fn move_issue_by_name_direct(&self, identifier: &str, new_status: &str) {
            if let Some(id) = self.identifier_to_uuid.borrow().get(identifier).cloned() {
                if let Some(issue) = self.issues.borrow_mut().get_mut(&id) {
                    issue.status = new_status.to_string();
                }
            }
        }

        /// Get all issues with a given status.
        pub fn get_issues_by_status_vec(&self, status: &str) -> Vec<IssueState> {
            self.issues
                .borrow()
                .values()
                .filter(|i| i.status == status)
                .cloned()
                .collect()
        }

        /// Get all issues with a given label.
        pub fn get_issues_by_label_vec(&self, label: &str) -> Vec<IssueState> {
            self.issues
                .borrow()
                .values()
                .filter(|i| i.labels.iter().any(|l| l == label))
                .cloned()
                .collect()
        }

        /// Configure the next N move_issue_by_name calls to return Err.
        pub fn fail_next_n_moves(&self, n: u32) {
            *self.fail_next_moves.borrow_mut() = n;
        }

        /// Get the current status of an issue by identifier.
        pub fn issue_status(&self, identifier: &str) -> Option<String> {
            let id = self.identifier_to_uuid.borrow().get(identifier).cloned()?;
            self.issues.borrow().get(&id).map(|i| i.status.clone())
        }

        /// Get the current labels of an issue by identifier.
        pub fn issue_labels(&self, identifier: &str) -> Vec<String> {
            self.identifier_to_uuid
                .borrow()
                .get(identifier)
                .cloned()
                .and_then(|id| self.issues.borrow().get(&id).map(|i| i.labels.clone()))
                .unwrap_or_default()
        }

        /// Build a serde_json::Value for an issue (for get_issues_by_status/label).
        fn issue_to_json(issue: &IssueState) -> serde_json::Value {
            let label_nodes: Vec<serde_json::Value> = issue
                .labels
                .iter()
                .map(|l| serde_json::json!({"name": l}))
                .collect();
            let state_type = status_to_state_type(&issue.status);
            let estimate = issue.estimate.unwrap_or(3);
            serde_json::json!({
                "id": issue.id.to_string(),
                "identifier": issue.identifier,
                "title": issue.title,
                "description": issue.description,
                "estimate": estimate,
                "state": {"type": state_type},
                "labels": {"nodes": label_nodes}
            })
        }
    }

    impl crate::linear::LinearApi for StatefulFakeLinearApi {
        fn get_issues_by_status(
            &self,
            status_name: &str,
        ) -> anyhow::Result<Vec<serde_json::Value>> {
            Ok(self
                .issues
                .borrow()
                .values()
                .filter(|i| i.status == status_name)
                .map(Self::issue_to_json)
                .collect())
        }

        fn get_issues_by_label(&self, label_name: &str) -> anyhow::Result<Vec<serde_json::Value>> {
            Ok(self
                .issues
                .borrow()
                .values()
                .filter(|i| i.labels.iter().any(|l| l == label_name))
                .map(Self::issue_to_json)
                .collect())
        }

        fn get_issue(&self, issue_id: &str) -> anyhow::Result<(String, String)> {
            // Try direct lookup by ID first, then by identifier
            if let Some(issue) = self.issues.borrow().get(issue_id) {
                return Ok((issue.title.clone(), issue.description.clone()));
            }
            // Try as identifier
            if let Some(uuid) = self.identifier_to_uuid.borrow().get(issue_id).cloned() {
                if let Some(issue) = self.issues.borrow().get(&uuid) {
                    return Ok((issue.title.clone(), issue.description.clone()));
                }
            }
            Ok((String::new(), String::new()))
        }

        fn get_issue_by_identifier(
            &self,
            identifier: &str,
        ) -> anyhow::Result<(String, String, String, String, Vec<String>)> {
            if let Some(id) = self.identifier_to_uuid.borrow().get(identifier).cloned() {
                if let Some(issue) = self.issues.borrow().get(&id) {
                    return Ok((
                        id.clone(),
                        issue.identifier.clone(),
                        issue.title.clone(),
                        issue.description.clone(),
                        issue.labels.clone(),
                    ));
                }
            }
            Ok((
                format!("fake-uuid-{identifier}"),
                identifier.to_string(),
                String::new(),
                String::new(),
                vec![],
            ))
        }

        fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> anyhow::Result<()> {
            let mut fail_count = self.fail_next_moves.borrow_mut();
            if *fail_count > 0 {
                *fail_count -= 1;
                return Err(anyhow::anyhow!(
                    "stateful fake move failure: {} -> {}",
                    issue_id,
                    status_name
                ));
            }
            drop(fail_count);

            // Try direct ID lookup first (issue UUID), then identifier
            let identifier = if let Some(issue) = self.issues.borrow_mut().get_mut(issue_id) {
                issue.status = status_name.to_string();
                issue.identifier.clone()
            } else if let Some(uuid) = self.identifier_to_uuid.borrow().get(issue_id).cloned() {
                if let Some(issue) = self.issues.borrow_mut().get_mut(&uuid) {
                    issue.status = status_name.to_string();
                }
                issue_id.to_string()
            } else {
                issue_id.to_string()
            };

            self.calls.borrow_mut().push(ApiCall::Move {
                identifier,
                status: status_name.to_string(),
            });
            Ok(())
        }

        fn comment(&self, issue_id: &str, body: &str) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(ApiCall::Comment {
                identifier: issue_id.to_string(),
                body: body.to_string(),
            });
            Ok(())
        }

        fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(ApiCall::AttachUrl {
                identifier: issue_id.to_string(),
                url: url.to_string(),
                title: title.to_string(),
            });
            Ok(())
        }

        fn update_estimate(&self, issue_id: &str, estimate: i32) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(ApiCall::UpdateEstimate {
                identifier: issue_id.to_string(),
                estimate,
            });
            Ok(())
        }

        fn remove_label(&self, issue_id: &str, label_name: &str) -> anyhow::Result<()> {
            let identifier = if let Some(issue) = self.issues.borrow_mut().get_mut(issue_id) {
                issue.labels.retain(|l| l != label_name);
                issue.identifier.clone()
            } else if let Some(uuid) = self.identifier_to_uuid.borrow().get(issue_id).cloned() {
                if let Some(issue) = self.issues.borrow_mut().get_mut(&uuid) {
                    issue.labels.retain(|l| l != label_name);
                }
                issue_id.to_string()
            } else {
                issue_id.to_string()
            };
            self.calls.borrow_mut().push(ApiCall::RemoveLabel {
                identifier,
                label: label_name.to_string(),
            });
            Ok(())
        }

        fn add_label(&self, issue_id: &str, label_name: &str) -> anyhow::Result<()> {
            let identifier = if let Some(issue) = self.issues.borrow_mut().get_mut(issue_id) {
                if !issue.labels.contains(&label_name.to_string()) {
                    issue.labels.push(label_name.to_string());
                }
                issue.identifier.clone()
            } else if let Some(uuid) = self.identifier_to_uuid.borrow().get(issue_id).cloned() {
                if let Some(issue) = self.issues.borrow_mut().get_mut(&uuid) {
                    if !issue.labels.contains(&label_name.to_string()) {
                        issue.labels.push(label_name.to_string());
                    }
                }
                issue_id.to_string()
            } else {
                issue_id.to_string()
            };
            self.calls.borrow_mut().push(ApiCall::AddLabel {
                identifier,
                label: label_name.to_string(),
            });
            Ok(())
        }

        fn get_issue_status(&self, issue_id: &str) -> anyhow::Result<String> {
            if let Some(issue) = self.issues.borrow().get(issue_id) {
                return Ok(issue.status.clone());
            }
            if let Some(uuid) = self.identifier_to_uuid.borrow().get(issue_id).cloned() {
                if let Some(issue) = self.issues.borrow().get(&uuid) {
                    return Ok(issue.status.clone());
                }
            }
            Ok(String::new())
        }

        fn get_issue_state_and_team(&self, issue_id: &str) -> anyhow::Result<(String, String)> {
            let find_issue = |id: &str| -> Option<(String, String)> {
                self.issues.borrow().get(id).map(|i| {
                    let state_type = status_to_state_type(&i.status).to_string();
                    // Derive team key from identifier prefix (e.g. "RIG-42" → "RIG")
                    let team_key = i.identifier.split('-').next().unwrap_or("").to_string();
                    (state_type, team_key)
                })
            };

            if let Some(result) = find_issue(issue_id) {
                return Ok(result);
            }
            if let Some(uuid) = self.identifier_to_uuid.borrow().get(issue_id).cloned() {
                if let Some(result) = find_issue(&uuid) {
                    return Ok(result);
                }
            }
            Ok((String::new(), String::new()))
        }

        fn list_comments(
            &self,
            _issue_id: &str,
            _after_iso: Option<&str>,
        ) -> anyhow::Result<Vec<(String, String, String)>> {
            // StatefulFakeLinearApi doesn't track comments — return empty
            Ok(vec![])
        }

        fn get_sub_issues(
            &self,
            _identifier: &str,
        ) -> anyhow::Result<Vec<(String, String, String, String)>> {
            // StatefulFakeLinearApi doesn't track sub-issues — return empty
            Ok(vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fakes::{FakeCommandRunner, FakeNotifier, StatefulFakeLinearApi};

    #[test]
    fn command_output_string_helpers() {
        let output = CommandOutput {
            success: true,
            stdout: b"hello world\n".to_vec(),
            stderr: b"warn\n".to_vec(),
        };
        assert_eq!(output.stdout_str(), "hello world");
        assert_eq!(output.stderr_str(), "warn");
    }

    #[test]
    fn real_command_runner_executes() {
        let cmd = RealCommandRunner;
        let output = cmd.run("echo", &["hello"], None).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout_str(), "hello");
    }

    #[test]
    fn fake_command_runner_fifo() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("output1");
        cmd.push_success("output2");
        cmd.push_failure("error");

        let r1 = cmd.run("git", &["status"], None).unwrap();
        assert!(r1.success);
        assert_eq!(r1.stdout_str(), "output1");

        let r2 = cmd.run("git", &["log"], None).unwrap();
        assert!(r2.success);
        assert_eq!(r2.stdout_str(), "output2");

        let r3 = cmd.run("gh", &["pr", "list"], None).unwrap();
        assert!(!r3.success);
        assert_eq!(r3.stderr_str(), "error");

        // Default: empty success
        let r4 = cmd.run("anything", &[], None).unwrap();
        assert!(r4.success);
        assert!(r4.stdout.is_empty());
    }

    #[test]
    fn fake_command_runner_records_calls() {
        let cmd = FakeCommandRunner::new();
        let dir = std::path::Path::new("/tmp");
        cmd.run("git", &["fetch"], Some(dir)).unwrap();

        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "git");
        assert_eq!(calls[0].1, vec!["fetch"]);
        assert_eq!(calls[0].2, Some("/tmp".to_string()));
    }

    #[test]
    fn fake_notifier_records_calls() {
        let n = FakeNotifier::new();
        n.notify_macos("title", "msg", "sound");
        n.notify_slack("#ch", "text");

        assert_eq!(n.macos_calls.borrow().len(), 1);
        assert_eq!(n.slack_calls.borrow().len(), 1);
        assert_eq!(n.macos_calls.borrow()[0].0, "title");
        assert_eq!(n.slack_calls.borrow()[0].1, "text");
    }

    // ─── StatefulFakeLinearApi tests ─────────────────────────────────────────

    use crate::linear::LinearApi;

    #[test]
    fn stateful_fake_get_issues_by_status() {
        let fake = StatefulFakeLinearApi::new();
        fake.add_issue("uuid-1", "RIG-1", "Issue 1", "desc", "in_progress", vec![]);
        fake.add_issue("uuid-2", "RIG-2", "Issue 2", "desc", "todo", vec![]);
        fake.add_issue("uuid-3", "RIG-3", "Issue 3", "desc", "in_progress", vec![]);

        let issues = fake.get_issues_by_status("in_progress").unwrap();
        assert_eq!(issues.len(), 2);
        let identifiers: Vec<&str> = issues
            .iter()
            .map(|i| i["identifier"].as_str().unwrap())
            .collect();
        assert!(identifiers.contains(&"RIG-1"));
        assert!(identifiers.contains(&"RIG-3"));
    }

    #[test]
    fn stateful_fake_move_updates_status() {
        let fake = StatefulFakeLinearApi::new();
        fake.add_issue("uuid-10", "RIG-10", "Title", "desc", "todo", vec![]);

        fake.move_issue_by_name("uuid-10", "in_progress").unwrap();

        assert_eq!(fake.issue_status("RIG-10"), Some("in_progress".to_string()));
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            fakes::ApiCall::Move { identifier, status } => {
                assert_eq!(identifier, "RIG-10");
                assert_eq!(status, "in_progress");
            }
            _ => panic!("expected Move call"),
        }
    }

    #[test]
    fn stateful_fake_add_remove_label() {
        let fake = StatefulFakeLinearApi::new();
        fake.add_issue(
            "uuid-20",
            "RIG-20",
            "Title",
            "desc",
            "backlog",
            vec!["analyze".to_string()],
        );

        // Remove label by identifier
        fake.remove_label("RIG-20", "analyze").unwrap();
        assert!(
            !fake.issue_labels("RIG-20").contains(&"analyze".to_string()),
            "label should be removed"
        );

        // Add new label
        fake.add_label("RIG-20", "analyze:done").unwrap();
        assert!(
            fake.issue_labels("RIG-20")
                .contains(&"analyze:done".to_string()),
            "new label should be added"
        );
    }

    #[test]
    fn stateful_fake_get_issues_by_label() {
        let fake = StatefulFakeLinearApi::new();
        fake.add_issue(
            "uuid-30",
            "RIG-30",
            "T1",
            "d",
            "backlog",
            vec!["analyze".to_string()],
        );
        fake.add_issue(
            "uuid-31",
            "RIG-31",
            "T2",
            "d",
            "backlog",
            vec!["feature".to_string()],
        );
        fake.add_issue(
            "uuid-32",
            "RIG-32",
            "T3",
            "d",
            "backlog",
            vec!["analyze".to_string(), "repo:werma".to_string()],
        );

        let issues = fake.get_issues_by_label("analyze").unwrap();
        assert_eq!(issues.len(), 2);
        let identifiers: Vec<&str> = issues
            .iter()
            .map(|i| i["identifier"].as_str().unwrap())
            .collect();
        assert!(identifiers.contains(&"RIG-30"));
        assert!(identifiers.contains(&"RIG-32"));
    }

    #[test]
    fn stateful_fake_fail_next_n_moves_retry() {
        let fake = StatefulFakeLinearApi::new();
        fake.add_issue("uuid-40", "RIG-40", "T", "d", "todo", vec![]);
        fake.fail_next_n_moves(2);

        // First two moves fail
        assert!(fake.move_issue_by_name("uuid-40", "in_progress").is_err());
        assert!(fake.move_issue_by_name("uuid-40", "in_progress").is_err());
        // Third succeeds
        assert!(fake.move_issue_by_name("uuid-40", "in_progress").is_ok());

        assert_eq!(fake.issue_status("RIG-40"), Some("in_progress".to_string()));
    }

    #[test]
    fn stateful_fake_call_recording() {
        let fake = StatefulFakeLinearApi::new();
        fake.add_issue("uuid-50", "RIG-50", "Title", "desc", "backlog", vec![]);

        fake.move_issue_by_name("uuid-50", "in_progress").unwrap();
        fake.comment("RIG-50", "hello").unwrap();
        fake.add_label("uuid-50", "label-x").unwrap();
        fake.remove_label("uuid-50", "label-x").unwrap();

        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 4);

        // Verify call types in order
        matches!(&calls[0], fakes::ApiCall::Move { .. });
        matches!(&calls[1], fakes::ApiCall::Comment { .. });
        matches!(&calls[2], fakes::ApiCall::AddLabel { .. });
        matches!(&calls[3], fakes::ApiCall::RemoveLabel { .. });
    }
}
