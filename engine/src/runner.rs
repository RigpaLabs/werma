use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::models::{Status, Task};

/// Map task type to allowed tools (matching aq bash patterns).
pub fn tools_for_type(task_type: &str, has_output: bool) -> String {
    const LINEAR_READ: &str = "mcp__plugin_linear_linear__get_issue,\
        mcp__plugin_linear_linear__list_issues,\
        mcp__plugin_linear_linear__list_comments";
    const LINEAR_COMMENT: &str = "mcp__plugin_linear_linear__save_comment";
    const LINEAR_LABEL: &str = "mcp__plugin_linear_linear__list_issue_labels,\
        mcp__plugin_linear_linear__create_issue_label";
    const SLACK_READ: &str = "mcp__plugin_slack_slack__slack_read_channel,\
        mcp__plugin_slack_slack__slack_read_thread,\
        mcp__plugin_slack_slack__slack_search_channels";
    const SLACK_WRITE: &str = "mcp__plugin_slack_slack__slack_send_message";

    let mut tools = match task_type {
        "research" => "Read,Grep,Glob,WebSearch,WebFetch".to_string(),
        "review" | "analyze" => "Read,Grep,Glob".to_string(),
        "code" | "refactor" => "Read,Edit,Write,Glob,Grep".to_string(),
        "full" => format!("Read,Edit,Write,Bash,Glob,Grep,{SLACK_READ},{SLACK_WRITE}"),
        "pipeline-analyst" => format!(
            "Read,Grep,Glob,Bash,WebSearch,WebFetch,{LINEAR_READ},{LINEAR_COMMENT},{LINEAR_LABEL}"
        ),
        "pipeline-engineer" => {
            format!("Read,Edit,Write,Bash,Glob,Grep,{LINEAR_READ},{LINEAR_COMMENT}")
        }
        "pipeline-reviewer" | "pipeline-qa" | "pipeline-devops" => {
            format!("Read,Glob,Grep,Bash,{LINEAR_READ},{LINEAR_COMMENT}")
        }
        _ => "Read,Grep,Glob,WebSearch,WebFetch".to_string(),
    };

    if has_output && !tools.contains("Write") {
        tools.push_str(",Write");
    }
    tools
}

/// Map model name to claude CLI model ID.
pub fn model_flag(model: &str) -> &str {
    match model {
        "opus" => "claude-opus-4-6",
        "haiku" => "claude-haiku-4-5",
        _ => "claude-sonnet-4-6",
    }
}

/// Build the full prompt with context files prepended.
pub fn build_prompt(task: &Task, working_dir: &Path) -> Result<String> {
    let mut prompt = String::new();

    if !task.context_files.is_empty() {
        prompt.push_str("Use the following context files for reference:\n");
        for ctx_path in &task.context_files {
            let resolved = if ctx_path.starts_with('/') {
                PathBuf::from(ctx_path)
            } else if ctx_path.starts_with('~') {
                let home = dirs::home_dir().context("no home dir")?;
                home.join(ctx_path.trim_start_matches("~/"))
            } else {
                working_dir.join(ctx_path)
            };

            if resolved.exists() {
                let content = std::fs::read_to_string(&resolved)
                    .with_context(|| format!("reading context file: {}", resolved.display()))?;
                let limited: String = content.lines().take(200).collect::<Vec<_>>().join("\n");
                prompt.push_str(&format!(
                    "\n--- Context: {} ---\n{}\n--- End context ---\n",
                    resolved.display(),
                    limited
                ));
            }
        }
        prompt.push_str("\nTask:\n");
    }

    prompt.push_str(&task.prompt);
    Ok(prompt)
}

/// Run the next pending task in a tmux session.
/// Uses claim_next_pending for atomic find+mark-running (no TOCTOU).
/// Returns the task ID if launched, None if no tasks available.
pub fn run_next(db: &Db, werma_dir: &Path) -> Result<Option<String>> {
    // claim_next_pending atomically finds and marks as running
    let task = match db.claim_next_pending()? {
        Some(t) => t,
        None => return Ok(None),
    };

    run_task(db, &task, werma_dir)
}

/// Run all pending tasks in waves (dependency-aware).
/// Launches all launchable tasks, waits for them to finish, repeats.
pub fn run_all(db: &Db, werma_dir: &Path) -> Result<()> {
    loop {
        let launchable = db.find_all_launchable()?;
        if launchable.is_empty() {
            // Check if there are still running tasks we should wait for
            let (_, running, _, _) = db.task_counts()?;
            if running > 0 {
                wait_for_running(db)?;
                continue;
            }
            break;
        }

        let mut launched = Vec::new();
        for task in &launchable {
            match run_task(db, task, werma_dir) {
                Ok(Some(id)) => launched.push(id),
                Ok(None) => {}
                Err(e) => eprintln!("error launching {}: {e}", task.id),
            }
        }

        if launched.is_empty() {
            break;
        }

        wait_for_running(db)?;
    }

    let (p, r, c, f) = db.task_counts()?;
    println!(
        "\nrun-all complete: {} pending, {} running, {} completed, {} failed",
        p, r, c, f
    );
    Ok(())
}

/// Poll every 5 seconds until no running tasks remain (check tmux sessions).
fn wait_for_running(db: &Db) -> Result<()> {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));

        let running = db.list_tasks(Some(Status::Running))?;
        if running.is_empty() {
            break;
        }

        let mut any_alive = false;
        for task in &running {
            let session_name = format!("werma-{}", task.id);
            let result = Command::new("tmux")
                .args(["has-session", "-t", &session_name])
                .output();

            match result {
                Ok(out) if out.status.success() => {
                    any_alive = true;
                }
                _ => {
                    // Session gone but status still running — mark as failed
                    // (the exec script should have updated it, but just in case)
                    let task_status = db.task(&task.id)?;
                    if let Some(t) = task_status
                        && t.status == Status::Running
                    {
                        eprintln!("tmux session gone for {}, marking failed", task.id);
                        db.set_task_status(&task.id, Status::Failed)?;
                        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                        db.update_task_field(&task.id, "finished_at", &now)?;
                    }
                }
            }
        }

        if !any_alive {
            break;
        }
    }
    Ok(())
}

/// Run a specific task in a tmux session.
pub fn run_task(db: &Db, task: &Task, werma_dir: &Path) -> Result<Option<String>> {
    let task_id = &task.id;
    let logs_dir = werma_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;

    let working_dir = resolve_home(&task.working_dir);

    let tools = if task.allowed_tools.is_empty() {
        tools_for_type(&task.task_type, !task.output_path.is_empty())
    } else {
        task.allowed_tools.clone()
    };

    let model = model_flag(&task.model);
    let log_file = logs_dir.join(format!("{task_id}.log"));
    let prompt_file = logs_dir.join(format!("{task_id}-prompt.txt"));
    let exec_script = logs_dir.join(format!("{task_id}-exec.sh"));

    let full_prompt = build_prompt(task, &working_dir)?;

    // Write prompt to file — never interpolated into shell
    std::fs::write(&prompt_file, &full_prompt)?;

    let output = if task.output_path.is_empty() {
        String::new()
    } else {
        resolve_home(&task.output_path)
            .to_string_lossy()
            .to_string()
    };

    let script = generate_exec_script(&ExecScriptParams {
        task_id,
        prompt_file: &prompt_file,
        output: &output,
        working_dir: &working_dir,
        tools: &tools,
        max_turns: task.max_turns,
        model,
        log_file: &log_file,
    });

    std::fs::write(&exec_script, &script)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exec_script, std::fs::Permissions::from_mode(0o755))?;
    }

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    // Mark as running (if not already claimed by claim_next_pending)
    if task.status != Status::Running {
        db.set_task_status(task_id, Status::Running)?;
        db.update_task_field(task_id, "started_at", &now)?;
    }

    // Log launch
    let log_entry = format!(
        "{now}: task={task_id} type={} model={} turns={} tools={tools}\n",
        task.task_type, task.model, task.max_turns
    );
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)?
        .write_all(log_entry.as_bytes())?;

    // Launch tmux session
    let session_name = format!("werma-{task_id}");
    let tmux_cmd = format!("bash {}", exec_script.display());
    let tmux_result = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session_name, &tmux_cmd])
        .status();

    match tmux_result {
        Ok(status) if status.success() => {
            println!("{now}: launched in tmux: {session_name}");
            Ok(Some(task_id.clone()))
        }
        Ok(status) => bail!("tmux exited with {status}"),
        Err(e) => bail!("failed to spawn tmux: {e}"),
    }
}

fn resolve_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Parameters for exec script generation — avoids too-many-arguments.
struct ExecScriptParams<'a> {
    task_id: &'a str,
    prompt_file: &'a Path,
    output: &'a str,
    working_dir: &'a Path,
    tools: &'a str,
    max_turns: i32,
    model: &'a str,
    log_file: &'a Path,
}

/// Generate a self-contained bash exec script for tmux.
/// Uses sqlite3 CLI for DB updates (no circular dependency on werma binary).
fn generate_exec_script(params: &ExecScriptParams<'_>) -> String {
    let prompt_file_str = params.prompt_file.display();
    let working_dir_str = params.working_dir.display();
    let log_file_str = params.log_file.display();

    let task_id = params.task_id;
    let output = params.output;
    let tools = params.tools;
    let max_turns = params.max_turns;
    let model = params.model;

    // Escape single quotes in task_id for SQL safety
    let safe_id = task_id.replace('\'', "''");

    format!(
        r##"#!/bin/bash
set -euo pipefail
unset CLAUDECODE

TASK_ID='{safe_id}'
WERMA_DB="$HOME/.werma/werma.db"
PROMPT_FILE='{prompt_file_str}'
OUTPUT='{output}'
WORKING_DIR='{working_dir_str}'
ALLOWED_TOOLS='{tools}'
MAX_TURNS='{max_turns}'
MODEL='{model}'
LOG_FILE='{log_file_str}'

cd "$WORKING_DIR"

PROMPT=$(cat "$PROMPT_FILE")

RESULT_JSON=$(claude -p "$PROMPT" \
    --allowedTools "$ALLOWED_TOOLS" \
    --max-turns "$MAX_TURNS" \
    --model "$MODEL" \
    --output-format json 2>> "$LOG_FILE") || {{
    echo "$(date): FAILED (exit $?)" >> "$LOG_FILE"
    sqlite3 "$WERMA_DB" "UPDATE tasks SET status='failed', finished_at='$(date +%Y-%m-%dT%H:%M:%S)' WHERE id='$TASK_ID';"
    osascript -e "display notification \"$TASK_ID FAILED\" with title \"werma\" sound name \"Basso\"" 2>/dev/null || true
    exit 1
}}

RESULT_TEXT=$(echo "$RESULT_JSON" | jq -r '.result // empty' 2>/dev/null || echo "$RESULT_JSON")
SESSION_ID=$(echo "$RESULT_JSON" | jq -r '.session_id // empty' 2>/dev/null || echo "")

# Always save output to logs
echo "$RESULT_TEXT" > "${{LOG_FILE%.log}}-output.md"

# Also write to custom output path if specified
if [ -n "$OUTPUT" ]; then
    mkdir -p "$(dirname "$OUTPUT")"
    echo "$RESULT_TEXT" > "$OUTPUT"
fi

sqlite3 "$WERMA_DB" "UPDATE tasks SET status='completed', finished_at='$(date +%Y-%m-%dT%H:%M:%S)', session_id='$(echo "$SESSION_ID" | sed "s/'/''/g")' WHERE id='$TASK_ID';"

echo "$(date): DONE (session=$SESSION_ID)" >> "$LOG_FILE"

osascript -e "display notification \"$TASK_ID done\" with title \"werma\" sound name \"Glass\"" 2>/dev/null || true
"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_for_research() {
        let tools = tools_for_type("research", false);
        assert!(tools.contains("Read"));
        assert!(tools.contains("Grep"));
        assert!(tools.contains("WebSearch"));
        assert!(!tools.contains("Edit"));
        assert!(!tools.contains("Bash"));
    }

    #[test]
    fn tools_for_code() {
        let tools = tools_for_type("code", false);
        assert!(tools.contains("Edit"));
        assert!(tools.contains("Write"));
        assert!(!tools.contains("Bash"));
    }

    #[test]
    fn tools_for_full() {
        let tools = tools_for_type("full", false);
        assert!(tools.contains("Bash"));
        assert!(tools.contains("Edit"));
        assert!(tools.contains("slack"));
    }

    #[test]
    fn tools_adds_write_for_output() {
        let tools = tools_for_type("research", true);
        assert!(tools.contains("Write"));
    }

    #[test]
    fn tools_no_duplicate_write() {
        let tools = tools_for_type("code", true);
        // Write already present, should not be duplicated
        let count = tools.matches("Write").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn tools_for_pipeline_types() {
        let analyst = tools_for_type("pipeline-analyst", false);
        assert!(analyst.contains("WebSearch"));
        assert!(analyst.contains("linear"));

        let engineer = tools_for_type("pipeline-engineer", false);
        assert!(engineer.contains("Edit"));
        assert!(engineer.contains("linear"));

        let reviewer = tools_for_type("pipeline-reviewer", false);
        assert!(reviewer.contains("linear"));
        assert!(!reviewer.contains("Edit"));
    }

    #[test]
    fn tools_for_unknown() {
        let tools = tools_for_type("something-random", false);
        assert!(tools.contains("Read"));
        assert!(tools.contains("WebSearch"));
    }

    #[test]
    fn model_flag_mapping() {
        assert_eq!(model_flag("opus"), "claude-opus-4-6");
        assert_eq!(model_flag("haiku"), "claude-haiku-4-5");
        assert_eq!(model_flag("sonnet"), "claude-sonnet-4-6");
        assert_eq!(model_flag("anything"), "claude-sonnet-4-6");
    }

    #[test]
    fn build_prompt_no_context() {
        let task = Task {
            id: "test-001".to_string(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: "research".to_string(),
            prompt: "Do something".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec![],
        };

        let result = build_prompt(&task, Path::new("/tmp")).unwrap();
        assert_eq!(result, "Do something");
    }

    #[test]
    fn build_prompt_with_context_file() {
        let dir = tempfile::tempdir().unwrap();
        let ctx_file = dir.path().join("ctx.txt");
        std::fs::write(&ctx_file, "context content here").unwrap();

        let task = Task {
            id: "test-002".to_string(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: "research".to_string(),
            prompt: "Do the thing".to_string(),
            output_path: String::new(),
            working_dir: dir.path().to_string_lossy().to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec!["ctx.txt".to_string()],
        };

        let result = build_prompt(&task, dir.path()).unwrap();
        assert!(result.contains("Use the following context files for reference:"));
        assert!(result.contains("context content here"));
        assert!(result.contains("Task:\nDo the thing"));
    }

    #[test]
    fn build_prompt_missing_context_file() {
        let task = Task {
            id: "test-003".to_string(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: "research".to_string(),
            prompt: "Do stuff".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec!["/nonexistent/file.txt".to_string()],
        };

        let result = build_prompt(&task, Path::new("/tmp")).unwrap();
        // Missing files are skipped, but header is still there
        assert!(result.contains("Task:\nDo stuff"));
    }

    #[test]
    fn exec_script_always_saves_output_to_logs() {
        // Even without --output, RESULT_TEXT should be saved to <id>-output.md
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260309-001",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "", // no custom output
            working_dir: Path::new("/home/user/project"),
            tools: "Read,Grep,Glob",
            max_turns: 15,
            model: "claude-sonnet-4-6",
            log_file: Path::new("/home/user/.werma/logs/20260309-001.log"),
        });

        // Must always write to the log-derived output path
        assert!(script.contains(r#"> "${LOG_FILE%.log}-output.md""#));
        // OUTPUT is empty, so the custom output block should still exist but not trigger
        assert!(script.contains(r#"if [ -n "$OUTPUT" ]"#));
    }

    #[test]
    fn exec_script_contains_key_elements() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260308-001",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "/tmp/output.md",
            working_dir: Path::new("/home/user/project"),
            tools: "Read,Grep,Glob",
            max_turns: 15,
            model: "claude-sonnet-4-6",
            log_file: Path::new("/tmp/log.log"),
        });

        assert!(script.contains("TASK_ID='20260308-001'"));
        assert!(script.contains("WERMA_DB="));
        assert!(script.contains("claude -p"));
        assert!(script.contains("sqlite3"));
        assert!(script.contains("unset CLAUDECODE"));
        assert!(script.contains("--output-format json"));
        assert!(script.contains("jq -r"));
        assert!(script.contains("osascript"));
    }
}
