use crate::models::{Effect, EffectStatus, EffectType};

/// Returns true if an effect of the given type is blocking (failure halts the chain).
///
/// Blocking: MoveIssue, CreatePr, UpdateEstimate — state-mutating, must succeed.
/// Non-blocking (best-effort): PostComment, AddLabel, RemoveLabel, Notify, PostPrComment, AttachUrl.
/// AttachUrl is metadata decoration — a transient Linear API failure shouldn't wedge the pipeline.
pub(super) fn is_blocking_effect(effect_type: EffectType) -> bool {
    matches!(
        effect_type,
        EffectType::MoveIssue | EffectType::CreatePr | EffectType::UpdateEstimate
    )
}

/// Helper: build a `Vec<Effect>` entry with deterministic dedup_key.
/// The `blocking` flag is set automatically based on EffectType.
pub(super) fn make_effect(
    task_id: &str,
    issue_id: &str,
    effect_type: EffectType,
    key_suffix: &str,
    payload: serde_json::Value,
) -> Effect {
    Effect {
        id: 0,
        dedup_key: format!("{task_id}:{key_suffix}"),
        task_id: task_id.to_string(),
        issue_id: issue_id.to_string(),
        effect_type,
        payload,
        blocking: is_blocking_effect(effect_type),
        status: EffectStatus::Pending,
        attempts: 0,
        max_attempts: 5,
        created_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        next_retry_at: None,
        executed_at: None,
        error: None,
    }
}

/// Build a comment string for a pipeline callback.
pub(crate) fn format_callback_comment(
    task_id: &str,
    stage: &str,
    verdict: &str,
    spawn: Option<&str>,
    pr_url: Option<&str>,
) -> String {
    let stage_label = stage
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + &stage[1..])
        .unwrap_or_else(|| stage.to_string());

    let spawn_note = spawn
        .map(|s| format!(" Spawning **{s}** stage."))
        .unwrap_or_default();

    let pr_note = pr_url.map(|url| format!(" PR: {url}")).unwrap_or_default();

    match verdict.to_lowercase().as_str() {
        "approved" | "passed" | "done" | "ok" | "fixed" => {
            format!(
                "**{stage_label} {verdict_upper}** (task: `{task_id}`).{pr_note}{spawn_note}",
                verdict_upper = verdict.to_uppercase()
            )
        }
        "rejected" | "failed" | "request_changes" => {
            format!(
                "**{stage_label}: {verdict_upper}** (task: `{task_id}`). Moving back.{pr_note}{spawn_note}",
                verdict_upper = verdict.to_uppercase()
            )
        }
        _ => {
            format!(
                "**{stage_label}** completed (task: `{task_id}`), verdict: {verdict}.{pr_note}{spawn_note}"
            )
        }
    }
}

pub(super) fn extract_spec_from_output(output: &str) -> String {
    let mut lines = Vec::new();
    let mut in_comment_block = false;
    let mut unclosed_block_start = 0;

    for (idx, line) in output.lines().enumerate() {
        let trimmed = line.trim();

        // Skip ---COMMENT---/---END COMMENT--- blocks (already posted)
        if trimmed == "---COMMENT---" {
            in_comment_block = true;
            unclosed_block_start = idx;
            continue;
        }
        if trimmed == "---END COMMENT---" {
            in_comment_block = false;
            continue;
        }
        if in_comment_block {
            continue;
        }

        // Skip verdict and estimate metadata lines
        let upper = trimmed.to_uppercase();
        if upper.starts_with("VERDICT=") || upper.starts_with("ESTIMATE=") {
            continue;
        }

        lines.push(line);
    }

    // If a ---COMMENT--- was never closed, treat it as plain text.
    // This handles non-conforming agent output where markers are malformed.
    if in_comment_block {
        lines.clear();
        for (idx, line) in output.lines().enumerate() {
            if idx < unclosed_block_start {
                let trimmed = line.trim();
                let upper = trimmed.to_uppercase();
                if upper.starts_with("VERDICT=") || upper.starts_with("ESTIMATE=") {
                    continue;
                }
                lines.push(line);
            } else {
                // Include unclosed block content as-is (skip the marker line itself)
                if idx == unclosed_block_start {
                    continue;
                }
                let trimmed = line.trim();
                let upper = trimmed.to_uppercase();
                if upper.starts_with("VERDICT=") || upper.starts_with("ESTIMATE=") {
                    continue;
                }
                lines.push(line);
            }
        }
    }

    // Trim leading/trailing blank lines
    let result = lines.join("\n");
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_callback_comment_approved() {
        let comment = format_callback_comment("task-123", "reviewer", "approved", None, None);
        assert!(comment.contains("APPROVED"));
        assert!(comment.contains("task-123"));
    }

    #[test]
    fn format_callback_comment_rejected_with_spawn() {
        let comment =
            format_callback_comment("task-456", "reviewer", "rejected", Some("engineer"), None);
        assert!(comment.contains("REJECTED"));
        assert!(comment.contains("engineer"));
    }

    #[test]
    fn format_callback_comment_with_pr_url() {
        let comment = format_callback_comment(
            "task-789",
            "engineer",
            "done",
            None,
            Some("https://github.com/org/repo/pull/42"),
        );
        assert!(comment.contains("DONE"));
        assert!(comment.contains("https://github.com/org/repo/pull/42"));
    }

    #[test]
    fn format_callback_comment_done_no_spawn() {
        let comment = format_callback_comment("task-001", "engineer", "done", None, None);
        assert!(comment.contains("DONE"));
        assert!(comment.contains("task-001"));
        assert!(comment.contains("Engineer"));
    }

    #[test]
    fn format_callback_comment_with_spawn_and_pr() {
        let comment = format_callback_comment(
            "task-002",
            "engineer",
            "done",
            Some("reviewer"),
            Some("https://github.com/org/repo/pull/5"),
        );
        assert!(comment.contains("reviewer"));
        assert!(comment.contains("pull/5"));
    }
}
