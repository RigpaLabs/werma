use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Agent execution runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentRuntime {
    #[default]
    ClaudeCode,
    Codex,
    #[serde(alias = "gemini")]
    GeminiCli,
    #[serde(alias = "qwen")]
    QwenCode,
}

impl AgentRuntime {
    /// All known runtime variants.
    pub const ALL: &[AgentRuntime] = &[
        Self::ClaudeCode,
        Self::Codex,
        Self::GeminiCli,
        Self::QwenCode,
    ];

    /// Whether this runtime is trusted for unsupervised execution.
    /// Trusted runtimes can run on any repo without an explicit allowlist entry
    /// (used by `UserConfig::is_runtime_allowed` as the default allowlist).
    /// Untrusted runtimes require explicit `repo_runtimes` configuration.
    pub fn is_trusted(self) -> bool {
        matches!(self, Self::ClaudeCode | Self::Codex)
    }
}

impl fmt::Display for AgentRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClaudeCode => write!(f, "claude-code"),
            Self::Codex => write!(f, "codex"),
            Self::GeminiCli => write!(f, "gemini-cli"),
            Self::QwenCode => write!(f, "qwen-code"),
        }
    }
}

impl FromStr for AgentRuntime {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude-code" | "claude" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "gemini-cli" | "gemini" => Ok(Self::GeminiCli),
            "qwen-code" | "qwen" => Ok(Self::QwenCode),
            _ => Err(anyhow::anyhow!(
                "unknown runtime: {s} (expected claude-code, codex, gemini-cli, or qwen-code)"
            )),
        }
    }
}

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
    pub issue_identifier: String,
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
    /// Total cost in USD for this task (parsed from Claude output JSON).
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Number of turns actually used by the agent.
    #[serde(default)]
    pub turns_used: i32,
    /// Handoff content stored in DB instead of filesystem (pipeline tasks).
    #[serde(default)]
    pub handoff_content: String,
    /// Agent execution runtime (claude-code or codex). Default: claude-code.
    #[serde(default)]
    pub runtime: AgentRuntime,
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

/// An outbox effect: an external side-effect to execute asynchronously.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub id: i64,
    pub dedup_key: String,
    pub task_id: String,
    pub issue_id: String,
    pub effect_type: EffectType,
    pub payload: serde_json::Value,
    pub blocking: bool,
    pub status: EffectStatus,
    pub attempts: i32,
    pub max_attempts: i32,
    pub created_at: String,
    pub next_retry_at: Option<String>,
    pub executed_at: Option<String>,
    pub error: Option<String>,
}

/// The type of external side effect to execute.
///
/// SpawnTask is NOT an EffectType — spawning a child task is an internal
/// DB change done synchronously inside the callback transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectType {
    MoveIssue,
    PostComment,
    AddLabel,
    RemoveLabel,
    UpdateEstimate,
    CreatePr,
    AttachUrl,
    PostPrComment,
    Notify,
}

impl fmt::Display for EffectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MoveIssue => write!(f, "MoveIssue"),
            Self::PostComment => write!(f, "PostComment"),
            Self::AddLabel => write!(f, "AddLabel"),
            Self::RemoveLabel => write!(f, "RemoveLabel"),
            Self::UpdateEstimate => write!(f, "UpdateEstimate"),
            Self::CreatePr => write!(f, "CreatePr"),
            Self::AttachUrl => write!(f, "AttachUrl"),
            Self::PostPrComment => write!(f, "PostPrComment"),
            Self::Notify => write!(f, "Notify"),
        }
    }
}

impl FromStr for EffectType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "MoveIssue" => Ok(Self::MoveIssue),
            "PostComment" => Ok(Self::PostComment),
            "AddLabel" => Ok(Self::AddLabel),
            "RemoveLabel" => Ok(Self::RemoveLabel),
            "UpdateEstimate" => Ok(Self::UpdateEstimate),
            "CreatePr" => Ok(Self::CreatePr),
            "AttachUrl" => Ok(Self::AttachUrl),
            "PostPrComment" => Ok(Self::PostPrComment),
            "Notify" => Ok(Self::Notify),
            _ => Err(anyhow::anyhow!("unknown effect type: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectStatus {
    Pending,
    Running,
    Done,
    Failed,
    Dead,
}

impl fmt::Display for EffectStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Done => write!(f, "done"),
            Self::Failed => write!(f, "failed"),
            Self::Dead => write!(f, "dead"),
        }
    }
}

impl FromStr for EffectStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "dead" => Ok(Self::Dead),
            _ => Err(anyhow::anyhow!("unknown effect status: {s}")),
        }
    }
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
    fn runtime_display() {
        assert_eq!(AgentRuntime::ClaudeCode.to_string(), "claude-code");
        assert_eq!(AgentRuntime::Codex.to_string(), "codex");
        assert_eq!(AgentRuntime::GeminiCli.to_string(), "gemini-cli");
        assert_eq!(AgentRuntime::QwenCode.to_string(), "qwen-code");
    }

    #[test]
    fn runtime_from_str_roundtrip() {
        for rt in [
            AgentRuntime::ClaudeCode,
            AgentRuntime::Codex,
            AgentRuntime::GeminiCli,
            AgentRuntime::QwenCode,
        ] {
            let s = rt.to_string();
            let parsed: AgentRuntime = s.parse().unwrap();
            assert_eq!(parsed, rt);
        }
        // aliases
        let parsed: AgentRuntime = "claude".parse().unwrap();
        assert_eq!(parsed, AgentRuntime::ClaudeCode);
        let parsed: AgentRuntime = "gemini".parse().unwrap();
        assert_eq!(parsed, AgentRuntime::GeminiCli);
        let parsed: AgentRuntime = "qwen".parse().unwrap();
        assert_eq!(parsed, AgentRuntime::QwenCode);
    }

    #[test]
    fn runtime_from_str_invalid() {
        let result: Result<AgentRuntime, _> = "unknown-runtime".parse();
        assert!(result.is_err());
    }

    #[test]
    fn runtime_serde_roundtrip() {
        let rt = AgentRuntime::Codex;
        let json = serde_json::to_string(&rt).unwrap();
        assert_eq!(json, "\"codex\"");
        let parsed: AgentRuntime = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rt);

        let rt2 = AgentRuntime::ClaudeCode;
        let json2 = serde_json::to_string(&rt2).unwrap();
        assert_eq!(json2, "\"claude-code\"");

        let rt3 = AgentRuntime::GeminiCli;
        let json3 = serde_json::to_string(&rt3).unwrap();
        assert_eq!(json3, "\"gemini-cli\"");

        let rt4 = AgentRuntime::QwenCode;
        let json4 = serde_json::to_string(&rt4).unwrap();
        assert_eq!(json4, "\"qwen-code\"");
    }

    #[test]
    fn runtime_serde_alias_deserialize() {
        // "gemini" alias deserializes to GeminiCli
        let parsed: AgentRuntime = serde_json::from_str("\"gemini\"").unwrap();
        assert_eq!(parsed, AgentRuntime::GeminiCli);
        // "qwen" alias deserializes to QwenCode
        let parsed: AgentRuntime = serde_json::from_str("\"qwen\"").unwrap();
        assert_eq!(parsed, AgentRuntime::QwenCode);
    }

    #[test]
    fn runtime_default_is_claude_code() {
        assert_eq!(AgentRuntime::default(), AgentRuntime::ClaudeCode);
    }

    #[test]
    fn runtime_is_trusted() {
        assert!(AgentRuntime::ClaudeCode.is_trusted());
        assert!(AgentRuntime::Codex.is_trusted());
        assert!(!AgentRuntime::GeminiCli.is_trusted());
        assert!(!AgentRuntime::QwenCode.is_trusted());
    }

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
        assert!(task.cost_usd.is_none());
        assert_eq!(task.turns_used, 0);
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
