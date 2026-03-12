use std::io::Write;

use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use indicatif::{ProgressBar, ProgressStyle};

use crate::dashboard::truncate_line;
use crate::models::{Status, Task};

// ─── ANSI color helpers (for String buffers) ─────────────────────────────────

fn green_bold(s: &str) -> String {
    format!("\x1b[1;32m{s}\x1b[0m")
}

fn yellow(s: &str) -> String {
    format!("\x1b[33m{s}\x1b[0m")
}

fn red(s: &str) -> String {
    format!("\x1b[31m{s}\x1b[0m")
}

fn dimmed(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}

fn blue(s: &str) -> String {
    format!("\x1b[34m{s}\x1b[0m")
}

fn cyan(s: &str) -> String {
    format!("\x1b[36m{s}\x1b[0m")
}

// ─── Spinner helpers ──────────────────────────────────────────────────────────

const SPINNER_TICK_MS: u64 = 80;

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
}

/// Run a closure while showing a spinner. Returns the closure's result.
/// Auto-disables when not a TTY.
pub fn with_spinner<T, F: FnOnce() -> T>(msg: &str, f: F) -> T {
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(SPINNER_TICK_MS));

    let result = f();

    pb.finish_and_clear();
    result
}

/// Create a waiting spinner (for polling loops like run-all).
pub fn waiting_spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.yellow} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(SPINNER_TICK_MS));
    pb
}

// ─── Table helpers ────────────────────────────────────────────────────────────

/// Build a table for `werma list` output.
pub fn task_list_table(tasks: &[Task], term_width: u16) -> Table {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(term_width)
        .load_preset(comfy_table::presets::NOTHING);

    for task in tasks {
        let icon = status_icon_cell(task.status);
        let id = Cell::new(&task.id);
        let task_type = Cell::new(&task.task_type).fg(Color::Blue);
        let priority = Cell::new(format!("p{}", task.priority));
        let model = Cell::new(&task.model);

        let linear = if task.linear_issue_id.is_empty() {
            Cell::new("")
        } else {
            Cell::new(&task.linear_issue_id).fg(Color::Cyan)
        };

        let max_prompt = (term_width as usize).saturating_sub(55);
        let prompt = Cell::new(truncate_line(&task.prompt, max_prompt.max(20)));

        table.add_row(vec![icon, id, task_type, priority, model, linear, prompt]);
    }

    table
}

/// Build a table for `werma sched list` output.
pub fn schedule_list_table(schedules: &[crate::models::Schedule], term_width: u16) -> Table {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(term_width)
        .load_preset(comfy_table::presets::NOTHING);

    for s in schedules {
        let icon = if s.enabled {
            Cell::new("✓").fg(Color::Green)
        } else {
            Cell::new("○").fg(Color::DarkGrey)
        };
        let id = Cell::new(&s.id);
        let cron = Cell::new(&s.cron_expr);
        let stype = Cell::new(&s.schedule_type).fg(Color::Blue);
        let model = Cell::new(&s.model);

        let max_prompt = (term_width as usize).saturating_sub(55);
        let prompt = Cell::new(truncate_line(&s.prompt, max_prompt.max(20)));

        table.add_row(vec![icon, id, cron, stype, model, prompt]);
    }

    table
}

fn status_icon_cell(status: Status) -> Cell {
    match status {
        Status::Pending => Cell::new("○").fg(Color::Yellow),
        Status::Running => Cell::new("◉")
            .fg(Color::Green)
            .add_attribute(Attribute::Bold),
        Status::Completed => Cell::new("✓").fg(Color::DarkGrey),
        Status::Failed => Cell::new("✗").fg(Color::Red),
    }
}

// ─── Watch mode (flicker-free) ────────────────────────────────────────────────

/// Write content to stdout with flicker-free refresh.
/// Uses cursor home + line-by-line overwrite + clear remaining lines.
pub fn refresh_screen(content: &str, prev_lines: usize) {
    let mut stdout = std::io::stdout();
    // Move cursor to home position (top-left)
    let _ = write!(stdout, "\x1b[H");
    // Write content
    for line in content.lines() {
        // Write line + clear to end of line + newline
        let _ = writeln!(stdout, "{line}\x1b[K");
    }
    // Clear any remaining lines from previous render
    let current_lines = content.lines().count();
    if current_lines < prev_lines {
        for _ in 0..(prev_lines - current_lines) {
            let _ = writeln!(stdout, "\x1b[K");
        }
    }
    let _ = stdout.flush();
}

/// Hide cursor (for watch mode).
pub fn hide_cursor() {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b[?25l");
    let _ = stdout.flush();
}

/// Show cursor (for watch mode cleanup).
pub fn show_cursor() {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b[?25h");
    let _ = stdout.flush();
}

/// Format a task line into a buffer (reusable by both status and compact renderers).
pub fn write_task_line(buf: &mut String, task: &Task, time_str: &str, max_prompt: usize) {
    use std::fmt::Write;
    let linear = if task.linear_issue_id.is_empty() {
        String::new()
    } else {
        format!("  [{}]", cyan(&task.linear_issue_id))
    };
    let preview = truncate_line(&task.prompt, max_prompt);
    let _ = writeln!(
        buf,
        "   {}  {}{}  {}  {}",
        task.id,
        blue(&task.task_type),
        linear,
        dimmed(time_str),
        preview,
    );
}

// ─── Braille spinner for running tasks in watch mode ──────────────────────────

const BRAILLE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Get the current braille spinner frame based on elapsed time.
pub fn braille_frame() -> &'static str {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let idx = (ms / 100) as usize % BRAILLE_FRAMES.len();
    BRAILLE_FRAMES[idx]
}

// ─── Status rendering to buffer ──────────────────────────────────────────────

/// Render full status view into a buffer string (for watch mode).
pub fn render_status_buf(
    running: &[Task],
    pending: &[Task],
    completed: &[Task],
    failed: &[Task],
    interval: Option<u64>,
) -> String {
    use std::fmt::Write;
    let mut buf = String::new();

    let _ = writeln!(buf);

    // Running
    let spinner = braille_frame();
    let _ = writeln!(
        buf,
        " {} {}",
        green_bold(&format!("{spinner} ")),
        green_bold(&format!("running ({})", running.len())),
    );
    for task in running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(crate::format_elapsed_since)
            .unwrap_or_default();
        write_task_line(&mut buf, task, &elapsed, 45);
    }

    // Pending
    let _ = writeln!(
        buf,
        " {} {}",
        yellow("○"),
        yellow(&format!("pending ({})", pending.len())),
    );
    for task in pending.iter().take(5) {
        let prio = format!("p{}", task.priority);
        write_task_line(&mut buf, task, &prio, 45);
    }
    if pending.len() > 5 {
        let _ = writeln!(buf, "   {}", dimmed(&format!("... +{} more", pending.len() - 5)));
    }

    // Completed + Failed
    let _ = writeln!(
        buf,
        " {} {}     {} {}",
        dimmed("✓"),
        dimmed(&format!("completed ({})", completed.len())),
        red("✗"),
        red(&format!("failed ({})", failed.len())),
    );

    let recent: Vec<&Task> = completed.iter().rev().take(10).collect();
    let failed_recent: Vec<&Task> = failed.iter().rev().take(5).collect();

    for task in &recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        write_task_line(&mut buf, task, &dur, 45);
    }

    for task in &failed_recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        write_task_line(&mut buf, task, &dur, 45);
    }

    if let Some(secs) = interval {
        let _ = writeln!(
            buf,
            "                                                              ↻ {secs}s"
        );
    }

    buf
}

/// Render compact status view into a buffer string (for watch mode).
pub fn render_compact_buf(
    running: &[Task],
    pending: &[Task],
    completed: &[Task],
    failed: &[Task],
    interval: Option<u64>,
) -> String {
    use std::fmt::Write;
    let mut buf = String::new();

    let sep = "───────────────────────────────────";
    let spinner = braille_frame();

    let _ = writeln!(
        buf,
        " werma {} {} running  {} {} pending",
        green_bold(spinner),
        green_bold(&running.len().to_string()),
        yellow("○"),
        yellow(&pending.len().to_string()),
    );
    let _ = writeln!(buf, " {sep}");

    for task in running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(crate::format_elapsed_since)
            .unwrap_or_default();
        let linear = compact_linear_label_colored(&task.linear_issue_id);
        let _ = writeln!(
            buf,
            " {} {} {}{} {}",
            green_bold(spinner),
            compact_task_id(&task.id),
            blue(compact_task_type(&task.task_type)),
            linear,
            dimmed(&elapsed),
        );
    }

    for task in pending.iter().take(3) {
        let linear = compact_linear_label_colored(&task.linear_issue_id);
        let _ = writeln!(
            buf,
            " {} {} {}{}",
            yellow("○"),
            compact_task_id(&task.id),
            blue(compact_task_type(&task.task_type)),
            linear,
        );
    }
    if pending.len() > 3 {
        let _ = writeln!(buf, " {}", dimmed(&format!("  +{} more", pending.len() - 3)));
    }

    // Only show separator if there were running or pending tasks above
    if !running.is_empty() || !pending.is_empty() {
        let _ = writeln!(buf, " {sep}");
    }

    let recent: Vec<&Task> = completed.iter().rev().take(5).collect();
    for task in &recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = compact_linear_label_dimmed(&task.linear_issue_id);
        let _ = writeln!(
            buf,
            " {} {} {}{} {}",
            dimmed("✓"),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear,
            dimmed(&dur),
        );
    }

    let failed_recent: Vec<&Task> = failed.iter().rev().take(5).collect();
    for task in &failed_recent {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = compact_linear_label_dimmed(&task.linear_issue_id);
        let _ = writeln!(
            buf,
            " {} {} {}{} {}",
            red("✗"),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear,
            dimmed(&dur),
        );
    }

    let _ = writeln!(buf, " {sep}");

    let refresh_str = if let Some(secs) = interval {
        format!("  ↻ {secs}s")
    } else {
        String::new()
    };
    let _ = writeln!(
        buf,
        " {} done  {} fail{}",
        dimmed(&completed.len().to_string()),
        red(&failed.len().to_string()),
        dimmed(&refresh_str),
    );

    buf
}

fn compact_task_type(task_type: &str) -> &str {
    match task_type {
        "pipeline-engineer" => "engineer",
        "pipeline-analyst" => "analyst",
        "pipeline-reviewer" => "reviewer",
        "pipeline-devops" => "devops",
        other => other,
    }
}

fn compact_task_id(id: &str) -> &str {
    id.rsplit('-').next().unwrap_or(id)
}

fn compact_linear_label(linear_issue_id: &str) -> String {
    if linear_issue_id.is_empty() {
        String::new()
    } else {
        format!(" [{linear_issue_id}]")
    }
}

fn compact_linear_label_colored(linear_issue_id: &str) -> String {
    if linear_issue_id.is_empty() {
        String::new()
    } else {
        format!(" {}", cyan(&format!("[{linear_issue_id}]")))
    }
}

fn compact_linear_label_dimmed(linear_issue_id: &str) -> String {
    if linear_issue_id.is_empty() {
        String::new()
    } else {
        format!(" {}", dimmed(&format!("[{linear_issue_id}]")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braille_frame_returns_valid() {
        let frame = braille_frame();
        assert!(BRAILLE_FRAMES.contains(&frame));
    }

    #[test]
    fn compact_helpers() {
        assert_eq!(compact_task_id("20260312-035"), "035");
        assert_eq!(compact_task_type("pipeline-engineer"), "engineer");
        assert_eq!(compact_task_type("research"), "research");
        assert_eq!(compact_linear_label(""), "");
        assert_eq!(compact_linear_label("RIG-179"), " [RIG-179]");
    }

    #[test]
    fn task_list_table_renders() {
        let tasks = vec![Task {
            id: "20260312-001".to_string(),
            status: Status::Pending,
            priority: 2,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
            task_type: "research".to_string(),
            prompt: "Do something".to_string(),
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
        }];

        let table = task_list_table(&tasks, 100);
        let rendered = table.to_string();
        assert!(rendered.contains("20260312-001"));
        assert!(rendered.contains("research"));
        assert!(rendered.contains("Do something"));
    }

    #[test]
    fn status_buf_renders() {
        let buf = render_status_buf(&[], &[], &[], &[], Some(3));
        assert!(buf.contains("running (0)"));
        assert!(buf.contains("pending (0)"));
        assert!(buf.contains("↻ 3s"));
        // Verify ANSI color codes are present
        assert!(buf.contains("\x1b["), "expected ANSI color codes in output");
    }

    #[test]
    fn compact_buf_renders() {
        let buf = render_compact_buf(&[], &[], &[], &[], Some(5));
        // ANSI codes wrap the numbers, so check for the text without exact formatting
        assert!(buf.contains("running"));
        assert!(buf.contains("pending"));
        assert!(buf.contains("↻ 5s"));
        // Verify ANSI color codes are present
        assert!(buf.contains("\x1b["), "expected ANSI color codes in output");
    }
}
