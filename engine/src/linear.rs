use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::db::Db;
use crate::models::{Status, Task};

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

/// Per-team configuration (team_id, team_key, and workflow status mapping).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct TeamConfig {
    pub team_id: String,
    #[serde(default)]
    pub team_key: String,
    pub statuses: std::collections::HashMap<String, String>,
}

/// Configuration stored in ~/.werma/linear.json.
/// Supports both legacy single-team format and new multi-team format.
///
/// Legacy format:   `{ "team_id": "...", "team_key": "RIG", "statuses": {...} }`
/// Multi-team:      `{ "teams": [ { "team_id": "...", "team_key": "RIG", "statuses": {...} }, ... ] }`
#[derive(Debug, Clone)]
pub struct LinearConfig {
    pub teams: Vec<TeamConfig>,
}

/// For backward compatibility: the primary team (first in the list).
impl LinearConfig {
    pub fn primary_team(&self) -> Option<&TeamConfig> {
        self.teams.first()
    }

    /// Look up team config by team_key (e.g. "RIG", "FAT").
    pub fn team_by_key(&self, key: &str) -> Option<&TeamConfig> {
        self.teams.iter().find(|t| t.team_key == key)
    }

    /// All configured team keys.
    pub fn team_keys(&self) -> Vec<&str> {
        self.teams.iter().map(|t| t.team_key.as_str()).collect()
    }

    /// Resolve a status name to a state ID for a given team key.
    /// Falls back to primary team if team_key is empty.
    pub fn status_id(&self, team_key: &str, status_name: &str) -> Option<&String> {
        let team = if team_key.is_empty() {
            self.primary_team()
        } else {
            self.team_by_key(team_key).or(self.primary_team())
        };
        team.and_then(|t| t.statuses.get(status_name))
    }
}

// Custom serde: support both legacy single-team and new multi-team format.
impl serde::Serialize for LinearConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Multi<'a> {
            teams: &'a Vec<TeamConfig>,
        }
        Multi { teams: &self.teams }.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for LinearConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: serde_json::Value = serde::Deserialize::deserialize(deserializer)?;

        // New format: { "teams": [...] }
        if raw.get("teams").is_some() {
            #[derive(serde::Deserialize)]
            struct Multi {
                teams: Vec<TeamConfig>,
            }
            let m: Multi = serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
            return Ok(LinearConfig { teams: m.teams });
        }

        // Legacy format: { "team_id": "...", "team_key": "...", "statuses": {...} }
        let single: TeamConfig = serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
        Ok(LinearConfig {
            teams: vec![single],
        })
    }
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

        Ok(Self {
            client: Client::new(),
            api_key,
        })
    }

    /// Execute a GraphQL query against the Linear API.
    fn query(&self, query: &str, variables: &Value) -> Result<Value> {
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

    /// Discover all teams and their workflow statuses, save to ~/.werma/linear.json.
    pub fn setup(&self) -> Result<()> {
        let config_path = config_path()?;

        // Check if already configured
        if config_path.exists() {
            let existing = load_config()?;
            if !existing.teams.is_empty() {
                let keys: Vec<&str> = existing.team_keys();
                println!(
                    "Already configured: {} team(s): {}",
                    keys.len(),
                    keys.join(", ")
                );
                println!("  Delete ~/.werma/linear.json to reconfigure");
                return Ok(());
            }
        }

        println!("Discovering Linear workspace...");

        // Get all teams
        let data = self.query("{ teams { nodes { id key name } } }", &json!({}))?;
        let api_teams = data["teams"]["nodes"]
            .as_array()
            .context("no teams found")?;

        if api_teams.is_empty() {
            bail!("no teams found in Linear workspace");
        }

        println!("Found {} team(s):", api_teams.len());
        for t in api_teams {
            let name = t["name"].as_str().unwrap_or("?");
            let key = t["key"].as_str().unwrap_or("?");
            println!("  {name} ({key})");
        }

        // Discover statuses for each team
        let mut team_configs = Vec::new();
        for team in api_teams {
            let team_id = team["id"].as_str().context("team has no id")?.to_string();
            let team_key = team["key"].as_str().unwrap_or("").to_string();
            let team_name = team["name"].as_str().unwrap_or("").to_string();

            let statuses = self.discover_team_statuses(&team_id)?;

            println!("\n{team_name} ({team_key}) — {} statuses:", statuses.len());
            for (name, id) in &statuses {
                println!("  {name}: {id}");
            }

            team_configs.push(TeamConfig {
                team_id,
                team_key,
                statuses,
            });
        }

        let config = LinearConfig {
            teams: team_configs,
        };

        save_config(&config)?;
        println!("\nConfig saved to {}", config_path.display());

        Ok(())
    }

    /// Discover workflow statuses for a single team. Extracted from setup() for reuse.
    fn discover_team_statuses(
        &self,
        team_id: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let states_query = r#"
            query($teamId: ID!) {
                workflowStates(filter: { team: { id: { eq: $teamId } } }) {
                    nodes { id name type }
                }
            }
        "#;
        let states_data = self.query(states_query, &json!({"teamId": team_id}))?;
        let states = states_data["workflowStates"]["nodes"]
            .as_array()
            .context("no workflow states")?;

        let mut statuses = std::collections::HashMap::new();

        let find_by_name = |name: &str| -> Option<String> {
            states
                .iter()
                .find(|s| {
                    s["name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
                .and_then(|s| s["id"].as_str().map(String::from))
        };

        let find_by_type = |stype: &str| -> Option<String> {
            states
                .iter()
                .find(|s| s["type"].as_str().is_some_and(|t| t == stype))
                .and_then(|s| s["id"].as_str().map(String::from))
        };

        if let Some(id) = find_by_type("backlog") {
            statuses.insert("backlog".to_string(), id);
        }
        if let Some(id) = find_by_type("unstarted") {
            statuses.insert("todo".to_string(), id);
        }
        if let Some(id) = find_by_type("completed") {
            statuses.insert("done".to_string(), id);
        }
        if let Some(id) = find_by_type("canceled") {
            statuses.insert("canceled".to_string(), id);
        }
        if let Some(id) = find_by_name("Blocked") {
            statuses.insert("blocked".to_string(), id);
        }
        if let Some(id) = find_by_name("In Progress") {
            statuses.insert("in_progress".to_string(), id);
        }
        if let Some(id) = find_by_name("In Review").or_else(|| find_by_name("Review")) {
            statuses.insert("review".to_string(), id);
        }
        if let Some(id) = find_by_name("QA") {
            statuses.insert("qa".to_string(), id);
        }
        if let Some(id) = find_by_name("Ready").or_else(|| find_by_name("Ready for Deploy")) {
            statuses.insert("ready".to_string(), id);
        }
        if let Some(id) = find_by_name("Deploy").or_else(|| find_by_name("Deploying")) {
            statuses.insert("deploy".to_string(), id);
        }
        if let Some(id) = find_by_name("Failed").or_else(|| find_by_name("Deploy Failed")) {
            statuses.insert("failed".to_string(), id);
        }

        Ok(statuses)
    }

    /// Pull Todo issues from Linear and create werma tasks.
    pub fn sync(&self, db: &Db) -> Result<()> {
        let config = load_config()?;
        if config.teams.is_empty() {
            bail!("Linear not configured. Run: werma linear setup");
        }

        let primary = config.primary_team().context("no teams configured")?;
        let todo_status_id = primary
            .statuses
            .get("todo")
            .context("'todo' status not found in linear.json")?;

        let issues_query = r#"
            query($teamId: ID!, $stateId: ID!) {
                issues(
                    filter: {
                        team: { id: { eq: $teamId } },
                        state: { id: { eq: $stateId } }
                    },
                    orderBy: updatedAt
                ) {
                    nodes {
                        id
                        identifier
                        title
                        description
                        priority
                        labels { nodes { name } }
                    }
                }
            }
        "#;

        let data = self.query(
            issues_query,
            &json!({"teamId": primary.team_id, "stateId": todo_status_id}),
        )?;

        let issues = data["issues"]["nodes"]
            .as_array()
            .context("no issues array")?;

        let mut added = 0;
        let mut skipped = 0;

        for issue in issues {
            let issue_id = issue["id"].as_str().unwrap_or("");
            let identifier = issue["identifier"].as_str().unwrap_or("");
            let title = issue["title"].as_str().unwrap_or("");
            let description = issue["description"].as_str().unwrap_or("");
            let priority_num = issue["priority"].as_i64().unwrap_or(0);

            // Skip if already in db
            let existing = db.tasks_by_linear_issue(issue_id, None, false)?;
            if !existing.is_empty() {
                skipped += 1;
                continue;
            }

            // Map priority: Linear 1,2→werma 1; Linear 3,0→werma 2; Linear 4→werma 3
            let werma_priority = map_priority(priority_num);

            // Get labels
            let labels: Vec<&str> = issue["labels"]["nodes"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l["name"].as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            // Skip manual issues — human-driven, agents must not pick up
            if is_manual_issue(&labels) {
                skipped += 1;
                continue;
            }

            let task_type = infer_type_from_labels(&labels);
            let working_dir = infer_working_dir(title, &labels);
            if validate_working_dir(&working_dir).is_none() {
                eprintln!(
                    "  ! skipping {identifier} [{title}]: working dir '{working_dir}' does not exist"
                );
                skipped += 1;
                continue;
            }
            let estimate = issue["estimate"].as_i64().unwrap_or(0) as i32;

            // Build prompt
            let prompt = if description.is_empty() {
                format!("[{identifier}] {title}")
            } else {
                format!("[{identifier}] {title}\n\n{description}")
            };

            let task_id = db.next_task_id()?;
            let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

            let max_turns = crate::default_turns(&task_type);
            let allowed_tools = crate::runner::tools_for_type(&task_type, false);

            let task = Task {
                id: task_id.clone(),
                status: Status::Pending,
                priority: werma_priority,
                created_at: now,
                started_at: None,
                finished_at: None,
                task_type,
                prompt,
                output_path: String::new(),
                working_dir,
                model: "opus".to_string(),
                max_turns,
                allowed_tools,
                session_id: String::new(),
                linear_issue_id: issue_id.to_string(),
                linear_pushed: false,
                pipeline_stage: String::new(),
                depends_on: vec![],
                context_files: vec![],
                repo_hash: crate::runtime_repo_hash(),
                estimate,
            };

            db.insert_task(&task)?;

            // Move issue to In Progress
            if let Some(ip_id) = primary.statuses.get("in_progress") {
                if let Err(e) = self.move_issue(issue_id, ip_id) {
                    eprintln!("warn: failed to move {identifier} to in_progress during sync: {e}");
                }
            }

            println!("  + {task_id} [{identifier}] p{werma_priority}");
            added += 1;
        }

        println!("\nSync: {added} added, {skipped} skipped");
        Ok(())
    }

    /// Push a single task result back to Linear.
    pub fn push(&self, db: &Db, task_id: &str) -> Result<()> {
        let task = db
            .task(task_id)?
            .context(format!("task not found: {task_id}"))?;

        if task.linear_issue_id.is_empty() {
            bail!("task {task_id} has no linear_issue_id");
        }

        // Read output file if exists (first 100 lines)
        let output_preview = if !task.output_path.is_empty() {
            let path = std::path::Path::new(&task.output_path);
            if path.exists() {
                let content = std::fs::read_to_string(path)?;
                let lines: Vec<&str> = content.lines().take(100).collect();
                lines.join("\n")
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Build comment
        let status_str = task.status.to_string();
        let mut comment = format!("**Werma task `{task_id}`** — status: **{status_str}**\n");
        if !output_preview.is_empty() {
            comment.push_str(&format!(
                "\n<details><summary>Output preview</summary>\n\n```\n{output_preview}\n```\n</details>"
            ));
        }

        self.comment(&task.linear_issue_id, &comment)?;

        // If completed, move to Done (uses move_issue_by_name which resolves
        // the correct team's status ID from the issue's team context).
        if task.status == Status::Completed {
            self.move_issue_by_name(&task.linear_issue_id, "done")?;
        }

        db.set_linear_pushed(task_id, true)?;
        println!(
            "pushed: {} -> Linear issue {}",
            task_id, task.linear_issue_id
        );

        Ok(())
    }

    /// Push all completed tasks with linear_issue_id where linear_pushed=false.
    pub fn push_all(&self, db: &Db) -> Result<()> {
        let tasks = db.unpushed_linear_tasks()?;

        if tasks.is_empty() {
            println!("no unpushed tasks");
            return Ok(());
        }

        let mut pushed = 0;
        for task in &tasks {
            match self.push(db, &task.id) {
                Ok(()) => pushed += 1,
                Err(e) => eprintln!("  error pushing {}: {}", task.id, e),
            }
        }

        println!("\npush-all: {pushed} pushed");
        Ok(())
    }

    /// Resolve an issue identifier (e.g. "RIG-95") to a UUID.
    /// If already a UUID (contains no dash-digit pattern), returns as-is.
    fn resolve_uuid(&self, issue_id: &str) -> Result<String> {
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
    fn move_issue(&self, issue_id: &str, state_id: &str) -> Result<()> {
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
            r#"query($issueId: String!) {
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

            // Filter by timestamp if provided
            if let Some(after) = after_iso {
                if created_at.as_str() <= after {
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

    /// Get issues filtered by status name, across all configured teams.
    pub fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>> {
        let config = load_config()?;
        let mut all_issues = Vec::new();

        for team in &config.teams {
            let state_id = match team.statuses.get(status_name) {
                Some(id) if !id.is_empty() => id.clone(),
                _ => continue,
            };

            let data = self.query(
                r#"query($teamId: ID!, $stateId: ID!) {
                    issues(
                        filter: {
                            team: { id: { eq: $teamId } },
                            state: { id: { eq: $stateId } }
                        },
                        orderBy: updatedAt
                    ) {
                        nodes {
                            id
                            identifier
                            title
                            description
                            priority
                            estimate
                            state { type }
                            labels { nodes { name } }
                        }
                    }
                }"#,
                &json!({"teamId": team.team_id, "stateId": state_id}),
            )?;

            if let Some(issues) = data["issues"]["nodes"].as_array() {
                all_issues.extend(issues.clone());
            }
        }

        Ok(all_issues)
    }

    /// Get issues filtered by label name, across all configured teams.
    pub fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<Value>> {
        let config = load_config()?;
        let mut all_issues = Vec::new();

        for team in &config.teams {
            let data = self.query(
                r#"query($teamId: ID!, $label: String!) {
                    issues(
                        filter: {
                            team: { id: { eq: $teamId } },
                            labels: { some: { name: { eqIgnoreCase: $label } } }
                        },
                        orderBy: updatedAt
                    ) {
                        nodes {
                            id
                            identifier
                            title
                            description
                            priority
                            estimate
                            state { type }
                            labels { nodes { id name } }
                        }
                    }
                }"#,
                &json!({"teamId": team.team_id, "label": label_name}),
            )?;

            if let Some(issues) = data["issues"]["nodes"].as_array() {
                all_issues.extend(issues.clone());
            }
        }

        Ok(all_issues)
    }

    /// Remove a label from an issue by label name.
    pub fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;

        // First, get the issue's current labels to find the label ID
        let data = self.query(
            r#"query($id: String!) {
                issue(id: $id) {
                    labels { nodes { id name } }
                }
            }"#,
            &json!({"id": uuid}),
        )?;

        let labels = data["issue"]["labels"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // Collect label IDs, excluding the one to remove
        let remaining_ids: Vec<String> = labels
            .iter()
            .filter(|l| {
                !l["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(label_name))
            })
            .filter_map(|l| l["id"].as_str().map(String::from))
            .collect();

        // Update issue with remaining labels
        self.query(
            r#"mutation($id: String!, $labelIds: [String!]!) {
                issueUpdate(id: $id, input: { labelIds: $labelIds }) { success }
            }"#,
            &json!({"id": uuid, "labelIds": remaining_ids}),
        )?;

        Ok(())
    }

    /// Add a label to an issue by label name.
    pub fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;
        let config = load_config()?;
        let team_key = team_key_from_identifier(issue_id);
        let team = config
            .team_by_key(&team_key)
            .or(config.primary_team())
            .context("no teams configured")?;

        // Find the label ID by name from team labels, and get the issue's current labels
        let data = self.query(
            r#"query($issueId: ID!, $teamId: ID!, $name: String!) {
                issue(id: $issueId) {
                    labels { nodes { id } }
                }
                issueLabels(filter: { team: { id: { eq: $teamId } }, name: { eq: $name } }) {
                    nodes { id }
                }
            }"#,
            &json!({"issueId": uuid, "teamId": team.team_id, "name": label_name}),
        )?;

        let new_label_id = data["issueLabels"]["nodes"][0]["id"]
            .as_str()
            .with_context(|| format!("label '{label_name}' not found in team labels"))?;

        let mut label_ids: Vec<String> = data["issue"]["labels"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|l| l["id"].as_str().map(String::from))
            .collect();

        if !label_ids.iter().any(|id| id == new_label_id) {
            label_ids.push(new_label_id.to_string());
        }

        self.query(
            r#"mutation($id: String!, $labelIds: [String!]!) {
                issueUpdate(id: $id, input: { labelIds: $labelIds }) { success }
            }"#,
            &json!({"id": uuid, "labelIds": label_ids}),
        )?;

        Ok(())
    }
}

// --- Helper functions ---

fn config_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".werma/linear.json"))
}

/// Get the configured team key (e.g. "RIG") from ~/.werma/linear.json.
/// Returns the primary (first) team key for backward compatibility.
pub fn configured_team_key() -> Result<String> {
    let config = load_config()?;
    Ok(config
        .primary_team()
        .map(|t| t.team_key.clone())
        .unwrap_or_default())
}

/// Get all configured team keys (e.g. ["RIG", "FAT"]).
pub fn configured_team_keys() -> Result<Vec<String>> {
    let config = load_config()?;
    Ok(config.teams.iter().map(|t| t.team_key.clone()).collect())
}

/// Extract the team key prefix from an issue identifier (e.g. "RIG-123" → "RIG").
/// Returns empty string for UUIDs or unparseable identifiers.
pub fn team_key_from_identifier(identifier: &str) -> String {
    if let Some(pos) = identifier.rfind('-') {
        let prefix = &identifier[..pos];
        let suffix = &identifier[pos + 1..];
        // Only treat as team key if suffix is all digits (e.g. "RIG-123")
        if suffix.chars().all(|c| c.is_ascii_digit()) && !suffix.is_empty() {
            return prefix.to_string();
        }
    }
    String::new()
}

fn load_config() -> Result<LinearConfig> {
    let path = config_path()?;
    if !path.exists() {
        bail!(
            "Linear not configured. Run: werma linear setup\n  (missing {})",
            path.display()
        );
    }
    let data = std::fs::read_to_string(&path)?;
    let config: LinearConfig = serde_json::from_str(&data)?;
    Ok(config)
}

fn save_config(config: &LinearConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Re-export from config module for convenience.
fn read_env_file_key(key: &str) -> Result<String, std::env::VarError> {
    crate::config::read_env_file_key(key)
}

/// Map Linear priority number to werma priority.
/// Linear: 0=No priority, 1=Urgent, 2=High, 3=Medium, 4=Low
/// Werma: 1=High, 2=Normal, 3=Low
pub fn map_priority(linear_priority: i64) -> i32 {
    match linear_priority {
        1 | 2 => 1,
        3 | 0 => 2,
        4 => 3,
        _ => 2,
    }
}

/// Infer task type from Linear issue labels.
pub fn infer_type_from_labels(labels: &[&str]) -> String {
    let labels_lower: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();

    if labels_lower.iter().any(|l| l.contains("bug")) {
        return "code".to_string();
    }
    if labels_lower.iter().any(|l| l.contains("research")) {
        return "research".to_string();
    }
    if labels_lower.iter().any(|l| l.contains("review")) {
        return "review".to_string();
    }
    if labels_lower
        .iter()
        .any(|l| l.contains("refactor") || l.contains("tech debt"))
    {
        return "refactor".to_string();
    }
    if labels_lower
        .iter()
        .any(|l| l.contains("feature") || l.contains("enhancement"))
    {
        return "code".to_string();
    }

    "code".to_string()
}

/// Check if issue has the `manual` label — human-driven, agents must skip.
pub fn is_manual_issue(labels: &[&str]) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case("manual"))
}

/// Map a `repo:*` label value to its local directory path.
/// All RigpaLabs repos live under `~/projects/rigpa/`.
fn repo_label_to_dir(repo: &str) -> Option<&'static str> {
    match repo.trim() {
        "forge" | "werma" => Some("~/projects/rigpa/werma"),
        "fathom" => Some("~/projects/rigpa/fathom"),
        "hyper-liq" => Some("~/projects/rigpa/hyper-liq"),
        "sui-bots" => Some("~/projects/rigpa/sui-bots"),
        "ar-quant" => Some("~/projects/rigpa/ar-quant"),
        "ar-quant-alpha" => Some("~/projects/rigpa/ar-quant-alpha"),
        "sigil" => Some("~/projects/rigpa/sigil"),
        _ => None,
    }
}

/// Expand `~` to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return format!("{}/{}", home.display(), rest);
    }
    path.to_string()
}

/// Validate that a resolved working directory actually exists on disk.
/// Returns `None` if the path doesn't exist.
pub fn validate_working_dir(dir: &str) -> Option<String> {
    let expanded = expand_tilde(dir);
    if std::path::Path::new(&expanded).is_dir() {
        Some(dir.to_string())
    } else {
        None
    }
}

/// Infer working directory from title keywords and labels.
pub fn infer_working_dir(title: &str, labels: &[&str]) -> String {
    let title_lower = title.to_lowercase();

    // Check for repo: label (explicit mapping takes priority)
    for label in labels {
        if let Some(repo) = label.strip_prefix("repo:") {
            if let Some(dir) = repo_label_to_dir(repo) {
                return dir.to_string();
            }
            // Unknown repo label — fall through to keyword matching
            eprintln!(
                "warning: unknown repo label 'repo:{repo}', falling back to keyword inference"
            );
        }
    }

    // Keyword-based inference
    let keywords: &[(&str, &str)] = &[
        ("werma", "~/projects/rigpa/werma"),
        ("pipeline", "~/projects/rigpa/werma"),
        ("fathom", "~/projects/rigpa/fathom"),
        ("sigil", "~/projects/rigpa/sigil"),
        ("sui", "~/projects/rigpa/sui-bots"),
        ("hyper", "~/projects/rigpa/hyper-liq"),
        ("ar-quant-alpha", "~/projects/rigpa/ar-quant-alpha"),
        ("ar-quant", "~/projects/rigpa/ar-quant"),
    ];

    for (keyword, dir) in keywords {
        if title_lower.contains(keyword) {
            return (*dir).to_string();
        }
    }

    "~/projects/rigpa/werma".to_string()
}

// ─── FakeLinearApi (test-only) ────────────────────────────────────────────────

#[cfg(test)]
pub mod fakes {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Fake LinearApi that records calls and returns pre-configured responses.
    /// Use `set_issues_for_status`/`set_issues_for_label` to configure per-key responses.
    pub struct FakeLinearApi {
        pub issues_by_status: RefCell<HashMap<String, Vec<Value>>>,
        pub issues_by_label: RefCell<HashMap<String, Vec<Value>>>,
        pub issue_details: RefCell<Option<(String, String, String, String, Vec<String>)>>,
        pub move_calls: RefCell<Vec<(String, String)>>,
        pub comment_calls: RefCell<Vec<(String, String)>>,
        pub attach_calls: RefCell<Vec<(String, String, String)>>,
        pub estimate_calls: RefCell<Vec<(String, i32)>>,
        pub remove_label_calls: RefCell<Vec<(String, String)>>,
        pub add_label_calls: RefCell<Vec<(String, String)>>,
    }

    impl FakeLinearApi {
        pub fn new() -> Self {
            Self {
                issues_by_status: RefCell::new(HashMap::new()),
                issues_by_label: RefCell::new(HashMap::new()),
                issue_details: RefCell::new(None),
                move_calls: RefCell::new(vec![]),
                comment_calls: RefCell::new(vec![]),
                attach_calls: RefCell::new(vec![]),
                estimate_calls: RefCell::new(vec![]),
                remove_label_calls: RefCell::new(vec![]),
                add_label_calls: RefCell::new(vec![]),
            }
        }

        /// Set issues returned for a specific status name.
        pub fn set_issues_for_status(&self, status: &str, issues: Vec<Value>) {
            self.issues_by_status
                .borrow_mut()
                .insert(status.to_string(), issues);
        }

        /// Set issues returned for a specific label name.
        pub fn set_issues_for_label(&self, label: &str, issues: Vec<Value>) {
            self.issues_by_label
                .borrow_mut()
                .insert(label.to_string(), issues);
        }
    }

    impl LinearApi for FakeLinearApi {
        fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>> {
            Ok(self
                .issues_by_status
                .borrow()
                .get(status_name)
                .cloned()
                .unwrap_or_default())
        }

        fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<Value>> {
            Ok(self
                .issues_by_label
                .borrow()
                .get(label_name)
                .cloned()
                .unwrap_or_default())
        }

        fn get_issue(&self, _issue_id: &str) -> Result<(String, String)> {
            if let Some(ref d) = *self.issue_details.borrow() {
                Ok((d.2.clone(), d.3.clone()))
            } else {
                Ok((String::new(), String::new()))
            }
        }

        fn get_issue_by_identifier(
            &self,
            _identifier: &str,
        ) -> Result<(String, String, String, String, Vec<String>)> {
            if let Some(ref d) = *self.issue_details.borrow() {
                Ok(d.clone())
            } else {
                bail!("issue not found")
            }
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

        fn get_issue_status(&self, _issue_id: &str) -> Result<String> {
            Ok(String::new())
        }

        fn get_issue_state_and_team(&self, _issue_id: &str) -> Result<(String, String)> {
            Ok(("started".to_string(), "RIG".to_string()))
        }

        fn list_comments(
            &self,
            _issue_id: &str,
            _after_iso: Option<&str>,
        ) -> Result<Vec<(String, String, String)>> {
            Ok(vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_mapping() {
        assert_eq!(map_priority(1), 1); // Urgent -> High
        assert_eq!(map_priority(2), 1); // High -> High
        assert_eq!(map_priority(3), 2); // Medium -> Normal
        assert_eq!(map_priority(0), 2); // No priority -> Normal
        assert_eq!(map_priority(4), 3); // Low -> Low
        assert_eq!(map_priority(99), 2); // Unknown -> Normal
    }

    #[test]
    fn type_inference_from_labels() {
        assert_eq!(infer_type_from_labels(&["Bug"]), "code");
        assert_eq!(infer_type_from_labels(&["bug-fix"]), "code");
        assert_eq!(infer_type_from_labels(&["Research"]), "research");
        assert_eq!(infer_type_from_labels(&["Code Review"]), "review");
        assert_eq!(infer_type_from_labels(&["Refactor"]), "refactor");
        assert_eq!(infer_type_from_labels(&["Tech Debt"]), "refactor");
        assert_eq!(infer_type_from_labels(&["Feature"]), "code");
        assert_eq!(infer_type_from_labels(&["Enhancement"]), "code");
        assert_eq!(infer_type_from_labels(&["random-label"]), "code"); // default
        assert_eq!(infer_type_from_labels(&[]), "code"); // empty labels
    }

    #[test]
    fn working_dir_from_title() {
        assert_eq!(
            infer_working_dir("Fix werma daemon crash", &[]),
            "~/projects/rigpa/werma"
        );
        assert_eq!(
            infer_working_dir("Add pipeline stage", &[]),
            "~/projects/rigpa/werma"
        );
        // Default fallback for unknown titles
        assert_eq!(
            infer_working_dir("Random task title", &[]),
            "~/projects/rigpa/werma"
        );
    }

    #[test]
    fn working_dir_from_repo_label() {
        // Known repo labels map to rigpa/ paths
        assert_eq!(
            infer_working_dir("Some task", &["repo:forge"]),
            "~/projects/rigpa/werma"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:werma"]),
            "~/projects/rigpa/werma"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:fathom"]),
            "~/projects/rigpa/fathom"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:hyper-liq"]),
            "~/projects/rigpa/hyper-liq"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:sui-bots"]),
            "~/projects/rigpa/sui-bots"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:ar-quant"]),
            "~/projects/rigpa/ar-quant"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:ar-quant-alpha"]),
            "~/projects/rigpa/ar-quant-alpha"
        );
        // repo: label takes priority over title keywords
        assert_eq!(
            infer_working_dir("Fix werma bug", &["repo:fathom"]),
            "~/projects/rigpa/fathom"
        );
        // Unknown repo label falls through to keyword inference
        assert_eq!(
            infer_working_dir("Fix werma bug", &["repo:unknown-project"]),
            "~/projects/rigpa/werma"
        );
    }

    #[test]
    fn working_dir_title_keywords() {
        assert_eq!(
            infer_working_dir("sui bot improvements", &[]),
            "~/projects/rigpa/sui-bots"
        );
        assert_eq!(
            infer_working_dir("hyper liquidation fix", &[]),
            "~/projects/rigpa/hyper-liq"
        );
    }

    #[test]
    fn manual_label_detection() {
        assert!(is_manual_issue(&["manual"]));
        assert!(is_manual_issue(&["Manual"]));
        assert!(is_manual_issue(&["MANUAL"]));
        assert!(is_manual_issue(&["Feature", "manual", "repo:werma"]));
        assert!(!is_manual_issue(&["Feature", "Bug"]));
        assert!(!is_manual_issue(&[]));
        assert!(!is_manual_issue(&["manually-created"])); // partial match must NOT trigger
    }

    #[test]
    fn resolve_uuid_detects_identifier_pattern() {
        let is_identifier = |id: &str| -> bool {
            id.contains('-')
                && id
                    .rsplit('-')
                    .next()
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        };

        assert!(is_identifier("RIG-155"));
        assert!(is_identifier("RIG-1"));
        assert!(is_identifier("PROJ-9999"));
        assert!(!is_identifier("755e63ee-a00e-4fef-9d7a-b8907652e2b2"));
        assert!(!is_identifier("no-digits-here"));
        assert!(!is_identifier("plainuuid"));
        assert!(!is_identifier(""));
    }

    #[test]
    fn repo_label_mapping() {
        assert_eq!(repo_label_to_dir("forge"), Some("~/projects/rigpa/werma"));
        assert_eq!(repo_label_to_dir("werma"), Some("~/projects/rigpa/werma"));
        assert_eq!(repo_label_to_dir("fathom"), Some("~/projects/rigpa/fathom"));
        assert_eq!(
            repo_label_to_dir("hyper-liq"),
            Some("~/projects/rigpa/hyper-liq")
        );
        assert_eq!(
            repo_label_to_dir("sui-bots"),
            Some("~/projects/rigpa/sui-bots")
        );
        assert_eq!(
            repo_label_to_dir("ar-quant"),
            Some("~/projects/rigpa/ar-quant")
        );
        assert_eq!(
            repo_label_to_dir("ar-quant-alpha"),
            Some("~/projects/rigpa/ar-quant-alpha")
        );
        assert_eq!(repo_label_to_dir("sigil"), Some("~/projects/rigpa/sigil"));
        assert_eq!(repo_label_to_dir("unknown-repo"), None);
    }

    #[test]
    fn infer_working_dir_repo_label_overrides_keyword() {
        // repo: label should take priority over title keyword matching
        assert_eq!(
            infer_working_dir("Fix fathom collector", &["repo:werma"]),
            "~/projects/rigpa/werma"
        );
    }

    #[test]
    fn infer_working_dir_all_repo_labels() {
        let cases = [
            ("repo:werma", "~/projects/rigpa/werma"),
            ("repo:forge", "~/projects/rigpa/werma"),
            ("repo:fathom", "~/projects/rigpa/fathom"),
            ("repo:hyper-liq", "~/projects/rigpa/hyper-liq"),
            ("repo:sui-bots", "~/projects/rigpa/sui-bots"),
            ("repo:ar-quant", "~/projects/rigpa/ar-quant"),
            ("repo:ar-quant-alpha", "~/projects/rigpa/ar-quant-alpha"),
            ("repo:sigil", "~/projects/rigpa/sigil"),
        ];
        for (label, expected) in cases {
            assert_eq!(
                infer_working_dir("Some task", &[label]),
                expected,
                "failed for label: {label}"
            );
        }
    }

    #[test]
    fn infer_working_dir_unknown_repo_falls_back_to_keyword() {
        // Unknown repo label should fall through to keyword inference
        assert_eq!(
            infer_working_dir("Fix fathom bug", &["repo:nonexistent"]),
            "~/projects/rigpa/fathom"
        );
    }

    #[test]
    fn infer_working_dir_unknown_repo_no_keyword_defaults_to_werma() {
        // Unknown repo label + no keyword match → default werma
        assert_eq!(
            infer_working_dir("Some generic task", &["repo:nonexistent"]),
            "~/projects/rigpa/werma"
        );
    }

    #[test]
    fn infer_working_dir_sigil_keyword() {
        assert_eq!(
            infer_working_dir("Build sigil signal engine", &[]),
            "~/projects/rigpa/sigil"
        );
    }

    #[test]
    fn validate_working_dir_nonexistent() {
        assert!(validate_working_dir("~/projects/nonexistent-xyz-999").is_none());
    }

    #[test]
    fn validate_working_dir_exists() {
        assert!(validate_working_dir("~/").is_some());
    }

    #[test]
    fn expand_tilde_works() {
        let expanded = expand_tilde("~/projects/test");
        assert!(!expanded.starts_with("~/"));
        assert!(expanded.ends_with("/projects/test"));
    }

    #[test]
    fn working_dir_fathom_keyword() {
        assert_eq!(
            infer_working_dir("Fix fathom collector", &[]),
            "~/projects/rigpa/fathom"
        );
    }

    #[test]
    fn working_dir_ar_quant_keywords() {
        assert_eq!(
            infer_working_dir("Update ar-quant-alpha bot", &[]),
            "~/projects/rigpa/ar-quant-alpha"
        );
        assert_eq!(
            infer_working_dir("Fix ar-quant backtesting", &[]),
            "~/projects/rigpa/ar-quant"
        );
    }

    #[test]
    fn read_env_file_key_missing_file() {
        // This tests the error path (file doesn't exist in test env)
        let result = read_env_file_key("NONEXISTENT_KEY");
        assert!(result.is_err());
    }

    #[test]
    fn mutations_use_string_type_not_id() {
        // Regression: Linear mutations must use String!, not ID!.
        // ID! works for queries but causes silent failures in mutations.
        let source = include_str!("linear.rs");
        let bad_lines: Vec<&str> = source
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                trimmed.starts_with("r#\"mutation(") && trimmed.contains("ID!")
            })
            .collect();
        assert!(
            bad_lines.is_empty(),
            "Found mutation(s) using ID! instead of String!:\n{}",
            bad_lines.join("\n")
        );
    }

    // ─── Multi-team config tests ────────────────────────────────────────

    #[test]
    fn multi_team_config_deserialize() {
        let json = r#"{
            "teams": [
                {
                    "team_id": "id-rig",
                    "team_key": "RIG",
                    "statuses": {"todo": "s1", "in_progress": "s2", "done": "s3"}
                },
                {
                    "team_id": "id-fat",
                    "team_key": "FAT",
                    "statuses": {"todo": "s4", "in_progress": "s5", "done": "s6"}
                }
            ]
        }"#;
        let config: LinearConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.teams.len(), 2);
        assert_eq!(config.teams[0].team_key, "RIG");
        assert_eq!(config.teams[1].team_key, "FAT");
        assert_eq!(config.team_keys(), vec!["RIG", "FAT"]);
    }

    #[test]
    fn legacy_single_team_config_deserialize() {
        let json = r#"{
            "team_id": "id-rig",
            "team_key": "RIG",
            "statuses": {"todo": "s1", "done": "s2"}
        }"#;
        let config: LinearConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.teams.len(), 1);
        assert_eq!(config.teams[0].team_key, "RIG");
        assert_eq!(config.teams[0].team_id, "id-rig");
    }

    #[test]
    fn multi_team_config_roundtrip() {
        let config = LinearConfig {
            teams: vec![
                TeamConfig {
                    team_id: "id-1".to_string(),
                    team_key: "RIG".to_string(),
                    statuses: [("todo".to_string(), "s1".to_string())]
                        .into_iter()
                        .collect(),
                },
                TeamConfig {
                    team_id: "id-2".to_string(),
                    team_key: "FAT".to_string(),
                    statuses: [("todo".to_string(), "s2".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
        };
        let json = serde_json::to_string(&config).unwrap();
        let loaded: LinearConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.teams.len(), 2);
        assert_eq!(loaded.team_by_key("FAT").unwrap().team_id, "id-2");
    }

    #[test]
    fn team_by_key_lookup() {
        let config = LinearConfig {
            teams: vec![
                TeamConfig {
                    team_id: "id-rig".to_string(),
                    team_key: "RIG".to_string(),
                    statuses: [("done".to_string(), "rig-done".to_string())]
                        .into_iter()
                        .collect(),
                },
                TeamConfig {
                    team_id: "id-fat".to_string(),
                    team_key: "FAT".to_string(),
                    statuses: [("done".to_string(), "fat-done".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
        };
        assert_eq!(config.team_by_key("RIG").unwrap().team_id, "id-rig");
        assert_eq!(config.team_by_key("FAT").unwrap().team_id, "id-fat");
        assert!(config.team_by_key("UNKNOWN").is_none());
    }

    #[test]
    fn status_id_resolves_per_team() {
        let config = LinearConfig {
            teams: vec![
                TeamConfig {
                    team_id: "id-rig".to_string(),
                    team_key: "RIG".to_string(),
                    statuses: [("in_progress".to_string(), "rig-ip".to_string())]
                        .into_iter()
                        .collect(),
                },
                TeamConfig {
                    team_id: "id-fat".to_string(),
                    team_key: "FAT".to_string(),
                    statuses: [("in_progress".to_string(), "fat-ip".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
        };
        assert_eq!(config.status_id("RIG", "in_progress").unwrap(), "rig-ip");
        assert_eq!(config.status_id("FAT", "in_progress").unwrap(), "fat-ip");
        // Empty team key falls back to primary
        assert_eq!(config.status_id("", "in_progress").unwrap(), "rig-ip");
    }

    #[test]
    fn team_key_from_identifier_extracts_prefix() {
        assert_eq!(team_key_from_identifier("RIG-123"), "RIG");
        assert_eq!(team_key_from_identifier("FAT-42"), "FAT");
        assert_eq!(team_key_from_identifier("AR-1"), "AR");
        // UUIDs should return empty
        assert_eq!(
            team_key_from_identifier("d199cc43-40ef-4e63-9caa-467506b781f6"),
            ""
        );
        // No dash
        assert_eq!(team_key_from_identifier("nodash"), "");
    }
}
