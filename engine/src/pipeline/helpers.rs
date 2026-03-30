use std::path::PathBuf;

use crate::db::Db;

/// Resolve `~/` prefix to the user's home directory.
pub(crate) fn resolve_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Truncate text to a maximum number of lines.
pub(crate) fn truncate_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().take(max).collect();
    let result = lines.join("\n");
    if text.lines().count() > max {
        format!(
            "{result}\n\n[... truncated, {max} of {} lines shown]",
            text.lines().count()
        )
    } else {
        result
    }
}

/// Infer working directory from existing tasks for the same Linear issue.
pub(crate) fn infer_working_dir_from_issue(db: &Db, linear_issue_id: &str) -> String {
    if let Ok(tasks) = db.tasks_by_linear_issue(linear_issue_id, None, false) {
        for task in &tasks {
            if !task.working_dir.is_empty() && task.working_dir != "~/projects/rigpa/werma" {
                return task.working_dir.clone();
            }
        }
        if let Some(task) = tasks.first() {
            return task.working_dir.clone();
        }
    }
    "~/projects/rigpa/werma".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Status, Task};

    #[test]
    fn resolve_home_expands_tilde() {
        let result = resolve_home("~/test/path");
        assert!(!result.to_string_lossy().starts_with("~/"));
        assert!(result.to_string_lossy().ends_with("/test/path"));
    }

    #[test]
    fn resolve_home_absolute_path_unchanged() {
        let result = resolve_home("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn truncate_lines_short() {
        let text = "line 1\nline 2\nline 3";
        assert_eq!(truncate_lines(text, 10), text);
    }

    #[test]
    fn truncate_lines_long() {
        let text: String = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_lines(&text, 5);
        assert!(result.contains("line 0"));
        assert!(result.contains("line 4"));
        assert!(!result.contains("line 5"));
        assert!(result.contains("[... truncated, 5 of 20 lines shown]"));
    }

    #[test]
    fn truncate_lines_empty() {
        assert_eq!(truncate_lines("", 10), "");
    }

    #[test]
    fn truncate_lines_exact_limit() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(truncate_lines(text, 5), text);
    }

    #[test]
    fn infer_working_dir_from_existing_tasks() {
        let db = crate::db::Db::open_in_memory().unwrap();

        let task = Task {
            id: "20260310-010".to_string(),
            status: Status::Completed,
            priority: 1,
            created_at: "2026-03-10T10:00:00".to_string(),
            started_at: None,
            finished_at: None,
            task_type: "pipeline-analyst".to_string(),
            prompt: "test".to_string(),
            output_path: String::new(),
            working_dir: "~/projects/rigpa/werma".to_string(),
            model: "opus".to_string(),
            max_turns: 20,
            allowed_tools: String::new(),
            session_id: String::new(),
            linear_issue_id: "issue-xyz".to_string(),
            linear_pushed: false,
            pipeline_stage: "analyst".to_string(),
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
        };
        db.insert_task(&task).unwrap();

        let dir = infer_working_dir_from_issue(&db, "issue-xyz");
        assert_eq!(dir, "~/projects/rigpa/werma");

        let dir = infer_working_dir_from_issue(&db, "unknown-issue");
        assert_eq!(dir, "~/projects/rigpa/werma");
    }
}
