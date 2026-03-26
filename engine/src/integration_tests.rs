use crate::db::{Db, make_test_task};
use crate::models::{EffectType, Status};
use crate::pipeline::executor::{callback, poll};
use crate::traits::fakes::{FakeCommandRunner, FakeLinearApi, FakeNotifier};
use serde_json::json;

/// Helper: assert that a pending effect of the given type with the given target_status payload exists.
fn assert_move_effect(db: &Db, target_status: &str) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        effects.iter().any(|e| {
            e.effect_type == EffectType::MoveIssue
                && e.payload.get("target_status").and_then(|v| v.as_str()) == Some(target_status)
        }),
        "expected MoveIssue effect with target_status={target_status}, got: {effects:?}"
    );
}

/// Helper: assert no MoveIssue effects exist with a given target_status.
fn assert_no_move_effect(db: &Db, target_status: &str) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        !effects.iter().any(|e| {
            e.effect_type == EffectType::MoveIssue
                && e.payload.get("target_status").and_then(|v| v.as_str()) == Some(target_status)
        }),
        "unexpected MoveIssue effect with target_status={target_status}, got: {effects:?}"
    );
}

/// Helper: assert a PostComment effect exists whose body contains the given substring.
fn assert_comment_effect(db: &Db, body_contains: &str) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        effects.iter().any(|e| {
            e.effect_type == EffectType::PostComment
                && e.payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .is_some_and(|b| b.contains(body_contains))
        }),
        "expected PostComment effect containing {body_contains:?}, got: {effects:?}"
    );
}

/// Helper: assert no MoveIssue effects exist at all.
fn assert_no_move_effects(db: &Db) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        !effects
            .iter()
            .any(|e| e.effect_type == EffectType::MoveIssue),
        "expected no MoveIssue effects, got: {effects:?}"
    );
}

/// Helper: assert an AddLabel effect exists with the given label.
fn assert_add_label_effect(db: &Db, label: &str) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        effects.iter().any(|e| {
            e.effect_type == EffectType::AddLabel
                && e.payload.get("label").and_then(|v| v.as_str()) == Some(label)
        }),
        "expected AddLabel effect with label={label:?}, got: {effects:?}"
    );
}

/// Helper: assert no AddLabel effect exists with the given label.
fn assert_no_add_label_effect(db: &Db, label: &str) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        !effects.iter().any(|e| {
            e.effect_type == EffectType::AddLabel
                && e.payload.get("label").and_then(|v| v.as_str()) == Some(label)
        }),
        "unexpected AddLabel effect with label={label:?}, got: {effects:?}"
    );
}

/// Helper: assert a RemoveLabel effect exists with the given label.
fn assert_remove_label_effect(db: &Db, label: &str) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        effects.iter().any(|e| {
            e.effect_type == EffectType::RemoveLabel
                && e.payload.get("label").and_then(|v| v.as_str()) == Some(label)
        }),
        "expected RemoveLabel effect with label={label:?}, got: {effects:?}"
    );
}

/// Helper: assert an AttachUrl effect exists for the given url.
fn assert_attach_url_effect(db: &Db, url_contains: &str) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        effects.iter().any(|e| {
            e.effect_type == EffectType::AttachUrl
                && e.payload
                    .get("url")
                    .and_then(|v| v.as_str())
                    .is_some_and(|u| u.contains(url_contains))
        }),
        "expected AttachUrl effect containing url={url_contains:?}, got: {effects:?}"
    );
}

/// Helper: assert an UpdateEstimate effect exists with the given estimate value.
fn assert_update_estimate_effect(db: &Db, estimate: i64) {
    let effects = db.pending_effects(100).unwrap();
    assert!(
        effects.iter().any(|e| {
            e.effect_type == EffectType::UpdateEstimate
                && e.payload.get("estimate").and_then(|v| v.as_i64()) == Some(estimate)
        }),
        "expected UpdateEstimate effect with estimate={estimate}, got: {effects:?}"
    );
}

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

    // callback() writes effects to DB outbox — does NOT call linear directly.
    // The MoveIssue effect will be executed by the effect processor (Task 4).
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

    // Verify the MoveIssue effect is queued with the correct target status.
    assert_move_effect(&db, "review");

    // Linear is NOT called during callback — effects are deferred to processor.
    assert!(
        linear.move_calls.borrow().is_empty(),
        "linear should not be called during callback"
    );
}

// ─── Test 2: callback_move_failure_returns_error ────────────────────────────
// With the outbox pattern, callback() always succeeds — it writes effects to DB.
// Move failures are handled by the effect processor with its own retry logic.
// The old "retry" behavior is now owned by the processor (Task 4).

#[test]
fn callback_move_failure_returns_error() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // fail_next_n_moves no longer affects callback — moves are deferred.
    linear.fail_next_n_moves(3);

    let mut task = make_test_task("20260313-101");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-201".to_string();
    task.pipeline_stage = "engineer".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Done\nVERDICT=DONE";

    // callback() always succeeds — effects are written to outbox for deferred execution.
    let ok = callback(
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
        ok.is_ok(),
        "callback should succeed: effects are durable outbox entries"
    );

    // MoveIssue effect should be queued for the processor.
    assert_move_effect(&db, "review");

    // callback_fired_at IS set (transaction succeeded).
    assert!(
        db.is_callback_recently_fired("20260313-101", 60).unwrap(),
        "callback_fired_at should be set after successful callback"
    );
}

// ─── Test 3: poll_no_duplicate_while_callback_pending (RIG-209) ─────────────
// Completed task with linear_pushed=false (callback hasn't fired yet) should
// block the poller from creating a duplicate — prevents RIG-209 race.

#[test]
fn poll_no_duplicate_while_callback_pending() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    // Pre-insert a completed but unpushed task (callback pending)
    let mut task = make_test_task("20260313-102");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-202".to_string();
    task.pipeline_stage = "engineer".to_string();
    task.linear_pushed = false; // callback hasn't fired yet
    db.insert_task(&task).unwrap();

    // Issue still at "in_progress" because callback hasn't moved it
    let issue = fake_issue(
        "uuid-202",
        "RIG-202",
        "Test issue",
        &["Feature", "repo:werma"],
    );
    linear.set_issues_for_status("in_progress", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    // Should NOT create a new task — unpushed completed task blocks it
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

// ─── Test 3b: poll_allows_respawn_after_rejection_cycle (RIG-277) ───────────
// After reviewer rejection, the issue returns to In Progress with the old
// engineer task completed+pushed. Poller should spawn a new engineer task.

#[test]
fn poll_allows_respawn_after_rejection_cycle() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    ensure_working_dir();

    // Engineer #1 completed and callback already processed (pushed=true)
    let mut eng1 = make_test_task("20260324-101");
    eng1.status = Status::Completed;
    eng1.linear_issue_id = "RIG-277".to_string();
    eng1.pipeline_stage = "engineer".to_string();
    eng1.linear_pushed = true;
    db.insert_task(&eng1).unwrap();

    // Reviewer also completed and processed
    let mut rev1 = make_test_task("20260324-102");
    rev1.status = Status::Completed;
    rev1.linear_issue_id = "RIG-277".to_string();
    rev1.pipeline_stage = "reviewer".to_string();
    rev1.linear_pushed = true;
    db.insert_task(&rev1).unwrap();

    // Issue is back at "in_progress" after reviewer rejection
    let issue = fake_issue(
        "uuid-277",
        "RIG-277",
        "Fix poller dedup",
        &["Feature", "repo:werma"],
    );
    linear.set_issues_for_status("in_progress", vec![issue]);

    poll(&db, &linear, &cmd).unwrap();

    // Should create a NEW engineer task (RIG-277 fix)
    let tasks = db
        .tasks_by_linear_issue("RIG-277", Some("engineer"), false)
        .unwrap();
    assert_eq!(
        tasks.len(),
        2,
        "expected 2 engineer tasks (original + respawn), got {}",
        tasks.len()
    );

    // The new task should be pending
    let new_task = tasks.iter().find(|t| t.id != "20260324-101").unwrap();
    assert_eq!(new_task.status, Status::Pending);
    assert_eq!(new_task.pipeline_stage, "engineer");
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

// ─── Test 7: callback succeeds with effects queued (RIG-211) ─────────────────
// With outbox pattern, callback always succeeds — move retry is handled by the
// effect processor. callback() dequeues nothing; it only queues effects.

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

    // fail_next_n_moves no longer affects callback — deferred to processor.
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
    assert!(ok.is_ok(), "callback should always succeed: {ok:?}");

    // MoveIssue effect should be queued.
    assert_move_effect(&db, "review");

    // AttachUrl effect should be queued for the PR.
    assert_attach_url_effect(&db, "/pull/99");
}

// ─── Test 7b: callback always succeeds with outbox (RIG-211) ─────────────────
// With outbox pattern, callback() always succeeds — retry exhaustion happens
// in the effect processor. The callback just enqueues effects atomically.

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

    // fail_next_n_moves no longer affects callback — deferred to processor.
    linear.fail_next_n_moves(3);
    let ok = callback(
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
    assert!(ok.is_ok(), "callback always succeeds: effects are durable");

    // callback_fired_at IS set after success.
    assert!(
        db.is_callback_recently_fired("20260313-301", 60).unwrap(),
        "callback_fired_at should be set after successful callback"
    );

    // MoveIssue effect should be in the outbox for the processor.
    assert_move_effect(&db, "review");
}

// ─── Test 7c: callback with outbox — dedup guard via callback_fired_at ───────
// With outbox pattern, callback() always succeeds on first call.
// The dedup guard (callback_fired_at) prevents duplicate effect insertion.
// Second call with fired_at set → no effects written.

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

    // First callback: succeeds, sets callback_fired_at, queues effects.
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
    assert!(ok.is_ok(), "callback should succeed: {ok:?}");

    // callback_fired_at IS set.
    assert!(
        db.is_callback_recently_fired("20260313-302", 60).unwrap(),
        "callback_fired_at should be set after first success"
    );

    // MoveIssue effect queued.
    assert_move_effect(&db, "ready");

    let effects_count = db.pending_effects(100).unwrap().len();

    // Second callback: dedup guard fires → no new effects.
    let ok2 = callback(
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
        ok2.is_ok(),
        "second callback should succeed (dedup guard): {ok2:?}"
    );

    let effects_count_after = db.pending_effects(100).unwrap().len();
    assert_eq!(
        effects_count, effects_count_after,
        "dedup guard should prevent new effects on second call"
    );
}

// ─── Test 7d: callback with outbox — no notifications during callback ─────────
// With outbox pattern, notifications are sent by the effect processor (Task 4),
// not by callback(). callback() always succeeds and notifier is not invoked.

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

    // fail_next_n_moves no longer affects callback — notifier is not called during callback.
    linear.fail_next_n_moves(3);
    let ok = callback(
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
    assert!(ok.is_ok(), "callback always succeeds: {ok:?}");

    // Notifications are deferred to the effect processor — not sent during callback.
    assert!(
        notifier.macos_calls.borrow().is_empty(),
        "notifier should NOT be called during callback (deferred to processor)"
    );
    assert!(
        notifier.slack_calls.borrow().is_empty(),
        "notifier should NOT be called during callback (deferred to processor)"
    );

    // MoveIssue effect should be queued.
    assert_move_effect(&db, "review");
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

    // Empty result queues a PostComment effect (no MoveIssue).
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

    // No MoveIssue effects.
    assert_no_move_effects(&db);

    // PostComment effect should be queued about empty output.
    assert_comment_effect(&db, "empty output");
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

    // Unknown stage returns Err from decide_callback → callback propagates Err.
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

    assert!(result.is_err(), "unknown stage should return Err");
    assert_no_move_effects(&db);
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

    // UpdateEstimate effect should be queued (processor calls linear.update_estimate).
    assert_update_estimate_effect(&db, 5);

    // MoveIssue effect should be queued with target "todo".
    assert_move_effect(&db, "todo");
}

// ─── Test 20b: callback_analyst_adds_done_label ──────────────────────────────

#[test]
fn callback_analyst_adds_done_label() {
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-219b-done");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-219b-done".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Analysis\nSpec here.\n\nESTIMATE=3\nVERDICT=DONE";

    callback(
        &db,
        "20260313-219b-done",
        "analyst",
        result,
        "RIG-219b-done",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // AddLabel effect should be queued for analyze:done.
    assert_add_label_effect(&db, "analyze:done");

    // MoveIssue effect should be queued with target "todo".
    assert_move_effect(&db, "todo");
}

// ─── Test 20c: callback_analyst_blocked_adds_blocked_label ───────────────────

#[test]
fn callback_analyst_blocked_adds_blocked_label() {
    // RIG-300: BLOCKED verdict should add analyze:blocked (not analyze:done)
    let db = Db::open_in_memory().unwrap();
    let linear = FakeLinearApi::new();
    let cmd = FakeCommandRunner::new();

    let mut task = make_test_task("20260313-219b-blk");
    task.status = Status::Completed;
    task.linear_issue_id = "RIG-219B-blk".to_string();
    task.pipeline_stage = "analyst".to_string();
    db.insert_task(&task).unwrap();

    let result = "## Analysis\nBlocked on external dependency.\n\nVERDICT=BLOCKED";

    callback(
        &db,
        "20260313-219b-blk",
        "analyst",
        result,
        "RIG-219B-blk",
        "~/projects/rigpa/werma",
        &linear,
        &cmd,
        &FakeNotifier::new(),
    )
    .unwrap();

    // AddLabel effect for analyze:blocked.
    assert_add_label_effect(&db, "analyze:blocked");

    // No AddLabel effect for analyze:done.
    assert_no_add_label_effect(&db, "analyze:done");

    // RemoveLabel effect for trigger label.
    assert_remove_label_effect(&db, "analyze");
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

    // Reviewer output without VERDICT= — should queue a PostComment effect.
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

    // No MoveIssue effects — missing verdict does not transition.
    assert_no_move_effects(&db);

    // PostComment effect with "no verdict" warning.
    assert_comment_effect(&db, "no verdict");
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

    // No MoveIssue effect to "done" — blocked by open PR.
    assert_no_move_effect(&db, "done");

    // PostComment effect explaining the block.
    assert_comment_effect(&db, "ALREADY_DONE blocked");
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

    // AttachUrl effect queued for the PR.
    assert_attach_url_effect(&db, "/pull/42");

    // MoveIssue effect queued for "review".
    assert_move_effect(&db, "review");
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

    // No PR_URL in output — triggers auto_create_pr flow.
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

    // AttachUrl effect queued for the auto-created PR.
    assert_attach_url_effect(&db, "/pull/99");

    // MoveIssue effect queued for "review".
    assert_move_effect(&db, "review");
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

    // No PR_URL in output and auto_create_pr returns None (branch=main safety check).
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

    // PostComment effect queued with "no PR created" warning.
    assert_comment_effect(&db, "no PR created");
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

    // MoveIssue effect queued for "in_progress" (rejected → in_progress per config).
    assert_move_effect(&db, "in_progress");

    // A new engineer task should be spawned (stored in DB atomically in the transaction).
    let engineer_tasks = db
        .tasks_by_linear_issue("RIG-226", Some("engineer"), false)
        .unwrap();
    assert_eq!(
        engineer_tasks.len(),
        1,
        "rejected review should spawn engineer task"
    );
    assert_eq!(engineer_tasks[0].pipeline_stage, "engineer");

    // Spawned task uses handoff_content (no filesystem dependency).
    assert!(
        !engineer_tasks[0].handoff_content.is_empty(),
        "spawned task should have handoff_content set"
    );
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

    // MoveIssue effect queued for "backlog" (reviewer cycle limit escalation).
    assert_move_effect(&db, "backlog");

    // No new engineer task should be spawned.
    let engineer_tasks = db
        .tasks_by_linear_issue("RIG-227", Some("engineer"), false)
        .unwrap();
    assert!(
        engineer_tasks.is_empty(),
        "review cycle limit should NOT spawn new engineer"
    );
}

// ─── Test 27b: escalation with outbox — move effect queued ───────────────────
// With outbox pattern, move retry is handled by processor. callback() always
// succeeds and queues the MoveIssue effect regardless of fake move failures.

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

    let result = "## Review\nStill broken.\n\nVERDICT=REJECTED";

    // fail_next_n_moves no longer affects callback — deferred to processor.
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

    // MoveIssue effect queued for "backlog" (escalation from cycle limit).
    assert_move_effect(&db, "backlog");
}
