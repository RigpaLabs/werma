/// Compare two ISO 8601 timestamps, returning true if `ts` is strictly after `after`.
/// Handles format mismatches between SQLite (local, no TZ) and Linear (UTC with millis).
/// Falls back to string comparison if chrono parsing fails.
pub fn is_after_timestamp(ts: &str, after: &str) -> bool {
    use chrono::{DateTime, NaiveDateTime, Utc};

    // Try parsing both as full RFC 3339 / ISO 8601 with timezone
    let parse_ts = |s: &str| -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                // Fallback: parse as naive (no timezone) — assume UTC
                NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                    .map(|ndt| ndt.and_utc())
                    .ok()
            })
    };

    match (parse_ts(ts), parse_ts(after)) {
        (Some(t), Some(a)) => t > a,
        _ => ts > after, // fallback to string comparison
    }
}

/// Map Linear priority number to werma priority.
/// Linear: 0=No priority, 1=Urgent, 2=High, 3=Medium, 4=Low
/// Werma: 1=High, 2=Normal, 3=Low
pub fn map_priority(linear_priority: i64) -> i32 {
    match linear_priority {
        1 | 2 => 1,
        3 | 0 => 2,
        4 => 3,
        _ => 2,
    }
}

/// Infer task type from Linear issue labels.
pub fn infer_type_from_labels(labels: &[&str]) -> String {
    let labels_lower: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();

    if labels_lower.iter().any(|l| l.contains("bug")) {
        return "code".to_string();
    }
    if labels_lower.iter().any(|l| l.contains("research")) {
        return "research".to_string();
    }
    if labels_lower.iter().any(|l| l.contains("review")) {
        return "review".to_string();
    }
    if labels_lower
        .iter()
        .any(|l| l.contains("refactor") || l.contains("tech debt"))
    {
        return "refactor".to_string();
    }
    if labels_lower
        .iter()
        .any(|l| l.contains("feature") || l.contains("enhancement"))
    {
        return "code".to_string();
    }

    "code".to_string()
}

/// Check if issue has the `manual` label — human-driven, agents must skip.
pub fn is_manual_issue(labels: &[&str]) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case("manual"))
}

/// Map a `repo:*` label value to its local directory path using config.
/// Handles the `forge` → `werma` alias, then delegates to `UserConfig::repo_dir`.
fn repo_label_to_dir(repo: &str, config: &crate::config::UserConfig) -> String {
    let repo = repo.trim();
    // Handle legacy alias
    let repo = if repo == "forge" { "werma" } else { repo };
    config.repo_dir(repo)
}

/// Expand `~` to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return format!("{}/{}", home.display(), rest);
    }
    path.to_string()
}

/// Validate that a resolved working directory actually exists on disk.
/// Returns `None` if the path doesn't exist.
pub fn validate_working_dir(dir: &str) -> Option<String> {
    let expanded = expand_tilde(dir);
    if std::path::Path::new(&expanded).is_dir() {
        Some(dir.to_string())
    } else {
        None
    }
}

/// Infer working directory from title keywords and labels.
/// Uses `UserConfig` for repo label → directory resolution.
pub fn infer_working_dir(
    title: &str,
    labels: &[&str],
    config: &crate::config::UserConfig,
) -> String {
    let title_lower = title.to_lowercase();

    // Check for repo: label (explicit mapping takes priority)
    for label in labels {
        if let Some(repo) = label.strip_prefix("repo:") {
            return repo_label_to_dir(repo, config);
        }
    }

    // Keyword-based inference: keyword → repo name, resolved via config
    let keywords: &[(&str, &str)] = &[
        ("werma", "werma"),
        ("pipeline", "werma"),
        ("fathom", "fathom"),
        ("sigil", "sigil"),
        ("sui", "sui-bots"),
        ("hyper", "hyper-liq"),
        ("ar-quant-alpha", "ar-quant-alpha"),
        ("ar-quant", "ar-quant"),
    ];

    for (keyword, repo) in keywords {
        if title_lower.contains(keyword) {
            return config.repo_dir(repo);
        }
    }

    config.repo_dir("werma")
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn expand_tilde_works() {
        let expanded = expand_tilde("~/projects/test");
        assert!(!expanded.starts_with("~/"));
        assert!(expanded.ends_with("/projects/test"));
    }

    #[test]
    fn repo_label_mapping() {
        let cfg = crate::config::UserConfig::default();
        assert_eq!(repo_label_to_dir("forge", &cfg), "~/projects/rigpa/werma");
        assert_eq!(repo_label_to_dir("werma", &cfg), "~/projects/rigpa/werma");
        assert_eq!(repo_label_to_dir("fathom", &cfg), "~/projects/rigpa/fathom");
        assert_eq!(
            repo_label_to_dir("hyper-liq", &cfg),
            "~/projects/rigpa/hyper-liq"
        );
        assert_eq!(
            repo_label_to_dir("sui-bots", &cfg),
            "~/projects/rigpa/sui-bots"
        );
        assert_eq!(
            repo_label_to_dir("ar-quant", &cfg),
            "~/projects/rigpa/ar-quant"
        );
        assert_eq!(
            repo_label_to_dir("ar-quant-alpha", &cfg),
            "~/projects/rigpa/ar-quant-alpha"
        );
        assert_eq!(repo_label_to_dir("sigil", &cfg), "~/projects/rigpa/sigil");
        // Unknown repos get convention-based fallback
        assert_eq!(
            repo_label_to_dir("unknown-repo", &cfg),
            "~/projects/rigpa/unknown-repo"
        );
    }
}
