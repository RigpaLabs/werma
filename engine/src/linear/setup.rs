use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::api::LinearClient;
use super::config::{LinearConfig, TeamConfig, config_path, load_config, save_config};

impl LinearClient {
    /// Discover all teams and their workflow statuses, save to ~/.werma/linear.json.
    pub fn setup(&self) -> Result<()> {
        let cfg_path = config_path()?;
        let force = std::env::var("FORCE_SETUP").is_ok();

        // Check if already configured (skip guard if FORCE_SETUP is set)
        if !force && cfg_path.exists() {
            let raw = std::fs::read_to_string(&cfg_path)?;
            let raw_json: Value = serde_json::from_str(&raw)?;

            // Detect legacy single-team format: has "team_id" but no "teams" key
            if raw_json.get("team_id").is_some() && raw_json.get("teams").is_none() {
                println!("Detected legacy single-team config — migrating to multi-team format...");
                return self.migrate_legacy_config(&raw_json);
            }

            let existing = load_config()?;
            if !existing.teams.is_empty() {
                // Check if workspace has more teams than config
                let workspace_team_count = self.count_workspace_teams();
                if let Ok(ws_count) = workspace_team_count {
                    if ws_count > existing.teams.len() {
                        eprintln!(
                            "Warning: config has {} team(s), workspace has {} — run FORCE_SETUP=1 werma linear setup to sync",
                            existing.teams.len(),
                            ws_count
                        );
                    }
                }

                let keys: Vec<&str> = existing.team_keys();
                println!(
                    "Already configured: {} team(s): {}",
                    keys.len(),
                    keys.join(", ")
                );
                println!("  To reconfigure: FORCE_SETUP=1 werma linear setup");
                return Ok(());
            }
        }

        if force {
            println!("FORCE_SETUP=1 — re-discovering all teams...");
        }

        self.discover_and_save_all_teams()
    }

    /// Migrate legacy single-team config to multi-team format.
    /// Preserves existing team's status IDs and discovers any additional workspace teams.
    fn migrate_legacy_config(&self, legacy_json: &Value) -> Result<()> {
        let legacy_team: TeamConfig =
            serde_json::from_value(legacy_json.clone()).context("parsing legacy config")?;
        let legacy_team_id = legacy_team.team_id.clone();
        println!(
            "  Existing team: {} ({})",
            legacy_team.team_key, legacy_team.team_id
        );

        // Discover all workspace teams
        let data = self.query("{ teams { nodes { id key name } } }", &json!({}))?;
        let api_teams = data["teams"]["nodes"]
            .as_array()
            .context("no teams found")?;

        let mut team_configs = Vec::new();

        for team in api_teams {
            let team_id = team["id"].as_str().context("team has no id")?.to_string();
            let team_key = team["key"].as_str().unwrap_or("").to_string();
            let team_name = team["name"].as_str().unwrap_or("").to_string();

            if team_id == legacy_team_id {
                // Preserve existing team's status IDs
                println!("  Keeping existing statuses for {team_key}");
                team_configs.push(legacy_team.clone());
            } else {
                // Discover statuses for new team
                let statuses = self.discover_team_statuses(&team_id)?;
                println!(
                    "  Discovered {team_name} ({team_key}) — {} statuses",
                    statuses.len()
                );
                team_configs.push(TeamConfig {
                    team_id,
                    team_key,
                    statuses,
                });
            }
        }

        let config = LinearConfig {
            teams: team_configs,
        };
        save_config(&config)?;
        let cfg_path = config_path()?;
        println!(
            "Migrated to multi-team format: {} team(s) — {}",
            config.teams.len(),
            cfg_path.display()
        );
        Ok(())
    }

    /// Count teams in the Linear workspace (cheap query, used for mismatch warning).
    fn count_workspace_teams(&self) -> Result<usize> {
        let data = self.query("{ teams { nodes { id } } }", &json!({}))?;
        let teams = data["teams"]["nodes"]
            .as_array()
            .context("no teams found")?;
        Ok(teams.len())
    }

    /// Discover all workspace teams and save config. Shared by setup() and FORCE_SETUP path.
    fn discover_and_save_all_teams(&self) -> Result<()> {
        println!("Discovering Linear workspace...");

        let data = self.query("{ teams { nodes { id key name } } }", &json!({}))?;
        let api_teams = data["teams"]["nodes"]
            .as_array()
            .context("no teams found")?;

        if api_teams.is_empty() {
            bail!("no teams found in Linear workspace");
        }

        println!("Found {} team(s):", api_teams.len());
        for t in api_teams {
            let name = t["name"].as_str().unwrap_or("?");
            let key = t["key"].as_str().unwrap_or("?");
            println!("  {name} ({key})");
        }

        let mut team_configs = Vec::new();
        for team in api_teams {
            let team_id = team["id"].as_str().context("team has no id")?.to_string();
            let team_key = team["key"].as_str().unwrap_or("").to_string();
            let team_name = team["name"].as_str().unwrap_or("").to_string();

            let statuses = self.discover_team_statuses(&team_id)?;

            println!("\n{team_name} ({team_key}) — {} statuses:", statuses.len());
            for (name, id) in &statuses {
                println!("  {name}: {id}");
            }

            team_configs.push(TeamConfig {
                team_id,
                team_key,
                statuses,
            });
        }

        let config = LinearConfig {
            teams: team_configs,
        };

        save_config(&config)?;
        let cfg_path = config_path()?;
        println!("\nConfig saved to {}", cfg_path.display());

        Ok(())
    }

    /// Discover workflow statuses for a single team. Extracted from setup() for reuse.
    pub(super) fn discover_team_statuses(
        &self,
        team_id: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let states_query = r#"
            query($teamId: ID!) {
                workflowStates(filter: { team: { id: { eq: $teamId } } }) {
                    nodes { id name type }
                }
            }
        "#;
        let states_data = self.query(states_query, &json!({"teamId": team_id}))?;
        let states = states_data["workflowStates"]["nodes"]
            .as_array()
            .context("no workflow states")?;

        let mut statuses = std::collections::HashMap::new();

        let find_by_name = |name: &str| -> Option<String> {
            states
                .iter()
                .find(|s| {
                    s["name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
                .and_then(|s| s["id"].as_str().map(String::from))
        };

        let find_by_type = |stype: &str| -> Option<String> {
            states
                .iter()
                .find(|s| s["type"].as_str().is_some_and(|t| t == stype))
                .and_then(|s| s["id"].as_str().map(String::from))
        };

        if let Some(id) = find_by_type("backlog") {
            statuses.insert("backlog".to_string(), id);
        }
        if let Some(id) = find_by_type("unstarted") {
            statuses.insert("todo".to_string(), id);
        }
        if let Some(id) = find_by_type("completed") {
            statuses.insert("done".to_string(), id);
        }
        if let Some(id) = find_by_type("canceled") {
            statuses.insert("canceled".to_string(), id);
        }
        if let Some(id) = find_by_name("Blocked") {
            statuses.insert("blocked".to_string(), id);
        }
        if let Some(id) = find_by_name("In Progress") {
            statuses.insert("in_progress".to_string(), id);
        }
        if let Some(id) = find_by_name("In Review").or_else(|| find_by_name("Review")) {
            statuses.insert("review".to_string(), id);
        }
        if let Some(id) = find_by_name("QA") {
            statuses.insert("qa".to_string(), id);
        }
        if let Some(id) = find_by_name("Ready").or_else(|| find_by_name("Ready for Deploy")) {
            statuses.insert("ready".to_string(), id);
        }
        if let Some(id) = find_by_name("Deploy").or_else(|| find_by_name("Deploying")) {
            statuses.insert("deploy".to_string(), id);
        }
        if let Some(id) = find_by_name("Failed").or_else(|| find_by_name("Deploy Failed")) {
            statuses.insert("failed".to_string(), id);
        }

        Ok(statuses)
    }
}
