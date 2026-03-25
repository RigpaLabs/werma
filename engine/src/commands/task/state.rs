use anyhow::{Context, Result};

use crate::db::Db;
use crate::models::{Status, Task};
use crate::{notify, pipeline};

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

    db.set_task_status(id, Status::Canceled)?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    db.update_task_field(id, "finished_at", &now)?;

    println!("status -> canceled: {id}");
    Ok(())
}

pub fn cmd_complete(
    db: &Db,
    id: &str,
    session: Option<&str>,
    result_file: Option<&str>,
    cost_usd: Option<f64>,
    turns_used: Option<i32>,
) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;

    // Idempotency: skip if already in terminal state
    if matches!(
        task.status,
        Status::Completed | Status::Failed | Status::Canceled
    ) {
        println!("{id} already in terminal state, skipping");
        return Ok(());
    }

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    db.set_task_status(id, Status::Completed)?;
    db.update_task_field(id, "finished_at", &now)?;
    if let Some(sid) = session {
        db.update_task_field(id, "session_id", sid)?;
    }
    if let Some(cost) = cost_usd {
        db.update_task_field(id, "cost_usd", &cost.to_string())?;
    }
    if let Some(turns) = turns_used {
        db.update_task_field(id, "turns_used", &turns.to_string())?;
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
        log_empty_output(id, &task, result_file);

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
        let linear_client = crate::linear::LinearClient::new()?;
        let cmd_runner = crate::traits::RealCommandRunner;
        let notifier = crate::traits::RealNotifier;
        match pipeline::callback(
            db,
            id,
            &task.pipeline_stage,
            &result_text,
            &task.linear_issue_id,
            &task.working_dir,
            &linear_client,
            &cmd_runner,
            &notifier,
        ) {
            Ok(()) => {
                db.set_linear_pushed(id, true)?;
            }
            Err(e) => {
                // Log to both stderr and daemon.log for visibility.
                // Daemon will retry via process_completed_pipeline_tasks.
                eprintln!("pipeline callback error for {id}: {e}");
                log_callback_error(id, &task, &e);
            }
        }
    }

    // Research completion: curator follow-up + Linear update
    if task.task_type == "research"
        && !task.linear_issue_id.is_empty()
        && let Err(e) = pipeline::handle_research_completion(
            db,
            &task,
            &result_text,
            &crate::linear::LinearClient::new()?,
        )
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
    if matches!(
        task.status,
        Status::Completed | Status::Failed | Status::Canceled
    ) {
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

pub fn cmd_peek(db: &Db, id: &str) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;

    println!("=== Peek: {id} ===");
    println!(
        "status: {:?}  type: {}  model: {}",
        task.status, task.task_type, task.model
    );
    if !task.pipeline_stage.is_empty() {
        println!(
            "stage: {}  issue: {}",
            task.pipeline_stage, task.linear_issue_id
        );
    }

    // Elapsed time
    if let Some(ref started_str) = task.started_at {
        if let Ok(started) = chrono::NaiveDateTime::parse_from_str(started_str, "%Y-%m-%dT%H:%M:%S")
        {
            let now = chrono::Local::now().naive_local();
            let elapsed = now - started;
            let mins = elapsed.num_minutes();
            let secs = elapsed.num_seconds() % 60;
            println!("elapsed: {mins}m {secs}s");
        }
    }

    // Cost and turns
    if let Some(cost) = task.cost_usd {
        println!("cost: ${cost:.4}");
    }
    if task.turns_used > 0 {
        println!("turns: {}", task.turns_used);
    }

    // Working directory info
    println!("dir: {}", task.working_dir);

    // Check worktree for recent activity
    let working_dir = std::path::Path::new(&task.working_dir);
    if working_dir.exists() {
        // git diff --stat for uncommitted changes
        let diff_output = std::process::Command::new("git")
            .args(["diff", "--stat", "HEAD"])
            .current_dir(working_dir)
            .output();
        if let Ok(out) = diff_output {
            let diff_text = String::from_utf8_lossy(&out.stdout);
            if !diff_text.trim().is_empty() {
                println!("\n--- uncommitted changes ---");
                print!("{diff_text}");
            } else {
                println!("\nno uncommitted changes");
            }
        }

        // Find most recently modified file
        let recent = std::process::Command::new("find")
            .args([
                ".", "-not", "-path", "./.git/*", "-type", "f", "-printf", "%T@ %p\n",
            ])
            .current_dir(working_dir)
            .output();

        // macOS find doesn't support -printf; try stat-based fallback
        let last_modified = if let Ok(ref out) = recent {
            if out.status.success() {
                parse_most_recent_from_printf(&String::from_utf8_lossy(&out.stdout))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((ts, path)) = last_modified {
            let now = chrono::Local::now().timestamp();
            let age_secs = now - ts;
            let age_min = age_secs / 60;
            if age_min < 1 {
                println!("last file change: {path} ({age_secs}s ago)");
            } else {
                println!("last file change: {path} ({age_min}m ago)");
            }
            if age_min > 5 && task.status == Status::Running {
                println!("WARNING: no file changes in >5min — agent may be idle");
            }
        }
    }

    // tmux session check
    let session_name = format!("werma-{id}");
    let tmux_check = std::process::Command::new("tmux")
        .args(["has-session", "-t", &session_name])
        .output();
    match tmux_check {
        Ok(out) if out.status.success() => println!("\ntmux: {session_name} (active)"),
        _ => println!("\ntmux: {session_name} (not found)"),
    }

    Ok(())
}

// --- Private helpers ---

fn parse_most_recent_from_printf(output: &str) -> Option<(i64, String)> {
    let mut best_ts: f64 = 0.0;
    let mut best_path = String::new();
    for line in output.lines() {
        if let Some((ts_str, path)) = line.split_once(' ') {
            if let Ok(ts) = ts_str.parse::<f64>() {
                if ts > best_ts {
                    best_ts = ts;
                    best_path = path.to_string();
                }
            }
        }
    }
    if best_ts > 0.0 {
        Some((best_ts as i64, best_path))
    } else {
        None
    }
}

fn log_empty_output(id: &str, task: &Task, result_file: Option<&str>) {
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
}

fn log_callback_error(id: &str, task: &Task, e: &anyhow::Error) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::models::Task;

    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
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

        cmd_complete(&db, "20260313-001", None, None, None, None).unwrap();
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
        cmd_clean(&db).unwrap();
    }

    #[test]
    fn cmd_kill_nonexistent_task() {
        let db = test_db();
        let result = cmd_kill(&db, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn cmd_kill_sets_canceled_status() {
        let db = test_db();
        let task = Task {
            id: "20260313-001".into(),
            status: Status::Running,
            task_type: "code".into(),
            prompt: "test".into(),
            working_dir: "/tmp".into(),
            model: "sonnet".into(),
            ..Default::default()
        };
        db.insert_task(&task).unwrap();

        cmd_kill(&db, "20260313-001").unwrap();

        let t = db.task("20260313-001").unwrap().unwrap();
        assert_eq!(
            t.status,
            Status::Canceled,
            "cmd_kill must write Canceled, not Failed"
        );
    }

    #[test]
    fn cmd_complete_skips_canceled_task() {
        let db = test_db();
        let task = Task {
            id: "20260313-002".into(),
            status: Status::Canceled,
            task_type: "code".into(),
            prompt: "test".into(),
            working_dir: "/tmp".into(),
            model: "sonnet".into(),
            ..Default::default()
        };
        db.insert_task(&task).unwrap();

        cmd_complete(&db, "20260313-002", None, None, None, None).unwrap();

        let t = db.task("20260313-002").unwrap().unwrap();
        assert_eq!(
            t.status,
            Status::Canceled,
            "cmd_complete must not overwrite a Canceled task"
        );
    }

    #[test]
    fn cmd_fail_skips_canceled_task() {
        let db = test_db();
        let task = Task {
            id: "20260313-003".into(),
            status: Status::Canceled,
            task_type: "code".into(),
            prompt: "test".into(),
            working_dir: "/tmp".into(),
            model: "sonnet".into(),
            ..Default::default()
        };
        db.insert_task(&task).unwrap();

        cmd_fail(&db, "20260313-003").unwrap();

        let t = db.task("20260313-003").unwrap().unwrap();
        assert_eq!(
            t.status,
            Status::Canceled,
            "cmd_fail must not overwrite a Canceled task"
        );
    }
}
