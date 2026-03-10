pub mod config;
pub mod executor;
pub mod loader;
pub mod prompt;
pub mod verdict;

// Re-export the public API that daemon.rs and main.rs call.
pub use executor::{callback, poll};

// ─── Research pipeline (unchanged from old pipeline.rs) ──────────────────────

use anyhow::Result;

use crate::db::Db;
use crate::linear::LinearClient;
use crate::models::{Status, Task};

/// Parse OUTPUT_FILE=<path> from research task output.
pub fn parse_output_file(result: &str) -> Option<String> {
    for line in result.lines().rev() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("OUTPUT_FILE=") {
            let path = path.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Handle research task completion: create curator follow-up and move issue to Done.
pub fn handle_research_completion(db: &Db, task: &Task, output: &str) -> Result<()> {
    let linear = LinearClient::new()?;

    let output_file = parse_output_file(output).unwrap_or_default();

    let summary = extract_tldr(output);
    if !summary.is_empty() {
        linear.comment(
            &task.linear_issue_id,
            &format!(
                "**Research completed** (task: `{}`)\n\n{}\n\n{}",
                task.id,
                summary,
                if output_file.is_empty() {
                    String::new()
                } else {
                    format!("File: `{}`", output_file)
                }
            ),
        )?;
    }

    if !output_file.is_empty() {
        let curator_prompt = format!(
            "# Research Curator\n\n\
             ## Input\n\
             Research file: {}\n\
             Linear issue: {}\n\n\
             ## Tasks\n\
             1. Read the research file\n\
             2. Extract key topics/entities (libraries, patterns, strategies, tools)\n\
             3. Search for related research files in docs/research/\n\
             4. Check if findings update any existing memory files in ~/.claude/projects/*/memory/\n\
             5. Output: CURATOR_VERDICT=DONE or CURATOR_VERDICT=SKIPPED (nothing to link)",
            output_file, task.linear_issue_id
        );

        let curator_id = db.next_task_id()?;
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

        let curator_task = Task {
            id: curator_id.clone(),
            status: Status::Pending,
            priority: 3,
            created_at: now,
            started_at: None,
            finished_at: None,
            task_type: "research-curator".to_string(),
            prompt: curator_prompt,
            output_path: String::new(),
            working_dir: task.working_dir.clone(),
            model: "haiku".to_string(),
            max_turns: crate::default_turns("research-curator"),
            allowed_tools: crate::runner::tools_for_type("research-curator", false),
            session_id: String::new(),
            linear_issue_id: task.linear_issue_id.clone(),
            linear_pushed: false,
            pipeline_stage: String::new(),
            depends_on: vec![task.id.clone()],
            context_files: vec![output_file],
            repo_hash: crate::runtime_repo_hash(),
            estimate: 0,
        };

        db.insert_task(&curator_task)?;
        println!("  + curator task: {} for research {}", curator_id, task.id);
    }

    let _ = linear.move_issue_by_name(&task.linear_issue_id, "done");

    Ok(())
}

/// Extract TL;DR section from research output.
fn extract_tldr(text: &str) -> String {
    let mut in_tldr = false;
    let mut lines = Vec::new();

    for line in text.lines() {
        if line.trim().starts_with("## TL;DR") || line.trim().starts_with("## TLDR") {
            in_tldr = true;
            continue;
        }
        if in_tldr {
            if line.starts_with("## ") {
                break;
            }
            lines.push(line);
        }
    }

    let result = lines.join("\n").trim().to_string();
    if result.is_empty() {
        text.lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .take(5)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        result
    }
}

/// Show pipeline status: count issues at each stage.
pub fn status(db: &Db) -> Result<()> {
    println!("\nPipeline Status:");
    println!("================\n");

    let linear = match LinearClient::new() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("  WARNING: Linear not available — {e}");
            eprintln!("  Pipeline poll/sync disabled until LINEAR_API_KEY is set.\n");
            None
        }
    };

    if let Some(ref linear) = linear {
        let stages = [
            ("backlog", "Backlog"),
            ("blocked", "Blocked"),
            ("todo", "Todo"),
            ("in_progress", "In Progress"),
            ("review", "In Review"),
            ("ready", "Ready (awaiting merge)"),
            ("done", "Done"),
        ];

        for (key, label) in &stages {
            match linear.get_issues_by_status(key) {
                Ok(issues) => {
                    if !issues.is_empty() {
                        println!("  {} ({}): {} issues", label, key, issues.len());
                        for issue in &issues {
                            let id = issue["identifier"].as_str().unwrap_or("?");
                            let title = issue["title"].as_str().unwrap_or("?");
                            println!("    {} {}", id, title);
                        }
                    }
                }
                Err(_) => {
                    println!("  {} ({}): <error fetching>", label, key);
                }
            }
        }
    }

    println!("\nLocal pipeline tasks:");
    let config = loader::load_default()?;

    let pending = db.list_tasks(Some(Status::Pending))?;
    let running = db.list_tasks(Some(Status::Running))?;

    for (stage_name, _) in &config.stages {
        let stage_pending: Vec<_> = pending
            .iter()
            .filter(|t| &t.pipeline_stage == stage_name)
            .collect();
        let stage_running: Vec<_> = running
            .iter()
            .filter(|t| &t.pipeline_stage == stage_name)
            .collect();

        if !stage_pending.is_empty() || !stage_running.is_empty() {
            println!(
                "  {}: {} pending, {} running",
                stage_name,
                stage_pending.len(),
                stage_running.len()
            );
        }
    }

    println!();
    Ok(())
}

// ─── CLI commands ─────────────────────────────────────────────────────────────

/// `werma pipeline show [--stage <name>]` — pretty-print pipeline config.
pub fn cmd_show(stage_filter: Option<&str>) -> Result<()> {
    let config = loader::load_default()?;

    println!("\nPipeline: {} — {}", config.pipeline, config.description);

    if !config.templates.is_empty() {
        println!("\nTemplates:");
        for (k, v) in &config.templates {
            let preview: String = v.chars().take(60).collect();
            let ellipsis = if v.len() > 60 { "…" } else { "" };
            println!("  {k}: {preview}{ellipsis}");
        }
    }

    println!("\nStages:");
    for (name, stage) in &config.stages {
        if let Some(filter) = stage_filter
            && name != filter
        {
            continue;
        }

        let status_keys = stage.status_keys();
        let status_str = if status_keys.is_empty() {
            "(spawned only)".to_string()
        } else {
            status_keys.join(", ")
        };

        let manual_str = match stage.manual {
            config::ManualBehavior::Skip => "skip",
            config::ManualBehavior::Process => "process",
        };

        let prompt_str = match &stage.prompt {
            Some(p) if p.contains('\n') => "(inline)",
            Some(p) => p.as_str(),
            None => "(none)",
        };

        println!();
        println!("  {name}:");
        println!("    status:  {status_str}");
        println!("    agent:   {}", stage.agent);
        println!("    model:   {}", stage.model);
        println!("    manual:  {manual_str}");
        println!("    prompt:  {prompt_str}");

        if !stage.transitions.is_empty() {
            println!("    transitions:");
            for (verdict, t) in &stage.transitions {
                let spawn_str = t
                    .spawn
                    .as_deref()
                    .map(|s| format!(" + spawn:{s}"))
                    .unwrap_or_default();
                println!("      {verdict}: → {}{spawn_str}", t.status);
            }
        }
    }
    println!();
    Ok(())
}

/// `werma pipeline validate` — load + validate, report errors.
pub fn cmd_validate() -> Result<()> {
    match loader::load_default() {
        Ok(config) => {
            println!(
                "Pipeline '{}' is valid ({} stages).",
                config.pipeline,
                config.stages.len()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("Pipeline config invalid: {e}");
            Err(e)
        }
    }
}

/// `werma pipeline eject` — export builtin config to `~/.werma/pipelines/`.
pub fn cmd_eject() -> Result<()> {
    loader::eject()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_file_from_result() {
        let text = "Research complete.\nSaved to file.\nOUTPUT_FILE=/path/to/file.md";
        assert_eq!(
            parse_output_file(text),
            Some("/path/to/file.md".to_string())
        );

        let text2 = "OUTPUT_FILE=/old/path.md\nMore output\nOUTPUT_FILE=/new/path.md";
        assert_eq!(parse_output_file(text2), Some("/new/path.md".to_string()));

        assert_eq!(parse_output_file("Just some text"), None);
        assert_eq!(parse_output_file(""), None);
        assert_eq!(parse_output_file("OUTPUT_FILE="), None);
    }

    #[test]
    fn extract_tldr_section() {
        let text = "# Research\n\n## TL;DR\n\n- Point 1\n- Point 2\n\n## Findings\n\nDetails...";
        let tldr = extract_tldr(text);
        assert!(tldr.contains("Point 1"));
        assert!(tldr.contains("Point 2"));
        assert!(!tldr.contains("Details"));
    }

    #[test]
    fn extract_tldr_fallback() {
        let text = "First line of findings.\nSecond line.\nThird line.";
        let tldr = extract_tldr(text);
        assert!(tldr.contains("First line"));
    }
}
