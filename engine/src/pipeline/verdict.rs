/// Detect whether agent output indicates a max_turns exit.
///
/// Claude Code returns `{"subtype":"error_max_turns","is_error":false}` when the agent
/// exhausts its turn budget. The runner script should catch this and call `werma fail`,
/// but this function provides a defense-in-depth check for the callback path.
///
/// Checks for:
/// - Raw JSON `"subtype":"error_max_turns"` (if output wasn't extracted from JSON)
/// - The text "error_max_turns" appearing in the output
/// - "MAX_TURNS_EXIT" marker from the runner script's log line
pub fn is_max_turns_exit(result: &str) -> bool {
    // Check last 30 lines only — the indicator would be near the end
    let tail: Vec<&str> = result.lines().rev().take(30).collect();
    for line in &tail {
        let line = line.trim();
        if line.contains("error_max_turns") || line.contains("MAX_TURNS_EXIT") {
            return true;
        }
    }
    // Also check for raw JSON subtype field (if entire JSON was dumped as output)
    if result.contains(r#""subtype":"error_max_turns""#)
        || result.contains(r#""subtype": "error_max_turns""#)
    {
        return true;
    }
    false
}

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

/// Extract comment blocks from agent output.
/// Looks for `---COMMENT---` ... `---END COMMENT---` delimiters.
/// Returns vec of comment bodies (trimmed). Empty blocks are skipped.
pub fn parse_comments(result: &str) -> Vec<String> {
    let mut comments = Vec::new();
    let mut in_comment = false;
    let mut current = String::new();

    for line in result.lines() {
        let trimmed = line.trim();
        if trimmed == "---COMMENT---" {
            in_comment = true;
            current.clear();
        } else if trimmed == "---END COMMENT---" && in_comment {
            in_comment = false;
            let body = current.trim().to_string();
            if !body.is_empty() {
                comments.push(body);
            }
        } else if in_comment {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }

    comments
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

/// Extract the review body from reviewer output for posting as a PR comment.
///
/// Uses the `---COMMENT---` block if present (preferred — structured output).
/// Falls back to extracting everything except the verdict line.
pub fn extract_review_body(output: &str) -> Option<String> {
    // Prefer structured comment blocks — reviewer is instructed to use them
    let comments = parse_comments(output);
    if !comments.is_empty() {
        return Some(comments.join("\n\n---\n\n"));
    }

    // Fallback: strip verdict lines and return the rest (trimmed)
    let body: String = output
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("VERDICT=")
                && !trimmed.starts_with("REVIEW_VERDICT=")
                && !trimmed.starts_with("QA_VERDICT=")
                && !trimmed.starts_with("DEPLOY_VERDICT=")
                && !trimmed.starts_with("FIX_VERDICT=")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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

/// Required sections that must appear in analyst spec output.
/// Each entry is (heading text for regex match, human-readable name for error messages).
const REQUIRED_SPEC_SECTIONS: &[(&str, &str)] = &[
    ("scope", "## Scope"),
    ("acceptance criteria", "## Acceptance Criteria"),
    ("out of scope", "## Out of Scope"),
];

/// Validate that analyst output contains all required spec sections.
///
/// Returns `Ok(())` if all sections are present, or `Err(Vec<String>)` with
/// the human-readable names of missing sections.
///
/// Uses case-insensitive heading detection: matches `## Scope`, `## SCOPE`,
/// `##Scope` (no space), and variations with extra `#` like `### Scope`.
pub fn validate_analyst_spec(output: &str) -> Result<(), Vec<String>> {
    let mut missing = Vec::new();

    for &(pattern, display_name) in REQUIRED_SPEC_SECTIONS {
        let found = output.lines().any(|line| {
            let trimmed = line.trim();
            // Match markdown headings: ##+ followed by optional space and the section name
            if let Some(rest) = trimmed.strip_prefix("##") {
                let after_hashes = rest.trim_start_matches('#');
                let heading_text = after_hashes.trim();
                heading_text.to_lowercase().starts_with(pattern)
            } else {
                false
            }
        });
        if !found {
            missing.push(display_name.to_string());
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
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

    #[test]
    fn parse_comments_single_block() {
        let output =
            "Some output.\n---COMMENT---\nThis is my comment.\n---END COMMENT---\nVERDICT=DONE";
        let comments = parse_comments(output);
        assert_eq!(comments, vec!["This is my comment."]);
    }

    #[test]
    fn parse_comments_multiple_blocks() {
        let output = "---COMMENT---\nFirst comment.\n---END COMMENT---\nSome text.\n---COMMENT---\nSecond comment.\n---END COMMENT---";
        let comments = parse_comments(output);
        assert_eq!(comments, vec!["First comment.", "Second comment."]);
    }

    #[test]
    fn parse_comments_no_blocks() {
        let output = "Just normal output.\nVERDICT=APPROVED";
        let comments = parse_comments(output);
        assert!(comments.is_empty());
    }

    #[test]
    fn parse_comments_empty_block_skipped() {
        let output = "---COMMENT---\n   \n---END COMMENT---\n---COMMENT---\nReal content.\n---END COMMENT---";
        let comments = parse_comments(output);
        assert_eq!(comments, vec!["Real content."]);
    }

    #[test]
    fn parse_comments_unclosed_block_ignored() {
        // Unclosed block — no END COMMENT → nothing pushed
        let output = "---COMMENT---\nOrphaned comment text.\nVERDICT=DONE";
        let comments = parse_comments(output);
        assert!(comments.is_empty());
    }

    #[test]
    fn parse_comments_end_without_start_ignored() {
        // END COMMENT before any START — should not panic or produce output
        let output =
            "---END COMMENT---\nSome text.\n---COMMENT---\nValid comment.\n---END COMMENT---";
        let comments = parse_comments(output);
        assert_eq!(comments, vec!["Valid comment."]);
    }

    #[test]
    fn parse_comments_multiline_body() {
        let output = "---COMMENT---\nLine one.\nLine two.\nLine three.\n---END COMMENT---";
        let comments = parse_comments(output);
        assert_eq!(comments, vec!["Line one.\nLine two.\nLine three."]);
    }

    // ─── is_max_turns_exit tests (RIG-252) ──────────────────────────────

    #[test]
    fn max_turns_exit_raw_json_subtype() {
        let output =
            r#"{"type":"result","subtype":"error_max_turns","is_error":false,"result":""}"#;
        assert!(is_max_turns_exit(output));
    }

    #[test]
    fn max_turns_exit_raw_json_spaced() {
        let output = r#"{"type": "result", "subtype": "error_max_turns", "is_error": false}"#;
        assert!(is_max_turns_exit(output));
    }

    #[test]
    fn max_turns_exit_in_text_output() {
        let output = "Some partial work done.\nerror_max_turns\nVERDICT=DONE";
        assert!(is_max_turns_exit(output));
    }

    #[test]
    fn max_turns_exit_runner_marker() {
        let output = "Partial output here.\nMAX_TURNS_EXIT — agent hit max_turns";
        assert!(is_max_turns_exit(output));
    }

    #[test]
    fn max_turns_exit_normal_output_not_detected() {
        let output = "All work completed successfully.\nVERDICT=DONE";
        assert!(!is_max_turns_exit(output));
    }

    #[test]
    fn max_turns_exit_empty_output() {
        assert!(!is_max_turns_exit(""));
    }

    // ─── extract_review_body ─────────────────────────────────────────────

    #[test]
    fn extract_review_body_from_comment_block() {
        let output = "Some preamble.\n---COMMENT---\n## Review\n- blocker: no tests\n- nit: typo\n---END COMMENT---\nREVIEW_VERDICT=REJECTED";
        let body = extract_review_body(output);
        assert!(body.is_some());
        let body = body.unwrap();
        assert!(body.contains("blocker: no tests"));
        assert!(body.contains("nit: typo"));
    }

    #[test]
    fn extract_review_body_fallback_strips_verdict() {
        let output = "## Review\nLooks good, minor issues only.\nREVIEW_VERDICT=APPROVED";
        let body = extract_review_body(output);
        assert!(body.is_some());
        let body = body.unwrap();
        assert!(body.contains("Looks good"));
        assert!(
            !body.contains("REVIEW_VERDICT="),
            "verdict line should be stripped"
        );
    }

    #[test]
    fn extract_review_body_empty_output() {
        assert!(extract_review_body("").is_none());
    }

    #[test]
    fn extract_review_body_only_verdict() {
        assert!(extract_review_body("REVIEW_VERDICT=APPROVED").is_none());
    }

    #[test]
    fn extract_review_body_multiple_comment_blocks() {
        let output = "---COMMENT---\nFirst finding.\n---END COMMENT---\n---COMMENT---\nSecond finding.\n---END COMMENT---\nREVIEW_VERDICT=REJECTED";
        let body = extract_review_body(output).unwrap();
        assert!(body.contains("First finding."));
        assert!(body.contains("Second finding."));
    }

    // ─── RIG-335: PR_URL parsing edge cases ─────────────────────────────

    #[test]
    fn parse_pr_url_strips_markdown_link_syntax() {
        // Agent might wrap URL in markdown: [PR](https://github.com/...)
        let output = "Created [PR](https://github.com/org/repo/pull/42)\nVERDICT=DONE";
        let url = parse_pr_url(output);
        assert_eq!(
            url,
            Some("https://github.com/org/repo/pull/42".to_string()),
            "should extract URL from markdown link, stripping trailing )"
        );
    }

    #[test]
    fn parse_pr_url_strips_angle_brackets() {
        // Agent might use angle brackets: <https://github.com/...>
        let output = "PR: <https://github.com/org/repo/pull/42>\nVERDICT=DONE";
        let url = parse_pr_url(output);
        assert_eq!(
            url,
            Some("https://github.com/org/repo/pull/42".to_string()),
            "should extract URL from angle brackets, stripping trailing >"
        );
    }

    #[test]
    fn parse_pr_url_with_trailing_whitespace() {
        let output = "PR_URL=https://github.com/org/repo/pull/42  \nVERDICT=DONE";
        let url = parse_pr_url(output);
        assert_eq!(
            url,
            Some("https://github.com/org/repo/pull/42".to_string()),
            "should trim trailing whitespace from PR URL"
        );
    }

    #[test]
    fn parse_pr_url_prefers_explicit_marker_over_raw_url() {
        // When both PR_URL= marker and raw URL exist, PR_URL= should win
        let output = "See https://github.com/org/repo/pull/1 for context\n\
                       PR_URL=https://github.com/org/repo/pull/99\nVERDICT=DONE";
        let url = parse_pr_url(output);
        assert_eq!(
            url,
            Some("https://github.com/org/repo/pull/99".to_string()),
            "explicit PR_URL= marker should take precedence over raw URL"
        );
    }

    #[test]
    fn parse_pr_url_issue_url_not_matched() {
        // GitHub issue URLs should NOT match (must contain /pull/)
        let output = "PR_URL=https://github.com/org/repo/issues/42\nVERDICT=DONE";
        assert_eq!(
            parse_pr_url(output),
            None,
            "issue URLs must not be treated as PR URLs"
        );
    }

    #[test]
    fn parse_pr_url_no_url_at_all() {
        // Engineer output without any URL — must return None
        let output = "Implementation complete.\nAll tests pass.\ncargo clippy clean.\nVERDICT=DONE";
        assert_eq!(
            parse_pr_url(output),
            None,
            "output with no URL must return None"
        );
    }

    #[test]
    fn parse_pr_url_multiple_raw_urls_takes_first() {
        // When no PR_URL= marker, fallback scans forward (first match wins)
        let output = "Created https://github.com/org/repo/pull/10\n\
                       Also see https://github.com/org/repo/pull/20";
        let url = parse_pr_url(output);
        assert_eq!(
            url,
            Some("https://github.com/org/repo/pull/10".to_string()),
            "raw URL fallback should take first match (forward scan)"
        );
    }

    // ─── RIG-335: REVIEW_VERDICT edge cases ─────────────────────────────

    #[test]
    fn verdict_parsing_case_insensitive_input() {
        assert_eq!(
            parse_verdict("REVIEW_VERDICT=approved"),
            Some("APPROVED".to_string()),
            "verdict should be uppercased regardless of input case"
        );
        assert_eq!(parse_verdict("VERDICT=Done"), Some("DONE".to_string()),);
    }

    #[test]
    fn verdict_parsing_inline_with_prefix_text() {
        // Agent might write "Final REVIEW_VERDICT=APPROVED" on one line
        let text = "After thorough review, REVIEW_VERDICT=APPROVED";
        assert_eq!(parse_verdict(text), Some("APPROVED".to_string()));
    }

    #[test]
    fn verdict_parsing_with_trailing_punctuation() {
        // Agent might add punctuation after verdict
        assert_eq!(
            parse_verdict("VERDICT=DONE."),
            Some("DONE".to_string()),
            "trailing punctuation should be stripped"
        );
    }

    #[test]
    fn verdict_failed_keyword_fallback() {
        // "FAILED" as standalone keyword in last 10 lines
        let text = "Tests did not pass.\nFAILED";
        assert_eq!(parse_verdict(text), Some("FAILED".to_string()));
    }

    // ─── validate_analyst_spec ──────────────────────────────────────────

    #[test]
    fn validate_spec_all_sections_present() {
        let output = "## Scope\nDo X\n## Acceptance Criteria\n- AC1\n## Out of Scope\n- Not Y\n";
        assert!(validate_analyst_spec(output).is_ok());
    }

    #[test]
    fn validate_spec_missing_scope() {
        let output = "## Acceptance Criteria\n- AC1\n## Out of Scope\n- Not Y\n";
        let err = validate_analyst_spec(output).unwrap_err();
        assert_eq!(err, vec!["## Scope"]);
    }

    #[test]
    fn validate_spec_missing_multiple() {
        let output = "## Introduction\nSome text\n";
        let err = validate_analyst_spec(output).unwrap_err();
        assert_eq!(err.len(), 3);
        assert!(err.contains(&"## Scope".to_string()));
        assert!(err.contains(&"## Acceptance Criteria".to_string()));
        assert!(err.contains(&"## Out of Scope".to_string()));
    }

    #[test]
    fn validate_spec_case_insensitive() {
        let output = "## SCOPE\nDo X\n## acceptance criteria\n- AC1\n## OUT OF SCOPE\n- Not Y\n";
        assert!(validate_analyst_spec(output).is_ok());
    }

    #[test]
    fn validate_spec_deeper_heading_level() {
        // ### Scope (h3) should also match
        let output = "### Scope\nDo X\n### Acceptance Criteria\n- AC1\n### Out of Scope\n- Not Y\n";
        assert!(validate_analyst_spec(output).is_ok());
    }

    #[test]
    fn validate_spec_no_space_after_hashes() {
        let output = "##Scope\nDo X\n##Acceptance Criteria\n- AC1\n##Out of Scope\n- Not Y\n";
        assert!(validate_analyst_spec(output).is_ok());
    }

    #[test]
    fn validate_spec_heading_with_extra_text() {
        // "## Scope & Boundaries" should match since it starts with "scope"
        let output = "## Scope & Boundaries\nDo X\n## Acceptance Criteria (MVP)\n- AC1\n## Out of Scope for v1\n- Not Y\n";
        assert!(validate_analyst_spec(output).is_ok());
    }

    #[test]
    fn validate_spec_empty_output() {
        let err = validate_analyst_spec("").unwrap_err();
        assert_eq!(err.len(), 3);
    }
}
