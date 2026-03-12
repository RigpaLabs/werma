use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::db::Db;
use crate::models::{Status, Task};

const LINEAR_API: &str = "https://api.linear.app/graphql";

/// Configuration stored in ~/.werma/linear.json.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct LinearConfig {
    pub team_id: String,
    #[serde(default)]
    pub team_key: String,
    pub statuses: std::collections::HashMap<String, String>,
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

    /// Discover team and workflow statuses, save to ~/.werma/linear.json.
    pub fn setup(&self) -> Result<()> {
        let config_path = config_path()?;

        // Check if already configured
        if config_path.exists() {
            let existing = load_config()?;
            if !existing.team_id.is_empty() {
                println!(
                    "Already configured: {} ({})",
                    existing.team_key, existing.team_id
                );
                println!("  Delete ~/.werma/linear.json to reconfigure");
                return Ok(());
            }
        }

        println!("Discovering Linear workspace...");

        // Get teams
        let data = self.query("{ teams { nodes { id key name } } }", &json!({}))?;
        let teams = data["teams"]["nodes"]
            .as_array()
            .context("no teams found")?;

        if teams.is_empty() {
            bail!("no teams found in Linear workspace");
        }

        let team = &teams[0];
        let team_id = team["id"].as_str().context("team has no id")?.to_string();
        let team_key = team["key"].as_str().unwrap_or("").to_string();
        let team_name = team["name"].as_str().unwrap_or("").to_string();

        if teams.len() > 1 {
            println!("Multiple teams found, using first:");
            for t in teams {
                let name = t["name"].as_str().unwrap_or("?");
                let key = t["key"].as_str().unwrap_or("?");
                println!("  {} ({})", name, key);
            }
        }
        println!("Team: {} ({})", team_name, team_key);

        // Get workflow statuses for this team
        let states_query = r#"
            query($teamId: String!) {
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

        // Map by name (case-insensitive) and type
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

        // Core statuses (by type)
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

        // Name-based statuses
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

        let config = LinearConfig {
            team_id,
            team_key,
            statuses,
        };

        save_config(&config)?;
        println!("Config saved to {}", config_path.display());

        // Print discovered statuses
        println!("\nStatuses:");
        for (name, id) in &config.statuses {
            println!("  {}: {}", name, id);
        }

        Ok(())
    }

    /// Pull Todo issues from Linear and create werma tasks.
    pub fn sync(&self, db: &Db) -> Result<()> {
        let config = load_config()?;
        if config.team_id.is_empty() {
            bail!("Linear not configured. Run: werma linear setup");
        }

        let todo_status_id = config
            .statuses
            .get("todo")
            .context("'todo' status not found in linear.json")?;

        let issues_query = r#"
            query($teamId: String!, $stateId: String!) {
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
            &json!({"teamId": config.team_id, "stateId": todo_status_id}),
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
                    "  ! skipping {} [{}]: working dir '{}' does not exist",
                    identifier, title, working_dir
                );
                skipped += 1;
                continue;
            }
            let estimate = issue["estimate"].as_i64().unwrap_or(0) as i32;

            // Build prompt
            let prompt = if description.is_empty() {
                format!("[{}] {}", identifier, title)
            } else {
                format!("[{}] {}\n\n{}", identifier, title, description)
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
            if let Some(ip_id) = config.statuses.get("in_progress") {
                let _ = self.move_issue(issue_id, ip_id);
            }

            println!("  + {} [{}] p{}", task_id, identifier, werma_priority);
            added += 1;
        }

        println!("\nSync: {} added, {} skipped", added, skipped);
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
        let mut comment = format!(
            "**Werma task `{}`** — status: **{}**\n",
            task_id, status_str
        );
        if !output_preview.is_empty() {
            comment.push_str(&format!(
                "\n<details><summary>Output preview</summary>\n\n```\n{}\n```\n</details>",
                output_preview
            ));
        }

        self.comment(&task.linear_issue_id, &comment)?;

        // If completed, move to Done
        if task.status == Status::Completed {
            let config = load_config()?;
            if let Some(done_id) = config.statuses.get("done") {
                self.move_issue(&task.linear_issue_id, done_id)?;
            }
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

        println!("\npush-all: {} pushed", pushed);
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
    pub fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
        let config = load_config()?;
        let state_id = config
            .statuses
            .get(status_name)
            .context(format!("unknown status: {status_name}"))?;
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

    /// Fetch a single issue by identifier (e.g. "RIG-95").
    /// Returns (uuid, identifier, title, description, labels).
    pub fn get_issue_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<(String, String, String, String, Vec<String>)> {
        let config = load_config()?;
        // Parse "RIG-95" → number 95
        let number: i64 = identifier
            .rsplit('-')
            .next()
            .and_then(|n| n.parse().ok())
            .with_context(|| format!("invalid identifier: {identifier}"))?;

        let data = self.query(
            r#"query($teamId: String!, $number: Float!) {
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
            &json!({"teamId": config.team_id, "number": number}),
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

    /// Get issues filtered by team and status name.
    pub fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>> {
        let config = load_config()?;
        let state_id = match config.statuses.get(status_name) {
            Some(id) if !id.is_empty() => id.clone(),
            _ => return Ok(vec![]),
        };

        let data = self.query(
            r#"query($teamId: String!, $stateId: String!) {
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
            }"#,
            &json!({"teamId": config.team_id, "stateId": state_id}),
        )?;

        Ok(data["issues"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
}

// --- Helper functions ---

fn config_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".werma/linear.json"))
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
                "warning: unknown repo label 'repo:{}', falling back to keyword inference",
                repo
            );
        }
    }

    // Keyword-based inference
    let keywords: &[(&str, &str)] = &[
        ("werma", "~/projects/rigpa/werma"),
        ("pipeline", "~/projects/rigpa/werma"),
        ("fathom", "~/projects/rigpa/fathom"),
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
        assert_eq!(repo_label_to_dir("unknown-repo"), None);
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
}
