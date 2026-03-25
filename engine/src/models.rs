use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl Status {
    /// Returns true if the task is in a terminal state (no further transitions).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Canceled => write!(f, "canceled"),
        }
    }
}

impl FromStr for Status {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            _ => Err(anyhow::anyhow!("unknown status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Task {
    pub id: String,
    pub status: Status,
    pub priority: i32,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(rename = "type")]
    pub task_type: String,
    pub prompt: String,
    pub output_path: String,
    pub working_dir: String,
    pub model: String,
    pub max_turns: i32,
    pub allowed_tools: String,
    pub session_id: String,
    pub linear_issue_id: String,
    pub linear_pushed: bool,
    pub pipeline_stage: String,
    pub depends_on: Vec<String>,
    pub context_files: Vec<String>,
    #[serde(default)]
    pub repo_hash: String,
    #[serde(default)]
    pub estimate: i32,
    /// How many times this task has been auto-retried after failure.
    #[serde(default)]
    pub retry_count: i32,
    /// ISO timestamp: task won't be re-launched until this time passes.
    #[serde(default)]
    pub retry_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub cron_expr: String,
    pub prompt: String,
    #[serde(rename = "type")]
    pub schedule_type: String,
    pub model: String,
    pub output_path: String,
    pub working_dir: String,
    pub max_turns: i32,
    pub enabled: bool,
    pub context_files: Vec<String>,
    pub last_enqueued: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyUsage {
    pub date: String,
    pub opus_calls: i32,
    pub sonnet_calls: i32,
    pub haiku_calls: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_display() {
        assert_eq!(Status::Pending.to_string(), "pending");
        assert_eq!(Status::Running.to_string(), "running");
        assert_eq!(Status::Completed.to_string(), "completed");
        assert_eq!(Status::Failed.to_string(), "failed");
        assert_eq!(Status::Canceled.to_string(), "canceled");
    }

    #[test]
    fn status_is_terminal() {
        assert!(!Status::Pending.is_terminal());
        assert!(!Status::Running.is_terminal());
        assert!(Status::Completed.is_terminal());
        assert!(Status::Failed.is_terminal());
        assert!(Status::Canceled.is_terminal());
    }

    #[test]
    fn status_from_str_roundtrip() {
        for status in [
            Status::Pending,
            Status::Running,
            Status::Completed,
            Status::Failed,
            Status::Canceled,
        ] {
            let s = status.to_string();
            let parsed: Status = s.parse().unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn status_from_str_invalid() {
        let result: Result<Status, _> = "bogus".parse();
        assert!(result.is_err());
    }

    #[test]
    fn status_serde_roundtrip() {
        let status = Status::Completed;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"completed\"");
        let parsed: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn task_default_values() {
        let task = Task::default();
        assert_eq!(task.status, Status::Pending);
        assert!(task.id.is_empty());
        assert!(task.depends_on.is_empty());
        assert!(task.context_files.is_empty());
        assert_eq!(task.estimate, 0);
        assert!(!task.linear_pushed);
    }

    #[test]
    fn task_serde_roundtrip() {
        let task = Task {
            id: "20260313-001".to_string(),
            status: Status::Running,
            priority: 1,
            task_type: "code".to_string(),
            prompt: "do stuff".to_string(),
            depends_on: vec!["dep-1".to_string()],
            context_files: vec!["ctx.md".to_string()],
            estimate: 5,
            ..Default::default()
        };

        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "20260313-001");
        assert_eq!(parsed.status, Status::Running);
        assert_eq!(parsed.depends_on, vec!["dep-1"]);
        assert_eq!(parsed.estimate, 5);
    }

    #[test]
    fn status_default_is_pending() {
        assert_eq!(Status::default(), Status::Pending);
    }
}
