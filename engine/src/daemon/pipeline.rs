use std::path::Path;

use anyhow::Result;

use crate::db::Db;
use crate::traits::RealCommandRunner;
use crate::{linear, pipeline};

use super::log_daemon;

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

    for task in &tasks {
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
            ) {
                Ok(()) => {
                    db.set_linear_pushed(&task.id, true)?;
                    log_daemon(
                        &log_path,
                        &format!(
                            "pipeline callback: {} stage={} issue={}",
                            task.id, task.pipeline_stage, task.linear_issue_id
                        ),
                    );
                }
                Err(e) => {
                    log_daemon(
                        &log_path,
                        &format!(
                            "pipeline callback failed: {} stage={} error={e}",
                            task.id, task.pipeline_stage
                        ),
                    );
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
}
