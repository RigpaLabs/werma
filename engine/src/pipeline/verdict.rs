/// Extract verdict from result text.
/// Looks for patterns like VERDICT=APPROVED, REVIEW_VERDICT=APPROVED, etc.
/// Returns None if no verdict found (critical fix from bash version).
pub fn parse_verdict(result: &str) -> Option<String> {
    // Look for explicit verdict patterns (last match wins)
    let patterns = [
        "VERDICT=",
        "REVIEW_VERDICT=",
        "QA_VERDICT=",
        "DEPLOY_VERDICT=",
        "FIX_VERDICT=",
    ];

    let mut found: Option<String> = None;

    for line in result.lines() {
        let line = line.trim();
        for pattern in &patterns {
            if let Some(rest) = line.strip_prefix(pattern).or_else(|| {
                // Also check within the line
                line.find(pattern).map(|pos| &line[pos + pattern.len()..])
            }) {
                let verdict = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                if !verdict.is_empty() {
                    found = Some(verdict.to_uppercase());
                }
            }
        }
    }

    // Also check for standalone APPROVED/REJECTED keywords in the last 10 lines
    if found.is_none() {
        let last_lines: Vec<&str> = result.lines().rev().take(10).collect();
        for line in &last_lines {
            let upper = line.trim().to_uppercase();
            if upper.contains("APPROVED") && !upper.contains("NOT APPROVED") {
                return Some("APPROVED".to_string());
            }
            if upper.contains("REJECTED") || upper.contains("REQUEST_CHANGES") {
                return Some("REJECTED".to_string());
            }
            if upper.contains("PASSED") && !upper.contains("NOT PASSED") {
                return Some("PASSED".to_string());
            }
            if upper.contains("FAILED") {
                return Some("FAILED".to_string());
            }
        }
    }

    found
}

/// Extract rejection/failure feedback from reviewer or QA output.
pub fn extract_rejection_feedback(output: &str) -> String {
    let mut feedback_lines = Vec::new();
    let mut in_findings = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("blocker")
            || trimmed.contains("Blocker")
            || trimmed.contains("REJECTED")
            || trimmed.contains("FAILED")
            || trimmed.contains("must change")
            || trimmed.contains("Must fix")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("1.")
            || trimmed.starts_with("##")
        {
            feedback_lines.push(line.to_string());
            in_findings = true;
        } else if in_findings && !trimmed.is_empty() {
            feedback_lines.push(line.to_string());
        } else {
            in_findings = false;
        }
    }

    if feedback_lines.is_empty() {
        output
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        feedback_lines.join("\n")
    }
}

/// Extract `PR_URL=<url>` from result text.
/// Falls back to scanning for `https://github.com/.../pull/N` patterns.
pub fn parse_pr_url(result: &str) -> Option<String> {
    // First: look for explicit PR_URL= marker (scan backwards — agents typically
    // emit PR_URL= near the end of output, so reverse scan finds it faster)
    for line in result.lines().rev() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("PR_URL=").or_else(|| {
            line.find("PR_URL=")
                .map(|pos| &line[pos + "PR_URL=".len()..])
        }) {
            let url: String = rest
                .trim()
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect();
            if url.contains("/pull/") {
                return Some(url);
            }
        }
    }

    // Fallback: scan forward for raw GitHub PR URLs (first occurrence wins —
    // unlike PR_URL= marker above which scans in reverse for last-wins semantics)
    const PREFIX: &str = "https://github.com/";
    for line in result.lines() {
        if let Some(start) = line.find(PREFIX) {
            let candidate = &line[start..];
            let url: String = candidate
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ')' && *c != '>' && *c != ']')
                .collect();
            if url.contains("/pull/") {
                return Some(url);
            }
        }
    }

    None
}

/// Story point thresholds for track routing.
const HEAVY_TRACK_THRESHOLD: i32 = 8;

/// Returns true if the task estimate qualifies for heavy track processing.
pub fn is_heavy_track(estimate: i32) -> bool {
    estimate >= HEAVY_TRACK_THRESHOLD
}

/// Extract ESTIMATE=X from result text. Returns 0 if not found.
pub fn parse_estimate(result: &str) -> i32 {
    for line in result.lines().rev() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ESTIMATE=").or_else(|| {
            line.find("ESTIMATE=")
                .map(|pos| &line[pos + "ESTIMATE=".len()..])
        }) {
            let value_str = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_ascii_digit());
            if let Ok(v) = value_str.parse::<i32>() {
                return v;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_parsing_explicit() {
        assert_eq!(
            parse_verdict("REVIEW_VERDICT=APPROVED"),
            Some("APPROVED".to_string())
        );
        assert_eq!(
            parse_verdict("VERDICT=REJECTED"),
            Some("REJECTED".to_string())
        );
        assert_eq!(
            parse_verdict("QA_VERDICT=PASSED"),
            Some("PASSED".to_string())
        );
        assert_eq!(
            parse_verdict("QA_VERDICT=FAILED"),
            Some("FAILED".to_string())
        );
        assert_eq!(parse_verdict("DEPLOY_VERDICT=OK"), Some("OK".to_string()));
        assert_eq!(
            parse_verdict("FIX_VERDICT=FIXED"),
            Some("FIXED".to_string())
        );
    }

    #[test]
    fn verdict_parsing_within_text() {
        let text = "Some output here\nAll checks passed\nREVIEW_VERDICT=APPROVED\nDone.";
        assert_eq!(parse_verdict(text), Some("APPROVED".to_string()));
    }

    #[test]
    fn verdict_parsing_keyword_fallback() {
        let text = "Everything looks good.\nAPPROVED";
        assert_eq!(parse_verdict(text), Some("APPROVED".to_string()));

        let text2 = "Found issues.\nREJECTED";
        assert_eq!(parse_verdict(text2), Some("REJECTED".to_string()));

        let text3 = "All tests pass.\nPASSED";
        assert_eq!(parse_verdict(text3), Some("PASSED".to_string()));
    }

    #[test]
    fn verdict_parsing_empty_no_verdict() {
        // CRITICAL: empty/no verdict should return None, NOT auto-approve
        assert_eq!(parse_verdict(""), None);
        assert_eq!(
            parse_verdict("Some random output without any verdict keywords"),
            None
        );
        assert_eq!(
            parse_verdict("Task completed successfully.\nAll done."),
            None
        );
    }

    #[test]
    fn verdict_parsing_not_approved() {
        // "NOT APPROVED" should not match as APPROVED
        assert_eq!(
            parse_verdict("The changes are NOT APPROVED due to issues."),
            None
        );
    }

    #[test]
    fn verdict_last_match_wins() {
        let text = "VERDICT=FAILED\nAfter fixes:\nVERDICT=APPROVED";
        assert_eq!(parse_verdict(text), Some("APPROVED".to_string()));
    }

    #[test]
    fn extract_feedback_structured() {
        let output = "Looking good overall.\n## Issues\n- blocker: SQL injection in query builder\n- nit: unused import\nREVIEW_VERDICT=REJECTED";
        let feedback = extract_rejection_feedback(output);
        assert!(feedback.contains("blocker"));
        assert!(feedback.contains("SQL injection"));
        assert!(feedback.contains("REJECTED"));
    }

    #[test]
    fn extract_feedback_fallback_last_lines() {
        let output =
            "Everything is fine, no structured markers.\nJust plain text.\nNothing special.";
        let feedback = extract_rejection_feedback(output);
        // Should fall back to last 20 lines
        assert!(feedback.contains("Everything is fine"));
    }

    #[test]
    fn parse_estimate_from_result() {
        assert_eq!(parse_estimate("ESTIMATE=5"), 5);
        assert_eq!(parse_estimate("Some output\nESTIMATE=8\nDone."), 8);
        assert_eq!(parse_estimate("prefix ESTIMATE=13 suffix"), 13);
        assert_eq!(parse_estimate("No estimate here"), 0);
        assert_eq!(parse_estimate(""), 0);
    }

    #[test]
    fn parse_estimate_last_match_wins() {
        assert_eq!(parse_estimate("ESTIMATE=3\nESTIMATE=8"), 8);
    }

    #[test]
    fn parse_pr_url_explicit_marker() {
        let output = "All done.\nPR_URL=https://github.com/RigpaLabs/werma/pull/42\nVERDICT=DONE";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/RigpaLabs/werma/pull/42".to_string())
        );
    }

    #[test]
    fn parse_pr_url_inline_marker() {
        let output = "Created PR_URL=https://github.com/org/repo/pull/7 successfully";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/org/repo/pull/7".to_string())
        );
    }

    #[test]
    fn parse_pr_url_fallback_to_raw_url() {
        let output = "PR: https://github.com/org/repo/pull/99\nVERDICT=DONE";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/org/repo/pull/99".to_string())
        );
    }

    #[test]
    fn parse_pr_url_none_without_pull() {
        assert_eq!(parse_pr_url("No PR here"), None);
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/org/repo/issues/10"),
            None
        );
    }

    #[test]
    fn is_heavy_track_routing() {
        assert!(!is_heavy_track(0));
        assert!(!is_heavy_track(5));
        assert!(!is_heavy_track(7));
        assert!(is_heavy_track(8));
        assert!(is_heavy_track(13));
    }
}
