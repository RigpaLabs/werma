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

    /// Ensure `~/projects/rigpa/werma` exists for validate_working_dir on CI.
    fn ensure_working_dir() {
        if let Some(home) = dirs::home_dir() {
            let dir = home.join("projects/rigpa/werma");
            let _ = std::fs::create_dir_all(dir);
        }
    }

    // ─── RIG-229: Analyst → Engineer → Reviewer full pipeline ──────────────────

    /// Test the analyst → engineer → reviewer full pipeline cycle using
    /// StatefulFakeLinearApi with real issue state transitions.
    ///
    /// Actual pipeline flow (from default.yaml):
    /// 1. Issue in backlog with "analyze" label → poll() creates analyst task, removes label,
    ///    moves issue to "in_progress" (analyst on_start)
    /// 2. Analyst DONE callback → moves issue to "todo" (no spawn)
    /// 3. Human gate: Ar moves issue to "in_progress"
    /// 4. poll() → engineer picks up issue in "in_progress", creates engineer task
    /// 5. Engineer DONE callback → moves to "review", spawns reviewer
    /// 6. Reviewer APPROVED callback → moves to "ready"
    #[test]
    fn rig229_analyst_engineer_reviewer_full_pipeline() {
        ensure_working_dir();
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // Seed the issue in Backlog with "analyze" label to trigger analyst
        linear.add_issue(
            "uuid-rig229",
            "RIG-229",
            "Full pipeline test",
            "Test the full pipeline cycle",
            "backlog",
            vec!["analyze".to_string(), "repo:werma".to_string()],
        );

        // Step 1: Poll — should create analyst task (label-based trigger)
        poll(&db, &linear, &cmd).unwrap();

        let analyst_tasks = db
            .tasks_by_linear_issue("RIG-229", Some("analyst"), false)
            .unwrap();
        assert_eq!(analyst_tasks.len(), 1, "analyst task should be created");
        assert_eq!(analyst_tasks[0].pipeline_stage, "analyst");
        assert_eq!(analyst_tasks[0].status, Status::Pending);

        // Label should be removed after task creation
        assert!(
            !linear
                .issue_labels("RIG-229")
                .contains(&"analyze".to_string()),
            "trigger label should be removed"
        );

        // Step 2: Simulate analyst completing with DONE verdict
        // Mark task as completed so callback dedup passes
        db.set_task_status(&analyst_tasks[0].id, Status::Completed)
            .unwrap();

        let analyst_output =
            "## Analysis\n\nThis feature needs X and Y.\n\nESTIMATE=3\nVERDICT=DONE";

        callback(
            &db,
            &analyst_tasks[0].id,
            "analyst",
            analyst_output,
            "RIG-229",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // Issue should have moved to "todo" (analyst DONE transition)
        let status_after_analyst = linear.issue_status("RIG-229");
        assert_eq!(
            status_after_analyst,
            Some("todo".to_string()),
            "analyst DONE should move issue to todo"
        );

        // Analyst does NOT spawn engineer directly — that's the human gate.
        // No engineer task should exist yet.
        let engineer_tasks_pre = db
            .tasks_by_linear_issue("RIG-229", Some("engineer"), false)
            .unwrap();
        assert_eq!(
            engineer_tasks_pre.len(),
            0,
            "engineer should not be spawned by analyst — it's polled from in_progress"
        );

        // Step 3: Simulate human gate — Ar reviews spec and moves to "in_progress"
        linear.move_issue_by_name_direct("RIG-229", "in_progress");
        assert_eq!(
            linear.issue_status("RIG-229"),
            Some("in_progress".to_string()),
            "issue should be in_progress after human gate"
        );

        // Step 4: Poll again — engineer should pick up the in_progress issue
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
        assert_eq!(engineer_tasks[0].status, Status::Pending);

        // Step 5: Simulate engineer completing with DONE + PR_URL
        db.set_task_status(&engineer_tasks[0].id, Status::Completed)
            .unwrap();

        let engineer_output = "## Implementation\nDone.\n\nPR_URL=https://github.com/RigpaLabs/werma/pull/99\nVERDICT=DONE";

        callback(
            &db,
            &engineer_tasks[0].id,
            "engineer",
            engineer_output,
            "RIG-229",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // Issue should move to "review"
        assert_eq!(
            linear.issue_status("RIG-229"),
            Some("review".to_string()),
            "engineer DONE should move issue to review"
        );

        // Reviewer task should be spawned (engineer done transition has spawn: reviewer)
        let reviewer_tasks = db
            .tasks_by_linear_issue("RIG-229", Some("reviewer"), false)
            .unwrap();
        assert_eq!(reviewer_tasks.len(), 1, "reviewer task should be spawned");
        assert_eq!(reviewer_tasks[0].pipeline_stage, "reviewer");

        // PR URL should be attached to Linear
        let attach_calls: Vec<_> = linear
            .calls
            .borrow()
            .iter()
            .filter_map(|c| {
                if let crate::traits::fakes::ApiCall::AttachUrl { url, .. } = c {
                    Some(url.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            attach_calls.iter().any(|u| u.contains("/pull/99")),
            "PR URL should be attached, got: {attach_calls:?}"
        );

        // Step 6: Simulate reviewer approving
        db.set_task_status(&reviewer_tasks[0].id, Status::Completed)
            .unwrap();

        let reviewer_output = "## Review\n- All good!\nREVIEW_VERDICT=APPROVED";

        callback(
            &db,
            &reviewer_tasks[0].id,
            "reviewer",
            reviewer_output,
            "RIG-229",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // Issue should move to "ready"
        assert_eq!(
            linear.issue_status("RIG-229"),
            Some("ready".to_string()),
            "reviewer APPROVED should move issue to ready"
        );
    }

    // ─── RIG-230: Callback failure → retry → success ────────────────────────────

    /// Test that move_with_retry handles N failures then succeeds on attempt N+1.
    #[test]
    fn rig230_callback_failure_retry_success() {
        let db = Db::open_in_memory().unwrap();
        let linear = StatefulFakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        linear.add_issue(
            "uuid-rig230",
            "RIG-230",
            "Retry test",
            "desc",
            "in_progress",
            vec![],
        );

        // Set up: first 2 moves will fail, 3rd will succeed (within CALLBACK_MAX_RETRIES=3)
        linear.fail_next_n_moves(2);

        let mut task = make_test_task("20260314-230");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-230".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        // Analyst DONE: should move to "todo"
        let analyst_output = "Analysis done.\nVERDICT=DONE";

        callback(
            &db,
            "20260314-230",
            "analyst",
            analyst_output,
            "RIG-230",
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // After 2 failures and 1 success, issue should be in "todo"
        assert_eq!(
            linear.issue_status("RIG-230"),
            Some("todo".to_string()),
            "after retries, issue should move to todo"
        );

        // Move calls: 2 failed (not recorded) + 1 successful = 1 move call
        let move_call_count = linear
            .calls
            .borrow()
            .iter()
            .filter(|c| matches!(c, crate::traits::fakes::ApiCall::Move { .. }))
            .count();
        // StatefulFakeLinearApi records the successful move
        assert_eq!(
            move_call_count, 1,
            "exactly 1 successful move should be recorded"
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

    /// Test review cycle limit: after max_review_rounds rejections, issue goes to Blocked.
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

        // Insert completed reviewer tasks to simulate max_review_rounds (3) rejections
        // Load config (to reference pipeline settings for review round limit)
        let _config = load_from_str(include_str!("../pipelines/default.yaml"), "<test>").unwrap();

        // Create 3 completed reviewer tasks (= max_review_rounds)
        for i in 0..3 {
            let mut task = make_test_task(&format!("20260314-231c-{i:03}"));
            task.status = Status::Completed;
            task.linear_issue_id = "RIG-231c".to_string();
            task.pipeline_stage = "reviewer".to_string();
            db.insert_task(&task).unwrap();
        }

        // Now simulate a 4th reviewer completing with REJECTED — should escalate to Blocked
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
            "~/projects/rigpa/werma",
            &linear,
            &cmd,
            &notifier,
        )
        .unwrap();

        // The reviewer REJECTED transition first moves to "in_progress" (the normal transition).
        // Then the cycle limit check fires and moves to "backlog" (from reviewer config's
        // blocked transition) — RIG-280: no longer hardcodes "blocked".
        let moves: Vec<String> = linear
            .calls
            .borrow()
            .iter()
            .filter_map(|c| {
                if let crate::traits::fakes::ApiCall::Move { status, .. } = c {
                    Some(status.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            moves.contains(&"backlog".to_string()),
            "review cycle limit should escalate to backlog (from config), got moves: {moves:?}"
        );

        // Final issue status should be "backlog"
        assert_eq!(
            linear.issue_status("RIG-231c"),
            Some("backlog".to_string()),
            "issue should end up in backlog state"
        );

        // No new engineer task should be created (cycle limit was reached)
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
}
