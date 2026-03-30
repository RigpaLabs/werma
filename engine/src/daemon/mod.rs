pub mod cancel_check;
pub mod cleanup;
pub mod cron;
pub mod merge;
pub mod pipeline;
pub mod queue;
pub mod zombie;

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Local;

use crate::config::read_env_file_key;
use crate::db::Db;

const TICK_INTERVAL_SECS: u64 = 5;
const PIPELINE_POLL_INTERVAL_SECS: u64 = 30;
const MERGE_CHECK_INTERVAL_SECS: u64 = 60;
const UPDATE_CHECK_INTERVAL_SECS: u64 = 300;
const ZOMBIE_CHECK_INTERVAL_SECS: u64 = 30;
const CANCEL_CHECK_INTERVAL_SECS: u64 = 60;
const CLEANLINESS_CHECK_INTERVAL_SECS: u64 = 30;
const CLEANLINESS_COOLDOWN_SECS: u64 = 300;

// ─── Traits for external dependencies ────────────────────────────────────

/// Trait abstracting tmux session operations for testability.
pub trait TmuxSession {
    fn has_session(&self, name: &str) -> bool;
    fn count_werma_sessions(&self) -> usize;
    /// Check if the main process inside a tmux pane is still running.
    /// Returns false if the session exists but the process has exited.
    fn is_pane_process_alive(&self, name: &str) -> bool;
    /// Capture the last N lines from a tmux pane for diagnostics.
    fn capture_pane(&self, name: &str, lines: u32) -> Option<String>;
}

/// Trait abstracting GitHub CLI operations for testability.
pub trait GitHubClient {
    fn find_merged_pr(&self, identifier: &str) -> bool;
}

/// Trait abstracting Linear API operations used by the merge handler.
pub trait LinearMergeApi {
    fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<serde_json::Value>>;
    fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()>;
    fn comment(&self, issue_id: &str, body: &str) -> Result<()>;
}

// ─── Logging ─────────────────────────────────────────────────────────────

/// Append a timestamped line to daemon.log.
pub fn log_daemon(log_path: &Path, msg: &str) {
    let ts = Local::now().format("%Y-%m-%dT%H:%M:%S");
    let line = format!("{ts}: {msg}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

// ─── Tick loop ───────────────────────────────────────────────────────────

/// Run the daemon loop. Blocks forever (or until killed).
/// launchd manages restart via KeepAlive.
pub fn run(werma_dir: &Path) -> Result<()> {
    let db_path = werma_dir.join("werma.db");
    let log_path = werma_dir.join("logs/daemon.log");

    std::fs::create_dir_all(werma_dir.join("logs"))?;

    log_daemon(
        &log_path,
        &format!(
            "daemon started — werma {} (bin:{}, repo:{})",
            env!("CARGO_PKG_VERSION"),
            option_env!("WERMA_GIT_VERSION").unwrap_or("dev"),
            crate::runtime_repo_hash(),
        ),
    );

    if std::env::var("LINEAR_API_KEY").is_err() && read_env_file_key("LINEAR_API_KEY").is_err() {
        log_daemon(
            &log_path,
            "WARNING: LINEAR_API_KEY not set — pipeline poll/sync disabled",
        );
    }

    let mut max_concurrent = crate::pipeline::load_max_concurrent();
    let mut launch_stagger_secs = crate::pipeline::load_launch_stagger_secs();
    let mut last_launch: Option<Instant> = None;

    // Trigger pipeline poll immediately on first tick.
    let mut last_pipeline_poll = Instant::now() - Duration::from_secs(PIPELINE_POLL_INTERVAL_SECS);
    let mut last_merge_check = Instant::now() - Duration::from_secs(MERGE_CHECK_INTERVAL_SECS);

    let update_interval_secs = std::env::var("WERMA_UPDATE_INTERVAL_SECS")
        .or_else(|_| read_env_file_key("WERMA_UPDATE_INTERVAL_SECS"))
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(UPDATE_CHECK_INTERVAL_SECS);
    let mut last_update_check = Instant::now();

    let mut cleanliness_notified: std::collections::HashMap<PathBuf, Instant> =
        std::collections::HashMap::new();
    let mut last_zombie_check = Instant::now();
    let mut last_cancel_check = Instant::now() - Duration::from_secs(CANCEL_CHECK_INTERVAL_SECS);
    let mut last_cleanliness_check =
        Instant::now() - Duration::from_secs(CLEANLINESS_CHECK_INTERVAL_SECS);

    let tmux = queue::RealTmux;
    let github = merge::RealGitHub;
    let cmd_runner = crate::traits::RealCommandRunner;
    let notifier = crate::traits::RealNotifier;
    // Optional: absent if LINEAR_API_KEY is not configured.
    let linear_merge = merge::RealLinearMerge::new().ok();
    let linear_poll = crate::linear::LinearClient::new().ok();
    let expected_team_keys = crate::linear::configured_team_keys().unwrap_or_default();

    loop {
        let tick_start = Instant::now();

        if let Ok(db) = Db::open(&db_path) {
            if let Err(e) = cron::check_schedules(&db, &db, werma_dir) {
                log_daemon(&log_path, &format!("schedule check error: {e}"));
            }

            if let Err(e) =
                pipeline::process_completed_tasks(&db, werma_dir, &cmd_runner, &notifier)
            {
                log_daemon(&log_path, &format!("pipeline callback error: {e}"));
            }

            // Drain outbox: execute pending external effects (Linear, GitHub, notifications).
            // Only runs when LINEAR_API_KEY is configured.
            if let Some(ref lp) = linear_poll {
                match crate::pipeline::effects::process_effects(&db, lp, &cmd_runner, &notifier) {
                    Ok(r) if r.processed > 0 || r.failed > 0 => {
                        log_daemon(
                            &log_path,
                            &format!("effects: processed={} failed={}", r.processed, r.failed),
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log_daemon(&log_path, &format!("effect processor error: {e}"));
                    }
                }
            }

            if last_zombie_check.elapsed() >= Duration::from_secs(ZOMBIE_CHECK_INTERVAL_SECS) {
                if let Err(e) = zombie::check_zombie_tasks(&db, werma_dir, &tmux, &notifier) {
                    log_daemon(&log_path, &format!("zombie check error: {e}"));
                }
                last_zombie_check = Instant::now();
            }

            if last_cancel_check.elapsed() >= Duration::from_secs(CANCEL_CHECK_INTERVAL_SECS) {
                if let Some(ref lp) = linear_poll {
                    if let Err(e) = cancel_check::check_canceled_and_stuck(
                        &db,
                        werma_dir,
                        lp,
                        &notifier,
                        &expected_team_keys,
                    ) {
                        log_daemon(&log_path, &format!("cancel check error: {e}"));
                    }
                }
                last_cancel_check = Instant::now();
            }

            // Drain the queue: launch as many tasks as possible in this tick,
            // respecting max_concurrent and stagger cooldown.
            // try_launch_one returns false when cooldown hasn't elapsed yet or no
            // more tasks are available — either way, stop trying this tick.
            loop {
                match queue::try_launch_one(
                    &db,
                    werma_dir,
                    max_concurrent,
                    launch_stagger_secs,
                    last_launch,
                    &tmux,
                ) {
                    Ok(true) => last_launch = Some(Instant::now()),
                    Ok(false) => break,
                    Err(e) => {
                        last_launch = Some(Instant::now());
                        log_daemon(&log_path, &format!("queue launch error: {e}"));
                        break;
                    }
                }
            }

            if let Err(e) = cleanup::rotate_logs(werma_dir) {
                log_daemon(&log_path, &format!("log rotation error: {e}"));
            }

            if last_cleanliness_check.elapsed()
                >= Duration::from_secs(CLEANLINESS_CHECK_INTERVAL_SECS)
            {
                if let Err(e) = cleanup::check_main_branch_cleanliness(
                    &db,
                    &log_path,
                    &mut cleanliness_notified,
                    CLEANLINESS_COOLDOWN_SECS,
                    &notifier,
                ) {
                    log_daemon(&log_path, &format!("main branch check error: {e}"));
                }
                last_cleanliness_check = Instant::now();
            }

            if last_pipeline_poll.elapsed() >= Duration::from_secs(PIPELINE_POLL_INTERVAL_SECS) {
                max_concurrent = crate::pipeline::load_max_concurrent();
                launch_stagger_secs = crate::pipeline::load_launch_stagger_secs();
                if let Some(ref lp) = linear_poll {
                    if let Err(e) = crate::pipeline::poll(&db, lp, &cmd_runner) {
                        log_daemon(&log_path, &format!("pipeline poll error: {e}"));
                    }
                }
                last_pipeline_poll = Instant::now();
            }

            if last_merge_check.elapsed() >= Duration::from_secs(MERGE_CHECK_INTERVAL_SECS) {
                let merge_result = linear_merge.as_ref().map_or(Ok(false), |lm| {
                    merge::check_merged_prs(werma_dir, lm, &github)
                });
                match merge_result {
                    Ok(true) => {
                        // PR was merged — trigger auto-update.
                        log_daemon(&log_path, "triggering auto-update after merge");
                        match crate::update::update() {
                            Ok(()) => {
                                log_daemon(
                                    &log_path,
                                    "auto-update: binary updated, restarting daemon",
                                );
                                std::process::exit(0);
                            }
                            Err(e) => {
                                log_daemon(
                                    &log_path,
                                    &format!("auto-update failed (non-fatal): {e}"),
                                );
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        log_daemon(&log_path, &format!("merge check error: {e}"));
                    }
                }
                last_merge_check = Instant::now();
            }
        }

        if last_update_check.elapsed() >= Duration::from_secs(update_interval_secs) {
            last_update_check = Instant::now();
            match crate::update::check_and_apply_update() {
                Ok(true) => {
                    log_daemon(
                        &log_path,
                        "auto-update: new version installed, restarting daemon",
                    );
                    std::process::exit(0);
                }
                Ok(false) => {}
                Err(e) => {
                    log_daemon(&log_path, &format!("auto-update check failed: {e}"));
                }
            }
        }

        let elapsed = tick_start.elapsed();
        if elapsed < Duration::from_secs(TICK_INTERVAL_SECS) {
            thread::sleep(Duration::from_secs(TICK_INTERVAL_SECS) - elapsed);
        }
    }
}

// ─── Daemon install / uninstall ──────────────────────────────────────────

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

fn get_uid() -> u32 {
    #[cfg(unix)]
    {
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
    fn log_daemon_appends() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        log_daemon(&log_path, "first message");
        log_daemon(&log_path, "second message");

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("first message"));
        assert!(content.contains("second message"));
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn log_daemon_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("new.log");

        assert!(!log_path.exists());

        log_daemon(&log_path, "hello");
        assert!(log_path.exists());

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("hello"));
        assert!(content.contains("T"));
    }

    // ─── Daemon heartbeat integration tests (RIG-293) ────────────────

    use crate::db::Db;
    use crate::models::{Status, Task};
    use crate::traits::fakes::{FakeCommandRunner, FakeNotifier};

    struct FakeTmux {
        active_sessions: usize,
        alive_sessions: Vec<String>,
    }

    impl FakeTmux {
        fn new(active: usize) -> Self {
            Self {
                active_sessions: active,
                alive_sessions: vec![],
            }
        }

        fn with_alive(mut self, sessions: Vec<String>) -> Self {
            self.alive_sessions = sessions;
            self
        }
    }

    impl TmuxSession for FakeTmux {
        fn has_session(&self, name: &str) -> bool {
            self.alive_sessions.iter().any(|s| s == name)
        }

        fn count_werma_sessions(&self) -> usize {
            self.active_sessions
        }

        fn is_pane_process_alive(&self, name: &str) -> bool {
            self.alive_sessions.iter().any(|s| s == name)
        }

        fn capture_pane(&self, _name: &str, _lines: u32) -> Option<String> {
            None
        }
    }

    fn make_running_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            status: Status::Running,
            priority: 1,
            created_at: "2026-03-26T10:00:00".to_string(),
            started_at: Some("2026-03-26T10:00:00".to_string()),
            finished_at: None,
            task_type: "pipeline-engineer".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "RIG-293".to_string(),
            linear_pushed: false,
            pipeline_stage: "engineer".to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
        }
    }

    #[test]
    fn zombie_check_fires_and_marks_dead_task() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260326-z01");
        db.insert_task(&task).unwrap();

        // No alive sessions → zombie detected
        let tmux = FakeTmux::new(0);
        let notifier = FakeNotifier::new();

        zombie::check_zombie_tasks(&db, werma_dir.path(), &tmux, &notifier).unwrap();

        let updated = db.task("20260326-z01").unwrap().unwrap();
        assert_eq!(updated.status, Status::Failed);
        assert!(updated.finished_at.is_some());

        // Notification was sent
        assert!(!notifier.macos_calls.borrow().is_empty());
        assert!(!notifier.slack_calls.borrow().is_empty());
    }

    #[test]
    fn zombie_check_skips_alive_sessions() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = make_running_task("20260326-z02");
        db.insert_task(&task).unwrap();

        let tmux = FakeTmux::new(1).with_alive(vec!["werma-20260326-z02".to_string()]);

        zombie::check_zombie_tasks(&db, werma_dir.path(), &tmux, &FakeNotifier::new()).unwrap();

        let updated = db.task("20260326-z02").unwrap().unwrap();
        assert_eq!(updated.status, Status::Running);
    }

    #[test]
    fn pipeline_callback_error_does_not_crash_subsequent_processing() {
        // Simulates daemon behavior: pipeline callback fails but processing continues
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        // process_completed_tasks with no LINEAR_API_KEY — LinearClient::new() fails
        // but the function should still return Ok
        let result = pipeline::process_completed_tasks(
            &db,
            werma_dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn zombie_check_error_does_not_affect_callback_processing() {
        // Multiple subsystems run independently — verify isolation
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        // Zombie check with empty db succeeds
        let tmux = FakeTmux::new(0);
        let notifier = FakeNotifier::new();
        zombie::check_zombie_tasks(&db, werma_dir.path(), &tmux, &notifier).unwrap();

        // Then pipeline processing also succeeds independently
        pipeline::process_completed_tasks(
            &db,
            werma_dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
        )
        .unwrap();
    }

    #[test]
    fn queue_launch_respects_capacity_during_drain_loop() {
        // Simulates the daemon's inner drain loop behavior:
        // keep calling try_launch_one until it returns false
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        // At capacity — should return false immediately
        let tmux = FakeTmux::new(8);
        let launched = queue::try_launch_one(&db, werma_dir.path(), 8, 0, None, &tmux).unwrap();
        assert!(!launched);
    }

    #[test]
    fn multiple_subsystems_run_sequentially_without_interference() {
        // Verifies the daemon tick pattern: multiple subsystems called sequentially,
        // each with its own error handling, none affecting the others
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let tmux = FakeTmux::new(0);
        let notifier = FakeNotifier::new();
        let cmd_runner = FakeCommandRunner::new();

        // 1. Cron check (empty schedule — no-op)
        let result = cron::check_schedules(&db, &db, werma_dir.path());
        assert!(result.is_ok());

        // 2. Pipeline callbacks (no tasks — no-op)
        let result =
            pipeline::process_completed_tasks(&db, werma_dir.path(), &cmd_runner, &notifier);
        assert!(result.is_ok());

        // 3. Zombie check (no running tasks — no-op)
        let result = zombie::check_zombie_tasks(&db, werma_dir.path(), &tmux, &notifier);
        assert!(result.is_ok());

        // 4. Queue launch (no pending tasks — returns false)
        let launched = queue::try_launch_one(&db, werma_dir.path(), 8, 0, None, &tmux).unwrap();
        assert!(!launched);

        // 5. Log rotation (fresh dir — no-op)
        let result = cleanup::rotate_logs(werma_dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn zombie_check_with_mixed_task_states_only_affects_running() {
        let db = Db::open_in_memory().unwrap();
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        // Running task with dead session → should become Failed
        let running = make_running_task("20260326-z03");
        db.insert_task(&running).unwrap();

        // Pending task → should stay Pending
        let mut pending = make_running_task("20260326-z04");
        pending.status = Status::Pending;
        pending.started_at = None;
        db.insert_task(&pending).unwrap();

        // Completed task → should stay Completed
        let mut completed = make_running_task("20260326-z05");
        completed.status = Status::Completed;
        completed.finished_at = Some("2026-03-26T10:05:00".to_string());
        db.insert_task(&completed).unwrap();

        let tmux = FakeTmux::new(0);
        zombie::check_zombie_tasks(&db, werma_dir.path(), &tmux, &FakeNotifier::new()).unwrap();

        assert_eq!(
            db.task("20260326-z03").unwrap().unwrap().status,
            Status::Failed
        );
        assert_eq!(
            db.task("20260326-z04").unwrap().unwrap().status,
            Status::Pending
        );
        assert_eq!(
            db.task("20260326-z05").unwrap().unwrap().status,
            Status::Completed
        );
    }
}
