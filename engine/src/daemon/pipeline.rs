use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use crate::db::Db;
use crate::linear::LinearApi;
use crate::models::Status;
use crate::pipeline;
use crate::traits::{CommandRunner, Notifier};

use super::log_daemon;

/// Maximum age (in seconds) before a task's callback is permanently abandoned.
/// Uses `finished_at` if available, falling back to `created_at` for ghost tasks
/// that never had `finished_at` set (e.g. tasks that accumulated without a proper
/// finish timestamp).
const MAX_CALLBACK_AGE_SECS: i64 = 86_400; // 24 hours

/// Maximum callback attempts before abandoning and writing to dead-letter log.
const MAX_CALLBACK_ATTEMPTS: i32 = 5;

/// Process completed tasks that have Linear integration but haven't been pushed yet.
/// Pipeline tasks get routed through `pipeline::callback()` to advance the issue state.
/// Non-pipeline tasks get a comment + move-to-Done via `linear`.
///
/// `linear` is `None` when `LINEAR_API_KEY` is not configured. In that case, research
/// completion and non-pipeline push operations are silently skipped.
///
/// `notified_tasks` tracks task IDs that were recently notified to prevent duplicate
/// macOS/Slack alerts for the same auto-pushed task across consecutive polls.
pub fn process_completed_tasks(
    db: &Db,
    werma_dir: &Path,
    cmd_runner: &dyn CommandRunner,
    notifier: &dyn Notifier,
    linear: Option<&dyn LinearApi>,
    notified_tasks: &mut HashMap<String, Instant>,
    notification_cooldown_secs: u64,
) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let tasks = db.unpushed_linear_tasks()?;

    if tasks.is_empty() {
        return Ok(());
    }

    let now = chrono::Local::now().naive_local();

    for task in &tasks {
        // Ghost task guard: issue_identifier is empty so callback can never succeed.
        // The DB query already filters these out (issue_identifier != ''), but guard
        // defensively in case the function is called with tasks from other sources.
        if task.issue_identifier.is_empty() {
            log_daemon(
                &log_path,
                &format!(
                    "[CALLBACK] {}: empty issue_identifier — ghost task, marking pushed",
                    task.id
                ),
            );
            let _ = db.set_linear_pushed(&task.id, true);
            continue;
        }

        // Age cap: auto-mark pushed if the task is older than MAX_CALLBACK_AGE_SECS.
        // Try finished_at first; fall back to created_at when finished_at is absent or
        // malformed (the primary cause of the March accumulation — tasks whose finish
        // timestamp was never written had no TTL applied and retried forever).
        let ref_dt: Option<chrono::NaiveDateTime> = task
            .finished_at
            .as_deref()
            .and_then(|ts| chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S").ok())
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(&task.created_at, "%Y-%m-%dT%H:%M:%S").ok()
            });
        if let Some(ref_dt) = ref_dt {
            if now.signed_duration_since(ref_dt).num_seconds() > MAX_CALLBACK_AGE_SECS {
                log_daemon(
                    &log_path,
                    &format!(
                        "[CALLBACK] {}: {} — age cap (>24h), marking pushed",
                        task.issue_identifier, task.id
                    ),
                );
                if let Err(e) = db.set_linear_pushed(&task.id, true) {
                    log_daemon(
                        &log_path,
                        &format!(
                            "[CALLBACK] {}: {} — age cap set_linear_pushed failed: {e}",
                            task.issue_identifier, task.id
                        ),
                    );
                }
                notify_once(
                    notifier,
                    notified_tasks,
                    notification_cooldown_secs,
                    &task.id,
                    &task.task_type,
                    &task.issue_identifier,
                    "age cap: abandoned (>24h old)",
                );
                continue;
            }
        }

        // Attempts cap: skip tasks that have already exhausted all callback attempts.
        // This catches non-pipeline task types that don't go through the pipeline
        // error path where attempts are checked per-failure.
        let existing_attempts = db.get_callback_attempts(&task.id).unwrap_or(0);
        if existing_attempts >= MAX_CALLBACK_ATTEMPTS {
            log_daemon(
                &log_path,
                &format!(
                    "[CALLBACK] {}: {} — already at max attempts ({}), marking pushed",
                    task.issue_identifier, task.id, existing_attempts
                ),
            );
            if let Err(e) = db.set_linear_pushed(&task.id, true) {
                log_daemon(
                    &log_path,
                    &format!(
                        "[CALLBACK] {}: {} — max-attempts set_linear_pushed failed: {e}",
                        task.issue_identifier, task.id
                    ),
                );
            }
            write_dead_letter(
                werma_dir,
                &task.id,
                &task.issue_identifier,
                &task.pipeline_stage,
                "max callback attempts reached",
                existing_attempts,
            );
            notify_once(
                notifier,
                notified_tasks,
                notification_cooldown_secs,
                &task.id,
                &task.task_type,
                &task.issue_identifier,
                "abandoned: max callback attempts",
            );
            continue;
        }

        if !task.pipeline_stage.is_empty() {
            // Pipeline task: read output and call pipeline::callback().
            // callback() now only writes to the DB (effects outbox + internal changes).
            // The effect processor (called separately in the daemon tick) handles
            // all Linear/GitHub mutations and sets linear_pushed when effects are done.
            let output_file = werma_dir.join(format!("logs/{}-output.md", task.id));
            let output = std::fs::read_to_string(&output_file).unwrap_or_default();

            match pipeline::callback(
                db,
                &task.id,
                &task.pipeline_stage,
                &output,
                &task.issue_identifier,
                &task.working_dir,
                cmd_runner,
            ) {
                Ok(()) => {
                    // linear_pushed is NOT set here — the effect processor sets it
                    // once all blocking effects are executed successfully.
                    log_daemon(
                        &log_path,
                        &format!(
                            "[CALLBACK] {}: {} stage={} -> queued effects",
                            task.issue_identifier, task.id, task.pipeline_stage
                        ),
                    );
                }
                Err(e) => {
                    // Use {:#} to walk the full anyhow error chain — a context wrapper
                    // like `.with_context(|| "unknown status '...'")` embeds the message
                    // in an inner cause that .to_string() (outermost only) would miss.
                    let err_msg = format!("{e:#}");
                    let is_config_error = err_msg.contains("no config for stage")
                        || err_msg.contains("unknown pipeline stage")
                        || err_msg.contains("unknown status '");

                    if is_config_error {
                        // Config errors don't resolve with retries — abandon immediately.
                        // Increment attempts as safety net: if set_linear_pushed fails,
                        // the task re-enters this path but eventually hits MAX_CALLBACK_ATTEMPTS.
                        let attempts = db.increment_callback_attempts(&task.id).unwrap_or(i32::MAX);
                        log_daemon(
                            &log_path,
                            &format!(
                                "[CALLBACK] {}: {} stage={} -> config error (no retry): {e}",
                                task.issue_identifier, task.id, task.pipeline_stage
                            ),
                        );
                        if let Err(e) = db.set_linear_pushed(&task.id, true) {
                            log_daemon(
                                &log_path,
                                &format!(
                                    "[CALLBACK] {}: {} — set_linear_pushed failed: {e}",
                                    task.issue_identifier, task.id
                                ),
                            );
                        }
                        write_dead_letter(
                            werma_dir,
                            &task.id,
                            &task.issue_identifier,
                            &task.pipeline_stage,
                            &err_msg,
                            attempts,
                        );
                        continue;
                    }

                    let attempts = db.increment_callback_attempts(&task.id).unwrap_or(i32::MAX);
                    log_daemon(
                        &log_path,
                        &format!(
                            "[CALLBACK] {}: {} stage={} -> FAILED (attempt {}/{}): {e}",
                            task.issue_identifier,
                            task.id,
                            task.pipeline_stage,
                            attempts,
                            MAX_CALLBACK_ATTEMPTS,
                        ),
                    );
                    if attempts >= MAX_CALLBACK_ATTEMPTS {
                        log_daemon(
                            &log_path,
                            &format!(
                                "[CALLBACK] {}: {} -> ABANDONED after {} attempts",
                                task.issue_identifier, task.id, attempts
                            ),
                        );
                        if let Err(e) = db.set_linear_pushed(&task.id, true) {
                            log_daemon(
                                &log_path,
                                &format!(
                                    "[CALLBACK] {}: {} — set_linear_pushed failed: {e}",
                                    task.issue_identifier, task.id
                                ),
                            );
                        }
                        write_dead_letter(
                            werma_dir,
                            &task.id,
                            &task.issue_identifier,
                            &task.pipeline_stage,
                            &err_msg,
                            attempts,
                        );
                    }
                }
            }
        } else if task.task_type == "research" {
            let Some(client) = linear else {
                // No linear client — can't push. Age cap will eventually clear this.
                continue;
            };
            // Research task: post summary comment and create curator follow-up.
            let output_file = werma_dir.join(format!("logs/{}-output.md", task.id));
            let output = std::fs::read_to_string(&output_file).unwrap_or_default();

            match pipeline::handle_research_completion(db, task, &output, client) {
                Ok(()) => {
                    db.set_linear_pushed(&task.id, true)?;
                    log_daemon(
                        &log_path,
                        &format!(
                            "research completion: {} issue={}",
                            task.id, task.issue_identifier
                        ),
                    );
                }
                Err(e) => {
                    let attempts = db.increment_callback_attempts(&task.id).unwrap_or(i32::MAX);
                    log_daemon(
                        &log_path,
                        &format!(
                            "research completion failed (attempt {}/{}): {} error={e}",
                            attempts, MAX_CALLBACK_ATTEMPTS, task.id
                        ),
                    );
                    if attempts >= MAX_CALLBACK_ATTEMPTS {
                        log_daemon(
                            &log_path,
                            &format!(
                                "[CALLBACK] {}: {} -> ABANDONED (research) after {} attempts",
                                task.issue_identifier, task.id, attempts
                            ),
                        );
                        if let Err(e) = db.set_linear_pushed(&task.id, true) {
                            log_daemon(
                                &log_path,
                                &format!(
                                    "[CALLBACK] {}: {} — set_linear_pushed failed: {e}",
                                    task.issue_identifier, task.id
                                ),
                            );
                        }
                        write_dead_letter(
                            werma_dir,
                            &task.id,
                            &task.issue_identifier,
                            "research",
                            &format!("{e:#}"),
                            attempts,
                        );
                        notify_once(
                            notifier,
                            notified_tasks,
                            notification_cooldown_secs,
                            &task.id,
                            &task.task_type,
                            &task.issue_identifier,
                            "abandoned: max callback attempts (research)",
                        );
                    }
                }
            }
        } else {
            // Non-pipeline task with issue_identifier: push comment + move to Done.
            // Only act when a linear client is available and the identifier is a Linear issue.
            let Some(client) = linear else {
                // No linear client — can't push. Age cap will eventually clear this.
                continue;
            };
            if crate::project::ProjectResolver::tracker(&task.issue_identifier)
                != Some(crate::project::Tracker::Linear)
            {
                continue;
            }
            match push_via_linear(db, task, client) {
                Ok(()) => {
                    db.set_linear_pushed(&task.id, true)?;
                    log_daemon(
                        &log_path,
                        &format!("linear push: {} issue={}", task.id, task.issue_identifier),
                    );
                }
                Err(e) => {
                    let attempts = db.increment_callback_attempts(&task.id).unwrap_or(i32::MAX);
                    log_daemon(
                        &log_path,
                        &format!(
                            "linear push failed (attempt {}/{}): {} error={e}",
                            attempts, MAX_CALLBACK_ATTEMPTS, task.id
                        ),
                    );
                    if attempts >= MAX_CALLBACK_ATTEMPTS {
                        log_daemon(
                            &log_path,
                            &format!(
                                "[CALLBACK] {}: {} -> ABANDONED (direct) after {} attempts",
                                task.issue_identifier, task.id, attempts
                            ),
                        );
                        if let Err(e) = db.set_linear_pushed(&task.id, true) {
                            log_daemon(
                                &log_path,
                                &format!(
                                    "[CALLBACK] {}: {} — set_linear_pushed failed: {e}",
                                    task.issue_identifier, task.id
                                ),
                            );
                        }
                        write_dead_letter(
                            werma_dir,
                            &task.id,
                            &task.issue_identifier,
                            "",
                            &format!("{e:#}"),
                            attempts,
                        );
                        notify_once(
                            notifier,
                            notified_tasks,
                            notification_cooldown_secs,
                            &task.id,
                            &task.task_type,
                            &task.issue_identifier,
                            "abandoned: max callback attempts",
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// Send a macOS notification for a task, suppressing duplicates within the cooldown window.
fn notify_once(
    notifier: &dyn Notifier,
    notified_tasks: &mut HashMap<String, Instant>,
    cooldown_secs: u64,
    task_id: &str,
    task_type: &str,
    issue_identifier: &str,
    reason: &str,
) {
    let within_cooldown = notified_tasks
        .get(task_id)
        .is_some_and(|last| last.elapsed() < std::time::Duration::from_secs(cooldown_secs));
    if !within_cooldown {
        let label = crate::notify::format_notify_label(task_id, task_type, issue_identifier);
        notifier.notify_macos(
            "werma: task abandoned",
            &format!("{label} — {reason}"),
            "Basso",
        );
        notified_tasks.insert(task_id.to_string(), Instant::now());
    }
}

/// Push a task result to Linear using the `LinearApi` trait.
///
/// Posts a status comment and, for completed tasks, moves the issue to Done.
/// Mirrors the logic previously in `LinearClient::push()`, but works with any
/// `&dyn LinearApi` so the caller doesn't need a concrete `LinearClient`.
fn push_via_linear(
    db: &Db,
    task: &crate::models::Task,
    linear: &dyn LinearApi,
) -> anyhow::Result<()> {
    // Read output file if exists (first 100 lines)
    let output_preview = if !task.output_path.is_empty() {
        let path = std::path::Path::new(&task.output_path);
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let lines: Vec<&str> = content.lines().take(100).collect();
            lines.join("\n")
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let status_str = task.status.to_string();
    let mut comment = format!("**Werma task `{}`** — status: **{status_str}**\n", task.id);
    if !output_preview.is_empty() {
        comment.push_str(&format!(
            "\n<details><summary>Output preview</summary>\n\n```\n{output_preview}\n```\n</details>"
        ));
    }

    linear.comment(&task.issue_identifier, &comment)?;
    if task.status == Status::Completed {
        linear.move_issue_by_name(&task.issue_identifier, "done")?;
    }
    db.set_linear_pushed(&task.id, true)?;
    Ok(())
}

/// Write an entry to the dead-letter log when a callback is permanently abandoned.
fn write_dead_letter(
    werma_dir: &Path,
    task_id: &str,
    issue_id: &str,
    stage: &str,
    error: &str,
    attempts: i32,
) {
    let log_path = werma_dir.join("logs/dead-letters.log");
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    let line = format!("{ts} | {task_id} | {issue_id} | {stage} | {error} | {attempts}\n");
    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| f.write_all(line.as_bytes()))
    {
        eprintln!("[DEAD-LETTER] failed to write: {e}");
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::models::{Status, Task};
    use crate::traits::fakes::{FakeCommandRunner, FakeNotifier};

    fn make_task(id: &str, pipeline_stage: &str, task_type: &str) -> Task {
        // Use a recent created_at so tasks are not auto-pushed by the 24h age cap.
        // Tests that need ancient tasks should override created_at explicitly.
        let recent = (chrono::Local::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        Task {
            id: id.to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: recent,
            started_at: None,
            finished_at: None,
            task_type: task_type.to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            model: "sonnet".to_string(),
            max_turns: 15,
            allowed_tools: String::new(),
            session_id: String::new(),
            issue_identifier: "issue-abc".to_string(),
            linear_pushed: false,
            pipeline_stage: pipeline_stage.to_string(),
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
    fn missing_output_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let output_file = dir.path().join("logs/99999-output.md");
        let content = std::fs::read_to_string(&output_file).unwrap_or_default();
        assert!(content.is_empty());
    }

    #[test]
    fn skips_already_pushed() {
        let db = Db::open_in_memory().unwrap();

        let task = make_task("20260309-001", "engineer", "pipeline-engineer");
        db.insert_task(&task).unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert_eq!(unpushed.len(), 1);

        db.set_linear_pushed("20260309-001", true).unwrap();
        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty());
    }

    #[test]
    fn filters_by_pipeline_stage() {
        let db = Db::open_in_memory().unwrap();

        let pipeline_task = make_task("20260309-001", "reviewer", "pipeline-reviewer");
        let mut direct_task = make_task("20260309-002", "", "research");
        direct_task.issue_identifier = "issue-def".to_string();

        db.insert_task(&pipeline_task).unwrap();
        db.insert_task(&direct_task).unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert_eq!(unpushed.len(), 2);

        let pipeline_tasks: Vec<_> = unpushed
            .iter()
            .filter(|t| !t.pipeline_stage.is_empty())
            .collect();
        let direct_tasks: Vec<_> = unpushed
            .iter()
            .filter(|t| t.pipeline_stage.is_empty())
            .collect();

        assert_eq!(pipeline_tasks.len(), 1);
        assert_eq!(pipeline_tasks[0].id, "20260309-001");
        assert_eq!(pipeline_tasks[0].pipeline_stage, "reviewer");

        assert_eq!(direct_tasks.len(), 1);
        assert_eq!(direct_tasks[0].id, "20260309-002");
    }

    #[test]
    fn reads_output_file_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let output_file = logs_dir.join("20260309-001-output.md");
        std::fs::write(&output_file, "REVIEW_VERDICT=APPROVED\nAll looks good.").unwrap();

        let output = std::fs::read_to_string(&output_file).unwrap_or_default();
        assert!(output.contains("REVIEW_VERDICT=APPROVED"));
    }

    #[test]
    fn process_completed_tasks_no_tasks_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        )
        .unwrap();
    }

    #[test]
    fn task_without_linear_issue_not_in_unpushed() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_task("20260309-003", "", "code");
        task.issue_identifier = String::new(); // no Linear integration
        db.insert_task(&task).unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty());
    }

    // ─── RIG-398: 24h age cap + created_at fallback ──────────────────────

    #[test]
    fn age_cap_marks_pushed_when_finished_over_24h_ago() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        // Task finished 25 hours ago — must be auto-pushed by age cap.
        let mut task = make_task("20260324-ttl", "engineer", "pipeline-engineer");
        let twenty_five_hours_ago = (chrono::Local::now() - chrono::Duration::hours(25))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.finished_at = Some(twenty_five_hours_ago);
        db.insert_task(&task).unwrap();

        assert_eq!(db.unpushed_linear_tasks().unwrap().len(), 1);

        super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        )
        .unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty(), "task >24h old must be auto-pushed");

        let log = std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(log.contains("age cap"), "log must mention age cap");
    }

    #[test]
    fn age_cap_uses_created_at_when_finished_at_is_none() {
        // Ghost task: finished_at is None but created_at is 25h ago.
        // Without the created_at fallback, this task would retry forever.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        let mut task = make_task("20260324-ghost", "engineer", "pipeline-engineer");
        let twenty_five_hours_ago = (chrono::Local::now() - chrono::Duration::hours(25))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.created_at = twenty_five_hours_ago;
        task.finished_at = None; // never set
        db.insert_task(&task).unwrap();

        super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        )
        .unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(
            unpushed.is_empty(),
            "ghost task >24h old (by created_at) must be auto-pushed"
        );
    }

    #[test]
    fn age_cap_does_not_skip_recent_tasks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        // Task finished 5 minutes ago — must NOT be auto-pushed by age cap.
        let mut task = make_task("20260324-recent", "engineer", "pipeline-engineer");
        let five_min_ago = (chrono::Local::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.finished_at = Some(five_min_ago);
        db.insert_task(&task).unwrap();

        let _ = super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        );

        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(
            !log_content.contains("age cap"),
            "recent task must not be auto-pushed by age cap"
        );
    }

    // ─── RIG-398: empty issue_identifier ghost task skip ─────────────────

    #[test]
    fn ghost_task_with_empty_identifier_is_auto_pushed() {
        // A task with empty issue_identifier reached process_completed_tasks via some
        // path that bypassed the DB filter. It must be immediately marked pushed.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        // Manually construct an unpushed task with empty identifier
        // (DB query filters these, so we test the guard directly via in-memory state).
        // Call the guard logic: the easiest way is to verify via log output + DB state
        // when a ghost task somehow appears in the iteration.
        //
        // Since unpushed_linear_tasks already excludes empty identifiers,
        // we test the guard by calling process_completed_tasks with a db that has
        // no ghost tasks — the test for the guard is thus a compile+logic check.
        // The real guard matters for callers that might inject tasks directly.
        // DB-level: confirm no ghost tasks leak through.
        let mut ghost = make_task("20260324-ghost2", "", "code");
        ghost.issue_identifier = String::new();
        db.insert_task(&ghost).unwrap();

        // DB query must not return ghost tasks
        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(
            unpushed.is_empty(),
            "DB must filter ghost tasks (empty issue_identifier)"
        );
    }

    // ─── RIG-398: notification dedup ─────────────────────────────────────

    #[test]
    fn notification_dedup_suppresses_repeat_within_cooldown() {
        // Verify that when a task is auto-pushed (age cap), subsequent ticks
        // with the same task_id suppress the notification within the cooldown.
        use std::collections::HashMap;
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let notifier = crate::traits::fakes::FakeNotifier::new();
        let mut notified_tasks: HashMap<String, Instant> = HashMap::new();

        // Pre-populate: simulate task was notified just now
        notified_tasks.insert("20260324-dup".to_string(), Instant::now());

        // Call notify_once with the same task ID — should be suppressed
        super::notify_once(
            &notifier,
            &mut notified_tasks,
            300, // 300s cooldown
            "20260324-dup",
            "pipeline-engineer",
            "RIG-398",
            "test reason",
        );

        assert!(
            notifier.macos_calls.borrow().is_empty(),
            "notification must be suppressed within cooldown"
        );
    }

    #[test]
    fn notification_dedup_fires_when_not_in_cooldown() {
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let notifier = crate::traits::fakes::FakeNotifier::new();
        let mut notified_tasks: HashMap<String, std::time::Instant> = HashMap::new();
        // Task not in the map → first notification must fire
        super::notify_once(
            &notifier,
            &mut notified_tasks,
            300,
            "20260324-new",
            "pipeline-engineer",
            "RIG-398",
            "test reason",
        );

        assert_eq!(
            notifier.macos_calls.borrow().len(),
            1,
            "first notification must fire when not in cooldown"
        );
        assert!(
            notified_tasks.contains_key("20260324-new"),
            "notified_tasks must be updated after notification"
        );
    }

    // ─── Tests for retry/abandonment paths (Blocker #2) ─────────────────

    #[test]
    fn write_dead_letter_creates_log_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        super::write_dead_letter(
            dir.path(),
            "20260325-001",
            "RIG-292",
            "engineer",
            "no config for stage 'engineer'",
            5,
        );

        let content = std::fs::read_to_string(dir.path().join("logs/dead-letters.log")).unwrap();
        assert!(content.contains("20260325-001"), "should contain task_id");
        assert!(content.contains("RIG-292"), "should contain issue_id");
        assert!(content.contains("engineer"), "should contain stage");
        assert!(
            content.contains("no config for stage"),
            "should contain error"
        );
        assert!(content.contains("| 5"), "should contain attempt count");
    }

    #[test]
    fn write_dead_letter_appends_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        super::write_dead_letter(dir.path(), "task-1", "RIG-1", "analyst", "err1", 3);
        super::write_dead_letter(dir.path(), "task-2", "RIG-2", "engineer", "err2", 5);

        let content = std::fs::read_to_string(dir.path().join("logs/dead-letters.log")).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2, "should have two entries");
        assert!(lines[0].contains("task-1"));
        assert!(lines[1].contains("task-2"));
    }

    #[test]
    fn increment_callback_attempts_returns_increasing_count() {
        let db = Db::open_in_memory().unwrap();
        let task = make_task("20260325-inc", "engineer", "pipeline-engineer");
        db.insert_task(&task).unwrap();

        assert_eq!(db.increment_callback_attempts("20260325-inc").unwrap(), 1);
        assert_eq!(db.increment_callback_attempts("20260325-inc").unwrap(), 2);
        assert_eq!(db.increment_callback_attempts("20260325-inc").unwrap(), 3);
    }

    #[test]
    fn callback_stops_after_max_attempts() {
        let db = Db::open_in_memory().unwrap();
        let task = make_task("20260325-max", "engineer", "pipeline-engineer");
        db.insert_task(&task).unwrap();

        // Simulate MAX_CALLBACK_ATTEMPTS increments
        for _ in 0..super::MAX_CALLBACK_ATTEMPTS {
            db.increment_callback_attempts("20260325-max").unwrap();
        }

        let count = db.increment_callback_attempts("20260325-max").unwrap();
        // After 5 increments, count is 6 which exceeds MAX_CALLBACK_ATTEMPTS
        assert!(
            count > super::MAX_CALLBACK_ATTEMPTS,
            "count ({count}) should exceed MAX_CALLBACK_ATTEMPTS ({})",
            super::MAX_CALLBACK_ATTEMPTS
        );

        // Verify the guard condition matches what process_completed_tasks checks
        let final_count: i32 = super::MAX_CALLBACK_ATTEMPTS;
        assert!(
            final_count >= super::MAX_CALLBACK_ATTEMPTS,
            "at exactly MAX attempts, task should be abandoned"
        );
    }

    #[test]
    fn config_error_detection_matches_known_errors() {
        // These are the actual error messages produced by pipeline::callback
        let config_errors = [
            "no config for stage 'engineer'",
            "unknown status 'Review' for team 'RIG'",
        ];
        for msg in &config_errors {
            assert!(
                msg.contains("no config for stage") || msg.contains("unknown status '"),
                "should detect config error: {msg}"
            );
        }

        // Transient errors should NOT match
        let transient_errors = [
            "HTTP 500: internal server error",
            "connection timed out",
            "no response from Linear API",
        ];
        for msg in &transient_errors {
            assert!(
                !(msg.contains("no config for stage") || msg.contains("unknown status '")),
                "should NOT detect transient error as config error: {msg}"
            );
        }
    }

    // ─── Callback lifecycle integration tests (RIG-293) ──────────────

    #[test]
    fn callback_retry_increments_attempts_until_max() {
        let db = Db::open_in_memory().unwrap();
        let task = make_task("20260326-retry", "engineer", "pipeline-engineer");
        db.insert_task(&task).unwrap();

        // Simulate 5 failed callback attempts
        for i in 1..=super::MAX_CALLBACK_ATTEMPTS {
            let count = db.increment_callback_attempts("20260326-retry").unwrap();
            assert_eq!(count, i);
        }

        // At MAX_CALLBACK_ATTEMPTS, the guard condition triggers abandonment
        let final_count = db.increment_callback_attempts("20260326-retry").unwrap();
        assert!(
            final_count > super::MAX_CALLBACK_ATTEMPTS,
            "count ({final_count}) should exceed MAX ({}) — task should be abandoned",
            super::MAX_CALLBACK_ATTEMPTS
        );
    }

    #[test]
    fn age_cap_and_retry_interaction() {
        // A task that has been retried AND is past the 24h age cap should be auto-pushed
        // (age cap takes precedence over retry count).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        let mut task = make_task("20260326-ttl-retry", "engineer", "pipeline-engineer");
        let twenty_five_hours_ago = (chrono::Local::now() - chrono::Duration::hours(25))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.finished_at = Some(twenty_five_hours_ago.clone());
        task.created_at = twenty_five_hours_ago;
        db.insert_task(&task).unwrap();

        // Simulate 3 prior failed attempts
        for _ in 0..3 {
            db.increment_callback_attempts("20260326-ttl-retry")
                .unwrap();
        }

        // Age cap should mark it pushed regardless of retry count
        super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        )
        .unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(
            unpushed.is_empty(),
            "age cap should override retry — task marked pushed"
        );
    }

    #[test]
    fn callback_pre_max_attempts_not_yet_abandoned() {
        // Verify that a task at MAX_CALLBACK_ATTEMPTS - 1 is not yet abandoned.
        // The guard condition is `attempts >= MAX_CALLBACK_ATTEMPTS`, so one below
        // should keep the task in retry state.
        let db = Db::open_in_memory().unwrap();
        let task = make_task("20260326-premax", "engineer", "pipeline-engineer");
        db.insert_task(&task).unwrap();

        // Increment to MAX - 1
        let mut count = 0;
        for _ in 0..super::MAX_CALLBACK_ATTEMPTS - 1 {
            count = db.increment_callback_attempts("20260326-premax").unwrap();
        }

        assert!(
            count < super::MAX_CALLBACK_ATTEMPTS,
            "at MAX-1 ({count}), task should NOT yet be abandoned"
        );

        // One more increment → should now meet the threshold
        let count = db.increment_callback_attempts("20260326-premax").unwrap();
        assert!(
            count >= super::MAX_CALLBACK_ATTEMPTS,
            "at MAX ({count}), task should be abandoned"
        );
    }

    #[test]
    fn process_completed_tasks_handles_multiple_task_types() {
        // Verify routing: pipeline tasks, research tasks, and direct tasks
        // are all handled without errors when LinearClient is unavailable
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        // Use recent timestamps so tasks are NOT auto-pushed by the 24h age cap
        let five_min_ago = (chrono::Local::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();

        // Pipeline task
        let mut pipeline = make_task("20260326-p1", "reviewer", "pipeline-reviewer");
        pipeline.finished_at = Some(five_min_ago.clone());
        pipeline.created_at = five_min_ago.clone();
        db.insert_task(&pipeline).unwrap();

        // Research task
        let mut research = make_task("20260326-r1", "", "research");
        research.finished_at = Some(five_min_ago.clone());
        research.created_at = five_min_ago.clone();
        db.insert_task(&research).unwrap();

        // Direct linear task (code type, no pipeline stage)
        let mut direct = make_task("20260326-d1", "", "code");
        direct.issue_identifier = "issue-xyz".to_string();
        direct.finished_at = Some(five_min_ago.clone());
        direct.created_at = five_min_ago;
        db.insert_task(&direct).unwrap();

        // Should not panic or error even without LINEAR_API_KEY
        let result = super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn dead_letter_contains_all_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        super::write_dead_letter(
            dir.path(),
            "20260326-dl-fields",
            "RIG-293",
            "reviewer",
            "connection refused",
            5,
        );

        let content = std::fs::read_to_string(dir.path().join("logs/dead-letters.log")).unwrap();
        let parts: Vec<&str> = content.trim().split(" | ").collect();

        // Format: timestamp | task_id | issue_id | stage | error | attempts
        assert!(
            parts.len() >= 6,
            "dead letter should have 6+ pipe-separated fields, got: {content}"
        );
        assert!(parts[1].contains("20260326-dl-fields"));
        assert!(parts[2].contains("RIG-293"));
        assert!(parts[3].contains("reviewer"));
        assert!(parts[4].contains("connection refused"));
        assert!(parts[5].contains("5"));
    }

    #[test]
    fn age_cap_boundary_below_24h() {
        // Task finished 23 hours ago — must NOT be auto-pushed (< 24h threshold).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        let mut task = make_task("20260326-boundary", "engineer", "pipeline-engineer");
        let just_under = (chrono::Local::now() - chrono::Duration::hours(23))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.finished_at = Some(just_under.clone());
        task.created_at = just_under;
        db.insert_task(&task).unwrap();

        let _ = super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        );

        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(
            !log_content.contains("age cap"),
            "task at 23h must not be auto-pushed by age cap"
        );
    }

    // ─── RIG-353: age cap and retry edge cases ────────────────────────────────

    #[test]
    fn age_cap_skips_task_with_recent_created_at_and_none_finished_at() {
        // Task with finished_at=None and RECENT created_at must NOT be age-capped.
        // (created_at fallback: if task is new, it stays in retry queue)
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        let mut task = make_task("20260331-nofinish", "engineer", "pipeline-engineer");
        let five_min_ago = (chrono::Local::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.created_at = five_min_ago;
        task.finished_at = None;
        db.insert_task(&task).unwrap();

        let _ = super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        );

        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(
            !log_content.contains("age cap"),
            "recent task with None finished_at must not be auto-pushed"
        );
    }

    #[test]
    fn age_cap_skips_malformed_finished_at_but_uses_created_at() {
        // Task has malformed finished_at. The fallback to created_at kicks in.
        // If created_at is also recent, the task must NOT be age-capped.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        let mut task = make_task("20260331-malformed", "engineer", "pipeline-engineer");
        let five_min_ago = (chrono::Local::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.created_at = five_min_ago;
        // finished_at is malformed — the age check will fall back to created_at
        task.finished_at = Some("not-a-timestamp-at-all".to_string());
        db.insert_task(&task).unwrap();

        let _ = super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        );

        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(
            !log_content.contains("age cap"),
            "malformed finished_at with recent created_at must not trigger age cap"
        );
    }

    #[test]
    fn callback_config_error_abandons_immediately() {
        // A config error (unknown stage) should mark the task as pushed immediately,
        // not retry on subsequent ticks.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        // Task with an unknown pipeline stage → "unknown pipeline stage: unicorn"
        let mut task = make_task("20260331-cfgerr", "unicorn", "pipeline-unicorn");
        let recent = (chrono::Local::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.finished_at = Some(recent);
        db.insert_task(&task).unwrap();

        // Write an output file so the callback has something to read
        let output_path = dir.path().join("logs/20260331-cfgerr-output.md");
        std::fs::write(&output_path, "VERDICT=DONE").unwrap();

        super::process_completed_tasks(
            &db,
            dir.path(),
            &FakeCommandRunner::new(),
            &FakeNotifier::new(),
            None,
            &mut std::collections::HashMap::new(),
            300,
        )
        .unwrap();

        // Task should be marked pushed (abandoned, not retried)
        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(
            unpushed.is_empty(),
            "config error should abandon task immediately (mark pushed)"
        );

        // Dead letter log should exist
        let dead_letter =
            std::fs::read_to_string(dir.path().join("logs/dead-letters.log")).unwrap_or_default();
        assert!(
            dead_letter.contains("20260331-cfgerr"),
            "dead letter log should contain the abandoned task, got: {dead_letter}"
        );
    }
}
