use anyhow::{Context, Result, bail};

use crate::db::Db;
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
