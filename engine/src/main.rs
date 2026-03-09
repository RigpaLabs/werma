mod backup;
mod cli;
mod config;
mod daemon;
mod dashboard;
#[allow(dead_code)]
mod db;
#[allow(dead_code)]
mod linear;
mod migrate;
#[allow(dead_code)]
mod models;
#[allow(dead_code)]
mod notify;
#[allow(dead_code)]
mod pipeline;
mod runner;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::db::Db;
use crate::models::{Schedule, Status, Task};

/// Returns ~/.werma/ and creates it (+ subdirs) if needed.
fn werma_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let dir = home.join(".werma");
    std::fs::create_dir_all(dir.join("logs"))?;
    std::fs::create_dir_all(dir.join("completed"))?;
    std::fs::create_dir_all(dir.join("backups"))?;
    Ok(dir)
}

fn open_db() -> Result<Db> {
    let dir = werma_dir()?;
    Db::open(&dir.join("werma.db"))
}

/// Map short model name to full Claude model ID.
/// Canonical version in runner::model_flag — this delegates to it.
fn model_to_id(model: &str) -> &str {
    runner::model_flag(model)
}

/// Default max_turns based on task type.
fn default_turns(task_type: &str) -> i32 {
    match task_type {
        "code" | "refactor" | "full" | "pipeline-engineer" => 30,
        "research" | "pipeline-analyst" => 20,
        "review" | "analyze" => 10,
        "pipeline-reviewer" | "pipeline-qa" | "pipeline-devops" => 15,
        _ => 15,
    }
}

/// Status icon for display.
fn status_icon(status: Status) -> &'static str {
    match status {
        Status::Pending => "○",
        Status::Running => "◉",
        Status::Completed => "✓",
        Status::Failed => "✗",
    }
}

/// Truncate string to max chars, append "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() <= max {
        first_line.to_string()
    } else {
        let mut result: String = first_line.chars().take(max).collect();
        result.push_str("...");
        result
    }
}

/// Expand ~ to home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().to_string();
    }
    path.to_string()
}

/// Parameters for the add command — avoids too-many-arguments.
struct AddParams {
    prompt: String,
    output: Option<String>,
    priority: i32,
    task_type: String,
    model: String,
    tools: Option<String>,
    dir: String,
    turns: Option<i32>,
    depends: Option<String>,
    context: Option<String>,
    linear: Option<String>,
    stage: Option<String>,
}

fn cmd_add(db: &Db, p: AddParams) -> Result<()> {
    let id = db.next_task_id()?;
    let max_turns = p.turns.unwrap_or_else(|| default_turns(&p.task_type));
    let has_output = p.output.is_some();
    let allowed_tools = p
        .tools
        .unwrap_or_else(|| runner::tools_for_type(&p.task_type, has_output));
    let working_dir = expand_tilde(&p.dir);
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
        linear_issue_id: p.linear.unwrap_or_default(),
        linear_pushed: false,
        pipeline_stage: p.stage.unwrap_or_default(),
        depends_on: depends_on.clone(),
        context_files: context_files.clone(),
    };

    db.insert_task(&task)?;

    println!(
        "added: {id} ({}, p{}, {}, {max_turns}t)",
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

fn cmd_list(db: &Db, status_filter: Option<&str>) -> Result<()> {
    let status = status_filter.map(str::parse::<Status>).transpose()?;

    let tasks = db.list_tasks(status)?;

    if tasks.is_empty() {
        println!("\n  (no tasks)\n");
        return Ok(());
    }

    println!();
    for task in &tasks {
        let icon = status_icon(task.status);
        let deps_str = if task.depends_on.is_empty() {
            String::new()
        } else {
            format!(" [->{}]", task.depends_on.join(","))
        };
        let prompt_preview = truncate(&task.prompt, 50);
        println!(
            " {icon}  {:<14} {:<9} p{}  {:<7}  {prompt_preview}{deps_str}",
            task.id, task.task_type, task.priority, task.model
        );
    }
    println!();

    Ok(())
}

fn cmd_status(db: &Db) -> Result<()> {
    let (p, r, c, f) = db.task_counts()?;

    println!();
    println!(" ○ pending:   {p}");
    println!(" ◉ running:   {r}");
    println!(" ✓ completed: {c}");
    println!(" ✗ failed:    {f}");
    println!();

    let output = std::process::Command::new("tmux").args(["ls"]).output();

    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let sessions: Vec<&str> = stdout.lines().filter(|l| l.starts_with("werma-")).collect();
        if !sessions.is_empty() {
            println!(" tmux sessions:");
            for s in sessions {
                println!("   {s}");
            }
            println!();
        }
    }

    Ok(())
}

fn cmd_view(db: &Db, id: &str) -> Result<()> {
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
        model_to_id(&task.model)
    );
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
    if !task.allowed_tools.is_empty() {
        println!("  tools:       {}", task.allowed_tools);
    }
    println!();
    println!("  prompt:");
    println!("  {}", task.prompt);
    println!();

    // Check custom output path first, then fall back to default log output
    let output_shown = if !task.output_path.is_empty() {
        let path = Path::new(&task.output_path);
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

fn cmd_retry(db: &Db, id: &str) -> Result<()> {
    let _task = db.task(id)?.context(format!("task not found: {id}"))?;

    db.set_task_status(id, Status::Pending)?;
    db.update_task_field(id, "started_at", "")?;
    db.update_task_field(id, "finished_at", "")?;

    println!("retry: {id} -> pending");
    Ok(())
}

fn cmd_kill(db: &Db, id: &str) -> Result<()> {
    let _task = db.task(id)?.context(format!("task not found: {id}"))?;

    let session_name = format!("werma-{id}");
    let result = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &session_name])
        .output();

    match result {
        Ok(out) if out.status.success() => println!("killed tmux: {session_name}"),
        _ => println!("tmux session not found: {session_name}"),
    }

    db.set_task_status(id, Status::Failed)?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    db.update_task_field(id, "finished_at", &now)?;

    println!("status -> failed: {id}");
    Ok(())
}

fn cmd_clean(db: &Db) -> Result<()> {
    let tasks = db.clean_completed()?;

    if tasks.is_empty() {
        println!("nothing to clean");
        return Ok(());
    }

    let dir = werma_dir()?.join("completed");
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let archive_path = dir.join(format!("{today}.json"));

    let mut existing: Vec<serde_json::Value> = if archive_path.exists() {
        let content = std::fs::read_to_string(&archive_path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    for task in &tasks {
        let val = serde_json::to_value(task)?;
        existing.push(val);
    }

    let json = serde_json::to_string_pretty(&existing)?;
    std::fs::write(&archive_path, json)?;

    println!(
        "archived: {} tasks -> {}",
        tasks.len(),
        archive_path.display()
    );
    Ok(())
}

fn cmd_log(id: Option<String>) -> Result<()> {
    let logs_dir = werma_dir()?.join("logs");

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

fn cmd_continue(db: &Db, id: &str, prompt: Option<String>) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;

    if task.session_id.is_empty() {
        bail!("no session_id for task {id}");
    }

    let follow_up = prompt.unwrap_or_else(|| "Continue the task.".to_string());
    let model_id = model_to_id(&task.model);
    let session_name = format!("werma-{id}-cont");
    let wdir = werma_dir()?;
    let logs_dir = wdir.join("logs");
    let log_file = logs_dir.join(format!("{id}.log"));
    let prompt_file = logs_dir.join(format!("{id}-cont-prompt.txt"));
    let exec_script = logs_dir.join(format!("{id}-cont-exec.sh"));

    // Write prompt to file — never interpolate into shell
    std::fs::write(&prompt_file, &follow_up)?;

    let tools = if task.allowed_tools.is_empty() {
        runner::tools_for_type(&task.task_type, !task.output_path.is_empty())
    } else {
        task.allowed_tools.clone()
    };

    let working_dir = expand_tilde(&task.working_dir);

    // Generate safe exec script
    let script = format!(
        r##"#!/bin/bash
set -euo pipefail
unset CLAUDECODE
cd '{working_dir}'
PROMPT=$(cat '{prompt_file}')
claude -p "$PROMPT" \
    --resume '{session_id}' \
    --allowedTools '{tools}' \
    --model {model_id} \
    2>> '{log_file}'
osascript -e 'display notification "{id} continue done" with title "werma" sound name "Glass"' 2>/dev/null || true
"##,
        working_dir = working_dir,
        prompt_file = prompt_file.display(),
        session_id = task.session_id.replace('\'', "'\\''"),
        tools = tools.replace('\'', "'\\''"),
        model_id = model_id,
        log_file = log_file.display(),
        id = id,
    );

    std::fs::write(&exec_script, &script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exec_script, std::fs::Permissions::from_mode(0o755))?;
    }

    let result = std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            &format!("bash {}", exec_script.display()),
        ])
        .output();

    match result {
        Ok(out) if out.status.success() => {
            println!("continue: {id} -> tmux: {session_name}");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("tmux failed: {stderr}");
        }
        Err(e) => bail!("failed to spawn tmux: {e}"),
    }

    Ok(())
}

/// Parameters for sched add — avoids too-many-arguments.
struct SchedAddParams {
    id: String,
    cron: String,
    prompt: String,
    task_type: String,
    model: String,
    output: Option<String>,
    context: Option<String>,
    dir: String,
    turns: Option<i32>,
}

fn cmd_sched_add(db: &Db, p: SchedAddParams) -> Result<()> {
    let working_dir = expand_tilde(&p.dir);
    let output_path = p.output.map(|o| expand_tilde(&o)).unwrap_or_default();
    let max_turns = p.turns.unwrap_or(0);
    let context_files: Vec<String> = p
        .context
        .map(|c| c.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let sched = Schedule {
        id: p.id.clone(),
        cron_expr: p.cron.clone(),
        prompt: p.prompt.clone(),
        schedule_type: p.task_type.clone(),
        model: p.model.clone(),
        output_path: output_path.clone(),
        working_dir: working_dir.clone(),
        max_turns,
        enabled: true,
        context_files: context_files.clone(),
        last_enqueued: String::new(),
    };

    db.insert_schedule(&sched)?;

    println!("scheduled: {}", p.id);
    println!("  cron: {}", p.cron);
    println!("  type: {}, model: {}", p.task_type, p.model);
    println!("  dir: {working_dir}");
    if !output_path.is_empty() {
        println!("  output: {output_path}");
    }
    if !context_files.is_empty() {
        println!("  context: {}", context_files.join(","));
    }
    if max_turns > 0 {
        println!("  turns: {max_turns}");
    }
    println!("  prompt: {}...", truncate(&p.prompt, 70));

    Ok(())
}

fn cmd_sched_list(db: &Db) -> Result<()> {
    let schedules = db.list_schedules()?;

    println!();
    println!(" Schedules:");
    println!();

    if schedules.is_empty() {
        println!("  (empty)");
    } else {
        for s in &schedules {
            let icon = if s.enabled { "✓" } else { "○" };
            let prompt_preview = truncate(&s.prompt, 42);
            println!(
                " {icon}  {:<16} {:<15} {:<8} {:<7} {prompt_preview}",
                s.id, s.cron_expr, s.schedule_type, s.model
            );
            if !s.last_enqueued.is_empty() {
                println!("    last: {}", s.last_enqueued);
            }
        }
    }
    println!();

    Ok(())
}

fn cmd_sched_trigger(db: &Db, id: &str) -> Result<()> {
    let sched = db
        .schedule(id)?
        .context(format!("schedule not found: {id}"))?;

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let dow = chrono::Local::now().format("%A").to_string().to_lowercase();

    let prompt = sched
        .prompt
        .replace("{date}", &today)
        .replace("{dow}", &dow);

    let output = if sched.output_path.is_empty() {
        None
    } else {
        Some(sched.output_path.replace("{date}", &today))
    };

    let turns = if sched.max_turns > 0 {
        Some(sched.max_turns)
    } else {
        None
    };

    let context = if sched.context_files.is_empty() {
        None
    } else {
        Some(sched.context_files.join(","))
    };

    cmd_add(
        db,
        AddParams {
            prompt,
            output,
            priority: 2,
            task_type: sched.schedule_type,
            model: sched.model,
            tools: None,
            dir: sched.working_dir,
            turns,
            depends: None,
            context,
            linear: None,
            stage: None,
        },
    )?;

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M").to_string();
    db.set_schedule_last_enqueued(id, &now)?;

    // Run the newly enqueued task immediately.
    let dir = werma_dir()?;
    match runner::run_next(db, &dir)? {
        Some(task_id) => println!("trigger: launched {task_id}"),
        None => println!("trigger: enqueued (no launchable tasks)"),
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Version => {
            let pkg = env!("CARGO_PKG_VERSION");
            let git = option_env!("WERMA_GIT_VERSION").unwrap_or("dev");
            println!("werma {pkg} ({git})");
            let dir = werma_dir()?;
            println!("db: {}", dir.join("werma.db").display());
        }

        cli::Commands::Add {
            prompt,
            output,
            priority,
            task_type,
            model,
            tools,
            dir,
            turns,
            depends,
            context,
            linear,
            stage,
        } => {
            let db = open_db()?;
            cmd_add(
                &db,
                AddParams {
                    prompt,
                    output,
                    priority,
                    task_type,
                    model,
                    tools,
                    dir,
                    turns,
                    depends,
                    context,
                    linear,
                    stage,
                },
            )?;
        }

        cli::Commands::List { status } => {
            let db = open_db()?;
            cmd_list(&db, status.as_deref())?;
        }

        cli::Commands::Status => {
            let db = open_db()?;
            cmd_status(&db)?;
        }

        cli::Commands::View { id } => {
            let db = open_db()?;
            cmd_view(&db, &id)?;
        }

        cli::Commands::Retry { id } => {
            let db = open_db()?;
            cmd_retry(&db, &id)?;
        }

        cli::Commands::Kill { id } => {
            let db = open_db()?;
            cmd_kill(&db, &id)?;
        }

        cli::Commands::Clean => {
            let db = open_db()?;
            cmd_clean(&db)?;
        }

        cli::Commands::Log { id } => {
            cmd_log(id)?;
        }

        cli::Commands::Continue { id, prompt } => {
            let db = open_db()?;
            cmd_continue(&db, &id, prompt)?;
        }

        cli::Commands::Run => {
            let db = open_db()?;
            let dir = werma_dir()?;
            match runner::run_next(&db, &dir)? {
                Some(id) => println!("launched: {id}"),
                None => println!("no launchable tasks (pending with resolved deps)"),
            }
        }

        cli::Commands::RunAll => {
            let db = open_db()?;
            let dir = werma_dir()?;
            runner::run_all(&db, &dir)?;
        }

        cli::Commands::Sched { action } => {
            let db = open_db()?;
            match action {
                cli::SchedAction::Add {
                    id,
                    cron,
                    prompt,
                    task_type,
                    model,
                    output,
                    context,
                    dir,
                    turns,
                } => {
                    cmd_sched_add(
                        &db,
                        SchedAddParams {
                            id,
                            cron,
                            prompt,
                            task_type,
                            model,
                            output,
                            context,
                            dir,
                            turns,
                        },
                    )?;
                }
                cli::SchedAction::List => {
                    cmd_sched_list(&db)?;
                }
                cli::SchedAction::Rm { id } => {
                    db.delete_schedule(&id)?;
                    println!("removed: {id}");
                }
                cli::SchedAction::On { id } => {
                    db.set_schedule_enabled(&id, true)?;
                    println!("enabled: {id}");
                }
                cli::SchedAction::Off { id } => {
                    db.set_schedule_enabled(&id, false)?;
                    println!("disabled: {id}");
                }
                cli::SchedAction::Trigger { id } => {
                    cmd_sched_trigger(&db, &id)?;
                }
            }
        }

        cli::Commands::Daemon { action } => match action {
            Some(cli::DaemonAction::Install) => {
                daemon::install()?;
            }
            Some(cli::DaemonAction::Uninstall) => {
                daemon::uninstall()?;
            }
            None => {
                let dir = werma_dir()?;
                daemon::run(&dir)?;
            }
        },

        cli::Commands::Linear { action } => match action {
            cli::LinearAction::Setup => {
                let client = linear::LinearClient::new()?;
                client.setup()?;
            }
            cli::LinearAction::Sync => {
                let db = open_db()?;
                let client = linear::LinearClient::new()?;
                client.sync(&db)?;
            }
            cli::LinearAction::Push { id } => {
                let db = open_db()?;
                let client = linear::LinearClient::new()?;
                client.push(&db, &id)?;
            }
            cli::LinearAction::PushAll => {
                let db = open_db()?;
                let client = linear::LinearClient::new()?;
                client.push_all(&db)?;
            }
        },

        cli::Commands::Pipeline { action } => match action {
            cli::PipelineAction::Poll => {
                let db = open_db()?;
                pipeline::poll(&db)?;
            }
            cli::PipelineAction::Status => {
                let db = open_db()?;
                pipeline::status(&db)?;
            }
        },

        cli::Commands::Review => {
            println!("not implemented yet, use `gh` CLI directly");
        }
        cli::Commands::Dash => {
            let db = open_db()?;
            dashboard::show_dashboard(&db)?;
        }
        cli::Commands::Backup => {
            let dir = werma_dir()?;
            backup::backup_db(&dir)?;
        }
        cli::Commands::Migrate => {
            let db = open_db()?;
            migrate::migrate(&db)?;
        }
    }

    Ok(())
}
