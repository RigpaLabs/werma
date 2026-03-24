use chrono::Local;
use colored::Colorize;

use crate::models::{Status, Task};

/// Status icon for display.
pub fn status_icon(status: Status) -> &'static str {
    match status {
        Status::Pending => "○",
        Status::Running => "◉",
        Status::Completed => "✓",
        Status::Failed => "✗",
        Status::Canceled => "⊘",
    }
}

/// Truncate string to max chars, append "..." if truncated.
pub fn truncate(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() <= max {
        first_line.to_string()
    } else {
        let mut result: String = first_line.chars().take(max).collect();
        result.push_str("...");
        result
    }
}

/// Current working directory as a String (fallback for `--dir`).
pub fn default_working_dir() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

/// Expand ~ to home directory.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().to_string();
    }
    path.to_string()
}

/// Default max_turns based on task type.
pub fn default_turns(task_type: &str) -> i32 {
    match task_type {
        "code" | "refactor" | "full" | "pipeline-engineer" => 30,
        "research" | "pipeline-analyst" => 20,
        "review" | "analyze" => 10,
        "pipeline-deployer" => 25,
        "pipeline-reviewer" | "pipeline-qa" | "pipeline-devops" => 15,
        _ => 15,
    }
}

pub fn parse_timestamp(s: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .ok()
}

pub fn format_duration_between(start: &str, end: &str) -> String {
    let (Some(s), Some(e)) = (parse_timestamp(start), parse_timestamp(end)) else {
        return String::new();
    };
    format_duration_secs((e - s).num_seconds().max(0))
}

pub fn format_elapsed_since(start: &str) -> String {
    let Some(s) = parse_timestamp(start) else {
        return String::new();
    };
    format_duration_secs((Local::now().naive_local() - s).num_seconds().max(0))
}

pub fn format_duration_secs(secs: i64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        "<1m".to_string()
    }
}

pub fn format_task_line(task: &Task, time_str: &str) -> String {
    let linear = if task.linear_issue_id.is_empty() {
        String::new()
    } else {
        format!("  [{}]", task.linear_issue_id.cyan())
    };
    let preview = truncate(&task.prompt, 45);
    format!(
        "   {}  {}{}  {}  {}",
        task.id,
        task.task_type.blue(),
        linear,
        time_str.dimmed(),
        preview,
    )
}

pub fn compact_task_type(task_type: &str) -> &str {
    match task_type {
        "pipeline-engineer" => "engineer",
        "pipeline-analyst" => "analyst",
        "pipeline-reviewer" => "reviewer",
        "pipeline-devops" => "devops",
        other => other,
    }
}

pub fn compact_task_id(id: &str) -> &str {
    id.rsplit('-').next().unwrap_or(id)
}

pub fn compact_linear_label(linear_issue_id: &str) -> String {
    if linear_issue_id.is_empty() {
        String::new()
    } else {
        format!(" [{linear_issue_id}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_icon_mapping() {
        assert_eq!(status_icon(Status::Pending), "○");
        assert_eq!(status_icon(Status::Running), "◉");
        assert_eq!(status_icon(Status::Completed), "✓");
        assert_eq!(status_icon(Status::Failed), "✗");
        assert_eq!(status_icon(Status::Canceled), "⊘");
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate("this is a very long string that should be truncated", 10);
        assert!(result.ends_with("..."));
        // 10 chars + "..."
        assert_eq!(result.len(), 13);
    }

    #[test]
    fn truncate_multiline_uses_first_line() {
        assert_eq!(truncate("line1\nline2\nline3", 50), "line1");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("12345", 5), "12345");
    }

    #[test]
    fn expand_tilde_with_home() {
        let result = expand_tilde("~/test/path");
        assert!(!result.starts_with("~/"));
        assert!(result.ends_with("test/path"));
    }

    #[test]
    fn expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn default_turns_by_type() {
        assert_eq!(default_turns("code"), 30);
        assert_eq!(default_turns("refactor"), 30);
        assert_eq!(default_turns("full"), 30);
        assert_eq!(default_turns("pipeline-engineer"), 30);
        assert_eq!(default_turns("research"), 20);
        assert_eq!(default_turns("pipeline-analyst"), 20);
        assert_eq!(default_turns("review"), 10);
        assert_eq!(default_turns("analyze"), 10);
        assert_eq!(default_turns("pipeline-deployer"), 25);
        assert_eq!(default_turns("pipeline-reviewer"), 15);
        assert_eq!(default_turns("pipeline-qa"), 15);
        assert_eq!(default_turns("pipeline-devops"), 15);
        assert_eq!(default_turns("unknown_type"), 15);
    }

    #[test]
    fn format_duration_secs_hours() {
        assert_eq!(format_duration_secs(3661), "1h 1m");
        assert_eq!(format_duration_secs(7200), "2h 0m");
    }

    #[test]
    fn format_duration_secs_minutes() {
        assert_eq!(format_duration_secs(120), "2m");
        assert_eq!(format_duration_secs(60), "1m");
    }

    #[test]
    fn format_duration_secs_under_minute() {
        assert_eq!(format_duration_secs(30), "<1m");
        assert_eq!(format_duration_secs(0), "<1m");
    }

    #[test]
    fn parse_timestamp_iso() {
        let ts = parse_timestamp("2026-03-13T10:30:00");
        assert!(ts.is_some());
        let dt = ts.unwrap();
        assert_eq!(dt.time().format("%H:%M").to_string(), "10:30");
    }

    #[test]
    fn parse_timestamp_space_format() {
        let ts = parse_timestamp("2026-03-13 10:30:00");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_invalid() {
        assert!(parse_timestamp("not-a-date").is_none());
        assert!(parse_timestamp("").is_none());
    }

    #[test]
    fn format_duration_between_valid() {
        let dur = format_duration_between("2026-03-13T10:00:00", "2026-03-13T11:30:00");
        assert_eq!(dur, "1h 30m");
    }

    #[test]
    fn format_duration_between_invalid() {
        assert_eq!(format_duration_between("bad", "bad"), "");
    }

    #[test]
    fn compact_task_type_maps() {
        assert_eq!(compact_task_type("pipeline-engineer"), "engineer");
        assert_eq!(compact_task_type("pipeline-analyst"), "analyst");
        assert_eq!(compact_task_type("pipeline-reviewer"), "reviewer");
        assert_eq!(compact_task_type("pipeline-devops"), "devops");
        assert_eq!(compact_task_type("code"), "code");
        assert_eq!(compact_task_type("research"), "research");
    }

    #[test]
    fn compact_task_id_extracts_suffix() {
        assert_eq!(compact_task_id("20260312-035"), "035");
        assert_eq!(compact_task_id("simple"), "simple");
    }

    #[test]
    fn compact_linear_label_empty() {
        assert_eq!(compact_linear_label(""), "");
    }

    #[test]
    fn compact_linear_label_present() {
        assert_eq!(compact_linear_label("RIG-42"), " [RIG-42]");
    }
}
