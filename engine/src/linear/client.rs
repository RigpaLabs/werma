use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use super::config::{
    LinearConfig, TeamConfig, config_path, load_config, save_config, team_key_from_identifier,
};
use crate::db::Db;
use crate::models::{Status, Task};

pub(super) const LINEAR_API: &str = "https://api.linear.app/graphql";

/// Compare two ISO 8601 timestamps, returning true if `ts` is strictly after `after`.
/// Handles format mismatches between SQLite (local, no TZ) and Linear (UTC with millis).
/// Falls back to string comparison if chrono parsing fails.
pub fn is_after_timestamp(ts: &str, after: &str) -> bool {
    use chrono::{DateTime, NaiveDateTime, Utc};

    // Try parsing both as full RFC 3339 / ISO 8601 with timezone
    let parse_ts = |s: &str| -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                // Fallback: parse as naive (no timezone) — assume UTC
                NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                    .map(|ndt| ndt.and_utc())
                    .ok()
            })
    };

    match (parse_ts(ts), parse_ts(after)) {
        (Some(t), Some(a)) => t > a,
        _ => ts > after, // fallback to string comparison
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

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("building HTTP client")?;

        Ok(Self { client, api_key })
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
        let force = std::env::var("FORCE_SETUP").is_ok();

        // Check if already configured (skip guard if FORCE_SETUP is set)
        if !force && config_path.exists() {
            let raw = std::fs::read_to_string(&config_path)?;
            let raw_json: Value = serde_json::from_str(&raw)?;

            // Detect legacy single-team format: has "team_id" but no "teams" key
            if raw_json.get("team_id").is_some() && raw_json.get("teams").is_none() {
                println!("Detected legacy single-team config — migrating to multi-team format...");
                return self.migrate_legacy_config(&raw_json);
            }

            let existing = load_config()?;
            if !existing.teams.is_empty() {
                // Check if workspace has more teams than config
                let workspace_team_count = self.count_workspace_teams();
                if let Ok(ws_count) = workspace_team_count {
                    if ws_count > existing.teams.len() {
                        eprintln!(
                            "Warning: config has {} team(s), workspace has {} — run FORCE_SETUP=1 werma linear setup to sync",
                            existing.teams.len(),
                            ws_count
                        );
                    }
                }

                let keys: Vec<&str> = existing.team_keys();
                println!(
                    "Already configured: {} team(s): {}",
                    keys.len(),
                    keys.join(", ")
                );
                println!("  To reconfigure: FORCE_SETUP=1 werma linear setup");
                return Ok(());
            }
        }

        if force {
            println!("FORCE_SETUP=1 — re-discovering all teams...");
        }

        self.discover_and_save_all_teams()
    }

    /// Migrate legacy single-team config to multi-team format.
    /// Preserves existing team's status IDs and discovers any additional workspace teams.
    fn migrate_legacy_config(&self, legacy_json: &Value) -> Result<()> {
        let legacy_team: TeamConfig =
            serde_json::from_value(legacy_json.clone()).context("parsing legacy config")?;
        let legacy_team_id = legacy_team.team_id.clone();
        println!(
            "  Existing team: {} ({})",
            legacy_team.team_key, legacy_team.team_id
        );

        // Discover all workspace teams
        let data = self.query("{ teams { nodes { id key name } } }", &json!({}))?;
        let api_teams = data["teams"]["nodes"]
            .as_array()
            .context("no teams found")?;

        let mut team_configs = Vec::new();

        for team in api_teams {
            let team_id = team["id"].as_str().context("team has no id")?.to_string();
            let team_key = team["key"].as_str().unwrap_or("").to_string();
            let team_name = team["name"].as_str().unwrap_or("").to_string();

            if team_id == legacy_team_id {
                // Preserve existing team's status IDs
                println!("  Keeping existing statuses for {team_key}");
                team_configs.push(legacy_team.clone());
            } else {
                // Discover statuses for new team
                let statuses = self.discover_team_statuses(&team_id)?;
                println!(
                    "  Discovered {team_name} ({team_key}) — {} statuses",
                    statuses.len()
                );
                team_configs.push(TeamConfig {
                    team_id,
                    team_key,
                    statuses,
                });
            }
        }

        let config = LinearConfig {
            teams: team_configs,
        };
        save_config(&config)?;
        let config_path = config_path()?;
        println!(
            "Migrated to multi-team format: {} team(s) — {}",
            config.teams.len(),
            config_path.display()
        );
        Ok(())
    }

    /// Count teams in the Linear workspace (cheap query, used for mismatch warning).
    fn count_workspace_teams(&self) -> Result<usize> {
        let data = self.query("{ teams { nodes { id } } }", &json!({}))?;
        let teams = data["teams"]["nodes"]
            .as_array()
            .context("no teams found")?;
        Ok(teams.len())
    }

    /// Discover all workspace teams and save config. Shared by setup() and FORCE_SETUP path.
    fn discover_and_save_all_teams(&self) -> Result<()> {
        println!("Discovering Linear workspace...");

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
        let config_path = config_path()?;
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
                        estimate
                        state { type }
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

            // RIG-307: skip issues with empty id or identifier to prevent ghost tasks
            if issue_id.is_empty() || identifier.is_empty() {
                continue;
            }

            // Skip if already in db (use identifier, not UUID, for consistency with poll dedup)
            let existing = db.tasks_by_linear_issue(identifier, None, false)?;
            if !existing.is_empty() {
                skipped += 1;
                continue;
            }

            // Map priority: Linear 1,2→werma 1; Linear 3,0→werma 2; Linear 4→werma 3
            let werma_priority = super::config::map_priority(priority_num);

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
            if super::config::is_manual_issue(&labels) {
                skipped += 1;
                continue;
            }

            let task_type = super::config::infer_type_from_labels(&labels);
            let user_cfg = crate::config::UserConfig::load();
            let working_dir = super::config::infer_working_dir(title, &labels, &user_cfg);
            if super::config::validate_working_dir(&working_dir).is_none() {
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
                linear_issue_id: identifier.to_string(),
                linear_pushed: false,
                pipeline_stage: String::new(),
                depends_on: vec![],
                context_files: vec![],
                repo_hash: crate::runtime_repo_hash(),
                estimate,
                retry_count: 0,
                retry_after: None,
                cost_usd: None,
                turns_used: 0,
                handoff_content: String::new(),
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

    /// Fetch child (sub) issues of a parent issue by identifier (e.g. "RIG-236").
    /// Returns vec of (identifier, title, status_name, description).
    /// Returns empty vec if the issue has no children.
    pub fn get_sub_issues(
        &self,
        identifier: &str,
    ) -> Result<Vec<(String, String, String, String)>> {
        let uuid = self.resolve_uuid(identifier)?;

        let data = self.query(
            r#"query($issueId: ID!) {
                issue(id: $issueId) {
                    children(first: 50, orderBy: createdAt) {
                        nodes {
                            identifier
                            title
                            description
                            state { name }
                        }
                    }
                }
            }"#,
            &json!({"issueId": uuid}),
        )?;

        let nodes = data["issue"]["children"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut sub_issues = Vec::new();
        for node in &nodes {
            let ident = node["identifier"].as_str().unwrap_or("").to_string();
            let title = node["title"].as_str().unwrap_or("").to_string();
            let status = node["state"]["name"].as_str().unwrap_or("").to_string();
            let description = node["description"].as_str().unwrap_or("").to_string();
            sub_issues.push((ident, title, status, description));
        }

        Ok(sub_issues)
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

// ─── Re-export from config module for convenience ────────────────────────────

fn read_env_file_key(key: &str) -> Result<String, std::env::VarError> {
    crate::config::read_env_file_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn read_env_file_key_missing_file() {
        // This tests the error path (file doesn't exist in test env)
        let result = read_env_file_key("NONEXISTENT_KEY");
        assert!(result.is_err());
    }

    #[test]
    fn is_after_timestamp_same_format() {
        // Both full ISO 8601 with timezone
        assert!(is_after_timestamp(
            "2026-03-24T16:00:00.000Z",
            "2026-03-24T15:00:00.000Z"
        ));
        assert!(!is_after_timestamp(
            "2026-03-24T14:00:00.000Z",
            "2026-03-24T15:00:00.000Z"
        ));
    }

    #[test]
    fn is_after_timestamp_mixed_formats() {
        // SQLite naive (no TZ) vs Linear RFC 3339 (with Z)
        // Both treated as UTC for comparison
        assert!(is_after_timestamp(
            "2026-03-24T16:00:00.000Z",
            "2026-03-24T15:00:00"
        ));
        assert!(!is_after_timestamp(
            "2026-03-24T14:00:00.000Z",
            "2026-03-24T15:00:00"
        ));
    }

    #[test]
    fn is_after_timestamp_equal_is_not_after() {
        assert!(!is_after_timestamp(
            "2026-03-24T15:00:00.000Z",
            "2026-03-24T15:00:00"
        ));
    }

    #[test]
    fn mutations_use_string_type_not_id() {
        // Regression: Linear mutations must use String!, not ID!.
        // ID! works for queries but causes silent failures in mutations.
        let source = include_str!("client.rs");
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
}
