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
