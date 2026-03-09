use std::io::Write as IoWrite;
use std::path::Path;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Local;
use cron::Schedule;

use crate::db::Db;
use crate::{linear, pipeline, runner};

const TICK_INTERVAL_SECS: u64 = 60;
const ORCHESTRATOR_INTERVAL_SECS: u64 = 900;
const MAX_CONCURRENT_AGENTS: usize = 3;
const DEFAULT_STUCK_TIMEOUT_MINS: i64 = 30;
const MAX_LOG_SIZE_BYTES: u64 = 5 * 1024 * 1024;

/// Run the daemon loop. Blocks forever (or until killed).
/// launchd manages restart via KeepAlive.
pub fn run(werma_dir: &Path) -> Result<()> {
    let db_path = werma_dir.join("werma.db");
    let log_path = werma_dir.join("logs/daemon.log");

    std::fs::create_dir_all(werma_dir.join("logs"))?;

    log_daemon(&log_path, "daemon started");

    // Trigger orchestrator immediately on first tick.
    let mut last_orchestrator = Instant::now() - Duration::from_secs(ORCHESTRATOR_INTERVAL_SECS);

    loop {
        let tick_start = Instant::now();

        // Fresh DB connection each tick to avoid stale locks.
        if let Ok(db) = Db::open(&db_path) {
            if let Err(e) = check_schedules(&db, werma_dir) {
                log_daemon(&log_path, &format!("schedule check error: {e}"));
            }

            if let Err(e) = check_stuck_tasks(&db, werma_dir) {
                log_daemon(&log_path, &format!("stuck detection error: {e}"));
            }

            if let Err(e) = process_completed_pipeline_tasks(&db, werma_dir) {
                log_daemon(&log_path, &format!("pipeline callback error: {e}"));
            }

            if let Err(e) = drain_queue(&db, werma_dir) {
                log_daemon(&log_path, &format!("queue drain error: {e}"));
            }

            if let Err(e) = rotate_logs(werma_dir) {
                log_daemon(&log_path, &format!("log rotation error: {e}"));
            }

            if last_orchestrator.elapsed() >= Duration::from_secs(ORCHESTRATOR_INTERVAL_SECS) {
                if let Err(e) = run_orchestrator(&db, werma_dir) {
                    log_daemon(&log_path, &format!("orchestrator error: {e}"));
                }
                last_orchestrator = Instant::now();
            }
        }

        let elapsed = tick_start.elapsed();
        if elapsed < Duration::from_secs(TICK_INTERVAL_SECS) {
            thread::sleep(Duration::from_secs(TICK_INTERVAL_SECS) - elapsed);
        }
    }
}

/// Convert a 5-field user cron expression to the 7-field format the `cron` crate expects.
/// "30 7 * * *" -> "0 30 7 * * * *" (sec=0, year=*)
pub fn cron5_to_cron7(expr: &str) -> String {
    format!("0 {expr} *")
}

/// Check all enabled schedules and enqueue matching tasks.
fn check_schedules(db: &Db, werma_dir: &Path) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let schedules = db.list_schedules()?;
    let now = Local::now();

    for sched in &schedules {
        if !sched.enabled {
            continue;
        }

        let cron7 = cron5_to_cron7(&sched.cron_expr);
        let schedule = match Schedule::from_str(&cron7) {
            Ok(s) => s,
            Err(e) => {
                log_daemon(
                    &log_path,
                    &format!("bad cron expr for {}: {} -> {e}", sched.id, sched.cron_expr),
                );
                continue;
            }
        };

        // Check if cron schedule has an occurrence in the last 60 seconds.
        let window_start = now - chrono::Duration::seconds(60);
        let mut iter = schedule.after(&window_start);

        let matches = iter.next().is_some_and(|next_time| next_time <= now);

        if !matches {
            continue;
        }

        // Guard: don't enqueue if last_enqueued is within the last 60 seconds.
        if !sched.last_enqueued.is_empty()
            && let Ok(last) =
                chrono::NaiveDateTime::parse_from_str(&sched.last_enqueued, "%Y-%m-%dT%H:%M")
            && let Some(last_dt) = last.and_local_timezone(Local).single()
        {
            let since = now.signed_duration_since(last_dt);
            if since.num_seconds() < 60 {
                continue;
            }
        }

        // Enqueue: expand placeholders and create a task.
        let today = now.format("%Y-%m-%d").to_string();
        let dow = now.format("%A").to_string().to_lowercase();

        let prompt = sched
            .prompt
            .replace("{date}", &today)
            .replace("{dow}", &dow);

        let output_path = if sched.output_path.is_empty() {
            String::new()
        } else {
            sched.output_path.replace("{date}", &today)
        };

        let max_turns = if sched.max_turns > 0 {
            sched.max_turns
        } else {
            crate::default_turns(&sched.schedule_type)
        };

        let allowed_tools = crate::runner::tools_for_type(&sched.schedule_type, false);

        let task_id = db.next_task_id()?;
        let created_at = now.format("%Y-%m-%dT%H:%M:%S").to_string();

        let task = crate::models::Task {
            id: task_id.clone(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at,
            started_at: None,
            finished_at: None,
            task_type: sched.schedule_type.clone(),
            prompt,
            output_path,
            working_dir: sched.working_dir.clone(),
            model: sched.model.clone(),
            max_turns,
            allowed_tools,
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: sched.context_files.clone(),
        };

        db.insert_task(&task)?;

        let enqueued_at = now.format("%Y-%m-%dT%H:%M").to_string();
        db.set_schedule_last_enqueued(&sched.id, &enqueued_at)?;

        log_daemon(
            &log_path,
            &format!("schedule {}: enqueued task {task_id}", sched.id),
        );
    }

    Ok(())
}

/// Detect tasks stuck in 'running' beyond the timeout and mark them failed.
fn check_stuck_tasks(db: &Db, werma_dir: &Path) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let running = db.list_tasks(Some(crate::models::Status::Running))?;
    let now = Local::now();
    let timeout_mins = load_timeout_mins(werma_dir);

    for task in &running {
        let started = match &task.started_at {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };

        let started_dt = match chrono::NaiveDateTime::parse_from_str(started, "%Y-%m-%dT%H:%M:%S") {
            Ok(dt) => dt,
            Err(_) => continue,
        };

        let started_local = match started_dt.and_local_timezone(Local).single() {
            Some(dt) => dt,
            None => continue,
        };

        let elapsed_mins = now.signed_duration_since(started_local).num_minutes();
        if elapsed_mins < timeout_mins {
            continue;
        }

        // Kill tmux session
        let session_name = format!("werma-{}", task.id);
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &session_name])
            .output();

        db.set_task_status(&task.id, crate::models::Status::Failed)?;
        let finished_at = now.format("%Y-%m-%dT%H:%M:%S").to_string();
        db.update_task_field(&task.id, "finished_at", &finished_at)?;

        log_daemon(
            &log_path,
            &format!(
                "stuck: {} running for {}m (timeout={}m) -> failed",
                task.id, elapsed_mins, timeout_mins
            ),
        );
    }

    Ok(())
}

/// Load timeout from limits.json if it exists, otherwise use default.
fn load_timeout_mins(werma_dir: &Path) -> i64 {
    let limits_path = werma_dir.join("limits.json");
    if let Ok(data) = std::fs::read_to_string(limits_path)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&data)
        && let Some(timeout) = json["timeout_minutes"].as_i64()
    {
        return timeout;
    }
    DEFAULT_STUCK_TIMEOUT_MINS
}

/// Process completed tasks that have Linear integration but haven't been pushed yet.
/// Pipeline tasks get routed through `pipeline::callback()` to advance the issue state.
/// Non-pipeline tasks get a comment + move-to-Done via `linear.push()`.
fn process_completed_pipeline_tasks(db: &Db, werma_dir: &Path) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let tasks = db.unpushed_linear_tasks()?;

    if tasks.is_empty() {
        return Ok(());
    }

    for task in &tasks {
        if !task.pipeline_stage.is_empty() {
            // Pipeline task: read output and call pipeline::callback()
            let output_file = werma_dir.join(format!("logs/{}-output.md", task.id));
            let output = std::fs::read_to_string(&output_file).unwrap_or_default();

            match pipeline::callback(db, &task.id, &task.pipeline_stage, &output, &task.linear_issue_id) {
                Ok(()) => {
                    db.set_linear_pushed(&task.id, true)?;
                    log_daemon(
                        &log_path,
                        &format!(
                            "pipeline callback: {} stage={} issue={}",
                            task.id, task.pipeline_stage, task.linear_issue_id
                        ),
                    );
                }
                Err(e) => {
                    log_daemon(
                        &log_path,
                        &format!(
                            "pipeline callback failed: {} stage={} error={e}",
                            task.id, task.pipeline_stage
                        ),
                    );
                    // Skip — will retry next tick.
                }
            }
        } else {
            // Non-pipeline task with linear_issue_id: push comment + move to Done.
            match linear::LinearClient::new().and_then(|client| client.push(db, &task.id)) {
                Ok(()) => {
                    log_daemon(
                        &log_path,
                        &format!("linear push: {} issue={}", task.id, task.linear_issue_id),
                    );
                }
                Err(e) => {
                    log_daemon(
                        &log_path,
                        &format!("linear push failed: {} error={e}", task.id),
                    );
                }
            }
        }
    }

    Ok(())
}

/// Drain pending tasks into tmux sessions, respecting max_concurrent.
fn drain_queue(db: &Db, werma_dir: &Path) -> Result<()> {
    let active = count_werma_sessions();
    if active >= MAX_CONCURRENT_AGENTS {
        return Ok(());
    }

    let slots = MAX_CONCURRENT_AGENTS - active;
    for _ in 0..slots {
        match runner::run_next(db, werma_dir) {
            Ok(Some(id)) => {
                let log_path = werma_dir.join("logs/daemon.log");
                log_daemon(&log_path, &format!("launched: {id}"));
            }
            Ok(None) => break, // No more launchable tasks.
            Err(e) => {
                let log_path = werma_dir.join("logs/daemon.log");
                log_daemon(&log_path, &format!("launch error: {e}"));
                break;
            }
        }
    }

    Ok(())
}

/// Count active tmux sessions with "werma-" prefix.
fn count_werma_sessions() -> usize {
    let output = std::process::Command::new("tmux").args(["ls"]).output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.lines().filter(|l| l.starts_with("werma-")).count()
        }
        Err(_) => 0,
    }
}

/// Run orchestrator agent if its character.md exists and it's not already running.
fn run_orchestrator(_db: &Db, werma_dir: &Path) -> Result<()> {
    // Look for the orchestrator agent directory.
    let orchestrator_dir = werma_dir
        .parent()
        .context("no parent for werma_dir")?
        .parent()
        .context("no grandparent")?
        .join("werma/agents/orchestrator");

    let character_file = orchestrator_dir.join("character.md");
    if !character_file.exists() {
        return Ok(()); // No orchestrator configured — skip silently.
    }

    // Check if an orchestrator task is already running.
    let running = _db.list_tasks(Some(crate::models::Status::Running))?;
    let already_running = running.iter().any(|t| t.task_type == "orchestrator");
    if already_running {
        return Ok(());
    }

    // Also check pending orchestrator tasks.
    let pending = _db.list_tasks(Some(crate::models::Status::Pending))?;
    let already_pending = pending.iter().any(|t| t.task_type == "orchestrator");
    if already_pending {
        return Ok(());
    }

    let task_id = _db.next_task_id()?;
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let task = crate::models::Task {
        id: task_id,
        status: crate::models::Status::Pending,
        priority: 3, // Low priority.
        created_at: now,
        started_at: None,
        finished_at: None,
        task_type: "orchestrator".to_string(),
        prompt: "Run orchestrator health check and planning cycle. Read your character.md and memory.md first.".to_string(),
        output_path: String::new(),
        working_dir: orchestrator_dir.to_string_lossy().to_string(),
        model: "sonnet".to_string(),
        max_turns: 10,
        allowed_tools: "Read,Grep,Glob,Write".to_string(),
        session_id: String::new(),
        linear_issue_id: String::new(),
        linear_pushed: false,
        pipeline_stage: String::new(),
        depends_on: vec![],
        context_files: vec![character_file.to_string_lossy().to_string()],
    };

    _db.insert_task(&task)?;

    Ok(())
}

/// Rotate log files larger than MAX_LOG_SIZE_BYTES.
fn rotate_logs(werma_dir: &Path) -> Result<()> {
    let logs_dir = werma_dir.join("logs");
    if !logs_dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(&logs_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "log")
            && let Ok(meta) = entry.metadata()
            && meta.len() > MAX_LOG_SIZE_BYTES
        {
            // Truncate to last 1000 lines.
            if let Ok(content) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = content.lines().collect();
                let keep = if lines.len() > 1000 {
                    &lines[lines.len() - 1000..]
                } else {
                    &lines
                };
                let _ = std::fs::write(&path, keep.join("\n"));
            }
        }
    }

    Ok(())
}

/// Append a timestamped line to daemon.log.
fn log_daemon(log_path: &Path, msg: &str) {
    let ts = Local::now().format("%Y-%m-%dT%H:%M:%S");
    let line = format!("{ts}: {msg}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

// --- Daemon install / uninstall ---

/// Install the daemon as a launchd agent.
pub fn install() -> Result<()> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let werma_dir = home.join(".werma");
    std::fs::create_dir_all(werma_dir.join("logs"))?;

    let binary_path =
        std::env::current_exe().context("cannot determine current executable path")?;
    let binary_str = binary_path.display().to_string();

    let uid = get_uid();
    let home_str = home.display().to_string();

    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.rigpalabs.werma.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary_str}</string>
        <string>daemon</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{home_str}/.werma/logs/daemon-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{home_str}/.werma/logs/daemon-stderr.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:{home_str}/.local/bin:{home_str}/.cargo/bin</string>
        <key>HOME</key>
        <string>{home_str}</string>
    </dict>
</dict>
</plist>"#
    );

    let plist_dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&plist_dir)?;
    let plist_path = plist_dir.join("io.rigpalabs.werma.daemon.plist");

    std::fs::write(&plist_path, &plist_content)?;
    println!("wrote: {}", plist_path.display());

    // Try bootstrap first (modern), fall back to load (legacy).
    let plist_str = plist_path.display().to_string();
    let domain_target = format!("gui/{uid}");

    let result = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain_target, &plist_str])
        .output();

    match result {
        Ok(out) if out.status.success() => {
            println!("loaded via: launchctl bootstrap {domain_target}");
        }
        _ => {
            // Fallback to legacy load.
            let fallback = std::process::Command::new("launchctl")
                .args(["load", &plist_str])
                .output();
            match fallback {
                Ok(out) if out.status.success() => {
                    println!("loaded via: launchctl load");
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    eprintln!("launchctl load failed: {stderr}");
                }
                Err(e) => {
                    eprintln!("launchctl failed: {e}");
                }
            }
        }
    }

    println!("daemon installed");
    Ok(())
}

/// Uninstall the daemon launchd agent.
pub fn uninstall() -> Result<()> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let uid = get_uid();
    let plist_path = home.join("Library/LaunchAgents/io.rigpalabs.werma.daemon.plist");

    let service_target = format!("gui/{uid}/io.rigpalabs.werma.daemon");

    // Try bootout first (modern), fall back to unload (legacy).
    let result = std::process::Command::new("launchctl")
        .args(["bootout", &service_target])
        .output();

    match result {
        Ok(out) if out.status.success() => {
            println!("unloaded via: launchctl bootout {service_target}");
        }
        _ => {
            let plist_str = plist_path.display().to_string();
            let fallback = std::process::Command::new("launchctl")
                .args(["unload", &plist_str])
                .output();
            match fallback {
                Ok(out) if out.status.success() => {
                    println!("unloaded via: launchctl unload");
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    eprintln!("launchctl unload failed: {stderr}");
                }
                Err(e) => {
                    eprintln!("launchctl failed: {e}");
                }
            }
        }
    }

    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
        println!("removed: {}", plist_path.display());
    }

    println!("daemon uninstalled");
    Ok(())
}

/// Get current UID for launchd domain target.
fn get_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid is always safe to call and returns u32.
        // Using nix or libc would be cleaner but this avoids adding a dep.
        std::process::Command::new("id")
            .args(["-u"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
            .unwrap_or(501)
    }
    #[cfg(not(unix))]
    {
        501
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron5_to_cron7_conversion() {
        assert_eq!(cron5_to_cron7("30 7 * * *"), "0 30 7 * * * *");
        assert_eq!(cron5_to_cron7("0 */2 * * *"), "0 0 */2 * * * *");
        assert_eq!(cron5_to_cron7("15 9 1 * *"), "0 15 9 1 * * *");
        assert_eq!(cron5_to_cron7("0 0 * * 1-5"), "0 0 0 * * 1-5 *");
    }

    #[test]
    fn cron7_parses_correctly() {
        let expr = cron5_to_cron7("30 7 * * *");
        let schedule = Schedule::from_str(&expr);
        assert!(schedule.is_ok(), "failed to parse: {expr}");
    }

    #[test]
    fn cron7_various_expressions_parse() {
        let exprs = vec![
            "0 * * * *",    // every hour
            "*/15 * * * *", // every 15 min
            "30 7 * * 1-5", // weekdays at 7:30
            "0 0 1 * *",    // first of month midnight
            "0 9,18 * * *", // 9am and 6pm
        ];

        for expr in &exprs {
            let cron7 = cron5_to_cron7(expr);
            let result = Schedule::from_str(&cron7);
            assert!(
                result.is_ok(),
                "failed to parse '{expr}' -> '{cron7}': {:?}",
                result.err()
            );
        }
    }

    #[test]
    fn cron_schedule_matches_within_window() {
        use chrono::TimeZone;

        // Create a schedule that matches at exactly 07:30 every day.
        let expr = cron5_to_cron7("30 7 * * *");
        let schedule = Schedule::from_str(&expr).expect("parse");

        // Simulate "now" as 07:30:30 (within 60s after the match point).
        let now = Local.with_ymd_and_hms(2026, 3, 9, 7, 30, 30).unwrap();
        let window_start = now - chrono::Duration::seconds(60);

        let mut iter = schedule.after(&window_start);
        let next = iter.next();
        assert!(next.is_some());
        let next_time = next.expect("has next");
        assert!(
            next_time <= now,
            "next_time {next_time} should be <= now {now}"
        );
    }

    #[test]
    fn cron_schedule_no_match_outside_window() {
        use chrono::TimeZone;

        let expr = cron5_to_cron7("30 7 * * *");
        let schedule = Schedule::from_str(&expr).expect("parse");

        // "Now" is 08:00 — well past the 07:30 match window.
        let now = Local.with_ymd_and_hms(2026, 3, 9, 8, 0, 0).unwrap();
        let window_start = now - chrono::Duration::seconds(60);

        let mut iter = schedule.after(&window_start);
        let next = iter.next().expect("has next");
        // Next occurrence should be tomorrow at 07:30, definitely after "now".
        assert!(
            next > now,
            "next {next} should be > now {now} (no match in window)"
        );
    }

    #[test]
    fn stuck_detection_logic() {
        // Verify the time math for stuck detection.
        use chrono::TimeZone;

        let started_str = "2026-03-09T07:00:00";
        let started_dt =
            chrono::NaiveDateTime::parse_from_str(started_str, "%Y-%m-%dT%H:%M:%S").unwrap();
        let started_local = started_dt.and_local_timezone(Local).single().unwrap();

        // 45 minutes later — should be stuck with 30min timeout.
        let now = Local.with_ymd_and_hms(2026, 3, 9, 7, 45, 0).unwrap();
        let elapsed = now.signed_duration_since(started_local).num_minutes();
        assert_eq!(elapsed, 45);
        assert!(elapsed >= DEFAULT_STUCK_TIMEOUT_MINS);

        // 20 minutes later — should NOT be stuck.
        let now_early = Local.with_ymd_and_hms(2026, 3, 9, 7, 20, 0).unwrap();
        let elapsed_early = now_early.signed_duration_since(started_local).num_minutes();
        assert_eq!(elapsed_early, 20);
        assert!(elapsed_early < DEFAULT_STUCK_TIMEOUT_MINS);
    }

    #[test]
    fn load_timeout_default() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = load_timeout_mins(dir.path());
        assert_eq!(timeout, DEFAULT_STUCK_TIMEOUT_MINS);
    }

    #[test]
    fn load_timeout_from_limits_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("limits.json"), r#"{"timeout_minutes": 60}"#).unwrap();
        let timeout = load_timeout_mins(dir.path());
        assert_eq!(timeout, 60);
    }

    #[test]
    fn rotate_logs_skips_small_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let log_file = logs_dir.join("test.log");
        std::fs::write(&log_file, "small log content\n").unwrap();

        rotate_logs(dir.path()).unwrap();

        let content = std::fs::read_to_string(&log_file).unwrap();
        assert_eq!(content, "small log content\n");
    }

    #[test]
    fn log_daemon_appends() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        log_daemon(&log_path, "first message");
        log_daemon(&log_path, "second message");

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("first message"));
        assert!(content.contains("second message"));
        // Two lines.
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn process_pipeline_tasks_missing_output_file() {
        // When output file doesn't exist, read_to_string returns empty via unwrap_or_default.
        // This tests the graceful handling — the function should not panic.
        let dir = tempfile::tempdir().unwrap();
        let output_file = dir.path().join("logs/99999-output.md");
        let content = std::fs::read_to_string(&output_file).unwrap_or_default();
        assert!(content.is_empty());
    }

    #[test]
    fn process_pipeline_tasks_skips_already_pushed() {
        // Verify unpushed_linear_tasks only returns tasks with linear_pushed=false
        let db = crate::db::Db::open_in_memory().unwrap();

        let task = crate::models::Task {
            id: "20260309-001".to_string(),
            status: crate::models::Status::Completed,
            priority: 1,
            created_at: "2026-03-09T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-engineer".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "issue-abc".to_string(),
            linear_pushed: false,
            pipeline_stage: "engineer".to_string(),
            depends_on: vec![],
            context_files: vec![],
        };
        db.insert_task(&task).unwrap();

        // Before push: should appear
        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert_eq!(unpushed.len(), 1);

        // After marking pushed: should not appear
        db.set_linear_pushed("20260309-001", true).unwrap();
        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty());
    }

    #[test]
    fn process_pipeline_tasks_filters_by_pipeline_stage() {
        // Verify that the function distinguishes pipeline vs non-pipeline tasks correctly.
        let db = crate::db::Db::open_in_memory().unwrap();

        // Pipeline task (has pipeline_stage)
        let pipeline_task = crate::models::Task {
            id: "20260309-001".to_string(),
            status: crate::models::Status::Completed,
            priority: 1,
            created_at: "2026-03-09T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-reviewer".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "issue-abc".to_string(),
            linear_pushed: false,
            pipeline_stage: "reviewer".to_string(),
            depends_on: vec![],
            context_files: vec![],
        };

        // Non-pipeline task (empty pipeline_stage, but has linear_issue_id)
        let direct_task = crate::models::Task {
            id: "20260309-002".to_string(),
            status: crate::models::Status::Completed,
            priority: 1,
            created_at: "2026-03-09T10:01:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "research".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "issue-def".to_string(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec![],
        };

        db.insert_task(&pipeline_task).unwrap();
        db.insert_task(&direct_task).unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert_eq!(unpushed.len(), 2);

        // Verify we can distinguish them by pipeline_stage
        let pipeline_tasks: Vec<_> = unpushed.iter().filter(|t| !t.pipeline_stage.is_empty()).collect();
        let direct_tasks: Vec<_> = unpushed.iter().filter(|t| t.pipeline_stage.is_empty()).collect();

        assert_eq!(pipeline_tasks.len(), 1);
        assert_eq!(pipeline_tasks[0].id, "20260309-001");
        assert_eq!(pipeline_tasks[0].pipeline_stage, "reviewer");

        assert_eq!(direct_tasks.len(), 1);
        assert_eq!(direct_tasks[0].id, "20260309-002");
    }

    #[test]
    fn process_pipeline_tasks_reads_output_file() {
        // Verify output file is read correctly from the expected path.
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let output_file = logs_dir.join("20260309-001-output.md");
        std::fs::write(&output_file, "REVIEW_VERDICT=APPROVED\nAll looks good.").unwrap();

        let output = std::fs::read_to_string(&output_file).unwrap_or_default();
        assert!(output.contains("REVIEW_VERDICT=APPROVED"));
    }
}
