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

pub(crate) fn extract_spec_from_output(output: &str) -> String {
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
