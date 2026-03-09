use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Running,
    Completed,
    Failed,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
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
            _ => Err(anyhow::anyhow!("unknown status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    }

    #[test]
    fn status_from_str_roundtrip() {
        for status in [
            Status::Pending,
            Status::Running,
            Status::Completed,
            Status::Failed,
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
}
