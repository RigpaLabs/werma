// ─── executor.rs — Public API hub ────────────────────────────────────────────
//
// This module re-exports the public pipeline API that was previously implemented
// inline here. Logic lives in the focused sub-modules:
//   poll.rs      — poll() + build_poll_prompt() + is_research_issue()
//   callback.rs  — callback() + move_with_retry() + format_callback_comment()
//                  + create_next_stage_task() + NextStageParams
//   pr.rs        — auto_create_pr(), is_pr_merged_for_issue(), has_open_pr_for_issue(),
//                  pr_exists_for_issue(), pr_title_from_url()
//   helpers.rs   — resolve_home(), infer_working_dir_from_issue(), truncate_lines()

// Public pipeline API (used by daemon.rs, commands, integration tests, mod.rs)
pub use super::callback::callback;
pub use super::poll::poll;

// ─── create_initial_stage_task ────────────────────────────────────────────────

use anyhow::{Context, Result};

use super::helpers::infer_working_dir_from_issue;
use super::poll::build_poll_prompt;

use super::config::PipelineConfig;
use crate::db::Db;
use crate::models::{Status, Task};

/// Create a pipeline task for an initial stage (no previous output).
/// Used by `werma pipeline run` to manually trigger a stage.
#[allow(clippy::too_many_arguments)]
pub fn create_initial_stage_task(
    db: &Db,
    config: &PipelineConfig,
    stage_name: &str,
    identifier: &str,
    title: &str,
    description: &str,
    working_dir: &str,
    estimate: i32,
) -> Result<String> {
    let stage_cfg = config
        .stage(stage_name)
        .with_context(|| format!("unknown pipeline stage: {stage_name}"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let max_turns = stage_cfg
        .max_turns
        .map(|t| t as i32)
        .unwrap_or_else(|| crate::default_turns(&stage_cfg.agent));
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
    let effective_model = stage_cfg.effective_model(estimate, 0).to_string();

    let prompt = build_poll_prompt(config, stage_cfg, identifier, title, description);

    let effective_working_dir = if working_dir.is_empty() || working_dir == "~/projects/rigpa/werma"
    {
        infer_working_dir_from_issue(db, identifier)
    } else {
        working_dir.to_string()
    };

    let task = Task {
        id: task_id.clone(),
        status: Status::Pending,
        priority: 1,
        created_at: now,
        started_at: None,
        finished_at: None,
        task_type: stage_cfg.agent.clone(),
        prompt,
        output_path: String::new(),
        working_dir: effective_working_dir,
        model: effective_model,
        max_turns,
        allowed_tools,
        session_id: String::new(),
        linear_issue_id: identifier.to_string(),
        linear_pushed: false,
        pipeline_stage: stage_name.to_string(),
        depends_on: vec![],
        context_files: vec![],
        repo_hash: crate::runtime_repo_hash(),
        estimate,
        retry_count: 0,
        retry_after: None,
    };

    db.insert_task(&task)?;
    Ok(task_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::loader::load_from_str;

    fn test_config() -> PipelineConfig {
        load_from_str(include_str!("../../pipelines/default.yaml"), "<test>").unwrap()
    }

    #[test]
    fn create_initial_stage_task_creates_pending_task() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let task_id = create_initial_stage_task(
            &db,
            &config,
            "analyst",
            "RIG-200",
            "Test issue title",
            "Test description",
            "~/projects/rigpa/werma",
            3,
        )
        .unwrap();

        let task = db.task(&task_id).unwrap().unwrap();
        assert_eq!(task.pipeline_stage, "analyst");
        assert_eq!(task.linear_issue_id, "RIG-200");
        assert_eq!(task.status, Status::Pending);
        assert_eq!(task.estimate, 3);
        assert_eq!(task.working_dir, "~/projects/rigpa/werma");
        assert!(task.prompt.contains("RIG-200"));
    }

    #[test]
    fn create_initial_stage_task_unknown_stage_errors() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let result = create_initial_stage_task(
            &db,
            &config,
            "nonexistent_stage",
            "RIG-201",
            "Title",
            "Desc",
            "/tmp",
            0,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown pipeline stage")
        );
    }

    #[test]
    fn create_initial_stage_task_infers_working_dir_from_existing() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let prior = Task {
            id: "20260313-001".to_string(),
            linear_issue_id: "RIG-202".to_string(),
            working_dir: "~/projects/rigpa/fathom".to_string(),
            pipeline_stage: "analyst".to_string(),
            task_type: "pipeline-analyst".to_string(),
            ..Default::default()
        };
        db.insert_task(&prior).unwrap();

        let task_id = create_initial_stage_task(
            &db,
            &config,
            "analyst",
            "RIG-202",
            "Fathom task",
            "Description",
            "~/projects/rigpa/werma",
            0,
        )
        .unwrap();

        let task = db.task(&task_id).unwrap().unwrap();
        assert_eq!(task.working_dir, "~/projects/rigpa/fathom");
    }
}
