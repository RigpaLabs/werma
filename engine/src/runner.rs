use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::models::{AgentRuntime, Status, Task};

/// Limits for context injection.
const MAX_CONTEXT_LINES: usize = 200;
const MAX_DEPENDENCY_OUTPUT_LINES: usize = 50;
const MAX_DEPENDENCY_OUTPUTS: usize = 5;

/// Map task type to allowed tools (matching aq bash patterns).
pub fn tools_for_type(task_type: &str, has_output: bool) -> String {
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
        "pipeline-analyst" => "Read,Grep,Glob,WebSearch,WebFetch".to_string(),
        "pipeline-engineer" => "Read,Edit,Write,Bash,Glob,Grep,Skill".to_string(),
        "pipeline-reviewer" | "pipeline-qa" | "pipeline-devops" | "pipeline-deployer" => {
            "Read,Glob,Grep,Bash,Skill".to_string()
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

/// Resolve model for Codex runtime.
/// Claude shorthands (opus/sonnet/haiku) are not valid Codex models — return empty
/// to let Codex CLI use its own default. Explicit Codex model IDs are passed through.
pub fn codex_model(model: &str) -> &str {
    match model {
        "opus" | "sonnet" | "haiku" => "",
        other => other,
    }
}

/// Resolve the model string for a task, taking runtime into account.
fn resolve_model(model: &str, runtime: AgentRuntime) -> &str {
    match runtime {
        AgentRuntime::Codex => codex_model(model),
        AgentRuntime::ClaudeCode => model_flag(model),
    }
}

/// Build the full prompt with context files and dependency outputs prepended.
pub fn build_prompt(task: &Task, working_dir: &Path, werma_dir: &Path) -> Result<String> {
    let mut prompt = String::new();

    // Inject pipeline handoff content from DB column (set by decide_callback).
    // Backward compatible: old tasks have handoff_content = "" and fall through to context_files.
    if !task.handoff_content.is_empty() {
        prompt.push_str("\n--- Pipeline Handoff ---\n");
        prompt.push_str(&task.handoff_content);
        prompt.push_str("\n--- End Handoff ---\n\n");
    }

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

    // Inject issue context for tasks that have a Linear identifier.
    // GitHub identifiers (owner/repo#N) are skipped — they cannot be resolved via the Linear API.
    if !task.linear_issue_id.is_empty()
        && crate::linear::is_linear_identifier(&task.linear_issue_id)
        && let Ok(client) = crate::linear::LinearClient::new()
    {
        match client.get_issue_by_identifier(&task.linear_issue_id) {
            Ok((_uuid, identifier, title, description, labels)) => {
                prompt.push_str("\n---ISSUE---\n");
                prompt.push_str(&format!("ID: {identifier}\n"));
                prompt.push_str(&format!("Title: {title}\n"));
                if !description.is_empty() {
                    prompt.push_str(&format!("Description:\n{description}\n"));
                }
                if !labels.is_empty() {
                    prompt.push_str(&format!("Labels: {}\n", labels.join(", ")));
                }
                prompt.push_str("---END ISSUE---\n\n");
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

    // For write tasks, instruct agents to commit and push autonomously.
    // RIG-281: agents must NOT call `gh pr create/merge/comment` — the engine handles all
    // GitHub mutations via callbacks (auto_create_pr, post_pr_comment).
    if crate::worktree::needs_worktree(&task.task_type) {
        prompt.push_str(concat!(
            "\n\nIMPORTANT — autonomous mode instructions:",
            "\nYou are running autonomously in a git worktree (feature branch).",
            "\nYou MUST complete the full cycle without waiting for human approval:",
            "\n1. Write the code changes",
            "\n2. Run `cargo test` (or equivalent) to verify",
            "\n3. Stage and commit changes with a descriptive message (conventional commits format)",
            "\n4. Push the branch to remote: `git push -u origin HEAD`",
            "\nDo NOT ask for permission to commit or push. Do NOT stop and wait for review.",
            "\nYou are in a worktree — commits here are safe and isolated from main.",
            "\n",
            "\nIMPORTANT: Do NOT call `gh pr create`, `gh pr merge`, `gh pr comment`, or any ",
            "other `gh` write commands. The pipeline engine handles all GitHub mutations ",
            "(PR creation, commenting, merging) automatically after your task completes.",
        ));
    }

    Ok(prompt)
}

/// Convert a naive local timestamp (written by `chrono::Local::now()`) to UTC RFC 3339.
///
/// All `finished_at` timestamps in the DB are stored as naive local time
/// (e.g. "2026-03-24T16:00:00" in WITA = UTC+8). Linear comment timestamps
/// are RFC 3339 UTC (e.g. "2026-03-24T08:00:00.000Z"). Without conversion,
/// `is_after_timestamp` would treat the naive value as UTC, creating an
/// 8-hour window where valid comments are incorrectly excluded.
fn naive_local_to_utc_iso(local_ts: &str) -> String {
    use chrono::{Local, NaiveDateTime};
    NaiveDateTime::parse_from_str(local_ts, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .and_then(|ndt| ndt.and_local_timezone(Local).single())
        .map(|dt| dt.to_utc().to_rfc3339())
        .unwrap_or_else(|| local_ts.to_string())
}

/// Fetch Linear comments for an issue at execution time.
///
/// Filters to comments posted after the previous pipeline stage completed,
/// skipping werma bot comments. Returns formatted markdown or empty string.
fn fetch_linear_comments(linear: &dyn crate::linear::LinearApi, db: &Db, task: &Task) -> String {
    // Find when the previous stage finished (to filter old comments).
    // Convert from local time (stored by chrono::Local::now) to UTC
    // so the comparison against Linear's UTC timestamps is correct.
    let after_iso = if !task.pipeline_stage.is_empty() {
        db.last_stage_finished_at(&task.linear_issue_id, &task.pipeline_stage)
            .ok()
            .flatten()
            .map(|ts| naive_local_to_utc_iso(&ts))
    } else {
        None
    };

    let comments = match linear.list_comments(&task.linear_issue_id, after_iso.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: could not fetch Linear comments for {}: {e}",
                task.linear_issue_id
            );
            return String::new();
        }
    };

    if comments.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Linear Comments (recent updates)\n\n");
    for (author, created_at, body) in &comments {
        // Truncate timestamp to date+time for readability
        let ts = created_at.get(..19).unwrap_or(created_at);
        out.push_str(&format!("**{author}** ({ts}):\n{body}\n\n---\n\n"));
    }
    // Sanitize escaped newlines/tabs that may come from Linear comment bodies
    out.replace("\\n", "\n").replace("\\t", "\t")
}

/// Fetch child (sub) issues for a parent issue at execution time.
/// Returns formatted markdown listing all sub-issues, or empty string if none.
fn fetch_sub_issues(linear: &dyn crate::linear::LinearApi, identifier: &str) -> String {
    let children = match linear.get_sub_issues(identifier) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not fetch sub-issues for {identifier}: {e}");
            return String::new();
        }
    };

    if children.is_empty() {
        return String::new();
    }

    let mut out = format!(
        "## Sub-issues ({count} children)\n\nThis is an **epic/parent issue**. Analyze all sub-issues holistically.\n\n",
        count = children.len()
    );
    for (ident, title, status, description) in &children {
        out.push_str(&format!("### [{ident}] {title}\n**Status:** {status}\n"));
        if !description.is_empty() {
            // Sanitize escaped newlines from Linear
            let desc = description.replace("\\n", "\n").replace("\\t", "\t");
            out.push_str(&format!("\n{desc}\n"));
        }
        out.push('\n');
    }
    out
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
    println!("\nrun-all complete: {p} pending, {r} running, {c} completed, {f} failed");
    Ok(())
}

/// Poll every 5 seconds until no running tasks remain (check tmux sessions).
fn wait_for_running(db: &Db) -> Result<()> {
    let spinner = crate::ui::waiting_spinner("Waiting for running tasks...");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));

        let running = db.list_tasks(Some(Status::Running))?;
        if running.is_empty() {
            break;
        }

        spinner.set_message(format!("Waiting for {} running task(s)...", running.len()));

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

    spinner.finish_and_clear();
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

        // RIG-372: If working_dir already points inside a worktree (e.g. from a previous
        // failed task that persisted the worktree path via RIG-351), resolve back to the
        // base repo to prevent nested .trees/.trees/ paths.
        let base_dir = crate::worktree::resolve_base_repo(&working_dir);

        let worktree_dir = crate::worktree::setup_worktree(&base_dir, &branch)?;

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

        // RIG-351: persist worktree path so callbacks (CreatePr, PostPrComment)
        // run from the correct directory instead of the base repo checkout.
        let wt_str = worktree_dir.to_string_lossy();
        db.update_task_field(task_id, "working_dir", &wt_str)?;

        worktree_dir
    } else {
        working_dir.clone()
    };

    let tools = if task.allowed_tools.is_empty() {
        tools_for_type(&task.task_type, !task.output_path.is_empty())
    } else {
        task.allowed_tools.clone()
    };

    let model = resolve_model(&task.model, task.runtime);
    let fallback_model = resolve_fallback_model(task);
    let log_file = logs_dir.join(format!("{task_id}.log"));
    let prompt_file = logs_dir.join(format!("{task_id}-prompt.txt"));
    let exec_script = logs_dir.join(format!("{task_id}-exec.sh"));

    let mut full_prompt = build_prompt(task, &effective_dir, werma_dir)?;

    // Late-inject Linear comments at execution time (not creation time)
    // so agents see context updates posted after task was created.
    if full_prompt.contains("{linear_comments}") {
        let comments_text = if task.linear_issue_id.is_empty()
            || !crate::linear::is_linear_identifier(&task.linear_issue_id)
        {
            String::new()
        } else if let Ok(client) = crate::linear::LinearClient::new() {
            fetch_linear_comments(&client, db, task)
        } else {
            eprintln!("warning: could not initialize Linear client, skipping comment fetch");
            String::new()
        };
        full_prompt = full_prompt.replace("{linear_comments}", &comments_text);
    }

    // Late-inject sub-issues for analyst stage (epic detection).
    // Fetches child issues from Linear so analyst can analyze epics holistically.
    if full_prompt.contains("{sub_issues}") {
        let sub_issues_text = if task.linear_issue_id.is_empty()
            || !crate::linear::is_linear_identifier(&task.linear_issue_id)
        {
            String::new()
        } else if let Ok(client) = crate::linear::LinearClient::new() {
            fetch_sub_issues(&client, &task.linear_issue_id)
        } else {
            String::new()
        };
        full_prompt = full_prompt.replace("{sub_issues}", &sub_issues_text);
    }

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
        runtime: task.runtime,
        task_type: &task.task_type,
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

/// Resolve `~/` prefix to home directory.
pub fn resolve_home(path: &str) -> PathBuf {
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
    stage_cfg
        .fallback
        .as_ref()
        .map(|f| model_flag(f).to_string())
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
    runtime: AgentRuntime,
    task_type: &'a str,
}

/// Generate a self-contained bash exec script for tmux.
/// Dispatches to Claude Code or Codex script generator based on runtime.
fn generate_exec_script(params: &ExecScriptParams<'_>) -> String {
    match params.runtime {
        AgentRuntime::ClaudeCode => generate_claude_exec_script(params),
        AgentRuntime::Codex => generate_codex_exec_script(params),
    }
}

/// Determine codex sandbox mode based on task type.
/// Read-only types get `read-only`, everything else gets `workspace-write`.
fn codex_sandbox_mode(task_type: &str) -> &'static str {
    match task_type {
        "pipeline-reviewer" | "pipeline-analyst" | "pipeline-qa" | "review" | "analyze" => {
            "read-only"
        }
        // research uses Write, so it's NOT read-only
        _ => "workspace-write",
    }
}

/// Generate a Codex CLI exec script for tmux.
fn generate_codex_exec_script(params: &ExecScriptParams<'_>) -> String {
    let prompt_file_str = params.prompt_file.display();
    let working_dir_str = params.working_dir.display();
    let log_file_str = params.log_file.display();

    let task_id = params.task_id;
    let output = params.output;
    let sandbox = codex_sandbox_mode(params.task_type);

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

    // Codex model flag: pass through the resolved model ID directly
    let model = params.model;

    // Codex needs --skip-git-repo-check when running from worktrees
    let skip_git_check = if params.is_write_task {
        " --skip-git-repo-check"
    } else {
        // Always check if we're in a git repo, skip if not
        ""
    };

    format!(
        r##"#!/bin/bash
set -euo pipefail

TASK_ID='{task_id}'
PROMPT_FILE='{prompt_file_str}'
OUTPUT='{output}'
WORKING_DIR='{working_dir_str}'
LOG_FILE='{log_file_str}'
RESULT_FILE="${{LOG_FILE%.log}}-output.md"

# Redirect all stderr to log from the start
exec 2>> "$LOG_FILE"

echo "$(date): EXEC_START task=$TASK_ID runtime=codex model={model} sandbox={sandbox}" >> "$LOG_FILE"

cd "$WORKING_DIR" || {{
    echo "$(date): FAILED — cd to $WORKING_DIR failed" >> "$LOG_FILE"
    werma fail "$TASK_ID"
    exit 1
}}
{worktree_guard}
PROMPT=$(cat "$PROMPT_FILE")

echo "$(date): CODEX_START pid=$$" >> "$LOG_FILE"

# Detect if we're in a git repo for --skip-git-repo-check
SKIP_GIT=""
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    SKIP_GIT="--skip-git-repo-check"
fi

MODEL_FLAG=""
if [ -n "{model}" ]; then
    MODEL_FLAG="--model {model}"
fi

codex exec \
    --sandbox {sandbox} \
    --full-auto \
    $MODEL_FLAG \
    -o "$RESULT_FILE" \
    $SKIP_GIT{skip_git_check} \
    "$PROMPT" || {{
    EXIT_CODE=$?
    echo "$(date): CODEX_EXIT code=$EXIT_CODE" >> "$LOG_FILE"
    echo "$(date): FAILED (exit $EXIT_CODE)" >> "$LOG_FILE"
    werma fail "$TASK_ID"
    exit 1
}}

# Codex writes output directly to -o file
if [ ! -f "$RESULT_FILE" ] || [ -z "$(tr -d '[:space:]' < "$RESULT_FILE")" ]; then
    echo "$(date): EMPTY OUTPUT — codex returned no output" >> "$LOG_FILE"
    werma fail "$TASK_ID"
    exit 1
fi

# Also write to custom output path if specified
if [ -n "$OUTPUT" ]; then
    mkdir -p "$(dirname "$OUTPUT")"
    cp "$RESULT_FILE" "$OUTPUT"
fi

# Codex does not provide session_id, cost, or turns — complete without them
werma complete "$TASK_ID" --result-file "$RESULT_FILE"

echo "$(date): DONE (runtime=codex)" >> "$LOG_FILE"
"##
    )
}

/// Generate a Claude Code exec script for tmux.
fn generate_claude_exec_script(params: &ExecScriptParams<'_>) -> String {
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

# Redirect all stderr to log from the start, so setup errors are captured
exec 2>> "$LOG_FILE"

echo "$(date): EXEC_START task=$TASK_ID model=$MODEL tools=$ALLOWED_TOOLS" >> "$LOG_FILE"

cd "$WORKING_DIR" || {{
    echo "$(date): FAILED — cd to $WORKING_DIR failed" >> "$LOG_FILE"
    werma fail "$TASK_ID"
    exit 1
}}
{worktree_guard}
PROMPT=$(cat "$PROMPT_FILE")

echo "$(date): CLAUDE_START pid=$$" >> "$LOG_FILE"

run_claude() {{
    local use_model="$1"
    claude -p "$PROMPT" \
        --allowedTools "$ALLOWED_TOOLS" \
        --max-turns "$MAX_TURNS" \
        --model "$use_model" \
        --output-format json
}}

is_rate_limit() {{
    # Check multiple sources for rate-limit indicators:
    # 1. stderr (redirected to LOG_FILE) — CLI error messages
    # 2. stdout (partial RESULT_JSON) — JSON error responses from API
    # 3. exit code 429 directly
    local exit_code="${{1:-0}}"
    local stdout_text="${{2:-}}"
    [ "$exit_code" = "429" ] && return 0
    local combined
    combined="$(tail -20 "$LOG_FILE" 2>/dev/null || echo "")
$stdout_text"
    echo "$combined" | grep -qiE "rate.?limit|429|too many requests|quota.?exceeded|server.?overloaded|api.?capacity|overloaded_error"
}}

RESULT_JSON=$(run_claude "$MODEL") || {{
    EXIT_CODE=$?
    echo "$(date): CLAUDE_EXIT code=$EXIT_CODE model=$MODEL" >> "$LOG_FILE"
    if [ -n "$FALLBACK_MODEL" ] && is_rate_limit "$EXIT_CODE" "$RESULT_JSON"; then
        echo "$(date): WARNING: $MODEL rate-limited, falling back to $FALLBACK_MODEL for task $TASK_ID" >> "$LOG_FILE"
        RESULT_JSON=$(run_claude "$FALLBACK_MODEL") || {{
            echo "$(date): FAILED — fallback model $FALLBACK_MODEL also failed (exit $?)" >> "$LOG_FILE"
            werma fail "$TASK_ID"
            exit 1
        }}
        MODEL="$FALLBACK_MODEL"
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

# RIG-291: Extract cost and turns from Claude JSON output
COST_USD=$(echo "$RESULT_JSON" | jq -r '.total_cost_usd // empty' 2>/dev/null || echo "")
NUM_TURNS=$(echo "$RESULT_JSON" | jq -r '.num_turns // empty' 2>/dev/null || echo "")

# RIG-252: Detect error_max_turns — agent ran out of turns without completing.
# Claude returns is_error=false for this, but the work is incomplete.
SUBTYPE=$(echo "$RESULT_JSON" | jq -r '.subtype // empty' 2>/dev/null || echo "")
if [ "$SUBTYPE" = "error_max_turns" ]; then
    echo "$(date): MAX_TURNS_EXIT — agent hit max_turns (subtype=$SUBTYPE), marking failed" >> "$LOG_FILE"
    # Still save output for inspection
    if [ -n "$RESULT_TEXT" ]; then
        echo "$RESULT_TEXT" > "$RESULT_FILE"
    fi
    werma fail "$TASK_ID"
    exit 1
fi

# RIG-299: Detect rate-limit errors in successful JSON responses (exit 0 but API error).
# Claude CLI may return exit 0 with an error JSON body on overload/rate-limit.
IS_ERROR=$(echo "$RESULT_JSON" | jq -r '.is_error // empty' 2>/dev/null || echo "")
if [ "$IS_ERROR" = "true" ] && is_rate_limit 0 "$RESULT_JSON"; then
    if [ -n "$FALLBACK_MODEL" ]; then
        echo "$(date): WARNING: $MODEL returned rate-limit error in JSON, falling back to $FALLBACK_MODEL for task $TASK_ID" >> "$LOG_FILE"
        RESULT_JSON=$(run_claude "$FALLBACK_MODEL") || {{
            echo "$(date): FAILED — fallback model $FALLBACK_MODEL also failed (exit $?)" >> "$LOG_FILE"
            werma fail "$TASK_ID"
            exit 1
        }}
        MODEL="$FALLBACK_MODEL"
        # Re-extract result fields from fallback response
        RESULT_TEXT=$(echo "$RESULT_JSON" | jq -r '.result // empty' 2>/dev/null)
        if [ -z "$RESULT_TEXT" ]; then
            RESULT_TEXT=$(echo "$RESULT_JSON" | jq -r '.content // .message // .text // empty' 2>/dev/null)
        fi
        if [ -z "$RESULT_TEXT" ]; then
            RESULT_TEXT="$RESULT_JSON"
        fi
        SESSION_ID=$(echo "$RESULT_JSON" | jq -r '.session_id // empty' 2>/dev/null || echo "")
    else
        echo "$(date): FAILED — rate-limit error in JSON, no fallback configured" >> "$LOG_FILE"
        werma fail "$TASK_ID"
        exit 1
    fi
fi

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

COMPLETE_ARGS="--session \"$SESSION_ID\" --result-file \"$RESULT_FILE\""
if [ -n "$COST_USD" ]; then
    COMPLETE_ARGS="$COMPLETE_ARGS --cost $COST_USD"
fi
if [ -n "$NUM_TURNS" ]; then
    COMPLETE_ARGS="$COMPLETE_ARGS --turns $NUM_TURNS"
fi
eval werma complete "$TASK_ID" $COMPLETE_ARGS

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
        assert!(
            !analyst.contains("linear"),
            "pipeline agents must not have Linear tools"
        );
        assert!(
            !analyst.contains("Bash"),
            "analyst is read-only — must not have Bash (can bypass Write restriction)"
        );
        assert!(
            !analyst.contains("Write"),
            "analyst is read-only — must not have Write"
        );
        assert!(
            !analyst.contains("Edit"),
            "analyst is read-only — must not have Edit"
        );

        let engineer = tools_for_type("pipeline-engineer", false);
        assert!(engineer.contains("Edit"));
        assert!(
            !engineer.contains("linear"),
            "pipeline agents must not have Linear tools"
        );

        let reviewer = tools_for_type("pipeline-reviewer", false);
        assert!(
            !reviewer.contains("linear"),
            "pipeline agents must not have Linear tools"
        );
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
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
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
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
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
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
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        assert!(script.contains("TASK_ID='20260308-001'"));
        assert!(script.contains("claude -p"));
        assert!(script.contains("werma complete"));
        assert!(script.contains("werma fail"));
        assert!(script.contains("unset CLAUDECODE"));
        assert!(script.contains("--output-format json"));
        assert!(script.contains("jq -r"));
        assert!(script.contains("--result-file"));
        assert!(script.contains("EXEC_START"));
        assert!(script.contains("CLAUDE_START"));
        assert!(script.contains("exec 2>>"));
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
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
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
        // Claude exit code is logged
        assert!(script.contains("CLAUDE_EXIT"));
    }

    #[test]
    fn exec_script_detects_max_turns_exit() {
        // RIG-252: runner script must detect error_max_turns subtype and call werma fail
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "20260325-252",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read,Edit,Write,Bash",
            max_turns: 30,
            model: "claude-opus-4-6",
            fallback_model: None,
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        assert!(
            script.contains("error_max_turns"),
            "script should check for error_max_turns subtype"
        );
        assert!(
            script.contains("SUBTYPE"),
            "script should extract SUBTYPE from JSON"
        );
        assert!(
            script.contains("MAX_TURNS_EXIT"),
            "script should log MAX_TURNS_EXIT marker"
        );
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
            runtime: AgentRuntime::ClaudeCode,
            task_type: "code",
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
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        assert!(script.contains("FALLBACK_MODEL='claude-sonnet-4-6'"));
        assert!(script.contains("run_claude"));
        assert!(script.contains(
            "rate.?limit|429|too many requests|quota.?exceeded|server.?overloaded|api.?capacity",
        ));
        assert!(script.contains("falling back to"));
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
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
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
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        assert!(!script.contains("SAFETY ABORT"));
    }

    // ─── resolve_fallback_model ───────────────────────────────────────────

    #[test]
    fn resolve_fallback_model_no_pipeline_stage() {
        let task = Task {
            pipeline_stage: String::new(),
            ..Default::default()
        };
        assert!(resolve_fallback_model(&task).is_none());
    }

    #[test]
    fn resolve_fallback_model_with_pipeline_stage() {
        let task = Task {
            pipeline_stage: "engineer".to_string(),
            ..Default::default()
        };
        // engineer stage has fallback: sonnet in default config
        let result = resolve_fallback_model(&task);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "claude-sonnet-4-6");
    }

    #[test]
    fn resolve_fallback_model_stage_without_fallback() {
        let task = Task {
            pipeline_stage: "deployer".to_string(),
            ..Default::default()
        };
        // deployer stage has no fallback in default config
        assert!(resolve_fallback_model(&task).is_none());
    }

    // ─── build_prompt: write task appends autonomous instructions ─────────

    #[test]
    fn build_prompt_write_task_appends_autonomous_mode() {
        let task = Task {
            id: "test-write-001".to_string(),
            task_type: "pipeline-engineer".to_string(),
            prompt: "Implement feature X".to_string(),
            working_dir: "/tmp".to_string(),
            ..Default::default()
        };

        let result = build_prompt(&task, Path::new("/tmp"), Path::new("/tmp/.werma")).unwrap();
        assert!(result.contains("autonomous mode instructions"));
        assert!(result.contains("git push"));
        // RIG-281: agents must NOT be told to call gh pr create — engine handles it
        assert!(
            !result.contains("gh pr create --title"),
            "prompt must not instruct agent to call gh pr create"
        );
        assert!(result.contains("Do NOT call `gh pr create`"));
    }

    #[test]
    fn build_prompt_read_task_no_autonomous_mode() {
        let task = Task {
            id: "test-read-001".to_string(),
            task_type: "research".to_string(),
            prompt: "Research topic Y".to_string(),
            working_dir: "/tmp".to_string(),
            ..Default::default()
        };

        let result = build_prompt(&task, Path::new("/tmp"), Path::new("/tmp/.werma")).unwrap();
        assert!(!result.contains("autonomous mode instructions"));
    }

    // ─── resolve_home ────────────────────────────────────────────────────

    #[test]
    fn resolve_home_expands_tilde() {
        let result = resolve_home("~/some/path");
        assert!(!result.to_string_lossy().starts_with("~/"));
        assert!(result.to_string_lossy().ends_with("/some/path"));
    }

    #[test]
    fn resolve_home_absolute_unchanged() {
        assert_eq!(resolve_home("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn resolve_home_relative_unchanged() {
        assert_eq!(
            resolve_home("relative/path"),
            PathBuf::from("relative/path")
        );
    }

    // ─── build_prompt: absolute context file path ─────────────────────────

    #[test]
    fn build_prompt_absolute_context_file() {
        let dir = tempfile::tempdir().unwrap();
        let ctx_file = dir.path().join("absolute-ctx.txt");
        std::fs::write(&ctx_file, "absolute context").unwrap();

        let task = Task {
            id: "test-abs-ctx".to_string(),
            task_type: "research".to_string(),
            prompt: "task prompt".to_string(),
            working_dir: "/tmp".to_string(),
            context_files: vec![ctx_file.to_string_lossy().to_string()],
            ..Default::default()
        };

        let result = build_prompt(&task, Path::new("/tmp"), dir.path()).unwrap();
        assert!(result.contains("absolute context"));
    }

    // ─── build_prompt: max dependency outputs limit ───────────────────────

    #[test]
    fn build_prompt_max_dependency_outputs() {
        let werma_dir = tempfile::tempdir().unwrap();
        let logs_dir = werma_dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        // Create 7 dependency outputs (limit is 5)
        let mut dep_ids = Vec::new();
        for i in 0..7 {
            let dep_id = format!("dep-{i:03}");
            std::fs::write(
                logs_dir.join(format!("{dep_id}-output.md")),
                format!("output from {dep_id}"),
            )
            .unwrap();
            dep_ids.push(dep_id);
        }

        let task = Task {
            id: "test-max-deps".to_string(),
            task_type: "code".to_string(),
            prompt: "task".to_string(),
            working_dir: "/tmp".to_string(),
            depends_on: dep_ids,
            ..Default::default()
        };

        let result = build_prompt(&task, Path::new("/tmp"), werma_dir.path()).unwrap();
        // Should include exactly MAX_DEPENDENCY_OUTPUTS (5) dependency sections
        let dep_count = result.matches("--- Dependency output:").count();
        assert_eq!(dep_count, MAX_DEPENDENCY_OUTPUTS);
        assert!(result.contains("dep-000"));
        assert!(result.contains("dep-004"));
        assert!(!result.contains("dep-005")); // beyond limit
    }

    #[test]
    fn naive_local_to_utc_iso_converts_correctly() {
        // The output should be a valid RFC 3339 timestamp in UTC.
        // We can't assert the exact value since it depends on the test machine's
        // timezone, but we can verify it parses as RFC 3339 and the round-trip
        // produces the same instant.
        use chrono::{DateTime, Local, NaiveDateTime, Utc};

        let local_ts = "2026-03-24T16:00:00";
        let result = naive_local_to_utc_iso(local_ts);

        // Should be valid RFC 3339
        let parsed = DateTime::parse_from_rfc3339(&result);
        assert!(
            parsed.is_ok(),
            "should produce valid RFC 3339, got: {result}"
        );

        // Round-trip: the UTC timestamp should represent the same instant
        // as the original local timestamp interpreted in the local timezone
        let expected = NaiveDateTime::parse_from_str(local_ts, "%Y-%m-%dT%H:%M:%S")
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parsed.unwrap().with_timezone(&Utc), expected);
    }

    #[test]
    fn naive_local_to_utc_iso_invalid_input_passthrough() {
        // Invalid timestamps should pass through unchanged
        assert_eq!(naive_local_to_utc_iso("not-a-timestamp"), "not-a-timestamp");
        assert_eq!(naive_local_to_utc_iso(""), "");
    }

    #[test]
    fn naive_local_to_utc_iso_already_rfc3339_passthrough() {
        // RFC 3339 timestamps don't match the naive format, so pass through
        let rfc = "2026-03-24T08:00:00+00:00";
        assert_eq!(naive_local_to_utc_iso(rfc), rfc);
    }

    #[test]
    fn fetch_linear_comments_sanitizes_escaped_newlines() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = crate::traits::fakes::FakeLinearApi::new();

        // Set up comments with escaped newlines (as Linear sometimes returns)
        linear.set_issue_comments(
            "RIG-TEST",
            vec![(
                "Ar".to_string(),
                "2026-03-24T14:00:00.000Z".to_string(),
                "Line one\\nLine two\\tTabbed".to_string(),
            )],
        );

        let task = Task {
            linear_issue_id: "RIG-TEST".to_string(),
            pipeline_stage: "engineer".to_string(),
            ..Default::default()
        };

        let result = fetch_linear_comments(&linear, &db, &task);
        assert!(
            result.contains("Line one\nLine two\tTabbed"),
            "should unescape \\n and \\t, got: {result}"
        );
        assert!(
            !result.contains("\\n"),
            "should not contain literal \\n, got: {result}"
        );
    }

    #[test]
    fn fetch_linear_comments_empty_when_no_comments() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let linear = crate::traits::fakes::FakeLinearApi::new();

        let task = Task {
            linear_issue_id: "RIG-EMPTY".to_string(),
            pipeline_stage: "engineer".to_string(),
            ..Default::default()
        };

        let result = fetch_linear_comments(&linear, &db, &task);
        assert!(result.is_empty(), "should be empty when no comments");
    }

    // ─── RIG-236: Sub-issue (epic) fetch tests ─────────────────────────────

    #[test]
    fn fetch_sub_issues_formats_children() {
        let linear = crate::traits::fakes::FakeLinearApi::new();
        linear.set_sub_issues(
            "RIG-236",
            vec![
                (
                    "RIG-237".to_string(),
                    "Add GraphQL query".to_string(),
                    "Todo".to_string(),
                    "Fetch children from Linear API".to_string(),
                ),
                (
                    "RIG-238".to_string(),
                    "Update prompt".to_string(),
                    "In Progress".to_string(),
                    String::new(),
                ),
            ],
        );

        let result = fetch_sub_issues(&linear, "RIG-236");
        assert!(
            result.contains("2 children"),
            "should show child count, got: {result}"
        );
        assert!(
            result.contains("[RIG-237] Add GraphQL query"),
            "should contain first child, got: {result}"
        );
        assert!(
            result.contains("**Status:** Todo"),
            "should show status, got: {result}"
        );
        assert!(
            result.contains("Fetch children from Linear API"),
            "should include description, got: {result}"
        );
        assert!(
            result.contains("[RIG-238] Update prompt"),
            "should contain second child, got: {result}"
        );
        assert!(
            result.contains("epic/parent issue"),
            "should indicate this is an epic, got: {result}"
        );
    }

    #[test]
    fn fetch_sub_issues_empty_when_no_children() {
        let linear = crate::traits::fakes::FakeLinearApi::new();
        let result = fetch_sub_issues(&linear, "RIG-100");
        assert!(result.is_empty(), "should be empty when no children");
    }

    #[test]
    fn fetch_sub_issues_sanitizes_escaped_newlines() {
        let linear = crate::traits::fakes::FakeLinearApi::new();
        linear.set_sub_issues(
            "RIG-300",
            vec![(
                "RIG-301".to_string(),
                "Child".to_string(),
                "Todo".to_string(),
                "Line one\\nLine two".to_string(),
            )],
        );

        let result = fetch_sub_issues(&linear, "RIG-300");
        assert!(
            result.contains("Line one\nLine two"),
            "should unescape \\n, got: {result}"
        );
    }

    // ─── RIG-299: Fallback model tests ──────────────────────────────────────

    #[test]
    fn resolve_fallback_model_returns_none_for_non_pipeline_task() {
        let task = Task {
            pipeline_stage: String::new(),
            ..Default::default()
        };
        assert!(resolve_fallback_model(&task).is_none());
    }

    #[test]
    fn resolve_fallback_model_returns_fallback_for_pipeline_task() {
        let task = Task {
            pipeline_stage: "engineer".to_string(),
            ..Default::default()
        };
        let result = resolve_fallback_model(&task);
        // default.yaml has fallback: sonnet for engineer stage
        assert_eq!(result.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn resolve_fallback_model_returns_none_for_stage_without_fallback() {
        let task = Task {
            pipeline_stage: "deployer".to_string(),
            ..Default::default()
        };
        let result = resolve_fallback_model(&task);
        // deployer stage has no fallback configured
        assert!(result.is_none());
    }

    #[test]
    fn exec_script_contains_fallback_model_when_configured() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-001",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read,Grep",
            max_turns: 10,
            model: "claude-opus-4-6",
            fallback_model: Some("claude-sonnet-4-6"),
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        assert!(
            script.contains("FALLBACK_MODEL='claude-sonnet-4-6'"),
            "script should set FALLBACK_MODEL"
        );
        assert!(
            script.contains("is_rate_limit"),
            "script should contain rate-limit detection function"
        );
        assert!(
            script.contains("falling back to"),
            "script should have fallback warning log"
        );
    }

    #[test]
    fn exec_script_empty_fallback_when_not_configured() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-002",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read,Grep",
            max_turns: 10,
            model: "claude-opus-4-6",
            fallback_model: None,
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        assert!(
            script.contains("FALLBACK_MODEL=''"),
            "script should have empty FALLBACK_MODEL"
        );
    }

    #[test]
    fn exec_script_rate_limit_detection_checks_multiple_sources() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-003",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read,Grep",
            max_turns: 10,
            model: "claude-opus-4-6",
            fallback_model: Some("claude-sonnet-4-6"),
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        // Verify the is_rate_limit function checks multiple patterns
        assert!(
            script.contains("overloaded_error"),
            "should detect overloaded_error (Claude API error type)"
        );
        assert!(script.contains("429"), "should detect HTTP 429 status");
        assert!(script.contains("quota"), "should detect quota exhaustion");
        // Verify it checks both stderr (LOG_FILE) and stdout
        assert!(
            script.contains("tail -20 \"$LOG_FILE\""),
            "should check stderr via LOG_FILE"
        );
        assert!(script.contains("stdout_text"), "should check stdout output");
    }

    #[test]
    fn exec_script_json_error_fallback() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-004",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read,Grep",
            max_turns: 10,
            model: "claude-opus-4-6",
            fallback_model: Some("claude-sonnet-4-6"),
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        // Verify the script handles is_error=true JSON responses with rate-limit
        assert!(
            script.contains("IS_ERROR"),
            "should extract is_error from JSON"
        );
        assert!(
            script.contains("rate-limit error in JSON"),
            "should log JSON rate-limit detection"
        );
    }

    #[test]
    fn exec_script_fallback_failure_marks_task_failed() {
        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-005",
            prompt_file: Path::new("/tmp/prompt.txt"),
            output: "",
            working_dir: Path::new("/tmp"),
            tools: "Read,Grep",
            max_turns: 10,
            model: "claude-opus-4-6",
            fallback_model: Some("claude-sonnet-4-6"),
            log_file: Path::new("/tmp/test.log"),
            is_write_task: false,
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        // After fallback failure, should call werma fail
        assert!(
            script.contains("fallback model $FALLBACK_MODEL also failed"),
            "should log fallback failure"
        );
        // Count werma fail calls — should be multiple (primary fail, fallback fail, JSON fail)
        let fail_count = script.matches("werma fail").count();
        assert!(
            fail_count >= 3,
            "should have at least 3 werma fail paths (no fallback, fallback failed, JSON fallback failed), got {fail_count}"
        );
    }

    // ─── build_prompt: handoff_content injection ──────────────────────────

    #[test]
    fn build_prompt_injects_handoff_content() {
        let dir = tempfile::tempdir().unwrap();

        let task = Task {
            id: "handoff-001".to_string(),
            task_type: "pipeline-engineer".to_string(),
            prompt: "Implement the feature".to_string(),
            working_dir: dir.path().to_string_lossy().to_string(),
            handoff_content: "## Handoff\nPrevious stage output here.".to_string(),
            ..Default::default()
        };

        let result = build_prompt(&task, dir.path(), dir.path()).unwrap();

        assert!(
            result.contains("--- Pipeline Handoff ---"),
            "prompt should contain handoff header, got: {result}"
        );
        assert!(
            result.contains("## Handoff\nPrevious stage output here."),
            "prompt should contain handoff markdown content, got: {result}"
        );
        assert!(
            result.contains("--- End Handoff ---"),
            "prompt should contain handoff footer, got: {result}"
        );
    }

    #[test]
    fn codex_sandbox_mode_read_only_types() {
        assert_eq!(codex_sandbox_mode("pipeline-reviewer"), "read-only");
        assert_eq!(codex_sandbox_mode("pipeline-analyst"), "read-only");
        assert_eq!(codex_sandbox_mode("pipeline-qa"), "read-only");
        assert_eq!(codex_sandbox_mode("review"), "read-only");
        assert_eq!(codex_sandbox_mode("analyze"), "read-only");
    }

    #[test]
    fn codex_sandbox_mode_write_types() {
        // research uses Write, so it must NOT be read-only
        assert_eq!(codex_sandbox_mode("research"), "workspace-write");
        assert_eq!(codex_sandbox_mode("code"), "workspace-write");
        assert_eq!(codex_sandbox_mode("full"), "workspace-write");
        assert_eq!(codex_sandbox_mode("pipeline-engineer"), "workspace-write");
        assert_eq!(codex_sandbox_mode("custom"), "workspace-write");
    }

    #[test]
    fn generate_codex_exec_script_contains_codex_exec() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_file = dir.path().join("prompt.txt");
        let log_file = dir.path().join("test.log");
        std::fs::write(&prompt_file, "test").unwrap();

        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-001",
            prompt_file: &prompt_file,
            output: "",
            working_dir: dir.path(),
            tools: "Read,Grep",
            max_turns: 10,
            model: "o3",
            fallback_model: None,
            log_file: &log_file,
            is_write_task: false,
            runtime: AgentRuntime::Codex,
            task_type: "research",
        });

        assert!(
            script.contains("codex exec"),
            "codex script should use codex exec"
        );
        assert!(
            script.contains("--sandbox workspace-write"),
            "research should use workspace-write sandbox"
        );
        assert!(
            script.contains("--full-auto"),
            "research should use --full-auto"
        );
        assert!(
            !script.contains("claude -p"),
            "codex script should NOT contain claude -p"
        );
    }

    #[test]
    fn generate_claude_exec_script_unchanged_for_default_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_file = dir.path().join("prompt.txt");
        let log_file = dir.path().join("test.log");
        std::fs::write(&prompt_file, "test").unwrap();

        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-002",
            prompt_file: &prompt_file,
            output: "",
            working_dir: dir.path(),
            tools: "Read,Grep",
            max_turns: 10,
            model: "claude-sonnet-4-6",
            fallback_model: None,
            log_file: &log_file,
            is_write_task: false,
            runtime: AgentRuntime::ClaudeCode,
            task_type: "research",
        });

        assert!(
            script.contains("claude -p"),
            "claude runtime should use claude -p"
        );
        assert!(
            !script.contains("codex exec"),
            "claude runtime should NOT contain codex exec"
        );
    }

    #[test]
    fn codex_read_only_sandbox_for_reviewer() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_file = dir.path().join("prompt.txt");
        let log_file = dir.path().join("test.log");
        std::fs::write(&prompt_file, "test").unwrap();

        let script = generate_exec_script(&ExecScriptParams {
            task_id: "test-003",
            prompt_file: &prompt_file,
            output: "",
            working_dir: dir.path(),
            tools: "Read,Grep",
            max_turns: 10,
            model: "o3",
            fallback_model: None,
            log_file: &log_file,
            is_write_task: false,
            runtime: AgentRuntime::Codex,
            task_type: "pipeline-reviewer",
        });

        assert!(
            script.contains("--sandbox read-only"),
            "reviewer should use read-only sandbox"
        );
        assert!(
            script.contains("--full-auto"),
            "reviewer should use --full-auto"
        );
    }

    // ─── RIG-335: Codex model mapping smoke tests ───────────────────────────

    #[test]
    fn codex_model_maps_claude_shorthands_to_empty() {
        assert_eq!(codex_model("opus"), "", "opus must map to empty for Codex");
        assert_eq!(
            codex_model("sonnet"),
            "",
            "sonnet must map to empty for Codex"
        );
        assert_eq!(
            codex_model("haiku"),
            "",
            "haiku must map to empty for Codex"
        );
    }

    #[test]
    fn codex_model_passes_through_explicit_models() {
        assert_eq!(codex_model("gpt-5.4"), "gpt-5.4");
        assert_eq!(codex_model("o4-mini"), "o4-mini");
        assert_eq!(codex_model("custom-model"), "custom-model");
    }

    #[test]
    fn resolve_model_dispatches_by_runtime() {
        // Codex: Claude shorthands → empty (let Codex use default gpt-5.4)
        assert_eq!(
            resolve_model("opus", AgentRuntime::Codex),
            "",
            "Codex runtime: opus → empty"
        );
        assert_eq!(
            resolve_model("sonnet", AgentRuntime::Codex),
            "",
            "Codex runtime: sonnet → empty"
        );

        // ClaudeCode: shorthands → full model IDs
        assert_eq!(
            resolve_model("opus", AgentRuntime::ClaudeCode),
            "claude-opus-4-6",
            "ClaudeCode runtime: opus → claude-opus-4-6"
        );
        assert_eq!(
            resolve_model("sonnet", AgentRuntime::ClaudeCode),
            "claude-sonnet-4-6",
            "ClaudeCode runtime: sonnet → claude-sonnet-4-6"
        );
    }
}
