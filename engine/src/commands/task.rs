use std::path::Path;

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::db::Db;
use crate::models::{Status, Task};
use crate::{notify, pipeline, runner, ui, worktree};

use super::display::*;

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
}

pub fn cmd_add(db: &Db, p: AddParams) -> Result<()> {
    let id = db.next_task_id()?;
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
            .unwrap_or_else(|| worktree::extract_rig_id_prefix(&p.prompt).unwrap_or_default()),
        linear_pushed: false,
        pipeline_stage: p.stage.unwrap_or_default(),
        depends_on: depends_on.clone(),
        context_files: context_files.clone(),
        repo_hash: crate::runtime_repo_hash(),
        estimate: 0,
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
    let table = ui::task_list_table(&tasks, term_width);
    println!("{table}");
    println!();

    Ok(())
}

pub fn cmd_status(db: &Db, watch: bool, compact: bool, interval: u64) -> Result<()> {
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    let use_compact = compact || term_width < 60;
    let auto_compacted = !compact && term_width < 60;

    if watch {
        if auto_compacted {
            eprintln!("tip: terminal width < 60, using compact mode (or pass -c)");
        }

        // Flicker-free watch: hide cursor, use cursor repositioning
        ui::hide_cursor();
        // Initial clear
        print!("\x1b[2J\x1b[H");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut prev_lines = 0;

        // Ensure cursor is restored on exit (Ctrl+C, normal return, or panic)
        struct CursorGuard;
        impl Drop for CursorGuard {
            fn drop(&mut self) {
                ui::show_cursor();
            }
        }
        let _guard = CursorGuard;

        // SIGINT handler: restore cursor before exiting
        ctrlc::set_handler(move || {
            ui::show_cursor();
            std::process::exit(0);
        })
        .ok();

        loop {
            let running = db.list_tasks(Some(Status::Running))?;
            let pending = db.list_tasks(Some(Status::Pending))?;
            let completed = db.list_tasks(Some(Status::Completed))?;
            let failed = db.list_tasks(Some(Status::Failed))?;

            let content = if use_compact {
                ui::render_compact_buf(&running, &pending, &completed, &failed, Some(interval))
            } else {
                ui::render_status_buf(&running, &pending, &completed, &failed, Some(interval))
            };

            ui::refresh_screen(&content, prev_lines);
            prev_lines = content.lines().count();

            std::thread::sleep(std::time::Duration::from_secs(interval));
        }
    } else if use_compact {
        if auto_compacted {
            eprintln!("tip: terminal width < 60, using compact mode (or pass -c)");
        }
        render_compact(db, None)?;
    } else {
        render_status(db)?;
    }
    Ok(())
}

fn render_status(db: &Db) -> Result<()> {
    let running = db.list_tasks(Some(Status::Running))?;
    let pending = db.list_tasks(Some(Status::Pending))?;
    let completed = db.list_tasks(Some(Status::Completed))?;
    let failed = db.list_tasks(Some(Status::Failed))?;

    println!();

    // Running
    println!(
        " {} {}",
        "◉".green().bold(),
        format!("running ({})", running.len()).green().bold()
    );
    for task in &running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(format_elapsed_since)
            .unwrap_or_default();
        println!("{}", format_task_line(task, &elapsed));
    }

    // Pending
    println!(
        " {} {}",
        "○".yellow(),
        format!("pending ({})", pending.len()).yellow()
    );
    for task in pending.iter().take(5) {
        let prio = format!("p{}", task.priority);
        println!("{}", format_task_line(task, &prio));
    }
    if pending.len() > 5 {
        println!("   {}", format!("... +{} more", pending.len() - 5).dimmed());
    }

    // Completed + Failed
    println!(
        " {} {}     {} {}",
        "✓".dimmed(),
        format!("completed ({})", completed.len()).dimmed(),
        "✗".red(),
        format!("failed ({})", failed.len()).red(),
    );

    // Show newest first: reverse order (DB returns oldest first typically)
    let recent: Vec<&Task> = completed.iter().rev().take(10).collect();
    let failed_recent: Vec<&Task> = failed.iter().rev().take(5).collect();

    for task in &recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        println!("{}", format_task_line(task, &dur));
    }

    for task in &failed_recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        println!("{}", format_task_line(task, &dur));
    }

    println!();
    Ok(())
}

fn render_compact(db: &Db, interval: Option<u64>) -> Result<()> {
    let running = db.list_tasks(Some(Status::Running))?;
    let pending = db.list_tasks(Some(Status::Pending))?;
    let completed = db.list_tasks(Some(Status::Completed))?;
    let failed = db.list_tasks(Some(Status::Failed))?;

    let sep = "───────────────────────────────────";

    println!(
        " werma {} {} running  {} {} pending",
        "●".green().bold(),
        running.len().to_string().green().bold(),
        "○".yellow(),
        pending.len().to_string().yellow(),
    );
    println!(" {sep}");

    for task in &running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(format_elapsed_since)
            .unwrap_or_default();
        let linear = compact_linear_label(&task.linear_issue_id);
        println!(
            " {} {} {}{} {}",
            "●".green().bold(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type).blue(),
            linear.cyan(),
            elapsed.dimmed(),
        );
    }

    for task in pending.iter().take(3) {
        let linear = compact_linear_label(&task.linear_issue_id);
        println!(
            " {} {} {}{}",
            "○".yellow(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type).blue(),
            linear.cyan(),
        );
    }
    if pending.len() > 3 {
        println!(" {}", format!("  +{} more", pending.len() - 3).dimmed());
    }

    // Only show separator if there were running or pending tasks above
    if !running.is_empty() || !pending.is_empty() {
        println!(" {sep}");
    }

    // Recent completed (last 5, newest first)
    let recent: Vec<&Task> = completed.iter().rev().take(5).collect();
    for task in &recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = compact_linear_label(&task.linear_issue_id);
        println!(
            " {} {} {}{} {}",
            "✓".dimmed(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear.dimmed(),
            dur.dimmed(),
        );
    }

    // Recent failed (last 5, newest first)
    let failed_recent: Vec<&Task> = failed.iter().rev().take(5).collect();
    for task in &failed_recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = compact_linear_label(&task.linear_issue_id);
        println!(
            " {} {} {}{} {}",
            "✗".red(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear.dimmed(),
            dur.dimmed(),
        );
    }

    println!(" {sep}");

    let refresh_str = if let Some(secs) = interval {
        format!("  ↻ {secs}s")
    } else {
        String::new()
    };
    println!(
        " {} done  {} fail{}",
        completed.len().to_string().dimmed(),
        failed.len().to_string().red(),
        refresh_str.dimmed(),
    );

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

pub fn cmd_retry(db: &Db, id: &str) -> Result<()> {
    let _task = db.task(id)?.context(format!("task not found: {id}"))?;

    db.set_task_status(id, Status::Pending)?;
    db.update_task_field(id, "started_at", "")?;
    db.update_task_field(id, "finished_at", "")?;

    println!("retry: {id} -> pending");
    Ok(())
}

pub fn cmd_kill(db: &Db, id: &str) -> Result<()> {
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

pub fn cmd_complete(
    db: &Db,
    id: &str,
    session: Option<&str>,
    result_file: Option<&str>,
) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;

    // Idempotency: skip if already in terminal state
    if task.status == Status::Completed || task.status == Status::Failed {
        println!("{id} already in terminal state, skipping");
        return Ok(());
    }

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    db.set_task_status(id, Status::Completed)?;
    db.update_task_field(id, "finished_at", &now)?;
    if let Some(sid) = session {
        db.update_task_field(id, "session_id", sid)?;
    }

    db.increment_usage(&task.model)?;

    // Read result text for pipeline callback
    let result_text = match result_file {
        Some(path) => std::fs::read_to_string(path)
            .inspect_err(|e| eprintln!("warn: failed to read result_file {path}: {e}"))
            .unwrap_or_default(),
        None => String::new(),
    };

    // Validate non-empty output: if empty, mark as failed instead of completed
    if result_text.trim().is_empty() {
        eprintln!("warning: empty output for task {id} — marking as failed");
        db.set_task_status(id, Status::Failed)?;
        // Log to daemon.log for visibility
        let werma_dir = dirs::home_dir()
            .map(|h| h.join(".werma"))
            .unwrap_or_default();
        let log_path = werma_dir.join("logs/daemon.log");
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
        let line = format!(
            "{ts}: EMPTY OUTPUT — task {id} stage={} marked failed (result_file: {})\n",
            task.pipeline_stage,
            result_file.unwrap_or("none"),
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));

        let label = notify::format_notify_label(id, &task.task_type, &task.linear_issue_id);
        notify::notify_macos(
            "werma",
            &format!("{label} EMPTY OUTPUT — marked failed"),
            "Basso",
        );
        notify::notify_slack(
            "#werma",
            &format!(":warning: {label} EMPTY OUTPUT — marked failed"),
        );

        println!("failed (empty output): {id}");
        return Ok(());
    }

    // Pipeline callback: trigger stage transitions.
    // On success, mark linear_pushed=true so daemon doesn't re-process.
    if !task.pipeline_stage.is_empty() && !task.linear_issue_id.is_empty() {
        match pipeline::callback(
            db,
            id,
            &task.pipeline_stage,
            &result_text,
            &task.linear_issue_id,
            &task.working_dir,
        ) {
            Ok(()) => {
                db.set_linear_pushed(id, true)?;
            }
            Err(e) => {
                // Log to both stderr and daemon.log for visibility.
                // Daemon will retry via process_completed_pipeline_tasks.
                eprintln!("pipeline callback error for {id}: {e}");
                let werma_dir = dirs::home_dir()
                    .map(|h| h.join(".werma"))
                    .unwrap_or_default();
                let log_path = werma_dir.join("logs/daemon.log");
                let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
                let line = format!(
                    "{ts}: cmd_complete callback failed: {id} stage={} error={e}\n",
                    task.pipeline_stage
                );
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
            }
        }
    }

    // Research completion: curator follow-up + Linear update
    if task.task_type == "research"
        && !task.linear_issue_id.is_empty()
        && let Err(e) = pipeline::handle_research_completion(db, &task, &result_text)
    {
        eprintln!("research completion error for {id}: {e}");
    }

    // Notifications
    let label = notify::format_notify_label(id, &task.task_type, &task.linear_issue_id);
    notify::notify_macos("werma", &format!("{label} done"), "Glass");
    notify::notify_slack("#werma", &format!(":white_check_mark: {label} done"));

    println!("completed: {id}");
    Ok(())
}

pub fn cmd_fail(db: &Db, id: &str) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;

    // Idempotency: skip if already in terminal state
    if task.status == Status::Completed || task.status == Status::Failed {
        println!("{id} already in terminal state, skipping");
        return Ok(());
    }

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    db.set_task_status(id, Status::Failed)?;
    db.update_task_field(id, "finished_at", &now)?;

    // Post failure comment to Linear for pipeline tasks
    if !task.pipeline_stage.is_empty()
        && !task.linear_issue_id.is_empty()
        && let Ok(linear) = crate::linear::LinearClient::new()
    {
        let _ = linear.comment(
            &task.linear_issue_id,
            &format!(
                "**Task `{id}` FAILED** (stage: {}). Manual intervention needed.",
                task.pipeline_stage,
            ),
        );
    }

    // Notifications
    let label = notify::format_notify_label(id, &task.task_type, &task.linear_issue_id);
    notify::notify_macos("werma", &format!("{label} FAILED"), "Basso");
    notify::notify_slack("#werma", &format!(":x: {label} FAILED"));

    println!("failed: {id}");
    Ok(())
}

pub fn cmd_clean(db: &Db) -> Result<()> {
    let tasks = db.clean_completed()?;

    if tasks.is_empty() {
        println!("nothing to clean");
        return Ok(());
    }

    let dir = crate::werma_dir()?.join("completed");
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

pub fn cmd_continue(db: &Db, id: &str, prompt: Option<String>) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;

    if task.session_id.is_empty() {
        bail!("no session_id for task {id}");
    }

    let follow_up = prompt.unwrap_or_else(|| "Continue the task.".to_string());
    let model_id = runner::model_flag(&task.model);
    let session_name = format!("werma-{id}-cont");
    let wdir = crate::werma_dir()?;
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

    // Resolve worktree path for write tasks (same logic as runner::run_task)
    let effective_dir = if worktree::needs_worktree(&task.task_type) {
        let branch = worktree::generate_branch_name(&task);
        let dir_name = branch.replace('/', "--");
        let wt_path = std::path::PathBuf::from(&working_dir)
            .join(".trees")
            .join(&dir_name);
        if wt_path.exists() {
            wt_path.to_string_lossy().to_string()
        } else {
            working_dir.clone()
        }
    } else {
        working_dir.clone()
    };

    // Build human-readable label for notification
    let notify_label = notify::format_notify_label(id, &task.task_type, &task.linear_issue_id);

    // Generate safe exec script
    let script = format!(
        r##"#!/bin/bash
set -euo pipefail
unset CLAUDECODE
cd '{effective_dir}'
PROMPT=$(cat '{prompt_file}')
claude -p "$PROMPT" \
    --resume '{session_id}' \
    --allowedTools '{tools}' \
    --model {model_id} \
    2>> '{log_file}'
osascript -e 'display notification "{notify_label} ↻" with title "werma" sound name "Glass"' 2>/dev/null || true
"##,
        effective_dir = effective_dir,
        prompt_file = prompt_file.display(),
        session_id = task.session_id.replace('\'', "'\\''"),
        tools = tools.replace('\'', "'\\''"),
        model_id = model_id,
        log_file = log_file.display(),
        notify_label = notify_label.replace('"', "\\\""),
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

pub fn cmd_run(db: &Db) -> Result<()> {
    let dir = crate::werma_dir()?;
    match runner::run_next(db, &dir)? {
        Some(id) => println!("launched: {id}"),
        None => println!("no launchable tasks (pending with resolved deps)"),
    }
    Ok(())
}

pub fn cmd_run_all(db: &Db) -> Result<()> {
    let dir = crate::werma_dir()?;
    runner::run_all(db, &dir)?;
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
        // Should not error on empty db
        cmd_list(&db, None).unwrap();
    }

    #[test]
    fn cmd_list_with_invalid_status() {
        let db = test_db();
        let result = cmd_list(&db, Some("bogus"));
        assert!(result.is_err());
    }

    #[test]
    fn cmd_retry_resets_to_pending() {
        let db = test_db();
        let task = Task {
            id: "20260313-001".into(),
            status: Status::Failed,
            task_type: "code".into(),
            prompt: "test".into(),
            working_dir: "/tmp".into(),
            model: "sonnet".into(),
            ..Default::default()
        };
        db.insert_task(&task).unwrap();

        cmd_retry(&db, "20260313-001").unwrap();

        let t = db.task("20260313-001").unwrap().unwrap();
        assert_eq!(t.status, Status::Pending);
    }

    #[test]
    fn cmd_retry_nonexistent_task() {
        let db = test_db();
        let result = cmd_retry(&db, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn cmd_complete_idempotent() {
        let db = test_db();
        let task = Task {
            id: "20260313-001".into(),
            status: Status::Completed,
            task_type: "code".into(),
            prompt: "test".into(),
            working_dir: "/tmp".into(),
            model: "sonnet".into(),
            ..Default::default()
        };
        db.insert_task(&task).unwrap();

        // Completing an already-completed task should be a no-op
        cmd_complete(&db, "20260313-001", None, None).unwrap();
        let t = db.task("20260313-001").unwrap().unwrap();
        assert_eq!(t.status, Status::Completed);
    }

    #[test]
    fn cmd_fail_idempotent() {
        let db = test_db();
        let task = Task {
            id: "20260313-001".into(),
            status: Status::Failed,
            task_type: "code".into(),
            prompt: "test".into(),
            working_dir: "/tmp".into(),
            model: "sonnet".into(),
            ..Default::default()
        };
        db.insert_task(&task).unwrap();

        cmd_fail(&db, "20260313-001").unwrap();
        let t = db.task("20260313-001").unwrap().unwrap();
        assert_eq!(t.status, Status::Failed);
    }

    #[test]
    fn cmd_fail_nonexistent_task() {
        let db = test_db();
        let result = cmd_fail(&db, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn cmd_clean_empty_db() {
        let db = test_db();
        // Should handle empty db gracefully
        cmd_clean(&db).unwrap();
    }

    #[test]
    fn cmd_view_nonexistent_task() {
        let db = test_db();
        let result = cmd_view(&db, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn cmd_kill_nonexistent_task() {
        let db = test_db();
        let result = cmd_kill(&db, "nonexistent");
        assert!(result.is_err());
    }
}
