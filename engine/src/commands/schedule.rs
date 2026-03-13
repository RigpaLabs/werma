use anyhow::{Context, Result};

use crate::db::Db;
use crate::models::Schedule;
use crate::{runner, ui};

use super::display::*;
use super::task::{AddParams, cmd_add};

/// Parameters for sched add — avoids too-many-arguments.
pub struct SchedAddParams {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub task_type: String,
    pub model: String,
    pub output: Option<String>,
    pub context: Option<String>,
    pub dir: Option<String>,
    pub turns: Option<i32>,
}

pub fn cmd_sched_add(db: &Db, p: SchedAddParams) -> Result<()> {
    let working_dir = expand_tilde(&p.dir.unwrap_or_else(default_working_dir));
    let output_path = p.output.map(|o| expand_tilde(&o)).unwrap_or_default();
    let max_turns = p.turns.unwrap_or(0);
    let context_files: Vec<String> = p
        .context
        .map(|c| c.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let sched = Schedule {
        id: p.id.clone(),
        cron_expr: p.cron.clone(),
        prompt: p.prompt.clone(),
        schedule_type: p.task_type.clone(),
        model: p.model.clone(),
        output_path: output_path.clone(),
        working_dir: working_dir.clone(),
        max_turns,
        enabled: true,
        context_files: context_files.clone(),
        last_enqueued: String::new(),
    };

    db.insert_schedule(&sched)?;

    println!("scheduled: {}", p.id);
    println!("  cron: {}", p.cron);
    println!("  type: {}, model: {}", p.task_type, p.model);
    println!("  dir: {working_dir}");
    if !output_path.is_empty() {
        println!("  output: {output_path}");
    }
    if !context_files.is_empty() {
        println!("  context: {}", context_files.join(","));
    }
    if max_turns > 0 {
        println!("  turns: {max_turns}");
    }
    println!("  prompt: {}...", truncate(&p.prompt, 70));

    Ok(())
}

pub fn cmd_sched_list(db: &Db) -> Result<()> {
    let schedules = db.list_schedules()?;

    println!();
    println!(" Schedules:");
    println!();

    if schedules.is_empty() {
        println!("  (empty)");
    } else {
        let term_width = terminal_size::terminal_size()
            .map(|(w, _)| w.0)
            .unwrap_or(100);
        let table = ui::schedule_list_table(&schedules, term_width);
        println!("{table}");

        // Show last_enqueued for schedules that have it
        for s in &schedules {
            if !s.last_enqueued.is_empty() {
                println!("    last: {}", s.last_enqueued);
            }
        }
    }
    println!();

    Ok(())
}

pub fn cmd_sched_trigger(db: &Db, id: &str) -> Result<()> {
    let sched = db
        .schedule(id)?
        .context(format!("schedule not found: {id}"))?;

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let dow = chrono::Local::now().format("%A").to_string().to_lowercase();

    let prompt = sched
        .prompt
        .replace("{date}", &today)
        .replace("{dow}", &dow);

    let output = if sched.output_path.is_empty() {
        None
    } else {
        Some(sched.output_path.replace("{date}", &today))
    };

    let turns = if sched.max_turns > 0 {
        Some(sched.max_turns)
    } else {
        None
    };

    let context = if sched.context_files.is_empty() {
        None
    } else {
        Some(sched.context_files.join(","))
    };

    cmd_add(
        db,
        AddParams {
            prompt,
            output,
            priority: 2,
            task_type: sched.schedule_type,
            model: sched.model,
            tools: None,
            dir: Some(sched.working_dir),
            turns,
            depends: None,
            context,
            linear: None,
            stage: None,
        },
    )?;

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M").to_string();
    db.set_schedule_last_enqueued(id, &now)?;

    // Run the newly enqueued task immediately.
    let dir = crate::werma_dir()?;
    match runner::run_next(db, &dir)? {
        Some(task_id) => println!("trigger: launched {task_id}"),
        None => println!("trigger: enqueued (no launchable tasks)"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn cmd_sched_add_inserts() {
        let db = test_db();
        cmd_sched_add(
            &db,
            SchedAddParams {
                id: "test-sched".into(),
                cron: "30 7 * * *".into(),
                prompt: "do stuff".into(),
                task_type: "research".into(),
                model: "sonnet".into(),
                output: None,
                context: None,
                dir: Some("/tmp".into()),
                turns: None,
            },
        )
        .unwrap();

        let sched = db.schedule("test-sched").unwrap().unwrap();
        assert_eq!(sched.cron_expr, "30 7 * * *");
        assert_eq!(sched.schedule_type, "research");
        assert!(sched.enabled);
    }

    #[test]
    fn cmd_sched_add_with_context_and_output() {
        let db = test_db();
        cmd_sched_add(
            &db,
            SchedAddParams {
                id: "with-extras".into(),
                cron: "0 9 * * *".into(),
                prompt: "report {date}".into(),
                task_type: "research".into(),
                model: "opus".into(),
                output: Some("/tmp/out-{date}.md".into()),
                context: Some("ctx1.md,ctx2.md".into()),
                dir: Some("/tmp".into()),
                turns: Some(10),
            },
        )
        .unwrap();

        let sched = db.schedule("with-extras").unwrap().unwrap();
        assert_eq!(sched.max_turns, 10);
        assert_eq!(sched.context_files, vec!["ctx1.md", "ctx2.md"]);
    }

    #[test]
    fn cmd_sched_list_empty() {
        let db = test_db();
        cmd_sched_list(&db).unwrap();
    }

    #[test]
    fn cmd_sched_trigger_not_found() {
        let db = test_db();
        let result = cmd_sched_trigger(&db, "nonexistent");
        assert!(result.is_err());
    }
}
