use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Default number of completed/failed/canceled tasks shown in `werma st`.
pub const DEFAULT_COMPLETED_LIMIT: usize = 17;

/// User-level configuration loaded from `~/.werma/config.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct UserConfig {
    /// Max completed/failed/canceled tasks in `werma st` (0 = unlimited).
    pub completed_limit: Option<usize>,
}

impl UserConfig {
    /// Resolved limit: config value → default (17). 0 means unlimited (returns `None`).
    pub fn resolved_completed_limit(&self) -> Option<usize> {
        match self.completed_limit {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(DEFAULT_COMPLETED_LIMIT),
        }
    }

    /// Load config from a specific path; returns `Default` on missing/invalid file.
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| toml::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Load from the default location `~/.werma/config.toml`.
    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    fn default_path() -> PathBuf {
        dirs::home_dir()
            .map(|h| h.join(".werma/config.toml"))
            .unwrap_or_default()
    }
}

/// Read a key from ~/.werma/.env file.
/// Falls back to VarError::NotPresent if not found.
pub fn read_env_file_key(key: &str) -> Result<String, std::env::VarError> {
    let env_path = dirs::home_dir()
        .map(|h| h.join(".werma/.env"))
        .unwrap_or_default();

    read_env_key_from_path(&env_path, key)
}

/// Read a key from a specific .env file path.
fn read_env_key_from_path(path: &std::path::Path, key: &str) -> Result<String, std::env::VarError> {
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == key
            {
                return Ok(v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    Err(std::env::VarError::NotPresent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_key_value() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "FOO=bar\nBAZ=qux\n").unwrap();

        assert_eq!(read_env_key_from_path(&env_file, "FOO").unwrap(), "bar");
        assert_eq!(read_env_key_from_path(&env_file, "BAZ").unwrap(), "qux");
    }

    #[test]
    fn parse_quoted_values() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "A=\"hello world\"\nB='single quoted'\n").unwrap();

        assert_eq!(
            read_env_key_from_path(&env_file, "A").unwrap(),
            "hello world"
        );
        assert_eq!(
            read_env_key_from_path(&env_file, "B").unwrap(),
            "single quoted"
        );
    }

    #[test]
    fn skip_comments_and_empty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "# comment\n\nKEY=value\n  # another comment\n").unwrap();

        assert_eq!(read_env_key_from_path(&env_file, "KEY").unwrap(), "value");
        assert!(read_env_key_from_path(&env_file, "comment").is_err());
    }

    #[test]
    fn missing_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "FOO=bar\n").unwrap();

        let result = read_env_key_from_path(&env_file, "MISSING");
        assert!(result.is_err());
    }

    #[test]
    fn missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("nonexistent");

        let result = read_env_key_from_path(&env_file, "ANY");
        assert!(result.is_err());
    }

    #[test]
    fn handles_whitespace_around_key_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "  KEY  =  value  \n").unwrap();

        assert_eq!(read_env_key_from_path(&env_file, "KEY").unwrap(), "value");
    }

    // ─── UserConfig tests ──────────────────────────────────────────────

    #[test]
    fn user_config_default_limit() {
        let cfg = UserConfig::default();
        assert_eq!(
            cfg.resolved_completed_limit(),
            Some(DEFAULT_COMPLETED_LIMIT)
        );
    }

    #[test]
    fn user_config_custom_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "completed_limit = 25\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(cfg.resolved_completed_limit(), Some(25));
    }

    #[test]
    fn user_config_zero_means_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "completed_limit = 0\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(cfg.resolved_completed_limit(), None);
    }

    #[test]
    fn user_config_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");

        let cfg = UserConfig::load_from(&path);
        assert_eq!(
            cfg.resolved_completed_limit(),
            Some(DEFAULT_COMPLETED_LIMIT)
        );
    }

    #[test]
    fn user_config_invalid_toml_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not valid toml {{{\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(
            cfg.resolved_completed_limit(),
            Some(DEFAULT_COMPLETED_LIMIT)
        );
    }

    #[test]
    fn user_config_ignores_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "completed_limit = 5\nunknown_key = true\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(cfg.resolved_completed_limit(), Some(5));
    }
}
