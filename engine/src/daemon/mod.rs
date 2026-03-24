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
            if let Err(e) = cron::check_schedules(&db, werma_dir) {
                log_daemon(&log_path, &format!("schedule check error: {e}"));
            }

            if let Err(e) = pipeline::process_completed_tasks(&db, werma_dir) {
                log_daemon(&log_path, &format!("pipeline callback error: {e}"));
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

            match queue::try_launch_one(
                &db,
                werma_dir,
                max_concurrent,
                launch_stagger_secs,
                last_launch,
                &tmux,
            ) {
                Ok(true) => last_launch = Some(Instant::now()),
                Ok(false) => {}
                Err(e) => {
                    last_launch = Some(Instant::now());
                    log_daemon(&log_path, &format!("queue launch error: {e}"));
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
}
