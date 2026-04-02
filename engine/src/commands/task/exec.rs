use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::models::Task;
use crate::{notify, runner, worktree};

use super::super::display::*;

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

/// Validated + resolved params for continue, ready for script generation and tmux spawn.
#[derive(Debug)]
struct PreparedContinue {
    effective_dir: String,
    tools: String,
    model_id: String,
    follow_up: String,
    session_name: String,
}

/// Validate a task for continue and resolve derived fields.
/// Pure logic — no I/O, no tmux, no filesystem writes.
fn prepare_continue(task: &Task, id: &str, prompt: Option<String>) -> Result<PreparedContinue> {
    if task.runtime != crate::models::AgentRuntime::ClaudeCode {
        bail!(
            "cannot continue {} task {id} — only Claude Code supports session resume. \
             Re-run with `werma retry {id}` instead.",
            task.runtime
        );
    }

    if task.session_id.is_empty() {
        bail!("no session_id for task {id}");
    }

    let follow_up = prompt.unwrap_or_else(|| "Continue the task.".to_string());
    let model_id = runner::model_flag(&task.model).to_string();
    let session_name = format!("werma-{id}-cont");

    let tools = if task.allowed_tools.is_empty() {
        runner::tools_for_type(&task.task_type, !task.output_path.is_empty())
    } else {
        task.allowed_tools.clone()
    };

    let working_dir = expand_tilde(&task.working_dir);

    let effective_dir = if worktree::needs_worktree(&task.task_type) {
        let branch = worktree::generate_branch_name(task);
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

    Ok(PreparedContinue {
        effective_dir,
        tools,
        model_id,
        follow_up,
        session_name,
    })
}

/// Parameters for continue script generation — mirrors ExecScriptParams from runner.rs.
pub(crate) struct ContinueScriptParams<'a> {
    pub effective_dir: &'a str,
    pub prompt_file: &'a Path,
    pub session_id: &'a str,
    pub tools: &'a str,
    pub model_id: &'a str,
    pub log_file: &'a Path,
    pub notify_label: &'a str,
}

/// Generate a self-contained bash script for continuing a task via `claude --resume`.
pub(crate) fn generate_continue_script(params: &ContinueScriptParams<'_>) -> String {
    format!(
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
        effective_dir = params.effective_dir,
        prompt_file = params.prompt_file.display(),
        session_id = params.session_id.replace('\'', "'\\''"),
        tools = params.tools.replace('\'', "'\\''"),
        model_id = params.model_id,
        log_file = params.log_file.display(),
        notify_label = params.notify_label.replace('"', "\\\""),
    )
}

pub fn cmd_continue(db: &Db, id: &str, prompt: Option<String>) -> Result<()> {
    let task = db.task(id)?.context(format!("task not found: {id}"))?;
    let prepared = prepare_continue(&task, id, prompt)?;

    let wdir = crate::werma_dir()?;
    let logs_dir = wdir.join("logs");
    let log_file = logs_dir.join(format!("{id}.log"));
    let prompt_file = logs_dir.join(format!("{id}-cont-prompt.txt"));
    let exec_script = logs_dir.join(format!("{id}-cont-exec.sh"));

    std::fs::write(&prompt_file, &prepared.follow_up)?;

    let notify_label = notify::format_notify_label(id, &task.task_type, &task.linear_issue_id);

    let script = generate_continue_script(&ContinueScriptParams {
        effective_dir: &prepared.effective_dir,
        prompt_file: &prompt_file,
        session_id: &task.session_id,
        tools: &prepared.tools,
        model_id: &prepared.model_id,
        log_file: &log_file,
        notify_label: &notify_label,
    });

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
            &prepared.session_name,
            &format!("bash {}", exec_script.display()),
        ])
        .output();

    match result {
        Ok(out) if out.status.success() => {
            println!("continue: {id} -> tmux: {}", prepared.session_name);
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("tmux failed: {stderr}");
        }
        Err(e) => bail!("failed to spawn tmux: {e}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::db::make_test_task;
    use crate::models::AgentRuntime;

    fn make_continue_task(id: &str) -> Task {
        let mut task = make_test_task(id);
        task.session_id = "abc123".to_string();
        task.status = crate::models::Status::Running;
        task
    }

    // --- generate_continue_script tests ---

    #[test]
    fn continue_script_contains_resume_flag() {
        let script = generate_continue_script(&ContinueScriptParams {
            effective_dir: "/tmp/project",
            prompt_file: Path::new("/tmp/prompt.txt"),
            session_id: "sess-abc",
            tools: "Read,Write",
            model_id: "claude-sonnet-4-6",
            log_file: Path::new("/tmp/task.log"),
            notify_label: "test-001",
        });
        assert!(
            script.contains("--resume 'sess-abc'"),
            "script must contain --resume flag"
        );
    }

    #[test]
    fn continue_script_escapes_session_id() {
        let script = generate_continue_script(&ContinueScriptParams {
            effective_dir: "/tmp",
            prompt_file: Path::new("/tmp/p.txt"),
            session_id: "it's-a-session",
            tools: "Read",
            model_id: "claude-sonnet-4-6",
            log_file: Path::new("/tmp/t.log"),
            notify_label: "t",
        });
        assert!(
            script.contains("it'\\''s-a-session"),
            "single quotes in session_id must be escaped: {script}"
        );
    }

    #[test]
    fn continue_script_escapes_tools() {
        let script = generate_continue_script(&ContinueScriptParams {
            effective_dir: "/tmp",
            prompt_file: Path::new("/tmp/p.txt"),
            session_id: "sess",
            tools: "Read,Write,it's-a-tool",
            model_id: "claude-sonnet-4-6",
            log_file: Path::new("/tmp/t.log"),
            notify_label: "t",
        });
        assert!(
            script.contains("it'\\''s-a-tool"),
            "single quotes in tools must be escaped: {script}"
        );
    }

    #[test]
    fn continue_script_uses_correct_model() {
        let script = generate_continue_script(&ContinueScriptParams {
            effective_dir: "/tmp",
            prompt_file: Path::new("/tmp/p.txt"),
            session_id: "sess",
            tools: "Read",
            model_id: "claude-opus-4-6",
            log_file: Path::new("/tmp/t.log"),
            notify_label: "t",
        });
        assert!(
            script.contains("--model claude-opus-4-6"),
            "script must use the provided model_id"
        );
    }

    #[test]
    fn continue_script_contains_cd_and_notification() {
        let script = generate_continue_script(&ContinueScriptParams {
            effective_dir: "/home/user/project",
            prompt_file: Path::new("/tmp/p.txt"),
            session_id: "sess",
            tools: "Read",
            model_id: "claude-sonnet-4-6",
            log_file: Path::new("/tmp/t.log"),
            notify_label: "my-label",
        });
        assert!(script.contains("cd '/home/user/project'"));
        assert!(script.contains("my-label"));
        assert!(script.contains("osascript"));
    }

    #[test]
    fn continue_script_escapes_notify_label_quotes() {
        let script = generate_continue_script(&ContinueScriptParams {
            effective_dir: "/tmp",
            prompt_file: Path::new("/tmp/p.txt"),
            session_id: "sess",
            tools: "Read",
            model_id: "claude-sonnet-4-6",
            log_file: Path::new("/tmp/t.log"),
            notify_label: "task \"with quotes\"",
        });
        assert!(
            script.contains(r#"task \"with quotes\""#),
            "double quotes in notify_label must be escaped: {script}"
        );
    }

    // --- prepare_continue tests ---

    #[test]
    fn continue_rejects_non_claude_runtimes() {
        for (runtime, label) in [
            (AgentRuntime::Codex, "codex"),
            (AgentRuntime::GeminiCli, "gemini-cli"),
            (AgentRuntime::QwenCode, "qwen-code"),
        ] {
            let mut task = make_continue_task("001");
            task.runtime = runtime;
            let result = prepare_continue(&task, "001", None);
            assert!(result.is_err(), "should reject {label}");
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("only Claude Code"),
                "error for {label} should mention Claude Code: {err}"
            );
        }
    }

    #[test]
    fn continue_rejects_empty_session_id() {
        let mut task = make_continue_task("002");
        task.session_id = String::new();
        let result = prepare_continue(&task, "002", None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no session_id"),
            "error should mention session_id: {err}"
        );
    }

    #[test]
    fn continue_uses_default_prompt() {
        let task = make_continue_task("003");
        let prepared = prepare_continue(&task, "003", None).unwrap();
        assert_eq!(prepared.follow_up, "Continue the task.");
    }

    #[test]
    fn continue_uses_custom_prompt() {
        let task = make_continue_task("004");
        let prepared = prepare_continue(&task, "004", Some("Fix the tests".to_string())).unwrap();
        assert_eq!(prepared.follow_up, "Fix the tests");
    }

    #[test]
    fn continue_infers_tools_when_empty() {
        let mut task = make_continue_task("005");
        task.task_type = "code".to_string();
        task.allowed_tools = String::new();
        let prepared = prepare_continue(&task, "005", None).unwrap();
        let expected = runner::tools_for_type("code", false);
        assert_eq!(prepared.tools, expected);
    }

    #[test]
    fn continue_preserves_explicit_tools() {
        let mut task = make_continue_task("006");
        task.allowed_tools = "Read,Grep".to_string();
        let prepared = prepare_continue(&task, "006", None).unwrap();
        assert_eq!(prepared.tools, "Read,Grep");
    }

    #[test]
    fn continue_uses_worktree_path_when_exists() {
        let tmp = tempfile::tempdir().unwrap();

        let mut task = make_continue_task("007");
        task.task_type = "code".to_string();
        task.working_dir = tmp.path().to_string_lossy().to_string();
        task.linear_issue_id = "RIG-999".to_string();

        // Create the exact directory that generate_branch_name would produce
        let branch = worktree::generate_branch_name(&task);
        let dir_name = branch.replace('/', "--");
        let trees_dir = tmp.path().join(".trees").join(&dir_name);
        std::fs::create_dir_all(&trees_dir).unwrap();

        let prepared = prepare_continue(&task, "007", None).unwrap();
        assert!(
            prepared.effective_dir.contains(".trees/"),
            "should resolve to worktree path: {}",
            prepared.effective_dir
        );
    }

    #[test]
    fn continue_falls_back_when_no_worktree() {
        let mut task = make_continue_task("008");
        task.task_type = "code".to_string();
        task.working_dir = "/tmp".to_string();
        task.linear_issue_id = "RIG-999".to_string();

        let prepared = prepare_continue(&task, "008", None).unwrap();
        assert_eq!(prepared.effective_dir, "/tmp");
    }

    #[test]
    fn continue_read_task_skips_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let trees_dir = tmp.path().join(".trees").join("some-branch");
        std::fs::create_dir_all(&trees_dir).unwrap();

        let mut task = make_continue_task("009");
        task.task_type = "research".to_string();
        task.working_dir = tmp.path().to_string_lossy().to_string();

        let prepared = prepare_continue(&task, "009", None).unwrap();
        assert_eq!(
            prepared.effective_dir,
            tmp.path().to_string_lossy().to_string(),
            "research tasks should not use worktree"
        );
    }

    #[test]
    fn continue_session_name_format() {
        let task = make_continue_task("20260331-042");
        let prepared = prepare_continue(&task, "20260331-042", None).unwrap();
        assert_eq!(prepared.session_name, "werma-20260331-042-cont");
    }

    #[test]
    fn continue_model_resolves_correctly() {
        let mut task = make_continue_task("010");
        task.model = "opus".to_string();
        let prepared = prepare_continue(&task, "010", None).unwrap();
        assert_eq!(prepared.model_id, runner::model_flag("opus"));
    }
}
