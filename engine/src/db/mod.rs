mod effects;
pub mod fakes;
mod pipeline;
mod schedule;
mod task;
mod usage;

pub use schedule::ScheduleRepository;
pub use task::TaskRepository;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

use crate::models::{Status, Task};

const MIGRATION_SQL: &str = include_str!("../../migrations/001_init.sql");
const MIGRATION_002_SQL: &str = include_str!("../../migrations/002_repo_hash.sql");
const MIGRATION_003_SQL: &str = include_str!("../../migrations/003_estimate.sql");
const MIGRATION_004_SQL: &str = include_str!("../../migrations/004_normalize_linear_ids.sql");
const MIGRATION_005_SQL: &str = include_str!("../../migrations/005_callback_fired_at.sql");
const MIGRATION_006_SQL: &str = include_str!("../../migrations/006_add_canceled_status.sql");
const MIGRATION_007_SQL: &str =
    include_str!("../../migrations/007_callback_attempts_and_indexes.sql");
const MIGRATION_008_SQL: &str = include_str!("../../migrations/008_retry.sql");
const MIGRATION_009_SQL: &str = include_str!("../../migrations/009_cost_tracking.sql");
const MIGRATION_010_SQL: &str = include_str!("../../migrations/010_effects_and_handoff.sql");
const MIGRATION_011_SQL: &str = include_str!("../../migrations/011_runtime.sql");

pub struct Db {
    pub(super) conn: Connection,
    /// Path to the database file (None for in-memory).
    db_path: Option<std::path::PathBuf>,
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

        // RIG-388: force WAL autocheckpoint after every transaction.
        // Without this, WAL writes from one Db::open() may not be visible
        // to the next Db::open() on the same file (e.g. daemon tick N inserts,
        // tick N+1 opens fresh connection and sees stale data → dedup fails).
        conn.execute_batch("PRAGMA wal_autocheckpoint = 1")?;

        let db = Self {
            conn,
            db_path: Some(path.to_path_buf()),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn,
            db_path: None,
        };
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
        // 006: add 'canceled' to status CHECK constraint (idempotent — recreates table).
        // Detect whether the current tasks table allows 'canceled' by inspecting the
        // schema stored in sqlite_master. The WHERE 0 trick does not work because SQLite
        // skips CHECK evaluation when no rows are modified, so execute() always succeeds
        // regardless of what the constraint says.
        let needs_006: bool = {
            let schema: String = self
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            !schema.contains("'canceled'")
        };
        if needs_006 {
            self.conn
                .execute_batch(MIGRATION_006_SQL)
                .context("migration 006_add_canceled_status")?;
        }
        // 007: add callback_attempts column + performance indexes.
        // ALTER TABLE is idempotent — ignore "duplicate column" error.
        // CREATE INDEX uses IF NOT EXISTS, so re-running is always safe.
        if let Err(e) = self.conn.execute_batch(MIGRATION_007_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e).context("migration 007_callback_attempts_and_indexes");
            }
        }
        // 008: add retry_count and retry_after columns for auto-retry.
        if let Err(e) = self.conn.execute_batch(MIGRATION_008_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e).context("migration 008_retry");
            }
        }
        // 009: add cost_usd and turns_used columns for monitoring (RIG-291).
        if let Err(e) = self.conn.execute_batch(MIGRATION_009_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e).context("migration 009_cost_tracking");
            }
        }
        // 010: effects outbox table + handoff_content column on tasks.
        if let Err(e) = self.conn.execute_batch(MIGRATION_010_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") && !msg.contains("already exists") {
                return Err(e).context("migration 010_effects_and_handoff");
            }
        }
        // 011: add runtime column for multi-runtime support (claude-code, codex).
        if let Err(e) = self.conn.execute_batch(MIGRATION_011_SQL) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e).context("migration 011_runtime");
            }
        }
        Ok(())
    }

    /// Run a closure inside a SQLite IMMEDIATE transaction.
    ///
    /// Uses BEGIN IMMEDIATE for explicit write-locking. Safe because werma
    /// daemon is single-threaded. If multi-threading is ever added, Db must
    /// switch to &mut self or Arc<Mutex<Connection>>.
    pub fn transaction<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&self.conn) {
            Ok(result) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(result)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

/// Parse a task row from a SELECT query with the standard column layout.
pub(super) fn task_from_row(row: &rusqlite::Row<'_>) -> Result<Task> {
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
        retry_count: row.get(21).unwrap_or(0),
        retry_after: row.get(22).ok(),
        cost_usd: row.get(23).ok().flatten(),
        turns_used: row.get(24).unwrap_or(0),
        handoff_content: row.get(25).unwrap_or_default(),
        runtime: row
            .get::<_, String>(26)
            .unwrap_or_else(|_| "claude-code".to_string())
            .parse()
            .unwrap_or_default(),
    })
}

/// Create a test task with sensible defaults.
#[cfg(test)]
pub(crate) fn make_test_task(id: &str) -> Task {
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
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content: String::new(),
        runtime: crate::models::AgentRuntime::default(),
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
    fn migration_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.migrate().unwrap();
        let counts = db.task_counts().unwrap();
        assert_eq!(counts, (0, 0, 0, 0));
    }

    #[test]
    fn open_with_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("subdir/werma.db");
        let db = Db::open(&db_path).unwrap();
        let counts = db.task_counts().unwrap();
        assert_eq!(counts, (0, 0, 0, 0));
    }

    #[test]
    fn migration_double_run_with_data() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260308-001");
        db.insert_task(&task).unwrap();
        db.migrate().unwrap();
        let fetched = db.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.id, "20260308-001");
    }

    // ─── Fake repository tests ──────────────────────────────────────────

    use super::fakes::{FakeScheduleRepo, FakeTaskRepo};

    #[test]
    fn fake_task_repo_insert_and_get() {
        let repo = FakeTaskRepo::new();
        let task = make_test_task("fake-001");
        repo.insert_task(&task).unwrap();

        let fetched = repo.task("fake-001").unwrap().unwrap();
        assert_eq!(fetched.id, "fake-001");
        assert_eq!(fetched.status, crate::models::Status::Pending);

        assert!(repo.task("nonexistent").unwrap().is_none());
    }

    #[test]
    fn fake_task_repo_list_and_filter() {
        let repo = FakeTaskRepo::new();

        let t1 = make_test_task("fake-001");
        let mut t2 = make_test_task("fake-002");
        t2.status = crate::models::Status::Completed;

        repo.insert_task(&t1).unwrap();
        repo.insert_task(&t2).unwrap();

        let all = repo.list_tasks(None).unwrap();
        assert_eq!(all.len(), 2);

        let pending = repo
            .list_tasks(Some(crate::models::Status::Pending))
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "fake-001");
    }

    #[test]
    fn fake_task_repo_find_next_pending() {
        let repo = FakeTaskRepo::new();

        let mut t1 = make_test_task("fake-001");
        t1.priority = 5;
        let mut t2 = make_test_task("fake-002");
        t2.priority = 1;

        repo.insert_task(&t1).unwrap();
        repo.insert_task(&t2).unwrap();

        let next = repo.find_next_pending().unwrap().unwrap();
        assert_eq!(next.id, "fake-002"); // lower priority number = higher priority
    }

    #[test]
    fn fake_task_repo_set_status() {
        let repo = FakeTaskRepo::new();
        let task = make_test_task("fake-001");
        repo.insert_task(&task).unwrap();

        repo.set_task_status("fake-001", crate::models::Status::Running)
            .unwrap();
        let fetched = repo.task("fake-001").unwrap().unwrap();
        assert_eq!(fetched.status, crate::models::Status::Running);
    }

    #[test]
    fn fake_schedule_repo_crud() {
        let repo = FakeScheduleRepo::new();

        let sched = crate::models::Schedule {
            id: "test-sched".to_string(),
            cron_expr: "0 9 * * *".to_string(),
            prompt: "do stuff".to_string(),
            schedule_type: "review".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        repo.insert_schedule(&sched).unwrap();

        let fetched = repo.schedule("test-sched").unwrap().unwrap();
        assert_eq!(fetched.cron_expr, "0 9 * * *");
        assert!(fetched.enabled);

        repo.set_schedule_enabled("test-sched", false).unwrap();
        let fetched = repo.schedule("test-sched").unwrap().unwrap();
        assert!(!fetched.enabled);

        repo.delete_schedule("test-sched").unwrap();
        assert!(repo.schedule("test-sched").unwrap().is_none());
    }

    #[test]
    fn db_implements_task_repository_trait() {
        let db = Db::open_in_memory().unwrap();
        let repo: &dyn TaskRepository = &db;

        let task = make_test_task("20260308-001");
        repo.insert_task(&task).unwrap();

        let fetched = repo.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.id, "20260308-001");

        repo.set_task_status("20260308-001", crate::models::Status::Running)
            .unwrap();
        let fetched = repo.task("20260308-001").unwrap().unwrap();
        assert_eq!(fetched.status, crate::models::Status::Running);
    }

    #[test]
    fn db_implements_schedule_repository_trait() {
        let db = Db::open_in_memory().unwrap();
        let repo: &dyn ScheduleRepository = &db;

        let sched = crate::models::Schedule {
            id: "trait-test".to_string(),
            cron_expr: "0 8 * * *".to_string(),
            prompt: "test".to_string(),
            schedule_type: "review".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 5,
            enabled: true,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        repo.insert_schedule(&sched).unwrap();

        let fetched = repo.schedule("trait-test").unwrap().unwrap();
        assert_eq!(fetched.cron_expr, "0 8 * * *");

        let all = repo.list_schedules().unwrap();
        assert_eq!(all.len(), 1);
    }

    /// Simulate an old DB whose tasks table lacks 'canceled' in its CHECK constraint.
    /// Migration 006 must be applied so that Status::Canceled rows can be inserted.
    #[test]
    fn migration_006_applied_on_old_db_without_canceled() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();

        // Create the tasks table without 'canceled' — this is the pre-006 schema.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE tasks (
                 id              TEXT PRIMARY KEY,
                 status          TEXT NOT NULL DEFAULT 'pending'
                                 CHECK(status IN ('pending','running','completed','failed')),
                 priority        INTEGER NOT NULL DEFAULT 2,
                 created_at      TEXT NOT NULL,
                 started_at      TEXT,
                 finished_at     TEXT,
                 type            TEXT NOT NULL DEFAULT 'custom',
                 prompt          TEXT NOT NULL,
                 output_path     TEXT DEFAULT '',
                 working_dir     TEXT NOT NULL,
                 model           TEXT NOT NULL DEFAULT 'sonnet',
                 max_turns       INTEGER NOT NULL DEFAULT 15,
                 allowed_tools   TEXT DEFAULT '',
                 session_id      TEXT DEFAULT '',
                 linear_issue_id TEXT DEFAULT '',
                 linear_pushed   INTEGER DEFAULT 0,
                 pipeline_stage  TEXT DEFAULT '',
                 depends_on      TEXT DEFAULT '[]',
                 context_files   TEXT DEFAULT '[]',
                 repo_hash       TEXT DEFAULT '',
                 estimate        INTEGER DEFAULT 0,
                 callback_fired_at TEXT
             );
             CREATE TABLE schedules (
                 id TEXT PRIMARY KEY,
                 cron_expr TEXT NOT NULL,
                 prompt TEXT NOT NULL,
                 type TEXT NOT NULL DEFAULT 'research',
                 model TEXT NOT NULL DEFAULT 'opus',
                 output_path TEXT DEFAULT '',
                 working_dir TEXT NOT NULL,
                 max_turns INTEGER DEFAULT 0,
                 enabled INTEGER DEFAULT 1,
                 context_files TEXT DEFAULT '[]',
                 last_enqueued TEXT DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS pr_reviewed (pr_key TEXT PRIMARY KEY, updated_at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS daily_usage (date TEXT PRIMARY KEY, opus_calls INTEGER DEFAULT 0, sonnet_calls INTEGER DEFAULT 0, haiku_calls INTEGER DEFAULT 0);",
        )
        .unwrap();

        // Verify the old schema actually rejects 'canceled' (confirms test setup is correct).
        let pre_check = conn.execute(
            "INSERT INTO tasks (id, status, created_at, prompt, working_dir) VALUES ('pre-check', 'canceled', '2026-01-01', 'p', '/tmp')",
            [],
        );
        assert!(
            pre_check.is_err(),
            "old schema should reject 'canceled' status"
        );

        // Now wrap in Db and run migrate — migration 006 must detect and fix it.
        let db = Db {
            conn,
            db_path: None,
        };
        db.migrate().unwrap();

        // After migration, inserting a 'canceled' task must succeed.
        let mut task = make_test_task("mig006-001");
        task.status = crate::models::Status::Canceled;
        db.insert_task(&task).unwrap();

        let fetched = db.task("mig006-001").unwrap().unwrap();
        assert_eq!(fetched.status, crate::models::Status::Canceled);
    }

    /// Fresh DB (open_in_memory) must also support Status::Canceled after migrate.
    /// This covers the path where 001_init.sql does NOT include 'canceled' and
    /// migration 006 must be applied unconditionally for fresh DBs too.
    #[test]
    fn fresh_db_supports_canceled_status() {
        let db = Db::open_in_memory().unwrap();

        let mut task = make_test_task("fresh-canceled-001");
        task.status = crate::models::Status::Canceled;
        db.insert_task(&task).unwrap();

        let fetched = db.task("fresh-canceled-001").unwrap().unwrap();
        assert_eq!(fetched.status, crate::models::Status::Canceled);
    }

    /// 001_init.sql must be in its original form (without 'canceled' in CHECK).
    /// The 'canceled' status is handled exclusively by migration 006.
    #[test]
    fn init_sql_does_not_contain_canceled_in_check() {
        assert!(
            !MIGRATION_SQL.contains("'canceled'"),
            "001_init.sql must not include 'canceled' in the CHECK constraint — \
             that belongs to migration 006 only"
        );
    }

    // ─── Migration 010 tests ─────────────────────────────────────────────

    #[test]
    fn migration_010_creates_effects_table() {
        let db = Db::open_in_memory().unwrap();
        let count: i32 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='effects'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn migration_010_adds_handoff_content_column() {
        let db = Db::open_in_memory().unwrap();
        // Should not panic — column exists
        db.conn
            .execute(
                "UPDATE tasks SET handoff_content = 'test' WHERE id = 'nonexistent'",
                [],
            )
            .unwrap();
    }

    #[test]
    fn migration_010_dedup_key_is_unique() {
        use rusqlite::params;
        let db = Db::open_in_memory().unwrap();
        // Insert a real task so the FK constraint is satisfied.
        let task = make_test_task("dedup-task-1");
        db.insert_task(&task).unwrap();
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        db.conn
            .execute(
                "INSERT INTO effects (dedup_key, task_id, effect_type, created_at) VALUES ('key1', 'dedup-task-1', 'MoveIssue', ?1)",
                params![now],
            )
            .unwrap();
        // Duplicate dedup_key should fail or be ignored
        let result = db.conn.execute(
            "INSERT OR IGNORE INTO effects (dedup_key, task_id, effect_type, created_at) VALUES ('key1', 'dedup-task-1', 'MoveIssue', ?1)",
            params![now],
        );
        assert!(result.is_ok()); // INSERT OR IGNORE succeeds but inserts 0 rows
        let count: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM effects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // ─── Transaction tests ───────────────────────────────────────────────

    #[test]
    fn transaction_commits_on_success() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260326-tx1");
        db.insert_task(&task).unwrap();

        db.transaction(|conn| {
            conn.execute(
                "UPDATE tasks SET priority = 4 WHERE id = '20260326-tx1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        let updated = db.task("20260326-tx1").unwrap().unwrap();
        assert_eq!(updated.priority, 4);
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let db = Db::open_in_memory().unwrap();
        let task = make_test_task("20260326-tx2");
        db.insert_task(&task).unwrap();

        let result = db.transaction(|conn| -> Result<()> {
            conn.execute(
                "UPDATE tasks SET priority = 4 WHERE id = '20260326-tx2'",
                [],
            )?;
            anyhow::bail!("simulated error");
        });
        assert!(result.is_err());

        let unchanged = db.task("20260326-tx2").unwrap().unwrap();
        assert_eq!(unchanged.priority, 2); // original value from make_test_task
    }
}
