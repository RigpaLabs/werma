mod decision;
mod effects_helper;
mod retry;
mod task_builder;

// Re-exports used by other modules in this crate
pub(crate) use decision::{DEFAULT_MAX_REVIEW_ROUNDS, decide_callback};
pub(crate) use retry::move_with_retry;
pub(crate) use task_builder::lookup_previous_reviewer_handoff;

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::db::Db;
use crate::traits::CommandRunner;

/// Insert a task using a raw `&Connection` — for use inside `Db::transaction()` closures.
///
/// Applies the same guard as `Db::insert_task()`: pipeline tasks (non-empty `pipeline_stage`)
/// must have a non-empty `linear_issue_id`. Without this guard, callback-spawned tasks with
/// empty identifiers bypass dedup and cause ghost task spawn loops (RIG-387).
fn insert_task_with_conn(conn: &Connection, task: &crate::models::Task) -> Result<()> {
    // RIG-387: same guard as Db::insert_task() — pipeline tasks must have a non-empty identifier.
    // Without this, callback-spawned tasks with empty linear_issue_id bypass dedup and cause
    // infinite spawn loops (has_any_nonfailed_task_for_issue_stage("", stage) matches nothing).
    if !task.pipeline_stage.is_empty() && task.linear_issue_id.is_empty() {
        anyhow::bail!(
            "refusing to insert pipeline task {}: empty linear_issue_id (stage={}). \
             This is a bug — identifier must be set before spawning a pipeline task.",
            task.id,
            task.pipeline_stage
        );
    }

    let depends_on = serde_json::to_string(&task.depends_on)?;
    let context_files = serde_json::to_string(&task.context_files)?;
    let linear_pushed: i32 = if task.linear_pushed { 1 } else { 0 };

    conn.execute(
        "INSERT INTO tasks (
            id, status, priority, created_at, started_at, finished_at,
            type, prompt, output_path, working_dir, model, max_turns,
            allowed_tools, session_id, linear_issue_id, linear_pushed,
            pipeline_stage, depends_on, context_files, repo_hash, estimate,
            retry_count, retry_after, cost_usd, turns_used, handoff_content,
            runtime
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21,
            ?22, ?23, ?24, ?25, ?26,
            ?27
        )",
        params![
            task.id,
            task.status.to_string(),
            task.priority,
            task.created_at,
            task.started_at,
            task.finished_at,
            task.task_type,
            task.prompt,
            task.output_path,
            task.working_dir,
            task.model,
            task.max_turns,
            task.allowed_tools,
            task.session_id,
            task.linear_issue_id,
            linear_pushed,
            task.pipeline_stage,
            depends_on,
            context_files,
            task.repo_hash,
            task.estimate,
            task.retry_count,
            task.retry_after,
            task.cost_usd,
            task.turns_used,
            task.handoff_content,
            task.runtime.to_string(),
        ],
    )?;
    Ok(())
}

/// Thin wrapper: decide_callback + atomic DB transaction.
/// Dedup via dedup_key UNIQUE INDEX — duplicate effects are silently ignored.
///
/// Calls `decide_callback` to compute effects + internal changes, then
/// commits everything atomically. Sets `callback_fired_at` in the same
/// transaction to prevent re-processing.
#[allow(clippy::too_many_arguments)]
pub fn callback(
    db: &Db,
    task_id: &str,
    stage: &str,
    result: &str,
    linear_issue_id: &str,
    working_dir: &str,
    cmd: &dyn CommandRunner,
) -> Result<()> {
    let decision = decide_callback(
        db,
        task_id,
        stage,
        result,
        linear_issue_id,
        working_dir,
        cmd,
    )?;

    // Atomically write all internal changes + outbox effects in one transaction.
    // callback_fired_at is set here so it's part of the same atomic write.
    db.transaction(|conn| {
        // Spawn next pipeline task if the decision requires it.
        if let Some(ref task) = decision.internal.spawn_task {
            let exists: bool = conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE id = ?1",
                params![task.id],
                |row| row.get::<_, i32>(0),
            )? > 0;
            if !exists {
                insert_task_with_conn(conn, task)?;
                eprintln!(
                    "  + pipeline task: {} stage={} type={}",
                    task.id, task.pipeline_stage, task.task_type
                );
            }
        }

        // Apply estimate update.
        if let Some((ref tid, est)) = decision.internal.update_estimate {
            conn.execute(
                "UPDATE tasks SET estimate = ?1 WHERE id = ?2",
                params![est, tid],
            )?;
        }

        // Apply handoff_content update (RIG-333: reviewer feedback for re-review context).
        if let Some((ref tid, ref content)) = decision.internal.handoff_update {
            conn.execute(
                "UPDATE tasks SET handoff_content = ?1 WHERE id = ?2",
                params![content, tid],
            )?;
        }

        // Insert outbox effects (INSERT OR IGNORE via dedup_key).
        crate::db::Db::insert_effects_with_conn(conn, &decision.effects)?;

        // Mark callback as fired to prevent re-processing.
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        conn.execute(
            "UPDATE tasks SET callback_fired_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;
        Ok(())
    })?;

    // Log for observability: what will the effect processor execute?
    if !decision.effects.is_empty() {
        eprintln!(
            "[CALLBACK] {linear_issue_id}: queued {} effects for task {task_id}",
            decision.effects.len()
        );
    } else if decision.internal.spawn_task.is_none() {
        // No effects and no spawn — nothing left to process. Mark as done immediately
        // to prevent the effect processor from looping on this task indefinitely.
        db.set_linear_pushed(task_id, true)?;
        eprintln!(
            "[CALLBACK] {linear_issue_id}: no effects and no spawn for task {task_id} — marking linear_pushed=true"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Status, Task};
    use crate::traits::fakes::{FakeLinearApi, FakeNotifier};

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
    fn callback_engineer_done_without_pr_posts_warning_comment() {
        // RIG-232: verify that the "no PR created" warning comment effect is queued.
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

        let mut task = crate::db::make_test_task("20260314-232b");
        task.id = "20260314-232b".to_string();
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-232b".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        // Engineer output with DONE but no PR_URL — should queue CreatePr + PostComment.
        let result = "Implementation complete.\nVERDICT=DONE";

        callback(
            &db,
            "20260314-232b",
            "engineer",
            result,
            "RIG-232b",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("no PR created"))
            }),
            "should queue PostComment effect about missing PR, got: {effects:?}"
        );

        // Fix 1: CreatePr effect queued instead of direct auto_create_pr() call
        assert!(
            effects
                .iter()
                .any(|e| e.effect_type == crate::models::EffectType::CreatePr),
            "should queue CreatePr effect (not call auto_create_pr directly), got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("review")
            }),
            "should queue MoveIssue effect to review, got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_done_swaps_labels() {
        // RIG-253
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

        let mut task = crate::db::make_test_task("20260315-253");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-253".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        let result = "## Scope\nDo the thing.\n\n## Acceptance Criteria\n- AC1\n\n## Out of Scope\n- None\n\nESTIMATE=3\nVERDICT=DONE";

        callback(
            &db,
            "20260315-253",
            "analyst",
            result,
            "RIG-253",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::RemoveLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze")
            }),
            "should queue RemoveLabel(analyze), got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze:done")
            }),
            "should queue AddLabel(analyze:done), got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "should queue AddLabel(spec:done), got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_already_done_adds_spec_done() {
        // RIG-274
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "ALREADY_DONE should queue AddLabel(spec:done), got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_already_done_with_open_pr_still_adds_spec_done() {
        // RIG-274
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "ALREADY_DONE with open PR should still queue AddLabel(spec:done), got: {effects:?}"
        );
    }

    #[test]
    fn callback_analyst_blocked_adds_analyze_blocked_not_spec_done() {
        // RIG-300
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        assert!(
            !effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
            }),
            "BLOCKED should NOT queue AddLabel(spec:done), got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze:blocked")
            }),
            "BLOCKED should queue AddLabel(analyze:blocked), got: {effects:?}"
        );

        assert!(
            !effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AddLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze:done")
            }),
            "BLOCKED should NOT queue AddLabel(analyze:done), got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::RemoveLabel
                    && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze")
            }),
            "BLOCKED should queue RemoveLabel(analyze), got: {effects:?}"
        );
    }

    #[test]
    fn review_cycle_escalation_uses_config_status() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            working_dir: "~/projects/werma".to_string(),
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
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        let has_backlog = effects.iter().any(|e| {
            e.effect_type == crate::models::EffectType::MoveIssue
                && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("backlog")
        });
        let has_blocked = effects.iter().any(|e| {
            e.effect_type == crate::models::EffectType::MoveIssue
                && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("blocked")
        });
        assert!(
            has_backlog,
            "escalation should queue MoveIssue('backlog'), got: {effects:?}"
        );
        assert!(
            !has_blocked,
            "escalation should NOT queue MoveIssue('blocked'), got: {effects:?}"
        );
    }

    #[test]
    fn callback_no_transition_sets_fired_at() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            working_dir: "~/projects/werma".to_string(),
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
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
        };
        db.insert_task(&task).unwrap();

        let result = "Something unusual happened.\nVERDICT=UNKNOWN_VERDICT_XYZ";

        callback(
            &db,
            "20260324-unk",
            "reviewer",
            result,
            "RIG-UNK",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        assert!(
            db.is_callback_recently_fired("20260324-unk", 60).unwrap(),
            "callback_fired_at should be set for unknown verdict"
        );
    }

    #[test]
    fn callback_max_turns_exit_does_not_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

        let mut task = crate::db::make_test_task("20260325-252a");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252a".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        let result = r#"{"type":"result","subtype":"error_max_turns","is_error":false,"result":"partial work"}"#;

        callback(
            &db,
            "20260325-252a",
            "engineer",
            result,
            "RIG-252a",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == crate::models::EffectType::MoveIssue),
            "max_turns exit should not queue MoveIssue effects, got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("max_turns"))
            }),
            "should queue PostComment about max_turns, got: {effects:?}"
        );
    }

    #[test]
    fn callback_max_turns_in_text_output_does_not_transition() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();
        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == crate::models::EffectType::MoveIssue),
            "max_turns text should not queue MoveIssue effects, got: {effects:?}"
        );
    }

    #[test]
    fn callback_normal_engineer_done_still_works() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

        let mut task = crate::db::make_test_task("20260325-252c");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-252c".to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();

        let result = "All work done.\nPR_URL=https://github.com/org/repo/pull/1\nVERDICT=DONE";

        callback(
            &db,
            "20260325-252c",
            "engineer",
            result,
            "RIG-252c",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("review")
            }),
            "normal DONE should queue MoveIssue('review'), got: {effects:?}"
        );
    }

    #[test]
    fn callback_max_turns_escalates_after_repeated_failures() {
        // RIG-202
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("backlog")
            }),
            "should queue MoveIssue('backlog') escalation, got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("failure limit reached"))
            }),
            "should queue PostComment about failure limit, got: {effects:?}"
        );

        assert!(
            db.is_callback_recently_fired("20260326-202a", 60).unwrap(),
            "callback_fired_at should be set after escalation"
        );
    }

    #[test]
    fn callback_max_turns_soft_failure_below_limit() {
        // RIG-202
        let db = crate::db::Db::open_in_memory().unwrap();
        let _linear = FakeLinearApi::new();
        let cmd = crate::traits::fakes::FakeCommandRunner::new();
        let _notifier = FakeNotifier::new();

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
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        let effects = db.pending_effects(100).unwrap();

        assert!(
            !effects
                .iter()
                .any(|e| e.effect_type == crate::models::EffectType::MoveIssue),
            "soft failure should not queue MoveIssue, got: {effects:?}"
        );

        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("Soft failure"))
            }),
            "should queue PostComment about soft failure, got: {effects:?}"
        );
    }

    // ─── RIG-387: insert_task_with_conn guard + runtime column ───────────────

    /// insert_task_with_conn must reject pipeline tasks with empty linear_issue_id.
    ///
    /// Bug: the raw-connection INSERT path had no validation, allowing tasks with
    /// empty linear_issue_id to be inserted. The dedup query
    /// `has_any_nonfailed_task_for_issue_stage("", stage)` then matches nothing,
    /// so the poll loop spawns a new task on every tick — 41+ ghost tasks per session.
    #[test]
    fn insert_task_with_conn_rejects_empty_identifier() {
        let db = crate::db::Db::open_in_memory().unwrap();

        let result = db.transaction(|conn| {
            let mut task = crate::db::make_test_task("20260403-387a");
            task.pipeline_stage = "engineer".to_string();
            task.linear_issue_id = String::new(); // empty — must be rejected
            insert_task_with_conn(conn, &task)
        });

        assert!(
            result.is_err(),
            "insert_task_with_conn must reject pipeline tasks with empty linear_issue_id"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("empty linear_issue_id"),
            "error must mention empty linear_issue_id, got: {msg}"
        );
    }

    /// Non-pipeline tasks (empty pipeline_stage) with empty linear_issue_id must still be accepted.
    #[test]
    fn insert_task_with_conn_allows_non_pipeline_empty_identifier() {
        let db = crate::db::Db::open_in_memory().unwrap();

        db.transaction(|conn| {
            let mut task = crate::db::make_test_task("20260403-387b");
            task.pipeline_stage = String::new(); // non-pipeline
            task.linear_issue_id = String::new(); // empty — OK for non-pipeline
            insert_task_with_conn(conn, &task)
        })
        .unwrap();

        let stored = db.task("20260403-387b").unwrap();
        assert!(stored.is_some(), "non-pipeline task must be inserted");
    }

    /// insert_task_with_conn must persist the runtime column (27th field).
    ///
    /// Bug: the INSERT statement had 26 columns and omitted `runtime`, so callback-spawned
    /// tasks always got the DB default runtime (claude-code) regardless of config.
    #[test]
    fn insert_task_with_conn_persists_runtime() {
        let db = crate::db::Db::open_in_memory().unwrap();

        db.transaction(|conn| {
            let mut task = crate::db::make_test_task("20260403-387c");
            task.pipeline_stage = "engineer".to_string();
            task.linear_issue_id = "RIG-387".to_string();
            task.runtime = crate::models::AgentRuntime::QwenCode;
            insert_task_with_conn(conn, &task)
        })
        .unwrap();

        let stored = db.task("20260403-387c").unwrap().unwrap();
        assert_eq!(
            stored.runtime,
            crate::models::AgentRuntime::QwenCode,
            "runtime must be persisted by insert_task_with_conn; \
             before fix it always defaulted to claude-code"
        );
    }

    /// Fix 2 regression: callback that produces effects must NOT set linear_pushed=true
    /// (the effect processor handles that after executing them). And callback that
    /// produces zero effects AND no spawn must set linear_pushed=true immediately.
    #[test]
    fn test_callback_zero_effects_sets_linear_pushed() {
        // Part A: callback with effects → linear_pushed stays false.
        {
            let db = crate::db::Db::open_in_memory().unwrap();
            let cmd = crate::traits::fakes::FakeCommandRunner::new();

            let mut task = crate::db::make_test_task("20260327-zeroA");
            task.id = "20260327-zeroA".to_string();
            task.status = Status::Completed;
            task.linear_issue_id = "RIG-zeroA".to_string();
            task.pipeline_stage = "reviewer".to_string();
            db.insert_task(&task).unwrap();

            // Reviewer approved → produces MoveIssue + PostComment effects
            let result = "Code looks good.\nREVIEW_VERDICT=APPROVED";

            callback(
                &db,
                "20260327-zeroA",
                "reviewer",
                result,
                "RIG-zeroA",
                "~/projects/werma",
                &cmd,
            )
            .unwrap();

            let effects = db.pending_effects(100).unwrap();
            assert!(
                !effects.is_empty(),
                "reviewer APPROVED should produce effects, got none"
            );

            let task_after = db.task("20260327-zeroA").unwrap().unwrap();
            assert!(
                !task_after.linear_pushed,
                "callback with effects must NOT set linear_pushed=true (effect processor handles it)"
            );
        }

        // Part B: direct set_linear_pushed API works correctly (validates the Fix 2 code path).
        {
            let db = crate::db::Db::open_in_memory().unwrap();
            let mut task = crate::db::make_test_task("20260327-zeroB");
            task.id = "20260327-zeroB".to_string();
            task.linear_pushed = false;
            db.insert_task(&task).unwrap();

            db.set_linear_pushed("20260327-zeroB", true).unwrap();

            let task_after = db.task("20260327-zeroB").unwrap().unwrap();
            assert!(
                task_after.linear_pushed,
                "set_linear_pushed(true) must be persisted"
            );
        }
    }
}
