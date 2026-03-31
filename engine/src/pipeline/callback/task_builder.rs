use std::collections::HashMap;

use anyhow::Result;

use super::super::config::PipelineConfig;
use super::super::helpers::{infer_working_dir_from_issue, truncate_lines};
use super::super::loader::resolve_prompt;
use super::super::prompt::{build_vars, render_prompt};
use super::super::verdict::{extract_rejection_feedback, is_heavy_track};
use crate::config::UserConfig;
use crate::db::Db;
use crate::linear::LinearApi;

/// Build a Task for the next pipeline stage with handoff content stored in `task.handoff_content`.
///
/// Unlike `create_next_stage_task()`, this function:
/// - Does NOT write any files (no `~/.werma/logs/*-handoff.md`)
/// - Does NOT insert into DB (caller does that atomically via `insert_task_with_conn`)
/// - Does NOT call Linear API for issue metadata (no `&dyn LinearApi` param)
/// - Returns `None` if an active task already exists for the issue + stage
#[allow(clippy::too_many_arguments)]
pub(super) fn build_next_stage_task(
    db: &Db,
    config: &PipelineConfig,
    linear_issue_id: &str,
    next_stage: &str,
    previous_output: &str,
    prev_task_id: &str,
    prev_stage: &str,
    working_dir: &str,
    estimate: i32,
    pr_url: Option<&str>,
) -> Result<Option<crate::models::Task>> {
    // Guard: don't spawn if an active task already exists for this issue + stage.
    let existing = db.tasks_by_linear_issue(linear_issue_id, Some(next_stage), true)?;
    if !existing.is_empty() {
        eprintln!(
            "skip spawn: active task already exists for {linear_issue_id} stage={next_stage}"
        );
        return Ok(None);
    }

    let stage_cfg = config
        .stage(next_stage)
        .ok_or_else(|| anyhow::anyhow!("no config for stage '{next_stage}'"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let review_round = if next_stage == "reviewer" {
        db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?
    } else {
        0
    };

    let max_turns = if let Some(t) = stage_cfg.max_turns {
        t as i32
    } else if next_stage == "engineer" {
        if is_heavy_track(estimate) { 45 } else { 30 }
    } else {
        crate::default_turns(&stage_cfg.agent)
    };
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
    let effective_model = stage_cfg
        .effective_model(estimate, review_round)
        .to_string();

    // RIG-333: For reviewer stage, look up previous reviewer's handoff_content
    // to inject context about what was flagged in prior review rounds.
    let previous_review = if next_stage == "reviewer" {
        lookup_previous_reviewer_handoff(db, linear_issue_id)
    } else {
        None
    };

    // Build the prompt without issue metadata (no Linear API call).
    let prompt = build_handoff_prompt(
        config,
        next_stage,
        prev_stage,
        linear_issue_id,
        "", // issue_title: unknown without Linear API call
        "", // issue_description: unknown without Linear API call
        previous_output,
        previous_review.as_deref(),
    );

    let pr_section = pr_url.map(|url| format!("PR: {url}\n")).unwrap_or_default();

    let handoff_content = format!(
        "## Pipeline Handoff: {} ({}) -> {} ({})\n\
         Linear issue: {}\n\
         {pr_section}\n\
         ### Previous Stage Output\n{}\n",
        prev_task_id,
        prev_stage,
        task_id,
        next_stage,
        linear_issue_id,
        truncate_lines(previous_output, 200),
    );

    let user_cfg = UserConfig::load();
    let default_dir = user_cfg.repo_dir("werma");
    let effective_working_dir = if working_dir.is_empty() || working_dir == default_dir {
        infer_working_dir_from_issue(db, linear_issue_id, &user_cfg)
    } else {
        working_dir.to_string()
    };

    use crate::models::{AgentRuntime, Status, Task};
    let task = Task {
        id: task_id,
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
        linear_issue_id: linear_issue_id.to_string(),
        linear_pushed: false,
        pipeline_stage: next_stage.to_string(),
        depends_on: vec![],
        context_files: vec![], // no filesystem dependency — handoff in DB column
        repo_hash: crate::runtime_repo_hash(),
        estimate,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content,
        runtime: stage_cfg.runtime.unwrap_or(AgentRuntime::ClaudeCode),
    };

    Ok(Some(task))
}

/// Parameters for creating the next pipeline stage task.
// Used only in tests; the production path uses build_next_stage_task() via decide_callback().
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct NextStageParams<'a> {
    pub db: &'a Db,
    pub config: &'a PipelineConfig,
    pub linear: Option<&'a dyn LinearApi>,
    pub linear_issue_id: &'a str,
    pub next_stage: &'a str,
    pub previous_output: &'a str,
    pub prev_task_id: &'a str,
    pub prev_stage: &'a str,
    pub working_dir: &'a str,
    pub estimate: i32,
    pub pr_url: Option<&'a str>,
    /// Override the logs directory for handoff files. `None` = use `~/.werma/logs/` (production).
    pub logs_dir: Option<&'a std::path::Path>,
}

/// Create a task for the next pipeline stage with handoff context.
// Used only in tests; the production path uses build_next_stage_task() via decide_callback().
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn create_next_stage_task(p: &NextStageParams<'_>) -> Result<()> {
    let NextStageParams {
        db,
        config,
        linear,
        linear_issue_id,
        next_stage,
        previous_output,
        prev_task_id,
        prev_stage,
        working_dir,
        estimate: _,
        pr_url: _,
        logs_dir: _,
    } = p;

    // Guard: don't spawn if an active task already exists for this issue + stage.
    let existing = db.tasks_by_linear_issue(linear_issue_id, Some(next_stage), true)?;
    if !existing.is_empty() {
        eprintln!(
            "skip spawn: active task already exists for {linear_issue_id} stage={next_stage}"
        );
        return Ok(());
    }

    let stage_cfg = config
        .stage(next_stage)
        .ok_or_else(|| anyhow::anyhow!("no config for stage '{next_stage}'"))?;

    let task_id = db.next_task_id()?;
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let review_round = if *next_stage == "reviewer" {
        db.count_completed_tasks_for_issue_stage(linear_issue_id, "reviewer")?
    } else {
        0
    };

    let max_turns = if let Some(t) = stage_cfg.max_turns {
        t as i32
    } else if *next_stage == "engineer" {
        if is_heavy_track(p.estimate) { 45 } else { 30 }
    } else {
        crate::default_turns(&stage_cfg.agent)
    };
    let allowed_tools = crate::runner::tools_for_type(&stage_cfg.agent, false);
    let effective_model = stage_cfg
        .effective_model(p.estimate, review_round)
        .to_string();

    let (issue_title, issue_description) = linear
        .and_then(|l| l.get_issue(linear_issue_id).ok())
        .unwrap_or_default();

    let prompt = build_handoff_prompt(
        config,
        next_stage,
        prev_stage,
        linear_issue_id,
        &issue_title,
        &issue_description,
        previous_output,
        None, // previous_review: not available in legacy create path
    );

    let logs_dir = match p.logs_dir {
        Some(dir) => dir.to_path_buf(),
        None => dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home dir"))?
            .join(".werma")
            .join("logs"),
    };
    std::fs::create_dir_all(&logs_dir)?;
    let handoff_path = logs_dir.join(format!("{task_id}-handoff.md"));

    let pr_section = p
        .pr_url
        .map(|url| format!("PR: {url}\n"))
        .unwrap_or_default();

    let handoff_content = format!(
        "## Pipeline Handoff: {} ({}) -> {} ({})\n\
         Linear issue: {}\n\
         {pr_section}\n\
         ### Previous Stage Output\n{}\n",
        prev_task_id,
        prev_stage,
        task_id,
        next_stage,
        linear_issue_id,
        truncate_lines(previous_output, 200),
    );
    std::fs::write(&handoff_path, &handoff_content)?;

    let user_cfg = UserConfig::load();
    let default_dir = user_cfg.repo_dir("werma");
    let effective_working_dir = if working_dir.is_empty() || *working_dir == default_dir {
        infer_working_dir_from_issue(db, linear_issue_id, &user_cfg)
    } else {
        working_dir.to_string()
    };

    use crate::models::{AgentRuntime, Status, Task};
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
        model: effective_model.clone(),
        max_turns,
        allowed_tools,
        session_id: String::new(),
        linear_issue_id: linear_issue_id.to_string(),
        linear_pushed: false,
        pipeline_stage: next_stage.to_string(),
        depends_on: vec![],
        context_files: vec![handoff_path.to_string_lossy().to_string()],
        repo_hash: crate::runtime_repo_hash(),
        estimate: p.estimate,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content: String::new(),
        runtime: stage_cfg.runtime.unwrap_or(AgentRuntime::ClaudeCode),
    };

    db.insert_task(&task)?;
    println!(
        "  + pipeline task: {} stage={} type={} model={}",
        task_id, next_stage, stage_cfg.agent, effective_model
    );

    Ok(())
}

/// Build the stage prompt for a spawned task (handoff context).
#[allow(clippy::too_many_arguments)]
pub(super) fn build_handoff_prompt(
    config: &PipelineConfig,
    next_stage: &str,
    prev_stage: &str,
    linear_issue_id: &str,
    issue_title: &str,
    issue_description: &str,
    previous_output: &str,
    previous_review: Option<&str>,
) -> String {
    let stage_cfg = match config.stage(next_stage) {
        Some(s) => s,
        None => {
            return format!(
                "Continue pipeline for Linear issue {linear_issue_id}. Stage: {next_stage}\n\n\
                 Previous stage ({prev_stage}) output is in the handoff context file."
            );
        }
    };

    let feedback = if next_stage == "engineer" && (prev_stage == "reviewer" || prev_stage == "qa") {
        Some(extract_rejection_feedback(previous_output))
    } else {
        None
    };

    let prompt_source = match &stage_cfg.prompt {
        Some(p) => resolve_prompt(p),
        None => {
            return format!(
                "Continue pipeline for Linear issue {linear_issue_id}. Stage: {next_stage}\n\n\
                 Previous stage ({prev_stage}) output is in the handoff context file."
            );
        }
    };

    let mut runtime: HashMap<String, String> = HashMap::new();
    runtime.insert("issue_id".to_string(), linear_issue_id.to_string());
    runtime.insert("issue_title".to_string(), issue_title.to_string());
    runtime.insert(
        "issue_description".to_string(),
        issue_description.to_string(),
    );
    runtime.insert("previous_output".to_string(), previous_output.to_string());
    runtime.insert(
        "rejection_feedback".to_string(),
        feedback.clone().unwrap_or_default(),
    );
    runtime.insert(
        "previous_review".to_string(),
        previous_review.unwrap_or_default().to_string(),
    );
    runtime.insert("working_dir".to_string(), String::new());

    let vars = build_vars(&config.templates, &runtime);
    let mut rendered = render_prompt(&prompt_source, &vars);

    if let Some(fb) = feedback
        && !rendered.contains(&fb)
        && !fb.is_empty()
    {
        let from_label = if prev_stage == "reviewer" {
            "Reviewer Feedback"
        } else {
            "QA Failure Report"
        };
        let stage_kind = if prev_stage == "reviewer" {
            "Revision"
        } else {
            "QA Fix"
        };
        rendered = format!(
            "# Pipeline: Engineer Stage ({stage_kind})\n\
             Linear issue: {linear_issue_id}\n\n\
             ## {from_label}\n{fb}\n\n{rendered}"
        );
    }

    rendered
}

/// RIG-333: Look up the most recent completed reviewer task for an issue
/// and return its handoff_content (which contains the review summary)
/// wrapped with re-review instructions.
pub(crate) fn lookup_previous_reviewer_handoff(db: &Db, linear_issue_id: &str) -> Option<String> {
    let reviewer_tasks = db
        .tasks_by_linear_issue(linear_issue_id, Some("reviewer"), false)
        .ok()?;

    // tasks_by_linear_issue returns ordered by created_at DESC.
    // Find the most recent completed reviewer task with non-empty handoff_content.
    let handoff = reviewer_tasks
        .into_iter()
        .filter(|t| t.status == crate::models::Status::Completed)
        .find(|t| !t.handoff_content.is_empty())
        .map(|t| t.handoff_content)?;

    Some(format!(
        "## Re-Review Context\n\n\
         This is a **re-review** — a previous reviewer flagged issues that the engineer has \
         attempted to fix. Your priority is to verify the previously flagged issues are resolved, \
         then do a light pass for any new issues introduced by the fix.\n\n\
         {handoff}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Status;
    use crate::pipeline::loader::load_from_str;

    fn test_config() -> PipelineConfig {
        load_from_str(include_str!("../../../pipelines/default.yaml"), "<test>").unwrap()
    }

    #[test]
    fn callback_analyst_creates_engineer_with_context() {
        use crate::models::Task;
        let db = crate::db::Db::open_in_memory().unwrap();

        let analyst_task = Task {
            id: "20260310-001".to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-10T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-analyst".to_string(),
            prompt: "analyze issue".to_string(),
            output_path: String::new(),
            working_dir: "~/projects/werma".to_string(),
            model: "opus".to_string(),
            max_turns: 20,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "test-issue-abc".to_string(),
            linear_pushed: false,
            pipeline_stage: "analyst".to_string(),
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
        db.insert_task(&analyst_task).unwrap();

        let config = test_config();
        let analyst_output = "## Spec\nImplement feature X\n## Requirements\n- Do A\n- Do B";
        let tmpdir = tempfile::tempdir().unwrap();

        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "test-issue-abc",
            next_stage: "engineer",
            previous_output: analyst_output,
            prev_task_id: "20260310-001",
            prev_stage: "analyst",
            working_dir: "~/projects/werma",
            estimate: 0,
            pr_url: None,
            logs_dir: Some(tmpdir.path()),
        })
        .unwrap();

        let tasks = db
            .tasks_by_linear_issue("test-issue-abc", Some("engineer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);

        let eng_task = &tasks[0];
        assert_eq!(eng_task.pipeline_stage, "engineer");
        assert_eq!(eng_task.task_type, "pipeline-engineer");
        assert!(!eng_task.context_files.is_empty());
        assert_eq!(eng_task.working_dir, "~/projects/werma");
    }

    #[test]
    fn callback_reviewer_rejected_passes_feedback() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let reviewer_output = "## Review\n- blocker: no tests\nREVIEW_VERDICT=REJECTED";
        let tmpdir = tempfile::tempdir().unwrap();

        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "test-issue-def",
            next_stage: "engineer",
            previous_output: reviewer_output,
            prev_task_id: "20260310-002",
            prev_stage: "reviewer",
            working_dir: "",
            estimate: 0,
            pr_url: None,
            logs_dir: Some(tmpdir.path()),
        })
        .unwrap();

        let pending = db.list_tasks(Some(Status::Pending)).unwrap();
        assert_eq!(pending.len(), 1);

        let eng_task = &pending[0];
        assert!(
            eng_task.prompt.contains("Revision")
                || eng_task.prompt.contains("rejected")
                || eng_task.prompt.contains("blocker")
        );
        assert_eq!(eng_task.pipeline_stage, "engineer");
        assert_eq!(eng_task.task_type, "pipeline-engineer");
    }

    #[test]
    fn create_next_stage_task_skips_if_active_exists() {
        use crate::models::Task;
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let existing = Task {
            id: "20260313-050".to_string(),
            status: Status::Pending,
            linear_issue_id: "RIG-300".to_string(),
            pipeline_stage: "engineer".to_string(),
            task_type: "pipeline-engineer".to_string(),
            ..Default::default()
        };
        db.insert_task(&existing).unwrap();

        let tmpdir = tempfile::tempdir().unwrap();
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "RIG-300",
            next_stage: "engineer",
            previous_output: "spec output",
            prev_task_id: "20260313-001",
            prev_stage: "analyst",
            working_dir: "~/projects/werma",
            estimate: 0,
            pr_url: None,
            logs_dir: Some(tmpdir.path()),
        })
        .unwrap();

        let tasks = db
            .tasks_by_linear_issue("RIG-300", Some("engineer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn build_handoff_prompt_for_engineer_from_analyst() {
        let config = test_config();
        let prompt = build_handoff_prompt(
            &config,
            "engineer",
            "analyst",
            "issue-123",
            "Test Issue Title",
            "Test description",
            "spec output",
            None,
        );
        assert!(prompt.contains("issue-123"));
    }

    #[test]
    fn build_handoff_prompt_for_engineer_from_reviewer_includes_feedback() {
        let config = test_config();
        let reviewer_output =
            "## Findings\n- blocker: missing error handling\nREVIEW_VERDICT=REJECTED";
        let prompt = build_handoff_prompt(
            &config,
            "engineer",
            "reviewer",
            "issue-123",
            "Title",
            "Desc",
            reviewer_output,
            None,
        );
        assert!(
            prompt.contains("blocker")
                || prompt.contains("Revision")
                || prompt.contains("rejected")
        );
    }

    #[test]
    fn build_handoff_prompt_fallback_when_no_config() {
        let yaml = r#"
pipeline: minimal
stages:
  unknown:
    agent: pipeline-test
    model: sonnet
"#;
        let config = load_from_str(yaml, "<test>").unwrap();
        let prompt = build_handoff_prompt(
            &config,
            "nonexistent",
            "analyst",
            "RIG-99",
            "Title",
            "Desc",
            "prev output",
            None,
        );
        assert!(prompt.contains("RIG-99"));
        assert!(prompt.contains("nonexistent"));
    }

    #[test]
    fn build_handoff_prompt_from_qa() {
        let config = test_config();
        if config.stage("qa").is_some() {
            let prompt = build_handoff_prompt(
                &config,
                "engineer",
                "qa",
                "issue-456",
                "QA Failed Issue",
                "Description",
                "QA found bugs\nVERDICT=REJECTED",
                None,
            );
            assert!(
                prompt.contains("issue-456")
                    || prompt.contains("QA")
                    || prompt.contains("REJECTED")
            );
        }
    }

    #[test]
    fn handoff_includes_pr_url_when_provided() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();
        let tmpdir = tempfile::tempdir().unwrap();
        let logs_dir = tmpdir.path().join("logs");

        let today = chrono::Local::now().format("%Y%m%d").to_string();
        for i in 0..20 {
            let dummy = crate::models::Task {
                id: format!("{today}-{:03}", i + 1),
                ..Default::default()
            };
            db.insert_task(&dummy).unwrap();
        }

        let engineer_output = "Implementation complete.\nVERDICT=DONE";
        let pr_url = "https://github.com/RigpaLabs/werma/pull/42";

        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "test-issue-pr",
            next_stage: "reviewer",
            previous_output: engineer_output,
            prev_task_id: "20260312-001",
            prev_stage: "engineer",
            working_dir: "~/projects/werma",
            estimate: 0,
            pr_url: Some(pr_url),
            logs_dir: Some(&logs_dir),
        })
        .unwrap();

        let tasks = db
            .tasks_by_linear_issue("test-issue-pr", Some("reviewer"), false)
            .unwrap();
        assert_eq!(tasks.len(), 1);

        let handoff_path = &tasks[0].context_files[0];
        let handoff_content = std::fs::read_to_string(handoff_path).unwrap();
        assert!(
            handoff_content.contains(pr_url),
            "handoff should contain PR URL"
        );
    }

    #[test]
    fn callback_reviewer_rejection_reuses_branch() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();
        let tmpdir = tempfile::tempdir().unwrap();
        let logs_dir = tmpdir.path().join("logs");

        for i in 0..10 {
            let dummy = crate::models::Task {
                id: format!("20260312-{:03}", i + 1),
                ..Default::default()
            };
            db.insert_task(&dummy).unwrap();
        }

        let issue_id = "RIG-42";

        let analyst_output = "## Spec\nImplement feature X for RIG-42";
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: issue_id,
            next_stage: "engineer",
            previous_output: analyst_output,
            prev_task_id: "20260310-001",
            prev_stage: "analyst",
            working_dir: "~/projects/werma",
            estimate: 0,
            pr_url: None,
            logs_dir: Some(&logs_dir),
        })
        .unwrap();

        let initial_tasks = db
            .tasks_by_linear_issue(issue_id, Some("engineer"), false)
            .unwrap();
        assert_eq!(initial_tasks.len(), 1);
        let initial_task = &initial_tasks[0];

        db.set_task_status(&initial_task.id, Status::Completed)
            .unwrap();

        let reviewer_output = "## Review\n- blocker: no tests\nREVIEW_VERDICT=REJECTED";
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: issue_id,
            next_stage: "engineer",
            previous_output: reviewer_output,
            prev_task_id: "20260310-002",
            prev_stage: "reviewer",
            working_dir: "~/projects/werma",
            estimate: 0,
            pr_url: None,
            logs_dir: Some(&logs_dir),
        })
        .unwrap();

        let all_eng_tasks = db
            .tasks_by_linear_issue(issue_id, Some("engineer"), false)
            .unwrap();
        assert_eq!(all_eng_tasks.len(), 2);

        let branch1 = crate::worktree::generate_branch_name(initial_task);
        let respawned_task = all_eng_tasks
            .iter()
            .find(|t| t.id != initial_task.id)
            .unwrap();
        let branch2 = crate::worktree::generate_branch_name(respawned_task);

        assert_eq!(
            branch1, branch2,
            "re-spawned engineer must reuse the same branch for PR continuity"
        );
    }

    #[test]
    fn callback_engineer_done_without_pr_still_creates_reviewer_task_directly() {
        // NOTE: This tests create_next_stage_task() directly (the low-level builder),
        // NOT the decide_callback flow. The builder itself always creates the task
        // when called. The RIG-334 gate that skips reviewer spawn when no PR_URL
        // lives in decide_callback, not in the builder.
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();

        let tmpdir = tempfile::tempdir().unwrap();
        create_next_stage_task(&NextStageParams {
            db: &db,
            config: &config,
            linear: None,
            linear_issue_id: "RIG-232",
            next_stage: "reviewer",
            previous_output: "Implementation complete.\nVERDICT=DONE",
            prev_task_id: "20260314-232",
            prev_stage: "engineer",
            working_dir: "~/projects/werma",
            estimate: 0,
            pr_url: None,
            logs_dir: Some(tmpdir.path()),
        })
        .unwrap();

        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-232", Some("reviewer"), false)
            .unwrap();
        assert!(
            !reviewer_tasks.is_empty(),
            "create_next_stage_task should create reviewer when called directly"
        );

        let reviewer = &reviewer_tasks[0];
        assert_eq!(reviewer.pipeline_stage, "reviewer");
        assert_eq!(reviewer.linear_issue_id, "RIG-232");
        assert_eq!(reviewer.status, Status::Pending);
    }

    /// RIG-333: build_handoff_prompt for reviewer includes previous review context.
    #[test]
    fn build_handoff_prompt_reviewer_with_previous_review() {
        let config = test_config();
        let previous_review = "## Re-Review Context\n\n## Previous Review (REVIEW_VERDICT=REJECTED)\n\n- blocker: missing tests\n- blocker: SQL injection";
        let prompt = build_handoff_prompt(
            &config,
            "reviewer",
            "engineer",
            "RIG-333",
            "Title",
            "Desc",
            "engineer output",
            Some(previous_review),
        );
        assert!(
            prompt.contains("Re-Review Context"),
            "reviewer prompt should contain re-review context, got:\n{prompt}"
        );
        assert!(
            prompt.contains("missing tests"),
            "reviewer prompt should contain previous review issues, got:\n{prompt}"
        );
    }

    /// RIG-333: build_handoff_prompt for reviewer without previous review (round 1).
    #[test]
    fn build_handoff_prompt_reviewer_no_previous_review() {
        let config = test_config();
        let prompt = build_handoff_prompt(
            &config,
            "reviewer",
            "engineer",
            "RIG-333",
            "Title",
            "Desc",
            "engineer output",
            None,
        );
        assert!(
            !prompt.contains("Re-Review"),
            "first review should not contain re-review context"
        );
    }

    /// RIG-333: lookup_previous_reviewer_handoff returns None when no previous reviewer.
    #[test]
    fn lookup_previous_reviewer_handoff_empty() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let result = lookup_previous_reviewer_handoff(&db, "RIG-333");
        assert!(result.is_none());
    }

    /// RIG-333: lookup_previous_reviewer_handoff returns handoff from completed reviewer.
    #[test]
    fn lookup_previous_reviewer_handoff_found() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let issue_id = "RIG-333-LOOKUP";

        // Insert a completed reviewer task with handoff_content
        let mut t = crate::db::make_test_task("333-rev-1");
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "reviewer".to_string();
        t.task_type = "pipeline-reviewer".to_string();
        t.handoff_content =
            "## Previous Review (REVIEW_VERDICT=REJECTED)\n\n- blocker: no tests".to_string();
        db.insert_task(&t).unwrap();

        let result = lookup_previous_reviewer_handoff(&db, issue_id);
        assert!(result.is_some(), "should find previous reviewer handoff");
        let content = result.unwrap();
        assert!(
            content.contains("Re-Review Context"),
            "should wrap with re-review instructions"
        );
        assert!(
            content.contains("no tests"),
            "should contain the original review feedback"
        );
    }

    /// RIG-333: lookup skips reviewer tasks without handoff_content.
    #[test]
    fn lookup_previous_reviewer_handoff_skips_empty() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let issue_id = "RIG-333-SKIP";

        // Completed reviewer with empty handoff (e.g. approved on first try)
        let mut t = crate::db::make_test_task("333-rev-empty");
        t.status = Status::Completed;
        t.linear_issue_id = issue_id.to_string();
        t.pipeline_stage = "reviewer".to_string();
        t.task_type = "pipeline-reviewer".to_string();
        t.handoff_content = String::new();
        db.insert_task(&t).unwrap();

        let result = lookup_previous_reviewer_handoff(&db, issue_id);
        assert!(
            result.is_none(),
            "should return None when reviewer has no handoff"
        );
    }

    // ─── RIG-353: task_builder working_dir + handoff coverage ─────────────

    /// build_next_stage_task inherits working_dir from previous task.
    #[test]
    fn build_next_stage_task_inherits_working_dir() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();
        let issue_id = "RIG-353-WDIR";

        // Insert a completed engineer task
        let mut eng = crate::db::make_test_task("353-eng-wd");
        eng.status = Status::Completed;
        eng.linear_issue_id = issue_id.to_string();
        eng.pipeline_stage = "engineer".to_string();
        eng.working_dir = "/Users/dev/projects/werma/.trees/feat--RIG-353".to_string();
        db.insert_task(&eng).unwrap();

        let result = build_next_stage_task(
            &db,
            &config,
            issue_id,
            "reviewer",
            "Implementation complete.\nVERDICT=DONE",
            "353-eng-wd",
            "engineer",
            "/Users/dev/projects/werma/.trees/feat--RIG-353",
            3,
            Some("https://github.com/org/repo/pull/42"),
        )
        .unwrap();

        let task = result.expect("should create reviewer task");
        assert_eq!(task.pipeline_stage, "reviewer");
        // working_dir should be the same as what was passed (worktree path)
        assert_eq!(
            task.working_dir, "/Users/dev/projects/werma/.trees/feat--RIG-353",
            "reviewer should inherit the worktree working_dir"
        );
    }

    /// build_next_stage_task stores handoff_content in the task, not filesystem.
    #[test]
    fn build_next_stage_task_stores_handoff_in_db() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();
        let issue_id = "RIG-353-HANDOFF";

        let result = build_next_stage_task(
            &db,
            &config,
            issue_id,
            "reviewer",
            "## Implementation\nCode changes done.\nVERDICT=DONE",
            "353-eng-ho",
            "engineer",
            "~/projects/werma",
            3,
            None,
        )
        .unwrap();

        let task = result.expect("should create reviewer task");

        // handoff_content should be stored in the task itself (DB column)
        assert!(
            !task.handoff_content.is_empty(),
            "handoff_content must be stored in task, not filesystem"
        );
        assert!(
            task.handoff_content.contains("Pipeline Handoff"),
            "handoff should contain handoff header, got: {}",
            &task.handoff_content[..100.min(task.handoff_content.len())]
        );
        assert!(
            task.handoff_content.contains(issue_id),
            "handoff should reference the issue ID"
        );

        // context_files should be empty (no filesystem dependency)
        assert!(
            task.context_files.is_empty(),
            "build_next_stage_task must not use filesystem — context_files should be empty"
        );
    }

    /// build_next_stage_task returns None when active task already exists for issue+stage.
    #[test]
    fn build_next_stage_task_guards_duplicate_spawn() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let config = test_config();
        let issue_id = "RIG-353-GUARD";

        // Insert an active (pending) reviewer task
        let mut existing = crate::db::make_test_task("353-rev-active");
        existing.status = Status::Pending;
        existing.linear_issue_id = issue_id.to_string();
        existing.pipeline_stage = "reviewer".to_string();
        existing.task_type = "pipeline-reviewer".to_string();
        db.insert_task(&existing).unwrap();

        let result = build_next_stage_task(
            &db,
            &config,
            issue_id,
            "reviewer",
            "engineer output",
            "353-eng-guard",
            "engineer",
            "~/projects/werma",
            3,
            None,
        )
        .unwrap();

        assert!(
            result.is_none(),
            "should return None when active task exists for issue+stage"
        );
    }
}
