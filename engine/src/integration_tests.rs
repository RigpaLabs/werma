use crate::db::{Db, make_test_task};
use crate::models::Status;
use crate::pipeline::executor::{callback, poll};
use crate::traits::fakes::{FakeCommandRunner, FakeLinearApi};
use serde_json::json;

/// Helper: build a minimal Linear issue JSON value for poll tests.
fn fake_issue(id: &str, identifier: &str, title: &str, labels: &[&str]) -> serde_json::Value {
    let label_nodes: Vec<serde_json::Value> = labels.iter().map(|l| json!({"name": l})).collect();

    json!({
        "id": id,
        "identifier": identifier,
        "title": title,
        "description": "test description",
        "priority": 2,
        "estimate": 3,
        "state": {"type": "backlog"},
        "labels": {"nodes": label_nodes}
    })
}

// ─── Test 1: callback_done_moves_issue ──────────────────────────────────────

#[test]
fn callback_done_moves_issue() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Insert a completed engineer task (callback needs it for dedup guard timestamp)
    let mut task = make_test_task("20260313-100");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-200".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Implementation\nDone.\n\nVERDICT=DONE";

    // FakeCommandRunner defaults to empty success (no PRs found),
    // so engineer DONE will warn about missing PR but won't fail the callback.
    // The key assertion: move_issue_by_name IS called with "review".
    callback(
        &db,
        "20260313-100",
        "engineer",
        result,
        "RIG-200",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
    )
    .unwrap();

    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-200" && status == "review"),
        "expected move to 'review', got: {moves:?}"
    );
}

// ─── Test 2: callback_move_failure_returns_error ────────────────────────────

#[test]
fn callback_move_failure_returns_error() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Configure the fake to fail the next move
    linear.fail_next_n_moves(1);

    let mut task = make_test_task("20260313-101");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-201".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Done\nVERDICT=DONE";

    let err = callback(
        &db,
        "20260313-101",
        "engineer",
        result,
        "RIG-201",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
    );

    assert!(err.is_err(), "callback should return Err when move fails");
}

// ─── Test 3: poll_no_duplicate_after_completion ─────────────────────────────

#[test]
fn poll_no_duplicate_after_completion() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Pre-insert a completed task for this issue+stage
    let mut task = make_test_task("20260313-102");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-202".to_string();
    task.pipeline_stage = "engineer".to_string();
    task.linear_pushed = true;
    db.insert_task(&task).unwrap();

    // Set up the fake Linear to return this issue at "in_progress" status
    let issue = fake_issue(
        "uuid-202",
        "RIG-202",
        "Test issue",
        &["Feature", "repo:werma"],
    );
    linear.set_issues_for_status("in_progress", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    // Should NOT create a new task — the completed one blocks it
    let tasks = db
        .tasks_by_linear_issue("RIG-202", Some("engineer"), false)
        .unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "expected only the original task, got {} tasks",
        tasks.len()
    );
}

// ─── Test 4: poll_skips_review_when_review_task_exists (RIG-135) ────────────

#[test]
fn poll_skips_review_when_review_task_exists() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Pre-insert a running review task for this issue (any review type)
    let mut task = make_test_task("20260313-103");
    task.status = Status::Running;
    task.linear_issue_id = "RIG-203".to_string();
    task.pipeline_stage = "reviewer".to_string();
    task.task_type = "pipeline-reviewer".to_string();
    db.insert_task(&task).unwrap();

    // Set up issue at "review" status (would normally trigger reviewer)
    let issue = fake_issue(
        "uuid-203",
        "RIG-203",
        "Test review",
        &["Feature", "repo:werma"],
    );
    linear.set_issues_for_status("review", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    // Should NOT create a new reviewer task — cross-stage dedup blocks it
    let tasks = db
        .tasks_by_linear_issue("RIG-203", Some("reviewer"), false)
        .unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "expected only the original task, no duplicate reviewer"
    );
}

// ─── Test 5: poll_sets_linear_issue_id (RIG-137 regression guard) ───────────

#[test]
fn poll_sets_linear_issue_id() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Set up an issue at "in_progress" that should trigger engineer stage
    let issue = fake_issue(
        "uuid-204",
        "RIG-204",
        "Test issue",
        &["Feature", "repo:werma"],
    );
    linear.set_issues_for_status("in_progress", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    // The created task should have linear_issue_id set to the identifier
    let tasks = db
        .tasks_by_linear_issue("RIG-204", Some("engineer"), false)
        .unwrap();
    assert_eq!(tasks.len(), 1, "poll should create exactly one task");
    assert_eq!(
        tasks[0].linear_issue_id, "RIG-204",
        "linear_issue_id should be set to the identifier"
    );
}

// ─── Test 6: poll_research_move_failure_nonfatal ────────────────────────────

#[test]
fn poll_research_move_failure_nonfatal() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Configure to fail the first move (research → in_progress)
    linear.fail_next_n_moves(1);

    // Set up a research issue at "todo" status
    let issue = fake_issue(
        "uuid-205",
        "RIG-205",
        "Research task",
        &["research", "repo:werma"],
    );
    linear.set_issues_for_status("todo", vec![issue]);

    // poll() should NOT fail even though the move fails — it's logged, not fatal
    poll(&db, &linear, &cmd).unwrap();

    // The task should still be created despite the move failure
    let tasks = db.tasks_by_linear_issue("RIG-205", None, false).unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "research task should be created even when move fails"
    );
    assert_eq!(tasks[0].task_type, "research");
}
