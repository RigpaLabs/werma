use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::config::PipelineConfig;

/// Built-in default pipeline YAML compiled into the binary.
const BUILTIN_DEFAULT_YAML: &str = include_str!("../../pipelines/default.yaml");

/// Returns the runtime pipelines override directory: `~/.werma/pipelines/`.
pub fn runtime_pipelines_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".werma/pipelines"))
}

/// Load the pipeline config.
///
/// Priority (highest to lowest):
/// 1. `~/.werma/pipelines/default.yaml` (runtime override)
/// 2. Built-in `default.yaml` compiled into the binary
pub fn load_default() -> Result<PipelineConfig> {
    // Try runtime override first.
    if let Some(runtime_dir) = runtime_pipelines_dir() {
        let override_path = runtime_dir.join("default.yaml");
        if override_path.exists() {
            let config = load_from_file(&override_path)?;
            warn_deprecated_per_stage(&config);
            return Ok(config);
        }
    }

    // Fall back to builtin.
    let config = load_from_str(BUILTIN_DEFAULT_YAML, "<builtin>")?;
    warn_deprecated_per_stage(&config);
    Ok(config)
}

/// Load a pipeline config from a YAML file path.
pub fn load_from_file(path: &Path) -> Result<PipelineConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read pipeline config: {}", path.display()))?;
    load_from_str(&content, &path.display().to_string())
}

/// Parse a pipeline config from a YAML string.
pub fn load_from_str(yaml: &str, source: &str) -> Result<PipelineConfig> {
    let config: PipelineConfig = serde_yaml::from_str(yaml)
        .with_context(|| format!("failed to parse pipeline config from {source}"))?;
    validate(&config, source)?;
    Ok(config)
}

/// Warn if any stage still has the deprecated per-stage max_concurrent field set.
fn warn_deprecated_per_stage(config: &PipelineConfig) {
    for (name, stage) in &config.stages {
        if stage.max_concurrent.is_some() {
            eprintln!(
                "warning: stage '{}' has per-stage max_concurrent (deprecated, ignored). Use pipeline-level max_concurrent: {}",
                name, config.max_concurrent
            );
        }
    }
}

/// Known model short names accepted in pipeline YAML.
const VALID_MODELS: &[&str] = &["opus", "sonnet", "haiku"];

/// Validate that the config is internally consistent.
pub fn validate(config: &PipelineConfig, source: &str) -> Result<()> {
    for (stage_name, stage) in &config.stages {
        // Validate model names.
        if !VALID_MODELS.contains(&stage.model.as_str()) {
            anyhow::bail!(
                "pipeline config {source}: stage '{stage_name}' has unknown model '{}' \
                 (expected one of: {})",
                stage.model,
                VALID_MODELS.join(", ")
            );
        }
        if let Some(ref fallback) = stage.fallback
            && !VALID_MODELS.contains(&fallback.as_str())
        {
            anyhow::bail!(
                "pipeline config {source}: stage '{stage_name}' has unknown fallback model '{}' \
                 (expected one of: {})",
                fallback,
                VALID_MODELS.join(", ")
            );
        }

        // Validate spawn targets exist.
        for (verdict, transition) in &stage.transitions {
            if let Some(ref spawn_target) = transition.spawn
                && !config.stages.contains_key(spawn_target.as_str())
            {
                anyhow::bail!(
                    "pipeline config {source}: stage '{stage_name}' transition '{verdict}' \
                     spawns '{spawn_target}' which is not a defined stage"
                );
            }
        }
    }
    Ok(())
}

/// Resolve a prompt source to its rendered content string.
///
/// - If `prompt_source` contains a newline → it's an inline prompt, return as-is.
/// - Otherwise → treat as a file path relative to the runtime pipelines dir (if it
///   exists) or the builtin pipelines embed map. Falls back to empty string with a
///   warning if not found.
pub fn resolve_prompt(prompt_source: &str) -> String {
    let trimmed = prompt_source.trim();

    // Inline: contains newline (YAML block scalar or explicit \n)
    if trimmed.contains('\n') {
        return trimmed.to_string();
    }

    // File path: try runtime override first, then builtin.
    let rel_path = trimmed;

    if let Some(runtime_dir) = runtime_pipelines_dir() {
        let full_path = runtime_dir.join(rel_path);
        if full_path.exists() {
            match std::fs::read_to_string(&full_path) {
                Ok(content) => return content,
                Err(e) => {
                    eprintln!(
                        "warning: failed to read prompt file {}: {e}",
                        full_path.display()
                    );
                }
            }
        }
    }

    // Builtin file embeds — resolved at compile time via a match.
    if let Some(content) = builtin_prompt(rel_path) {
        return content.to_string();
    }

    eprintln!("warning: prompt file not found: {rel_path}");
    String::new()
}

/// Returns builtin prompt file content by relative path.
/// These are compiled into the binary via include_str!.
fn builtin_prompt(rel_path: &str) -> Option<&'static str> {
    match rel_path {
        "prompts/engineer.md" => Some(include_str!("../../pipelines/prompts/engineer.md")),
        "prompts/reviewer.md" => Some(include_str!("../../pipelines/prompts/reviewer.md")),
        "prompts/qa.md" => Some(include_str!("../../pipelines/prompts/qa.md")),
        "prompts/devops.md" => Some(include_str!("../../pipelines/prompts/devops.md")),
        _ => None,
    }
}

/// Export the builtin config + prompt files to `~/.werma/pipelines/`.
pub fn eject() -> Result<()> {
    let runtime_dir = runtime_pipelines_dir().context("cannot determine home directory")?;
    let prompts_dir = runtime_dir.join("prompts");

    std::fs::create_dir_all(&prompts_dir)
        .with_context(|| format!("failed to create {}", prompts_dir.display()))?;

    // Write default.yaml
    let config_path = runtime_dir.join("default.yaml");
    std::fs::write(&config_path, BUILTIN_DEFAULT_YAML)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    println!("wrote: {}", config_path.display());

    // Write each builtin prompt file.
    let prompts: &[(&str, &str)] = &[
        (
            "prompts/engineer.md",
            include_str!("../../pipelines/prompts/engineer.md"),
        ),
        (
            "prompts/reviewer.md",
            include_str!("../../pipelines/prompts/reviewer.md"),
        ),
        (
            "prompts/qa.md",
            include_str!("../../pipelines/prompts/qa.md"),
        ),
        (
            "prompts/devops.md",
            include_str!("../../pipelines/prompts/devops.md"),
        ),
    ];

    for (rel, content) in prompts {
        let path = runtime_dir.join(rel);
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("wrote: {}", path.display());
    }

    println!("\nPipeline config ejected to: {}", runtime_dir.display());
    println!("Edit files there to override the builtin pipeline.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_builtin_succeeds() {
        let config = load_from_str(BUILTIN_DEFAULT_YAML, "<test>").unwrap();
        assert_eq!(config.pipeline, "default");
        assert!(!config.stages.is_empty());
    }

    #[test]
    fn load_from_str_invalid_yaml_errors() {
        let result = load_from_str("not: valid: yaml: {{{", "<test>");
        assert!(result.is_err());
    }

    #[test]
    fn validate_valid_config_passes() {
        let config = load_from_str(BUILTIN_DEFAULT_YAML, "<test>").unwrap();
        // validate is called inside load_from_str — if we got here, it passed
        let _ = config;
    }

    #[test]
    fn validate_invalid_model_fails() {
        let yaml = r#"
pipeline: bad
stages:
  test:
    agent: pipeline-test
    model: gpt4
"#;
        let result = load_from_str(yaml, "<test>");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown model 'gpt4'"));
    }

    #[test]
    fn validate_invalid_fallback_model_fails() {
        let yaml = r#"
pipeline: bad
stages:
  test:
    agent: pipeline-test
    model: opus
    fallback: typo
"#;
        let result = load_from_str(yaml, "<test>");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown fallback model 'typo'"));
    }

    #[test]
    fn validate_invalid_spawn_target_fails() {
        let yaml = r#"
pipeline: bad
stages:
  reviewer:
    linear_status: review
    agent: pipeline-reviewer
    model: sonnet
    transitions:
      rejected:
        status: in_progress
        spawn: nonexistent
"#;
        let result = load_from_str(yaml, "<test>");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("nonexistent"));
    }

    #[test]
    fn resolve_prompt_inline_with_newline() {
        let prompt = "line one\nline two\n";
        let resolved = resolve_prompt(prompt);
        assert_eq!(resolved, prompt.trim());
    }

    #[test]
    fn resolve_prompt_unknown_file_returns_empty() {
        // A path that doesn't exist in builtin or runtime → empty + warning
        let resolved = resolve_prompt("prompts/nonexistent.md");
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_builtin_reviewer_prompt() {
        let content = builtin_prompt("prompts/reviewer.md");
        assert!(content.is_some());
        assert!(!content.unwrap().is_empty());
    }

    #[test]
    fn resolve_builtin_qa_prompt() {
        let content = builtin_prompt("prompts/qa.md");
        assert!(content.is_some());
    }

    #[test]
    fn resolve_builtin_devops_prompt() {
        let content = builtin_prompt("prompts/devops.md");
        assert!(content.is_some());
    }

    #[test]
    fn runtime_pipelines_dir_is_some() {
        // Should always succeed on a system with a home dir
        let dir = runtime_pipelines_dir();
        assert!(dir.is_some());
        let path = dir.unwrap();
        assert!(path.ends_with(".werma/pipelines"));
    }

    #[test]
    fn load_from_file_missing_returns_error() {
        let result = load_from_file(Path::new("/tmp/nonexistent-werma-pipeline.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn load_from_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, BUILTIN_DEFAULT_YAML).unwrap();
        let config = load_from_file(&path).unwrap();
        assert_eq!(config.pipeline, "default");
    }
}
