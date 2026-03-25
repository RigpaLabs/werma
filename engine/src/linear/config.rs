use anyhow::{Context, Result, bail};
use serde_json::Value;

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
        let raw: Value = serde::Deserialize::deserialize(deserializer)?;

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

// --- Config file I/O ---

pub(crate) fn config_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".werma/linear.json"))
}

pub fn load_config() -> Result<LinearConfig> {
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

pub(crate) fn save_config(config: &LinearConfig) -> Result<()> {
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
