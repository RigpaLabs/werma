use anyhow::Result;
use rusqlite::params;

use crate::models::Schedule;

/// Trait for schedule persistence operations, enabling testability via fakes/mocks.
pub trait ScheduleRepository {
    fn insert_schedule(&self, sched: &Schedule) -> Result<()>;
    fn list_schedules(&self) -> Result<Vec<Schedule>>;
    fn schedule(&self, id: &str) -> Result<Option<Schedule>>;
    fn delete_schedule(&self, id: &str) -> Result<()>;
    fn set_schedule_enabled(&self, id: &str, enabled: bool) -> Result<()>;
    fn set_schedule_last_enqueued(&self, id: &str, timestamp: &str) -> Result<()>;
}

impl ScheduleRepository for super::Db {
    fn insert_schedule(&self, sched: &Schedule) -> Result<()> {
        self.insert_schedule(sched)
    }

    fn list_schedules(&self) -> Result<Vec<Schedule>> {
        self.list_schedules()
    }

    fn schedule(&self, id: &str) -> Result<Option<Schedule>> {
        self.schedule(id)
    }

    fn delete_schedule(&self, id: &str) -> Result<()> {
        self.delete_schedule(id)
    }

    fn set_schedule_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.set_schedule_enabled(id, enabled)
    }

    fn set_schedule_last_enqueued(&self, id: &str, timestamp: &str) -> Result<()> {
        self.set_schedule_last_enqueued(id, timestamp)
    }
}

impl super::Db {
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
}

#[cfg(test)]
mod tests {
    use super::super::Db;
    use crate::models::Schedule;

    fn make_test_schedule(id: &str) -> Schedule {
        Schedule {
            id: id.to_string(),
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
        }
    }

    #[test]
    fn schedule_crud() {
        let db = Db::open_in_memory().unwrap();
        let sched = make_test_schedule("daily-review");

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
    fn schedule_not_found() {
        let db = Db::open_in_memory().unwrap();
        let result = db.schedule("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn schedule_context_files_roundtrip() {
        let db = Db::open_in_memory().unwrap();

        let mut sched = make_test_schedule("ctx-test");
        sched.context_files = vec!["file1.md".to_string(), "file2.md".to_string()];
        db.insert_schedule(&sched).unwrap();

        let fetched = db.schedule("ctx-test").unwrap().unwrap();
        assert_eq!(fetched.context_files, vec!["file1.md", "file2.md"]);
    }

    #[test]
    fn schedule_empty_context_files() {
        let db = Db::open_in_memory().unwrap();

        let mut sched = make_test_schedule("no-ctx");
        sched.context_files = vec![];
        db.insert_schedule(&sched).unwrap();

        let fetched = db.schedule("no-ctx").unwrap().unwrap();
        assert!(fetched.context_files.is_empty());
    }

    #[test]
    fn list_schedules_ordered_by_id() {
        let db = Db::open_in_memory().unwrap();
        db.insert_schedule(&make_test_schedule("z-last")).unwrap();
        db.insert_schedule(&make_test_schedule("a-first")).unwrap();

        let all = db.list_schedules().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "a-first");
        assert_eq!(all[1].id, "z-last");
    }

    #[test]
    fn list_schedules_empty() {
        let db = Db::open_in_memory().unwrap();
        let all = db.list_schedules().unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn delete_nonexistent_schedule_is_ok() {
        let db = Db::open_in_memory().unwrap();
        db.delete_schedule("nonexistent").unwrap();
    }
}
