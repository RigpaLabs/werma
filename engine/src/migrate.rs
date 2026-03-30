use anyhow::{Context, Result};

use crate::db::Db;
use crate::models::{Schedule, Status, Task};

/// Migrate data from ~/.agent-queue/ JSON files into SQLite.
pub fn migrate(db: &Db) -> Result<()> {
    let aq_dir = dirs::home_dir()
        .context("no home dir")?
        .join(".agent-queue");

    if !aq_dir.exists() {
        println!("No ~/.agent-queue/ found — nothing to migrate");
        return Ok(());
    }

    // Migrate tasks from queue.json
    let queue_file = aq_dir.join("queue.json");
    if queue_file.exists() {
        let data = std::fs::read_to_string(&queue_file)?;
        let json: serde_json::Value = serde_json::from_str(&data)?;

        if let Some(tasks) = json["tasks"].as_array() {
            let mut imported = 0;
            let mut skipped = 0;

            for task_val in tasks {
                if task_val.is_null() {
                    skipped += 1;
                    continue;
                }

                let task = parse_task(task_val);

                if db.task(&task.id)?.is_some() {
                    skipped += 1;
                    continue;
                }

                db.insert_task(&task)?;
                imported += 1;
            }

            println!("Tasks: {imported} imported, {skipped} skipped (null or duplicate)");
        }

        std::fs::rename(&queue_file, aq_dir.join("queue.json.bak"))?;
        println!("Renamed queue.json -> queue.json.bak");
    }

    // Migrate schedules from schedules.json
    let sched_file = aq_dir.join("schedules.json");
    if sched_file.exists() {
        let data = std::fs::read_to_string(&sched_file)?;
        let json: serde_json::Value = serde_json::from_str(&data)?;

        if let Some(schedules) = json["schedules"].as_array() {
            let mut imported = 0;
            let mut skipped = 0;

            for sched_val in schedules {
                if sched_val.is_null() {
                    skipped += 1;
                    continue;
                }

                let sched = parse_schedule(sched_val);

                if db.schedule(&sched.id)?.is_some() {
                    skipped += 1;
                    continue;
                }

                db.insert_schedule(&sched)?;
                imported += 1;
            }

            println!("Schedules: {imported} imported, {skipped} skipped");
        }

        std::fs::rename(&sched_file, aq_dir.join("schedules.json.bak"))?;
        println!("Renamed schedules.json -> schedules.json.bak");
    }

    // Migrate pr-reviewed.json
    let pr_file = aq_dir.join("pr-reviewed.json");
    if pr_file.exists() {
        let data = std::fs::read_to_string(&pr_file)?;
        let json: serde_json::Value = serde_json::from_str(&data)?;

        if let Some(reviewed) = json["reviewed"].as_object() {
            let mut imported = 0;
            for (pr_key, _) in reviewed {
                if !db.is_pr_reviewed(pr_key)? {
                    db.mark_pr_reviewed(pr_key)?;
                    imported += 1;
                }
            }
            println!("PR reviewed: {imported} imported");
        }

        std::fs::rename(&pr_file, aq_dir.join("pr-reviewed.json.bak"))?;
        println!("Renamed pr-reviewed.json -> pr-reviewed.json.bak");
    }

    // Verify
    let (p, r, c, f) = db.task_counts()?;
    println!("\nVerification — tasks in db: {p} pending, {r} running, {c} completed, {f} failed");

    Ok(())
}

fn parse_status(s: &str) -> Status {
    match s {
        "running" => Status::Running,
        "completed" => Status::Completed,
        "failed" => Status::Failed,
        _ => Status::Pending,
    }
}

fn parse_string_array(val: &serde_json::Value) -> Vec<String> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_task(v: &serde_json::Value) -> Task {
    Task {
        id: v["id"].as_str().unwrap_or("unknown").to_string(),
        status: parse_status(v["status"].as_str().unwrap_or("pending")),
        priority: v["priority"].as_i64().unwrap_or(2) as i32,
        created_at: v["created"]
            .as_str()
            .or_else(|| v["created_at"].as_str())
            .unwrap_or("")
            .to_string(),
        started_at: v["started"]
            .as_str()
            .or_else(|| v["started_at"].as_str())
            .map(String::from),
        finished_at: v["finished"]
            .as_str()
            .or_else(|| v["finished_at"].as_str())
            .map(String::from),
        task_type: v["type"].as_str().unwrap_or("custom").to_string(),
        prompt: v["prompt"].as_str().unwrap_or("").to_string(),
        output_path: v["output"]
            .as_str()
            .or_else(|| v["output_path"].as_str())
            .unwrap_or("")
            .to_string(),
        working_dir: v["working_dir"]
            .as_str()
            .unwrap_or("~/projects/ar")
            .to_string(),
        model: v["model"].as_str().unwrap_or("sonnet").to_string(),
        max_turns: v["max_turns"].as_i64().unwrap_or(15) as i32,
        allowed_tools: v["allowed_tools"].as_str().unwrap_or("").to_string(),
        session_id: v["session_id"].as_str().unwrap_or("").to_string(),
        linear_issue_id: v["linear_issue_id"].as_str().unwrap_or("").to_string(),
        linear_pushed: v["linear_pushed"].as_bool().unwrap_or(false),
        pipeline_stage: v["pipeline_stage"].as_str().unwrap_or("").to_string(),
        depends_on: parse_string_array(&v["depends_on"]),
        context_files: parse_string_array(&v["context_files"]),
        repo_hash: v["repo_hash"].as_str().unwrap_or("").to_string(),
        estimate: v["estimate"].as_i64().unwrap_or(0) as i32,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content: String::new(),
        runtime: crate::models::AgentRuntime::default(),
    }
}

fn parse_schedule(v: &serde_json::Value) -> Schedule {
    Schedule {
        id: v["id"].as_str().unwrap_or("unknown").to_string(),
        cron_expr: v["cron"]
            .as_str()
            .or_else(|| v["cron_expr"].as_str())
            .unwrap_or("")
            .to_string(),
        prompt: v["prompt"].as_str().unwrap_or("").to_string(),
        schedule_type: v["type"]
            .as_str()
            .or_else(|| v["schedule_type"].as_str())
            .unwrap_or("research")
            .to_string(),
        model: v["model"].as_str().unwrap_or("opus").to_string(),
        output_path: v["output"]
            .as_str()
            .or_else(|| v["output_path"].as_str())
            .unwrap_or("")
            .to_string(),
        working_dir: v["working_dir"]
            .as_str()
            .unwrap_or("~/projects/ar")
            .to_string(),
        max_turns: v["max_turns"].as_i64().unwrap_or(0) as i32,
        enabled: v["enabled"].as_bool().unwrap_or(true),
        context_files: parse_string_array(&v["context_files"]),
        last_enqueued: v["last_enqueued"].as_str().unwrap_or("").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_from_json() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
                "id": "20260308-001",
                "status": "completed",
                "priority": 1,
                "created": "2026-03-08T10:00:00",
                "started": "2026-03-08T10:01:00",
                "finished": "2026-03-08T10:05:00",
                "type": "research",
                "prompt": "Do research",
                "output": "~/output.md",
                "working_dir": "~/projects/test",
                "model": "opus",
                "max_turns": 20,
                "session_id": "abc-123",
                "depends_on": ["20260308-000"],
                "context_files": ["file1.md", "file2.md"]
            }"#,
        )
        .unwrap();

        let task = parse_task(&json);
        assert_eq!(task.id, "20260308-001");
        assert_eq!(task.status, Status::Completed);
        assert_eq!(task.priority, 1);
        assert_eq!(task.created_at, "2026-03-08T10:00:00");
        assert_eq!(task.started_at.as_deref(), Some("2026-03-08T10:01:00"));
        assert_eq!(task.finished_at.as_deref(), Some("2026-03-08T10:05:00"));
        assert_eq!(task.task_type, "research");
        assert_eq!(task.prompt, "Do research");
        assert_eq!(task.output_path, "~/output.md");
        assert_eq!(task.model, "opus");
        assert_eq!(task.max_turns, 20);
        assert_eq!(task.session_id, "abc-123");
        assert_eq!(task.depends_on, vec!["20260308-000"]);
        assert_eq!(task.context_files, vec!["file1.md", "file2.md"]);
    }

    #[test]
    fn parse_task_with_nulls_and_missing_fields() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
                "id": "test-001",
                "prompt": "minimal task"
            }"#,
        )
        .unwrap();

        let task = parse_task(&json);
        assert_eq!(task.id, "test-001");
        assert_eq!(task.status, Status::Pending);
        assert_eq!(task.priority, 2);
        assert_eq!(task.task_type, "custom");
        assert_eq!(task.model, "sonnet");
        assert_eq!(task.max_turns, 15);
        assert!(task.depends_on.is_empty());
        assert!(task.context_files.is_empty());
    }

    #[test]
    fn parse_queue_with_null_entries() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
                "tasks": [
                    null,
                    {"id": "t1", "prompt": "task one"},
                    null,
                    null,
                    {"id": "t2", "prompt": "task two", "status": "completed"}
                ]
            }"#,
        )
        .unwrap();

        let tasks = json["tasks"].as_array().unwrap();
        let mut parsed = Vec::new();
        let mut null_count = 0;

        for val in tasks {
            if val.is_null() {
                null_count += 1;
                continue;
            }
            parsed.push(parse_task(val));
        }

        assert_eq!(null_count, 3);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "t1");
        assert_eq!(parsed[0].status, Status::Pending);
        assert_eq!(parsed[1].id, "t2");
        assert_eq!(parsed[1].status, Status::Completed);
    }

    #[test]
    fn parse_schedule_from_json() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
                "id": "daily-review",
                "cron": "30 7 * * *",
                "prompt": "Review PRs for {date}",
                "type": "review",
                "model": "opus",
                "output": "~/reports/{date}.md",
                "working_dir": "~/projects/main",
                "max_turns": 10,
                "enabled": true,
                "context_files": ["context.md"]
            }"#,
        )
        .unwrap();

        let sched = parse_schedule(&json);
        assert_eq!(sched.id, "daily-review");
        assert_eq!(sched.cron_expr, "30 7 * * *");
        assert_eq!(sched.schedule_type, "review");
        assert!(sched.enabled);
        assert_eq!(sched.context_files, vec!["context.md"]);
    }

    #[test]
    fn parse_status_values() {
        assert_eq!(parse_status("pending"), Status::Pending);
        assert_eq!(parse_status("running"), Status::Running);
        assert_eq!(parse_status("completed"), Status::Completed);
        assert_eq!(parse_status("failed"), Status::Failed);
        assert_eq!(parse_status("bogus"), Status::Pending);
    }

    #[test]
    fn parse_string_array_empty() {
        let val = serde_json::json!(null);
        assert!(parse_string_array(&val).is_empty());

        let val = serde_json::json!([]);
        assert!(parse_string_array(&val).is_empty());
    }

    #[test]
    fn parse_string_array_values() {
        let val = serde_json::json!(["a", "b", "c"]);
        assert_eq!(parse_string_array(&val), vec!["a", "b", "c"]);
    }

    #[test]
    fn migrate_with_real_db() {
        let dir = tempfile::tempdir().unwrap();
        let aq_dir = dir.path().join(".agent-queue");
        std::fs::create_dir_all(&aq_dir).unwrap();

        // Write a sample queue.json
        std::fs::write(
            aq_dir.join("queue.json"),
            r#"{"tasks": [null, {"id": "t1", "prompt": "hello", "working_dir": "/tmp"}]}"#,
        )
        .unwrap();

        // We can't easily test the full migrate() because it hardcodes ~/.agent-queue/,
        // but we can test parse_task + db.insert_task roundtrip
        let db = crate::db::Db::open_in_memory().unwrap();

        let json: serde_json::Value =
            serde_json::from_str(r#"{"id": "t1", "prompt": "hello", "working_dir": "/tmp"}"#)
                .unwrap();
        let task = parse_task(&json);
        db.insert_task(&task).unwrap();

        let fetched = db.task("t1").unwrap().unwrap();
        assert_eq!(fetched.prompt, "hello");
        assert_eq!(fetched.status, Status::Pending);
    }
}
