use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;

use crate::models::{DailyUsage, Schedule, Status, Task};

const MIGRATION_SQL: &str = include_str!("../migrations/001_init.sql");
const MIGRATION_002_SQL: &str = include_str!("../migrations/002_repo_hash.sql");
const MIGRATION_003_SQL: &str = include_str!("../migrations/003_estimate.sql");
const MIGRATION_004_SQL: &str = include_str!("../migrations/004_normalize_linear_ids.sql");
const MIGRATION_005_SQL: &str = include_str!("../migrations/005_callback_fired_at.sql");

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
        // 004: normalize linear_issue_id from UUIDs to identifiers (idempotent)
        self.conn
            .execute_batch(MIGRATION_004_SQL)
            .context("migration 004_normalize_linear_ids")?;
        // 005: add callback_fired_at column (idempotent — ignore "duplicate column" error)
        if let Err(e) = self.conn.execute_batch(MIGRATION_005_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e).context("migration 005_callback_fired_at");
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

    /// Check if there's a completed but unpushed task for a given issue + stage.
    /// Used to prevent poll from spawning duplicates while callback is pending.
    pub fn has_unpushed_completed_task(&self, issue_id: &str, stage: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE linear_issue_id = ?1
               AND pipeline_stage = ?2
               AND status = 'completed'
               AND linear_pushed = 0",
            params![issue_id, stage],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if any non-failed task exists for a given issue + stage.
    /// Covers pending, running, AND completed tasks (regardless of linear_pushed).
    /// Used by poll() to prevent re-spawning tasks for issues that have already been
    /// processed — even when the callback succeeded but the Linear status didn't move.
    pub fn has_any_nonfailed_task_for_issue_stage(
        &self,
        issue_id: &str,
        stage: &str,
    ) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE linear_issue_id = ?1
               AND pipeline_stage = ?2
               AND status IN ('pending', 'running', 'completed')",
            params![issue_id, stage],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if callback was recently fired for a task (within `window_secs` seconds).
    /// Used to prevent duplicate callback execution from overlapping daemon ticks.
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
    /// Must be called synchronously before any callback work to prevent duplicates.
    pub fn set_callback_fired_at(&self, task_id: &str) -> Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn.execute(
            "UPDATE tasks SET callback_fired_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;
        Ok(())
    }

    /// Clear callback_fired_at (set to NULL) so the dedup guard no longer blocks retries.
    /// Called when callback_inner() fails — allows the next daemon tick to retry.
    pub fn clear_callback_fired_at(&self, task_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET callback_fired_at = NULL WHERE id = ?1",
            params![task_id],
        )?;
        Ok(())
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
    fn callback_guard_not_fired() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        // Not fired yet — should return false
        assert!(!db.is_callback_recently_fired("20260308-001", 60).unwrap());
    }

    #[test]
    fn callback_guard_recently_fired() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        db.set_callback_fired_at("20260308-001").unwrap();

        // Just fired — should return true within 60s window
        assert!(db.is_callback_recently_fired("20260308-001", 60).unwrap());

        // With 0s window — should return false (already elapsed)
        assert!(!db.is_callback_recently_fired("20260308-001", 0).unwrap());
    }

    #[test]
    fn callback_guard_expired() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        // Set a timestamp far in the past
        db.conn
            .execute(
                "UPDATE tasks SET callback_fired_at = '2020-01-01T00:00:00' WHERE id = ?1",
                params!["20260308-001"],
            )
            .unwrap();

        // Should be expired (>60s ago)
        assert!(!db.is_callback_recently_fired("20260308-001", 60).unwrap());
    }

    #[test]
    fn callback_guard_clear_allows_retry() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();

        // Set the guard
        db.set_callback_fired_at("20260308-001").unwrap();
        assert!(db.is_callback_recently_fired("20260308-001", 60).unwrap());

        // Clear it — should allow retry
        db.clear_callback_fired_at("20260308-001").unwrap();
        assert!(!db.is_callback_recently_fired("20260308-001", 60).unwrap());
    }

    #[test]
    fn has_unpushed_completed_task() {
        let db = Db::open_in_memory().unwrap();

        // No tasks: should be false
        assert!(
            !db.has_unpushed_completed_task("RIG-105", "engineer")
                .unwrap()
        );

        // Insert a completed, unpushed pipeline task
        let mut task = make_test_task("20260312-001");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-105".to_string();
        task.pipeline_stage = "engineer".to_string();
        task.linear_pushed = false;
        db.insert_task(&task).unwrap();

        // Should find it
        assert!(
            db.has_unpushed_completed_task("RIG-105", "engineer")
                .unwrap()
        );
        // Different issue: not found
        assert!(
            !db.has_unpushed_completed_task("RIG-999", "engineer")
                .unwrap()
        );
        // Different stage: not found
        assert!(
            !db.has_unpushed_completed_task("RIG-105", "reviewer")
                .unwrap()
        );

        // After marking pushed: should not find it
        db.set_linear_pushed("20260312-001", true).unwrap();
        assert!(
            !db.has_unpushed_completed_task("RIG-105", "engineer")
                .unwrap()
        );
    }

    #[test]
    fn has_any_nonfailed_task_for_issue_stage() {
        let db = Db::open_in_memory().unwrap();

        // No tasks: should be false
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-209", "analyst")
                .unwrap()
        );

        // Insert a completed + pushed task (the gap that RIG-209 fixes)
        let mut task = make_test_task("20260313-001");
        task.status = Status::Completed;
        task.linear_issue_id = "RIG-209".to_string();
        task.pipeline_stage = "analyst".to_string();
        task.linear_pushed = true; // callback ran, but status didn't actually move
        db.insert_task(&task).unwrap();

        // Should find it — this is the case that was previously invisible
        assert!(
            db.has_any_nonfailed_task_for_issue_stage("RIG-209", "analyst")
                .unwrap()
        );

        // Different issue: not found
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-999", "analyst")
                .unwrap()
        );

        // Different stage: not found
        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-209", "engineer")
                .unwrap()
        );

        // Failed tasks don't block (allow retry via poll)
        let mut failed_task = make_test_task("20260313-002");
        failed_task.status = Status::Failed;
        failed_task.linear_issue_id = "RIG-210".to_string();
        failed_task.pipeline_stage = "analyst".to_string();
        db.insert_task(&failed_task).unwrap();

        assert!(
            !db.has_any_nonfailed_task_for_issue_stage("RIG-210", "analyst")
                .unwrap()
        );

        // Pending tasks block (already queued)
        let mut pending_task = make_test_task("20260313-003");
        pending_task.status = Status::Pending;
        pending_task.linear_issue_id = "RIG-211".to_string();
        pending_task.pipeline_stage = "engineer".to_string();
        db.insert_task(&pending_task).unwrap();

        assert!(
            db.has_any_nonfailed_task_for_issue_stage("RIG-211", "engineer")
                .unwrap()
        );
    }

    #[test]
    fn open_with_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("subdir/werma.db");
        let db = Db::open(&db_path).unwrap();
        let counts = db.task_counts().unwrap();
        assert_eq!(counts, (0, 0, 0, 0));
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
        assert_eq!(claimed.id, "20260312-001"); // high priority first
    }

    #[test]
    fn claim_next_pending_skips_unresolved_deps() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("20260312-002");
        task.depends_on = vec!["20260312-001".to_string()]; // dep doesn't exist
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
        // Second claim should find nothing (task is now Running)
        let second = db.claim_next_pending().unwrap();
        assert!(second.is_none());
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
        with_dep.depends_on = vec!["20260312-999".to_string()]; // nonexistent
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

    // ─── count_active_pipeline_tasks ────────────────────────────────────────

    #[test]
    fn count_active_pipeline_tasks_empty() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.count_active_pipeline_tasks().unwrap(), 0);
    }

    #[test]
    fn count_active_pipeline_tasks_counts_correctly() {
        let db = Db::open_in_memory().unwrap();

        // Pipeline task (pending)
        let mut t1 = make_test_task("20260312-001");
        t1.pipeline_stage = "engineer".to_string();
        db.insert_task(&t1).unwrap();

        // Pipeline task (running)
        let mut t2 = make_test_task("20260312-002");
        t2.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t2).unwrap();
        db.set_task_status("20260312-002", Status::Running).unwrap();

        // Non-pipeline task (should not count)
        db.insert_task(&make_test_task("20260312-003")).unwrap();

        // Completed pipeline task (should not count)
        let mut t4 = make_test_task("20260312-004");
        t4.pipeline_stage = "engineer".to_string();
        db.insert_task(&t4).unwrap();
        db.set_task_status("20260312-004", Status::Completed)
            .unwrap();

        assert_eq!(db.count_active_pipeline_tasks().unwrap(), 2);
    }

    // ─── tasks_by_linear_issue (all parameter combinations) ─────────────────

    #[test]
    fn tasks_by_linear_issue_all_combos() {
        let db = Db::open_in_memory().unwrap();

        // Active engineer task
        let mut t1 = make_test_task("20260312-001");
        t1.linear_issue_id = "issue-1".to_string();
        t1.pipeline_stage = "engineer".to_string();
        db.insert_task(&t1).unwrap();

        // Completed engineer task
        let mut t2 = make_test_task("20260312-002");
        t2.linear_issue_id = "issue-1".to_string();
        t2.pipeline_stage = "engineer".to_string();
        db.insert_task(&t2).unwrap();
        db.set_task_status("20260312-002", Status::Completed)
            .unwrap();

        // Active reviewer task
        let mut t3 = make_test_task("20260312-003");
        t3.linear_issue_id = "issue-1".to_string();
        t3.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t3).unwrap();

        // (stage=None, active_only=false) → all 3
        let all = db.tasks_by_linear_issue("issue-1", None, false).unwrap();
        assert_eq!(all.len(), 3);

        // (stage=None, active_only=true) → 2 active
        let active = db.tasks_by_linear_issue("issue-1", None, true).unwrap();
        assert_eq!(active.len(), 2);

        // (stage=Some("engineer"), active_only=false) → 2 engineer tasks
        let eng = db
            .tasks_by_linear_issue("issue-1", Some("engineer"), false)
            .unwrap();
        assert_eq!(eng.len(), 2);

        // (stage=Some("engineer"), active_only=true) → 1 active engineer
        let eng_active = db
            .tasks_by_linear_issue("issue-1", Some("engineer"), true)
            .unwrap();
        assert_eq!(eng_active.len(), 1);
        assert_eq!(eng_active[0].id, "20260312-001");

        // (stage=Some("reviewer"), active_only=false) → 1
        let rev = db
            .tasks_by_linear_issue("issue-1", Some("reviewer"), false)
            .unwrap();
        assert_eq!(rev.len(), 1);

        // Nonexistent issue → 0
        let none = db.tasks_by_linear_issue("issue-999", None, false).unwrap();
        assert!(none.is_empty());
    }

    // ─── count_completed_tasks_for_issue_stage ──────────────────────────────

    #[test]
    fn count_completed_for_issue_stage() {
        let db = Db::open_in_memory().unwrap();

        // Completed reviewer task
        let mut t1 = make_test_task("20260312-001");
        t1.linear_issue_id = "issue-1".to_string();
        t1.pipeline_stage = "reviewer".to_string();
        db.insert_task(&t1).unwrap();
        db.set_task_status("20260312-001", Status::Completed)
            .unwrap();

        // Pending reviewer task (should not count)
        let mut t2 = make_test_task("20260312-002");
        t2.linear_issue_id = "issue-1".to_string();
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

    // ─── increment_usage edge cases ─────────────────────────────────────────

    #[test]
    fn increment_usage_haiku() {
        let db = Db::open_in_memory().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        db.increment_usage("haiku").unwrap();
        db.increment_usage("haiku").unwrap();
        db.increment_usage("haiku").unwrap();

        let usage = db.daily_usage(&today).unwrap();
        assert_eq!(usage.haiku_calls, 3);
        assert_eq!(usage.opus_calls, 0);
        assert_eq!(usage.sonnet_calls, 0);
    }

    #[test]
    fn increment_usage_unknown_model_errors() {
        let db = Db::open_in_memory().unwrap();
        let result = db.increment_usage("gpt-4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown model"));
    }

    #[test]
    fn increment_usage_all_models() {
        let db = Db::open_in_memory().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        db.increment_usage("opus").unwrap();
        db.increment_usage("sonnet").unwrap();
        db.increment_usage("haiku").unwrap();

        let usage = db.daily_usage(&today).unwrap();
        assert_eq!(usage.opus_calls, 1);
        assert_eq!(usage.sonnet_calls, 1);
        assert_eq!(usage.haiku_calls, 1);
    }

    // ─── migration idempotency ──────────────────────────────────────────────

    #[test]
    fn migration_idempotent() {
        let db = Db::open_in_memory().unwrap();
        // Running migrate again should not fail
        db.migrate().unwrap();
        // And data should still work
        let counts = db.task_counts().unwrap();
        assert_eq!(counts, (0, 0, 0, 0));
    }

    // ─── schedule: context_files roundtrip ────────────────────────────────

    #[test]
    fn schedule_context_files_roundtrip() {
        let db = Db::open_in_memory().unwrap();

        let sched = Schedule {
            id: "ctx-test".to_string(),
            cron_expr: "0 9 * * *".to_string(),
            prompt: "review".to_string(),
            schedule_type: "review".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec!["file1.md".to_string(), "file2.md".to_string()],
            last_enqueued: String::new(),
        };
        db.insert_schedule(&sched).unwrap();

        let fetched = db.schedule("ctx-test").unwrap().unwrap();
        assert_eq!(fetched.context_files, vec!["file1.md", "file2.md"]);
    }

    #[test]
    fn schedule_empty_context_files() {
        let db = Db::open_in_memory().unwrap();

        let sched = Schedule {
            id: "no-ctx".to_string(),
            cron_expr: "0 9 * * *".to_string(),
            prompt: "review".to_string(),
            schedule_type: "review".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        db.insert_schedule(&sched).unwrap();

        let fetched = db.schedule("no-ctx").unwrap().unwrap();
        assert!(fetched.context_files.is_empty());
    }

    // ─── schedule: not found ──────────────────────────────────────────────

    #[test]
    fn schedule_not_found() {
        let db = Db::open_in_memory().unwrap();
        let result = db.schedule("nonexistent").unwrap();
        assert!(result.is_none());
    }

    // ─── clean_completed: no completed tasks ──────────────────────────────

    #[test]
    fn clean_completed_empty() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260313-001");
        db.insert_task(&task).unwrap();

        let cleaned = db.clean_completed().unwrap();
        assert!(cleaned.is_empty());

        // Original pending task should still exist
        let all = db.list_tasks(None).unwrap();
        assert_eq!(all.len(), 1);
    }

    // ─── task_counts: all statuses ────────────────────────────────────────

    #[test]
    fn task_counts_all_statuses() {
        let db = Db::open_in_memory().unwrap();

        // Pending
        db.insert_task(&make_test_task("20260313-001")).unwrap();
        db.insert_task(&make_test_task("20260313-002")).unwrap();

        // Running
        db.insert_task(&make_test_task("20260313-003")).unwrap();
        db.set_task_status("20260313-003", Status::Running).unwrap();

        // Completed
        db.insert_task(&make_test_task("20260313-004")).unwrap();
        db.set_task_status("20260313-004", Status::Completed)
            .unwrap();

        // Failed
        db.insert_task(&make_test_task("20260313-005")).unwrap();
        db.set_task_status("20260313-005", Status::Failed).unwrap();

        let (p, r, c, f) = db.task_counts().unwrap();
        assert_eq!(p, 2);
        assert_eq!(r, 1);
        assert_eq!(c, 1);
        assert_eq!(f, 1);
    }

    // ─── pr_reviewed: idempotent ──────────────────────────────────────────

    #[test]
    fn pr_reviewed_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.mark_pr_reviewed("repo/1").unwrap();
        db.mark_pr_reviewed("repo/1").unwrap(); // INSERT OR REPLACE
        assert!(db.is_pr_reviewed("repo/1").unwrap());
    }

    // ─── next_task_id: sequential within day ──────────────────────────────

    #[test]
    fn next_task_id_sequential() {
        let db = Db::open_in_memory().unwrap();

        let id1 = db.next_task_id().unwrap();
        db.insert_task(&make_test_task(&id1)).unwrap();

        let id2 = db.next_task_id().unwrap();
        db.insert_task(&make_test_task(&id2)).unwrap();

        let id3 = db.next_task_id().unwrap();

        // IDs should be sequential
        assert!(id1.ends_with("-001"));
        assert!(id2.ends_with("-002"));
        assert!(id3.ends_with("-003"));

        // All should share the same date prefix
        let prefix1 = id1.split('-').next().unwrap();
        let prefix2 = id2.split('-').next().unwrap();
        assert_eq!(prefix1, prefix2);
    }
}
