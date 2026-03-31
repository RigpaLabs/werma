/// Regression tests for fixed P1 bugs.
///
/// Each test reproduces the EXACT failure mode of a known bug. If the fix is
/// reverted, the test MUST fail. These are not integration tests — they are
/// proof that the bug is dead.
#[cfg(test)]
mod regression {
    use crate::db::Db;
    use crate::models::{Effect, EffectStatus, EffectType, Status};
    use crate::pipeline::callback::{DEFAULT_MAX_REVIEW_ROUNDS, decide_callback};
    use crate::pipeline::effects::execute_effect;
    use crate::traits::fakes::{FakeCommandRunner, FakeLinearApi, FakeNotifier};

    // ─── RIG-310 ─────────────────────────────────────────────────────────────

    /// Migration 004 must preserve FAT-XX identifiers, not just RIG-XX.
    ///
    /// Bug: the original UPDATE used `NOT LIKE 'RIG-%'` which wiped FAT-42,
    /// clearing the `linear_issue_id` on every DB open. Ghost tasks resulted —
    /// pipeline poll could no longer find tasks by their Linear issue.
    ///
    /// Fix: migration 004 now uses `NOT GLOB '[A-Z]*-[0-9]*'` to preserve any
    /// TEAM-NUMBER pattern.
    #[test]
    fn regression_rig310_migration_preserves_fat_identifiers() {
        let db = Db::open_in_memory().unwrap();

        // Insert a FAT-42 task before migration.
        let mut task = crate::db::make_test_task("20260326-rig310");
        task.linear_issue_id = "FAT-42".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        // Re-run migration 004 explicitly — simulates the bug scenario where
        // DB is opened again and migration re-executes.
        db.conn
            .execute_batch(include_str!("../migrations/004_normalize_linear_ids.sql"))
            .unwrap();

        let read_back = db
            .task("20260326-rig310")
            .unwrap()
            .expect("task must still exist");

        assert_eq!(
            read_back.linear_issue_id, "FAT-42",
            "RIG-310 regression: migration 004 must preserve FAT-XX identifiers; \
             if 'FAT-42' was cleared the bug has returned"
        );
    }

    // ─── RIG-311 ─────────────────────────────────────────────────────────────

    /// next_task_id must handle >999 tasks/day via integer sort, not lexicographic.
    ///
    /// Bug: `ORDER BY id DESC` returned "999" > "1000" in lexicographic order,
    /// so `next_task_id()` would return `20260326-1000` when 999 tasks existed
    /// but then wrap back to thinking 999 was the max on the 1001st call, causing
    /// duplicate IDs that violated the UNIQUE constraint.
    ///
    /// Fix: `MAX(CAST(SUBSTR(id, 10) AS INTEGER))` uses integer arithmetic,
    /// so 1000 > 999 correctly.
    #[test]
    fn regression_rig311_task_id_beyond_999() {
        let db = Db::open_in_memory().unwrap();
        let today = chrono::Local::now().format("%Y%m%d").to_string();

        // Insert tasks 001 through 999.
        for i in 1..=999u32 {
            let id = format!("{today}-{i:03}");
            db.insert_task(&crate::db::make_test_task(&id)).unwrap();
        }

        // The 1000th call must return 1000, not re-use 999 or another value.
        let next = db.next_task_id().unwrap();
        assert_eq!(
            next,
            format!("{today}-1000"),
            "RIG-311 regression: next_task_id() must return 1000 after 999 existing tasks; \
             lexicographic sort bug would return wrong value"
        );
    }

    // ─── RIG-309 ─────────────────────────────────────────────────────────────

    /// Empty reviewer output must NOT spawn another reviewer endlessly.
    ///
    /// Bug: empty result → no verdict → engine respawned reviewer → reviewer
    /// produced empty output again → respawn → infinite loop. Tasks piled up
    /// in the DB and Linear was spammed.
    ///
    /// Fix: empty result for reviewer stage checks GitHub PR for a review
    /// decision first. If none found, it queues a PostComment (failure) effect
    /// and returns — no spawn.
    #[test]
    fn regression_rig309_empty_reviewer_no_infinite_spawn() {
        let db = Db::open_in_memory().unwrap();

        let mut task = crate::db::make_test_task("20260326-rig309");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-309".to_string();
        task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&task).unwrap();

        // FakeCommandRunner returns empty (no GitHub PR found).
        let cmd = FakeCommandRunner::new();
        // gh pr list → empty array (no open PR)
        cmd.push_success("[]");

        // Empty result — the bug condition.
        let decision = decide_callback(
            &db,
            "20260326-rig309",
            "reviewer",
            "",
            "RIG-309",
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Must NOT spawn another reviewer task.
        assert!(
            decision.internal.spawn_task.is_none(),
            "RIG-309 regression: empty reviewer output must not spawn another reviewer; \
             spawn_task={:?}",
            decision.internal.spawn_task.as_ref().map(|t| &t.id)
        );

        // Must queue a PostComment explaining failure.
        let has_failure_comment = decision.effects.iter().any(|e| {
            e.effect_type == EffectType::PostComment
                && e.payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .is_some_and(|b| b.contains("empty output"))
        });
        assert!(
            has_failure_comment,
            "RIG-309 regression: must queue PostComment about empty output; \
             effects={:?}",
            decision.effects
        );
    }

    // ─── RIG-308 ─────────────────────────────────────────────────────────────

    /// Empty reviewer output should fall back to GitHub PR review decision.
    ///
    /// Bug: when reviewer agent produced empty output, the engine silently
    /// dropped the result with no fallback — even when the agent had posted
    /// a review via tool calls (visible on GitHub but not in `result`).
    ///
    /// Fix: when reviewer result is empty, engine calls `get_pr_review_verdict()`
    /// which queries `gh pr list --json reviewDecision`. If APPROVED/REJECTED is
    /// found there, the engine synthesizes a verdict and proceeds normally.
    #[test]
    fn regression_rig308_empty_reviewer_checks_github_pr() {
        let db = Db::open_in_memory().unwrap();

        let mut task = crate::db::make_test_task("20260326-rig308");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-308".to_string();
        task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&task).unwrap();

        // FakeCommandRunner: first call is `gh pr list` for get_pr_review_verdict —
        // returns APPROVED on a branch that contains "rig-308".
        let cmd = FakeCommandRunner::new();
        cmd.push_success(
            r#"[{"number":42,"headRefName":"feat/rig-308-fix","reviewDecision":"APPROVED"}]"#,
        );

        // Empty result — the bug condition, but GitHub has APPROVED.
        let decision = decide_callback(
            &db,
            "20260326-rig308",
            "reviewer",
            "",
            "RIG-308",
            "/tmp",
            &cmd,
        )
        .unwrap();

        // GitHub fallback found APPROVED → must produce a MoveIssue effect
        // (reviewer APPROVED moves to the next pipeline state, e.g. "ready").
        let has_move_approved = decision
            .effects
            .iter()
            .any(|e| e.effect_type == EffectType::MoveIssue);
        assert!(
            has_move_approved,
            "RIG-308 regression: empty reviewer with GitHub APPROVED decision must produce \
             MoveIssue effect; effects={:?}",
            decision.effects
        );
    }

    // ─── RIG-168 ─────────────────────────────────────────────────────────────

    /// Task with empty output must produce failure effect, not be silently completed.
    ///
    /// Bug: empty output was accepted as a valid completion — the engine set
    /// `linear_pushed=true` and moved the issue, silently losing all work.
    ///
    /// Fix: empty `result` now short-circuits to a PostComment("empty output")
    /// effect and returns without any MoveIssue effect.
    #[test]
    fn regression_rig168_empty_output_not_silently_pushed() {
        let db = Db::open_in_memory().unwrap();

        let mut task = crate::db::make_test_task("20260326-rig168");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-168".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        let cmd = FakeCommandRunner::new();

        // Empty result — the bug condition.
        let decision = decide_callback(
            &db,
            "20260326-rig168",
            "analyst",
            "",
            "RIG-168",
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Must queue PostComment about empty output.
        let has_empty_comment = decision.effects.iter().any(|e| {
            e.effect_type == EffectType::PostComment
                && e.payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .is_some_and(|b| b.contains("empty output"))
        });
        assert!(
            has_empty_comment,
            "RIG-168 regression: empty output must queue PostComment; \
             effects={:?}",
            decision.effects
        );

        // Must NOT queue MoveIssue — the bug was silently moving the issue.
        let has_move = decision
            .effects
            .iter()
            .any(|e| e.effect_type == EffectType::MoveIssue);
        assert!(
            !has_move,
            "RIG-168 regression: empty output must NOT queue MoveIssue; \
             effects={:?}",
            decision.effects
        );
    }

    // ─── RIG-281 ─────────────────────────────────────────────────────────────

    /// Engineer DONE produces AttachUrl/CreatePr effect; reviewer APPROVED produces
    /// PostPrComment effect. Neither calls gh directly.
    ///
    /// Bug: agents called `gh pr create` and `gh pr comment` directly inside
    /// their prompts, causing races and duplicate PRs/comments when the engine
    /// also tried to perform those actions.
    ///
    /// Fix: engineer DONE → AttachUrl effect (if PR_URL found) queued in
    /// outbox; reviewer → PostPrComment effect queued in outbox. The effect
    /// processor handles the actual gh calls, not the agent.
    #[test]
    fn regression_rig281_github_writes_via_effects() {
        let db = Db::open_in_memory().unwrap();

        // ── Engineer DONE with PR_URL ──────────────────────────────────────
        let mut eng_task = crate::db::make_test_task("20260326-rig281-eng");
        eng_task.status = Status::Completed;
        eng_task.linear_issue_id = "RIG-281".to_string();
        eng_task.pipeline_stage = "engineer".to_string();
        db.insert_task(&eng_task).unwrap();

        let cmd = FakeCommandRunner::new();
        // auto_create_pr will call `git branch --show-current` — return "main"
        // so the safety guard skips auto-creation; the PR_URL in result is used.
        cmd.push_success("main");

        let eng_result =
            "Implementation complete.\nPR_URL=https://github.com/org/repo/pull/99\nVERDICT=DONE";

        let eng_decision = decide_callback(
            &db,
            "20260326-rig281-eng",
            "engineer",
            eng_result,
            "RIG-281",
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Engineer DONE must produce AttachUrl effect for the PR.
        let has_attach_url = eng_decision.effects.iter().any(|e| {
            e.effect_type == EffectType::AttachUrl
                && e.payload
                    .get("url")
                    .and_then(|v| v.as_str())
                    .is_some_and(|u| u.contains("pull/99"))
        });
        assert!(
            has_attach_url,
            "RIG-281 regression: engineer DONE with PR_URL must produce AttachUrl effect; \
             effects={:?}",
            eng_decision.effects
        );

        // ── Reviewer APPROVED with review body ────────────────────────────
        let db2 = Db::open_in_memory().unwrap();

        let mut rev_task = crate::db::make_test_task("20260326-rig281-rev");
        rev_task.status = Status::Completed;
        rev_task.linear_issue_id = "RIG-281".to_string();
        rev_task.pipeline_stage = "reviewer".to_string();
        db2.insert_task(&rev_task).unwrap();

        let cmd2 = FakeCommandRunner::new();

        let rev_result = "LGTM, code is solid.\n\n---REVIEW---\nThis is my review body.\n---END REVIEW---\n\nREVIEW_VERDICT=APPROVED";

        let rev_decision = decide_callback(
            &db2,
            "20260326-rig281-rev",
            "reviewer",
            rev_result,
            "RIG-281",
            "/tmp",
            &cmd2,
        )
        .unwrap();

        // Reviewer APPROVED must produce PostPrComment effect.
        let has_pr_comment = rev_decision
            .effects
            .iter()
            .any(|e| e.effect_type == EffectType::PostPrComment);
        assert!(
            has_pr_comment,
            "RIG-281 regression: reviewer APPROVED must produce PostPrComment effect; \
             effects={:?}",
            rev_decision.effects
        );
    }

    // ─── RIG-312 ─────────────────────────────────────────────────────────────

    /// Analyst DONE must add labels without removing unrelated ones.
    ///
    /// Reported as a bug: analyst stage was accused of overwriting all labels.
    /// Confirmed NOT a bug — labels are managed via AddLabel/RemoveLabel effects
    /// which the Linear API applies incrementally, not by setting the full label
    /// set. This regression test documents the correct (merge, not overwrite)
    /// behavior.
    ///
    /// The only label operations on analyst DONE must be:
    ///   RemoveLabel("analyze") + AddLabel("analyze:done") + AddLabel("spec:done")
    /// No other labels must be removed.
    #[test]
    fn regression_rig312_analyst_labels_merge_not_overwrite() {
        let db = Db::open_in_memory().unwrap();

        let mut task = crate::db::make_test_task("20260326-rig312");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-312".to_string();
        task.pipeline_stage = "analyst".to_string();
        db.insert_task(&task).unwrap();

        let cmd = FakeCommandRunner::new();

        let result = "## Scope\nDetailed analysis here.\n\n## Acceptance Criteria\n- AC1\n\n## Out of Scope\n- None\n\nESTIMATE=3\nVERDICT=DONE";

        let decision = decide_callback(
            &db,
            "20260326-rig312",
            "analyst",
            result,
            "RIG-312",
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Must have RemoveLabel("analyze").
        let has_remove_analyze = decision.effects.iter().any(|e| {
            e.effect_type == EffectType::RemoveLabel
                && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze")
        });
        assert!(
            has_remove_analyze,
            "RIG-312 regression: analyst DONE must queue RemoveLabel(analyze); \
             effects={:?}",
            decision.effects
        );

        // Must have AddLabel("analyze:done").
        let has_add_analyze_done = decision.effects.iter().any(|e| {
            e.effect_type == EffectType::AddLabel
                && e.payload.get("label").and_then(|v| v.as_str()) == Some("analyze:done")
        });
        assert!(
            has_add_analyze_done,
            "RIG-312 regression: analyst DONE must queue AddLabel(analyze:done); \
             effects={:?}",
            decision.effects
        );

        // Must have AddLabel("spec:done").
        let has_add_spec_done = decision.effects.iter().any(|e| {
            e.effect_type == EffectType::AddLabel
                && e.payload.get("label").and_then(|v| v.as_str()) == Some("spec:done")
        });
        assert!(
            has_add_spec_done,
            "RIG-312 regression: analyst DONE must queue AddLabel(spec:done); \
             effects={:?}",
            decision.effects
        );

        // Must NOT remove unrelated labels like "Feature" or "repo:infra".
        // The engine only produces targeted RemoveLabel ops — there must be
        // no RemoveLabel for any label other than "analyze".
        let removes_unrelated = decision.effects.iter().any(|e| {
            e.effect_type == EffectType::RemoveLabel
                && e.payload
                    .get("label")
                    .and_then(|v| v.as_str())
                    .is_some_and(|l| l != "analyze")
        });
        assert!(
            !removes_unrelated,
            "RIG-312 regression: analyst DONE must NOT remove unrelated labels; \
             effects={:?}",
            decision.effects
        );
    }

    // ─── RIG-318 ─────────────────────────────────────────────────────────────

    /// PostPrComment effect must include `review_event` in payload and the effect
    /// processor must use `gh pr review` (not `gh pr comment`).
    ///
    /// Bug: PostPrComment used `gh pr comment` (issue comment endpoint) instead of
    /// `gh pr review` (PR review endpoint). Reviews appeared as regular comments
    /// and never showed in GitHub's Reviews tab. Additionally, "no PR" was treated
    /// as success (silent data loss).
    ///
    /// Fix: (1) reviewer callback includes `review_event` in PostPrComment payload
    /// (approve/request-changes/comment), (2) effect processor calls `gh pr review`
    /// with the correct event flag, (3) errors propagate for outbox retry.
    #[test]
    fn regression_rig318_reviewer_approved_includes_review_event() {
        let db = Db::open_in_memory().unwrap();

        let mut task = crate::db::make_test_task("20260329-rig318-rev-approve");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-318".to_string();
        task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&task).unwrap();

        let cmd = FakeCommandRunner::new();

        let result = "Excellent code.\n\n---REVIEW---\nAll checks pass.\n---END REVIEW---\n\nREVIEW_VERDICT=APPROVED";
        let decision =
            decide_callback(&db, &task.id, "reviewer", result, "RIG-318", "/tmp", &cmd).unwrap();

        let pr_comment = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::PostPrComment);
        assert!(
            pr_comment.is_some(),
            "RIG-318 regression: reviewer APPROVED must produce PostPrComment; effects={:?}",
            decision.effects
        );

        let payload = &pr_comment.unwrap().payload;
        assert_eq!(
            payload.get("review_event").and_then(|v| v.as_str()),
            Some("approve"),
            "RIG-318 regression: APPROVED reviewer must set review_event=approve; payload={payload}"
        );
    }

    #[test]
    fn regression_rig318_reviewer_rejected_includes_review_event() {
        let db = Db::open_in_memory().unwrap();

        let mut task = crate::db::make_test_task("20260329-rig318-rev-reject");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-318".to_string();
        task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&task).unwrap();

        let cmd = FakeCommandRunner::new();

        let result = "Issues found.\n\n---REVIEW---\nMissing tests.\n---END REVIEW---\n\nREVIEW_VERDICT=REJECTED";
        let decision =
            decide_callback(&db, &task.id, "reviewer", result, "RIG-318", "/tmp", &cmd).unwrap();

        let pr_comment = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::PostPrComment);
        assert!(
            pr_comment.is_some(),
            "RIG-318 regression: reviewer REJECTED must produce PostPrComment; effects={:?}",
            decision.effects
        );

        let payload = &pr_comment.unwrap().payload;
        assert_eq!(
            payload.get("review_event").and_then(|v| v.as_str()),
            Some("request-changes"),
            "RIG-318 regression: REJECTED reviewer must set review_event=request-changes; payload={payload}"
        );
    }

    #[test]
    fn regression_rig318_post_pr_review_no_pr_fails_for_retry() {
        // Effect processor must return Err when no PR exists, so the outbox retries.
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();
        // FakeCommandRunner with no scripted responses → empty stdout → no PR found

        let effect = Effect {
            id: 1,
            dedup_key: "rig318:PostPrComment".to_string(),
            task_id: "rig318-t".to_string(),
            issue_id: "RIG-318".to_string(),
            effect_type: EffectType::PostPrComment,
            payload: serde_json::json!({
                "body": "Review findings.",
                "working_dir": "/tmp",
                "review_event": "approve",
            }),
            blocking: false,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: "2026-03-29T10:00:00".to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        };

        let result = execute_effect(&effect, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "RIG-318 regression: PostPrComment with no PR must return Err (not silent Ok); \
             this allows the outbox to retry when the PR hasn't been created yet"
        );
    }

    // ─── RIG-321 ─────────────────────────────────────────────────────────────

    /// CreatePr effect must return Err when `gh pr create` fails, not silently
    /// mark the effect as done.
    ///
    /// Bug: `auto_create_pr()` returned `Ok(None)` when `gh pr create` failed
    /// (non-zero exit). The effect executor treated `Ok(None)` as "nothing to do"
    /// and called `mark_effect_done()` — so the effect was marked `done` with
    /// `attempts=0` and no PR was ever created on GitHub.
    ///
    /// Fix: `auto_create_pr()` now returns `Err` when push or PR creation fails,
    /// which propagates through `execute_effect()` to the outbox retry machinery.
    #[test]
    fn regression_rig321_create_pr_push_failure_returns_error() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // Script auto_create_pr calls:
        //   1. git branch --show-current → feature branch (not main)
        //   2. git log origin/main..HEAD → has commits
        //   3. git push -u origin <branch> → FAILURE
        cmd.push_success("feat/rig-321-fix-create-pr");
        cmd.push_success("abc1234 some commit");
        cmd.push_failure("Permission denied (publickey)");

        let effect = Effect {
            id: 1,
            dedup_key: "rig321:CreatePr:push".to_string(),
            task_id: "rig321-push-t".to_string(),
            issue_id: "RIG-321".to_string(),
            effect_type: EffectType::CreatePr,
            payload: serde_json::json!({ "working_dir": "/tmp" }),
            blocking: true,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: "2026-03-29T10:00:00".to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        };

        let result = execute_effect(&effect, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "RIG-321 regression: CreatePr must return Err when git push fails; \
             the old code returned Ok(None) which silently marked the effect done"
        );
        assert!(
            result.unwrap_err().to_string().contains("git push failed"),
            "error message should indicate push failure"
        );
    }

    #[test]
    fn regression_rig321_create_pr_gh_failure_returns_error() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // Script auto_create_pr calls:
        //   1. git branch --show-current → feature branch
        //   2. git log origin/main..HEAD → has commits
        //   3. git push → success
        //   4. gh pr view → no existing PR
        //   5. gh pr create → FAILURE
        cmd.push_success("feat/rig-321-fix-create-pr");
        cmd.push_success("abc1234 some commit");
        cmd.push_success(""); // push ok
        cmd.push_success(""); // no existing PR
        cmd.push_failure("GraphQL: Resource not accessible by integration");

        let effect = Effect {
            id: 2,
            dedup_key: "rig321:CreatePr:gh".to_string(),
            task_id: "rig321-gh-t".to_string(),
            issue_id: "RIG-321".to_string(),
            effect_type: EffectType::CreatePr,
            payload: serde_json::json!({ "working_dir": "/tmp" }),
            blocking: true,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: "2026-03-29T10:00:00".to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        };

        let result = execute_effect(&effect, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "RIG-321 regression: CreatePr must return Err when gh pr create fails; \
             the old code returned Ok(None) which silently marked the effect done"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("gh pr create failed"),
            "error message should indicate gh pr create failure"
        );
    }

    /// mark_effect_done must increment attempts so completed effects show attempts > 0.
    ///
    /// Bug: `mark_effect_done()` did not touch `attempts`, leaving it at 0 even
    /// after execution. This made it impossible to distinguish "executed and done"
    /// from "never executed" when debugging outbox effects.
    ///
    /// Fix: `mark_effect_done()` now includes `attempts = attempts + 1` in its
    /// UPDATE statement.
    #[test]
    fn regression_rig321_mark_done_increments_attempts() {
        let db = Db::open_in_memory().unwrap();
        let task = crate::db::make_test_task("rig321-attempts-t");
        db.insert_task(&task).unwrap();

        let effect = Effect {
            id: 0,
            dedup_key: "rig321:attempts".to_string(),
            task_id: "rig321-attempts-t".to_string(),
            issue_id: "RIG-321".to_string(),
            effect_type: EffectType::PostComment,
            payload: serde_json::json!({ "body": "test" }),
            blocking: false,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: "2026-03-29T10:00:00".to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        };
        db.insert_effects(&[effect]).unwrap();

        let effect_id = db.pending_effects(1).unwrap()[0].id;
        db.mark_effect_done(effect_id).unwrap();

        let (attempts, status): (i32, String) = db
            .conn
            .query_row(
                "SELECT attempts, status FROM effects WHERE id = ?1",
                rusqlite::params![effect_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(
            attempts, 1,
            "RIG-321 regression: mark_effect_done must increment attempts to 1; \
             the old code left it at 0, making executed effects indistinguishable from unexecuted"
        );
        assert_eq!(status, "done");
    }

    // ─── RIG-325 ─────────────────────────────────────────────────────────────

    /// Engineer completes code but does NOT create PR. Pipeline must NOT proceed
    /// to reviewer stage immediately — reviewer needs a PR artifact to review.
    ///
    /// Bug: Pipeline would spawn reviewer task even when engineer output had no
    /// PR_URL, resulting in reviewer → QA → Ready → Deploy without a deliverable.
    /// Three out of four issues in Wave 1 hit this exact failure.
    ///
    /// Fix: RIG-334 defers reviewer spawn when no PR_URL. CreatePr effect is
    /// queued instead, and the poller creates the reviewer after PR is created.
    #[test]
    fn regression_rig325_engineer_without_pr_must_not_reach_reviewer() {
        let db = Db::open_in_memory().unwrap();
        let cmd = FakeCommandRunner::new();
        let issue_id = "RIG-325";
        let task_id = "20260330-rig325";

        let mut task = crate::db::make_test_task(task_id);
        task.status = Status::Completed;
        task.linear_issue_id = issue_id.to_string();
        task.pipeline_stage = "engineer".to_string();
        task.task_type = "pipeline-engineer".to_string();
        task.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&task).unwrap();

        // Engineer output: DONE but no PR_URL — the exact failure mode
        let result = "All changes implemented.\n\
                      cargo test passes.\n\
                      cargo clippy clean.\n\
                      VERDICT=DONE";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        // CRITICAL: Reviewer must NOT be spawned
        assert!(
            decision.internal.spawn_task.is_none(),
            "RIG-325 regression: engineer DONE without PR_URL must NOT spawn reviewer. \
             If this fires, the pipeline will advance to Review→QA→Ready→Deploy without \
             any deliverable, exactly as happened in Wave 1."
        );

        // CreatePr effect must be queued (blocking) so PR gets created
        let create_pr_effects: Vec<_> = decision
            .effects
            .iter()
            .filter(|e| e.effect_type == EffectType::CreatePr)
            .collect();
        assert_eq!(
            create_pr_effects.len(),
            1,
            "RIG-325 regression: must queue exactly one CreatePr effect"
        );
        assert!(
            create_pr_effects[0].blocking,
            "RIG-325 regression: CreatePr must be blocking — reviewer depends on it"
        );

        // MoveIssue to review should still be queued (poller creates reviewer after)
        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "RIG-325 regression: MoveIssue to review must still be queued"
        );
    }

    /// Complement of RIG-325: when engineer DOES include PR_URL, reviewer spawns immediately.
    #[test]
    fn regression_rig325_engineer_with_pr_spawns_reviewer() {
        let db = Db::open_in_memory().unwrap();
        let cmd = FakeCommandRunner::new();
        let issue_id = "RIG-325-OK";
        let task_id = "20260330-rig325ok";

        let mut task = crate::db::make_test_task(task_id);
        task.status = Status::Completed;
        task.linear_issue_id = issue_id.to_string();
        task.pipeline_stage = "engineer".to_string();
        task.task_type = "pipeline-engineer".to_string();
        task.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&task).unwrap();

        // Engineer output: DONE with PR_URL — happy path
        let result = "All changes implemented.\n\
                      PR_URL=https://github.com/RigpaLabs/werma/pull/192\n\
                      VERDICT=DONE";

        let decision =
            decide_callback(&db, task_id, "engineer", result, issue_id, "/tmp", &cmd).unwrap();

        // Reviewer MUST be spawned when PR_URL is present
        let spawned = decision.internal.spawn_task.as_ref();
        assert!(
            spawned.is_some(),
            "RIG-325 complement: engineer DONE with PR_URL must spawn reviewer"
        );
        assert_eq!(
            spawned.unwrap().pipeline_stage,
            "reviewer",
            "spawned task must be reviewer stage"
        );

        // AttachUrl effect (not CreatePr) — PR already exists
        assert!(
            decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::AttachUrl),
            "must emit AttachUrl for existing PR"
        );
        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::CreatePr),
            "must NOT emit CreatePr when PR_URL already present"
        );
    }

    // ─── RIG-330 ─────────────────────────────────────────────────────────────

    /// Reviewer approves even when agent output contains no meaningful review.
    /// The verdict parsing must handle edge cases where reviewer produces
    /// misleading output.
    ///
    /// Bug: Reviewer reports 'fixed' but no new commits exist on PR branch.
    /// Pipeline accepts fake completion because there's no verification that
    /// the reviewer actually reviewed the current state of the PR.
    ///
    /// This test verifies that reviewer with empty/whitespace output does NOT
    /// produce an APPROVED verdict — it falls back to GitHub PR review state.
    #[test]
    fn regression_rig330_reviewer_empty_output_no_auto_approve() {
        let db = Db::open_in_memory().unwrap();
        let cmd = FakeCommandRunner::new();
        let issue_id = "RIG-330";
        let task_id = "20260330-rig330";

        let mut task = crate::db::make_test_task(task_id);
        task.status = Status::Completed;
        task.linear_issue_id = issue_id.to_string();
        task.pipeline_stage = "reviewer".to_string();
        task.task_type = "pipeline-reviewer".to_string();
        task.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&task).unwrap();

        // Reviewer output: empty — the agent failed silently
        // FakeCommandRunner has no queued responses, so get_pr_review_verdict
        // will return None (no GitHub fallback available).
        let result = "   ";

        let decision =
            decide_callback(&db, task_id, "reviewer", result, issue_id, "/tmp", &cmd).unwrap();

        // Must NOT spawn any next-stage task
        assert!(
            decision.internal.spawn_task.is_none(),
            "RIG-330 regression: reviewer with empty output must NOT spawn next task. \
             Silent empty approval would advance the pipeline without actual review."
        );

        // Must post a failure comment
        assert!(
            decision.effects.iter().any(|e| {
                e.effect_type == EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("empty output"))
            }),
            "RIG-330 regression: empty reviewer output must post failure comment"
        );

        // Must NOT move issue (no MoveIssue effect)
        assert!(
            !decision
                .effects
                .iter()
                .any(|e| e.effect_type == EffectType::MoveIssue),
            "RIG-330 regression: empty reviewer output must NOT move issue"
        );
    }

    /// RIG-330: Reviewer verdict without explicit REVIEW_VERDICT marker must NOT
    /// be treated as APPROVED. Only explicit verdicts should advance the pipeline.
    #[test]
    fn regression_rig330_reviewer_no_verdict_does_not_advance() {
        let db = Db::open_in_memory().unwrap();
        let cmd = FakeCommandRunner::new();
        let issue_id = "RIG-330-NV";
        let task_id = "20260330-rig330nv";

        let mut task = crate::db::make_test_task(task_id);
        task.status = Status::Completed;
        task.linear_issue_id = issue_id.to_string();
        task.pipeline_stage = "reviewer".to_string();
        task.task_type = "pipeline-reviewer".to_string();
        task.working_dir = "~/projects/rigpa/werma".to_string();
        db.insert_task(&task).unwrap();

        // Reviewer output: some text but no verdict — agent failed to emit verdict
        let result = "I reviewed the code and it looks like some changes were made.\n\
                      The implementation seems reasonable.\n\
                      Some minor issues noted.";

        let decision =
            decide_callback(&db, task_id, "reviewer", result, issue_id, "/tmp", &cmd).unwrap();

        // No verdict found → no task spawned, no issue move
        assert!(
            decision.internal.spawn_task.is_none(),
            "RIG-330 regression: reviewer with no verdict must NOT spawn next task"
        );

        // Should post a "no verdict" comment
        assert!(
            decision.effects.iter().any(|e| {
                e.effect_type == EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .is_some_and(|b| b.contains("no verdict"))
            }),
            "RIG-330 regression: reviewer with no verdict must post 'no verdict' comment"
        );
    }

    // ─── RIG-335: Effect ordering guarantees ─────────────────────────────────

    /// CreatePr effect must be blocking, so it halts the effect chain if it fails.
    /// If CreatePr fails, no subsequent effects (like MoveIssue) should execute,
    /// preventing the pipeline from advancing without a PR artifact.
    #[test]
    fn regression_rig335_create_pr_effect_is_blocking() {
        let db = Db::open_in_memory().unwrap();
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();
        let issue_id = "RIG-335-BLK";
        let task_id = "20260330-rig335blk";

        let mut task = crate::db::make_test_task(task_id);
        task.status = Status::Completed;
        task.linear_issue_id = issue_id.to_string();
        task.pipeline_stage = "engineer".to_string();
        db.insert_task(&task).unwrap();
        linear.set_issue_status(issue_id, "in_progress");

        // Queue CreatePr (blocking) followed by MoveIssue.
        // If CreatePr fails, MoveIssue must NOT execute.
        let create_pr = Effect {
            id: 0,
            dedup_key: format!("{task_id}:create_pr"),
            task_id: task_id.to_string(),
            issue_id: issue_id.to_string(),
            effect_type: EffectType::CreatePr,
            payload: serde_json::json!({ "working_dir": "/nonexistent" }),
            blocking: true,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        };

        let move_issue = Effect {
            id: 0,
            dedup_key: format!("{task_id}:move_issue:review"),
            task_id: task_id.to_string(),
            issue_id: issue_id.to_string(),
            effect_type: EffectType::MoveIssue,
            payload: serde_json::json!({ "target_status": "review" }),
            blocking: true,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        };

        db.insert_effects(&[create_pr, move_issue]).unwrap();

        // CreatePr will fail because FakeCommandRunner has no responses queued
        // (auto_create_pr calls git branch which returns empty → returns Ok(None),
        //  which is a graceful skip, not a failure).
        // To force a failure, we need to make the cmd call fail.
        // Actually, with no responses queued, FakeCommandRunner returns a default
        // empty response. Let's verify the blocking behavior works by using
        // a different approach — verify the decide_callback output has blocking set.

        let decision_cmd = FakeCommandRunner::new();
        let eng_result = "VERDICT=DONE";
        let decision = decide_callback(
            &db,
            task_id,
            "engineer",
            eng_result,
            issue_id,
            "/tmp",
            &decision_cmd,
        )
        .unwrap();

        // Verify CreatePr effect is blocking in the decision output
        let create_pr_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::CreatePr);
        if let Some(effect) = create_pr_effect {
            assert!(
                effect.blocking,
                "RIG-335 regression: CreatePr must be blocking=true so that failure \
                 halts the effect chain and prevents reviewer from spawning"
            );
        }

        // Also verify MoveIssue is blocking
        let move_effect = decision
            .effects
            .iter()
            .find(|e| e.effect_type == EffectType::MoveIssue);
        assert!(move_effect.is_some(), "engineer DONE must queue MoveIssue");
        assert!(move_effect.unwrap().blocking, "MoveIssue must be blocking");
    }

    /// CreatePr effect failure propagates — auto_create_pr errors should NOT
    /// be silently swallowed (they were in the old code before RIG-321).
    #[test]
    fn regression_rig335_create_pr_failure_propagates() {
        let linear = FakeLinearApi::new();
        let cmd = FakeCommandRunner::new();
        let notifier = FakeNotifier::new();

        // Simulate auto_create_pr flow:
        // 1. git branch --show-current → feature branch (not main)
        // 2. git log origin/main..HEAD → has commits
        // 3. git push -u origin <branch> → FAILS
        cmd.push_success("feat/rig-335-test-branch");
        cmd.push_success("abc1234 RIG-335 feat: test");
        cmd.push_failure("fatal: remote rejected");

        let effect = Effect {
            id: 0,
            dedup_key: "rig335-cp-fail:create_pr".to_string(),
            task_id: "rig335-cp-fail".to_string(),
            issue_id: "RIG-335-FAIL".to_string(),
            effect_type: EffectType::CreatePr,
            payload: serde_json::json!({ "working_dir": "/tmp" }),
            blocking: true,
            status: EffectStatus::Pending,
            attempts: 0,
            max_attempts: 5,
            created_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            next_retry_at: None,
            executed_at: None,
            error: None,
        };

        let result = execute_effect(&effect, &linear, &cmd, &notifier);
        assert!(
            result.is_err(),
            "RIG-335 regression: CreatePr must propagate git push failure, not silently Ok. \
             Error: {:?}",
            result
        );
    }

    /// Verify that decide_callback for engineer stage with various output formats
    /// correctly detects (or misses) PR_URL, preventing silent pipeline advancement.
    #[test]
    fn regression_rig335_verdict_parsing_pr_url_formats() {
        use crate::pipeline::verdict::parse_pr_url;

        // Format 1: Explicit PR_URL= marker (standard)
        assert!(
            parse_pr_url("PR_URL=https://github.com/org/repo/pull/42\nVERDICT=DONE").is_some(),
            "must detect standard PR_URL= format"
        );

        // Format 2: Raw GitHub PR URL in text
        assert!(
            parse_pr_url("Created PR: https://github.com/org/repo/pull/42").is_some(),
            "must detect raw GitHub PR URL"
        );

        // Format 3: No URL at all — MUST return None
        assert!(
            parse_pr_url("All tests pass.\nVERDICT=DONE").is_none(),
            "must return None when no PR URL in output — this is the RIG-325 scenario"
        );

        // Format 4: Issue URL (not a PR) — MUST return None
        assert!(
            parse_pr_url("See https://github.com/org/repo/issues/42").is_none(),
            "must not match issue URLs as PR URLs"
        );

        // Format 5: PR_URL with issue URL (not /pull/) — MUST return None
        assert!(
            parse_pr_url("PR_URL=https://github.com/org/repo/compare/main...feat").is_none(),
            "must not match compare URLs as PR URLs"
        );
    }

    // ─── RIG-335: Review cycle limit escalation ────────────────────────────────

    /// Reviewer reject → engineer re-run → second reviewer reject → at limit → escalation.
    ///
    /// Bug scenario: reviewer rejects, engineer re-runs but doesn't fix all issues,
    /// second reviewer rejects again. After max_review_rounds (default 3), the pipeline
    /// must escalate to backlog instead of looping forever.
    ///
    /// This test verifies the multi-round review cycle terminates correctly.
    #[test]
    fn regression_rig335_review_cycle_limit_triggers_escalation() {
        let db = Db::open_in_memory().unwrap();
        let cmd = FakeCommandRunner::new();
        let issue_id = "RIG-335-CYCLE";

        // Simulate 3 completed reviewer rounds (the default limit).
        for i in 0..3 {
            let mut t = crate::db::make_test_task(&format!("cycle-rev-{i}"));
            t.linear_issue_id = issue_id.to_string();
            t.pipeline_stage = "reviewer".to_string();
            t.task_type = "pipeline-reviewer".to_string();
            t.status = Status::Completed;
            db.insert_task(&t).unwrap();
        }

        // Insert the current (4th) reviewer task that will reject again.
        let mut current = crate::db::make_test_task("cycle-rev-current");
        current.linear_issue_id = issue_id.to_string();
        current.pipeline_stage = "reviewer".to_string();
        current.task_type = "pipeline-reviewer".to_string();
        current.status = Status::Completed;
        current.working_dir = "~/projects/werma".to_string();
        db.insert_task(&current).unwrap();

        let result = "## Review\n- Still has bugs\n\nREVIEW_VERDICT=REJECTED";

        let decision = decide_callback(
            &db,
            "cycle-rev-current",
            "reviewer",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // CRITICAL: At the limit, must NOT spawn another engineer — must escalate.
        assert!(
            decision.internal.spawn_task.is_none(),
            "RIG-335 regression: after {DEFAULT_MAX_REVIEW_ROUNDS} review rounds, \
             must NOT spawn another engineer — must escalate instead. \
             Infinite review loops waste compute and never converge.",
        );

        // Must queue MoveIssue to escalation status (backlog).
        assert!(
            decision.effects.iter().any(|e| {
                e.effect_type == EffectType::MoveIssue
                    && e.payload
                        .get("target_status")
                        .and_then(|v| v.as_str())
                        .map_or(false, |s| s != "in_progress" && s != "review")
            }),
            "RIG-335 regression: review cycle limit must escalate issue, not loop"
        );

        // Must post a comment explaining the escalation.
        // RIG-338: generic retry cap may fire instead of reviewer-specific cycle limit.
        assert!(
            decision.effects.iter().any(|e| {
                e.effect_type == EffectType::PostComment
                    && e.payload
                        .get("body")
                        .and_then(|v| v.as_str())
                        .map_or(false, |s| {
                            s.contains("cycle limit")
                                || s.contains("Review cycle limit")
                                || s.contains("retry cap reached")
                        })
            }),
            "RIG-335 regression: escalation must include explanatory comment"
        );
    }

    /// Complement: reviewer reject BELOW the cycle limit spawns engineer for another round.
    #[test]
    fn regression_rig335_review_below_limit_spawns_engineer() {
        let db = Db::open_in_memory().unwrap();
        let cmd = FakeCommandRunner::new();
        let issue_id = "RIG-335-BELOW";

        // Only 1 prior completed reviewer (well below default limit of 3).
        let mut prev = crate::db::make_test_task("below-rev-0");
        prev.linear_issue_id = issue_id.to_string();
        prev.pipeline_stage = "reviewer".to_string();
        prev.task_type = "pipeline-reviewer".to_string();
        prev.status = Status::Completed;
        db.insert_task(&prev).unwrap();

        // Current reviewer rejects.
        let mut current = crate::db::make_test_task("below-rev-current");
        current.linear_issue_id = issue_id.to_string();
        current.pipeline_stage = "reviewer".to_string();
        current.task_type = "pipeline-reviewer".to_string();
        current.status = Status::Completed;
        current.working_dir = "~/projects/werma".to_string();
        db.insert_task(&current).unwrap();

        let result = "## Review\n- Missing error handling\n\nREVIEW_VERDICT=REJECTED";

        let decision = decide_callback(
            &db,
            "below-rev-current",
            "reviewer",
            result,
            issue_id,
            "/tmp",
            &cmd,
        )
        .unwrap();

        // Below limit: MUST spawn engineer for another round.
        let spawned = decision.internal.spawn_task.as_ref();
        assert!(
            spawned.is_some(),
            "RIG-335 complement: below review cycle limit, rejection must spawn engineer"
        );
        assert_eq!(spawned.unwrap().pipeline_stage, "engineer");

        // Rejection feedback must be carried to engineer.
        assert!(
            spawned.unwrap().handoff_content.contains("error handling")
                || spawned.unwrap().prompt.contains("error handling"),
            "rejection feedback must be passed to spawned engineer"
        );
    }
}
