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
        format!("{linear_issue_id} {short_num} {task_type}")
    }
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
