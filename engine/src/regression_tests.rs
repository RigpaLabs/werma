/// Regression tests for fixed P1 bugs.
///
/// Each test reproduces the EXACT failure mode of a known bug. If the fix is
/// reverted, the test MUST fail. These are not integration tests — they are
/// proof that the bug is dead.
#[cfg(test)]
mod regression {
    use crate::db::Db;
    use crate::models::{Effect, EffectStatus, EffectType, Status};
    use crate::pipeline::callback::decide_callback;
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

        let result = "## Spec\nDetailed analysis here.\nESTIMATE=3\nVERDICT=DONE";

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
}
