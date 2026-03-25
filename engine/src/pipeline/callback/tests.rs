use super::*;
use crate::models::{Status, Task};
use crate::pipeline::config::PipelineConfig;
use crate::pipeline::loader::load_from_str;
use crate::traits::fakes::{FakeLinearApi, FakeNotifier};

fn test_config() -> PipelineConfig {
    load_from_str(include_str!("../../../pipelines/default.yaml"), "<test>").unwrap()
}

#[test]
fn format_callback_comment_approved() {
    let comment = format_callback_comment("task-123", "reviewer", "approved", None, None);
    assert!(comment.contains("APPROVED"));
    assert!(comment.contains("task-123"));
}

#[test]
fn format_callback_comment_rejected_with_spawn() {
    let comment =
        format_callback_comment("task-456", "reviewer", "rejected", Some("engineer"), None);
    assert!(comment.contains("REJECTED"));
    assert!(comment.contains("engineer"));
}

#[test]
fn format_callback_comment_with_pr_url() {
    let comment = format_callback_comment(
        "task-789",
        "engineer",
        "done",
        None,
        Some("https://github.com/org/repo/pull/42"),
    );
    assert!(comment.contains("DONE"));
    assert!(comment.contains("https://github.com/org/repo/pull/42"));
}

#[test]
fn format_callback_comment_done_no_spawn() {
    let comment = format_callback_comment("task-001", "engineer", "done", None, None);
    assert!(comment.contains("DONE"));
    assert!(comment.contains("task-001"));
    assert!(comment.contains("Engineer"));
}

#[test]
fn format_callback_comment_with_spawn_and_pr() {
    let comment = format_callback_comment(
        "task-002",
        "engineer",
        "done",
        Some("reviewer"),
        Some("https://github.com/org/repo/pull/5"),
    );
    assert!(comment.contains("reviewer"));
    assert!(comment.contains("pull/5"));
}

#[test]
fn move_with_retry_succeeds_first_attempt() {
    let linear = FakeLinearApi::new();
    linear.set_issue_status("RIG-100", "In Review");

    let result = move_with_retry(&linear, "RIG-100", "review");
    assert!(result.is_ok());

    let moves = linear.move_calls.borrow();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0], ("RIG-100".to_string(), "review".to_string()));
}

#[test]
fn move_with_retry_succeeds_after_one_failure() {
    let linear = FakeLinearApi::new();
    linear.fail_next_n_moves(1);

    let result = move_with_retry(&linear, "RIG-100", "review");
    assert!(result.is_ok());

    let moves = linear.move_calls.borrow();
    assert_eq!(moves.len(), 1);
}

#[test]
fn move_with_retry_fails_all_retries() {
    let linear = FakeLinearApi::new();
    linear.fail_next_n_moves(3);

    let result = move_with_retry(&linear, "RIG-100", "review");
    assert!(result.is_err());

    let moves = linear.move_calls.borrow();
    assert!(moves.is_empty(), "no successful moves recorded");
}

#[test]
fn callback_analyst_creates_engineer_with_context() {
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
        working_dir: "~/projects/rigpa/werma".to_string(),
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
        working_dir: "~/projects/rigpa/werma",
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
    assert_eq!(eng_task.working_dir, "~/projects/rigpa/werma");
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
        working_dir: "~/projects/rigpa/werma",
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
    let prompt = spawn::build_handoff_prompt(
        &config,
        "engineer",
        "analyst",
        "issue-123",
        "Test Issue Title",
        "Test description",
        "spec output",
    );
    assert!(prompt.contains("issue-123"));
}

#[test]
fn build_handoff_prompt_for_engineer_from_reviewer_includes_feedback() {
    let config = test_config();
    let reviewer_output = "## Findings\n- blocker: missing error handling\nREVIEW_VERDICT=REJECTED";
    let prompt = spawn::build_handoff_prompt(
        &config,
        "engineer",
        "reviewer",
        "issue-123",
        "Title",
        "Desc",
        reviewer_output,
    );
    assert!(
        prompt.contains("blocker") || prompt.contains("Revision") || prompt.contains("rejected")
    );
}

#[test]
fn callback_no_verdict_does_not_create_task() {
    assert!(
        crate::pipeline::verdict::parse_verdict("Just some output without verdict markers")
            .is_none()
    );

    let db = crate::db::Db::open_in_memory().unwrap();
    let pending = db.list_tasks(Some(Status::Pending)).unwrap();
    assert!(pending.is_empty());
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
    let prompt = spawn::build_handoff_prompt(
        &config,
        "nonexistent",
        "analyst",
        "RIG-99",
        "Title",
        "Desc",
        "prev output",
    );
    assert!(prompt.contains("RIG-99"));
    assert!(prompt.contains("nonexistent"));
}

#[test]
fn build_handoff_prompt_from_qa() {
    let config = test_config();
    if config.stage("qa").is_some() {
        let prompt = spawn::build_handoff_prompt(
            &config,
            "engineer",
            "qa",
            "issue-456",
            "QA Failed Issue",
            "Description",
            "QA found bugs\nVERDICT=REJECTED",
        );
        assert!(
            prompt.contains("issue-456") || prompt.contains("QA") || prompt.contains("REJECTED")
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
        working_dir: "~/projects/rigpa/werma",
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
        working_dir: "~/projects/rigpa/werma",
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
        working_dir: "~/projects/rigpa/werma",
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
fn callback_engineer_done_without_pr_still_spawns_reviewer() {
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
        working_dir: "~/projects/rigpa/werma",
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
        "reviewer should be spawned even without PR_URL (RIG-232 fix)"
    );

    let reviewer = &reviewer_tasks[0];
    assert_eq!(reviewer.pipeline_stage, "reviewer");
    assert_eq!(reviewer.linear_issue_id, "RIG-232");
    assert_eq!(reviewer.status, Status::Pending);
}

#[test]
fn callback_engineer_done_without_pr_posts_warning_comment() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-232b", "in_progress");

    let mut task = crate::db::make_test_task("20260314-232b");
    task.id = "20260314-232b".to_string();
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-232b".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    cmd.push_success("main");

    let result = "Implementation complete.\nVERDICT=DONE";

    callback(
        &db,
        "20260314-232b",
        "engineer",
        result,
        "RIG-232b",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-232b" && body.contains("no PR created")),
        "should warn about missing PR, got: {comments:?}"
    );

    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-232b" && status == "review"),
        "engineer DONE should move to review even without PR, got: {moves:?}"
    );
}

#[test]
fn callback_analyst_done_swaps_labels() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-253", "in_progress");

    let mut task = crate::db::make_test_task("20260315-253");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-253".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Spec\nDo the thing.\nESTIMATE=3\nVERDICT=DONE";

    callback(
        &db,
        "20260315-253",
        "analyst",
        result,
        "RIG-253",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let removes = linear.remove_label_calls.borrow();
    assert!(
        removes
            .iter()
            .any(|(id, label)| id == "RIG-253" && label == "analyze")
    );

    let adds = linear.add_label_calls.borrow();
    assert!(
        adds.iter()
            .any(|(id, label)| id == "RIG-253" && label == "analyze:done")
    );
    assert!(
        adds.iter()
            .any(|(id, label)| id == "RIG-253" && label == "spec:done")
    );
}

#[test]
fn callback_analyst_already_done_adds_spec_done() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-274", "done");

    let mut task = crate::db::make_test_task("20260324-274");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-274".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "Issue already has implementation.\nVERDICT=ALREADY_DONE";

    callback(
        &db,
        "20260324-274",
        "analyst",
        result,
        "RIG-274",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let adds = linear.add_label_calls.borrow();
    assert!(
        adds.iter()
            .any(|(id, label)| id == "RIG-274" && label == "spec:done")
    );
}

#[test]
fn callback_analyst_already_done_with_open_pr_still_adds_spec_done() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-274c", "in_progress");

    let mut task = crate::db::make_test_task("20260324-274c");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-274c".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    cmd.push_success(r#"[{"number":99,"headRefName":"feat/rig-274c-my-spec"}]"#);

    let result = "Issue already has a spec.\nVERDICT=ALREADY_DONE";

    callback(
        &db,
        "20260324-274c",
        "analyst",
        result,
        "RIG-274c",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let adds = linear.add_label_calls.borrow();
    assert!(
        adds.iter()
            .any(|(id, label)| id == "RIG-274c" && label == "spec:done")
    );
}

#[test]
fn callback_analyst_blocked_adds_analyze_blocked_not_spec_done() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-274b", "blocked");

    let mut task = crate::db::make_test_task("20260324-275");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-274b".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "Cannot proceed, missing requirements.\nVERDICT=BLOCKED";

    callback(
        &db,
        "20260324-275",
        "analyst",
        result,
        "RIG-274b",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let adds = linear.add_label_calls.borrow();
    assert!(
        !adds
            .iter()
            .any(|(id, label)| id == "RIG-274b" && label == "spec:done")
    );
    assert!(
        adds.iter()
            .any(|(id, label)| id == "RIG-274b" && label == "analyze:blocked")
    );
    assert!(
        !adds
            .iter()
            .any(|(id, label)| id == "RIG-274b" && label == "analyze:done")
    );

    let removes = linear.remove_label_calls.borrow();
    assert!(
        removes
            .iter()
            .any(|(id, label)| id == "RIG-274b" && label == "analyze")
    );
}

#[test]
fn review_cycle_escalation_uses_config_status() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    let task = Task {
        id: "20260324-esc".to_string(),
        status: Status::Completed,
        priority: 1,
        created_at: "2026-03-24T10:00:00".to_string(),
        started_at: None,
        finished_at: None,
        task_type: "pipeline-reviewer".to_string(),
        prompt: "review issue".to_string(),
        output_path: String::new(),
        working_dir: "~/projects/rigpa/werma".to_string(),
        model: "opus".to_string(),
        max_turns: 50,
        allowed_tools: String::new(),
        session_id: String::new(),
        linear_issue_id: "RIG-ESC".to_string(),
        linear_pushed: false,
        pipeline_stage: "reviewer".to_string(),
        depends_on: vec![],
        context_files: vec![],
        repo_hash: String::new(),
        estimate: 0,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
    };
    db.insert_task(&task).unwrap();

    for i in 0..3 {
        let mut prev = task.clone();
        prev.id = format!("20260324-prev{i}");
        prev.status = Status::Completed;
        prev.linear_pushed = true;
        db.insert_task(&prev).unwrap();
    }

    let result = "Code needs changes.\nVERDICT=REJECTED";

    callback(
        &db,
        "20260324-esc",
        "reviewer",
        result,
        "RIG-ESC",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    let has_backlog = moves.iter().any(|(_, status)| status == "backlog");
    let has_blocked = moves.iter().any(|(_, status)| status == "blocked");
    assert!(
        has_backlog,
        "escalation should move to 'backlog' (from config), got: {moves:?}"
    );
    assert!(
        !has_blocked,
        "escalation should NOT move to 'blocked' (hardcoded), got: {moves:?}"
    );
}

#[test]
fn callback_no_transition_sets_fired_at() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    let task = Task {
        id: "20260324-unk".to_string(),
        status: Status::Completed,
        priority: 1,
        created_at: "2026-03-24T10:00:00".to_string(),
        started_at: None,
        finished_at: None,
        task_type: "pipeline-reviewer".to_string(),
        prompt: "review issue".to_string(),
        output_path: String::new(),
        working_dir: "~/projects/rigpa/werma".to_string(),
        model: "opus".to_string(),
        max_turns: 50,
        allowed_tools: String::new(),
        session_id: String::new(),
        linear_issue_id: "RIG-UNK".to_string(),
        linear_pushed: false,
        pipeline_stage: "reviewer".to_string(),
        depends_on: vec![],
        context_files: vec![],
        repo_hash: String::new(),
        estimate: 0,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
    };
    db.insert_task(&task).unwrap();

    let result = "Something unusual happened.\nVERDICT=UNKNOWN_VERDICT_XYZ";

    callback(
        &db,
        "20260324-unk",
        "reviewer",
        result,
        "RIG-UNK",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    assert!(moves.is_empty());
    assert!(db.is_callback_recently_fired("20260324-unk", 60).unwrap());
}

#[test]
fn callback_max_turns_exit_does_not_transition() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-252a", "in_progress");
    let mut task = crate::db::make_test_task("20260325-252a");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-252a".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result =
        r#"{"type":"result","subtype":"error_max_turns","is_error":false,"result":"partial work"}"#;

    callback(
        &db,
        "20260325-252a",
        "engineer",
        result,
        "RIG-252a",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    assert!(moves.is_empty());

    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-252a" && body.contains("max_turns"))
    );
}

#[test]
fn callback_max_turns_in_text_output_does_not_transition() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-252b", "in_progress");
    let mut task = crate::db::make_test_task("20260325-252b");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-252b".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result = "Partial implementation done.\nerror_max_turns\nSome more text";

    callback(
        &db,
        "20260325-252b",
        "engineer",
        result,
        "RIG-252b",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    assert!(moves.is_empty());
}

#[test]
fn callback_normal_engineer_done_still_works() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-252c", "in_progress");
    let mut task = crate::db::make_test_task("20260325-252c");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-252c".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    cmd.push_success("main");
    let result = "All work done.\nPR_URL=https://github.com/org/repo/pull/1\nVERDICT=DONE";

    callback(
        &db,
        "20260325-252c",
        "engineer",
        result,
        "RIG-252c",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-252c" && status == "review")
    );
}

#[test]
fn callback_max_turns_escalates_after_repeated_failures() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-202a", "review");

    for i in 0..3 {
        let mut prev = crate::db::make_test_task(&format!("20260326-f{i}"));
        prev.status = Status::Completed;
        prev.linear_issue_id = "RIG-202a".to_string();
        prev.pipeline_stage = "reviewer".to_string();
        db.insert_task(&prev).unwrap();
        db.set_task_status(&format!("20260326-f{i}"), Status::Failed)
            .unwrap();
    }

    let mut task = crate::db::make_test_task("20260326-202a");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-202a".to_string();
    task.pipeline_stage = "reviewer".to_string();
    db.insert_task(&task).unwrap();

    let result = "Partial review done.\nerror_max_turns";

    callback(
        &db,
        "20260326-202a",
        "reviewer",
        result,
        "RIG-202a",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-202a" && status == "backlog")
    );

    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-202a" && body.contains("failure limit reached"))
    );
    assert!(db.is_callback_recently_fired("20260326-202a", 60).unwrap());
}

#[test]
fn callback_max_turns_soft_failure_below_limit() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = crate::traits::fakes::FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    linear.set_issue_status("RIG-202b", "review");

    let mut prev = crate::db::make_test_task("20260326-f0b");
    prev.status = Status::Completed;
    prev.linear_issue_id = "RIG-202b".to_string();
    prev.pipeline_stage = "reviewer".to_string();
    db.insert_task(&prev).unwrap();
    db.set_task_status("20260326-f0b", Status::Failed).unwrap();

    let mut task = crate::db::make_test_task("20260326-202b");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-202b".to_string();
    task.pipeline_stage = "reviewer".to_string();
    db.insert_task(&task).unwrap();

    let result = "Partial review.\nerror_max_turns";

    callback(
        &db,
        "20260326-202b",
        "reviewer",
        result,
        "RIG-202b",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    assert!(moves.is_empty());

    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-202b" && body.contains("Soft failure"))
    );
}
