use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::models::AgentRuntime;
use crate::notify::{self, DisplayField};

/// Default number of completed/failed/canceled tasks shown in `werma st`.
pub const DEFAULT_COMPLETED_LIMIT: usize = 17;

/// Default base directory for repo convention fallback.
/// Users can override per-repo via `[repos]` in `~/.werma/config.toml`.
const DEFAULT_REPO_BASE: &str = "~/projects";

// Default allowed runtimes are derived from `AgentRuntime::is_trusted()` — no
// separate constant needed.  See `is_runtime_allowed()` and `allowed_runtimes_for_repo()`.

/// GitHub owner + repo pair for per-repo tracker override.
///
/// ```toml
/// [tracker.github]
/// my-oss-project = { owner = "arleyar", repo = "my-oss-project" }
/// honeyjourney = { owner = "ArLeyar", repo = "honeyjourney", prefix = "HJ" }
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GitHubTrackerEntry {
    pub owner: String,
    pub repo: String,
    /// Optional short prefix for display (e.g. "HJ" → "HJ-20" instead of "honeyjourney#20").
    /// Falls back to `repo#N` format if not set.
    pub prefix: Option<String>,
}

/// Tracker selection config.
///
/// ```toml
/// [tracker]
/// default = "linear"          # "linear" or "github"
///
/// [tracker.github]
/// my-oss-project = { owner = "arleyar", repo = "my-oss-project" }
/// ```
///
/// Repos not listed under `[tracker.github]` fall back to `default`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct TrackerConfig {
    /// Default tracker for repos not explicitly mapped. Defaults to `"linear"`.
    /// Used by `tracker_for_repo()` — not read directly by current code.
    #[serde(default = "default_tracker")]
    #[allow(dead_code)]
    pub default: String,

    /// Repo label → GitHub owner/repo pair.
    pub github: HashMap<String, GitHubTrackerEntry>,
}

fn default_tracker() -> String {
    "linear".to_string()
}

impl TrackerConfig {
    /// Which tracker handles the given repo label.
    ///
    /// Returns `"github"` when the repo is listed under `[tracker.github]`,
    /// otherwise falls back to `self.default` (usually `"linear"`).
    ///
    /// Route a repo label to the correct tracker type (`"github"` or `"linear"`).
    /// Currently unused — callers use `github_entry()` / `.github.values()` directly.
    /// Kept as a convenience helper for future use.
    #[allow(dead_code)]
    pub fn tracker_for_repo(&self, repo: &str) -> &str {
        if self.github.contains_key(repo) {
            "github"
        } else {
            &self.default
        }
    }

    /// Look up the GitHub owner/repo pair for a given repo label.
    ///
    /// Returns `None` when no explicit GitHub mapping is configured for this repo.
    ///
    /// Called by GitHub Issues polling (RIG-384) to create per-repo `GitHubIssueClient`.
    pub fn github_entry(&self, repo: &str) -> Option<&GitHubTrackerEntry> {
        self.github.get(repo)
    }

    /// Format an identifier for display using configured short prefixes.
    ///
    /// Converts `repo#N` → `PREFIX-N` when a prefix is configured for that repo.
    /// Non-GitHub identifiers (Linear `RIG-42`) and repos without prefixes pass through unchanged.
    ///
    /// Internal storage always uses canonical `repo#N` — this is display-only.
    pub fn display_identifier(&self, identifier: &str) -> String {
        // Only transform GitHub identifiers (contain '#')
        if let Some(hash_pos) = identifier.rfind('#') {
            let repo_part = &identifier[..hash_pos];
            let number_part = &identifier[hash_pos + 1..];
            // Direct O(1) lookup — TOML key matches repo name by convention
            if let Some(entry) = self.github.get(repo_part) {
                if let Some(prefix) = &entry.prefix {
                    return format!("{prefix}-{number_part}");
                }
            }
        }
        identifier.to_string()
    }
}

/// User-level configuration loaded from `~/.werma/config.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct UserConfig {
    /// Max completed/failed/canceled tasks in `werma st` (0 = unlimited).
    pub completed_limit: Option<usize>,

    /// Repo label → local directory mapping.
    /// Example: `werma = "~/projects/werma"`
    pub repos: HashMap<String, String>,

    /// Repo label → named pipeline to use.
    /// Example: `fathom = "economy"`
    /// Repos not listed here use the "default" pipeline.
    #[serde(default)]
    pub repo_pipelines: HashMap<String, String>,

    /// Repo label → allowed runtimes list.
    /// Example: `fathom = ["claude-code", "codex"]`
    /// Repos not listed here allow only trusted runtimes (see `AgentRuntime::is_trusted`).
    #[serde(default)]
    pub repo_runtimes: HashMap<String, Vec<String>>,

    /// Per-repo tracker selection (Linear vs GitHub Issues).
    /// Used by pipeline polling (RIG-384) to create per-repo GitHub clients.
    #[serde(default)]
    pub tracker: TrackerConfig,

    /// Configurable fields for `werma st` output.
    #[serde(default)]
    pub status: StatusDisplayConfig,

    /// Configurable fields for macOS/Slack notifications.
    #[serde(default)]
    pub notifications: NotificationsDisplayConfig,
}

/// Display field configuration for `werma st` output.
///
/// ```toml
/// [status]
/// fields = ["model", "turns"]   # default
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct StatusDisplayConfig {
    /// Field names to show in parentheses after elapsed time.
    /// Available: runtime, model, cost, turns, verdict.
    pub fields: Option<Vec<String>>,
}

/// Display field configuration for macOS/Slack notifications.
///
/// ```toml
/// [notifications]
/// fields = ["model"]   # default
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct NotificationsDisplayConfig {
    /// Field names to append to notification messages.
    /// Available: runtime, model, cost, turns, verdict.
    pub fields: Option<Vec<String>>,
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

    /// Resolve a repo label to its local directory path.
    /// Priority: explicit config entry → convention `~/projects/{repo_name}`.
    pub fn repo_dir(&self, repo: &str) -> String {
        if let Some(dir) = self.repos.get(repo) {
            return dir.clone();
        }
        format!("{DEFAULT_REPO_BASE}/{repo}")
    }

    /// Return all explicitly configured repo mappings.
    pub fn all_repos(&self) -> HashMap<String, String> {
        self.repos.clone()
    }

    /// Which named pipeline to use for a given repo.
    /// Returns "default" if no explicit mapping exists.
    pub fn pipeline_for_repo(&self, repo: &str) -> &str {
        self.repo_pipelines
            .get(repo)
            .map(String::as_str)
            .unwrap_or("default")
    }

    /// Alias for `pipeline_for_repo` — used by `pipeline switch` command.
    pub fn active_pipeline(&self, repo: &str) -> &str {
        self.pipeline_for_repo(repo)
    }

    /// Check if a runtime is allowed for a given repo.
    /// Uses the explicit allowlist if configured, otherwise falls back to
    /// `AgentRuntime::is_trusted()` (currently Claude Code + Codex).
    pub fn is_runtime_allowed(&self, repo: &str, runtime: AgentRuntime) -> bool {
        if let Some(allowed) = self.repo_runtimes.get(repo) {
            let runtime_str = runtime.to_string();
            allowed.iter().any(|r| r == &runtime_str)
        } else {
            runtime.is_trusted()
        }
    }

    /// Return the allowed runtimes list for a repo (for error messages).
    pub fn allowed_runtimes_for_repo(&self, repo: &str) -> Vec<String> {
        if let Some(allowed) = self.repo_runtimes.get(repo) {
            allowed.clone()
        } else {
            AgentRuntime::ALL
                .iter()
                .filter(|r| r.is_trusted())
                .map(ToString::to_string)
                .collect()
        }
    }

    /// Infer the repo label from a working directory path.
    /// Checks all configured repos first, then falls back to the last path component.
    pub fn repo_label_from_dir(&self, working_dir: &str) -> Option<String> {
        // Check explicit config mappings
        for (label, dir) in &self.repos {
            if working_dir == dir || working_dir.starts_with(&format!("{dir}/")) {
                return Some(label.clone());
            }
        }
        // Convention: ~/projects/{repo_name} → repo_name
        let expanded = if let Some(stripped) = working_dir.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                home.join(stripped).to_string_lossy().to_string()
            } else {
                working_dir.to_string()
            }
        } else {
            working_dir.to_string()
        };
        let path = std::path::Path::new(&expanded);
        path.file_name()
            .and_then(|n| n.to_str())
            .map(std::string::ToString::to_string)
    }

    /// Resolved display fields for `werma st` output.
    /// Returns configured fields or defaults to `["model", "turns"]`.
    pub fn status_fields(&self) -> Vec<DisplayField> {
        match &self.status.fields {
            Some(names) => notify::parse_field_names(names),
            None => notify::DEFAULT_STATUS_FIELDS.to_vec(),
        }
    }

    /// Resolved display fields for notifications.
    /// Returns configured fields or defaults to `["model"]`.
    pub fn notification_fields(&self) -> Vec<DisplayField> {
        match &self.notifications.fields {
            Some(names) => notify::parse_field_names(names),
            None => notify::DEFAULT_NOTIFICATION_FIELDS.to_vec(),
        }
    }

    /// Load config from a specific path; returns `Default` on missing/invalid file.
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| toml::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Derive the repo name from a working directory path.
    ///
    /// Checks explicit `[repos]` config first (reverse lookup), then falls back
    /// to the last path component (matches `~/projects/{repo}` convention).
    #[allow(dead_code)] // Used by `werma pipeline switch` — callers coming in RIG-367
    pub fn repo_from_working_dir(&self, working_dir: &str) -> String {
        let normalized =
            working_dir.replace('~', &dirs::home_dir().unwrap_or_default().to_string_lossy());

        // Exact match against configured repos
        for (name, dir) in &self.repos {
            let norm_dir =
                dir.replace('~', &dirs::home_dir().unwrap_or_default().to_string_lossy());
            if normalized == norm_dir
                || normalized.ends_with(&format!("/{}", norm_dir.trim_start_matches('/')))
            {
                return name.clone();
            }
        }

        // Convention fallback: last path component
        std::path::Path::new(working_dir)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("unknown")
            .to_string()
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

    // ─── Repo config tests ──────────────────────────────────────────────

    #[test]
    fn repo_dir_convention_fallback() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.repo_dir("werma"), "~/projects/werma");
        assert_eq!(cfg.repo_dir("my-app"), "~/projects/my-app");
    }

    #[test]
    fn repo_dir_config_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[repos]\nwerma = \"/custom/werma\"\nmy-repo = \"/opt/my-repo\"\n",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(cfg.repo_dir("werma"), "/custom/werma");
        assert_eq!(cfg.repo_dir("my-repo"), "/opt/my-repo");
        // Non-overridden repos use convention
        assert_eq!(cfg.repo_dir("other"), "~/projects/other");
    }

    #[test]
    fn all_repos_returns_configured_repos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[repos]\nwerma = \"/custom/werma\"\nextra = \"/opt/extra\"\n",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        let repos = cfg.all_repos();

        assert_eq!(repos["werma"], "/custom/werma");
        assert_eq!(repos["extra"], "/opt/extra");
        assert_eq!(repos.len(), 2);
    }

    #[test]
    fn repos_empty_by_default() {
        let cfg = UserConfig::default();
        assert!(cfg.repos.is_empty());
        let repos = cfg.all_repos();
        assert!(repos.is_empty());
    }

    // ─── Pipeline per-repo tests ──────────────────────────────────────────

    #[test]
    fn pipeline_for_repo_defaults_to_default() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.pipeline_for_repo("werma"), "default");
        assert_eq!(cfg.pipeline_for_repo("fathom"), "default");
    }

    #[test]
    fn pipeline_for_repo_uses_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[repo_pipelines]\nfathom = \"economy\"\nwerma = \"default\"\n",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(cfg.pipeline_for_repo("fathom"), "economy");
        assert_eq!(cfg.pipeline_for_repo("werma"), "default");
        assert_eq!(cfg.pipeline_for_repo("other"), "default");
    }

    #[test]
    fn active_pipeline_is_alias_for_pipeline_for_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[repo_pipelines]\nfathom = \"economy\"\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(
            cfg.active_pipeline("fathom"),
            cfg.pipeline_for_repo("fathom")
        );
        assert_eq!(cfg.active_pipeline("werma"), "default");
    }

    // ─── Runtime allowlist tests ───────────────────────────────────────────

    #[test]
    fn is_runtime_allowed_default_allows_both() {
        let cfg = UserConfig::default();
        assert!(cfg.is_runtime_allowed("any-repo", AgentRuntime::ClaudeCode));
        assert!(cfg.is_runtime_allowed("any-repo", AgentRuntime::Codex));
    }

    #[test]
    fn is_runtime_allowed_explicit_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[repo_runtimes]\nrestricted = [\"claude-code\"]\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        assert!(cfg.is_runtime_allowed("restricted", AgentRuntime::ClaudeCode));
        assert!(!cfg.is_runtime_allowed("restricted", AgentRuntime::Codex));
        // Non-configured repo still has default allowlist
        assert!(cfg.is_runtime_allowed("other", AgentRuntime::Codex));
    }

    #[test]
    fn allowed_runtimes_for_repo_default() {
        let cfg = UserConfig::default();
        let allowed = cfg.allowed_runtimes_for_repo("any");
        assert_eq!(allowed, vec!["claude-code", "codex"]);
    }

    #[test]
    fn allowed_runtimes_for_repo_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[repo_runtimes]\nmy-repo = [\"claude-code\"]\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        let allowed = cfg.allowed_runtimes_for_repo("my-repo");
        assert_eq!(allowed, vec!["claude-code"]);
    }

    #[test]
    fn gemini_qwen_blocked_by_default_allowlist() {
        let cfg = UserConfig::default();
        assert!(
            !cfg.is_runtime_allowed("any-repo", AgentRuntime::GeminiCli),
            "gemini should be blocked by default"
        );
        assert!(
            !cfg.is_runtime_allowed("any-repo", AgentRuntime::QwenCode),
            "qwen should be blocked by default"
        );
    }

    // ─── Status/Notifications display config tests ──────────────────────

    #[test]
    fn status_fields_default() {
        let cfg = UserConfig::default();
        let fields = cfg.status_fields();
        assert_eq!(fields, vec![DisplayField::Model, DisplayField::Turns]);
    }

    #[test]
    fn status_fields_custom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[status]\nfields = [\"runtime\", \"model\", \"turns\"]\n",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        let fields = cfg.status_fields();
        assert_eq!(
            fields,
            vec![
                DisplayField::Runtime,
                DisplayField::Model,
                DisplayField::Turns
            ]
        );
    }

    #[test]
    fn status_fields_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[status]\nfields = []\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        let fields = cfg.status_fields();
        assert!(fields.is_empty());
    }

    #[test]
    fn notification_fields_default() {
        let cfg = UserConfig::default();
        let fields = cfg.notification_fields();
        assert_eq!(fields, vec![DisplayField::Model]);
    }

    #[test]
    fn notification_fields_custom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[notifications]\nfields = [\"cost\", \"model\"]\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        let fields = cfg.notification_fields();
        assert_eq!(fields, vec![DisplayField::Cost, DisplayField::Model]);
    }

    #[test]
    fn gemini_qwen_allowed_with_explicit_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[repo_runtimes]\nmy-repo = [\"claude-code\", \"gemini-cli\", \"qwen-code\"]\n",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        assert!(cfg.is_runtime_allowed("my-repo", AgentRuntime::GeminiCli));
        assert!(cfg.is_runtime_allowed("my-repo", AgentRuntime::QwenCode));
        assert!(cfg.is_runtime_allowed("my-repo", AgentRuntime::ClaudeCode));
        // Not in allowlist for other repos
        assert!(!cfg.is_runtime_allowed("other", AgentRuntime::GeminiCli));
    }

    // ─── repo_label_from_dir tests ──────────────────────────────────────────

    #[test]
    fn repo_label_from_dir_convention() {
        let cfg = UserConfig::default();
        assert_eq!(
            cfg.repo_label_from_dir("~/projects/werma"),
            Some("werma".to_string())
        );
        assert_eq!(
            cfg.repo_label_from_dir("~/projects/fathom"),
            Some("fathom".to_string())
        );
    }

    #[test]
    fn repo_label_from_dir_explicit_config() {
        let mut cfg = UserConfig::default();
        cfg.repos
            .insert("my-app".to_string(), "/custom/path/to/app".to_string());

        assert_eq!(
            cfg.repo_label_from_dir("/custom/path/to/app"),
            Some("my-app".to_string())
        );
        // Also matches subdirectories
        assert_eq!(
            cfg.repo_label_from_dir("/custom/path/to/app/.trees/feat-branch"),
            Some("my-app".to_string())
        );
    }

    #[test]
    fn repo_label_from_dir_absolute_path() {
        let cfg = UserConfig::default();
        assert_eq!(
            cfg.repo_label_from_dir("/opt/my-project"),
            Some("my-project".to_string())
        );
    }

    // ─── repo_from_working_dir tests ──────────────────────────────────────

    #[test]
    fn repo_from_working_dir_convention_fallback() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.repo_from_working_dir("~/projects/werma"), "werma");
        assert_eq!(cfg.repo_from_working_dir("~/projects/fathom"), "fathom");
        assert_eq!(
            cfg.repo_from_working_dir("/home/user/projects/my-app"),
            "my-app"
        );
    }

    #[test]
    fn repo_from_working_dir_explicit_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[repos]\nwerma = \"~/projects/rigpa/werma\"\n").unwrap();

        let cfg = UserConfig::load_from(&path);
        // The last path component is "werma", which matches convention.
        // Even without explicit match, it resolves correctly.
        assert_eq!(cfg.repo_from_working_dir("~/projects/rigpa/werma"), "werma");
    }

    #[test]
    fn repo_from_working_dir_unknown_returns_dirname() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.repo_from_working_dir("/opt/custom/my-repo"), "my-repo");
    }

    // ─── Full config TOML parsing test ─────────────────────────────────────

    #[test]
    fn full_config_with_pipelines_and_runtimes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
completed_limit = 10

[repos]
werma = "~/projects/rigpa/werma"
fathom = "~/projects/rigpa/fathom"

[repo_pipelines]
fathom = "economy"

[repo_runtimes]
fathom = ["claude-code", "codex"]
restricted = ["claude-code"]
"#,
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        assert_eq!(cfg.resolved_completed_limit(), Some(10));
        assert_eq!(cfg.pipeline_for_repo("fathom"), "economy");
        assert_eq!(cfg.pipeline_for_repo("werma"), "default");
        assert!(cfg.is_runtime_allowed("fathom", AgentRuntime::Codex));
        assert!(!cfg.is_runtime_allowed("restricted", AgentRuntime::Codex));
    }

    // ─── GitHubTrackerEntry prefix tests ──────────────────────────────────────

    #[test]
    fn github_entry_without_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[tracker.github]\nmy-oss = { owner = \"ArLeyar\", repo = \"my-oss\" }\n",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        let entry = cfg.tracker.github_entry("my-oss").unwrap();
        assert_eq!(entry.owner, "ArLeyar");
        assert_eq!(entry.repo, "my-oss");
        assert!(entry.prefix.is_none());
    }

    #[test]
    fn github_entry_with_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[tracker.github]\nhoneyjourney = { owner = \"ArLeyar\", repo = \"honeyjourney\", prefix = \"HJ\" }\n",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        let entry = cfg.tracker.github_entry("honeyjourney").unwrap();
        assert_eq!(entry.owner, "ArLeyar");
        assert_eq!(entry.repo, "honeyjourney");
        assert_eq!(entry.prefix.as_deref(), Some("HJ"));
    }

    #[test]
    fn github_entry_prefix_optional_backward_compat() {
        // Existing configs without prefix field still parse correctly
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[tracker.github]
project-a = { owner = "org", repo = "project-a" }
project-b = { owner = "org", repo = "project-b", prefix = "PB" }
"#,
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path);
        assert!(
            cfg.tracker
                .github_entry("project-a")
                .unwrap()
                .prefix
                .is_none()
        );
        assert_eq!(
            cfg.tracker
                .github_entry("project-b")
                .unwrap()
                .prefix
                .as_deref(),
            Some("PB")
        );
    }

    #[test]
    fn display_identifier_with_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[tracker.github]
honeyjourney = { owner = "ArLeyar", repo = "honeyjourney", prefix = "HJ" }
"#,
        )
        .unwrap();
        let cfg = UserConfig::load_from(&path);

        assert_eq!(cfg.tracker.display_identifier("honeyjourney#20"), "HJ-20");
        assert_eq!(cfg.tracker.display_identifier("honeyjourney#1"), "HJ-1");
    }

    #[test]
    fn display_identifier_without_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[tracker.github]
myrepo = { owner = "org", repo = "myrepo" }
"#,
        )
        .unwrap();
        let cfg = UserConfig::load_from(&path);

        // No prefix configured → pass through unchanged
        assert_eq!(cfg.tracker.display_identifier("myrepo#42"), "myrepo#42");
    }

    #[test]
    fn display_identifier_linear_passthrough() {
        let cfg = UserConfig::default();

        // Linear identifiers pass through unchanged
        assert_eq!(cfg.tracker.display_identifier("RIG-42"), "RIG-42");
        assert_eq!(cfg.tracker.display_identifier("FAT-100"), "FAT-100");
    }

    #[test]
    fn display_identifier_empty() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.tracker.display_identifier(""), "");
    }

    #[test]
    fn display_identifier_unknown_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[tracker.github]
honeyjourney = { owner = "ArLeyar", repo = "honeyjourney", prefix = "HJ" }
"#,
        )
        .unwrap();
        let cfg = UserConfig::load_from(&path);

        // Unknown repo → pass through unchanged
        assert_eq!(
            cfg.tracker.display_identifier("other-repo#5"),
            "other-repo#5"
        );
    }
}
