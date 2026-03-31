use anyhow::{Result, bail};

use crate::db::Db;
use crate::traits::RealCommandRunner;
use crate::{linear, pipeline, ui};

pub fn cmd_pipeline_poll(db: &Db) -> Result<()> {
    let linear_client = linear::LinearClient::new()?;
    let cmd = RealCommandRunner;
    ui::with_spinner("Polling Linear statuses...", || {
        pipeline::poll(db, &linear_client, &cmd)
    })
}

pub fn cmd_pipeline_status(db: &Db) -> Result<()> {
    let linear_client = match linear::LinearClient::new() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("  WARNING: Linear not available — {e}");
            eprintln!("  Pipeline status will not show Linear issue counts.\n");
            None
        }
    };
    ui::with_spinner("Fetching pipeline status...", || {
        pipeline::status(
            db,
            linear_client
                .as_ref()
                .map(|c| c as &dyn pipeline::LinearApi),
        )
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
    let linear_client = linear::LinearClient::new()?;
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

        let task_id = pipeline::create_initial_stage_task(
            &db,
            &config,
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

#[cfg(test)]
mod tests {
    // Pipeline commands require Linear API access, so we test the
    // delegated pipeline module functions (which have their own tests).
    // Here we just verify the module structure is correct.

    #[test]
    fn pipeline_cmd_module_exists() {
        // Ensures this module compiles and links correctly
        assert!(true);
    }
}
