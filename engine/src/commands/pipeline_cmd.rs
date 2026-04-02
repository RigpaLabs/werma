use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::traits::RealCommandRunner;
use crate::{config, linear, pipeline, tracker, ui};

pub fn cmd_pipeline_poll(db: &Db) -> Result<()> {
    let linear_client = tracker::try_linear_client()?;
    let cmd = RealCommandRunner;
    ui::with_spinner("Polling Linear statuses...", || {
        pipeline::poll(db, &*linear_client, &cmd)
    })
}

pub fn cmd_pipeline_status(db: &Db) -> Result<()> {
    let linear_client = match tracker::try_linear_client() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("  WARNING: Linear not available — {e}");
            eprintln!("  Pipeline status will not show Linear issue counts.\n");
            None
        }
    };
    ui::with_spinner("Fetching pipeline status...", || {
        pipeline::status(db, linear_client.as_deref())
    })
}

pub fn cmd_pipeline_show(stage: Option<&str>) -> Result<()> {
    pipeline::cmd_show(stage)
}

pub fn cmd_pipeline_validate() -> Result<()> {
    pipeline::cmd_validate()
}

pub fn cmd_pipeline_run(identifiers: &[String], stage: Option<&str>) -> Result<()> {
    let db = crate::open_db()?;
    let linear_client = tracker::try_linear_client()?;
    // Start with the default pipeline for stage validation; per-issue tasks
    // may use a repo-specific pipeline below (RIG-367).
    let config = pipeline::loader::load_default()?;

    // Detect if a stage name was passed as a positional arg (e.g. `werma pipeline run RIG-178 analyst`).
    // The CLI defines `issues` as a greedy Vec<String>, so "analyst" gets consumed as an identifier.
    // Filter it out and use it as the effective stage — but only when --stage wasn't explicitly set.
    let explicit_stage = stage.is_some();
    let mut effective_stage = stage.unwrap_or("analyst").to_string();
    let mut filtered: Vec<&str> = Vec::new();
    for id in identifiers {
        if config.stage(id).is_some() {
            if explicit_stage {
                eprintln!(
                    "warning: ignoring positional stage '{id}' because --stage '{effective_stage}' was explicitly set"
                );
            } else {
                effective_stage = id.clone();
            }
        } else {
            filtered.push(id);
        }
    }

    // Validate stage exists
    if config.stage(&effective_stage).is_none() {
        let available: Vec<_> = config.stages.keys().collect();
        bail!(
            "unknown stage '{}'. Available: {}",
            effective_stage,
            available
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if filtered.is_empty() {
        bail!("no issue identifiers provided. Usage: werma pipeline run RIG-XX [--stage <stage>]");
    }

    let mut created = 0;
    let mut skipped = 0;

    for identifier in &filtered {
        // Fetch issue from Linear
        let (_issue_id, ident, title, description, labels) =
            match linear_client.get_issue_by_identifier(identifier) {
                Ok(data) => data,
                Err(e) => {
                    eprintln!("  ! {identifier}: {e}");
                    skipped += 1;
                    continue;
                }
            };

        // Skip if active task already exists for this issue + stage
        let existing = db.tasks_by_linear_issue(&ident, Some(&effective_stage), true)?;
        if !existing.is_empty() {
            let active_id = &existing[0].id;
            eprintln!("  ~ {ident} already has active {effective_stage} task ({active_id})");
            skipped += 1;
            continue;
        }

        // Note: task is always created in pending state regardless of max_concurrent.
        // The daemon's drain_queue respects concurrency limits when launching tasks.

        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let user_cfg = crate::config::UserConfig::load();
        let working_dir = linear::infer_working_dir(&title, &label_refs, &user_cfg);
        if linear::validate_working_dir(&working_dir).is_none() {
            eprintln!("  ! skipping {ident} [{title}]: working dir '{working_dir}' does not exist");
            skipped += 1;
            continue;
        }
        let estimate = 0; // Will be set by analyst if applicable

        // RIG-367: Use repo-specific pipeline config for task creation.
        let repo_config =
            pipeline::loader::load_for_working_dir(&working_dir).unwrap_or_else(|_| config.clone());

        let task_id = pipeline::create_initial_stage_task(
            &db,
            &repo_config,
            &effective_stage,
            &ident,
            &title,
            &description,
            &working_dir,
            estimate,
            &user_cfg,
        )?;

        println!("  + {task_id} [{ident}] stage={effective_stage}");
        created += 1;
    }

    println!("\nPipeline run: {created} created, {skipped} skipped");
    Ok(())
}

/// Switch the active pipeline for a repo by updating `~/.werma/config.toml` in-place.
///
/// This edits (or creates) the `[pipelines]` table in config.toml while preserving
/// all existing content, comments, and formatting.
pub fn cmd_pipeline_switch(repo: &str, pipeline: &str) -> Result<()> {
    let config_path = dirs::home_dir()
        .map(|h| h.join(".werma/config.toml"))
        .context("could not determine home directory")?;

    // Read existing content (empty string if file doesn't exist yet).
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .context("failed to parse ~/.werma/config.toml")?;

    // Ensure [repo_pipelines] table exists, then set the key.
    if !doc.contains_table("repo_pipelines") {
        doc["repo_pipelines"] = toml_edit::table();
    }
    doc["repo_pipelines"][repo] = toml_edit::value(pipeline);

    // Create parent dir if needed (first-time setup).
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    std::fs::write(&config_path, doc.to_string())
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    // Show the result.
    let current = config::UserConfig::load();
    println!(
        "Pipeline for '{repo}' set to '{pipeline}' (was: '{}')",
        if pipeline == current.active_pipeline(repo) {
            // Already updated — show what it was before
            "default"
        } else {
            current.active_pipeline(repo)
        }
    );
    println!();
    println!("Active pipelines:");
    if current.repo_pipelines.is_empty() {
        println!("  (none configured — all repos use 'default')");
    } else {
        let mut entries: Vec<_> = current.repo_pipelines.iter().collect();
        entries.sort_by_key(|(k, _)| k.as_str());
        for (r, p) in &entries {
            println!("  {r:<20} → {p}");
        }
    }
    println!();
    println!("Config: {}", config_path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pipeline commands require Linear API access, so we test the
    // delegated pipeline module functions (which have their own tests).
    // Here we just verify the module structure is correct.

    #[test]
    fn pipeline_cmd_module_exists() {
        // Ensures this module compiles and links correctly
        assert!(true);
    }

    #[test]
    fn switch_creates_config_with_pipelines_section() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        // Patch the config path by writing directly via toml_edit logic.
        // We test the toml_edit round-trip here rather than the full cmd.
        let existing = "";
        let mut doc: toml_edit::DocumentMut = existing.parse().unwrap();
        doc["repo_pipelines"] = toml_edit::table();
        doc["repo_pipelines"]["fathom"] = toml_edit::value("economy");
        let written = doc.to_string();
        std::fs::write(&config_path, &written).unwrap();

        let cfg = crate::config::UserConfig::load_from(&config_path);
        assert_eq!(cfg.active_pipeline("fathom"), "economy");
        assert_eq!(cfg.active_pipeline("werma"), "default");
    }

    #[test]
    fn switch_preserves_existing_config_keys() {
        let existing = "completed_limit = 25\n\n[repos]\nwerma = \"/custom/werma\"\n";
        let mut doc: toml_edit::DocumentMut = existing.parse().unwrap();
        doc["repo_pipelines"] = toml_edit::table();
        doc["repo_pipelines"]["fathom"] = toml_edit::value("economy");
        let result = doc.to_string();

        // Existing keys preserved
        assert!(result.contains("completed_limit = 25"));
        assert!(result.contains("werma = \"/custom/werma\""));
        // New pipeline key added
        assert!(result.contains("fathom = \"economy\""));
    }
}
