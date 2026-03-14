use crate::db::{Db, make_test_task};
use crate::models::Status;
use crate::pipeline::executor::{callback, poll};
use crate::traits::fakes::{FakeCommandRunner, FakeLinearApi, FakeNotifier};
use serde_json::json;

/// Ensure `~/projects/rigpa/werma` exists so `validate_working_dir` passes on CI.
/// Locally this is a no-op (dir already exists). On CI it creates empty dirs.
fn ensure_working_dir() {
    if let Some(home) = dirs::home_dir() {
        let dir = home.join("projects/rigpa/werma");
        let _ = std::fs::create_dir_all(dir);
    }
}

/// Helper: build a minimal Linear issue JSON value for poll tests.
/// State type defaults to "backlog" (needed for label-based polling).
fn fake_issue(id: &str, identifier: &str, title: &str, labels: &[&str]) -> serde_json::Value {
    fake_issue_with_state(id, identifier, title, labels, "backlog")
}

/// Helper with explicit state type (e.g. "started" for non-backlog issues).
fn fake_issue_with_state(
    id: &str,
    identifier: &str,
    title: &str,
    labels: &[&str],
    state_type: &str,
) -> serde_json::Value {
    let label_nodes: Vec<serde_json::Value> = labels.iter().map(|l| json!({"name": l})).collect();

    json!({
        "id": id,
        "identifier": identifier,
        "title": title,
        "description": "test description",
        "priority": 2,
        "estimate": 3,
        "state": {"type": state_type},
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
        &FakeNotifier::new(),
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
// With RIG-211 retry logic, all 3 retries must fail for the callback to error.

#[test]
fn callback_move_failure_returns_error() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Configure the fake to fail all 3 retry attempts
    linear.fail_next_n_moves(3);

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
        &FakeNotifier::new(),
    );

    assert!(
        err.is_err(),
        "callback should return Err when all retries fail"
    );
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
    ensure_working_dir();
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
    ensure_working_dir();
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

// ─── Test 7: callback succeeds on retry after initial failure (RIG-211) ─────
// move_with_retry retries within a single callback invocation. One failure
// followed by a success means the callback itself returns Ok.

#[test]
fn callback_retry_after_move_failure() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-300");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-300".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result =
        "## Implementation\nDone.\n\nPR_URL=https://github.com/org/repo/pull/99\nVERDICT=DONE";

    // First move attempt fails, second succeeds (within the same callback call)
    linear.fail_next_n_moves(1);
    let ok = callback(
        &db,
        "20260313-300",
        "engineer",
        result,
        "RIG-300",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    );
    assert!(ok.is_ok(), "callback should succeed on retry: {ok:?}");

    // The retry should have moved the issue to "review"
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-300" && status == "review"),
        "retry should move to 'review', got: {moves:?}"
    );
}

// ─── Test 7b: callback fails after all retries exhausted (RIG-211) ──────────

#[test]
fn callback_all_retries_exhausted() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-301");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-301".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Done\nVERDICT=DONE";

    // All 3 retry attempts fail
    linear.fail_next_n_moves(3);
    let err = callback(
        &db,
        "20260313-301",
        "engineer",
        result,
        "RIG-301",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    );
    assert!(
        err.is_err(),
        "callback should return Err when all retries exhausted"
    );

    // Dedup guard should NOT be set after failure
    assert!(
        !db.is_callback_recently_fired("20260313-301", 60).unwrap(),
        "callback_fired_at should not be set after failure"
    );
}

// ─── Test 7c: daemon-level retry — failed callback retried on next tick ──────
// Verifies the dedup guard is NOT set on failure, allowing the daemon's next
// tick to retry successfully. This is the two-invocation scenario: first call
// fails (all moves fail), second call succeeds (moves work).

#[test]
fn callback_daemon_retry_after_failure() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    let mut task = make_test_task("20260313-302");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-302".to_string();
    task.pipeline_stage = "reviewer".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Review\nLooks good.\n\nVERDICT=APPROVED";

    // --- Daemon tick 1: all moves fail ---
    linear.fail_next_n_moves(3);
    let err = callback(
        &db,
        "20260313-302",
        "reviewer",
        result,
        "RIG-302",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    );
    assert!(
        err.is_err(),
        "first callback should fail when all moves fail"
    );

    // Dedup guard must NOT be set — allows daemon retry on next tick
    assert!(
        !db.is_callback_recently_fired("20260313-302", 60).unwrap(),
        "callback_fired_at should not be set after failure"
    );

    // Notifications should have been sent on failure
    assert!(
        !notifier.macos_calls.borrow().is_empty(),
        "macOS alert should fire on retry exhaustion"
    );
    assert!(
        !notifier.slack_calls.borrow().is_empty(),
        "Slack alert should fire on retry exhaustion"
    );

    // --- Daemon tick 2: moves succeed now ---
    // (fail_next_n_moves counter is exhausted, so moves work)
    let ok = callback(
        &db,
        "20260313-302",
        "reviewer",
        result,
        "RIG-302",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    );
    assert!(ok.is_ok(), "second callback should succeed: {ok:?}");

    // Dedup guard should now be set after success
    assert!(
        db.is_callback_recently_fired("20260313-302", 60).unwrap(),
        "callback_fired_at should be set after successful retry"
    );

    // Issue should have been moved to "ready" (reviewer APPROVED transition)
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-302" && status == "ready"),
        "retry should move to 'ready', got: {moves:?}"
    );
}

// ─── Test 7d: callback failure sends notifications via Notifier trait ────────

#[test]
fn callback_failure_sends_notifications() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();
    let notifier = FakeNotifier::new();

    let mut task = make_test_task("20260313-303");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-303".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Done\nVERDICT=DONE";

    // All retries fail
    linear.fail_next_n_moves(3);
    let _ = callback(
        &db,
        "20260313-303",
        "engineer",
        result,
        "RIG-303",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &notifier,
    );

    // Verify notifications were sent via the Notifier trait
    let macos = notifier.macos_calls.borrow();
    assert_eq!(macos.len(), 1, "should send exactly one macOS notification");
    assert!(
        macos[0].0.contains("Callback Failed"),
        "macOS title should mention failure, got: {:?}",
        macos[0].0
    );
    assert!(
        macos[0].1.contains("RIG-303"),
        "macOS message should mention issue ID, got: {:?}",
        macos[0].1
    );

    let slack = notifier.slack_calls.borrow();
    assert_eq!(slack.len(), 1, "should send exactly one Slack notification");
    assert_eq!(slack[0].0, "#werma-alerts");
    assert!(
        slack[0].1.contains("RIG-303"),
        "Slack message should mention issue ID, got: {:?}",
        slack[0].1
    );
}

// ─── Test 8: poll_creates_research_task ──────────────────────────────────────

#[test]
fn poll_creates_research_task() {
    ensure_working_dir();
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let issue = fake_issue(
        "uuid-208",
        "RIG-208",
        "Research werma scheduling",
        &["research", "repo:werma"],
    );
    linear.set_issues_for_status("todo", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    let tasks = db.tasks_by_linear_issue("RIG-208", None, false).unwrap();
    assert_eq!(tasks.len(), 1, "poll should create one research task");
    assert_eq!(tasks[0].task_type, "research");
    assert_eq!(tasks[0].linear_issue_id, "RIG-208");

    // Research issues get moved to in_progress
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "uuid-208" && status == "in_progress"),
        "research issue should move to in_progress, got: {moves:?}"
    );
}

// ─── Test 9: poll_skips_manual_research ──────────────────────────────────────

#[test]
fn poll_skips_manual_research() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let issue = fake_issue(
        "uuid-209",
        "RIG-209",
        "Research werma scheduling",
        &["research", "manual", "repo:werma"],
    );
    linear.set_issues_for_status("todo", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    let tasks = db.tasks_by_linear_issue("RIG-209", None, false).unwrap();
    assert!(tasks.is_empty(), "manual research issues should be skipped");
}

// ─── Test 10: poll_creates_engineer_task ─────────────────────────────────────

#[test]
fn poll_creates_engineer_task() {
    ensure_working_dir();
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let issue = fake_issue(
        "uuid-210",
        "RIG-210",
        "Implement werma feature",
        &["Feature", "repo:werma"],
    );
    linear.set_issues_for_status("in_progress", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    let tasks = db
        .tasks_by_linear_issue("RIG-210", Some("engineer"), false)
        .unwrap();
    assert_eq!(tasks.len(), 1, "poll should create one engineer task");
    assert_eq!(tasks[0].pipeline_stage, "engineer");
    assert_eq!(tasks[0].task_type, "pipeline-engineer");
}

// ─── Test 11: poll_skips_manual_engineer ─────────────────────────────────────

#[test]
fn poll_skips_manual_engineer() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let issue = fake_issue(
        "uuid-211",
        "RIG-211",
        "Manual werma task",
        &["Feature", "manual", "repo:werma"],
    );
    linear.set_issues_for_status("in_progress", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    let tasks = db
        .tasks_by_linear_issue("RIG-211", Some("engineer"), false)
        .unwrap();
    assert!(
        tasks.is_empty(),
        "manual issues should be skipped for engineer stage"
    );
}

// ─── Test 12: poll_reviewer_skips_merged_pr ──────────────────────────────────

#[test]
fn poll_reviewer_skips_merged_pr() {
    ensure_working_dir();
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // FakeCommandRunner: `gh pr list --search RIG-212 --state merged` returns a PR
    cmd.push_success(r#"[{"number":1}]"#);

    let issue = fake_issue(
        "uuid-212",
        "RIG-212",
        "Review werma PR",
        &["Feature", "repo:werma"],
    );
    linear.set_issues_for_status("review", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    // No reviewer task should be created — PR already merged
    let tasks = db
        .tasks_by_linear_issue("RIG-212", Some("reviewer"), false)
        .unwrap();
    assert!(
        tasks.is_empty(),
        "merged PR should skip reviewer task creation"
    );

    // Issue should be moved to done
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "uuid-212" && status == "done"),
        "merged PR should move issue to done, got: {moves:?}"
    );
}

// ─── Test 13: poll_label_removes_trigger_label ───────────────────────────────
// Analyst stage is label-triggered. After creating the task, the trigger label
// ("analyze") should be removed from the issue so it doesn't re-trigger.

#[test]
fn poll_label_removes_trigger_label() {
    ensure_working_dir();
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let issue = fake_issue(
        "uuid-213",
        "RIG-213",
        "Analyze werma feature",
        &["analyze", "repo:werma"],
    );
    linear.set_issues_for_label("analyze", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    // Task should be created
    let tasks = db
        .tasks_by_linear_issue("RIG-213", Some("analyst"), false)
        .unwrap();
    assert_eq!(tasks.len(), 1);

    // Label should be removed after task creation
    let removes = linear.remove_label_calls.borrow();
    assert!(
        removes
            .iter()
            .any(|(id, label)| id == "uuid-213" && label == "analyze"),
        "trigger label should be removed, got: {removes:?}"
    );
}

// ─── Test 14: poll_label_creates_analyst_task ────────────────────────────────

#[test]
fn poll_label_creates_analyst_task() {
    ensure_working_dir();
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let issue = fake_issue(
        "uuid-214",
        "RIG-214",
        "Analyze werma feature",
        &["analyze", "repo:werma"],
    );
    linear.set_issues_for_label("analyze", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    let tasks = db
        .tasks_by_linear_issue("RIG-214", Some("analyst"), false)
        .unwrap();
    assert_eq!(tasks.len(), 1, "poll should create one analyst task");
    assert_eq!(tasks[0].pipeline_stage, "analyst");
    assert_eq!(tasks[0].task_type, "pipeline-analyst");
}

// ─── Test 15: poll_label_skips_non_backlog ───────────────────────────────────

#[test]
fn poll_label_skips_non_backlog() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Issue with analyze label but in "started" state (not backlog)
    let issue = fake_issue_with_state(
        "uuid-215",
        "RIG-215",
        "Analyze werma feature",
        &["analyze", "repo:werma"],
        "started",
    );
    linear.set_issues_for_label("analyze", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    let tasks = db
        .tasks_by_linear_issue("RIG-215", Some("analyst"), false)
        .unwrap();
    assert!(
        tasks.is_empty(),
        "non-backlog issues should be skipped for label-based triggers"
    );
}

// ─── Test 16: poll_analyst_skips_if_engineer_ran ─────────────────────────────

#[test]
fn poll_analyst_skips_if_engineer_ran() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Pre-insert a completed engineer task for this issue
    let mut task = make_test_task("20260313-216");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-216".to_string();
    task.pipeline_stage = "engineer".to_string();
    task.task_type = "pipeline-engineer".to_string();
    db.insert_task(&task).unwrap();

    let issue = fake_issue(
        "uuid-216",
        "RIG-216",
        "Analyze werma feature",
        &["analyze", "repo:werma"],
    );
    linear.set_issues_for_label("analyze", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    let tasks = db
        .tasks_by_linear_issue("RIG-216", Some("analyst"), false)
        .unwrap();
    assert!(
        tasks.is_empty(),
        "analyst should be skipped when engineer already ran"
    );
}

// ─── Test 17: callback_dedup_guard_blocks_duplicate ──────────────────────────
// Use reviewer APPROVED — it does a clean transition (no PR logic) so
// callback_fired_at gets set, blocking duplicate calls.

#[test]
fn callback_dedup_guard_blocks_duplicate() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-217");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-217".to_string();
    task.pipeline_stage = "reviewer".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Review\nLooks good.\n\nVERDICT=APPROVED";

    // First callback: succeeds (move to ready + dedup guard set)
    callback(
        &db,
        "20260313-217",
        "reviewer",
        result,
        "RIG-217",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    let moves_before = linear.move_calls.borrow().len();

    // Second callback: dedup guard should block it
    callback(
        &db,
        "20260313-217",
        "reviewer",
        result,
        "RIG-217",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    let moves_after = linear.move_calls.borrow().len();
    assert_eq!(
        moves_before, moves_after,
        "dedup guard should prevent second move"
    );
}

// ─── Test 18: callback_empty_output_posts_comment ────────────────────────────

#[test]
fn callback_empty_output_posts_comment() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-218");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-218".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    // Empty result triggers early return with comment
    callback(
        &db,
        "20260313-218",
        "engineer",
        "   ",
        "RIG-218",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // No moves should happen
    assert!(
        linear.move_calls.borrow().is_empty(),
        "empty output should not trigger any moves"
    );

    // A comment should be posted about empty output
    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-218" && body.contains("empty output")),
        "empty output should post a comment, got: {comments:?}"
    );
}

// ─── Test 19: callback_unknown_stage_noop ────────────────────────────────────

#[test]
fn callback_unknown_stage_noop() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-219");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-219".to_string();
    task.pipeline_stage = "nonexistent_stage".to_string();
    db.insert_task(&task).unwrap();

    // Unknown stage should return Ok without doing anything
    let result = callback(
        &db,
        "20260313-219",
        "nonexistent_stage",
        "Some output\nVERDICT=DONE",
        "RIG-219",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    );

    assert!(result.is_ok(), "unknown stage should return Ok");
    assert!(
        linear.move_calls.borrow().is_empty(),
        "unknown stage should not trigger moves"
    );
}

// ─── Test 20: callback_analyst_estimate_updates_linear ───────────────────────

#[test]
fn callback_analyst_estimate_updates_linear() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-220");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-220".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Analysis\nSpec here.\n\nESTIMATE=5\nVERDICT=DONE";

    callback(
        &db,
        "20260313-220",
        "analyst",
        result,
        "RIG-220",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // Estimate should be updated on Linear
    let estimates = linear.estimate_calls.borrow();
    assert!(
        estimates
            .iter()
            .any(|(id, est)| id == "RIG-220" && *est == 5),
        "analyst should update estimate to 5, got: {estimates:?}"
    );

    // Issue should move to todo (analyst done → todo per config)
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-220" && status == "todo"),
        "analyst DONE should move to todo, got: {moves:?}"
    );
}

// ─── Test 20b: callback_analyst_adds_done_label ──────────────────────────────

#[test]
fn callback_analyst_adds_done_label() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-219");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-219".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Analysis\nSpec here.\n\nESTIMATE=3\nVERDICT=DONE";

    callback(
        &db,
        "20260313-219",
        "analyst",
        result,
        "RIG-219",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // analyze:done label should be added
    let adds = linear.add_label_calls.borrow();
    assert!(
        adds.iter()
            .any(|(id, label)| id == "RIG-219" && label == "analyze:done"),
        "analyst callback should add 'analyze:done' label, got: {adds:?}"
    );

    // Issue should still move to todo (existing behavior)
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-219" && status == "todo"),
        "analyst DONE should move to todo, got: {moves:?}"
    );
}

// ─── Test 20c: callback_analyst_blocked_also_adds_done_label ─────────────────

#[test]
fn callback_analyst_blocked_also_adds_done_label() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-219b");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-219B".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Analysis\nBlocked on external dependency.\n\nVERDICT=BLOCKED";

    callback(
        &db,
        "20260313-219b",
        "analyst",
        result,
        "RIG-219B",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // analyze:done label should be added even for BLOCKED verdict
    let adds = linear.add_label_calls.borrow();
    assert!(
        adds.iter()
            .any(|(id, label)| id == "RIG-219B" && label == "analyze:done"),
        "analyst BLOCKED callback should also add 'analyze:done' label, got: {adds:?}"
    );
}

// ─── Test 21: callback_missing_verdict_warns ─────────────────────────────────

#[test]
fn callback_missing_verdict_warns() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-221");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-221".to_string();
    task.pipeline_stage = "reviewer".to_string();
    db.insert_task(&task).unwrap();

    // Reviewer output without VERDICT= — should post warning comment
    let result = "Code looks fine. No major issues found.";

    callback(
        &db,
        "20260313-221",
        "reviewer",
        result,
        "RIG-221",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // No moves — missing verdict keeps current state
    assert!(
        linear.move_calls.borrow().is_empty(),
        "missing verdict should not trigger moves"
    );

    // Warning comment should be posted
    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-221" && body.contains("no verdict")),
        "missing verdict should post warning comment, got: {comments:?}"
    );
}

// ─── Test 22: callback_already_done_blocked_by_open_pr ───────────────────────

#[test]
fn callback_already_done_blocked_by_open_pr() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-222");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-222".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    // Analyst says ALREADY_DONE, but there's an open PR.
    // The ALREADY_DONE guard checks for open PRs BEFORE calling move_issue_by_name.
    // FakeCommandRunner: gh pr list --search RIG-222 --state open → returns a PR
    cmd.push_success(r#"[{"number":42}]"#);

    let result = "Already implemented.\n\nVERDICT=ALREADY_DONE";

    callback(
        &db,
        "20260313-222",
        "analyst",
        result,
        "RIG-222",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // The move to "done" should be blocked — open PR exists
    let moves = linear.move_calls.borrow();
    assert!(
        !moves.iter().any(|(_, status)| status == "done"),
        "ALREADY_DONE should be blocked when open PR exists, got: {moves:?}"
    );

    // A comment should explain why it was blocked
    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-222" && body.contains("ALREADY_DONE blocked")),
        "should post blocking comment, got: {comments:?}"
    );
}

// ─── Test 23: callback_engineer_done_with_pr_url ─────────────────────────────

#[test]
fn callback_engineer_done_with_pr_url() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-223");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-223".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    // Provide issue data for the spawned reviewer task
    linear.set_issue_data("RIG-223", "Test issue", "test description");

    let result = "## Implementation\nDone.\n\nPR_URL=https://github.com/RigpaLabs/werma/pull/42\nVERDICT=DONE";

    callback(
        &db,
        "20260313-223",
        "engineer",
        result,
        "RIG-223",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // PR should be attached to Linear
    let attaches = linear.attach_calls.borrow();
    assert!(
        attaches
            .iter()
            .any(|(id, url, _)| id == "RIG-223"
                && url == "https://github.com/RigpaLabs/werma/pull/42"),
        "PR URL should be attached, got: {attaches:?}"
    );

    // Issue should move to review
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-223" && status == "review"),
        "engineer DONE should move to review, got: {moves:?}"
    );
}

// ─── Test 24: callback_engineer_done_auto_pr ─────────────────────────────────

#[test]
fn callback_engineer_done_auto_pr() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-224");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-224".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    linear.set_issue_data("RIG-224", "Test issue", "test description");

    // No PR_URL in output — triggers auto_create_pr flow.
    // auto_create_pr calls sequence:
    // 1. git branch --show-current → "feat/RIG-224-impl"
    // 2. git log origin/main..HEAD --oneline → "abc1234 feat: impl"
    // 3. git push -u origin feat/RIG-224-impl → success
    // 4. gh pr view --json url -q .url → failure (no existing PR)
    // 5. gh pr create ... → PR URL
    cmd.push_success("feat/RIG-224-impl");
    cmd.push_success("abc1234 feat: implementation");
    cmd.push_success(""); // git push success
    cmd.push_failure("no pull requests found"); // gh pr view — no existing PR
    cmd.push_success("https://github.com/RigpaLabs/werma/pull/99");

    let result = "## Implementation\nDone.\n\nVERDICT=DONE";

    callback(
        &db,
        "20260313-224",
        "engineer",
        result,
        "RIG-224",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // Auto-created PR should be attached
    let attaches = linear.attach_calls.borrow();
    assert!(
        attaches
            .iter()
            .any(|(id, url, _)| id == "RIG-224"
                && url == "https://github.com/RigpaLabs/werma/pull/99"),
        "auto-created PR should be attached, got: {attaches:?}"
    );

    // Issue should move to review
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-224" && status == "review"),
        "engineer DONE should move to review, got: {moves:?}"
    );
}

// ─── Test 25: callback_engineer_done_no_pr_warns ─────────────────────────────

#[test]
fn callback_engineer_done_no_pr_warns() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-225");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-225".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    // No PR_URL in output and auto_create_pr returns None.
    // auto_create_pr: git branch → "main" (safety check → returns None)
    cmd.push_success("main");

    let result = "## Implementation\nDone.\n\nVERDICT=DONE";

    callback(
        &db,
        "20260313-225",
        "engineer",
        result,
        "RIG-225",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // A warning comment about missing PR should be posted
    let comments = linear.comment_calls.borrow();
    assert!(
        comments
            .iter()
            .any(|(id, body)| id == "RIG-225" && body.contains("no PR created")),
        "should warn about missing PR, got: {comments:?}"
    );
}

// ─── Test 26: callback_reviewer_rejected_spawns_engineer ─────────────────────

#[test]
fn callback_reviewer_rejected_spawns_engineer() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-226");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-226".to_string();
    task.pipeline_stage = "reviewer".to_string();
    db.insert_task(&task).unwrap();

    // Provide issue data for spawned engineer task
    linear.set_issue_data("RIG-226", "Fix werma bug", "bug description");

    let result =
        "## Review\nFound issues.\n\n### Feedback\nFix the error handling.\n\nVERDICT=REJECTED";

    callback(
        &db,
        "20260313-226",
        "reviewer",
        result,
        "RIG-226",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // Issue should move to in_progress (rejected → in_progress per config)
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-226" && status == "in_progress"),
        "rejected review should move to in_progress, got: {moves:?}"
    );

    // A new engineer task should be spawned
    let engineer_tasks = db
        .tasks_by_linear_issue("RIG-226", Some("engineer"), false)
        .unwrap();
    assert_eq!(
        engineer_tasks.len(),
        1,
        "rejected review should spawn engineer task"
    );
    assert_eq!(engineer_tasks[0].pipeline_stage, "engineer");
}

// ─── Test 27: callback_review_cycle_limit_escalates ──────────────────────────

#[test]
fn callback_review_cycle_limit_escalates() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Pre-insert 3 completed reviewer tasks (the max_review_rounds limit)
    for i in 0..3 {
        let mut task = make_test_task(&format!("20260313-27{i}"));
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-227".to_string();
        task.pipeline_stage = "reviewer".to_string();
        task.task_type = "pipeline-reviewer".to_string();
        task.linear_pushed = true;
        db.insert_task(&task).unwrap();
    }

    // Now insert the current (4th) reviewer task that just completed
    let mut task = make_test_task("20260313-227");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-227".to_string();
    task.pipeline_stage = "reviewer".to_string();
    task.task_type = "pipeline-reviewer".to_string();
    db.insert_task(&task).unwrap();

    linear.set_issue_data("RIG-227", "Fix werma bug", "bug description");

    let result = "## Review\nStill broken.\n\nVERDICT=REJECTED";

    callback(
        &db,
        "20260313-227",
        "reviewer",
        result,
        "RIG-227",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // Issue should be moved to blocked (escalation), not in_progress
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-227" && status == "blocked"),
        "review cycle limit should escalate to blocked, got: {moves:?}"
    );

    // No new engineer task should be spawned
    let engineer_tasks = db
        .tasks_by_linear_issue("RIG-227", Some("engineer"), false)
        .unwrap();
    assert!(
        engineer_tasks.is_empty(),
        "review cycle limit should NOT spawn new engineer"
    );
}

// ─── Test 27b: escalation to blocked retries on failure (RIG-211) ────────────
// Verifies that the escalation path uses move_with_retry, not bare move.

#[test]
fn callback_review_escalation_retries_on_failure() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Pre-insert 3 completed reviewer tasks (the max_review_rounds limit)
    for i in 0..3 {
        let mut task = make_test_task(&format!("20260313-28{i}"));
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-228".to_string();
        task.pipeline_stage = "reviewer".to_string();
        task.task_type = "pipeline-reviewer".to_string();
        task.linear_pushed = true;
        db.insert_task(&task).unwrap();
    }

    // Current (4th) reviewer task
    let mut task = make_test_task("20260313-228");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-228".to_string();
    task.pipeline_stage = "reviewer".to_string();
    task.task_type = "pipeline-reviewer".to_string();
    db.insert_task(&task).unwrap();

    linear.set_issue_data("RIG-228", "Fix werma bug", "bug description");

    let result = "## Review\nStill broken.\n\nVERDICT=REJECTED";

    // First move attempt fails, but second should succeed (retry logic)
    linear.fail_next_n_moves(1);

    callback(
        &db,
        "20260313-228",
        "reviewer",
        result,
        "RIG-228",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // Should still escalate to blocked despite initial move failure
    let moves = linear.move_calls.borrow();
    assert!(
        moves
            .iter()
            .any(|(id, status)| id == "RIG-228" && status == "blocked"),
        "escalation should retry and succeed moving to blocked, got: {moves:?}"
    );
}
