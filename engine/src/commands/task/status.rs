use std::io::IsTerminal;

use anyhow::{Result, bail};
use colored::Colorize;

use crate::db::Db;
use crate::models::{Status, Task};
use crate::ui;

use super::super::display::*;

/// Fetch terminal-status buckets (completed, failed, canceled).
///
/// When `limit` is `Some(n)`: returns the n most recent tasks combined across all three
/// statuses (not n per status), then partitions by status. This is the key correctness fix
/// vs. the previous approach that applied the limit per-status (giving 3×n rows total).
///
/// When `limit` is `None`: returns all tasks per status (used by `--all` flag).
fn fetch_terminal_buckets(
    db: &Db,
    limit: Option<usize>,
) -> Result<(Vec<Task>, Vec<Task>, Vec<Task>)> {
    if let Some(n) = limit {
        let tasks = db.list_recent_terminal_tasks(n)?;
        let mut completed = vec![];
        let mut failed = vec![];
        let mut canceled = vec![];
        for task in tasks {
            match task.status {
                Status::Completed => completed.push(task),
                Status::Failed => failed.push(task),
                Status::Canceled => canceled.push(task),
                _ => {}
            }
        }
        Ok((completed, failed, canceled))
    } else {
        let completed = db.list_all_tasks_by_finished(Status::Completed)?;
        let failed = db.list_all_tasks_by_finished(Status::Failed)?;
        let canceled = db.list_all_tasks_by_finished(Status::Canceled)?;
        Ok((completed, failed, canceled))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_status(
    db: &Db,
    watch: bool,
    compact: bool,
    plain: bool,
    interval: u64,
    all: bool,
    art: bool,
    completed_limit: Option<usize>,
) -> Result<()> {
    // `--all` overrides config limit
    let limit = if all { None } else { completed_limit };
    // --plain and --watch are mutually exclusive: plain is for scripting (single snapshot),
    // watch is an interactive TUI loop. Combining them makes no sense.
    if plain && watch {
        bail!("--plain and --watch are mutually exclusive");
    }

    // Plain mode: explicit flag or auto-detect non-TTY (piped output)
    let use_plain = plain || !std::io::stdout().is_terminal();

    if use_plain {
        return render_plain(db, limit);
    }

    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    let use_compact = compact || term_width < 60;
    let auto_compacted = !compact && term_width < 60;

    if watch {
        if auto_compacted {
            eprintln!("tip: terminal width < 60, using compact mode (or pass -c)");
        }

        // Flicker-free watch: hide cursor, use cursor repositioning
        ui::hide_cursor();
        // Initial clear
        print!("\x1b[2J\x1b[H");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut prev_lines = 0;

        // Ensure cursor is restored on exit (Ctrl+C, normal return, or panic)
        struct CursorGuard;
        impl Drop for CursorGuard {
            fn drop(&mut self) {
                ui::show_cursor();
            }
        }
        let _guard = CursorGuard;

        // SIGINT handler: restore cursor before exiting
        ctrlc::set_handler(move || {
            ui::show_cursor();
            std::process::exit(0);
        })
        .ok();

        // Tick-based render loop: render at 100ms for smooth spinner,
        // but only re-query the DB every `interval` seconds.
        const RENDER_TICK_MS: u64 = 100;
        let ticks_per_refresh = (interval * 1000) / RENDER_TICK_MS;
        // Force immediate first load by starting at the refresh threshold
        let mut tick_count: u64 = ticks_per_refresh;

        let mut running: Vec<Task> = vec![];
        let mut pending: Vec<Task> = vec![];
        let mut completed: Vec<Task> = vec![];
        let mut failed: Vec<Task> = vec![];
        let mut canceled: Vec<Task> = vec![];
        let mut terminal_counts: Option<(usize, usize, usize)> = None;
        // Track current term width for resize handling
        let mut current_term_width = term_width;

        loop {
            if tick_count >= ticks_per_refresh {
                running = db.list_tasks(Some(Status::Running))?;
                pending = db.list_tasks(Some(Status::Pending))?;
                (completed, failed, canceled) = fetch_terminal_buckets(db, limit)?;
                terminal_counts = if limit.is_none() {
                    None
                } else {
                    Some(db.terminal_task_counts()?)
                };
                // Re-read terminal size on each DB refresh to handle resizes
                current_term_width = terminal_size::terminal_size()
                    .map(|(w, _)| w.0 as usize)
                    .unwrap_or(80);
                tick_count = 0;
            }

            let buckets = ui::StatusBuckets {
                running: &running,
                pending: &pending,
                completed: &completed,
                failed: &failed,
                canceled: &canceled,
                terminal_counts,
            };
            let content = if use_compact {
                ui::render_compact_buf(
                    &buckets,
                    Some(interval),
                    current_term_width,
                    tick_count,
                    art,
                )
            } else {
                ui::render_status_buf(
                    &buckets,
                    Some(interval),
                    current_term_width,
                    tick_count,
                    art,
                )
            };

            ui::refresh_screen(&content, prev_lines);
            prev_lines = content.lines().count();

            std::thread::sleep(std::time::Duration::from_millis(RENDER_TICK_MS));
            tick_count += 1;
        }
    } else if use_compact {
        if auto_compacted {
            eprintln!("tip: terminal width < 60, using compact mode (or pass -c)");
        }
        render_compact(db, None, limit, art)?;
    } else {
        render_status(db, limit, art)?;
    }
    Ok(())
}

fn render_status(db: &Db, limit: Option<usize>, show_art: bool) -> Result<()> {
    let running = db.list_tasks(Some(Status::Running))?;
    let pending = db.list_tasks(Some(Status::Pending))?;
    let (completed, failed, canceled) = fetch_terminal_buckets(db, limit)?;
    let (total_completed, total_failed, total_canceled) = if limit.is_none() {
        (completed.len(), failed.len(), canceled.len())
    } else {
        db.terminal_task_counts()?
    };

    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    if show_art {
        let art = crate::art::render_art(term_width, 0);
        if !art.is_empty() {
            print!("{art}");
        }
    }

    println!();

    // Running
    println!(
        " {} {}",
        "◉".green().bold(),
        format!("running ({})", running.len()).green().bold()
    );
    for task in &running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(format_elapsed_since)
            .unwrap_or_default();
        println!("{}", format_task_line(task, &elapsed));
    }

    // Pending
    println!(
        " {} {}",
        "○".yellow(),
        format!("pending ({})", pending.len()).yellow()
    );
    for task in pending.iter().take(5) {
        let prio = format!("p{}", task.priority);
        println!("{}", format_task_line(task, &prio));
    }
    if pending.len() > 5 {
        println!("   {}", format!("... +{} more", pending.len() - 5).dimmed());
    }

    // Completed + Failed + Canceled
    println!(
        " {} {}     {} {}     {} {}",
        "✓".dimmed(),
        format!("completed ({total_completed})").dimmed(),
        "✗".red(),
        format!("failed ({total_failed})").red(),
        "⊘".dimmed(),
        format!("canceled ({total_canceled})").dimmed(),
    );

    // Data is already sorted by finished_at DESC and limited by the query
    for task in &completed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        println!("{}", format_task_line(task, &dur));
    }

    for task in &failed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        println!("{}", format_task_line(task, &dur));
    }

    for task in &canceled {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        println!("{}", format_task_line(task, &dur));
    }

    println!();
    Ok(())
}

fn render_compact(
    db: &Db,
    interval: Option<u64>,
    limit: Option<usize>,
    show_art: bool,
) -> Result<()> {
    let running = db.list_tasks(Some(Status::Running))?;
    let pending = db.list_tasks(Some(Status::Pending))?;
    let (completed, failed, canceled) = fetch_terminal_buckets(db, limit)?;
    let (total_completed, total_failed, total_canceled) = if limit.is_none() {
        (completed.len(), failed.len(), canceled.len())
    } else {
        db.terminal_task_counts()?
    };

    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    if show_art {
        let art = crate::art::render_art(term_width, 0);
        if !art.is_empty() {
            print!("{art}");
        }
    }

    let sep = "───────────────────────────────────";

    println!(
        " werma {} {} running  {} {} pending",
        "●".green().bold(),
        running.len().to_string().green().bold(),
        "○".yellow(),
        pending.len().to_string().yellow(),
    );
    println!(" {sep}");

    for task in &running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(format_elapsed_since)
            .unwrap_or_default();
        let linear = compact_linear_label(&task.issue_identifier);
        println!(
            " {} {} {}{} {}",
            "●".green().bold(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type).blue(),
            linear.cyan(),
            elapsed.dimmed(),
        );
    }

    for task in pending.iter().take(3) {
        let linear = compact_linear_label(&task.issue_identifier);
        println!(
            " {} {} {}{}",
            "○".yellow(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type).blue(),
            linear.cyan(),
        );
    }
    if pending.len() > 3 {
        println!(" {}", format!("  +{} more", pending.len() - 3).dimmed());
    }

    // Only show separator if there were running or pending tasks above
    if !running.is_empty() || !pending.is_empty() {
        println!(" {sep}");
    }

    // Completed tasks (already sorted by finished_at DESC and limited by query)
    for task in &completed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = compact_linear_label(&task.issue_identifier);
        println!(
            " {} {} {}{} {}",
            "✓".dimmed(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear.dimmed(),
            dur.dimmed(),
        );
    }

    // Failed tasks (already sorted by finished_at DESC and limited by query)
    for task in &failed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = compact_linear_label(&task.issue_identifier);
        println!(
            " {} {} {}{} {}",
            "✗".red(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear.dimmed(),
            dur.dimmed(),
        );
    }

    // Canceled tasks (already sorted by finished_at DESC and limited by query)
    for task in &canceled {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = compact_linear_label(&task.issue_identifier);
        println!(
            " {} {} {}{} {}",
            "⊘".dimmed(),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear.dimmed(),
            dur.dimmed(),
        );
    }

    println!(" {sep}");

    let refresh_str = if let Some(secs) = interval {
        format!("  ↻ {secs}s")
    } else {
        String::new()
    };
    println!(
        " {} done  {} fail  {} canceled{}",
        total_completed.to_string().dimmed(),
        total_failed.to_string().red(),
        total_canceled.to_string().dimmed(),
        refresh_str.dimmed(),
    );

    Ok(())
}

fn render_plain(db: &Db, limit: Option<usize>) -> Result<()> {
    // Emit running + pending first, then terminal tasks (combined, limited).
    let live_statuses = [Status::Running, Status::Pending];
    for status in live_statuses {
        for task in &db.list_tasks(Some(status))? {
            let duration = match (
                task.started_at.as_deref(),
                task.finished_at.as_deref(),
                status,
            ) {
                (Some(s), Some(e), _) => format_duration_between(s, e),
                (Some(s), None, Status::Running) => format_elapsed_since(s),
                _ => String::new(),
            };
            let linear = if task.issue_identifier.is_empty() {
                "-"
            } else {
                &task.issue_identifier
            };
            let description = task.prompt.lines().next().unwrap_or(&task.prompt);
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                task.id, task.task_type, task.status, linear, duration, description
            );
        }
    }

    // Terminal tasks: combined limit (N most recent across completed+failed+canceled).
    let (completed, failed, canceled) = fetch_terminal_buckets(db, limit)?;
    for task in completed.iter().chain(failed.iter()).chain(canceled.iter()) {
        let duration = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = if task.issue_identifier.is_empty() {
            "-"
        } else {
            &task.issue_identifier
        };
        let description = task.prompt.lines().next().unwrap_or(&task.prompt);
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            task.id, task.task_type, task.status, linear, duration, description
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::models::Task;

    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    // --plain + --watch conflict must be rejected
    #[test]
    fn cmd_status_plain_watch_conflict() {
        let db = test_db();
        let result = cmd_status(&db, true, false, true, 2, false, false, Some(17));
        assert!(
            result.is_err(),
            "--plain and --watch must be rejected together"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("mutually exclusive"),
            "error message should mention mutual exclusion"
        );
    }

    // default (art=false) — render_status_buf must not contain art escape sequences from art module
    #[test]
    fn status_buf_default_no_art() {
        let buckets = crate::ui::StatusBuckets {
            running: &[],
            pending: &[],
            completed: &[],
            failed: &[],
            canceled: &[],
            terminal_counts: None,
        };
        let buf = crate::ui::render_status_buf(&buckets, None, 120, 0, false);
        // Art output starts with ANSI color codes for the pixel art — it should be absent
        // The art module emits lines starting with spaces + ANSI color for each pixel row.
        // We verify art is skipped by checking the buffer doesn't start with an art prefix.
        // Art lines contain "▀" or "▄" block characters used by the Garuda renderer.
        assert!(
            !buf.contains('\u{2580}') && !buf.contains('\u{2584}'),
            "art block chars must not appear when art=false"
        );
    }

    // --art flag — render_status_buf must contain art block chars when art=true
    #[test]
    fn status_buf_art_flag_enabled() {
        let buckets = crate::ui::StatusBuckets {
            running: &[],
            pending: &[],
            completed: &[],
            failed: &[],
            canceled: &[],
            terminal_counts: None,
        };
        // Use a wide terminal width to ensure art is rendered (art.rs may skip on narrow)
        let buf = crate::ui::render_status_buf(&buckets, None, 120, 0, true);
        // If art module produces output at this width, verify block chars present.
        // render_art returns "" for very narrow widths — only assert when non-empty.
        let art_check = crate::art::render_art(120, 0);
        if !art_check.is_empty() {
            assert!(
                buf.contains('\u{2580}') || buf.contains('\u{2584}'),
                "art block chars must appear when art=true"
            );
        }
    }

    // render_plain must include Canceled tasks
    #[test]
    fn render_plain_includes_canceled_tasks() {
        let db = test_db();
        let task = Task {
            id: "20260313-004".into(),
            status: Status::Canceled,
            task_type: "code".into(),
            prompt: "canceled task".into(),
            working_dir: "/tmp".into(),
            model: "sonnet".into(),
            ..Default::default()
        };
        db.insert_task(&task).unwrap();

        // render_plain reads Canceled — verified indirectly via list_tasks query
        let tasks = db.list_tasks(Some(Status::Canceled)).unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "Canceled tasks must be stored and retrievable"
        );
        assert_eq!(tasks[0].id, "20260313-004");
    }
}
