use anyhow::Result;

use crate::db::Db;
use crate::models::{Effect, EffectStatus};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";

fn status_color(status: &EffectStatus) -> &'static str {
    match status {
        EffectStatus::Pending => CYAN,
        EffectStatus::Running => CYAN,
        EffectStatus::Done => GREEN,
        EffectStatus::Failed => YELLOW,
        EffectStatus::Dead => RED,
    }
}

fn print_effects_table(effects: &[Effect]) {
    if effects.is_empty() {
        println!("No effects found.");
        return;
    }

    println!(
        "{BOLD}{:<6} {:<20} {:<16} {:<10} {:<8} {:<10} ERROR{RESET}",
        "ID", "TASK", "TYPE", "ISSUE", "STATUS", "ATTEMPTS"
    );
    println!("{DIM}{}{RESET}", "-".repeat(90));

    for e in effects {
        let status_str = format!("{}", e.status);
        let color = status_color(&e.status);
        let attempts = format!("{}/{}", e.attempts, e.max_attempts);
        let error = e
            .error
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect::<String>();

        println!(
            "{:<6} {:<20} {:<16} {:<10} {color}{:<8}{RESET} {:<10} {}",
            e.id,
            truncate(&e.task_id, 20),
            format!("{}", e.effect_type),
            truncate(&e.issue_id, 10),
            status_str,
            attempts,
            error,
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

/// `werma effects` — list pending + failed effects.
pub fn cmd_effects_list(db: &Db) -> Result<()> {
    let effects = db.pending_and_failed_effects()?;
    println!("{BOLD}Pending + Failed Effects{RESET}");
    print_effects_table(&effects);
    Ok(())
}

/// `werma effects dead` — list dead-lettered effects.
pub fn cmd_effects_dead(db: &Db) -> Result<()> {
    let effects = db.dead_effects()?;
    println!("{BOLD}Dead-lettered Effects{RESET}");
    print_effects_table(&effects);
    Ok(())
}

/// `werma effects retry <id>` — reset a dead/failed effect back to pending.
pub fn cmd_effects_retry(db: &Db, id: i64) -> Result<()> {
    let changed = db.retry_effect(id)?;
    if changed {
        println!("Effect {id} reset to pending.");
    } else {
        println!("Effect {id} not found or not in a retryable state (must be dead or failed).");
    }
    Ok(())
}

/// `werma effects history <task_id>` — show all effects for a given task.
pub fn cmd_effects_history(db: &Db, task_id: &str) -> Result<()> {
    let effects = db.effects_for_task(task_id)?;
    println!("{BOLD}Effects for task {task_id}{RESET}");
    print_effects_table(&effects);
    Ok(())
}
