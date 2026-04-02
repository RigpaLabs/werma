mod art;
mod backup;
mod build;
mod cli;
mod commands;
mod config;
mod daemon;
mod dashboard;
#[allow(dead_code)]
mod db;
#[allow(dead_code)]
mod github;
#[allow(dead_code)]
mod issue_helpers;
#[allow(dead_code)]
mod linear;
mod migrate;
#[allow(dead_code)]
mod models;
#[allow(dead_code)]
mod notify;
mod pipeline;
mod project;
mod runner;
mod tracker;
mod traits;
mod ui;
mod update;
mod worktree;

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod full_cycle_tests;

#[cfg(test)]
mod regression_tests;

#[cfg(all(test, feature = "e2e"))]
mod e2e_helpers;

#[cfg(all(test, feature = "e2e"))]
mod e2e_tests;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::db::Db;

// Re-export display helpers so other modules can use `crate::function_name`
pub use commands::display::{default_turns, format_duration_between, format_elapsed_since};

/// Build a version string for display.
/// Returns &'static str because clap requires it.
pub fn version_string() -> &'static str {
    let version = match option_env!("WERMA_GIT_VERSION") {
        Some(tag) => {
            let v = tag.strip_prefix('v').unwrap_or(tag);
            v.to_string()
        }
        None => {
            format!("{}-dev", env!("CARGO_PKG_VERSION"))
        }
    };
    Box::leak(version.into_boxed_str())
}

/// Get the current git HEAD hash of the werma repo at runtime.
pub fn runtime_repo_hash() -> String {
    let repo = std::env::var("WERMA_REPO").unwrap_or_else(|_| {
        let cfg = config::UserConfig::load();
        let dir = cfg.repo_dir("werma");
        pipeline::helpers::resolve_home(&dir)
            .to_string_lossy()
            .into_owned()
    });
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&repo)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

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

fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Version => commands::misc::cmd_version(),

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
            runtime,
        } => {
            let db = open_db()?;
            commands::task::cmd_add(
                &db,
                commands::task::AddParams {
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
                    runtime,
                },
            )?;
        }

        cli::Commands::List { status } => {
            let db = open_db()?;
            commands::task::cmd_list(&db, status.as_deref())?;
        }

        cli::Commands::Status {
            watch,
            compact,
            plain,
            interval,
            all,
        } => {
            let db = open_db()?;
            let cfg = config::UserConfig::load();
            commands::task::cmd_status(
                &db,
                watch,
                compact,
                plain,
                interval,
                all,
                cfg.resolved_completed_limit(),
            )?;
        }

        cli::Commands::View { id } => {
            let db = open_db()?;
            commands::task::cmd_view(&db, &id)?;
        }

        cli::Commands::Retry { id } => {
            let db = open_db()?;
            commands::task::cmd_retry(&db, &id)?;
        }

        cli::Commands::Kill { id } => {
            let db = open_db()?;
            commands::task::cmd_kill(&db, &id)?;
        }

        cli::Commands::Complete {
            id,
            session,
            result_file,
            cost,
            turns,
        } => {
            let db = open_db()?;
            commands::task::cmd_complete(
                &db,
                &id,
                session.as_deref(),
                result_file.as_deref(),
                cost,
                turns,
            )?;
        }

        cli::Commands::Peek { id } => {
            let db = open_db()?;
            commands::task::cmd_peek(&db, &id)?;
        }

        cli::Commands::Fail { id } => {
            let db = open_db()?;
            commands::task::cmd_fail(&db, &id)?;
        }

        cli::Commands::Clean { force } => {
            let db = open_db()?;
            commands::clean::cmd_clean_worktrees(&db, force)?;
        }

        cli::Commands::Log { id } => {
            commands::task::cmd_log(id)?;
        }

        cli::Commands::Continue { id, prompt } => {
            let db = open_db()?;
            commands::task::cmd_continue(&db, &id, prompt)?;
        }

        cli::Commands::Run => {
            let db = open_db()?;
            commands::task::cmd_run(&db)?;
        }

        cli::Commands::RunAll => {
            let db = open_db()?;
            commands::task::cmd_run_all(&db)?;
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
                    commands::schedule::cmd_sched_add(
                        &db,
                        commands::schedule::SchedAddParams {
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
                    commands::schedule::cmd_sched_list(&db)?;
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
                    commands::schedule::cmd_sched_trigger(&db, &id)?;
                }
            }
        }

        cli::Commands::Daemon { action } => match action {
            Some(cli::DaemonAction::Install) => commands::daemon_cmd::cmd_daemon_install()?,
            Some(cli::DaemonAction::Uninstall) => commands::daemon_cmd::cmd_daemon_uninstall()?,
            None => commands::daemon_cmd::cmd_daemon_run()?,
        },

        cli::Commands::Linear { action } => {
            let db = open_db()?;
            match action {
                cli::LinearAction::Setup => commands::linear_cmd::cmd_linear_setup()?,
                cli::LinearAction::Sync => commands::linear_cmd::cmd_linear_sync(&db)?,
                cli::LinearAction::Push { id } => commands::linear_cmd::cmd_linear_push(&db, &id)?,
                cli::LinearAction::PushAll => commands::linear_cmd::cmd_linear_push_all(&db)?,
            }
        }

        cli::Commands::Pipeline { action } => {
            let db = open_db()?;
            match action {
                cli::PipelineAction::Poll => commands::pipeline_cmd::cmd_pipeline_poll(&db)?,
                cli::PipelineAction::Status => commands::pipeline_cmd::cmd_pipeline_status(&db)?,
                cli::PipelineAction::Show { stage, pipeline } => {
                    commands::pipeline_cmd::cmd_pipeline_show(
                        stage.as_deref(),
                        pipeline.as_deref(),
                    )?;
                }
                cli::PipelineAction::Validate => commands::pipeline_cmd::cmd_pipeline_validate()?,
                cli::PipelineAction::Run { issues, stage } => {
                    commands::pipeline_cmd::cmd_pipeline_run(&issues, stage.as_deref())?;
                }
                cli::PipelineAction::Switch { repo, pipeline } => {
                    commands::pipeline_cmd::cmd_pipeline_switch(&repo, &pipeline)?;
                }
            }
        }

        cli::Commands::Build => commands::misc::cmd_build()?,

        cli::Commands::Update => commands::misc::cmd_update()?,

        cli::Commands::Review { target, dir, force } => {
            let db = open_db()?;
            let wdir = werma_dir()?;
            commands::review::cmd_review(&db, &wdir, target.as_deref(), dir.as_deref(), force)?;
        }

        cli::Commands::Config { action } => match action {
            cli::ConfigAction::Show => commands::config_cmd::cmd_config_show()?,
        },

        cli::Commands::Dash => {
            let db = open_db()?;
            commands::misc::cmd_dash(&db)?;
        }

        cli::Commands::Backup => commands::misc::cmd_backup()?,

        cli::Commands::Migrate => {
            let db = open_db()?;
            commands::misc::cmd_migrate(&db)?;
        }

        cli::Commands::Effects { action } => {
            let db = open_db()?;
            match action {
                None => commands::effects::cmd_effects_list(&db)?,
                Some(cli::EffectsAction::Dead) => commands::effects::cmd_effects_dead(&db)?,
                Some(cli::EffectsAction::Retry { id }) => {
                    commands::effects::cmd_effects_retry(&db, id)?;
                }
                Some(cli::EffectsAction::History { task_id }) => {
                    commands::effects::cmd_effects_history(&db, &task_id)?;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_returns_non_empty() {
        let v = version_string();
        assert!(!v.is_empty());
        // Dev builds have "-dev" suffix
        assert!(v.contains("-dev") || v.chars().next().unwrap().is_ascii_digit());
    }

    #[test]
    fn runtime_repo_hash_returns_something() {
        let hash = runtime_repo_hash();
        assert!(!hash.is_empty());
    }

    // CLI parsing tests
    mod cli_parsing {
        use crate::cli::{Cli, Commands};
        use clap::Parser;

        fn parse(args: &[&str]) -> Commands {
            let mut full_args = vec!["werma"];
            full_args.extend_from_slice(args);
            Cli::parse_from(full_args).command
        }

        #[test]
        fn parse_add_minimal() {
            match parse(&["add", "test prompt"]) {
                Commands::Add {
                    prompt,
                    priority,
                    task_type,
                    model,
                    ..
                } => {
                    assert_eq!(prompt, "test prompt");
                    assert_eq!(priority, 2); // default
                    assert_eq!(task_type, "custom"); // default
                    assert_eq!(model, "opus"); // default
                }
                other => panic!("expected Add, got {other:?}"),
            }
        }

        #[test]
        fn parse_add_full() {
            match parse(&[
                "add",
                "do stuff",
                "-p",
                "1",
                "-t",
                "code",
                "-m",
                "sonnet",
                "--turns",
                "25",
                "--depends",
                "dep1,dep2",
                "--context",
                "ctx.md",
                "--linear",
                "RIG-42",
                "--stage",
                "engineer",
                "-o",
                "/tmp/out.md",
                "-d",
                "/tmp/work",
            ]) {
                Commands::Add {
                    prompt,
                    priority,
                    task_type,
                    model,
                    turns,
                    depends,
                    context,
                    linear,
                    stage,
                    output,
                    dir,
                    ..
                } => {
                    assert_eq!(prompt, "do stuff");
                    assert_eq!(priority, 1);
                    assert_eq!(task_type, "code");
                    assert_eq!(model, "sonnet");
                    assert_eq!(turns, Some(25));
                    assert_eq!(depends, Some("dep1,dep2".into()));
                    assert_eq!(context, Some("ctx.md".into()));
                    assert_eq!(linear, Some("RIG-42".into()));
                    assert_eq!(stage, Some("engineer".into()));
                    assert_eq!(output, Some("/tmp/out.md".into()));
                    assert_eq!(dir, Some("/tmp/work".into()));
                }
                other => panic!("expected Add, got {other:?}"),
            }
        }

        #[test]
        fn parse_add_with_runtime() {
            match parse(&["add", "test prompt", "--runtime", "codex"]) {
                Commands::Add { runtime, .. } => {
                    assert_eq!(runtime, "codex");
                }
                other => panic!("expected Add, got {other:?}"),
            }
        }

        #[test]
        fn parse_add_with_gemini_runtime() {
            match parse(&["add", "test prompt", "--runtime", "gemini-cli"]) {
                Commands::Add { runtime, .. } => {
                    assert_eq!(runtime, "gemini-cli");
                }
                other => panic!("expected Add, got {other:?}"),
            }
        }

        #[test]
        fn parse_add_with_qwen_runtime() {
            match parse(&["add", "test prompt", "--runtime", "qwen-code"]) {
                Commands::Add { runtime, .. } => {
                    assert_eq!(runtime, "qwen-code");
                }
                other => panic!("expected Add, got {other:?}"),
            }
        }

        #[test]
        fn parse_add_default_runtime() {
            match parse(&["add", "test prompt"]) {
                Commands::Add { runtime, .. } => {
                    assert_eq!(runtime, "claude-code");
                }
                other => panic!("expected Add, got {other:?}"),
            }
        }

        #[test]
        fn parse_list_no_filter() {
            match parse(&["list"]) {
                Commands::List { status } => assert!(status.is_none()),
                other => panic!("expected List, got {other:?}"),
            }
        }

        #[test]
        fn parse_list_with_filter() {
            match parse(&["list", "running"]) {
                Commands::List { status } => assert_eq!(status, Some("running".into())),
                other => panic!("expected List, got {other:?}"),
            }
        }

        #[test]
        fn parse_list_alias_ls() {
            match parse(&["ls"]) {
                Commands::List { status } => assert!(status.is_none()),
                other => panic!("expected List, got {other:?}"),
            }
        }

        #[test]
        fn parse_status_defaults() {
            match parse(&["status"]) {
                Commands::Status {
                    watch,
                    compact,
                    plain,
                    interval,
                    all,
                } => {
                    assert!(!watch);
                    assert!(!compact);
                    assert!(!plain);
                    assert_eq!(interval, 3);
                    assert!(!all);
                }
                other => panic!("expected Status, got {other:?}"),
            }
        }

        #[test]
        fn parse_status_alias_st() {
            match parse(&["st"]) {
                Commands::Status { .. } => {}
                other => panic!("expected Status, got {other:?}"),
            }
        }

        #[test]
        fn parse_status_with_flags() {
            match parse(&["status", "-w", "-c", "-i", "5"]) {
                Commands::Status {
                    watch,
                    compact,
                    plain,
                    interval,
                    all,
                } => {
                    assert!(watch);
                    assert!(compact);
                    assert!(!plain);
                    assert_eq!(interval, 5);
                    assert!(!all);
                }
                other => panic!("expected Status, got {other:?}"),
            }
        }

        #[test]
        fn parse_status_plain_flag() {
            match parse(&["status", "--plain"]) {
                Commands::Status {
                    watch,
                    compact,
                    plain,
                    interval,
                    all,
                } => {
                    assert!(!watch);
                    assert!(!compact);
                    assert!(plain);
                    assert_eq!(interval, 3);
                    assert!(!all);
                }
                other => panic!("expected Status, got {other:?}"),
            }
        }

        #[test]
        fn parse_status_all_flag() {
            match parse(&["st", "--all"]) {
                Commands::Status { all, .. } => {
                    assert!(all);
                }
                other => panic!("expected Status, got {other:?}"),
            }
        }

        #[test]
        fn parse_status_plain_short_flag() {
            match parse(&["st", "-p"]) {
                Commands::Status { plain, .. } => {
                    assert!(plain);
                }
                other => panic!("expected Status, got {other:?}"),
            }
        }

        #[test]
        fn parse_view() {
            match parse(&["view", "20260313-001"]) {
                Commands::View { id } => assert_eq!(id, "20260313-001"),
                other => panic!("expected View, got {other:?}"),
            }
        }

        #[test]
        fn parse_continue_with_prompt() {
            match parse(&["continue", "task-1", "keep going"]) {
                Commands::Continue { id, prompt } => {
                    assert_eq!(id, "task-1");
                    assert_eq!(prompt, Some("keep going".into()));
                }
                other => panic!("expected Continue, got {other:?}"),
            }
        }

        #[test]
        fn parse_continue_alias_cont() {
            match parse(&["cont", "task-1"]) {
                Commands::Continue { id, prompt } => {
                    assert_eq!(id, "task-1");
                    assert!(prompt.is_none());
                }
                other => panic!("expected Continue, got {other:?}"),
            }
        }

        #[test]
        fn parse_retry() {
            match parse(&["retry", "task-1"]) {
                Commands::Retry { id } => assert_eq!(id, "task-1"),
                other => panic!("expected Retry, got {other:?}"),
            }
        }

        #[test]
        fn parse_kill() {
            match parse(&["kill", "task-1"]) {
                Commands::Kill { id } => assert_eq!(id, "task-1"),
                other => panic!("expected Kill, got {other:?}"),
            }
        }

        #[test]
        fn parse_complete_with_options() {
            match parse(&[
                "complete",
                "task-1",
                "--session",
                "sess-abc",
                "--result-file",
                "/tmp/r.md",
                "--cost",
                "1.23",
                "--turns",
                "42",
            ]) {
                Commands::Complete {
                    id,
                    session,
                    result_file,
                    cost,
                    turns,
                } => {
                    assert_eq!(id, "task-1");
                    assert_eq!(session, Some("sess-abc".into()));
                    assert_eq!(result_file, Some("/tmp/r.md".into()));
                    assert!((cost.unwrap() - 1.23).abs() < f64::EPSILON);
                    assert_eq!(turns, Some(42));
                }
                other => panic!("expected Complete, got {other:?}"),
            }
        }

        #[test]
        fn parse_fail() {
            match parse(&["fail", "task-1"]) {
                Commands::Fail { id } => assert_eq!(id, "task-1"),
                other => panic!("expected Fail, got {other:?}"),
            }
        }

        #[test]
        fn parse_run() {
            match parse(&["run"]) {
                Commands::Run => {}
                other => panic!("expected Run, got {other:?}"),
            }
        }

        #[test]
        fn parse_run_all() {
            match parse(&["run-all"]) {
                Commands::RunAll => {}
                other => panic!("expected RunAll, got {other:?}"),
            }
        }

        #[test]
        fn parse_clean_default() {
            match parse(&["clean"]) {
                Commands::Clean { force } => assert!(!force),
                other => panic!("expected Clean, got {other:?}"),
            }
        }

        #[test]
        fn parse_clean_force() {
            match parse(&["clean", "--force"]) {
                Commands::Clean { force } => assert!(force),
                other => panic!("expected Clean, got {other:?}"),
            }
        }

        #[test]
        fn parse_log_no_id() {
            match parse(&["log"]) {
                Commands::Log { id } => assert!(id.is_none()),
                other => panic!("expected Log, got {other:?}"),
            }
        }

        #[test]
        fn parse_log_with_id() {
            match parse(&["log", "task-1"]) {
                Commands::Log { id } => assert_eq!(id, Some("task-1".into())),
                other => panic!("expected Log, got {other:?}"),
            }
        }

        #[test]
        fn parse_daemon_install() {
            match parse(&["daemon", "install"]) {
                Commands::Daemon {
                    action: Some(crate::cli::DaemonAction::Install),
                } => {}
                other => panic!("expected Daemon Install, got {other:?}"),
            }
        }

        #[test]
        fn parse_daemon_no_action() {
            match parse(&["daemon"]) {
                Commands::Daemon { action: None } => {}
                other => panic!("expected Daemon (no action), got {other:?}"),
            }
        }

        #[test]
        fn parse_sched_add() {
            match parse(&[
                "sched",
                "add",
                "daily",
                "30 7 * * *",
                "do stuff",
                "-t",
                "research",
            ]) {
                Commands::Sched {
                    action:
                        crate::cli::SchedAction::Add {
                            id,
                            cron,
                            prompt,
                            task_type,
                            ..
                        },
                } => {
                    assert_eq!(id, "daily");
                    assert_eq!(cron, "30 7 * * *");
                    assert_eq!(prompt, "do stuff");
                    assert_eq!(task_type, "research");
                }
                other => panic!("expected Sched Add, got {other:?}"),
            }
        }

        #[test]
        fn parse_sched_list() {
            match parse(&["sched", "list"]) {
                Commands::Sched {
                    action: crate::cli::SchedAction::List,
                } => {}
                other => panic!("expected Sched List, got {other:?}"),
            }
        }

        #[test]
        fn parse_sched_ls_alias() {
            match parse(&["sched", "ls"]) {
                Commands::Sched {
                    action: crate::cli::SchedAction::List,
                } => {}
                other => panic!("expected Sched List, got {other:?}"),
            }
        }

        #[test]
        fn parse_sched_on_off_rm() {
            match parse(&["sched", "on", "daily"]) {
                Commands::Sched {
                    action: crate::cli::SchedAction::On { id },
                } => assert_eq!(id, "daily"),
                other => panic!("expected Sched On, got {other:?}"),
            }
            match parse(&["sched", "off", "daily"]) {
                Commands::Sched {
                    action: crate::cli::SchedAction::Off { id },
                } => assert_eq!(id, "daily"),
                other => panic!("expected Sched Off, got {other:?}"),
            }
            match parse(&["sched", "rm", "daily"]) {
                Commands::Sched {
                    action: crate::cli::SchedAction::Rm { id },
                } => assert_eq!(id, "daily"),
                other => panic!("expected Sched Rm, got {other:?}"),
            }
        }

        #[test]
        fn parse_sched_trigger() {
            match parse(&["sched", "trigger", "daily"]) {
                Commands::Sched {
                    action: crate::cli::SchedAction::Trigger { id },
                } => assert_eq!(id, "daily"),
                other => panic!("expected Sched Trigger, got {other:?}"),
            }
        }

        #[test]
        fn parse_linear_setup() {
            match parse(&["linear", "setup"]) {
                Commands::Linear {
                    action: crate::cli::LinearAction::Setup,
                } => {}
                other => panic!("expected Linear Setup, got {other:?}"),
            }
        }

        #[test]
        fn parse_linear_sync() {
            match parse(&["linear", "sync"]) {
                Commands::Linear {
                    action: crate::cli::LinearAction::Sync,
                } => {}
                other => panic!("expected Linear Sync, got {other:?}"),
            }
        }

        #[test]
        fn parse_linear_push() {
            match parse(&["linear", "push", "task-1"]) {
                Commands::Linear {
                    action: crate::cli::LinearAction::Push { id },
                } => assert_eq!(id, "task-1"),
                other => panic!("expected Linear Push, got {other:?}"),
            }
        }

        #[test]
        fn parse_linear_push_all() {
            match parse(&["linear", "push-all"]) {
                Commands::Linear {
                    action: crate::cli::LinearAction::PushAll,
                } => {}
                other => panic!("expected Linear PushAll, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_poll() {
            match parse(&["pipeline", "poll"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Poll,
                } => {}
                other => panic!("expected Pipeline Poll, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_status() {
            match parse(&["pipeline", "status"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Status,
                } => {}
                other => panic!("expected Pipeline Status, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_show() {
            match parse(&["pipeline", "show"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Show { stage, pipeline },
                } => {
                    assert!(stage.is_none());
                    assert!(pipeline.is_none());
                }
                other => panic!("expected Pipeline Show, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_show_with_stage() {
            match parse(&["pipeline", "show", "--stage", "engineer"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Show { stage, pipeline },
                } => {
                    assert_eq!(stage, Some("engineer".into()));
                    assert!(pipeline.is_none());
                }
                other => panic!("expected Pipeline Show, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_show_with_pipeline() {
            match parse(&["pipeline", "show", "--pipeline", "honeyjourney"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Show { stage, pipeline },
                } => {
                    assert!(stage.is_none());
                    assert_eq!(pipeline, Some("honeyjourney".into()));
                }
                other => panic!("expected Pipeline Show, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_validate() {
            match parse(&["pipeline", "validate"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Validate,
                } => {}
                other => panic!("expected Pipeline Validate, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_run() {
            match parse(&["pipeline", "run", "RIG-42", "RIG-43"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Run { issues, stage },
                } => {
                    assert_eq!(issues, vec!["RIG-42", "RIG-43"]);
                    assert!(stage.is_none());
                }
                other => panic!("expected Pipeline Run, got {other:?}"),
            }
        }

        #[test]
        fn parse_pipeline_run_with_stage() {
            match parse(&["pipeline", "run", "RIG-42", "--stage", "engineer"]) {
                Commands::Pipeline {
                    action: crate::cli::PipelineAction::Run { issues, stage },
                } => {
                    assert_eq!(issues, vec!["RIG-42"]);
                    assert_eq!(stage, Some("engineer".into()));
                }
                other => panic!("expected Pipeline Run, got {other:?}"),
            }
        }

        #[test]
        fn parse_review_no_target() {
            match parse(&["review"]) {
                Commands::Review { target, dir, force } => {
                    assert!(target.is_none());
                    assert!(dir.is_none());
                    assert!(!force);
                }
                other => panic!("expected Review, got {other:?}"),
            }
        }

        #[test]
        fn parse_review_with_target() {
            match parse(&["review", "#42", "-d", "/tmp/repo", "-f"]) {
                Commands::Review { target, dir, force } => {
                    assert_eq!(target, Some("#42".into()));
                    assert_eq!(dir, Some("/tmp/repo".into()));
                    assert!(force);
                }
                other => panic!("expected Review, got {other:?}"),
            }
        }

        #[test]
        fn parse_dash() {
            match parse(&["dash"]) {
                Commands::Dash => {}
                other => panic!("expected Dash, got {other:?}"),
            }
        }

        #[test]
        fn parse_backup() {
            match parse(&["backup"]) {
                Commands::Backup => {}
                other => panic!("expected Backup, got {other:?}"),
            }
        }

        #[test]
        fn parse_migrate() {
            match parse(&["migrate"]) {
                Commands::Migrate => {}
                other => panic!("expected Migrate, got {other:?}"),
            }
        }

        #[test]
        fn parse_build() {
            match parse(&["build"]) {
                Commands::Build => {}
                other => panic!("expected Build, got {other:?}"),
            }
        }

        #[test]
        fn parse_update() {
            match parse(&["update"]) {
                Commands::Update => {}
                other => panic!("expected Update, got {other:?}"),
            }
        }

        #[test]
        fn parse_version() {
            match parse(&["version"]) {
                Commands::Version => {}
                other => panic!("expected Version, got {other:?}"),
            }
        }

        #[test]
        fn parse_effects_no_subcommand() {
            match parse(&["effects"]) {
                Commands::Effects { action: None } => {}
                other => panic!("expected Effects (no action), got {other:?}"),
            }
        }

        #[test]
        fn parse_effects_dead() {
            match parse(&["effects", "dead"]) {
                Commands::Effects {
                    action: Some(crate::cli::EffectsAction::Dead),
                } => {}
                other => panic!("expected Effects Dead, got {other:?}"),
            }
        }

        #[test]
        fn parse_effects_retry() {
            match parse(&["effects", "retry", "42"]) {
                Commands::Effects {
                    action: Some(crate::cli::EffectsAction::Retry { id }),
                } => assert_eq!(id, 42),
                other => panic!("expected Effects Retry, got {other:?}"),
            }
        }

        #[test]
        fn parse_effects_history() {
            match parse(&["effects", "history", "20260326-001"]) {
                Commands::Effects {
                    action: Some(crate::cli::EffectsAction::History { task_id }),
                } => assert_eq!(task_id, "20260326-001"),
                other => panic!("expected Effects History, got {other:?}"),
            }
        }
    }
}
