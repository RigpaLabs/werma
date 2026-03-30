use anyhow::{Context, Result};

use crate::db::Db;
use crate::models::{Status, Task};
use crate::{runner, worktree};

use super::super::display::*;

/// Parameters for the add command — avoids too-many-arguments.
pub struct AddParams {
    pub prompt: String,
    pub output: Option<String>,
    pub priority: i32,
    pub task_type: String,
    pub model: String,
    pub tools: Option<String>,
    pub dir: Option<String>,
    pub turns: Option<i32>,
    pub depends: Option<String>,
    pub context: Option<String>,
    pub linear: Option<String>,
    pub stage: Option<String>,
    pub runtime: String,
}

pub fn cmd_add(db: &Db, p: AddParams) -> Result<()> {
    let id = db.next_task_id()?;
    let runtime: crate::models::AgentRuntime = p.runtime.parse()?;
    let max_turns = p.turns.unwrap_or_else(|| default_turns(&p.task_type));
    let has_output = p.output.is_some();
    let allowed_tools = p
        .tools
        .unwrap_or_else(|| runner::tools_for_type(&p.task_type, has_output));
    let working_dir = expand_tilde(&p.dir.unwrap_or_else(default_working_dir));
    let output_path = p.output.map(|o| expand_tilde(&o)).unwrap_or_default();
    let depends_on: Vec<String> = p
        .depends
        .map(|d| d.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let context_files: Vec<String> = p
        .context
        .map(|c| c.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let task = Task {
        id: id.clone(),
        status: Status::Pending,
        priority: p.priority,
        created_at: now,
        started_at: None,
        finished_at: None,
        task_type: p.task_type.clone(),
        prompt: p.prompt.clone(),
        output_path: output_path.clone(),
        working_dir,
        model: p.model.clone(),
        max_turns,
        allowed_tools,
        session_id: String::new(),
        linear_issue_id: p
            .linear
            .unwrap_or_else(|| worktree::extract_linear_id_prefix(&p.prompt).unwrap_or_default()),
        linear_pushed: false,
        pipeline_stage: p.stage.unwrap_or_default(),
        depends_on: depends_on.clone(),
        context_files: context_files.clone(),
        repo_hash: crate::runtime_repo_hash(),
        estimate: 0,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content: String::new(),
        runtime,
    };

    db.insert_task(&task)?;

    let runtime_suffix = if runtime != crate::models::AgentRuntime::ClaudeCode {
        format!(", {runtime}")
    } else {
        String::new()
    };
    println!(
        "added: {id} ({}, p{}, {}, {max_turns}t{runtime_suffix})",
        p.task_type, p.priority, p.model
    );
    if !output_path.is_empty() {
        println!("  output: {output_path}");
    }
    if !depends_on.is_empty() {
        println!("  depends: {}", depends_on.join(","));
    }
    if !context_files.is_empty() {
        println!("  context: {}", context_files.join(","));
    }
    println!("  prompt: {}...", truncate(&p.prompt, 80));

    Ok(())
}

pub fn cmd_list(db: &Db, status_filter: Option<&str>) -> Result<()> {
    let status = status_filter.map(str::parse::<Status>).transpose()?;

    let tasks = db.list_tasks(status)?;

    if tasks.is_empty() {
        println!("\n  (no tasks)\n");
        return Ok(());
    }

    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0)
        .unwrap_or(100);

    println!();
    let table = crate::ui::task_list_table(&tasks, term_width);
    println!("{table}");
    println!();

    Ok(())
}

pub fn cmd_view(db: &Db, id: &str) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;

    println!();
    println!("  id:          {}", task.id);
    println!(
        "  status:      {} {}",
        status_icon(task.status),
        task.status
    );
    println!("  type:        {}", task.task_type);
    println!("  priority:    {}", task.priority);
    println!(
        "  model:       {} ({})",
        task.model,
        runner::model_flag(&task.model)
    );
    if task.runtime != crate::models::AgentRuntime::ClaudeCode {
        println!("  runtime:     {}", task.runtime);
    }
    println!("  max_turns:   {}", task.max_turns);
    println!("  working_dir: {}", task.working_dir);
    println!("  created_at:  {}", task.created_at);
    if let Some(ref s) = task.started_at {
        println!("  started_at:  {s}");
    }
    if let Some(ref s) = task.finished_at {
        println!("  finished_at: {s}");
    }
    if !task.output_path.is_empty() {
        println!("  output_path: {}", task.output_path);
    }
    if !task.session_id.is_empty() {
        println!("  session_id:  {}", task.session_id);
    }
    if !task.linear_issue_id.is_empty() {
        println!("  linear:      {}", task.linear_issue_id);
    }
    if !task.pipeline_stage.is_empty() {
        println!("  stage:       {}", task.pipeline_stage);
    }
    if !task.depends_on.is_empty() {
        println!("  depends_on:  {}", task.depends_on.join(", "));
    }
    if !task.context_files.is_empty() {
        println!("  context:     {}", task.context_files.join(", "));
    }
    if !task.repo_hash.is_empty() {
        println!("  repo_hash:   {}", task.repo_hash);
    }
    if !task.allowed_tools.is_empty() {
        println!("  tools:       {}", task.allowed_tools);
    }
    println!();
    println!("  prompt:");
    println!("  {}", task.prompt);
    println!();

    // Check custom output path first, then fall back to default log output
    let output_shown = if !task.output_path.is_empty() {
        let path = std::path::Path::new(&task.output_path);
        if path.exists() {
            println!("  --- output ---");
            let content = std::fs::read_to_string(path)?;
            println!("{content}");
            true
        } else {
            false
        }
    } else {
        false
    };

    if !output_shown {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        let log_output = home
            .join(".werma/logs")
            .join(format!("{}-output.md", task.id));
        if log_output.exists() {
            println!("  --- output ---");
            let content = std::fs::read_to_string(&log_output)?;
            println!("{content}");
        }
    }

    Ok(())
}

pub fn cmd_log(id: Option<String>) -> Result<()> {
    let logs_dir = crate::werma_dir()?.join("logs");

    if let Some(task_id) = id {
        let log_path = logs_dir.join(format!("{task_id}.log"));
        if log_path.exists() {
            let content = std::fs::read_to_string(&log_path)?;
            print!("{content}");
        } else {
            println!("log not found: {task_id}");
        }
    } else {
        let mut entries: Vec<_> = std::fs::read_dir(&logs_dir)?
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
            .collect();

        entries.sort_by_key(|e| {
            std::cmp::Reverse(
                e.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            )
        });

        if let Some(entry) = entries.first() {
            let content = std::fs::read_to_string(entry.path())?;
            print!("{content}");
        } else {
            println!("no logs found");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn cmd_add_creates_task() {
        let db = test_db();
        cmd_add(
            &db,
            AddParams {
                prompt: "test prompt".into(),
                output: None,
                priority: 2,
                task_type: "research".into(),
                model: "sonnet".into(),
                tools: None,
                dir: Some("/tmp".into()),
                turns: Some(5),
                depends: None,
                context: None,
                linear: None,
                stage: None,
                runtime: "claude-code".into(),
            },
        )
        .unwrap();

        let tasks = db.list_tasks(Some(Status::Pending)).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].prompt, "test prompt");
        assert_eq!(tasks[0].max_turns, 5);
        assert_eq!(tasks[0].task_type, "research");
    }

    #[test]
    fn cmd_add_with_depends_and_context() {
        let db = test_db();
        cmd_add(
            &db,
            AddParams {
                prompt: "test".into(),
                output: Some("/tmp/out.md".into()),
                priority: 1,
                task_type: "code".into(),
                model: "opus".into(),
                tools: None,
                dir: Some("/tmp".into()),
                turns: None,
                depends: Some("dep1,dep2".into()),
                context: Some("file1.md,file2.md".into()),
                linear: Some("RIG-42".into()),
                stage: Some("engineer".into()),
                runtime: "claude-code".into(),
            },
        )
        .unwrap();

        let tasks = db.list_tasks(Some(Status::Pending)).unwrap();
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.depends_on, vec!["dep1", "dep2"]);
        assert_eq!(t.context_files, vec!["file1.md", "file2.md"]);
        assert_eq!(t.linear_issue_id, "RIG-42");
        assert_eq!(t.pipeline_stage, "engineer");
        assert_eq!(t.max_turns, 30); // default for "code"
    }

    #[test]
    fn cmd_list_empty() {
        let db = test_db();
        cmd_list(&db, None).unwrap();
    }

    #[test]
    fn cmd_list_with_invalid_status() {
        let db = test_db();
        let result = cmd_list(&db, Some("bogus"));
        assert!(result.is_err());
    }

    #[test]
    fn cmd_view_nonexistent_task() {
        let db = test_db();
        let result = cmd_view(&db, "nonexistent");
        assert!(result.is_err());
    }
}
