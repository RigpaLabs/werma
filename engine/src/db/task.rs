use anyhow::Result;
use rusqlite::params;

use crate::models::{Status, Task};

use super::task_from_row;

/// Trait for task persistence operations, enabling testability via fakes/mocks.
pub trait TaskRepository {
    fn next_task_id(&self) -> Result<String>;
    fn insert_task(&self, task: &Task) -> Result<()>;
    fn task(&self, id: &str) -> Result<Option<Task>>;
    fn list_tasks(&self, status: Option<Status>) -> Result<Vec<Task>>;
    fn list_recent_tasks(&self, status: Status, limit: usize) -> Result<Vec<Task>>;
    fn list_all_tasks_by_finished(&self, status: Status) -> Result<Vec<Task>>;
    fn list_recent_terminal_tasks(&self, limit: usize) -> Result<Vec<Task>>;
    fn set_task_status(&self, id: &str, status: Status) -> Result<()>;
    fn find_next_pending(&self) -> Result<Option<Task>>;
    fn update_task_field(&self, id: &str, field: &str, value: &str) -> Result<()>;
}

impl TaskRepository for super::Db {
    fn next_task_id(&self) -> Result<String> {
        self.next_task_id()
    }

    fn insert_task(&self, task: &Task) -> Result<()> {
        self.insert_task(task)
    }

    fn task(&self, id: &str) -> Result<Option<Task>> {
        self.task(id)
    }

    fn list_tasks(&self, status: Option<Status>) -> Result<Vec<Task>> {
        self.list_tasks(status)
    }

    fn list_recent_tasks(&self, status: Status, limit: usize) -> Result<Vec<Task>> {
        self.list_recent_tasks(status, limit)
    }

    fn list_all_tasks_by_finished(&self, status: Status) -> Result<Vec<Task>> {
        self.list_all_tasks_by_finished(status)
    }

    fn list_recent_terminal_tasks(&self, limit: usize) -> Result<Vec<Task>> {
        self.list_recent_terminal_tasks(limit)
    }

    fn set_task_status(&self, id: &str, status: Status) -> Result<()> {
        self.set_task_status(id, status)
    }

    fn find_next_pending(&self) -> Result<Option<Task>> {
        self.find_next_pending()
    }

    fn update_task_field(&self, id: &str, field: &str, value: &str) -> Result<()> {
        self.update_task_field(id, field, value)
    }
}

impl super::Db {
    /// Generate next task ID: YYYYMMDD-NNN (sequential within day).
    ///
    /// Queries MAX(seq) from DB using integer cast to handle >999 tasks correctly
    /// (lexicographic ORDER BY breaks when digit count changes).
    pub fn next_task_id(&self) -> Result<String> {
        let today = chrono::Local::now().format("%Y%m%d").to_string();
        let pattern = format!("{today}-%");

        let max_seq: Option<u32> = self
            .conn
            .query_row(
                "SELECT MAX(CAST(SUBSTR(id, 10) AS INTEGER)) FROM tasks WHERE id LIKE ?1",
                params![pattern],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        let next_num = max_seq.map_or(1, |n| n + 1);

        Ok(format!("{today}-{next_num:03}"))
    }

    /// Insert a new task.
    pub fn insert_task(&self, task: &Task) -> Result<()> {
        let depends_on = serde_json::to_string(&task.depends_on)?;
        let context_files = serde_json::to_string(&task.context_files)?;
        let linear_pushed: i32 = if task.linear_pushed { 1 } else { 0 };

        self.conn.execute(
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

    /// Get task by ID.
    pub fn task(&self, id: &str) -> Result<Option<Task>> {
        let result = self.conn.query_row(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks WHERE id = ?1",
            params![id],
            |row| Ok(task_from_row(row)),
        );

        match result {
            Ok(task) => Ok(Some(task?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List tasks, optionally filtered by status.
    pub fn list_tasks(&self, status: Option<Status>) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();

        if let Some(s) = status {
            let mut stmt = self.conn.prepare(
                "SELECT id, status, priority, created_at, started_at, finished_at,
                        type, prompt, output_path, working_dir, model, max_turns,
                        allowed_tools, session_id, linear_issue_id, linear_pushed,
                        pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
                 FROM tasks WHERE status = ?1
                 ORDER BY priority ASC, created_at ASC",
            )?;
            let rows = stmt.query_map(params![s.to_string()], |row| Ok(task_from_row(row)))?;
            for row in rows {
                tasks.push(row??);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, status, priority, created_at, started_at, finished_at,
                        type, prompt, output_path, working_dir, model, max_turns,
                        allowed_tools, session_id, linear_issue_id, linear_pushed,
                        pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
                 FROM tasks ORDER BY priority ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([], |row| Ok(task_from_row(row)))?;
            for row in rows {
                tasks.push(row??);
            }
        }

        Ok(tasks)
    }

    /// List recent tasks for a terminal status, sorted by finished_at DESC with a limit.
    /// Used by status display to show newest completed/failed/canceled tasks first.
    pub fn list_recent_tasks(&self, status: Status, limit: usize) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks WHERE status = ?1
             ORDER BY finished_at DESC, created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![status.to_string(), limit as i64], |row| {
            Ok(task_from_row(row))
        })?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row??);
        }
        Ok(tasks)
    }

    /// List all tasks for a terminal status, sorted by finished_at DESC (no limit).
    /// Used by `--all` flag to show full history with correct sort order.
    pub fn list_all_tasks_by_finished(&self, status: Status) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks WHERE status = ?1
             ORDER BY finished_at DESC, created_at DESC",
        )?;
        let rows = stmt.query_map(params![status.to_string()], |row| Ok(task_from_row(row)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row??);
        }
        Ok(tasks)
    }

    /// List the N most recent terminal tasks (completed, failed, canceled) combined,
    /// sorted by finished_at DESC. Used by `werma st` to apply a single combined limit
    /// across all three terminal statuses (not 17 per status).
    pub fn list_recent_terminal_tasks(&self, limit: usize) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks WHERE status IN ('completed', 'failed', 'canceled')
             ORDER BY finished_at DESC, created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| Ok(task_from_row(row)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row??);
        }
        Ok(tasks)
    }

    /// Count tasks for terminal statuses: (completed, failed, canceled).
    pub fn terminal_task_counts(&self) -> Result<(usize, usize, usize)> {
        let count = |status: &str| -> Result<usize> {
            Ok(self.conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE status = ?1",
                params![status],
                |row| row.get::<_, i64>(0),
            )? as usize)
        };
        Ok((count("completed")?, count("failed")?, count("canceled")?))
    }

    /// Count tasks by status: (pending, running, completed, failed).
    pub fn task_counts(&self) -> Result<(i64, i64, i64, i64)> {
        let count = |status: &str| -> Result<i64> {
            Ok(self.conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE status = ?1",
                params![status],
                |row| row.get(0),
            )?)
        };
        Ok((
            count("pending")?,
            count("running")?,
            count("completed")?,
            count("failed")?,
        ))
    }

    /// Update task status.
    pub fn set_task_status(&self, id: &str, status: Status) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET status = ?1 WHERE id = ?2",
            params![status.to_string(), id],
        )?;
        Ok(())
    }

    /// Update a single task field.
    ///
    /// Only allows known safe fields to prevent SQL injection.
    pub fn update_task_field(&self, id: &str, field: &str, value: &str) -> Result<()> {
        let allowed = [
            "session_id",
            "started_at",
            "finished_at",
            "output_path",
            "pipeline_stage",
            "allowed_tools",
            "model",
            "repo_hash",
            "estimate",
            "retry_count",
            "retry_after",
            "cost_usd",
            "turns_used",
        ];
        anyhow::ensure!(
            allowed.contains(&field),
            "field '{field}' is not allowed for update"
        );

        let sql = format!("UPDATE tasks SET {field} = ?1 WHERE id = ?2");
        self.conn.execute(&sql, params![value, id])?;
        Ok(())
    }

    /// Find next pending task with resolved dependencies.
    /// Skips tasks whose retry_after is in the future.
    pub fn find_next_pending(&self) -> Result<Option<Task>> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let result = self.conn.query_row(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks
             WHERE status = 'pending'
               AND (retry_after IS NULL OR retry_after <= ?1)
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(depends_on) AS dep
                 WHERE NOT EXISTS (
                   SELECT 1 FROM tasks t2 WHERE t2.id = dep.value AND t2.status = 'completed'
                 )
               )
             ORDER BY priority ASC, created_at ASC
             LIMIT 1",
            params![now],
            |row| Ok(task_from_row(row)),
        );

        match result {
            Ok(task) => Ok(Some(task?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically find the next pending task and mark it as running.
    /// Uses BEGIN IMMEDIATE to prevent two callers from claiming the same task.
    /// Skips tasks whose retry_after is in the future.
    pub fn claim_next_pending(&self) -> Result<Option<Task>> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;

        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let result = self.conn.query_row(
            "SELECT id FROM tasks
             WHERE status = 'pending'
               AND (retry_after IS NULL OR retry_after <= ?1)
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(depends_on) AS dep
                 WHERE NOT EXISTS (
                   SELECT 1 FROM tasks t2 WHERE t2.id = dep.value AND t2.status = 'completed'
                 )
               )
               -- RIG-296: cross-stage guard — don't launch a pipeline task if another
               -- pipeline task for the same issue is still running. Prevents reviewer
               -- and engineer from running simultaneously on the same issue.
               AND NOT (
                 linear_issue_id != ''
                 AND pipeline_stage != ''
                 AND EXISTS (
                   SELECT 1 FROM tasks t3
                   WHERE t3.linear_issue_id = tasks.linear_issue_id
                     AND t3.pipeline_stage != ''
                     AND t3.status = 'running'
                     AND t3.id != tasks.id
                 )
               )
             ORDER BY priority ASC, created_at ASC
             LIMIT 1",
            params![now],
            |row| row.get::<_, String>(0),
        );

        let task_id = match result {
            Ok(id) => id,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute("ROLLBACK", [])?;
                return Ok(None);
            }
            Err(e) => {
                self.conn.execute("ROLLBACK", [])?;
                return Err(e.into());
            }
        };

        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn.execute(
            "UPDATE tasks SET status = 'running', started_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;
        self.conn.execute("COMMIT", [])?;

        // Now fetch the full task
        self.task(&task_id)
    }

    /// Find all pending tasks with resolved dependencies (for run-all wave execution).
    /// Skips tasks whose retry_after is in the future.
    pub fn find_all_launchable(&self) -> Result<Vec<Task>> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut stmt = self.conn.prepare(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate,
                    retry_count, retry_after, cost_usd, turns_used, handoff_content
             FROM tasks
             WHERE status = 'pending'
               AND (retry_after IS NULL OR retry_after <= ?1)
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(depends_on) AS dep
                 WHERE NOT EXISTS (
                   SELECT 1 FROM tasks t2 WHERE t2.id = dep.value AND t2.status = 'completed'
                 )
               )
             ORDER BY priority ASC, created_at ASC",
        )?;
        let rows = stmt.query_map(params![now], |row| Ok(task_from_row(row)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row??);
        }
        Ok(tasks)
    }

    /// Enqueue a failed task for retry: set status to Pending, increment retry_count,
    /// set retry_after to now + delay_secs, and clear started_at/finished_at.
    /// Atomically enqueue a task for retry. Returns `true` if the retry was applied,
    /// `false` if another caller already incremented past `max_retries` (CAS guard).
    pub fn enqueue_retry(&self, id: &str, delay_secs: u64, max_retries: u32) -> Result<bool> {
        let retry_after = (chrono::Local::now() + chrono::Duration::seconds(delay_secs as i64))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();

        let rows = self.conn.execute(
            "UPDATE tasks SET status = 'pending',
                              retry_count = retry_count + 1,
                              retry_after = ?1,
                              session_id = '',
                              started_at = NULL,
                              finished_at = NULL
             WHERE id = ?2 AND retry_count < ?3",
            params![retry_after, id, max_retries],
        )?;
        Ok(rows > 0)
    }

    /// Reset retry_count and retry_after (used by manual `werma retry`).
    pub fn reset_retry(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET retry_count = 0, retry_after = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Delete completed tasks, return them.
    pub fn clean_completed(&self) -> Result<Vec<Task>> {
        let tasks = self.list_tasks(Some(Status::Completed))?;
        self.conn
            .execute("DELETE FROM tasks WHERE status = 'completed'", [])?;
        Ok(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Db, make_test_task};
    use crate::models::Status;

    #[test]
    fn insert_and_get_task() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.id, "20260308-001");
        assert_eq!(fetched.status, Status::Pending);
        assert_eq!(fetched.prompt, "test prompt");
        assert_eq!(fetched.estimate, 0);
    }

    #[test]
    fn task_not_found() {
        let db = Db::open_in_memory().unwrap();
        let fetched = db.task("nonexistent").unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn list_tasks_filter_by_status() {
        let db = Db::open_in_memory().unwrap();

        let t1 = make_test_task("20260308-001");
        let mut t2 = make_test_task("20260308-002");
        t2.status = Status::Completed;
        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();

        let pending = db.list_tasks(Some(Status::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "20260308-001");

        let all = db.list_tasks(None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn set_task_status() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.set_task_status("20260308-001", Status::Running).unwrap();
        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.status, Status::Running);
    }

    #[test]
    fn task_counts() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260308-001");
        let mut t2 = make_test_task("20260308-002");
        t2.status = Status::Running;
        let mut t3 = make_test_task("20260308-003");
        t3.status = Status::Completed;
        t1.priority = 1;

        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();
        db.insert_task(&t3).unwrap();

        let counts = db.task_counts().unwrap();
        assert_eq!(counts, (1, 1, 1, 0));
    }

    #[test]
    fn find_next_pending_no_deps() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260308-001");
        t1.priority = 3;
        let mut t2 = make_test_task("20260308-002");
        t2.priority = 1;

        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();

        let next = db.find_next_pending().unwrap().unwrap();
        assert_eq!(next.id, "20260308-002"); // lower priority number = higher priority
    }

    #[test]
    fn find_next_pending_with_deps() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260308-001");
        t1.depends_on = vec!["20260308-002".to_string()];
        t1.priority = 1;
        let t2 = make_test_task("20260308-002");

        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();

        let next = db.find_next_pending().unwrap().unwrap();
        assert_eq!(next.id, "20260308-002");

        db.set_task_status("20260308-002", Status::Completed)
            .unwrap();

        let next = db.find_next_pending().unwrap().unwrap();
        assert_eq!(next.id, "20260308-001");
    }

    #[test]
    fn find_next_pending_all_deps_unresolved() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260308-001");
        t1.depends_on = vec!["20260308-002".to_string()];
        let mut t2 = make_test_task("20260308-002");
        t2.depends_on = vec!["20260308-003".to_string()];

        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();

        let next = db.find_next_pending().unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn next_task_id() {
        let db = Db::open_in_memory().unwrap();
        let id1 = db.next_task_id().unwrap();
        assert!(id1.ends_with("-001"));

        let mut task = make_test_task(&id1);
        task.id = id1.clone();
        db.insert_task(&task).unwrap();

        let id2 = db.next_task_id().unwrap();
        assert!(id2.ends_with("-002"));
    }

    #[test]
    fn next_task_id_sequential() {
        let db = Db::open_in_memory().unwrap();

        let id1 = db.next_task_id().unwrap();
        db.insert_task(&make_test_task(&id1)).unwrap();

        let id2 = db.next_task_id().unwrap();
        db.insert_task(&make_test_task(&id2)).unwrap();

        let id3 = db.next_task_id().unwrap();

        assert!(id1.ends_with("-001"));
        assert!(id2.ends_with("-002"));
        assert!(id3.ends_with("-003"));

        let prefix1 = id1.split('-').next().unwrap();
        let prefix2 = id2.split('-').next().unwrap();
        assert_eq!(prefix1, prefix2);
    }

    #[test]
    fn next_task_id_beyond_999() {
        let db = Db::open_in_memory().unwrap();
        let today = chrono::Local::now().format("%Y%m%d").to_string();

        // Insert tasks 001 through 999
        for i in 1..=999 {
            let id = format!("{today}-{i:03}");
            db.insert_task(&make_test_task(&id)).unwrap();
        }

        let next = db.next_task_id().unwrap();
        assert_eq!(next, format!("{today}-1000"));
    }

    #[test]
    fn next_task_id_beyond_1500() {
        let db = Db::open_in_memory().unwrap();
        let today = chrono::Local::now().format("%Y%m%d").to_string();

        // Insert tasks up to 1500
        for i in 1..=1500 {
            let id = format!("{today}-{i:03}");
            db.insert_task(&make_test_task(&id)).unwrap();
        }

        let next = db.next_task_id().unwrap();
        assert_eq!(next, format!("{today}-1501"));
    }

    #[test]
    fn next_task_id_ignores_other_days() {
        let db = Db::open_in_memory().unwrap();

        // Insert tasks from a different day
        for i in 1..=500 {
            let id = format!("20250101-{i:03}");
            db.insert_task(&make_test_task(&id)).unwrap();
        }

        // Today should start fresh at 001
        let next = db.next_task_id().unwrap();
        assert!(next.ends_with("-001"));
        // And it should NOT be prefixed with the old date
        assert!(!next.starts_with("20250101"));
    }

    #[test]
    fn clean_completed() {
        let db = Db::open_in_memory().unwrap();

        let t1 = make_test_task("20260308-001");
        let mut t2 = make_test_task("20260308-002");
        t2.status = Status::Completed;

        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();

        let cleaned = db.clean_completed().unwrap();
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].id, "20260308-002");

        let all = db.list_tasks(None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn clean_completed_empty() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260313-001");
        db.insert_task(&task).unwrap();

        let cleaned = db.clean_completed().unwrap();
        assert!(cleaned.is_empty());

        let all = db.list_tasks(None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn update_task_field() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.update_task_field("20260308-001", "session_id", "abc-123")
            .unwrap();
        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.session_id, "abc-123");
    }

    #[test]
    fn update_task_field_estimate() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.update_task_field("20260308-001", "estimate", "8")
            .unwrap();
        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.estimate, 8);
    }

    #[test]
    fn update_task_field_disallowed() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        let result = db.update_task_field("20260308-001", "prompt", "hacked");
        assert!(result.is_err());
    }

    #[test]
    fn task_counts_all_statuses() {
        let db = Db::open_in_memory().unwrap();

        db.insert_task(&make_test_task("20260313-001")).unwrap();
        db.insert_task(&make_test_task("20260313-002")).unwrap();

        db.insert_task(&make_test_task("20260313-003")).unwrap();
        db.set_task_status("20260313-003", Status::Running).unwrap();

        db.insert_task(&make_test_task("20260313-004")).unwrap();
        db.set_task_status("20260313-004", Status::Completed)
            .unwrap();

        db.insert_task(&make_test_task("20260313-005")).unwrap();
        db.set_task_status("20260313-005", Status::Failed).unwrap();

        let (p, r, c, f) = db.task_counts().unwrap();
        assert_eq!(p, 2);
        assert_eq!(r, 1);
        assert_eq!(c, 1);
        assert_eq!(f, 1);
    }

    // ─── claim_next_pending ─────────────────────────────────────────────────

    #[test]
    fn claim_next_pending_empty_db() {
        let db = Db::open_in_memory().unwrap();
        let claimed = db.claim_next_pending().unwrap();
        assert!(claimed.is_none());
    }

    #[test]
    fn claim_next_pending_sets_running() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260312-001");
        db.insert_task(&task).unwrap();

        let claimed = db.claim_next_pending().unwrap();
        assert!(claimed.is_some());
        let claimed = claimed.unwrap();
        assert_eq!(claimed.id, "20260312-001");
        assert_eq!(claimed.status, Status::Running);
        assert!(claimed.started_at.is_some());
    }

    #[test]
    fn claim_next_pending_respects_priority() {
        let db = Db::open_in_memory().unwrap();

        let mut low_prio = make_test_task("20260312-002");
        low_prio.priority = 3;
        let mut high_prio = make_test_task("20260312-001");
        high_prio.priority = 1;

        db.insert_task(&low_prio).unwrap();
        db.insert_task(&high_prio).unwrap();

        let claimed = db.claim_next_pending().unwrap().unwrap();
        assert_eq!(claimed.id, "20260312-001");
    }

    #[test]
    fn claim_next_pending_skips_unresolved_deps() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260312-002");
        task.depends_on = vec!["20260312-001".to_string()];
        db.insert_task(&task).unwrap();

        let claimed = db.claim_next_pending().unwrap();
        assert!(claimed.is_none());
    }

    #[test]
    fn claim_next_pending_with_resolved_deps() {
        let db = Db::open_in_memory().unwrap();

        let mut dep = make_test_task("20260312-001");
        dep.status = Status::Completed;
        db.insert_task(&dep).unwrap();

        let mut task = make_test_task("20260312-002");
        task.depends_on = vec!["20260312-001".to_string()];
        db.insert_task(&task).unwrap();

        let claimed = db.claim_next_pending().unwrap().unwrap();
        assert_eq!(claimed.id, "20260312-002");
    }

    #[test]
    fn claim_next_pending_no_double_claim() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260312-001");
        db.insert_task(&task).unwrap();

        let first = db.claim_next_pending().unwrap();
        assert!(first.is_some());
        let second = db.claim_next_pending().unwrap();
        assert!(second.is_none());
    }

    /// RIG-296: cross-stage guard — pending engineer should NOT be claimed
    /// while reviewer is still running for the same issue.
    #[test]
    fn claim_next_pending_blocks_cross_stage_conflict() {
        let db = Db::open_in_memory().unwrap();

        // Reviewer is running for RIG-296
        let mut reviewer = make_test_task("20260325-rev");
        reviewer.linear_issue_id = "RIG-296".to_string();
        reviewer.pipeline_stage = "reviewer".to_string();
        db.insert_task(&reviewer).unwrap();
        db.set_task_status("20260325-rev", Status::Running).unwrap();

        // Engineer is pending for the same issue
        let mut engineer = make_test_task("20260325-eng");
        engineer.linear_issue_id = "RIG-296".to_string();
        engineer.pipeline_stage = "engineer".to_string();
        db.insert_task(&engineer).unwrap();

        // Should NOT claim engineer — reviewer is still running
        let claimed = db.claim_next_pending().unwrap();
        assert!(
            claimed.is_none(),
            "should not claim engineer while reviewer is running for same issue"
        );

        // Once reviewer completes, engineer becomes claimable
        db.set_task_status("20260325-rev", Status::Completed)
            .unwrap();
        let claimed = db.claim_next_pending().unwrap();
        assert!(claimed.is_some());
        assert_eq!(claimed.unwrap().id, "20260325-eng");
    }

    /// RIG-296: cross-stage guard should NOT block tasks for different issues.
    #[test]
    fn claim_next_pending_allows_different_issues() {
        let db = Db::open_in_memory().unwrap();

        // Reviewer running for RIG-100
        let mut reviewer = make_test_task("20260325-rev");
        reviewer.linear_issue_id = "RIG-100".to_string();
        reviewer.pipeline_stage = "reviewer".to_string();
        db.insert_task(&reviewer).unwrap();
        db.set_task_status("20260325-rev", Status::Running).unwrap();

        // Engineer pending for RIG-200 (different issue)
        let mut engineer = make_test_task("20260325-eng");
        engineer.linear_issue_id = "RIG-200".to_string();
        engineer.pipeline_stage = "engineer".to_string();
        db.insert_task(&engineer).unwrap();

        // Should claim — different issue
        let claimed = db.claim_next_pending().unwrap();
        assert!(claimed.is_some());
        assert_eq!(claimed.unwrap().id, "20260325-eng");
    }

    /// RIG-296: non-pipeline tasks should not be blocked by the cross-stage guard.
    #[test]
    fn claim_next_pending_allows_non_pipeline_tasks() {
        let db = Db::open_in_memory().unwrap();

        // Pipeline reviewer running for RIG-296
        let mut reviewer = make_test_task("20260325-rev");
        reviewer.linear_issue_id = "RIG-296".to_string();
        reviewer.pipeline_stage = "reviewer".to_string();
        db.insert_task(&reviewer).unwrap();
        db.set_task_status("20260325-rev", Status::Running).unwrap();

        // Non-pipeline task (no pipeline_stage, no linear_issue_id) should still be claimable
        let mut adhoc = make_test_task("20260325-adhoc");
        adhoc.linear_issue_id = String::new();
        adhoc.pipeline_stage = String::new();
        db.insert_task(&adhoc).unwrap();

        let claimed = db.claim_next_pending().unwrap();
        assert!(claimed.is_some());
        assert_eq!(claimed.unwrap().id, "20260325-adhoc");
    }

    // ─── find_all_launchable ────────────────────────────────────────────────

    #[test]
    fn find_all_launchable_empty() {
        let db = Db::open_in_memory().unwrap();
        let tasks = db.find_all_launchable().unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn find_all_launchable_returns_multiple() {
        let db = Db::open_in_memory().unwrap();
        db.insert_task(&make_test_task("20260312-001")).unwrap();
        db.insert_task(&make_test_task("20260312-002")).unwrap();

        let tasks = db.find_all_launchable().unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn find_all_launchable_excludes_unresolved_deps() {
        let db = Db::open_in_memory().unwrap();
        db.insert_task(&make_test_task("20260312-001")).unwrap();

        let mut with_dep = make_test_task("20260312-002");
        with_dep.depends_on = vec!["20260312-999".to_string()];
        db.insert_task(&with_dep).unwrap();

        let tasks = db.find_all_launchable().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "20260312-001");
    }

    #[test]
    fn find_all_launchable_skips_running() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260312-001");
        db.insert_task(&task).unwrap();
        db.set_task_status("20260312-001", Status::Running).unwrap();

        let tasks = db.find_all_launchable().unwrap();
        assert!(tasks.is_empty());
    }

    // ─── boundary tests ────────────────────────────────────────────────────

    #[test]
    fn task_with_unicode_fields() {
        let db = Db::open_in_memory().unwrap();
        let mut task = make_test_task("20260308-001");
        task.prompt = "проверить 日本語 ✨ emoji".to_string();
        task.working_dir = "/tmp/тест".to_string();
        db.insert_task(&task).unwrap();

        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.prompt, "проверить 日本語 ✨ emoji");
        assert_eq!(fetched.working_dir, "/tmp/тест");
    }

    #[test]
    fn task_with_empty_depends_on() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert!(fetched.depends_on.is_empty());
    }

    #[test]
    fn list_tasks_empty_table() {
        let db = Db::open_in_memory().unwrap();
        let tasks = db.list_tasks(None).unwrap();
        assert!(tasks.is_empty());
        let tasks = db.list_tasks(Some(Status::Pending)).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn duplicate_task_id_errors() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();
        let result = db.insert_task(&task);
        assert!(result.is_err());
    }

    #[test]
    fn update_task_field_all_allowed_fields() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        let allowed = [
            "session_id",
            "started_at",
            "finished_at",
            "output_path",
            "pipeline_stage",
            "allowed_tools",
            "model",
            "repo_hash",
            "estimate",
        ];
        for field in allowed {
            db.update_task_field("20260308-001", field, "test_value")
                .unwrap();
        }
    }

    // ─── list_recent_tasks ────────────────────────────────────────────────

    #[test]
    fn list_recent_tasks_sorted_by_finished_at_desc() {
        let db = Db::open_in_memory().unwrap();

        // Insert 3 completed tasks with different finished_at times
        let mut t1 = make_test_task("20260310-001");
        t1.status = Status::Completed;
        t1.finished_at = Some("2026-03-10T10:00:00".to_string());
        db.insert_task(&t1).unwrap();

        let mut t2 = make_test_task("20260324-001");
        t2.status = Status::Completed;
        t2.finished_at = Some("2026-03-24T15:00:00".to_string());
        db.insert_task(&t2).unwrap();

        let mut t3 = make_test_task("20260320-001");
        t3.status = Status::Completed;
        t3.finished_at = Some("2026-03-20T12:00:00".to_string());
        db.insert_task(&t3).unwrap();

        let recent = db.list_recent_tasks(Status::Completed, 10).unwrap();
        assert_eq!(recent.len(), 3);
        // Newest first
        assert_eq!(recent[0].id, "20260324-001");
        assert_eq!(recent[1].id, "20260320-001");
        assert_eq!(recent[2].id, "20260310-001");
    }

    #[test]
    fn list_recent_tasks_respects_limit() {
        let db = Db::open_in_memory().unwrap();

        for i in 1..=15 {
            let mut t = make_test_task(&format!("20260324-{i:03}"));
            t.status = Status::Completed;
            t.finished_at = Some(format!("2026-03-24T{i:02}:00:00"));
            db.insert_task(&t).unwrap();
        }

        let recent = db.list_recent_tasks(Status::Completed, 10).unwrap();
        assert_eq!(recent.len(), 10);
        // First result is the newest (finished_at hour 15)
        assert_eq!(recent[0].id, "20260324-015");
        // Last result is the 6th newest (finished_at hour 6)
        assert_eq!(recent[9].id, "20260324-006");
    }

    #[test]
    fn list_recent_tasks_different_priorities_still_sorted_by_finished_at() {
        let db = Db::open_in_memory().unwrap();

        // Ancient task with low priority number (high priority)
        let mut old = make_test_task("20260310-001");
        old.status = Status::Completed;
        old.priority = 1;
        old.finished_at = Some("2026-03-10T10:00:00".to_string());
        db.insert_task(&old).unwrap();

        // Recent task with high priority number (low priority)
        let mut new = make_test_task("20260324-001");
        new.status = Status::Completed;
        new.priority = 99;
        new.finished_at = Some("2026-03-24T15:00:00".to_string());
        db.insert_task(&new).unwrap();

        let recent = db.list_recent_tasks(Status::Completed, 10).unwrap();
        assert_eq!(recent.len(), 2);
        // Recent task must come first regardless of priority
        assert_eq!(recent[0].id, "20260324-001");
        assert_eq!(recent[1].id, "20260310-001");
    }

    #[test]
    fn list_recent_tasks_empty() {
        let db = Db::open_in_memory().unwrap();
        let recent = db.list_recent_tasks(Status::Completed, 10).unwrap();
        assert!(recent.is_empty());
    }

    #[test]
    fn list_recent_tasks_filters_by_status() {
        let db = Db::open_in_memory().unwrap();

        let mut completed = make_test_task("20260324-001");
        completed.status = Status::Completed;
        completed.finished_at = Some("2026-03-24T10:00:00".to_string());
        db.insert_task(&completed).unwrap();

        let mut failed = make_test_task("20260324-002");
        failed.status = Status::Failed;
        failed.finished_at = Some("2026-03-24T11:00:00".to_string());
        db.insert_task(&failed).unwrap();

        let recent_completed = db.list_recent_tasks(Status::Completed, 10).unwrap();
        assert_eq!(recent_completed.len(), 1);
        assert_eq!(recent_completed[0].id, "20260324-001");

        let recent_failed = db.list_recent_tasks(Status::Failed, 10).unwrap();
        assert_eq!(recent_failed.len(), 1);
        assert_eq!(recent_failed[0].id, "20260324-002");
    }

    // ─── list_all_tasks_by_finished ─────────────────────────────────────

    #[test]
    fn list_all_tasks_by_finished_sorted_desc_no_limit() {
        let db = Db::open_in_memory().unwrap();

        // Insert 15 completed tasks with different finished_at times
        for i in 1..=15 {
            let mut t = make_test_task(&format!("20260324-{i:03}"));
            t.status = Status::Completed;
            t.finished_at = Some(format!("2026-03-24T{i:02}:00:00"));
            db.insert_task(&t).unwrap();
        }

        let all = db.list_all_tasks_by_finished(Status::Completed).unwrap();
        // No limit — all 15 returned
        assert_eq!(all.len(), 15);
        // Newest first
        assert_eq!(all[0].id, "20260324-015");
        assert_eq!(all[14].id, "20260324-001");
    }

    #[test]
    fn list_all_tasks_by_finished_ignores_priority() {
        let db = Db::open_in_memory().unwrap();

        // High priority (low number) but old
        let mut old = make_test_task("20260310-001");
        old.status = Status::Completed;
        old.priority = 1;
        old.finished_at = Some("2026-03-10T10:00:00".to_string());
        db.insert_task(&old).unwrap();

        // Low priority but recent
        let mut new = make_test_task("20260324-001");
        new.status = Status::Completed;
        new.priority = 99;
        new.finished_at = Some("2026-03-24T15:00:00".to_string());
        db.insert_task(&new).unwrap();

        let all = db.list_all_tasks_by_finished(Status::Completed).unwrap();
        assert_eq!(all.len(), 2);
        // Recent task first regardless of priority
        assert_eq!(all[0].id, "20260324-001");
        assert_eq!(all[1].id, "20260310-001");
    }

    #[test]
    fn terminal_task_counts() {
        let db = Db::open_in_memory().unwrap();

        let mut t1 = make_test_task("20260324-001");
        t1.status = Status::Completed;
        db.insert_task(&t1).unwrap();

        let mut t2 = make_test_task("20260324-002");
        t2.status = Status::Completed;
        db.insert_task(&t2).unwrap();

        let mut t3 = make_test_task("20260324-003");
        t3.status = Status::Failed;
        db.insert_task(&t3).unwrap();

        let mut t4 = make_test_task("20260324-004");
        t4.status = Status::Canceled;
        db.insert_task(&t4).unwrap();

        let (c, f, x) = db.terminal_task_counts().unwrap();
        assert_eq!(c, 2);
        assert_eq!(f, 1);
        assert_eq!(x, 1);
    }

    // ─── list_recent_terminal_tasks ──────────────────────────────────────

    #[test]
    fn list_recent_terminal_tasks_combined_limit() {
        let db = Db::open_in_memory().unwrap();

        // 10 completed + 5 failed + 3 canceled = 18 terminal tasks total
        for i in 1..=10u32 {
            let mut t = make_test_task(&format!("20260310-c{i:02}"));
            t.status = Status::Completed;
            t.finished_at = Some(format!("2026-03-10T10:{i:02}:00"));
            db.insert_task(&t).unwrap();
        }
        for i in 1..=5u32 {
            let mut t = make_test_task(&format!("20260310-f{i:02}"));
            t.status = Status::Failed;
            t.finished_at = Some(format!("2026-03-10T11:{i:02}:00"));
            db.insert_task(&t).unwrap();
        }
        for i in 1..=3u32 {
            let mut t = make_test_task(&format!("20260310-x{i:02}"));
            t.status = Status::Canceled;
            t.finished_at = Some(format!("2026-03-10T12:{i:02}:00"));
            db.insert_task(&t).unwrap();
        }

        // limit=17 should return exactly 17 total (not 17+17+17)
        let tasks = db.list_recent_terminal_tasks(17).unwrap();
        assert_eq!(
            tasks.len(),
            17,
            "combined limit must cap total, not per-status"
        );

        // All 18 without limit
        let all = db.list_recent_terminal_tasks(18).unwrap();
        assert_eq!(all.len(), 18);

        // Tasks are sorted by finished_at DESC — most recent first
        assert!(all[0].finished_at >= all[1].finished_at);
    }

    #[test]
    fn list_recent_terminal_tasks_fewer_than_limit() {
        let db = Db::open_in_memory().unwrap();
        let mut t = make_test_task("20260310-001");
        t.status = Status::Completed;
        t.finished_at = Some("2026-03-10T10:00:00".to_string());
        db.insert_task(&t).unwrap();

        let tasks = db.list_recent_terminal_tasks(17).unwrap();
        assert_eq!(tasks.len(), 1, "returns all tasks when fewer than limit");
    }
}
