use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;

use crate::models::{DailyUsage, Schedule, Status, Task};

const MIGRATION_SQL: &str = include_str!("../migrations/001_init.sql");
const MIGRATION_002_SQL: &str = include_str!("../migrations/002_repo_hash.sql");
const MIGRATION_003_SQL: &str = include_str!("../migrations/003_estimate.sql");

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open or create database at path, run migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database: {}", path.display()))?;

        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(MIGRATION_SQL)
            .context("running migrations")?;
        // 002: add repo_hash column (idempotent — ignore "duplicate column" error)
        if let Err(e) = self.conn.execute_batch(MIGRATION_002_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e).context("migration 002_repo_hash");
            }
        }
        // 003: add estimate column (idempotent — ignore "duplicate column" error)
        if let Err(e) = self.conn.execute_batch(MIGRATION_003_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e).context("migration 003_estimate");
            }
        }
        Ok(())
    }

    // --- Task CRUD ---

    /// Generate next task ID: YYYYMMDD-NNN (sequential within day).
    pub fn next_task_id(&self) -> Result<String> {
        let today = chrono::Local::now().format("%Y%m%d").to_string();
        let prefix = format!("{today}-");

        let last_id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM tasks WHERE id LIKE ?1 || '%' ORDER BY id DESC LIMIT 1",
                params![prefix],
                |row| row.get(0),
            )
            .ok();

        let next_num = match last_id {
            Some(id) => {
                let num_str = id.strip_prefix(&prefix).unwrap_or("000");
                let num: u32 = num_str.parse().unwrap_or(0);
                num + 1
            }
            None => 1,
        };

        Ok(format!("{prefix}{next_num:03}"))
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
                pipeline_stage, depends_on, context_files, repo_hash, estimate
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21
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
                    pipeline_stage, depends_on, context_files, repo_hash, estimate
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
                        pipeline_stage, depends_on, context_files, repo_hash, estimate
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
                        pipeline_stage, depends_on, context_files, repo_hash, estimate
                 FROM tasks ORDER BY priority ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([], |row| Ok(task_from_row(row)))?;
            for row in rows {
                tasks.push(row??);
            }
        }

        Ok(tasks)
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
        ];
        anyhow::ensure!(
            allowed.contains(&field),
            "field '{field}' is not allowed for update"
        );

        let sql = format!("UPDATE tasks SET {field} = ?1 WHERE id = ?2");
        self.conn.execute(&sql, params![value, id])?;
        Ok(())
    }

    /// Set linear_pushed flag.
    pub fn set_linear_pushed(&self, id: &str, pushed: bool) -> Result<()> {
        let val: i32 = if pushed { 1 } else { 0 };
        self.conn.execute(
            "UPDATE tasks SET linear_pushed = ?1 WHERE id = ?2",
            params![val, id],
        )?;
        Ok(())
    }

    /// Find next pending task with resolved dependencies.
    pub fn find_next_pending(&self) -> Result<Option<Task>> {
        let result = self.conn.query_row(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate
             FROM tasks
             WHERE status = 'pending'
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(depends_on) AS dep
                 WHERE NOT EXISTS (
                   SELECT 1 FROM tasks t2 WHERE t2.id = dep.value AND t2.status = 'completed'
                 )
               )
             ORDER BY priority ASC, created_at ASC
             LIMIT 1",
            [],
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
    pub fn claim_next_pending(&self) -> Result<Option<Task>> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;

        let result = self.conn.query_row(
            "SELECT id FROM tasks
             WHERE status = 'pending'
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(depends_on) AS dep
                 WHERE NOT EXISTS (
                   SELECT 1 FROM tasks t2 WHERE t2.id = dep.value AND t2.status = 'completed'
                 )
               )
             ORDER BY priority ASC, created_at ASC
             LIMIT 1",
            [],
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
    pub fn find_all_launchable(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate
             FROM tasks
             WHERE status = 'pending'
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(depends_on) AS dep
                 WHERE NOT EXISTS (
                   SELECT 1 FROM tasks t2 WHERE t2.id = dep.value AND t2.status = 'completed'
                 )
               )
             ORDER BY priority ASC, created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok(task_from_row(row)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row??);
        }
        Ok(tasks)
    }

    /// Delete completed tasks, return them.
    pub fn clean_completed(&self) -> Result<Vec<Task>> {
        let tasks = self.list_tasks(Some(Status::Completed))?;
        self.conn
            .execute("DELETE FROM tasks WHERE status = 'completed'", [])?;
        Ok(tasks)
    }

    // --- Schedule CRUD ---

    pub fn insert_schedule(&self, sched: &Schedule) -> Result<()> {
        let context_files = serde_json::to_string(&sched.context_files)?;
        let enabled: i32 = if sched.enabled { 1 } else { 0 };

        self.conn.execute(
            "INSERT INTO schedules (
                id, cron_expr, prompt, type, model, output_path,
                working_dir, max_turns, enabled, context_files, last_enqueued
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                sched.id,
                sched.cron_expr,
                sched.prompt,
                sched.schedule_type,
                sched.model,
                sched.output_path,
                sched.working_dir,
                sched.max_turns,
                enabled,
                context_files,
                sched.last_enqueued,
            ],
        )?;
        Ok(())
    }

    pub fn list_schedules(&self) -> Result<Vec<Schedule>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cron_expr, prompt, type, model, output_path,
                    working_dir, max_turns, enabled, context_files, last_enqueued
             FROM schedules ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let enabled_int: i32 = row.get(8)?;
            let context_files_str: String = row.get(9)?;
            let context_files: Vec<String> =
                serde_json::from_str(&context_files_str).unwrap_or_default();
            Ok(Schedule {
                id: row.get(0)?,
                cron_expr: row.get(1)?,
                prompt: row.get(2)?,
                schedule_type: row.get(3)?,
                model: row.get(4)?,
                output_path: row.get(5)?,
                working_dir: row.get(6)?,
                max_turns: row.get(7)?,
                enabled: enabled_int != 0,
                context_files,
                last_enqueued: row.get(10)?,
            })
        })?;

        let mut schedules = Vec::new();
        for row in rows {
            schedules.push(row?);
        }
        Ok(schedules)
    }

    pub fn schedule(&self, id: &str) -> Result<Option<Schedule>> {
        let result = self.conn.query_row(
            "SELECT id, cron_expr, prompt, type, model, output_path,
                    working_dir, max_turns, enabled, context_files, last_enqueued
             FROM schedules WHERE id = ?1",
            params![id],
            |row| {
                let enabled_int: i32 = row.get(8)?;
                let context_files_str: String = row.get(9)?;
                let context_files: Vec<String> =
                    serde_json::from_str(&context_files_str).unwrap_or_default();
                Ok(Schedule {
                    id: row.get(0)?,
                    cron_expr: row.get(1)?,
                    prompt: row.get(2)?,
                    schedule_type: row.get(3)?,
                    model: row.get(4)?,
                    output_path: row.get(5)?,
                    working_dir: row.get(6)?,
                    max_turns: row.get(7)?,
                    enabled: enabled_int != 0,
                    context_files,
                    last_enqueued: row.get(10)?,
                })
            },
        );

        match result {
            Ok(sched) => Ok(Some(sched)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_schedule(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_schedule_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let val: i32 = if enabled { 1 } else { 0 };
        self.conn.execute(
            "UPDATE schedules SET enabled = ?1 WHERE id = ?2",
            params![val, id],
        )?;
        Ok(())
    }

    pub fn set_schedule_last_enqueued(&self, id: &str, timestamp: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE schedules SET last_enqueued = ?1 WHERE id = ?2",
            params![timestamp, id],
        )?;
        Ok(())
    }

    /// Find tasks by linear_issue_id (optionally filter by pipeline_stage and active status).
    pub fn tasks_by_linear_issue(
        &self,
        issue_id: &str,
        stage: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<Task>> {
        let base_sql = "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate
             FROM tasks WHERE linear_issue_id = ?1";
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

    /// Count active (pending + running) tasks for a given pipeline stage.
    pub fn count_active_tasks_for_stage(&self, stage: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE pipeline_stage = ?1
               AND status IN ('pending', 'running')",
            params![stage],
            |row| row.get(0),
        )?)
    }

    /// Count completed tasks for a given Linear issue and pipeline stage.
    /// Used to track review cycles (how many times reviewer has run for an issue).
    pub fn count_completed_tasks_for_issue_stage(
        &self,
        issue_id: &str,
        stage: &str,
    ) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE linear_issue_id = ?1
               AND pipeline_stage = ?2
               AND status = 'completed'",
            params![issue_id, stage],
            |row| row.get(0),
        )?)
    }

    /// Find all completed tasks with a linear_issue_id where linear_pushed=false.
    pub fn unpushed_linear_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, priority, created_at, started_at, finished_at,
                    type, prompt, output_path, working_dir, model, max_turns,
                    allowed_tools, session_id, linear_issue_id, linear_pushed,
                    pipeline_stage, depends_on, context_files, repo_hash, estimate
             FROM tasks
             WHERE linear_issue_id != '' AND linear_pushed = 0 AND status = 'completed'
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok(task_from_row(row)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row??);
        }
        Ok(tasks)
    }

    /// Check if a review task for the same target is already running or pending.
    /// NOTE: dedup is coupled to the prompt format "# Code Review: {label}" in cmd_review().
    /// If that format changes, this query must be updated too.
    pub fn has_active_review_task(&self, working_dir: &str, target_label: &str) -> Result<bool> {
        // Escape SQL LIKE wildcards in target_label to prevent over-matching
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

    // --- Daily Usage ---

    pub fn increment_usage(&self, model: &str) -> Result<()> {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let column = match model {
            "opus" => "opus_calls",
            "sonnet" => "sonnet_calls",
            "haiku" => "haiku_calls",
            _ => anyhow::bail!("unknown model for usage tracking: {model}"),
        };

        let sql = format!(
            "INSERT INTO daily_usage (date, {column})
             VALUES (?1, 1)
             ON CONFLICT(date) DO UPDATE SET {column} = {column} + 1"
        );
        self.conn.execute(&sql, params![today])?;
        Ok(())
    }

    pub fn daily_usage(&self, date: &str) -> Result<DailyUsage> {
        let result = self.conn.query_row(
            "SELECT date, opus_calls, sonnet_calls, haiku_calls
             FROM daily_usage WHERE date = ?1",
            params![date],
            |row| {
                Ok(DailyUsage {
                    date: row.get(0)?,
                    opus_calls: row.get(1)?,
                    sonnet_calls: row.get(2)?,
                    haiku_calls: row.get(3)?,
                })
            },
        );

        match result {
            Ok(usage) => Ok(usage),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(DailyUsage {
                date: date.to_string(),
                opus_calls: 0,
                sonnet_calls: 0,
                haiku_calls: 0,
            }),
            Err(e) => Err(e.into()),
        }
    }
}

fn task_from_row(row: &rusqlite::Row<'_>) -> Result<Task> {
    let status_str: String = row.get(1)?;
    let status: Status = status_str.parse()?;
    let linear_pushed_int: i32 = row.get(15)?;
    let depends_on_str: String = row.get(17)?;
    let context_files_str: String = row.get(18)?;
    let depends_on: Vec<String> = serde_json::from_str(&depends_on_str).unwrap_or_default();
    let context_files: Vec<String> = serde_json::from_str(&context_files_str).unwrap_or_default();

    Ok(Task {
        id: row.get(0)?,
        status,
        priority: row.get(2)?,
        created_at: row.get(3)?,
        started_at: row.get(4)?,
        finished_at: row.get(5)?,
        task_type: row.get(6)?,
        prompt: row.get(7)?,
        output_path: row.get(8)?,
        working_dir: row.get(9)?,
        model: row.get(10)?,
        max_turns: row.get(11)?,
        allowed_tools: row.get(12)?,
        session_id: row.get(13)?,
        linear_issue_id: row.get(14)?,
        linear_pushed: linear_pushed_int != 0,
        pipeline_stage: row.get(16)?,
        depends_on,
        context_files,
        repo_hash: row.get(19)?,
        estimate: row.get(20).unwrap_or(0),
    })
}

fn make_test_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        status: Status::Pending,
        priority: 2,
        created_at: "2026-03-08T10:00:00Z".to_string(),
        started_at: None,
        finished_at: None,
        task_type: "research".to_string(),
        prompt: "test prompt".to_string(),
        output_path: String::new(),
        working_dir: "/tmp".to_string(),
        model: "sonnet".to_string(),
        max_turns: 15,
        allowed_tools: String::new(),
        session_id: String::new(),
        linear_issue_id: String::new(),
        linear_pushed: false,
        pipeline_stage: String::new(),
        depends_on: vec![],
        context_files: vec![],
        repo_hash: String::new(),
        estimate: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_and_migrate() {
        let db = Db::open_in_memory().unwrap();
        let counts = db.task_counts().unwrap();
        assert_eq!(counts, (0, 0, 0, 0));
    }

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

        // t1 depends on t2, t2 is pending
        let mut t1 = make_test_task("20260308-001");
        t1.depends_on = vec!["20260308-002".to_string()];
        t1.priority = 1;
        let t2 = make_test_task("20260308-002");

        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();

        // t1 has unresolved dep, so t2 should be next
        let next = db.find_next_pending().unwrap().unwrap();
        assert_eq!(next.id, "20260308-002");

        // Complete t2
        db.set_task_status("20260308-002", Status::Completed)
            .unwrap();

        // Now t1 should be next (its dep is completed)
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
        // t3 doesn't exist, so both have unresolved deps

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
    fn schedule_crud() {
        let db = Db::open_in_memory().unwrap();

        let sched = Schedule {
            id: "daily-review".to_string(),
            cron_expr: "30 7 * * *".to_string(),
            prompt: "review PRs".to_string(),
            schedule_type: "review".to_string(),
            model: "opus".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec!["file1.md".to_string()],
            last_enqueued: String::new(),
        };

        db.insert_schedule(&sched).unwrap();

        let fetched = db.schedule("daily-review").unwrap().unwrap();
        assert_eq!(fetched.cron_expr, "30 7 * * *");
        assert!(fetched.enabled);
        assert_eq!(fetched.context_files, vec!["file1.md"]);

        let all = db.list_schedules().unwrap();
        assert_eq!(all.len(), 1);

        db.set_schedule_enabled("daily-review", false).unwrap();
        let fetched = db.schedule("daily-review").unwrap().unwrap();
        assert!(!fetched.enabled);

        db.set_schedule_last_enqueued("daily-review", "2026-03-08T10:00:00Z")
            .unwrap();
        let fetched = db.schedule("daily-review").unwrap().unwrap();
        assert_eq!(fetched.last_enqueued, "2026-03-08T10:00:00Z");

        db.delete_schedule("daily-review").unwrap();
        let fetched = db.schedule("daily-review").unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn pr_reviewed() {
        let db = Db::open_in_memory().unwrap();

        assert!(!db.is_pr_reviewed("repo/123").unwrap());
        db.mark_pr_reviewed("repo/123").unwrap();
        assert!(db.is_pr_reviewed("repo/123").unwrap());
    }

    #[test]
    fn has_active_review_task_dedup() {
        let db = Db::open_in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();

        // No tasks yet — no active review
        assert!(!db.has_active_review_task("/repo", "PR #8").unwrap());

        // Insert a pending review task
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
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: vec![],
            repo_hash: String::new(),
            estimate: 0,
        };
        db.insert_task(&task).unwrap();

        // Now detected as active
        assert!(db.has_active_review_task("/repo", "PR #8").unwrap());

        // Different target — not detected
        assert!(!db.has_active_review_task("/repo", "PR #9").unwrap());

        // Different working_dir — not detected
        assert!(!db.has_active_review_task("/other-repo", "PR #8").unwrap());

        // Complete the task — no longer active
        db.set_task_status("20260310-001", crate::models::Status::Completed)
            .unwrap();
        assert!(!db.has_active_review_task("/repo", "PR #8").unwrap());
    }

    #[test]
    fn daily_usage() {
        let db = Db::open_in_memory().unwrap();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        db.increment_usage("opus").unwrap();
        db.increment_usage("opus").unwrap();
        db.increment_usage("sonnet").unwrap();

        let usage = db.daily_usage(&today).unwrap();
        assert_eq!(usage.opus_calls, 2);
        assert_eq!(usage.sonnet_calls, 1);
        assert_eq!(usage.haiku_calls, 0);
    }

    #[test]
    fn daily_usage_no_data() {
        let db = Db::open_in_memory().unwrap();
        let usage = db.daily_usage("2020-01-01").unwrap();
        assert_eq!(usage.opus_calls, 0);
        assert_eq!(usage.sonnet_calls, 0);
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
    fn set_linear_pushed() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.set_linear_pushed("20260308-001", true).unwrap();
        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert!(fetched.linear_pushed);
    }

    #[test]
    fn open_with_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("subdir/werma.db");
        let db = Db::open(&db_path).unwrap();
        let counts = db.task_counts().unwrap();
        assert_eq!(counts, (0, 0, 0, 0));
    }
}
