use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::json;

use crate::config::read_env_file_key;

pub struct SlackNotifier {
    client: Client,
    bot_token: String,
}

impl SlackNotifier {
    pub fn new() -> Result<Self> {
        let bot_token = std::env::var("SLACK_BOT_TOKEN")
            .or_else(|_| read_env_file_key("SLACK_BOT_TOKEN"))
            .context("SLACK_BOT_TOKEN not set")?;

        Ok(Self {
            client: Client::new(),
            bot_token,
        })
    }

    /// Send message to a Slack channel.
    pub fn send(&self, channel: &str, text: &str) -> Result<()> {
        self.client
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&json!({
                "channel": channel,
                "text": text,
                "unfurl_links": false
            }))
            .send()
            .context("Slack API request failed")?;
        Ok(())
    }
}

/// Try to send a Slack notification (best effort, never fails).
pub fn notify_slack(channel: &str, text: &str) {
    if let Ok(notifier) = SlackNotifier::new() {
        let _ = notifier.send(channel, text);
    }
}

/// Build a human-readable notification label from task metadata.
///
/// Format: `#NNN task_type` or `RIG-XX #NNN task_type` (with Linear issue).
/// The short number is extracted from the task ID suffix (e.g. `20260309-001` → `#001`).
pub fn format_notify_label(task_id: &str, task_type: &str, linear_issue_id: &str) -> String {
    let short_num = task_id
        .rsplit('-')
        .next()
        .map(|n| format!("#{n}"))
        .unwrap_or_else(|| task_id.to_string());

    if linear_issue_id.is_empty() {
        format!("{short_num} {task_type}")
    } else {
        let cfg = crate::config::UserConfig::load();
        let display_id = cfg.tracker.display_identifier(linear_issue_id);
        format!("{display_id} {short_num} {task_type}")
    }
}

/// Returns true if the verdict represents a success outcome (pipeline advances forward).
/// Mirrors `is_forward_verdict` logic from `pipeline::callback::decision`.
pub fn is_success_verdict(verdict: &str) -> bool {
    matches!(
        verdict.to_lowercase().as_str(),
        "done" | "approved" | "passed" | "ok" | "already_done"
    )
}

/// Build an enriched notification message for pipeline tasks.
///
/// Format varies by context:
/// - `(Some(verdict), Some(next))` → `"{label} {VERDICT} → {next}"`
/// - `(Some(verdict), None)` + success → `"{label} {VERDICT} ✓"`
/// - `(Some(verdict), None)` + failure → `"{label} {VERDICT}"`
/// - Non-pipeline (empty stage) → `"{label} done"`
pub fn format_pipeline_notify(
    label: &str,
    pipeline_stage: &str,
    verdict: Option<&str>,
    next_stage: Option<&str>,
) -> String {
    if pipeline_stage.is_empty() {
        return format!("{label} done");
    }

    match (verdict, next_stage) {
        (Some(v), Some(next)) => {
            format!("{label} {} → {next}", v.to_uppercase())
        }
        (Some(v), None) => {
            let upper = v.to_uppercase();
            if is_success_verdict(v) {
                format!("{label} {upper} ✓")
            } else {
                format!("{label} {upper}")
            }
        }
        (None, _) => format!("{label} done"),
    }
}

/// Available display fields for status output and notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayField {
    Runtime,
    Model,
    Cost,
    Turns,
    Verdict,
}

impl DisplayField {
    /// Parse a field name from config string.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "runtime" => Some(Self::Runtime),
            "model" => Some(Self::Model),
            "cost" => Some(Self::Cost),
            "turns" => Some(Self::Turns),
            "verdict" => Some(Self::Verdict),
            _ => None,
        }
    }
}

/// Render a single display field value from a task.
fn render_field(field: DisplayField, task: &crate::models::Task) -> Option<String> {
    match field {
        DisplayField::Runtime => Some(task.runtime.to_string()),
        DisplayField::Model => {
            if task.model.is_empty() {
                None
            } else if task.runtime != crate::models::AgentRuntime::ClaudeCode {
                // RIG-387: show runtime prefix when not claude-code so qwen/gemini/codex
                // tasks are distinguishable from claude tasks in `werma st`.
                // e.g. "(qwen-code/sonnet)" instead of just "(sonnet)"
                Some(format!("{}/{}", task.runtime, task.model))
            } else {
                Some(task.model.clone())
            }
        }
        DisplayField::Cost => task.cost_usd.map(|c| format!("${c:.2}")),
        DisplayField::Turns => {
            if task.turns_used > 0 {
                Some(format!("{}t", task.turns_used))
            } else {
                None
            }
        }
        DisplayField::Verdict => None, // Verdict requires parsing output, handled separately
    }
}

/// Format configurable display fields for a task.
///
/// Returns formatted string like `"  (opus/19t)"` or empty string if no fields have values.
/// Fields are rendered in the order specified, separated by `/`.
pub fn format_display_fields(task: &crate::models::Task, fields: &[DisplayField]) -> String {
    let parts: Vec<String> = fields
        .iter()
        .filter_map(|f| render_field(*f, task))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("  ({})", parts.join("/"))
    }
}

/// Default display fields for `werma st` output.
pub const DEFAULT_STATUS_FIELDS: &[DisplayField] = &[DisplayField::Model, DisplayField::Turns];

/// Default display fields for macOS/Slack notifications.
pub const DEFAULT_NOTIFICATION_FIELDS: &[DisplayField] = &[DisplayField::Model];

/// Parse a list of field name strings into DisplayField values.
/// Unknown field names are silently skipped.
pub fn parse_field_names(names: &[String]) -> Vec<DisplayField> {
    names
        .iter()
        .filter_map(|s| DisplayField::from_str(s))
        .collect()
}

/// Send macOS notification via osascript.
pub fn notify_macos(title: &str, message: &str, sound: &str) {
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!(
                "display notification \"{}\" with title \"{}\" sound name \"{}\"",
                message.replace('"', "\\\""),
                title.replace('"', "\\\""),
                sound
            ),
        ])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_label_without_linear() {
        let label = format_notify_label("20260309-001", "research", "");
        assert_eq!(label, "#001 research");
    }

    #[test]
    fn format_label_with_linear() {
        let label = format_notify_label("20260309-001", "analyst", "RIG-34");
        assert_eq!(label, "RIG-34 #001 analyst");
    }

    #[test]
    fn format_label_different_numbers() {
        let label = format_notify_label("20260310-042", "code", "");
        assert_eq!(label, "#042 code");
    }

    // ─── is_success_verdict tests ───────────────────────────────────────

    #[test]
    fn success_verdicts() {
        assert!(is_success_verdict("done"));
        assert!(is_success_verdict("DONE"));
        assert!(is_success_verdict("approved"));
        assert!(is_success_verdict("APPROVED"));
        assert!(is_success_verdict("passed"));
        assert!(is_success_verdict("ok"));
        assert!(is_success_verdict("already_done"));
    }

    #[test]
    fn failure_verdicts() {
        assert!(!is_success_verdict("rejected"));
        assert!(!is_success_verdict("REJECTED"));
        assert!(!is_success_verdict("failed"));
        assert!(!is_success_verdict("blocked"));
    }

    // ─── format_pipeline_notify tests ───────────────────────────────────

    #[test]
    fn pipeline_notify_with_verdict_and_next_stage() {
        let msg = format_pipeline_notify(
            "RIG-369 #019 engineer",
            "engineer",
            Some("done"),
            Some("review"),
        );
        assert_eq!(msg, "RIG-369 #019 engineer DONE → review");
    }

    #[test]
    fn pipeline_notify_terminal_success() {
        let msg =
            format_pipeline_notify("RIG-369 #019 reviewer", "reviewer", Some("approved"), None);
        assert_eq!(msg, "RIG-369 #019 reviewer APPROVED ✓");
    }

    #[test]
    fn pipeline_notify_terminal_failure_verdict() {
        let msg =
            format_pipeline_notify("RIG-369 #019 reviewer", "reviewer", Some("rejected"), None);
        assert_eq!(msg, "RIG-369 #019 reviewer REJECTED");
    }

    #[test]
    fn pipeline_notify_no_verdict() {
        let msg = format_pipeline_notify("RIG-369 #019 engineer", "engineer", None, None);
        assert_eq!(msg, "RIG-369 #019 engineer done");
    }

    #[test]
    fn pipeline_notify_non_pipeline_task() {
        let msg = format_pipeline_notify("#004 code", "", None, None);
        assert_eq!(msg, "#004 code done");
    }

    // ─── format_display_fields tests ────────────────────────────────────

    #[test]
    fn display_fields_model_and_turns() {
        let task = crate::models::Task {
            model: "opus".into(),
            turns_used: 19,
            ..Default::default()
        };
        let result = format_display_fields(&task, DEFAULT_STATUS_FIELDS);
        assert_eq!(result, "  (opus/19t)");
    }

    #[test]
    fn display_fields_cost_model_turns() {
        let task = crate::models::Task {
            model: "opus".into(),
            turns_used: 19,
            cost_usd: Some(0.66),
            ..Default::default()
        };
        let fields = &[DisplayField::Cost, DisplayField::Model, DisplayField::Turns];
        let result = format_display_fields(&task, fields);
        assert_eq!(result, "  ($0.66/opus/19t)");
    }

    #[test]
    fn display_fields_runtime_only() {
        let task = crate::models::Task {
            runtime: crate::models::AgentRuntime::Codex,
            ..Default::default()
        };
        let fields = &[DisplayField::Runtime];
        let result = format_display_fields(&task, fields);
        assert_eq!(result, "  (codex)");
    }

    #[test]
    fn display_fields_empty_when_no_values() {
        let task = crate::models::Task::default();
        let fields = &[DisplayField::Cost, DisplayField::Turns];
        let result = format_display_fields(&task, fields);
        assert_eq!(result, "");
    }

    #[test]
    fn display_fields_notification_default() {
        let task = crate::models::Task {
            model: "sonnet".into(),
            ..Default::default()
        };
        let result = format_display_fields(&task, DEFAULT_NOTIFICATION_FIELDS);
        assert_eq!(result, "  (sonnet)");
    }

    #[test]
    fn parse_field_names_valid() {
        let names: Vec<String> = vec!["model".into(), "turns".into(), "cost".into()];
        let fields = parse_field_names(&names);
        assert_eq!(
            fields,
            vec![DisplayField::Model, DisplayField::Turns, DisplayField::Cost]
        );
    }

    #[test]
    fn parse_field_names_skips_unknown() {
        let names: Vec<String> = vec!["model".into(), "bogus".into(), "turns".into()];
        let fields = parse_field_names(&names);
        assert_eq!(fields, vec![DisplayField::Model, DisplayField::Turns]);
    }

    #[test]
    fn parse_field_names_empty() {
        let fields = parse_field_names(&[]);
        assert!(fields.is_empty());
    }

    // ─── RIG-387: runtime prefix in Model display ────────────────────────

    /// When runtime is not claude-code, Model display must include the runtime prefix.
    ///
    /// Bug: `render_field(Model)` returned just `task.model` ("sonnet") even for qwen/gemini
    /// tasks, making them indistinguishable from claude-code tasks in `werma st`.
    #[test]
    fn display_fields_model_shows_runtime_prefix_for_non_claude() {
        let task = crate::models::Task {
            model: "sonnet".into(),
            runtime: crate::models::AgentRuntime::QwenCode,
            turns_used: 5,
            ..Default::default()
        };
        let result = format_display_fields(&task, DEFAULT_STATUS_FIELDS);
        assert_eq!(
            result, "  (qwen-code/sonnet/5t)",
            "non-claude runtime must be prefixed to model in display"
        );
    }

    /// Claude-code runtime must NOT add a prefix — keep backward compatibility.
    #[test]
    fn display_fields_model_no_prefix_for_claude_code() {
        let task = crate::models::Task {
            model: "opus".into(),
            runtime: crate::models::AgentRuntime::ClaudeCode,
            turns_used: 10,
            ..Default::default()
        };
        let result = format_display_fields(&task, DEFAULT_STATUS_FIELDS);
        assert_eq!(
            result, "  (opus/10t)",
            "claude-code runtime must not add a prefix"
        );
    }

    /// Codex runtime also shows prefix.
    #[test]
    fn display_fields_model_shows_codex_prefix() {
        let task = crate::models::Task {
            model: "o4-mini".into(),
            runtime: crate::models::AgentRuntime::Codex,
            ..Default::default()
        };
        let fields = &[DisplayField::Model];
        let result = format_display_fields(&task, fields);
        assert_eq!(result, "  (codex/o4-mini)");
    }
}
