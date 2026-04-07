use std::io::Write;

use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use indicatif::{ProgressBar, ProgressStyle};

use crate::config::TrackerConfig;
use crate::dashboard::truncate_line;
use crate::models::{Status, Task};

/// Task lists grouped by status, used by the rendering functions.
pub struct StatusBuckets<'a> {
    pub running: &'a [Task],
    pub pending: &'a [Task],
    pub completed: &'a [Task],
    pub failed: &'a [Task],
    pub canceled: &'a [Task],
    /// Total counts for terminal statuses (may differ from slice len when limited).
    /// (completed_total, failed_total, canceled_total)
    pub terminal_counts: Option<(usize, usize, usize)>,
}

impl StatusBuckets<'_> {
    /// Get total counts for completed/failed/canceled, using terminal_counts if available.
    fn totals(&self) -> (usize, usize, usize) {
        self.terminal_counts.unwrap_or((
            self.completed.len(),
            self.failed.len(),
            self.canceled.len(),
        ))
    }
}

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

    let cfg = crate::config::UserConfig::load();
    let tracker = &cfg.tracker;

    for task in tasks {
        let icon = status_icon_cell(task.status);
        let id = Cell::new(&task.id);
        let task_type = Cell::new(&task.task_type).fg(Color::Blue);
        let priority = Cell::new(format!("p{}", task.priority));
        let model = Cell::new(&task.model);

        let linear = if task.issue_identifier.is_empty() {
            Cell::new("")
        } else {
            let display_id = tracker.display_identifier(&task.issue_identifier);
            Cell::new(display_id).fg(Color::Cyan)
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
        Status::Canceled => Cell::new("⊘").fg(Color::DarkGrey),
    }
}

// ─── Watch mode (flicker-free) ────────────────────────────────────────────────

/// Count visible (non-ANSI) characters in a string.
///
/// ANSI escape sequences (`\x1b[...m`, `\x1b[...{letter}`) are skipped so that
/// only the characters that occupy terminal columns are counted.
fn ansi_visual_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Consume until the terminating ASCII letter (e.g. 'm', 'K', 'J', 'H')
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Truncate a string that may contain ANSI escape codes to `max_width` visible columns.
///
/// If the visible length exceeds `max_width`, the output is clipped to
/// `max_width - 1` visible chars followed by `…`, then a color-reset sequence
/// (`\x1b[0m`) to prevent color bleed into adjacent lines.
pub fn ansi_truncate(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if ansi_visual_len(s) <= max_width {
        return s.to_string();
    }
    // Reserve one column for the ellipsis character.
    let target = max_width.saturating_sub(1);
    let mut visible = 0usize;
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;

    for ch in s.chars() {
        if ch == '\x1b' {
            in_escape = true;
            result.push(ch);
        } else if in_escape {
            result.push(ch);
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if visible < target {
            result.push(ch);
            visible += 1;
        } else {
            // Reached the visible limit — append ellipsis and stop.
            break;
        }
    }
    result.push('…');
    result.push_str("\x1b[0m");
    result
}

/// Write content to stdout with flicker-free refresh.
///
/// Each line is truncated to `term_width` visible columns before writing so that
/// long lines never wrap in the terminal — wrapping breaks the cursor-home
/// refresh strategy and makes the layout explode on narrow panels.
pub fn refresh_screen(content: &str, prev_lines: usize, term_width: usize) {
    let mut stdout = std::io::stdout();
    // Move cursor to home position (top-left)
    let _ = write!(stdout, "\x1b[H");
    // Write content, truncating each line to the terminal width
    for line in content.lines() {
        let safe = if term_width > 0 {
            ansi_truncate(line, term_width)
        } else {
            line.to_string()
        };
        // Write line + clear to end of line + newline
        let _ = writeln!(stdout, "{safe}\x1b[K");
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
pub fn write_task_line(
    buf: &mut String,
    task: &Task,
    time_str: &str,
    max_prompt: usize,
    cfg: &crate::config::UserConfig,
) {
    use std::fmt::Write;
    let linear = if task.issue_identifier.is_empty() {
        String::new()
    } else {
        let display_id = cfg.tracker.display_identifier(&task.issue_identifier);
        format!("  [{}]", cyan(&display_id))
    };
    let cost_turns = crate::commands::display::format_cost_turns(task, cfg);
    let preview = truncate_line(&task.prompt, max_prompt);
    let _ = writeln!(
        buf,
        "   {}  {}{}  {}{}  {}",
        task.id,
        blue(&task.task_type),
        linear,
        dimmed(time_str),
        dimmed(&cost_turns),
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
    buckets: &StatusBuckets<'_>,
    interval: Option<u64>,
    term_width: usize,
    tick: u64,
    show_art: bool,
) -> String {
    use std::fmt::Write;
    let StatusBuckets {
        running,
        pending,
        completed,
        failed,
        canceled,
        ..
    } = buckets;
    let mut buf = String::new();
    let cfg = crate::config::UserConfig::load();

    // Pixel art mascot header (opt-in via --art flag)
    if show_art {
        let art = crate::art::render_art(term_width, tick);
        if !art.is_empty() {
            buf.push_str(&art);
        }
    }

    let _ = writeln!(buf);

    // Running
    let spinner = braille_frame();
    let _ = writeln!(
        buf,
        " {} {}",
        green_bold(spinner),
        green_bold(&format!("running ({})", running.len())),
    );
    for task in *running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(crate::format_elapsed_since)
            .unwrap_or_default();
        write_task_line(&mut buf, task, &elapsed, 45, &cfg);
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
        write_task_line(&mut buf, task, &prio, 45, &cfg);
    }
    if pending.len() > 5 {
        let _ = writeln!(
            buf,
            "   {}",
            dimmed(&format!("... +{} more", pending.len() - 5))
        );
    }

    // Completed + Failed + Canceled
    let (total_completed, total_failed, total_canceled) = buckets.totals();
    let _ = writeln!(
        buf,
        " {} {}     {} {}     {} {}",
        dimmed("✓"),
        dimmed(&format!("completed ({total_completed})")),
        red("✗"),
        red(&format!("failed ({total_failed})")),
        dimmed("⊘"),
        dimmed(&format!("canceled ({total_canceled})")),
    );

    // Data is already sorted by finished_at DESC and limited by the caller
    for task in *completed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        write_task_line(&mut buf, task, &dur, 45, &cfg);
    }

    for task in *failed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        write_task_line(&mut buf, task, &dur, 45, &cfg);
    }

    for task in *canceled {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        write_task_line(&mut buf, task, &dur, 45, &cfg);
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
    buckets: &StatusBuckets<'_>,
    interval: Option<u64>,
    term_width: usize,
    tick: u64,
    show_art: bool,
) -> String {
    use std::fmt::Write;
    let StatusBuckets {
        running,
        pending,
        completed,
        failed,
        canceled,
        ..
    } = buckets;
    let mut buf = String::new();
    let cfg = crate::config::UserConfig::load();
    let tracker = &cfg.tracker;

    // Pixel art mascot header (opt-in via --art flag)
    if show_art {
        let art = crate::art::render_art(term_width, tick);
        if !art.is_empty() {
            buf.push_str(&art);
        }
    }

    // Width-aware field visibility:
    //   < 40 cols → hide identifier (too narrow for [honeyjourney#20] etc.)
    //   < 60 cols → hide model/turns metadata
    let show_identifier = term_width >= 40;
    let show_cost_turns = term_width >= 60;

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

    for task in *running {
        let elapsed = task
            .started_at
            .as_deref()
            .map(crate::format_elapsed_since)
            .unwrap_or_default();
        let linear = if show_identifier {
            compact_linear_label_colored(&task.issue_identifier, tracker)
        } else {
            String::new()
        };
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
        let linear = if show_identifier {
            compact_linear_label_colored(&task.issue_identifier, tracker)
        } else {
            String::new()
        };
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
        let _ = writeln!(
            buf,
            " {}",
            dimmed(&format!("  +{} more", pending.len() - 3))
        );
    }

    // Only show separator if there were running or pending tasks above
    if !running.is_empty() || !pending.is_empty() {
        let _ = writeln!(buf, " {sep}");
    }

    // Data is already sorted by finished_at DESC and limited by the caller
    for task in *completed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = if show_identifier {
            compact_linear_label_dimmed(&task.issue_identifier, tracker)
        } else {
            String::new()
        };
        let cost_turns = if show_cost_turns {
            crate::commands::display::format_cost_turns(task, &cfg)
        } else {
            String::new()
        };
        let _ = writeln!(
            buf,
            " {} {} {}{} {}{}",
            dimmed("✓"),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear,
            dimmed(&dur),
            dimmed(&cost_turns),
        );
    }

    for task in *failed {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = if show_identifier {
            compact_linear_label_dimmed(&task.issue_identifier, tracker)
        } else {
            String::new()
        };
        let cost_turns = if show_cost_turns {
            crate::commands::display::format_cost_turns(task, &cfg)
        } else {
            String::new()
        };
        let _ = writeln!(
            buf,
            " {} {} {}{} {}{}",
            red("✗"),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear,
            dimmed(&dur),
            dimmed(&cost_turns),
        );
    }

    for task in *canceled {
        let dur = match (task.started_at.as_deref(), task.finished_at.as_deref()) {
            (Some(s), Some(e)) => crate::format_duration_between(s, e),
            _ => String::new(),
        };
        let linear = if show_identifier {
            compact_linear_label_dimmed(&task.issue_identifier, tracker)
        } else {
            String::new()
        };
        let cost_turns = if show_cost_turns {
            crate::commands::display::format_cost_turns(task, &cfg)
        } else {
            String::new()
        };
        let _ = writeln!(
            buf,
            " {} {} {}{} {}{}",
            dimmed("⊘"),
            compact_task_id(&task.id),
            compact_task_type(&task.task_type),
            linear,
            dimmed(&dur),
            dimmed(&cost_turns),
        );
    }

    let _ = writeln!(buf, " {sep}");

    let refresh_str = if let Some(secs) = interval {
        format!("  ↻ {secs}s")
    } else {
        String::new()
    };
    let (total_completed, total_failed, total_canceled) = buckets.totals();
    let _ = writeln!(
        buf,
        " {} done  {} fail  {} canceled{}",
        dimmed(&total_completed.to_string()),
        red(&total_failed.to_string()),
        dimmed(&total_canceled.to_string()),
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

#[allow(dead_code)]
fn compact_linear_label(issue_identifier: &str, tracker: &TrackerConfig) -> String {
    if issue_identifier.is_empty() {
        String::new()
    } else {
        let display_id = tracker.display_identifier(issue_identifier);
        format!(" [{display_id}]")
    }
}

fn compact_linear_label_colored(issue_identifier: &str, tracker: &TrackerConfig) -> String {
    if issue_identifier.is_empty() {
        String::new()
    } else {
        let display_id = tracker.display_identifier(issue_identifier);
        format!(" {}", cyan(&format!("[{display_id}]")))
    }
}

fn compact_linear_label_dimmed(issue_identifier: &str, tracker: &TrackerConfig) -> String {
    if issue_identifier.is_empty() {
        String::new()
    } else {
        let display_id = tracker.display_identifier(issue_identifier);
        format!(" {}", dimmed(&format!("[{display_id}]")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_visual_len_plain() {
        assert_eq!(ansi_visual_len("hello"), 5);
    }

    #[test]
    fn ansi_visual_len_with_ansi() {
        // "\x1b[1;32mfoo\x1b[0m" → 3 visible chars
        assert_eq!(ansi_visual_len("\x1b[1;32mfoo\x1b[0m"), 3);
    }

    #[test]
    fn ansi_visual_len_empty() {
        assert_eq!(ansi_visual_len(""), 0);
    }

    #[test]
    fn ansi_truncate_no_truncation_needed() {
        let s = "hello";
        assert_eq!(ansi_truncate(s, 10), s);
    }

    #[test]
    fn ansi_truncate_plain_string() {
        let result = ansi_truncate("abcdefghij", 5);
        // 4 visible chars + '…' + reset
        assert!(result.starts_with("abcd…"));
        assert!(result.contains("\x1b[0m"));
    }

    #[test]
    fn ansi_truncate_with_ansi_codes() {
        // colored "hello world" = 11 visible chars, truncate to 6 → "hello…" (5+1)
        let colored = format!("\x1b[32mhello world\x1b[0m");
        let result = ansi_truncate(&colored, 6);
        assert_eq!(ansi_visual_len(&result), 6); // 5 chars + '…'
        assert!(result.ends_with("\x1b[0m"));
    }

    #[test]
    fn ansi_truncate_zero_width() {
        assert_eq!(ansi_truncate("anything", 0), "");
    }

    #[test]
    fn braille_frame_returns_valid() {
        let frame = braille_frame();
        assert!(BRAILLE_FRAMES.contains(&frame));
    }

    #[test]
    fn compact_helpers() {
        let tracker = TrackerConfig::default();
        assert_eq!(compact_task_id("20260312-035"), "035");
        assert_eq!(compact_task_type("pipeline-engineer"), "engineer");
        assert_eq!(compact_task_type("research"), "research");
        assert_eq!(compact_linear_label("", &tracker), "");
        assert_eq!(compact_linear_label("RIG-179", &tracker), " [RIG-179]");
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
            issue_identifier: String::new(),
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
        }];

        let table = task_list_table(&tasks, 100);
        let rendered = table.to_string();
        assert!(rendered.contains("20260312-001"));
        assert!(rendered.contains("research"));
        assert!(rendered.contains("Do something"));
    }

    #[test]
    fn status_buf_renders() {
        let buckets = StatusBuckets {
            running: &[],
            pending: &[],
            completed: &[],
            failed: &[],
            canceled: &[],
            terminal_counts: None,
        };
        let buf = render_status_buf(&buckets, Some(3), 80, 0, false);
        assert!(buf.contains("running (0)"));
        assert!(buf.contains("pending (0)"));
        assert!(buf.contains("↻ 3s"));
        // Verify ANSI color codes are present
        assert!(buf.contains("\x1b["), "expected ANSI color codes in output");
    }

    #[test]
    fn compact_buf_renders() {
        let buckets = StatusBuckets {
            running: &[],
            pending: &[],
            completed: &[],
            failed: &[],
            canceled: &[],
            terminal_counts: None,
        };
        let buf = render_compact_buf(&buckets, Some(5), 80, 0, false);
        // ANSI codes wrap the numbers, so check for the text without exact formatting
        assert!(buf.contains("running"));
        assert!(buf.contains("pending"));
        assert!(buf.contains("↻ 5s"));
        // Verify ANSI color codes are present
        assert!(buf.contains("\x1b["), "expected ANSI color codes in output");
    }
}
