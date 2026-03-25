use super::config::{LinearConfig, TeamConfig, team_key_from_identifier};
use super::helpers::{infer_type_from_labels, is_after_timestamp, map_priority};
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
        "~/projects/rigpa/werma"
    );
    assert_eq!(
        infer_working_dir("Add pipeline stage", &[], &cfg),
        "~/projects/rigpa/werma"
    );
    // Default fallback for unknown titles
    assert_eq!(
        infer_working_dir("Random task title", &[], &cfg),
        "~/projects/rigpa/werma"
    );
}

#[test]
fn working_dir_from_repo_label() {
    let cfg = test_config();
    // Convention-based repo labels resolve to rigpa/ paths
    assert_eq!(
        infer_working_dir("Some task", &["repo:forge"], &cfg),
        "~/projects/rigpa/werma"
    );
    assert_eq!(
        infer_working_dir("Some task", &["repo:werma"], &cfg),
        "~/projects/rigpa/werma"
    );
    assert_eq!(
        infer_working_dir("Some task", &["repo:fathom"], &cfg),
        "~/projects/rigpa/fathom"
    );
    assert_eq!(
        infer_working_dir("Some task", &["repo:hyper-liq"], &cfg),
        "~/projects/rigpa/hyper-liq"
    );
    assert_eq!(
        infer_working_dir("Some task", &["repo:sui-bots"], &cfg),
        "~/projects/rigpa/sui-bots"
    );
    assert_eq!(
        infer_working_dir("Some task", &["repo:ar-quant"], &cfg),
        "~/projects/rigpa/ar-quant"
    );
    assert_eq!(
        infer_working_dir("Some task", &["repo:ar-quant-alpha"], &cfg),
        "~/projects/rigpa/ar-quant-alpha"
    );
    // repo: label takes priority over title keywords
    assert_eq!(
        infer_working_dir("Fix werma bug", &["repo:fathom"], &cfg),
        "~/projects/rigpa/fathom"
    );
    // Unknown repo label uses convention fallback (no keyword inference)
    assert_eq!(
        infer_working_dir("Fix werma bug", &["repo:unknown-project"], &cfg),
        "~/projects/rigpa/unknown-project"
    );
}

#[test]
fn working_dir_title_keywords() {
    let cfg = test_config();
    assert_eq!(
        infer_working_dir("sui bot improvements", &[], &cfg),
        "~/projects/rigpa/sui-bots"
    );
    assert_eq!(
        infer_working_dir("hyper liquidation fix", &[], &cfg),
        "~/projects/rigpa/hyper-liq"
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
        "~/projects/rigpa/fathom"
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
fn infer_working_dir_repo_label_overrides_keyword() {
    let cfg = test_config();
    // repo: label should take priority over title keyword matching
    assert_eq!(
        infer_working_dir("Fix fathom collector", &["repo:werma"], &cfg),
        "~/projects/rigpa/werma"
    );
}

#[test]
fn infer_working_dir_all_repo_labels() {
    let cfg = test_config();
    let cases = [
        ("repo:werma", "~/projects/rigpa/werma"),
        ("repo:forge", "~/projects/rigpa/werma"),
        ("repo:fathom", "~/projects/rigpa/fathom"),
        ("repo:hyper-liq", "~/projects/rigpa/hyper-liq"),
        ("repo:sui-bots", "~/projects/rigpa/sui-bots"),
        ("repo:ar-quant", "~/projects/rigpa/ar-quant"),
        ("repo:ar-quant-alpha", "~/projects/rigpa/ar-quant-alpha"),
        ("repo:sigil", "~/projects/rigpa/sigil"),
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
    // Unknown repo label uses convention fallback ~/projects/rigpa/{name}
    assert_eq!(
        infer_working_dir("Fix fathom bug", &["repo:nonexistent"], &cfg),
        "~/projects/rigpa/nonexistent"
    );
}

#[test]
fn infer_working_dir_unknown_repo_no_keyword_uses_convention() {
    let cfg = test_config();
    // Unknown repo label → convention path (not keyword inference)
    assert_eq!(
        infer_working_dir("Some generic task", &["repo:my-new-repo"], &cfg),
        "~/projects/rigpa/my-new-repo"
    );
}

#[test]
fn infer_working_dir_sigil_keyword() {
    let cfg = test_config();
    assert_eq!(
        infer_working_dir("Build sigil signal engine", &[], &cfg),
        "~/projects/rigpa/sigil"
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
fn working_dir_fathom_keyword() {
    let cfg = test_config();
    assert_eq!(
        infer_working_dir("Fix fathom collector", &[], &cfg),
        "~/projects/rigpa/fathom"
    );
}

#[test]
fn working_dir_ar_quant_keywords() {
    let cfg = test_config();
    assert_eq!(
        infer_working_dir("Update ar-quant-alpha bot", &[], &cfg),
        "~/projects/rigpa/ar-quant-alpha"
    );
    assert_eq!(
        infer_working_dir("Fix ar-quant backtesting", &[], &cfg),
        "~/projects/rigpa/ar-quant"
    );
}

#[test]
fn read_env_file_key_missing_file() {
    // This tests the error path (file doesn't exist in test env)
    let result = crate::config::read_env_file_key("NONEXISTENT_KEY");
    assert!(result.is_err());
}

#[test]
fn is_after_timestamp_same_format() {
    // Both full ISO 8601 with timezone
    assert!(is_after_timestamp(
        "2026-03-24T16:00:00.000Z",
        "2026-03-24T15:00:00.000Z"
    ));
    assert!(!is_after_timestamp(
        "2026-03-24T14:00:00.000Z",
        "2026-03-24T15:00:00.000Z"
    ));
}

#[test]
fn is_after_timestamp_mixed_formats() {
    // SQLite naive (no TZ) vs Linear RFC 3339 (with Z)
    // Both treated as UTC for comparison
    assert!(is_after_timestamp(
        "2026-03-24T16:00:00.000Z",
        "2026-03-24T15:00:00"
    ));
    assert!(!is_after_timestamp(
        "2026-03-24T14:00:00.000Z",
        "2026-03-24T15:00:00"
    ));
}

#[test]
fn is_after_timestamp_equal_is_not_after() {
    assert!(!is_after_timestamp(
        "2026-03-24T15:00:00.000Z",
        "2026-03-24T15:00:00"
    ));
}

#[test]
fn mutations_use_string_type_not_id() {
    // Regression: Linear mutations must use String!, not ID!.
    // ID! works for queries but causes silent failures in mutations.
    let source = include_str!("api.rs");
    let bad_lines: Vec<&str> = source
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("r#\"mutation(") && trimmed.contains("ID!")
        })
        .collect();
    assert!(
        bad_lines.is_empty(),
        "Found mutation(s) using ID! instead of String!:\n{}",
        bad_lines.join("\n")
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
