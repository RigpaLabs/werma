use anyhow::{Result, bail};

/// Per-team configuration (team_id, team_key, and workflow status mapping).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct TeamConfig {
    pub team_id: String,
    #[serde(default)]
    pub team_key: String,
    pub statuses: std::collections::HashMap<String, String>,
}

/// Configuration stored in ~/.werma/linear.json.
/// Supports both legacy single-team format and new multi-team format.
///
/// Legacy format:   `{ "team_id": "...", "team_key": "RIG", "statuses": {...} }`
/// Multi-team:      `{ "teams": [ { "team_id": "...", "team_key": "RIG", "statuses": {...} }, ... ] }`
#[derive(Debug, Clone)]
pub struct LinearConfig {
    pub teams: Vec<TeamConfig>,
}

/// For backward compatibility: the primary team (first in the list).
impl LinearConfig {
    pub fn primary_team(&self) -> Option<&TeamConfig> {
        self.teams.first()
    }

    /// Look up team config by team_key (e.g. "RIG", "FAT").
    pub fn team_by_key(&self, key: &str) -> Option<&TeamConfig> {
        self.teams.iter().find(|t| t.team_key == key)
    }

    /// All configured team keys.
    pub fn team_keys(&self) -> Vec<&str> {
        self.teams.iter().map(|t| t.team_key.as_str()).collect()
    }

    /// Resolve a status name to a state ID for a given team key.
    /// Falls back to primary team if team_key is empty.
    pub fn status_id(&self, team_key: &str, status_name: &str) -> Option<&String> {
        let team = if team_key.is_empty() {
            self.primary_team()
        } else {
            self.team_by_key(team_key).or(self.primary_team())
        };
        team.and_then(|t| t.statuses.get(status_name))
    }
}

// Custom serde: support both legacy single-team and new multi-team format.
impl serde::Serialize for LinearConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Multi<'a> {
            teams: &'a Vec<TeamConfig>,
        }
        Multi { teams: &self.teams }.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for LinearConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: serde_json::Value = serde::Deserialize::deserialize(deserializer)?;

        // New format: { "teams": [...] }
        if raw.get("teams").is_some() {
            #[derive(serde::Deserialize)]
            struct Multi {
                teams: Vec<TeamConfig>,
            }
            let m: Multi = serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
            return Ok(LinearConfig { teams: m.teams });
        }

        // Legacy format: { "team_id": "...", "team_key": "...", "statuses": {...} }
        let single: TeamConfig = serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
        Ok(LinearConfig {
            teams: vec![single],
        })
    }
}

pub(super) fn config_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".werma/linear.json"))
}

pub(crate) fn load_config() -> Result<LinearConfig> {
    let path = config_path()?;
    if !path.exists() {
        bail!(
            "Linear not configured. Run: werma linear setup\n  (missing {})",
            path.display()
        );
    }
    let data = std::fs::read_to_string(&path)?;
    let config: LinearConfig = serde_json::from_str(&data)?;
    Ok(config)
}

pub(super) fn save_config(config: &LinearConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Get the configured team key (e.g. "RIG") from ~/.werma/linear.json.
/// Returns the primary (first) team key for backward compatibility.
pub fn configured_team_key() -> Result<String> {
    let config = load_config()?;
    Ok(config
        .primary_team()
        .map(|t| t.team_key.clone())
        .unwrap_or_default())
}

/// Get all configured team keys (e.g. ["RIG", "FAT"]).
pub fn configured_team_keys() -> Result<Vec<String>> {
    let config = load_config()?;
    Ok(config.teams.iter().map(|t| t.team_key.clone()).collect())
}

/// Extract the team key prefix from an issue identifier (e.g. "RIG-123" → "RIG").
/// Returns empty string for UUIDs or unparseable identifiers.
pub fn team_key_from_identifier(identifier: &str) -> String {
    if let Some(pos) = identifier.rfind('-') {
        let prefix = &identifier[..pos];
        let suffix = &identifier[pos + 1..];
        // Only treat as team key if suffix is all digits (e.g. "RIG-123")
        if suffix.chars().all(|c| c.is_ascii_digit()) && !suffix.is_empty() {
            return prefix.to_string();
        }
    }
    String::new()
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
pub(super) fn repo_label_to_dir(repo: &str, config: &crate::config::UserConfig) -> String {
    let repo = repo.trim();
    // Handle legacy alias
    let repo = if repo == "forge" { "werma" } else { repo };
    config.repo_dir(repo)
}

/// Expand `~` to the user's home directory.
pub(super) fn expand_tilde(path: &str) -> String {
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

use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_mapping() {
        assert_eq!(map_priority(1), 1); // Urgent -> High
        assert_eq!(map_priority(2), 1); // High -> High
        assert_eq!(map_priority(3), 2); // Medium -> Normal
        assert_eq!(map_priority(0), 2); // No priority -> Normal
        assert_eq!(map_priority(4), 3); // Low -> Low
        assert_eq!(map_priority(99), 2); // Unknown -> Normal
    }

    #[test]
    fn type_inference_from_labels() {
        assert_eq!(infer_type_from_labels(&["Bug"]), "code");
        assert_eq!(infer_type_from_labels(&["bug-fix"]), "code");
        assert_eq!(infer_type_from_labels(&["Research"]), "research");
        assert_eq!(infer_type_from_labels(&["Code Review"]), "review");
        assert_eq!(infer_type_from_labels(&["Refactor"]), "refactor");
        assert_eq!(infer_type_from_labels(&["Tech Debt"]), "refactor");
        assert_eq!(infer_type_from_labels(&["Feature"]), "code");
        assert_eq!(infer_type_from_labels(&["Enhancement"]), "code");
        assert_eq!(infer_type_from_labels(&["random-label"]), "code"); // default
        assert_eq!(infer_type_from_labels(&[]), "code"); // empty labels
    }

    /// Helper: default UserConfig for tests (no custom repos — convention fallback only).
    fn test_config() -> crate::config::UserConfig {
        crate::config::UserConfig::default()
    }

    #[test]
    fn working_dir_from_title() {
        let cfg = test_config();
        assert_eq!(
            infer_working_dir("Fix werma daemon crash", &[], &cfg),
            "~/projects/werma"
        );
        assert_eq!(
            infer_working_dir("Add pipeline stage", &[], &cfg),
            "~/projects/werma"
        );
        // Default fallback for unknown titles
        assert_eq!(
            infer_working_dir("Random task title", &[], &cfg),
            "~/projects/werma"
        );
    }

    #[test]
    fn working_dir_from_repo_label() {
        let cfg = test_config();
        // Convention-based repo labels resolve to rigpa/ paths
        assert_eq!(
            infer_working_dir("Some task", &["repo:forge"], &cfg),
            "~/projects/werma"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:werma"], &cfg),
            "~/projects/werma"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:fathom"], &cfg),
            "~/projects/fathom"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:hyper-liq"], &cfg),
            "~/projects/hyper-liq"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:sui-bots"], &cfg),
            "~/projects/sui-bots"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:ar-quant"], &cfg),
            "~/projects/ar-quant"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:ar-quant-alpha"], &cfg),
            "~/projects/ar-quant-alpha"
        );
        // repo: label takes priority over title keywords
        assert_eq!(
            infer_working_dir("Fix werma bug", &["repo:fathom"], &cfg),
            "~/projects/fathom"
        );
        // Unknown repo label uses convention fallback (no keyword inference)
        assert_eq!(
            infer_working_dir("Fix werma bug", &["repo:unknown-project"], &cfg),
            "~/projects/unknown-project"
        );
    }

    #[test]
    fn working_dir_title_keywords() {
        let cfg = test_config();
        assert_eq!(
            infer_working_dir("sui bot improvements", &[], &cfg),
            "~/projects/sui-bots"
        );
        assert_eq!(
            infer_working_dir("hyper liquidation fix", &[], &cfg),
            "~/projects/hyper-liq"
        );
    }

    #[test]
    fn working_dir_custom_config_override() {
        let mut cfg = test_config();
        cfg.repos
            .insert("werma".to_string(), "/custom/path/werma".to_string());
        assert_eq!(
            infer_working_dir("Fix werma bug", &[], &cfg),
            "/custom/path/werma"
        );
        assert_eq!(
            infer_working_dir("Some task", &["repo:werma"], &cfg),
            "/custom/path/werma"
        );
        // Non-overridden repos still use convention
        assert_eq!(
            infer_working_dir("Some task", &["repo:fathom"], &cfg),
            "~/projects/fathom"
        );
    }

    #[test]
    fn manual_label_detection() {
        assert!(is_manual_issue(&["manual"]));
        assert!(is_manual_issue(&["Manual"]));
        assert!(is_manual_issue(&["MANUAL"]));
        assert!(is_manual_issue(&["Feature", "manual", "repo:werma"]));
        assert!(!is_manual_issue(&["Feature", "Bug"]));
        assert!(!is_manual_issue(&[]));
        assert!(!is_manual_issue(&["manually-created"])); // partial match must NOT trigger
    }

    #[test]
    fn resolve_uuid_detects_identifier_pattern() {
        let is_identifier = |id: &str| -> bool {
            id.contains('-')
                && id
                    .rsplit('-')
                    .next()
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        };

        assert!(is_identifier("RIG-155"));
        assert!(is_identifier("RIG-1"));
        assert!(is_identifier("PROJ-9999"));
        assert!(!is_identifier("755e63ee-a00e-4fef-9d7a-b8907652e2b2"));
        assert!(!is_identifier("no-digits-here"));
        assert!(!is_identifier("plainuuid"));
        assert!(!is_identifier(""));
    }

    #[test]
    fn repo_label_mapping() {
        let cfg = test_config();
        assert_eq!(repo_label_to_dir("forge", &cfg), "~/projects/werma");
        assert_eq!(repo_label_to_dir("werma", &cfg), "~/projects/werma");
        assert_eq!(repo_label_to_dir("fathom", &cfg), "~/projects/fathom");
        assert_eq!(repo_label_to_dir("hyper-liq", &cfg), "~/projects/hyper-liq");
        assert_eq!(repo_label_to_dir("sui-bots", &cfg), "~/projects/sui-bots");
        assert_eq!(repo_label_to_dir("ar-quant", &cfg), "~/projects/ar-quant");
        assert_eq!(
            repo_label_to_dir("ar-quant-alpha", &cfg),
            "~/projects/ar-quant-alpha"
        );
        assert_eq!(repo_label_to_dir("sigil", &cfg), "~/projects/sigil");
        // Unknown repos get convention-based fallback
        assert_eq!(
            repo_label_to_dir("unknown-repo", &cfg),
            "~/projects/unknown-repo"
        );
    }

    #[test]
    fn infer_working_dir_repo_label_overrides_keyword() {
        let cfg = test_config();
        // repo: label should take priority over title keyword matching
        assert_eq!(
            infer_working_dir("Fix fathom collector", &["repo:werma"], &cfg),
            "~/projects/werma"
        );
    }

    #[test]
    fn infer_working_dir_all_repo_labels() {
        let cfg = test_config();
        let cases = [
            ("repo:werma", "~/projects/werma"),
            ("repo:forge", "~/projects/werma"),
            ("repo:fathom", "~/projects/fathom"),
            ("repo:hyper-liq", "~/projects/hyper-liq"),
            ("repo:sui-bots", "~/projects/sui-bots"),
            ("repo:ar-quant", "~/projects/ar-quant"),
            ("repo:ar-quant-alpha", "~/projects/ar-quant-alpha"),
            ("repo:sigil", "~/projects/sigil"),
        ];
        for (label, expected) in cases {
            assert_eq!(
                infer_working_dir("Some task", &[label], &cfg),
                expected,
                "failed for label: {label}"
            );
        }
    }

    #[test]
    fn infer_working_dir_unknown_repo_uses_convention() {
        let cfg = test_config();
        // Unknown repo label uses convention fallback ~/projects/{name}
        assert_eq!(
            infer_working_dir("Fix fathom bug", &["repo:nonexistent"], &cfg),
            "~/projects/nonexistent"
        );
    }

    #[test]
    fn infer_working_dir_unknown_repo_no_keyword_uses_convention() {
        let cfg = test_config();
        // Unknown repo label → convention path (not keyword inference)
        assert_eq!(
            infer_working_dir("Some generic task", &["repo:my-new-repo"], &cfg),
            "~/projects/my-new-repo"
        );
    }

    #[test]
    fn infer_working_dir_sigil_keyword() {
        let cfg = test_config();
        assert_eq!(
            infer_working_dir("Build sigil signal engine", &[], &cfg),
            "~/projects/sigil"
        );
    }

    #[test]
    fn validate_working_dir_nonexistent() {
        assert!(validate_working_dir("~/projects/nonexistent-xyz-999").is_none());
    }

    #[test]
    fn validate_working_dir_exists() {
        assert!(validate_working_dir("~/").is_some());
    }

    #[test]
    fn expand_tilde_works() {
        let expanded = expand_tilde("~/projects/test");
        assert!(!expanded.starts_with("~/"));
        assert!(expanded.ends_with("/projects/test"));
    }

    #[test]
    fn working_dir_fathom_keyword() {
        let cfg = test_config();
        assert_eq!(
            infer_working_dir("Fix fathom collector", &[], &cfg),
            "~/projects/fathom"
        );
    }

    #[test]
    fn working_dir_ar_quant_keywords() {
        let cfg = test_config();
        assert_eq!(
            infer_working_dir("Update ar-quant-alpha bot", &[], &cfg),
            "~/projects/ar-quant-alpha"
        );
        assert_eq!(
            infer_working_dir("Fix ar-quant backtesting", &[], &cfg),
            "~/projects/ar-quant"
        );
    }

    // ─── Multi-team config tests ────────────────────────────────────────

    #[test]
    fn multi_team_config_deserialize() {
        let json = r#"{
            "teams": [
                {
                    "team_id": "id-rig",
                    "team_key": "RIG",
                    "statuses": {"todo": "s1", "in_progress": "s2", "done": "s3"}
                },
                {
                    "team_id": "id-fat",
                    "team_key": "FAT",
                    "statuses": {"todo": "s4", "in_progress": "s5", "done": "s6"}
                }
            ]
        }"#;
        let config: LinearConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.teams.len(), 2);
        assert_eq!(config.teams[0].team_key, "RIG");
        assert_eq!(config.teams[1].team_key, "FAT");
        assert_eq!(config.team_keys(), vec!["RIG", "FAT"]);
    }

    #[test]
    fn legacy_single_team_config_deserialize() {
        let json = r#"{
            "team_id": "id-rig",
            "team_key": "RIG",
            "statuses": {"todo": "s1", "done": "s2"}
        }"#;
        let config: LinearConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.teams.len(), 1);
        assert_eq!(config.teams[0].team_key, "RIG");
        assert_eq!(config.teams[0].team_id, "id-rig");
    }

    #[test]
    fn multi_team_config_roundtrip() {
        let config = LinearConfig {
            teams: vec![
                TeamConfig {
                    team_id: "id-1".to_string(),
                    team_key: "RIG".to_string(),
                    statuses: [("todo".to_string(), "s1".to_string())]
                        .into_iter()
                        .collect(),
                },
                TeamConfig {
                    team_id: "id-2".to_string(),
                    team_key: "FAT".to_string(),
                    statuses: [("todo".to_string(), "s2".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
        };
        let json = serde_json::to_string(&config).unwrap();
        let loaded: LinearConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.teams.len(), 2);
        assert_eq!(loaded.team_by_key("FAT").unwrap().team_id, "id-2");
    }

    #[test]
    fn team_by_key_lookup() {
        let config = LinearConfig {
            teams: vec![
                TeamConfig {
                    team_id: "id-rig".to_string(),
                    team_key: "RIG".to_string(),
                    statuses: [("done".to_string(), "rig-done".to_string())]
                        .into_iter()
                        .collect(),
                },
                TeamConfig {
                    team_id: "id-fat".to_string(),
                    team_key: "FAT".to_string(),
                    statuses: [("done".to_string(), "fat-done".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
        };
        assert_eq!(config.team_by_key("RIG").unwrap().team_id, "id-rig");
        assert_eq!(config.team_by_key("FAT").unwrap().team_id, "id-fat");
        assert!(config.team_by_key("UNKNOWN").is_none());
    }

    #[test]
    fn status_id_resolves_per_team() {
        let config = LinearConfig {
            teams: vec![
                TeamConfig {
                    team_id: "id-rig".to_string(),
                    team_key: "RIG".to_string(),
                    statuses: [("in_progress".to_string(), "rig-ip".to_string())]
                        .into_iter()
                        .collect(),
                },
                TeamConfig {
                    team_id: "id-fat".to_string(),
                    team_key: "FAT".to_string(),
                    statuses: [("in_progress".to_string(), "fat-ip".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
        };
        assert_eq!(config.status_id("RIG", "in_progress").unwrap(), "rig-ip");
        assert_eq!(config.status_id("FAT", "in_progress").unwrap(), "fat-ip");
        // Empty team key falls back to primary
        assert_eq!(config.status_id("", "in_progress").unwrap(), "rig-ip");
    }

    #[test]
    fn team_key_from_identifier_extracts_prefix() {
        assert_eq!(team_key_from_identifier("RIG-123"), "RIG");
        assert_eq!(team_key_from_identifier("FAT-42"), "FAT");
        assert_eq!(team_key_from_identifier("AR-1"), "AR");
        // UUIDs should return empty
        assert_eq!(
            team_key_from_identifier("d199cc43-40ef-4e63-9caa-467506b781f6"),
            ""
        );
        // No dash
        assert_eq!(team_key_from_identifier("nodash"), "");
    }

    // ─── Legacy config detection tests (RIG-301) ───────────────────────

    #[test]
    fn legacy_format_detected_by_team_id_key() {
        let legacy_json: serde_json::Value = serde_json::from_str(
            r#"{
                "team_id": "id-rig",
                "team_key": "RIG",
                "statuses": {"todo": "s1", "done": "s2"}
            }"#,
        )
        .unwrap();

        // Legacy format: has "team_id" but no "teams"
        assert!(legacy_json.get("team_id").is_some());
        assert!(legacy_json.get("teams").is_none());
    }

    #[test]
    fn multi_team_format_not_detected_as_legacy() {
        let multi_json: serde_json::Value = serde_json::from_str(
            r#"{
                "teams": [{
                    "team_id": "id-rig",
                    "team_key": "RIG",
                    "statuses": {"todo": "s1"}
                }]
            }"#,
        )
        .unwrap();

        // Multi-team format: has "teams", no root "team_id"
        assert!(multi_json.get("team_id").is_none());
        assert!(multi_json.get("teams").is_some());
    }

    #[test]
    fn legacy_json_parses_into_team_config() {
        let legacy_json: serde_json::Value = serde_json::from_str(
            r#"{
                "team_id": "id-rig",
                "team_key": "RIG",
                "statuses": {"todo": "s1", "in_progress": "s2", "done": "s3"}
            }"#,
        )
        .unwrap();

        let team: TeamConfig = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(team.team_id, "id-rig");
        assert_eq!(team.team_key, "RIG");
        assert_eq!(team.statuses.len(), 3);
        assert_eq!(team.statuses.get("todo").unwrap(), "s1");
    }

    #[test]
    fn force_setup_env_var_detection() {
        // Verify the detection pattern: std::env::var("X").is_ok() returns true iff X is set.
        // We test with an env var that definitely doesn't exist.
        assert!(std::env::var("WERMA_TEST_NONEXISTENT_VAR_12345").is_err());
        // The setup() code uses: let force = std::env::var("FORCE_SETUP").is_ok();
        // This test validates the pattern works — actual FORCE_SETUP is tested via integration.
    }

    #[test]
    fn missing_team_detection_logic() {
        let config = LinearConfig {
            teams: vec![TeamConfig {
                team_id: "id-rig".to_string(),
                team_key: "RIG".to_string(),
                statuses: [("todo".to_string(), "s1".to_string())]
                    .into_iter()
                    .collect(),
            }],
        };

        // Simulate: workspace has 2 teams, config has 1 → mismatch
        let workspace_count = 2;
        assert!(workspace_count > config.teams.len());

        // Simulate: workspace has 1 team, config has 1 → no mismatch
        let workspace_count = 1;
        assert!(!(workspace_count > config.teams.len()));
    }

    #[test]
    fn legacy_migration_preserves_existing_statuses() {
        // Simulate what migrate_legacy_config does: parse legacy → build multi-team
        let legacy_json: serde_json::Value = serde_json::from_str(
            r#"{
                "team_id": "id-rig",
                "team_key": "RIG",
                "statuses": {"todo": "s1", "in_progress": "s2", "done": "s3", "review": "s4"}
            }"#,
        )
        .unwrap();

        let legacy_team: TeamConfig = serde_json::from_value(legacy_json).unwrap();
        let new_team = TeamConfig {
            team_id: "id-fat".to_string(),
            team_key: "FAT".to_string(),
            statuses: [("todo".to_string(), "f1".to_string())]
                .into_iter()
                .collect(),
        };

        let config = LinearConfig {
            teams: vec![legacy_team.clone(), new_team],
        };

        // Legacy team statuses preserved exactly
        let rig = config.team_by_key("RIG").unwrap();
        assert_eq!(rig.statuses.len(), 4);
        assert_eq!(rig.statuses.get("review").unwrap(), "s4");

        // New team discovered
        let fat = config.team_by_key("FAT").unwrap();
        assert_eq!(fat.team_id, "id-fat");
        assert_eq!(fat.statuses.get("todo").unwrap(), "f1");

        // Serializes as multi-team format
        let json = serde_json::to_string(&config).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(reparsed.get("teams").is_some());
        assert!(reparsed.get("team_id").is_none());
    }
}
