use anyhow::Result;
use rusqlite::params;

use crate::models::Task;

use super::task_from_row;

impl super::Db {
    /// Set linear_pushed flag.
    pub fn set_linear_pushed(&self, id: &str, pushed: bool) -> Result<()> {
        let val: i32 = if pushed { 1 } else { 0 };
        self.conn.execute(
            "UPDATE tasks SET linear_pushed = ?1 WHERE id = ?2",
            params![val, id],
        )?;
        Ok(())
    }

    /// Find tasks by issue_identifier (optionally filter by pipeline_stage and active status).
    pub fn tasks_by_linear_issue(
        &self,
        issue_id: &str,
        stage: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<Task>> {
        let base_sql = "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, issue_identifier, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks WHERE issue_identifier = ?1";
        let stage_clause = if stage.is_some() {
            " AND pipeline_stage = ?2"
        } else {
            ""
        };
        let active_clause = if active_only {
            " AND status IN ('pending', 'running')"
        } else {
            ""
        };
        let sql = format!("{base_sql}{stage_clause}{active_clause} ORDER BY created_at DESC");

        let mut stmt = self.conn.prepare(&sql)?;
        let mut tasks = Vec::new();
        if let Some(s) = stage {
            let rows = stmt.query_map(params![issue_id, s], |row| Ok(task_from_row(row)))?;
            for row in rows {
                tasks.push(row??);
            }
        } else {
            let rows = stmt.query_map(params![issue_id], |row| Ok(task_from_row(row)))?;
            for row in rows {
                tasks.push(row??);
            }
        }
        Ok(tasks)
    }

    /// Count all active (pending + running) pipeline tasks across all stages.
    pub fn count_active_pipeline_tasks(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE pipeline_stage != ''
               AND status IN ('pending', 'running')",
            [],
            |row| row.get(0),
        )?)
    }

    /// Count completed tasks for a given Linear issue and pipeline stage.
    pub fn count_completed_tasks_for_issue_stage(
        &self,
        issue_id: &str,
        stage: &str,
    ) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage = ?2
               AND status = 'completed'",
            params![issue_id, stage],
            |row| row.get(0),
        )?)
    }

    /// Count ALL tasks (any status) for a given Linear issue and pipeline stage.
    /// Used as a circuit breaker to prevent infinite spawn loops (RIG-309).
    pub fn count_all_tasks_for_issue_stage(&self, issue_id: &str, stage: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage = ?2",
            params![issue_id, stage],
            |row| row.get(0),
        )?)
    }

    /// Count failed tasks for a given Linear issue and pipeline stage.
    /// Used to detect repeated soft failures (e.g. max_turns exits) and escalate.
    pub fn count_failed_tasks_for_issue_stage(&self, issue_id: &str, stage: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage = ?2
               AND status = 'failed'",
            params![issue_id, stage],
            |row| row.get(0),
        )?)
    }

    /// Count all attempts (completed + failed) for a given Linear issue and pipeline stage.
    /// Used as a general circuit breaker (RIG-309). Retry cap (RIG-338) uses
    /// `count_failed_tasks_for_issue_stage` instead to avoid capping successful verdicts.
    pub fn count_all_attempts_for_issue_stage(&self, issue_id: &str, stage: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage = ?2
               AND status IN ('completed', 'failed')",
            params![issue_id, stage],
            |row| row.get(0),
        )?)
    }

    /// Get the most recent `finished_at` timestamp for any completed task
    /// on the given Linear issue, excluding the current stage.
    /// Used to filter Linear comments to only those posted after the previous stage completed.
    pub fn last_stage_finished_at(
        &self,
        issue_id: &str,
        current_stage: &str,
    ) -> Result<Option<String>> {
        let result: Option<String> = self
            .conn
            .query_row(
                "SELECT finished_at FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage != ?2
               AND pipeline_stage != ''
               AND status = 'completed'
               AND finished_at IS NOT NULL
             ORDER BY finished_at DESC
             LIMIT 1",
                rusqlite::params![issue_id, current_stage],
                |row| row.get(0),
            )
            .ok();
        Ok(result)
    }

    /// Find all completed tasks with a issue_identifier where linear_pushed=false.
    pub fn unpushed_linear_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, issue_identifier, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks
             WHERE issue_identifier != '' AND linear_pushed = 0 AND status = 'completed'
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok(task_from_row(row)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row??);
        }
        Ok(tasks)
    }

    /// Check if there's a completed but unpushed task for a given issue + stage.
    pub fn has_unpushed_completed_task(&self, issue_id: &str, stage: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage = ?2
               AND status = 'completed'
               AND linear_pushed = 0",
            params![issue_id, stage],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if a task exists that should block spawning a new one for this issue + stage.
    ///
    /// Blocks on:
    /// - Active tasks (pending/running) — work in progress
    /// - Completed but unpushed tasks (callback pending) — prevents RIG-209 duplicates
    ///
    /// Does NOT block on:
    /// - Completed + pushed tasks — cycle finished (e.g. reviewer rejected, issue back
    ///   to In Progress). Allows re-spawn for new pipeline cycles (RIG-277).
    /// - Failed tasks — poll can retry those.
    pub fn has_any_nonfailed_task_for_issue_stage(
        &self,
        issue_id: &str,
        stage: &str,
    ) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage = ?2
               AND (
                   status IN ('pending', 'running')
                   OR (status = 'completed' AND linear_pushed = 0)
               )",
            params![issue_id, stage],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if any running or pending review task exists for a given issue,
    /// regardless of pipeline_stage name. Catches cross-stage duplicates where
    /// different stage names (e.g. "reviewer" vs "pipeline-reviewer") both
    /// represent review work for the same issue.
    pub fn has_any_review_task_for_issue(&self, issue_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND status IN ('pending', 'running')
               AND (pipeline_stage LIKE '%review%' OR type LIKE '%review%')",
            params![issue_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if any pipeline task is currently running for this issue (any stage).
    ///
    /// Used to prevent cross-stage races: e.g. don't launch engineer while
    /// reviewer's tmux session is still alive (RIG-296).
    pub fn has_running_pipeline_task_for_issue(&self, issue_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE issue_identifier = ?1
               AND pipeline_stage != ''
               AND status = 'running'",
            params![issue_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if callback was recently fired for a task (within `window_secs` seconds).
    pub fn is_callback_recently_fired(&self, task_id: &str, window_secs: i64) -> Result<bool> {
        let fired_at: String = self
            .conn
            .query_row(
                "SELECT COALESCE(callback_fired_at, '') FROM tasks WHERE id = ?1",
                params![task_id],
                |row| row.get(0),
            )
            .unwrap_or_default();

        if fired_at.is_empty() {
            return Ok(false);
        }

        if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(&fired_at, "%Y-%m-%dT%H:%M:%S") {
            let now = chrono::Local::now().naive_local();
            let elapsed = now.signed_duration_since(ts).num_seconds();
            return Ok(elapsed < window_secs);
        }

        Ok(false)
    }

    /// Set callback_fired_at to current timestamp.
    pub fn set_callback_fired_at(&self, task_id: &str) -> Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn.execute(
            "UPDATE tasks SET callback_fired_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;
        Ok(())
    }

    /// Check if a review task for the same target is already running or pending.
    pub fn has_active_review_task(&self, working_dir: &str, target_label: &str) -> Result<bool> {
        let escaped = target_label.replace('%', "\\%").replace('_', "\\_");
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE type = 'pipeline-reviewer'
               AND status IN ('pending', 'running')
               AND working_dir = ?1
               AND prompt LIKE ?2 ESCAPE '\\'",
            params![working_dir, format!("%Code Review: {escaped}%")],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Increment callback_attempts counter for a task. Returns the new count.
    /// Uses UPDATE ... RETURNING for atomicity (SQLite 3.35+).
    pub fn increment_callback_attempts(&self, id: &str) -> Result<i32> {
        let count: i32 = self.conn.query_row(
            "UPDATE tasks SET callback_attempts = COALESCE(callback_attempts, 0) + 1 WHERE id = ?1 RETURNING callback_attempts",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Read the current callback_attempts counter for a task without incrementing.
    /// Returns 0 if the task has no recorded attempts yet.
    pub fn get_callback_attempts(&self, id: &str) -> Result<i32> {
        let count: i32 = self.conn.query_row(
            "SELECT COALESCE(callback_attempts, 0) FROM tasks WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get the `finished_at` timestamp of the most recently failed task for an issue+stage.
    /// Returns `None` if no failed tasks exist or if `finished_at` is NULL.
    /// Used by the poller to impose a cooldown between rapid failure retries (RIG-357).
    pub fn last_failed_task_time_for_issue_stage(
        &self,
        issue_id: &str,
        stage: &str,
    ) -> Result<Option<String>> {
        let result: Option<String> = self
            .conn
            .query_row(
                "SELECT finished_at FROM tasks
                 WHERE issue_identifier = ?1
                   AND pipeline_stage = ?2
                   AND status = 'failed'
                   AND finished_at IS NOT NULL
                 ORDER BY finished_at DESC
                 LIMIT 1",
                params![issue_id, stage],
                |row| row.get(0),
            )
            .ok();
        Ok(result)
    }

    // --- PR Reviewed ---

    pub fn is_pr_reviewed(&self, pr_key: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pr_reviewed WHERE pr_key = ?1",
            params![pr_key],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_pr_reviewed(&self, pr_key: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR REPLACE INTO pr_reviewed (pr_key, updated_at) VALUES (?1, ?2)",
            params![pr_key, now],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Db, make_test_task};
    use crate::models::Status;
    use rusqlite::params;

    #[test]
    fn set_linear_pushed() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.set_linear_pushed("20260308-001", true).unwrap();
        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert!(fetched.linear_pushed);
    }

    #[test]
    fn has_unpushed_completed_task() {
        let db = Db::open_in_memory().unwrap();

        assert!(
            !db.has_unpushed_completed_task("RIG-105", "engineer")
                .unwrap()
        );

        let mut task = make_test_task("20260312-001");
        task.status = Status::Completed;
        task.issue_identifier = "RIG-105".to_string();
        task.pipeline_stage = "engineer".to_string();
        task.linear_pushed = false;
        db.insert_task(&task).unwrap();

        assert!(
            db.has_unpushed_completed_task("RIG-105", "engineer")
                .unwrap()
        );
        assert!(
            !db.has_unpushed_completed_task("RIG-999", "engineer")
                .unwrap()
        );
        assert!(
            !db.has_unpushed_completed_task("RIG-105", "reviewer")
                .unwrap()
        );

        db.set_linear_pushed("20260312-001", true).unwrap();
        assert!(
            !db.has_unpushed_completed_task("RIG-105", "engineer")
                .unwrap()
        );
    }

    #[test]
    fn has_any_nonfailed_task_for_issue_stage() {
        let db = Db::open_in_memory().unwrap();

        // No tasks → not blocked
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-209", "analyst")
                .unwrap()
        );

        // Completed + unpushed (callback pending) → blocked (RIG-209 protection)
        let mut task = make_test_task("20260313-001");
        task.status = Status::Completed;
        task.issue_identifier = "RIG-209".to_string();
        task.pipeline_stage = "analyst".to_string();
        task.linear_pushed = false;
        db.insert_task(&task).unwrap();

        assert!(
            db.has_any_nonfailed_task_for_issue_stage("RIG-209", "analyst")
                .unwrap()
        );

        // Different issue / different stage → not blocked
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-999", "analyst")
                .unwrap()
        );
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-209", "engineer")
                .unwrap()
        );

        // Completed + pushed (cycle finished) → NOT blocked (RIG-277 fix)
        db.set_linear_pushed("20260313-001", true).unwrap();
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-209", "analyst")
                .unwrap()
        );

        // Failed task → not blocked
        let mut failed_task = make_test_task("20260313-002");
        failed_task.status = Status::Failed;
        failed_task.issue_identifier = "RIG-210".to_string();
        failed_task.pipeline_stage = "analyst".to_string();
        db.insert_task(&failed_task).unwrap();

        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-210", "analyst")
                .unwrap()
        );

        // Pending task → blocked
        let mut pending_task = make_test_task("20260313-003");
        pending_task.status = Status::Pending;
        pending_task.issue_identifier = "RIG-211".to_string();
        pending_task.pipeline_stage = "engineer".to_string();
        db.insert_task(&pending_task).unwrap();

        assert!(
            db.has_any_nonfailed_task_for_issue_stage("RIG-211", "engineer")
                .unwrap()
        );

        // Running task → blocked
        db.set_task_status("20260313-003", Status::Running).unwrap();
        assert!(
            db.has_any_nonfailed_task_for_issue_stage("RIG-211", "engineer")
                .unwrap()
        );
    }

    /// RIG-277: Full rejection cycle — engineer completes, reviewer rejects,
    /// issue returns to In Progress, poller should allow new engineer spawn.
    #[test]
    fn rejection_cycle_allows_respawn() {
        let db = Db::open_in_memory().unwrap();

        // Engineer #1 completes and callback processes it (pushed=true)
        let mut eng1 = make_test_task("20260324-001");
        eng1.status = Status::Completed;
        eng1.issue_identifier = "RIG-272".to_string();
        eng1.pipeline_stage = "engineer".to_string();
        eng1.linear_pushed = true;
        db.insert_task(&eng1).unwrap();

        // Reviewer completes and callback processes it (pushed=true)
        let mut rev1 = make_test_task("20260324-002");
        rev1.status = Status::Completed;
        rev1.issue_identifier = "RIG-272".to_string();
        rev1.pipeline_stage = "reviewer".to_string();
        rev1.linear_pushed = true;
        db.insert_task(&rev1).unwrap();

        // Issue is back at In Progress after rejection — poller should allow new engineer
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-272", "engineer")
                .unwrap(),
            "completed+pushed engineer should not block re-spawn after rejection"
        );

        // Reviewer stage also unblocked for future re-review
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-272", "reviewer")
                .unwrap(),
            "completed+pushed reviewer should not block re-spawn"
        );

        // Engineer #2 spawned by poller (pending) — now blocks
        let mut eng2 = make_test_task("20260324-003");
        eng2.status = Status::Pending;
        eng2.issue_identifier = "RIG-272".to_string();
        eng2.pipeline_stage = "engineer".to_string();
        db.insert_task(&eng2).unwrap();

        assert!(
            db.has_any_nonfailed_task_for_issue_stage("RIG-272", "engineer")
                .unwrap(),
            "pending engineer #2 should block duplicate spawn"
        );
    }

    #[test]
    fn last_stage_finished_at_returns_none_when_no_tasks() {
        let db = Db::open_in_memory().unwrap();
        let result = db.last_stage_finished_at("RIG-275", "engineer").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn last_stage_finished_at_finds_previous_stage() {
        let db = Db::open_in_memory().unwrap();

        // Analyst completed with a finished_at timestamp
        let mut analyst_task = make_test_task("20260324-001");
        analyst_task.status = Status::Completed;
        analyst_task.issue_identifier = "RIG-275".to_string();
        analyst_task.pipeline_stage = "analyst".to_string();
        db.insert_task(&analyst_task).unwrap();
        db.update_task_field("20260324-001", "finished_at", "2026-03-24T10:00:00")
            .unwrap();

        // Query for engineer stage should find the analyst's finished_at
        let result = db.last_stage_finished_at("RIG-275", "engineer").unwrap();
        assert_eq!(result, Some("2026-03-24T10:00:00".to_string()));

        // Query for analyst stage should NOT return its own timestamp
        let result = db.last_stage_finished_at("RIG-275", "analyst").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn last_stage_finished_at_returns_most_recent() {
        let db = Db::open_in_memory().unwrap();

        // Two completed stages — should return the most recent
        let mut t1 = make_test_task("20260324-001");
        t1.status = Status::Completed;
        t1.issue_identifier = "RIG-275".to_string();
        t1.pipeline_stage = "analyst".to_string();
        db.insert_task(&t1).unwrap();
        db.update_task_field("20260324-001", "finished_at", "2026-03-24T09:00:00")
            .unwrap();

        let mut t2 = make_test_task("20260324-002");
        t2.status = Status::Completed;
        t2.issue_identifier = "RIG-275".to_string();
        t2.pipeline_stage = "engineer".to_string();
        db.insert_task(&t2).unwrap();
        db.update_task_field("20260324-002", "finished_at", "2026-03-24T11:00:00")
            .unwrap();

        // Reviewer should see engineer's timestamp (most recent non-self)
        let result = db.last_stage_finished_at("RIG-275", "reviewer").unwrap();
        assert_eq!(result, Some("2026-03-24T11:00:00".to_string()));
    }

    #[test]
    fn last_stage_finished_at_excludes_non_pipeline_and_different_issue() {
        let db = Db::open_in_memory().unwrap();

        // Non-pipeline task (empty pipeline_stage) — should be excluded
        let mut t1 = make_test_task("20260324-001");
        t1.status = Status::Completed;
        t1.issue_identifier = "RIG-275".to_string();
        t1.pipeline_stage = String::new();
        db.insert_task(&t1).unwrap();
        db.update_task_field("20260324-001", "finished_at", "2026-03-24T10:00:00")
            .unwrap();

        let result = db.last_stage_finished_at("RIG-275", "engineer").unwrap();
        assert!(result.is_none(), "non-pipeline tasks should be excluded");

        // Task for a different issue — should not match
        let mut t2 = make_test_task("20260324-002");
        t2.status = Status::Completed;
        t2.issue_identifier = "RIG-999".to_string();
        t2.pipeline_stage = "analyst".to_string();
        db.insert_task(&t2).unwrap();
        db.update_task_field("20260324-002", "finished_at", "2026-03-24T12:00:00")
            .unwrap();

        let result = db.last_stage_finished_at("RIG-275", "engineer").unwrap();
        assert!(result.is_none(), "different issue should not match");
    }

    #[test]
    fn pr_reviewed() {
        let db = Db::open_in_memory().unwrap();

        assert!(!db.is_pr_reviewed("repo/123").unwrap());
        db.mark_pr_reviewed("repo/123").unwrap();
        assert!(db.is_pr_reviewed("repo/123").unwrap());
    }

    #[test]
    fn pr_reviewed_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.mark_pr_reviewed("repo/1").unwrap();
        db.mark_pr_reviewed("repo/1").unwrap();
        assert!(db.is_pr_reviewed("repo/1").unwrap());
    }

    #[test]
    fn has_active_review_task_dedup() {
        let db = Db::open_in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();

        assert!(!db.has_active_review_task("/repo", "PR #8").unwrap());

        let task = crate::models::Task {
            id: "20260310-001".to_string(),
            status: crate::models::Status::Pending,
            priority: 1,
            created_at: now.clone(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-reviewer".to_string(),
            prompt: "# Code Review: PR #8\n\nReview the diff.".to_string(),
            output_path: String::new(),
            working_dir: "/repo".to_string(),
            model: "sonnet".to_string(),
            max_turns: 10,
            allowed_tools: "Read,Glob,Grep".to_string(),
            session_id: String::new(),
            issue_identifier: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
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

        assert!(db.has_active_review_task("/repo", "PR #8").unwrap());
        assert!(!db.has_active_review_task("/repo", "PR #9").unwrap());
        assert!(!db.has_active_review_task("/other-repo", "PR #8").unwrap());

        db.set_task_status("20260310-001", crate::models::Status::Completed)
            .unwrap();
        assert!(!db.has_active_review_task("/repo", "PR #8").unwrap());
    }

    #[test]
    fn callback_guard_not_fired() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        assert!(!db.is_callback_recently_fired("20260308-001", 60).unwrap());
    }

    #[test]
    fn callback_guard_recently_fired() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.set_callback_fired_at("20260308-001").unwrap();

        assert!(db.is_callback_recently_fired("20260308-001", 60).unwrap());
        assert!(!db.is_callback_recently_fired("20260308-001", 0).unwrap());
    }

    #[test]
    fn callback_guard_expired() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.conn
            .execute(
                "UPDATE tasks SET callback_fired_at = '2020-01-01T00:00:00' WHERE id = ?1",
                params!["20260308-001"],
            )
            .unwrap();

        assert!(!db.is_callback_recently_fired("20260308-001", 60).unwrap());
    }

    #[test]
    fn tasks_by_linear_issue_all_combos() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260312-001");
        t1.issue_identifier = "issue-1".to_string();
        t1.pipeline_stage = "engineer".to_string();
        db.insert_task(&t1).unwrap();

        let mut t2 = make_test_task("20260312-002");
        t2.issue_identifier = "issue-1".to_string();
        t2.pipeline_stage = "engineer".to_string();
        db.insert_task(&t2).unwrap();
        db.set_task_status("20260312-002", Status::Completed)
            .unwrap();

        let mut t3 = make_test_task("20260312-003");
        t3.issue_identifier = "issue-1".to_string();
        t3.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t3).unwrap();

        let all = db.tasks_by_linear_issue("issue-1", None, false).unwrap();
        assert_eq!(all.len(), 3);

        let active = db.tasks_by_linear_issue("issue-1", None, true).unwrap();
        assert_eq!(active.len(), 2);

        let eng = db
            .tasks_by_linear_issue("issue-1", Some("engineer"), false)
            .unwrap();
        assert_eq!(eng.len(), 2);

        let eng_active = db
            .tasks_by_linear_issue("issue-1", Some("engineer"), true)
            .unwrap();
        assert_eq!(eng_active.len(), 1);
        assert_eq!(eng_active[0].id, "20260312-001");

        let rev = db
            .tasks_by_linear_issue("issue-1", Some("reviewer"), false)
            .unwrap();
        assert_eq!(rev.len(), 1);

        let none = db.tasks_by_linear_issue("issue-999", None, false).unwrap();
        assert!(none.is_empty());
    }

    /// RIG-310: pipeline task insert must persist issue_identifier.
    #[test]
    fn pipeline_task_persists_issue_identifier() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260326-310");
        task.issue_identifier = "FAT-59".to_string();
        task.pipeline_stage = "engineer".to_string();
        task.task_type = "pipeline-engineer".to_string();
        db.insert_task(&task).unwrap();

        let read_back = db.task("20260326-310").unwrap().expect("task must exist");
        assert_eq!(
            read_back.issue_identifier, "FAT-59",
            "issue_identifier must survive insert+read round-trip"
        );
    }

    /// RIG-310: tasks_by_linear_issue must find FAT-team pipeline tasks.
    #[test]
    fn tasks_by_linear_issue_finds_fat_engineer_task() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260326-311");
        task.issue_identifier = "FAT-59".to_string();
        task.pipeline_stage = "engineer".to_string();
        task.task_type = "pipeline-engineer".to_string();
        db.insert_task(&task).unwrap();

        let found = db
            .tasks_by_linear_issue("FAT-59", Some("engineer"), false)
            .unwrap();
        assert_eq!(found.len(), 1, "must find exactly one engineer task");
        assert_eq!(found[0].id, "20260326-311");
        assert_eq!(found[0].issue_identifier, "FAT-59");
    }

    /// RIG-310: migration 004 must NOT clear FAT-* identifiers.
    #[test]
    fn migration_004_preserves_fat_identifiers() {
        let db = Db::open_in_memory().unwrap();

        let mut fat_task = make_test_task("20260326-312");
        fat_task.issue_identifier = "FAT-42".to_string();
        fat_task.pipeline_stage = "analyst".to_string();
        db.insert_task(&fat_task).unwrap();

        let mut rig_task = make_test_task("20260326-313");
        rig_task.issue_identifier = "RIG-100".to_string();
        rig_task.pipeline_stage = "engineer".to_string();
        db.insert_task(&rig_task).unwrap();

        // Re-run migration 004 to verify it doesn't nuke FAT identifiers
        db.conn
            .execute_batch(include_str!(
                "../../migrations/004_normalize_linear_ids.sql"
            ))
            .unwrap();

        let fat_read = db
            .task("20260326-312")
            .unwrap()
            .expect("FAT task must exist");
        assert_eq!(
            fat_read.issue_identifier, "FAT-42",
            "migration 004 must preserve FAT-* identifiers"
        );

        let rig_read = db
            .task("20260326-313")
            .unwrap()
            .expect("RIG task must exist");
        assert_eq!(
            rig_read.issue_identifier, "RIG-100",
            "migration 004 must preserve RIG-* identifiers"
        );
    }

    #[test]
    fn count_active_pipeline_tasks_empty() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.count_active_pipeline_tasks().unwrap(), 0);
    }

    #[test]
    fn count_active_pipeline_tasks_counts_correctly() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260312-001");
        t1.pipeline_stage = "engineer".to_string();
        t1.issue_identifier = "RIG-001".to_string();
        db.insert_task(&t1).unwrap();

        let mut t2 = make_test_task("20260312-002");
        t2.pipeline_stage = "reviewer".to_string();
        t2.issue_identifier = "RIG-001".to_string();
        db.insert_task(&t2).unwrap();
        db.set_task_status("20260312-002", Status::Running).unwrap();

        db.insert_task(&make_test_task("20260312-003")).unwrap();

        let mut t4 = make_test_task("20260312-004");
        t4.pipeline_stage = "engineer".to_string();
        t4.issue_identifier = "RIG-002".to_string();
        db.insert_task(&t4).unwrap();
        db.set_task_status("20260312-004", Status::Completed)
            .unwrap();

        assert_eq!(db.count_active_pipeline_tasks().unwrap(), 2);
    }

    #[test]
    fn count_completed_for_issue_stage() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260312-001");
        t1.issue_identifier = "issue-1".to_string();
        t1.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t1).unwrap();
        db.set_task_status("20260312-001", Status::Completed)
            .unwrap();

        let mut t2 = make_test_task("20260312-002");
        t2.issue_identifier = "issue-1".to_string();
        t2.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t2).unwrap();

        assert_eq!(
            db.count_completed_tasks_for_issue_stage("issue-1", "reviewer")
                .unwrap(),
            1
        );
        assert_eq!(
            db.count_completed_tasks_for_issue_stage("issue-1", "engineer")
                .unwrap(),
            0
        );
        assert_eq!(
            db.count_completed_tasks_for_issue_stage("issue-999", "reviewer")
                .unwrap(),
            0
        );
    }

    #[test]
    fn count_failed_for_issue_stage() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260312-010");
        t1.issue_identifier = "issue-1".to_string();
        t1.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t1).unwrap();
        db.set_task_status("20260312-010", Status::Failed).unwrap();

        let mut t2 = make_test_task("20260312-011");
        t2.issue_identifier = "issue-1".to_string();
        t2.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t2).unwrap();
        db.set_task_status("20260312-011", Status::Failed).unwrap();

        // Completed task should NOT be counted
        let mut t3 = make_test_task("20260312-012");
        t3.issue_identifier = "issue-1".to_string();
        t3.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t3).unwrap();
        db.set_task_status("20260312-012", Status::Completed)
            .unwrap();

        assert_eq!(
            db.count_failed_tasks_for_issue_stage("issue-1", "reviewer")
                .unwrap(),
            2
        );
        assert_eq!(
            db.count_failed_tasks_for_issue_stage("issue-1", "engineer")
                .unwrap(),
            0
        );
        assert_eq!(
            db.count_failed_tasks_for_issue_stage("issue-999", "reviewer")
                .unwrap(),
            0
        );
    }

    #[test]
    fn count_all_attempts_for_issue_stage() {
        let db = Db::open_in_memory().unwrap();

        // 2 failed + 1 completed = 3 total attempts
        let mut t1 = make_test_task("20260331-010");
        t1.issue_identifier = "issue-att".to_string();
        t1.pipeline_stage = "qa".to_string();
        db.insert_task(&t1).unwrap();
        db.set_task_status("20260331-010", Status::Failed).unwrap();

        let mut t2 = make_test_task("20260331-011");
        t2.issue_identifier = "issue-att".to_string();
        t2.pipeline_stage = "qa".to_string();
        db.insert_task(&t2).unwrap();
        db.set_task_status("20260331-011", Status::Completed)
            .unwrap();

        let mut t3 = make_test_task("20260331-012");
        t3.issue_identifier = "issue-att".to_string();
        t3.pipeline_stage = "qa".to_string();
        db.insert_task(&t3).unwrap();
        db.set_task_status("20260331-012", Status::Failed).unwrap();

        // Pending task should NOT be counted
        let mut t4 = make_test_task("20260331-013");
        t4.issue_identifier = "issue-att".to_string();
        t4.pipeline_stage = "qa".to_string();
        db.insert_task(&t4).unwrap();

        assert_eq!(
            db.count_all_attempts_for_issue_stage("issue-att", "qa")
                .unwrap(),
            3
        );
        assert_eq!(
            db.count_all_attempts_for_issue_stage("issue-att", "engineer")
                .unwrap(),
            0
        );
        assert_eq!(
            db.count_all_attempts_for_issue_stage("issue-999", "qa")
                .unwrap(),
            0
        );
    }

    #[test]
    fn unpushed_linear_tasks_basic() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260312-001");
        task.status = Status::Completed;
        task.issue_identifier = "RIG-100".to_string();
        task.linear_pushed = false;
        db.insert_task(&task).unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert_eq!(unpushed.len(), 1);
        assert_eq!(unpushed[0].id, "20260312-001");

        db.set_linear_pushed("20260312-001", true).unwrap();
        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty());
    }

    #[test]
    fn unpushed_excludes_no_linear_id() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260312-001");
        task.status = Status::Completed;
        // issue_identifier is empty
        db.insert_task(&task).unwrap();

        let unpushed = db.unpushed_linear_tasks().unwrap();
        assert!(unpushed.is_empty());
    }

    // ─── RIG-296: cross-stage race guard ─────────────────────────────

    #[test]
    fn has_running_pipeline_task_for_issue_empty() {
        let db = Db::open_in_memory().unwrap();
        assert!(!db.has_running_pipeline_task_for_issue("RIG-296").unwrap());
    }

    #[test]
    fn has_running_pipeline_task_for_issue_detects_running() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260325-001");
        task.issue_identifier = "RIG-296".to_string();
        task.pipeline_stage = "reviewer".to_string();
        db.insert_task(&task).unwrap();
        db.set_task_status("20260325-001", Status::Running).unwrap();

        assert!(db.has_running_pipeline_task_for_issue("RIG-296").unwrap());
        // Different issue → not blocked
        assert!(!db.has_running_pipeline_task_for_issue("RIG-999").unwrap());
    }

    #[test]
    fn has_running_pipeline_task_ignores_completed() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260325-002");
        task.issue_identifier = "RIG-296".to_string();
        task.pipeline_stage = "reviewer".to_string();
        task.status = Status::Completed;
        db.insert_task(&task).unwrap();

        assert!(!db.has_running_pipeline_task_for_issue("RIG-296").unwrap());
    }

    #[test]
    fn has_running_pipeline_task_ignores_non_pipeline() {
        let db = Db::open_in_memory().unwrap();

        // Non-pipeline task (empty pipeline_stage) should not block
        let mut task = make_test_task("20260325-003");
        task.issue_identifier = "RIG-296".to_string();
        task.pipeline_stage = String::new();
        db.insert_task(&task).unwrap();
        db.set_task_status("20260325-003", Status::Running).unwrap();

        assert!(!db.has_running_pipeline_task_for_issue("RIG-296").unwrap());
    }

    // ─── RIG-357: last_failed_task_time_for_issue_stage ─────────────────

    #[test]
    fn last_failed_task_time_none_when_no_tasks() {
        let db = Db::open_in_memory().unwrap();
        let result = db
            .last_failed_task_time_for_issue_stage("RIG-357", "reviewer")
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn last_failed_task_time_returns_most_recent() {
        let db = Db::open_in_memory().unwrap();

        // Two failed tasks with different finished_at
        let mut t1 = make_test_task("20260401-001");
        t1.issue_identifier = "RIG-357".to_string();
        t1.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t1).unwrap();
        db.set_task_status("20260401-001", Status::Failed).unwrap();
        db.update_task_field("20260401-001", "finished_at", "2026-04-01T10:00:00")
            .unwrap();

        let mut t2 = make_test_task("20260401-002");
        t2.issue_identifier = "RIG-357".to_string();
        t2.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t2).unwrap();
        db.set_task_status("20260401-002", Status::Failed).unwrap();
        db.update_task_field("20260401-002", "finished_at", "2026-04-01T11:00:00")
            .unwrap();

        let result = db
            .last_failed_task_time_for_issue_stage("RIG-357", "reviewer")
            .unwrap();
        assert_eq!(result, Some("2026-04-01T11:00:00".to_string()));
    }

    #[test]
    fn last_failed_task_time_ignores_completed_tasks() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260401-003");
        t1.issue_identifier = "RIG-357".to_string();
        t1.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t1).unwrap();
        db.set_task_status("20260401-003", Status::Completed)
            .unwrap();
        db.update_task_field("20260401-003", "finished_at", "2026-04-01T10:00:00")
            .unwrap();

        let result = db
            .last_failed_task_time_for_issue_stage("RIG-357", "reviewer")
            .unwrap();
        assert!(result.is_none(), "completed tasks should not be returned");
    }

    #[test]
    fn last_failed_task_time_filters_by_stage() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260401-004");
        t1.issue_identifier = "RIG-357".to_string();
        t1.pipeline_stage = "engineer".to_string();
        db.insert_task(&t1).unwrap();
        db.set_task_status("20260401-004", Status::Failed).unwrap();
        db.update_task_field("20260401-004", "finished_at", "2026-04-01T10:00:00")
            .unwrap();

        // Different stage → should not match
        let result = db
            .last_failed_task_time_for_issue_stage("RIG-357", "reviewer")
            .unwrap();
        assert!(result.is_none());

        // Same stage → should match
        let result = db
            .last_failed_task_time_for_issue_stage("RIG-357", "engineer")
            .unwrap();
        assert_eq!(result, Some("2026-04-01T10:00:00".to_string()));
    }
}
