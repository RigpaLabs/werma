use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use chrono::Local;
use cron::Schedule;

use crate::db::{ScheduleRepository, TaskRepository};

use super::log_daemon;

/// Convert a 5-field user cron expression to the 7-field format the `cron` crate expects.
/// "30 7 * * *" -> "0 30 7 * * * *" (sec=0, year=*)
pub fn cron5_to_cron7(expr: &str) -> String {
    format!("0 {expr} *")
}

/// Check all enabled schedules and enqueue matching tasks.
pub fn check_schedules(
    task_repo: &dyn TaskRepository,
    sched_repo: &dyn ScheduleRepository,
    werma_dir: &Path,
) -> Result<()> {
    let log_path = werma_dir.join("logs/daemon.log");
    let schedules = sched_repo.list_schedules()?;
    let now = Local::now();

    for sched in &schedules {
        if !sched.enabled {
            continue;
        }

        let cron7 = cron5_to_cron7(&sched.cron_expr);
        let schedule = match Schedule::from_str(&cron7) {
            Ok(s) => s,
            Err(e) => {
                log_daemon(
                    &log_path,
                    &format!("bad cron expr for {}: {} -> {e}", sched.id, sched.cron_expr),
                );
                continue;
            }
        };

        // Check if cron schedule has an occurrence in the last 60 seconds.
        let window_start = now - chrono::Duration::seconds(60);
        let mut iter = schedule.after(&window_start);

        let matches = iter.next().is_some_and(|next_time| next_time <= now);

        if !matches {
            continue;
        }

        // Guard: don't enqueue if last_enqueued is within the last 60 seconds.
        if !sched.last_enqueued.is_empty()
            && let Ok(last) =
                chrono::NaiveDateTime::parse_from_str(&sched.last_enqueued, "%Y-%m-%dT%H:%M")
            && let Some(last_dt) = last.and_local_timezone(Local).single()
        {
            let since = now.signed_duration_since(last_dt);
            if since.num_seconds() < 60 {
                continue;
            }
        }

        // Enqueue: expand placeholders and create a task.
        let today = now.format("%Y-%m-%d").to_string();
        let dow = now.format("%A").to_string().to_lowercase();

        let prompt = sched
            .prompt
            .replace("{date}", &today)
            .replace("{dow}", &dow);

        let output_path = if sched.output_path.is_empty() {
            String::new()
        } else {
            sched.output_path.replace("{date}", &today)
        };

        let max_turns = if sched.max_turns > 0 {
            sched.max_turns
        } else {
            crate::default_turns(&sched.schedule_type)
        };

        let allowed_tools = crate::runner::tools_for_type(&sched.schedule_type, false);

        let task_id = task_repo.next_task_id()?;
        let created_at = now.format("%Y-%m-%dT%H:%M:%S").to_string();

        let task = crate::models::Task {
            id: task_id.clone(),
            status: crate::models::Status::Pending,
            priority: 2,
            created_at,
            started_at: None,
            finished_at: None,
            task_type: sched.schedule_type.clone(),
            prompt,
            output_path,
            working_dir: sched.working_dir.clone(),
            model: sched.model.clone(),
            max_turns,
            allowed_tools,
            session_id: String::new(),
            linear_issue_id: String::new(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![],
            context_files: sched.context_files.clone(),
            repo_hash: crate::runtime_repo_hash(),
            estimate: 0,
            retry_count: 0,
            retry_after: None,
            cost_usd: None,
            turns_used: 0,
            handoff_content: String::new(),
            runtime: crate::models::AgentRuntime::default(),
        };

        task_repo.insert_task(&task)?;

        let enqueued_at = now.format("%Y-%m-%dT%H:%M").to_string();
        sched_repo.set_schedule_last_enqueued(&sched.id, &enqueued_at)?;

        log_daemon(
            &log_path,
            &format!("schedule {}: enqueued task {task_id}", sched.id),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── cron5_to_cron7 ─────────────────────────────────────────────────

    #[test]
    fn cron5_to_cron7_conversion() {
        assert_eq!(cron5_to_cron7("30 7 * * *"), "0 30 7 * * * *");
        assert_eq!(cron5_to_cron7("0 */2 * * *"), "0 0 */2 * * * *");
        assert_eq!(cron5_to_cron7("15 9 1 * *"), "0 15 9 1 * * *");
        assert_eq!(cron5_to_cron7("0 0 * * 1-5"), "0 0 0 * * 1-5 *");
    }

    #[test]
    fn cron7_parses_correctly() {
        let expr = cron5_to_cron7("30 7 * * *");
        let schedule = Schedule::from_str(&expr);
        assert!(schedule.is_ok(), "failed to parse: {expr}");
    }

    #[test]
    fn cron7_various_expressions_parse() {
        let exprs = vec![
            "0 * * * *",    // every hour
            "*/15 * * * *", // every 15 min
            "30 7 * * 1-5", // weekdays at 7:30
            "0 0 1 * *",    // first of month midnight
            "0 9,18 * * *", // 9am and 6pm
        ];

        for expr in &exprs {
            let cron7 = cron5_to_cron7(expr);
            let result = Schedule::from_str(&cron7);
            assert!(
                result.is_ok(),
                "failed to parse '{expr}' -> '{cron7}': {:?}",
                result.err()
            );
        }
    }

    #[test]
    fn cron_schedule_matches_within_window() {
        use chrono::TimeZone;

        let expr = cron5_to_cron7("30 7 * * *");
        let schedule = Schedule::from_str(&expr).expect("parse");

        let now = Local.with_ymd_and_hms(2026, 3, 9, 7, 30, 30).unwrap();
        let window_start = now - chrono::Duration::seconds(60);

        let mut iter = schedule.after(&window_start);
        let next = iter.next();
        assert!(next.is_some());
        let next_time = next.expect("has next");
        assert!(
            next_time <= now,
            "next_time {next_time} should be <= now {now}"
        );
    }

    #[test]
    fn cron_schedule_no_match_outside_window() {
        use chrono::TimeZone;

        let expr = cron5_to_cron7("30 7 * * *");
        let schedule = Schedule::from_str(&expr).expect("parse");

        let now = Local.with_ymd_and_hms(2026, 3, 9, 8, 0, 0).unwrap();
        let window_start = now - chrono::Duration::seconds(60);

        let mut iter = schedule.after(&window_start);
        let next = iter.next().expect("has next");
        assert!(
            next > now,
            "next {next} should be > now {now} (no match in window)"
        );
    }

    #[test]
    fn cron5_to_cron7_empty_string() {
        let result = cron5_to_cron7("");
        assert_eq!(result, "0  *");
    }

    // ─── check_schedules ────────────────────────────────────────────────

    #[test]
    fn check_schedules_enqueues_matching_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let db = crate::db::Db::open_in_memory().unwrap();

        let sched = crate::models::Schedule {
            id: "every-minute".to_string(),
            cron_expr: "* * * * *".to_string(),
            prompt: "do the thing {date}".to_string(),
            schedule_type: "research".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        db.insert_schedule(&sched).unwrap();

        check_schedules(&db, &db, dir.path()).unwrap();

        let tasks = db.list_tasks(Some(crate::models::Status::Pending)).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].prompt.contains("{date}"));
        assert_eq!(tasks[0].task_type, "research");
        assert_eq!(tasks[0].model, "sonnet");
    }

    #[test]
    fn check_schedules_skips_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let db = crate::db::Db::open_in_memory().unwrap();

        let sched = crate::models::Schedule {
            id: "disabled-one".to_string(),
            cron_expr: "* * * * *".to_string(),
            prompt: "should not run".to_string(),
            schedule_type: "research".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: false,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        db.insert_schedule(&sched).unwrap();

        check_schedules(&db, &db, dir.path()).unwrap();

        let tasks = db.list_tasks(None).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn check_schedules_dedup_guard() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let db = crate::db::Db::open_in_memory().unwrap();

        let now = Local::now().format("%Y-%m-%dT%H:%M").to_string();
        let sched = crate::models::Schedule {
            id: "dedup-test".to_string(),
            cron_expr: "* * * * *".to_string(),
            prompt: "should be deduped".to_string(),
            schedule_type: "research".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec![],
            last_enqueued: now,
        };
        db.insert_schedule(&sched).unwrap();

        check_schedules(&db, &db, dir.path()).unwrap();

        let tasks = db.list_tasks(None).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn check_schedules_expands_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let db = crate::db::Db::open_in_memory().unwrap();

        let sched = crate::models::Schedule {
            id: "placeholder-test".to_string(),
            cron_expr: "* * * * *".to_string(),
            prompt: "Review for {date} ({dow})".to_string(),
            schedule_type: "review".to_string(),
            model: "opus".to_string(),
            output_path: "/tmp/report-{date}.md".to_string(),
            working_dir: "/tmp".to_string(),
            max_turns: 15,
            enabled: true,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        db.insert_schedule(&sched).unwrap();

        check_schedules(&db, &db, dir.path()).unwrap();

        let tasks = db.list_tasks(Some(crate::models::Status::Pending)).unwrap();
        assert_eq!(tasks.len(), 1);

        let today = Local::now().format("%Y-%m-%d").to_string();
        assert!(tasks[0].prompt.contains(&today));
        assert!(!tasks[0].prompt.contains("{date}"));
        assert!(!tasks[0].prompt.contains("{dow}"));
    }

    // ─── Additional edge cases ──────────────────────────────────────────

    #[test]
    fn check_schedules_bad_cron_expr_skips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let db = crate::db::Db::open_in_memory().unwrap();

        let sched = crate::models::Schedule {
            id: "bad-cron".to_string(),
            cron_expr: "not a valid cron".to_string(),
            prompt: "should not run".to_string(),
            schedule_type: "research".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        db.insert_schedule(&sched).unwrap();

        // Should not error — just skip the bad schedule
        check_schedules(&db, &db, dir.path()).unwrap();

        let tasks = db.list_tasks(None).unwrap();
        assert!(tasks.is_empty());

        // Should have logged the error
        let log_content =
            std::fs::read_to_string(dir.path().join("logs/daemon.log")).unwrap_or_default();
        assert!(log_content.contains("bad cron expr"));
    }

    #[test]
    fn check_schedules_no_schedules_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let db = crate::db::Db::open_in_memory().unwrap();
        check_schedules(&db, &db, dir.path()).unwrap();
    }

    #[test]
    fn cron_midnight_rollover() {
        use chrono::TimeZone;

        // Schedule at 23:59 — check that it matches at 23:59:30
        let expr = cron5_to_cron7("59 23 * * *");
        let schedule = Schedule::from_str(&expr).expect("parse");

        let now = Local.with_ymd_and_hms(2026, 3, 9, 23, 59, 30).unwrap();
        let window_start = now - chrono::Duration::seconds(60);

        let mut iter = schedule.after(&window_start);
        let next = iter.next().expect("has next");
        assert!(
            next <= now,
            "23:59 schedule should match at 23:59:30, got {next}"
        );
    }

    #[test]
    fn cron_midnight_exact() {
        use chrono::TimeZone;

        // Schedule at midnight (0 0)
        let expr = cron5_to_cron7("0 0 * * *");
        let schedule = Schedule::from_str(&expr).expect("parse");

        let now = Local.with_ymd_and_hms(2026, 3, 10, 0, 0, 30).unwrap();
        let window_start = now - chrono::Duration::seconds(60);

        let mut iter = schedule.after(&window_start);
        let next = iter.next().expect("has next");
        assert!(
            next <= now,
            "midnight schedule should match at 00:00:30, got {next}"
        );
    }

    // ─── Tests using FakeTaskRepo + FakeScheduleRepo (no SQLite) ─────────

    use crate::db::TaskRepository;
    use crate::db::fakes::{FakeScheduleRepo, FakeTaskRepo};

    #[test]
    fn fake_repos_enqueue_matching_schedule() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let task_repo = FakeTaskRepo::new();
        let sched_repo = FakeScheduleRepo::new();

        let sched = crate::models::Schedule {
            id: "every-minute".to_string(),
            cron_expr: "* * * * *".to_string(),
            prompt: "do the thing {date}".to_string(),
            schedule_type: "research".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        sched_repo.insert_schedule(&sched).unwrap();

        check_schedules(&task_repo, &sched_repo, dir.path()).unwrap();

        let tasks = task_repo
            .list_tasks(Some(crate::models::Status::Pending))
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].prompt.contains("{date}"));
        assert_eq!(tasks[0].task_type, "research");
    }

    #[test]
    fn fake_repos_skip_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let task_repo = FakeTaskRepo::new();
        let sched_repo = FakeScheduleRepo::new();

        let sched = crate::models::Schedule {
            id: "disabled-one".to_string(),
            cron_expr: "* * * * *".to_string(),
            prompt: "should not run".to_string(),
            schedule_type: "research".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: false,
            context_files: vec![],
            last_enqueued: String::new(),
        };
        sched_repo.insert_schedule(&sched).unwrap();

        check_schedules(&task_repo, &sched_repo, dir.path()).unwrap();

        let tasks = task_repo.list_tasks(None).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn fake_repos_dedup_guard() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();

        let task_repo = FakeTaskRepo::new();
        let sched_repo = FakeScheduleRepo::new();

        let now = Local::now().format("%Y-%m-%dT%H:%M").to_string();
        let sched = crate::models::Schedule {
            id: "dedup-test".to_string(),
            cron_expr: "* * * * *".to_string(),
            prompt: "should be deduped".to_string(),
            schedule_type: "research".to_string(),
            model: "sonnet".to_string(),
            output_path: String::new(),
            working_dir: "/tmp".to_string(),
            max_turns: 10,
            enabled: true,
            context_files: vec![],
            last_enqueued: now,
        };
        sched_repo.insert_schedule(&sched).unwrap();

        check_schedules(&task_repo, &sched_repo, dir.path()).unwrap();

        let tasks = task_repo.list_tasks(None).unwrap();
        assert!(tasks.is_empty());
    }
}
