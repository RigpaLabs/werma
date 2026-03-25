use anyhow::{Context, Result, bail};
use serde_json::json;

use super::api::LinearClient;
use super::config::load_config;
use super::helpers::{
    infer_type_from_labels, infer_working_dir, is_manual_issue, map_priority, validate_working_dir,
};
use crate::db::Db;
use crate::models::{Status, Task};

impl LinearClient {
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
            let user_cfg = crate::config::UserConfig::load();
            let working_dir = infer_working_dir(title, &labels, &user_cfg);
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
                retry_count: 0,
                retry_after: None,
                cost_usd: None,
                turns_used: 0,
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
}
