//! Full-cycle pipeline integration tests (RIG-229, RIG-230, RIG-231).
//!
//! These tests simulate multi-stage pipeline flows using StatefulFakeLinearApi
//! to maintain real issue state across poll() and callback() calls.

#[cfg(test)]
mod tests {
    use crate::db::{Db, make_test_task};
    use crate::models::Status;
    use crate::pipeline::executor::{callback, poll};
    use crate::pipeline::loader::load_from_str;
    use crate::traits::fakes::{FakeCommandRunner, FakeNotifier, StatefulFakeLinearApi};

    /// Ensure `~/projects/werma` exists for validate_working_dir on CI.
    fn ensure_working_dir() {
        if let Some(home) = dirs::home_dir() {
            let dir = home.join("projects/werma");
            let _ = std::fs::create_dir_all(dir);
        }
    }

    // ─── RIG-229: Analyst → Engineer → Reviewer full pipeline ──────────────────

    /// Test the analyst → engineer → reviewer full pipeline cycle using outbox pattern.
    ///
    /// With the transactional outbox, callback() does NOT call Linear API directly.
    /// Instead it writes effects to the outbox for deferred execution by the processor.
    /// This test verifies:
    /// 1. poll() still triggers task creation and calls linear directly (poll is synchronous)
    /// 2. callback() queues correct effects for each stage
    /// 3. spawn_task is written atomically to DB (reviewer task created by engineer callback)
    #[test]
    fn rig229_analyst_engineer_reviewer_full_pipeline() {
        ensure_working_dir();
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.add_issue(
            "uuid-rig229",
            "RIG-229",
            "Full pipeline test",
            "Test the full pipeline cycle",
            "backlog",
            vec!["analyze".to_string(), "repo:werma".to_string()],
        );

        // Step 1: Poll — creates analyst task (label-based trigger, calls linear synchronously)
        poll(&db, &linear, &cmd).unwrap();

        let analyst_tasks = db
            .tasks_by_linear_issue("RIG-229", Some("analyst"), false)
            .unwrap();
        assert_eq!(analyst_tasks.len(), 1, "analyst task should be created");
        assert_eq!(analyst_tasks[0].status, Status::Pending);

        assert!(
            !linear
                .issue_labels("RIG-229")
                .contains(&"analyze".to_string()),
            "trigger label should be removed by poll"
        );

        // Step 2: Analyst callback — queues effects (MoveIssue → todo, etc.)
        db.set_task_status(&analyst_tasks[0].id, Status::Completed)
            .unwrap();

        let analyst_output = "## Scope\nThis feature needs X and Y.\n\n## Acceptance Criteria\n- Feature works\n\n## Out of Scope\n- None\n\nESTIMATE=3\nVERDICT=DONE";

        callback(
            &db,
            &analyst_tasks[0].id,
            "analyst",
            analyst_output,
            "RIG-229",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        // Verify MoveIssue("todo") effect is in outbox.
        let effects_after_analyst = db.pending_effects(100).unwrap();
        assert!(
            effects_after_analyst.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("todo")
            }),
            "analyst DONE should queue MoveIssue('todo'), got: {effects_after_analyst:?}"
        );

        // Analyst does NOT spawn engineer (no spawn in analyst config).
        let engineer_tasks_pre = db
            .tasks_by_linear_issue("RIG-229", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            engineer_tasks_pre.len(),
            0,
            "engineer should not be spawned by analyst"
        );

        // Step 3: Simulate processor executing the move (then human gate).
        // In production: processor calls linear. In test: move directly.
        linear.move_issue_by_name_direct("RIG-229", "in_progress");

        // Step 4: Poll — engineer picks up in_progress issue.
        poll(&db, &linear, &cmd).unwrap();

        let engineer_tasks = db
            .tasks_by_linear_issue("RIG-229", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            engineer_tasks.len(),
            1,
            "engineer task should be created by poll"
        );
        assert_eq!(engineer_tasks[0].pipeline_stage, "engineer");

        // Step 5: Engineer callback — queues MoveIssue("review") + AttachUrl + spawns reviewer.
        db.set_task_status(&engineer_tasks[0].id, Status::Completed)
            .unwrap();

        let engineer_output = "## Implementation\nDone.\n\nPR_URL=https://github.com/RigpaLabs/werma/pull/99\nVERDICT=DONE";

        callback(
            &db,
            &engineer_tasks[0].id,
            "engineer",
            engineer_output,
            "RIG-229",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        // Reviewer task spawned atomically in DB.
        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-229", Some("reviewer"), false)
            .unwrap();
        assert_eq!(
            reviewer_tasks.len(),
            1,
            "reviewer task should be spawned by engineer callback"
        );

        // MoveIssue("review") effect queued.
        let effects_after_eng: Vec<_> = db.pending_effects(100).unwrap();
        assert!(
            effects_after_eng.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("review")
            }),
            "engineer DONE should queue MoveIssue('review'), got: {effects_after_eng:?}"
        );

        // AttachUrl effect queued for PR.
        assert!(
            effects_after_eng.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AttachUrl
                    && e.payload
                        .get("url")
                        .and_then(|v| v.as_str())
                        .is_some_and(|u| u.contains("/pull/99"))
            }),
            "engineer DONE should queue AttachUrl, got: {effects_after_eng:?}"
        );

        // Step 6: Reviewer callback — queues MoveIssue("ready").
        db.set_task_status(&reviewer_tasks[0].id, Status::Completed)
            .unwrap();

        let reviewer_output = "## Review\n- All good!\nREVIEW_VERDICT=APPROVED";

        callback(
            &db,
            &reviewer_tasks[0].id,
            "reviewer",
            reviewer_output,
            "RIG-229",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        // MoveIssue("ready") effect queued.
        let effects_after_rev = db.pending_effects(100).unwrap();
        assert!(
            effects_after_rev.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("ready")
            }),
            "reviewer APPROVED should queue MoveIssue('ready'), got: {effects_after_rev:?}"
        );
    }

    // ─── RIG-230: Engineer callback with outbox — dedup guard + spawn ────────────

    /// Test that callback() successfully queues effects on first call,
    /// sets callback_fired_at, and spawns reviewer task atomically.
    /// With outbox pattern, callback always succeeds — retry is processor's job.
    #[test]
    fn rig230_callback_failure_retry_success() {
        ensure_working_dir();
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.add_issue(
            "uuid-rig230",
            "RIG-230",
            "Retry test",
            "Test callback dedup cycle",
            "in_progress",
            vec!["repo:werma".to_string()],
        );

        // Step 1: Poll creates engineer task.
        poll(&db, &linear, &cmd).unwrap();

        let engineer_tasks = db
            .tasks_by_linear_issue("RIG-230", Some("engineer"), false)
            .unwrap();
        assert_eq!(engineer_tasks.len(), 1, "poll should create engineer task");
        let task_id = &engineer_tasks[0].id;

        db.set_task_status(task_id, Status::Completed).unwrap();

        let engineer_output = "## Implementation\nDone.\n\nPR_URL=https://github.com/RigpaLabs/werma/pull/230\nVERDICT=DONE";

        // fail_next_n_moves has no effect on callback — effects are durable outbox entries.
        linear.fail_next_n_moves(3);

        // Step 2: First callback — always succeeds, queues effects.
        let ok = callback(
            &db,
            task_id,
            "engineer",
            engineer_output,
            "RIG-230",
            "~/projects/werma",
            &cmd,
        );
        assert!(ok.is_ok(), "callback should always succeed: {ok:?}");

        // callback_fired_at IS set.
        assert!(
            db.is_callback_recently_fired(task_id, 60).unwrap(),
            "callback_fired_at should be set after first callback"
        );

        // MoveIssue("review") effect queued.
        let effects = db.pending_effects(100).unwrap();
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("review")
            }),
            "should queue MoveIssue('review'), got: {effects:?}"
        );

        // AttachUrl effect for PR.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::AttachUrl
                    && e.payload
                        .get("url")
                        .and_then(|v| v.as_str())
                        .is_some_and(|u| u.contains("/pull/230"))
            }),
            "should queue AttachUrl for /pull/230, got: {effects:?}"
        );

        // Reviewer task spawned atomically.
        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-230", Some("reviewer"), false)
            .unwrap();
        assert_eq!(
            reviewer_tasks.len(),
            1,
            "reviewer should be spawned by engineer callback"
        );

        // Step 3: Second callback — dedup guard blocks (no new effects).
        let effects_count = db.pending_effects(100).unwrap().len();

        let ok2 = callback(
            &db,
            task_id,
            "engineer",
            engineer_output,
            "RIG-230",
            "~/projects/werma",
            &cmd,
        );
        assert!(
            ok2.is_ok(),
            "second callback should succeed (dedup guard): {ok2:?}"
        );

        let effects_count_after = db.pending_effects(100).unwrap().len();
        assert_eq!(
            effects_count, effects_count_after,
            "dedup guard prevents duplicate effects"
        );

        // poll() should NOT create duplicate engineer or reviewer tasks.
        // Simulate processor moving issue to "review" so poll sees the right state.
        linear.move_issue_by_name_direct("RIG-230", "review");
        poll(&db, &linear, &cmd).unwrap();

        let engineer_tasks_after = db
            .tasks_by_linear_issue("RIG-230", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            engineer_tasks_after.len(),
            1,
            "poll should not duplicate engineer task"
        );

        let reviewer_tasks_after = db
            .tasks_by_linear_issue("RIG-230", Some("reviewer"), false)
            .unwrap();
        assert_eq!(
            reviewer_tasks_after.len(),
            1,
            "poll should not duplicate reviewer task"
        );
    }

    // ─── RIG-231: Dedup guards ───────────────────────────────────────────────────

    /// Test label removal dedup: after analyst triggers, re-polling should not
    /// create another analyst task (label was removed, so get_issues_by_label returns empty).
    #[test]
    fn rig231_label_removal_prevents_respawn() {
        ensure_working_dir();
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        linear.add_issue(
            "uuid-rig231a",
            "RIG-231a",
            "Label dedup test",
            "desc",
            "backlog",
            vec!["analyze".to_string(), "repo:werma".to_string()],
        );

        // First poll: creates analyst task and removes label
        poll(&db, &linear, &cmd).unwrap();

        let tasks_after_first = db
            .tasks_by_linear_issue("RIG-231a", Some("analyst"), false)
            .unwrap();
        assert_eq!(tasks_after_first.len(), 1);

        // Second poll: label is gone, issue is no longer in backlog (on_start moved it)
        // So no new task should be created
        poll(&db, &linear, &cmd).unwrap();

        let tasks_after_second = db
            .tasks_by_linear_issue("RIG-231a", Some("analyst"), false)
            .unwrap();
        assert_eq!(
            tasks_after_second.len(),
            1,
            "second poll should not create duplicate analyst task"
        );
    }

    /// Test status dedup: engineer stage only polls "in_progress". If issue moves to
    /// "review" (after engineer completes), poll should not create another engineer task.
    #[test]
    fn rig231_status_dedup_engineer_not_re_polled_in_review() {
        ensure_working_dir();
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        linear.add_issue(
            "uuid-rig231b",
            "RIG-231b",
            "Status dedup test",
            "desc",
            "review",
            vec!["repo:werma".to_string()],
        );

        // Poll: issue is in "review" — engineer polls "in_progress", so no task created
        // Reviewer polls "review" — should create a reviewer task
        poll(&db, &linear, &cmd).unwrap();

        let engineer_tasks = db
            .tasks_by_linear_issue("RIG-231b", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            engineer_tasks.len(),
            0,
            "engineer should not be created for issue in review"
        );

        // Reviewer task SHOULD be created since issue is in "review"
        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-231b", Some("reviewer"), false)
            .unwrap();
        assert_eq!(reviewer_tasks.len(), 1, "reviewer task should be created");
    }

    /// Test review cycle limit: after max_review_rounds rejections, callback queues MoveIssue("backlog").
    #[test]
    fn rig231_review_cycle_limit_escalates_to_blocked() {
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.add_issue(
            "uuid-rig231c",
            "RIG-231c",
            "Review cycle limit test",
            "desc",
            "review",
            vec![],
        );

        let _config = load_from_str(include_str!("../pipelines/default.yaml"), "<test>").unwrap();

        for i in 0..3 {
            let mut task = make_test_task(&format!("20260314-231c-{i:03}"));
            task.status = Status::Completed;
            task.linear_issue_id = "RIG-231c".to_string();
            task.pipeline_stage = "reviewer".to_string();
            db.insert_task(&task).unwrap();
        }

        let mut reviewer_task = make_test_task("20260314-231c-004");
        reviewer_task.status = Status::Completed;
        reviewer_task.linear_issue_id = "RIG-231c".to_string();
        reviewer_task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&reviewer_task).unwrap();

        let reviewer_output = "## Review\n- Too many issues.\nREVIEW_VERDICT=REJECTED";

        callback(
            &db,
            "20260314-231c-004",
            "reviewer",
            reviewer_output,
            "RIG-231c",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        // With outbox pattern, callback() does NOT call linear directly.
        // Verify the MoveIssue("backlog") effect is queued (processor will execute it).
        let effects = db.pending_effects(100).unwrap();
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("backlog")
            }),
            "review cycle limit should queue MoveIssue('backlog'), got: {effects:?}"
        );

        // No new engineer task should be spawned (cycle limit preempts spawn).
        let engineer_tasks = db
            .tasks_by_linear_issue("RIG-231c", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            engineer_tasks.len(),
            0,
            "no engineer task should be spawned after limit"
        );
    }

    /// Test cross-stage reviewer dedup: if a reviewer task already exists (regardless of
    /// stage polling), poll should not create a second reviewer.
    #[test]
    fn rig231_reviewer_cross_stage_dedup() {
        ensure_working_dir();
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        linear.add_issue(
            "uuid-rig231d",
            "RIG-231d",
            "Cross-stage dedup test",
            "desc",
            "review",
            vec!["repo:werma".to_string()],
        );

        // Insert an existing active reviewer task
        let mut existing = make_test_task("20260314-231d-001");
        existing.status = Status::Pending;
        existing.linear_issue_id = "RIG-231d".to_string();
        existing.pipeline_stage = "reviewer".to_string();
        db.insert_task(&existing).unwrap();

        // Poll: should NOT create a second reviewer task (dedup guard)
        poll(&db, &linear, &cmd).unwrap();

        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-231d", Some("reviewer"), false)
            .unwrap();
        assert_eq!(
            reviewer_tasks.len(),
            1,
            "should not create duplicate reviewer when active one exists"
        );
    }

    // ─── RIG-335: Engineer no PR → CreatePr blocking → reviewer deferred ────

    /// Full-cycle test: engineer completes without PR_URL → callback defers reviewer.
    ///
    /// This is the RIG-325/RIG-334 failure mode tested end-to-end:
    /// 1. Engineer outputs VERDICT=DONE but no PR_URL
    /// 2. Callback queues blocking CreatePr effect, does NOT spawn reviewer
    /// 3. MoveIssue to review IS still queued
    /// 4. No reviewer task exists until CreatePr completes (poller creates it later)
    #[test]
    fn rig335_engineer_no_pr_defers_reviewer_spawn() {
        ensure_working_dir();
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();

        linear.add_issue(
            "uuid-rig335",
            "RIG-335",
            "Engineer no PR test",
            "Test deferred reviewer flow",
            "in_progress",
            vec![
                "repo:werma".to_string(),
                "analyze:done".to_string(),
                "spec:done".to_string(),
            ],
        );

        // Step 1: Poll — creates engineer task for in_progress issue.
        poll(&db, &linear, &cmd).unwrap();

        let engineer_tasks = db
            .tasks_by_linear_issue("RIG-335", Some("engineer"), false)
            .unwrap();
        assert_eq!(engineer_tasks.len(), 1, "engineer task should be created");

        // Step 2: Engineer completes WITHOUT PR_URL — the critical scenario.
        db.set_task_status(&engineer_tasks[0].id, Status::Completed)
            .unwrap();

        let engineer_output = "## Implementation\n\
                               All changes committed and pushed.\n\
                               cargo test passes.\n\
                               VERDICT=DONE";

        callback(
            &db,
            &engineer_tasks[0].id,
            "engineer",
            engineer_output,
            "RIG-335",
            "~/projects/werma",
            &cmd,
        )
        .unwrap();

        // CRITICAL: Reviewer must NOT be spawned yet.
        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-335", Some("reviewer"), false)
            .unwrap();
        assert_eq!(
            reviewer_tasks.len(),
            0,
            "RIG-335: reviewer must NOT be spawned when engineer has no PR_URL — \
             CreatePr must complete first"
        );

        // CreatePr effect must be queued and blocking.
        let effects = db.pending_effects(100).unwrap();
        let create_pr = effects
            .iter()
            .filter(|e| e.effect_type == crate::models::EffectType::CreatePr)
            .collect::<Vec<_>>();
        assert_eq!(create_pr.len(), 1, "must queue exactly one CreatePr effect");
        assert!(
            create_pr[0].blocking,
            "CreatePr must be blocking — reviewer depends on PR existing"
        );

        // MoveIssue to review should still be queued.
        assert!(
            effects.iter().any(|e| {
                e.effect_type == crate::models::EffectType::MoveIssue
                    && e.payload.get("target_status").and_then(|v| v.as_str()) == Some("review")
            }),
            "MoveIssue('review') must still be queued even without PR_URL"
        );
    }
}
