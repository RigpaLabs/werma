mod pipeline;
mod schedule;
mod task;
mod usage;

#[allow(unused_imports)]
pub use schedule::ScheduleRepository;
#[allow(unused_imports)]
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

pub struct Db {
    pub(super) conn: Connection,
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
        // 006: add 'canceled' to status CHECK constraint (idempotent — recreates table)
        // Only needed for existing DBs; fresh DBs already have 'canceled' in 001_init.sql.
        // Check if migration is needed by attempting to set a dummy value.
        let needs_006: bool = {
            let check = self
                .conn
                .execute("UPDATE tasks SET status = 'canceled' WHERE 0", []);
            check.is_err()
        };
        if needs_006 {
            self.conn
                .execute_batch(MIGRATION_006_SQL)
                .context("migration 006_add_canceled_status")?;
        }
        Ok(())
    }
}

/// Parse a task row from a SELECT query with the standard 21-column layout.
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

    use std::cell::RefCell;
    use std::collections::HashMap;

    /// In-memory fake implementing TaskRepository for testing consumers
    /// without touching SQLite.
    struct FakeTaskRepo {
        tasks: RefCell<HashMap<String, crate::models::Task>>,
    }

    impl FakeTaskRepo {
        fn new() -> Self {
            Self {
                tasks: RefCell::new(HashMap::new()),
            }
        }
    }

    impl TaskRepository for FakeTaskRepo {
        fn insert_task(&self, task: &crate::models::Task) -> anyhow::Result<()> {
            self.tasks
                .borrow_mut()
                .insert(task.id.clone(), task.clone());
            Ok(())
        }

        fn task(&self, id: &str) -> anyhow::Result<Option<crate::models::Task>> {
            Ok(self.tasks.borrow().get(id).cloned())
        }

        fn list_tasks(
            &self,
            status: Option<crate::models::Status>,
        ) -> anyhow::Result<Vec<crate::models::Task>> {
            let tasks = self.tasks.borrow();
            let iter = tasks.values();
            match status {
                Some(s) => Ok(iter.filter(|t| t.status == s).cloned().collect()),
                None => Ok(iter.cloned().collect()),
            }
        }

        fn set_task_status(&self, id: &str, status: crate::models::Status) -> anyhow::Result<()> {
            if let Some(task) = self.tasks.borrow_mut().get_mut(id) {
                task.status = status;
            }
            Ok(())
        }

        fn find_next_pending(&self) -> anyhow::Result<Option<crate::models::Task>> {
            let tasks = self.tasks.borrow();
            Ok(tasks
                .values()
                .filter(|t| t.status == crate::models::Status::Pending)
                .min_by_key(|t| t.priority)
                .cloned())
        }

        fn update_task_field(&self, id: &str, field: &str, value: &str) -> anyhow::Result<()> {
            if let Some(task) = self.tasks.borrow_mut().get_mut(id) {
                match field {
                    "session_id" => task.session_id = value.to_string(),
                    _ => {}
                }
            }
            Ok(())
        }
    }

    /// In-memory fake implementing ScheduleRepository.
    struct FakeScheduleRepo {
        schedules: RefCell<HashMap<String, crate::models::Schedule>>,
    }

    impl FakeScheduleRepo {
        fn new() -> Self {
            Self {
                schedules: RefCell::new(HashMap::new()),
            }
        }
    }

    impl ScheduleRepository for FakeScheduleRepo {
        fn insert_schedule(&self, sched: &crate::models::Schedule) -> anyhow::Result<()> {
            self.schedules
                .borrow_mut()
                .insert(sched.id.clone(), sched.clone());
            Ok(())
        }

        fn list_schedules(&self) -> anyhow::Result<Vec<crate::models::Schedule>> {
            Ok(self.schedules.borrow().values().cloned().collect())
        }

        fn schedule(&self, id: &str) -> anyhow::Result<Option<crate::models::Schedule>> {
            Ok(self.schedules.borrow().get(id).cloned())
        }

        fn delete_schedule(&self, id: &str) -> anyhow::Result<()> {
            self.schedules.borrow_mut().remove(id);
            Ok(())
        }

        fn set_schedule_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<()> {
            if let Some(sched) = self.schedules.borrow_mut().get_mut(id) {
                sched.enabled = enabled;
            }
            Ok(())
        }

        fn set_schedule_last_enqueued(&self, id: &str, timestamp: &str) -> anyhow::Result<()> {
            if let Some(sched) = self.schedules.borrow_mut().get_mut(id) {
                sched.last_enqueued = timestamp.to_string();
            }
            Ok(())
        }
    }

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
}
