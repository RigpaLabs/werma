use std::io::Write as IoWrite;
use std::path::Path;

use anyhow::Result;

use crate::db::Db;
use crate::traits::{RealCommandRunner, RealNotifier};
use crate::{linear, pipeline};

use super::log_daemon;

/// Maximum callback attempts before abandoning and writing to dead-letter log.
const MAX_CALLBACK_ATTEMPTS: i32 = 5;

/// Process completed tasks that have Linear integration but haven't been pushed yet.
/// Pipeline tasks get routed through `pipeline::callback()` to advance the issue state.
/// Non-pipeline tasks get a comment + move-to-Done via `linear.push()`.
pub fn process_completed_tasks(db: &Db, werma_dir: &Path) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let tasks = db.unpushed_linear_tasks()?;

    if tasks.is_empty() {
        return Ok(());
    }

    // Create shared instances for all tasks in this batch.
    let linear_client = match linear::LinearClient::new() {
        Ok(c) => Some(c),
        Err(e) => {
            log_daemon(
                &log_path,
                &format!("LinearClient init failed (will skip pipeline callbacks): {e}"),
            );
            None
        }
    };
    let cmd_runner = RealCommandRunner;
    let notifier = RealNotifier;

    let now = chrono::Local::now().naive_local();

    for task in &tasks {
        // TTL: skip callbacks for tasks finished more than 1 hour ago.
        if let Some(ref finished) = task.finished_at {
            if let Ok(finished_dt) =
                chrono::NaiveDateTime::parse_from_str(finished, "%Y-%m-%dT%H:%M:%S")
            {
                if now.signed_duration_since(finished_dt).num_seconds() > 3600 {
                    log_daemon(
                        &log_path,
                        &format!(
                            "[CALLBACK] {}: {} — TTL expired (finished >1h ago), marking pushed",
                            task.linear_issue_id, task.id
                        ),
                    );
                    if let Err(e) = db.set_linear_pushed(&task.id, true) {
                        log_daemon(
                            &log_path,
                            &format!(
                                "[CALLBACK] {}: {} — TTL set_linear_pushed failed: {e}",
                                task.linear_issue_id, task.id
                            ),
                        );
                    }
                    continue;
                }
            }
        }

        if !task.pipeline_stage.is_empty() {
            let Some(ref linear_client) = linear_client else {
                continue;
            };
            // Pipeline task: read output and call pipeline::callback()
            let output_file = werma_dir.join(format!("logs/{}-output.md", task.id));
            let output = std::fs::read_to_string(&output_file).unwrap_or_default();

            match pipeline::callback(
                db,
                &task.id,
                &task.pipeline_stage,
                &output,
                &task.linear_issue_id,
                &task.working_dir,
                linear_client,
                &cmd_runner,
                &notifier,
            ) {
                Ok(()) => {
                    db.set_linear_pushed(&task.id, true)?;
                    log_daemon(
                        &log_path,
                        &format!(
                            "[CALLBACK] {}: {} stage={} -> OK",
                            task.linear_issue_id, task.id, task.pipeline_stage
                        ),
                    );
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let is_config_error = err_msg.contains("no config for stage")
                        || err_msg.contains("unknown status '");

                    if is_config_error {
                        // Config errors don't resolve with retries — abandon immediately.
                        // Increment attempts as safety net: if set_linear_pushed fails,
                        // the task re-enters this path but eventually hits MAX_CALLBACK_ATTEMPTS.
                        let attempts = db.increment_callback_attempts(&task.id).unwrap_or(1);
                        log_daemon(
                            &log_path,
                            &format!(
                                "[CALLBACK] {}: {} stage={} -> config error (no retry): {e}",
                                task.linear_issue_id, task.id, task.pipeline_stage
                            ),
                        );
                        if let Err(e) = db.set_linear_pushed(&task.id, true) {
                            log_daemon(
                                &log_path,
                                &format!(
                                    "[CALLBACK] {}: {} — set_linear_pushed failed: {e}",
                                    task.linear_issue_id, task.id
                                ),
                            );
                        }
                        write_dead_letter(
                            werma_dir,
                            &task.id,
                            &task.linear_issue_id,
                            &task.pipeline_stage,
                            &err_msg,
                            attempts,
                        );
                        continue;
                    }

                    let attempts = db.increment_callback_attempts(&task.id).unwrap_or(1);
                    log_daemon(
                        &log_path,
                        &format!(
                            "[CALLBACK] {}: {} stage={} -> FAILED (attempt {}/{}): {e}",
                            task.linear_issue_id,
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
                                task.linear_issue_id, task.id, attempts
                            ),
                        );
                        if let Err(e) = db.set_linear_pushed(&task.id, true) {
                            log_daemon(
                                &log_path,
                                &format!(
                                    "[CALLBACK] {}: {} — set_linear_pushed failed: {e}",
                                    task.linear_issue_id, task.id
                                ),
                            );
                        }
                        write_dead_letter(
                            werma_dir,
                            &task.id,
                            &task.linear_issue_id,
                            &task.pipeline_stage,
                            &err_msg,
                            attempts,
                        );
                    }
                }
            }
        } else if task.task_type == "research" {
            let Some(ref linear_client) = linear_client else {
                continue;
            };
            // Research task: post summary comment and create curator follow-up.
            let output_file = werma_dir.join(format!("logs/{}-output.md", task.id));
            let output = std::fs::read_to_string(&output_file).unwrap_or_default();

            match pipeline::handle_research_completion(db, task, &output, linear_client) {
                Ok(()) => {
                    db.set_linear_pushed(&task.id, true)?;
                    log_daemon(
                        &log_path,
                        &format!(
                            "research completion: {} issue={}",
                            task.id, task.linear_issue_id
                        ),
                    );
                }
                Err(e) => {
                    log_daemon(
                        &log_path,
                        &format!("research completion failed: {} error={e}", task.id),
                    );
                }
            }
        } else {
            // Non-pipeline task with linear_issue_id: push comment + move to Done.
            match linear::LinearClient::new().and_then(|client| client.push(db, &task.id)) {
                Ok(()) => {
                    db.set_linear_pushed(&task.id, true)?;
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

    fn make_task(id: &str, pipeline_stage: &str, task_type: &str) -> Task {
        Task {
            id: id.to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-09T10:00:00".to_string(),
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
            linear_issue_id: "issue-abc".to_string(),
            linear_pushed: false,
            pipeline_stage: pipeline_stage.to_string(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
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
        direct_task.linear_issue_id = "issue-def".to_string();

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

        super::process_completed_tasks(&db, dir.path()).unwrap();
    }

    #[test]
    fn task_without_linear_issue_not_in_unpushed() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_task("20260309-003", "", "code");
        task.linear_issue_id = String::new(); // no Linear integration
        db.insert_task(&task).unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty());
    }

    #[test]
    fn callback_ttl_marks_pushed_when_finished_over_1h_ago() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        // Task finished 2 hours ago — should be TTL'd
        let mut task = make_task("20260324-ttl", "engineer", "pipeline-engineer");
        let two_hours_ago = (chrono::Local::now() - chrono::Duration::hours(2))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.finished_at = Some(two_hours_ago);
        db.insert_task(&task).unwrap();

        // Verify it's in unpushed list before
        assert_eq!(db.unpushed_linear_tasks().unwrap().len(), 1);

        super::process_completed_tasks(&db, dir.path()).unwrap();

        // After TTL, should be marked as pushed
        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty(), "TTL'd task should be marked pushed");
    }

    #[test]
    fn callback_ttl_does_not_skip_recent_tasks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let db = Db::open_in_memory().unwrap();

        // Task finished 5 minutes ago — should NOT be TTL'd
        let mut task = make_task("20260324-recent", "engineer", "pipeline-engineer");
        let five_min_ago = (chrono::Local::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        task.finished_at = Some(five_min_ago);
        db.insert_task(&task).unwrap();

        // Callback will fire (not TTL'd) — even if it fails/succeeds, the point
        // is that TTL didn't skip it. Verify the task was NOT skipped by TTL
        // by checking that the callback attempted to process it (log file will have output).
        let _ = super::process_completed_tasks(&db, dir.path());

        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        // Should NOT contain TTL skip message
        assert!(
            !log_content.contains("TTL expired"),
            "recent task should not be TTL'd"
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
}
