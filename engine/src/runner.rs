use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::models::{Status, Task};

/// Limits for context injection.
const MAX_CONTEXT_LINES: usize = 200;
const MAX_DEPENDENCY_OUTPUT_LINES: usize = 50;
const MAX_DEPENDENCY_OUTPUTS: usize = 5;

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
        "research" => "Read,Grep,Glob,WebSearch,WebFetch,Write".to_string(),
        "research-curator" => "Read,Grep,Glob".to_string(),
        "review" | "analyze" => "Read,Grep,Glob".to_string(),
        "code" | "refactor" => "Read,Edit,Write,Bash,Glob,Grep".to_string(),
        "full" => format!("Read,Edit,Write,Bash,Glob,Grep,{SLACK_READ},{SLACK_WRITE}"),
        "pipeline-analyst" => {
            const LINEAR_WRITE: &str = "mcp__plugin_linear_linear__save_issue";
            format!(
                "Read,Grep,Glob,Bash,WebSearch,WebFetch,{LINEAR_READ},{LINEAR_COMMENT},{LINEAR_LABEL},{LINEAR_WRITE}"
            )
        }
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

/// Build the full prompt with context files and dependency outputs prepended.
pub fn build_prompt(task: &Task, working_dir: &Path, werma_dir: &Path) -> Result<String> {
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
                let limited: String = content
                    .lines()
                    .take(MAX_CONTEXT_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                prompt.push_str(&format!(
                    "\n--- Context: {} ---\n{}\n--- End context ---\n",
                    resolved.display(),
                    limited
                ));
            }
        }
        prompt.push_str("\nTask:\n");
    }

    // Inject dependency outputs
    if !task.depends_on.is_empty() {
        let logs_dir = werma_dir.join("logs");
        let mut dep_count = 0;
        for dep_id in &task.depends_on {
            if dep_count >= MAX_DEPENDENCY_OUTPUTS {
                break;
            }
            let output_file = logs_dir.join(format!("{dep_id}-output.md"));
            if output_file.exists() {
                let content = std::fs::read_to_string(&output_file)?;
                let limited: String = content
                    .lines()
                    .take(MAX_DEPENDENCY_OUTPUT_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                prompt.push_str(&format!(
                    "\n--- Dependency output: {dep_id} ---\n{limited}\n--- End dependency ---\n"
                ));
                dep_count += 1;
            }
        }
    }

    // Auto-inject Linear issue description for non-pipeline tasks
    if !task.linear_issue_id.is_empty()
        && task.pipeline_stage.is_empty()
        && let Ok(client) = crate::linear::LinearClient::new()
    {
        match client.get_issue_by_identifier(&task.linear_issue_id) {
            Ok((_uuid, identifier, title, description, _labels)) => {
                if !description.is_empty() {
                    prompt.push_str(&format!(
                        "\n## Linear Issue: {identifier} — {title}\n\n{description}\n\n"
                    ));
                } else {
                    prompt.push_str(&format!("\n## Linear Issue: {identifier} — {title}\n\n"));
                }
            }
            Err(e) => {
                eprintln!(
                    "warning: could not fetch Linear issue {}: {e}",
                    task.linear_issue_id
                );
            }
        }
    }

    prompt.push_str(&task.prompt);

    // For write tasks, instruct agents to commit, push, and create PRs autonomously
    if crate::worktree::needs_worktree(&task.task_type) {
        prompt.push_str(concat!(
            "\n\nIMPORTANT — autonomous mode instructions:",
            "\nYou are running autonomously in a git worktree (feature branch).",
            "\nYou MUST complete the full cycle without waiting for human approval:",
            "\n1. Write the code changes",
            "\n2. Run `cargo test` (or equivalent) to verify",
            "\n3. Stage and commit changes with a descriptive message (conventional commits format)",
            "\n4. Push the branch to remote: `git push -u origin HEAD`",
            "\n5. Create a PR: `gh pr create --title \"RIG-XX type: description\" --body \"...\" --label ai-generated`",
            "\nDo NOT ask for permission to commit or push. Do NOT stop and wait for review.",
            "\nYou are in a worktree — commits here are safe and isolated from main.",
        ));
    }

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

    // Set up worktree for write tasks — gives each agent an isolated copy
    let effective_dir = if crate::worktree::needs_worktree(&task.task_type) {
        let branch = crate::worktree::generate_branch_name(task);
        let worktree_dir = crate::worktree::setup_worktree(&working_dir, &branch)?;

        // SAFETY GUARD: verify the worktree is actually inside .trees/
        // This prevents agents from accidentally working on the main checkout
        // if worktree setup silently falls back to the main repo directory.
        if !crate::worktree::is_inside_worktree(&worktree_dir) {
            bail!(
                "SAFETY: write task {} would run outside .trees/ (dir: {}). \
                 Aborting to prevent main branch contamination.",
                task_id,
                worktree_dir.display()
            );
        }

        worktree_dir
    } else {
        working_dir.clone()
    };

    let tools = if task.allowed_tools.is_empty() {
        tools_for_type(&task.task_type, !task.output_path.is_empty())
    } else {
        task.allowed_tools.clone()
    };

    let model = model_flag(&task.model);
    let fallback_model = resolve_fallback_model(task);
    let log_file = logs_dir.join(format!("{task_id}.log"));
    let prompt_file = logs_dir.join(format!("{task_id}-prompt.txt"));
    let exec_script = logs_dir.join(format!("{task_id}-exec.sh"));

    let full_prompt = build_prompt(task, &effective_dir, werma_dir)?;

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
        working_dir: &effective_dir,
        tools: &tools,
        max_turns: task.max_turns,
        model,
        fallback_model: fallback_model.as_deref(),
        log_file: &log_file,
        is_write_task: crate::worktree::needs_worktree(&task.task_type),
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

/// Resolve `~/` prefix to home directory. Public for use by daemon watchdog.
pub fn resolve_home_pub(path: &str) -> PathBuf {
    resolve_home(path)
}

fn resolve_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Resolve fallback model for a pipeline task by looking up the pipeline config.
fn resolve_fallback_model(task: &Task) -> Option<String> {
    if task.pipeline_stage.is_empty() {
        return None;
    }
    let config = crate::pipeline::loader::load_default().ok()?;
    let stage_cfg = config.stage(&task.pipeline_stage)?;
    stage_cfg.fallback.as_ref().map(|f| model_flag(f).to_string())
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
    fallback_model: Option<&'a str>,
    log_file: &'a Path,
    is_write_task: bool,
}

/// Generate a self-contained bash exec script for tmux.
/// Uses `werma complete`/`werma fail` for DB updates and pipeline callbacks.
fn generate_exec_script(params: &ExecScriptParams<'_>) -> String {
    let prompt_file_str = params.prompt_file.display();
    let working_dir_str = params.working_dir.display();
    let log_file_str = params.log_file.display();

    let task_id = params.task_id;
    let output = params.output;
    let tools = params.tools;
    let max_turns = params.max_turns;
    let model = params.model;
    let fallback_model = params.fallback_model.unwrap_or("");

    // For write tasks, add a bash guard that verifies cwd is inside .trees/
    let worktree_guard = if params.is_write_task {
        r#"
# SAFETY GUARD: write tasks must run inside a .trees/ worktree, never on main checkout
if [[ "$WORKING_DIR" != */.trees/* ]]; then
    echo "$(date): SAFETY ABORT — write task $TASK_ID would run outside .trees/ (dir: $WORKING_DIR)" >> "$LOG_FILE"
    werma fail "$TASK_ID"
    exit 1
fi
"#
    } else {
        ""
    };

    format!(
        r##"#!/bin/bash
set -euo pipefail
unset CLAUDECODE

TASK_ID='{task_id}'
PROMPT_FILE='{prompt_file_str}'
OUTPUT='{output}'
WORKING_DIR='{working_dir_str}'
ALLOWED_TOOLS='{tools}'
MAX_TURNS='{max_turns}'
MODEL='{model}'
FALLBACK_MODEL='{fallback_model}'
LOG_FILE='{log_file_str}'
RESULT_FILE="${{LOG_FILE%.log}}-output.md"

cd "$WORKING_DIR"
{worktree_guard}
PROMPT=$(cat "$PROMPT_FILE")

run_claude() {{
    local use_model="$1"
    claude -p "$PROMPT" \
        --allowedTools "$ALLOWED_TOOLS" \
        --max-turns "$MAX_TURNS" \
        --model "$use_model" \
        --output-format json 2>> "$LOG_FILE"
}}

RESULT_JSON=$(run_claude "$MODEL") || {{
    EXIT_CODE=$?
    # Check if fallback model is configured and error looks like a rate limit
    if [ -n "$FALLBACK_MODEL" ]; then
        LAST_LINES=$(tail -20 "$LOG_FILE" 2>/dev/null || echo "")
        if echo "$LAST_LINES" | grep -qiE "rate.?limit|overloaded|429|too many requests|quota|capacity"; then
            echo "$(date): primary model $MODEL rate-limited (exit $EXIT_CODE), retrying with fallback $FALLBACK_MODEL" >> "$LOG_FILE"
            RESULT_JSON=$(run_claude "$FALLBACK_MODEL") || {{
                echo "$(date): FAILED with fallback model (exit $?)" >> "$LOG_FILE"
                werma fail "$TASK_ID"
                exit 1
            }}
            MODEL="$FALLBACK_MODEL"
        else
            echo "$(date): FAILED (exit $EXIT_CODE, no rate-limit match)" >> "$LOG_FILE"
            werma fail "$TASK_ID"
            exit 1
        fi
    else
        echo "$(date): FAILED (exit $EXIT_CODE)" >> "$LOG_FILE"
        werma fail "$TASK_ID"
        exit 1
    fi
}}

# Strategy 1: extract .result from JSON
RESULT_TEXT=$(echo "$RESULT_JSON" | jq -r '.result // empty' 2>/dev/null)

# Strategy 2: if .result is empty/null, try alternative fields
if [ -z "$RESULT_TEXT" ]; then
    RESULT_TEXT=$(echo "$RESULT_JSON" | jq -r '.content // .message // .text // empty' 2>/dev/null)
fi

# Strategy 3: if still empty, use raw JSON (better than losing everything)
if [ -z "$RESULT_TEXT" ]; then
    RESULT_TEXT="$RESULT_JSON"
fi

SESSION_ID=$(echo "$RESULT_JSON" | jq -r '.session_id // empty' 2>/dev/null || echo "")

# Guard: if truly empty (claude returned nothing), log and fail
if [ -z "$(echo "$RESULT_TEXT" | tr -d '[:space:]')" ]; then
    echo "$(date): EMPTY OUTPUT — claude returned no parseable result" >> "$LOG_FILE"
    werma fail "$TASK_ID"
    exit 1
fi

# Always save output to logs
echo "$RESULT_TEXT" > "$RESULT_FILE"

# Also write to custom output path if specified
if [ -n "$OUTPUT" ]; then
    mkdir -p "$(dirname "$OUTPUT")"
    echo "$RESULT_TEXT" > "$OUTPUT"
fi

werma complete "$TASK_ID" --session "$SESSION_ID" --result-file "$RESULT_FILE"

echo "$(date): DONE (session=$SESSION_ID)" >> "$LOG_FILE"
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
        assert!(tools.contains("Write"));
        assert!(!tools.contains("Edit"));
        assert!(!tools.contains("Bash"));
    }

    #[test]
    fn tools_for_research_curator() {
        let tools = tools_for_type("research-curator", false);
        assert!(tools.contains("Read"));
        assert!(tools.contains("Grep"));
        assert!(tools.contains("Glob"));
        assert!(!tools.contains("Write"));
        assert!(!tools.contains("WebSearch"));
        assert!(!tools.contains("Bash"));
    }

    #[test]
    fn tools_for_code() {
        let tools = tools_for_type("code", false);
        assert!(tools.contains("Edit"));
        assert!(tools.contains("Write"));
        assert!(tools.contains("Bash"));
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
        let tools = tools_for_type("review", true);
        assert!(tools.contains("Write"));
    }

    #[test]
    fn tools_no_duplicate_write() {
        // research already has Write, adding output should not duplicate it
        let tools = tools_for_type("research", true);
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
            repo_hash: String::new(),
            estimate: 0,
        };

        let result = build_prompt(&task, Path::new("/tmp"), Path::new("/tmp/.werma")).unwrap();
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
            repo_hash: String::new(),
            estimate: 0,
        };

        let result = build_prompt(&task, dir.path(), dir.path()).unwrap();
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
            repo_hash: String::new(),
            estimate: 0,
        };

        let result = build_prompt(&task, Path::new("/tmp"), Path::new("/tmp/.werma")).unwrap();
        // Missing files are skipped, but header is still there
        assert!(result.contains("Task:\nDo stuff"));
    }

    #[test]
    fn build_prompt_with_dependency_output() {
        let werma_dir = tempfile::tempdir().unwrap();
        let logs_dir = werma_dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(
            logs_dir.join("dep-001-output.md"),
            "Analyst found: use pattern X\nKey insight here",
        )
        .unwrap();

        let task = Task {
            id: "test-004".to_string(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: "code".to_string(),
            prompt: "Implement feature".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec!["dep-001".to_string()],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };

        let result = build_prompt(&task, Path::new("/tmp"), werma_dir.path()).unwrap();
        assert!(result.contains("--- Dependency output: dep-001 ---"));
        assert!(result.contains("Analyst found: use pattern X"));
        assert!(result.contains("--- End dependency ---"));
        assert!(result.contains("Implement feature"));
    }

    #[test]
    fn build_prompt_dependency_output_truncated() {
        let werma_dir = tempfile::tempdir().unwrap();
        let logs_dir = werma_dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        // Create output with 100 lines — should be truncated to 50
        let long_output: String = (0..100)
            .map(|i| format!("Line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(logs_dir.join("dep-002-output.md"), &long_output).unwrap();

        let task = Task {
            id: "test-005".to_string(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: "code".to_string(),
            prompt: "Do work".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec!["dep-002".to_string()],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };

        let result = build_prompt(&task, Path::new("/tmp"), werma_dir.path()).unwrap();
        assert!(result.contains("Line 0"));
        assert!(result.contains("Line 49"));
        assert!(!result.contains("Line 50"));
    }

    #[test]
    fn build_prompt_missing_dependency_output() {
        let werma_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(werma_dir.path().join("logs")).unwrap();

        let task = Task {
            id: "test-006".to_string(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: "code".to_string(),
            prompt: "Do work".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec!["nonexistent-dep".to_string()],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };

        let result = build_prompt(&task, Path::new("/tmp"), werma_dir.path()).unwrap();
        // Missing dependency output is silently skipped
        assert!(!result.contains("Dependency output"));
        assert!(result.starts_with("Do work"));
    }

    #[test]
    fn exec_script_always_saves_output_to_logs() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260309-001",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/home/user/project"),
            tools: "Read,Grep,Glob",
            max_turns: 15,
            model: "claude-sonnet-4-6",
            fallback_model: None,
            log_file: Path::new("/home/user/.werma/logs/20260309-001.log"),
            is_write_task: false,
        });

        assert!(script.contains(r#"> "$RESULT_FILE""#));
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
            fallback_model: None,
            log_file: Path::new("/tmp/log.log"),
            is_write_task: false,
        });

        assert!(script.contains("TASK_ID='20260308-001'"));
        assert!(script.contains("claude -p"));
        assert!(script.contains("werma complete"));
        assert!(script.contains("werma fail"));
        assert!(script.contains("unset CLAUDECODE"));
        assert!(script.contains("--output-format json"));
        assert!(script.contains("jq -r"));
        assert!(script.contains("--result-file"));
        // No more raw sqlite3 or osascript — handled by werma complete/fail
        assert!(!script.contains("sqlite3"));
        assert!(!script.contains("osascript"));
    }

    #[test]
    fn exec_script_multi_strategy_output_extraction() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260312-001",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read",
            max_turns: 10,
            model: "claude-sonnet-4-6",
            fallback_model: None,
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
        });

        // Strategy 1: .result extraction
        assert!(script.contains(".result // empty"));
        // Strategy 2: alternative fields
        assert!(script.contains(".content // .message // .text // empty"));
        // Strategy 3: raw JSON fallback
        assert!(script.contains("RESULT_TEXT=\"$RESULT_JSON\""));
        // Guard: empty output detection and fail
        assert!(script.contains("EMPTY OUTPUT"));
        assert!(script.contains("werma fail \"$TASK_ID\""));
    }

    #[test]
    fn exec_script_write_task_has_worktree_guard() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260312-002",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/home/user/project/.trees/feat--RIG-177"),
            tools: "Read,Edit,Write,Bash",
            max_turns: 15,
            model: "claude-sonnet-4-6",
            fallback_model: None,
            log_file: Path::new("/tmp/test.log"),
            is_write_task: true,
        });

        assert!(script.contains("SAFETY ABORT"));
        assert!(script.contains(".trees/"));
    }

    #[test]
    fn exec_script_with_fallback_model() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260312-004",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read,Edit,Write,Bash",
            max_turns: 15,
            model: "claude-opus-4-6",
            fallback_model: Some("claude-sonnet-4-6"),
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
        });

        assert!(script.contains("FALLBACK_MODEL='claude-sonnet-4-6'"));
        assert!(script.contains("run_claude"));
        assert!(script.contains("rate.?limit|overloaded|429|too many requests|quota|capacity"));
        assert!(script.contains("retrying with fallback"));
    }

    #[test]
    fn exec_script_without_fallback_no_retry() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260312-005",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read",
            max_turns: 10,
            model: "claude-sonnet-4-6",
            fallback_model: None,
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
        });

        assert!(script.contains("FALLBACK_MODEL=''"));
    }

    #[test]
    fn exec_script_read_task_no_worktree_guard() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260312-003",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/home/user/project"),
            tools: "Read,Grep,Glob",
            max_turns: 15,
            model: "claude-sonnet-4-6",
            fallback_model: None,
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
        });

        assert!(!script.contains("SAFETY ABORT"));
    }
}
