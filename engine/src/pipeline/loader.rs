use anyhow::{Context, Result};

use super::config::PipelineConfig;

/// Built-in default pipeline YAML compiled into the binary.
const BUILTIN_DEFAULT_YAML: &str = include_str!("../../pipelines/default.yaml");

/// Built-in economy pipeline YAML compiled into the binary.
const BUILTIN_ECONOMY_YAML: &str = include_str!("../../pipelines/economy.yaml");

/// Built-in honeyjourney pipeline YAML compiled into the binary.
const BUILTIN_HONEYJOURNEY_YAML: &str = include_str!("../../pipelines/honeyjourney.yaml");

/// Load the pipeline config (always uses the compiled-in builtin).
pub fn load_default() -> Result<PipelineConfig> {
    warn_stale_runtime_override();
    let config = load_from_str(BUILTIN_DEFAULT_YAML, "<builtin>")?;
    warn_deprecated_per_stage(&config);
    Ok(config)
}

/// Load a pipeline config by name.
///
/// Lookup order:
/// 1. Builtin names: `"default"` → compiled-in default, `"economy"` → compiled-in economy.
/// 2. File on disk: `~/.werma/pipelines/{name}.yaml` (user-defined pipelines).
/// 3. Falls back to the builtin default with a warning if no file is found.
pub fn load_named(name: &str) -> Result<PipelineConfig> {
    // Check builtins first.
    match name {
        "default" => return load_default(),
        "economy" => {
            let config = load_from_str(BUILTIN_ECONOMY_YAML, "<builtin:economy>")?;
            warn_deprecated_per_stage(&config);
            return Ok(config);
        }
        "honeyjourney" => {
            let config = load_from_str(BUILTIN_HONEYJOURNEY_YAML, "<builtin:honeyjourney>")?;
            warn_deprecated_per_stage(&config);
            return Ok(config);
        }
        _ => {}
    }

    // Try user-defined pipeline file.
    if let Some(home) = dirs::home_dir() {
        let path = home.join(format!(".werma/pipelines/{name}.yaml"));
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read pipeline {}", path.display()))?;
            let config = load_from_str(&content, &path.display().to_string())?;
            warn_deprecated_per_stage(&config);
            return Ok(config);
        }
    }

    eprintln!(
        "warning: pipeline '{name}' not found in ~/.werma/pipelines/{name}.yaml — using 'default'"
    );
    load_default()
}

/// Load the pipeline config for a specific working directory, consulting
/// `UserConfig::pipeline_for_repo()` to select the right pipeline name.
pub fn load_for_working_dir(working_dir: &str) -> Result<PipelineConfig> {
    let user_cfg = crate::config::UserConfig::load();
    let repo_label = user_cfg.repo_label_from_dir(working_dir);
    let pipeline_name = repo_label
        .as_deref()
        .map(|r| user_cfg.pipeline_for_repo(r))
        .unwrap_or("default");
    load_named(pipeline_name)
}

/// Warn once if a stale runtime override exists from a previous `werma pipeline eject`.
fn warn_stale_runtime_override() {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    if let Some(home) = dirs::home_dir() {
        let stale = home.join(".werma/pipelines/default.yaml");
        if stale.exists() {
            WARNED.call_once(|| {
                eprintln!(
                    "warning: stale pipeline override found at {}. \
                     Runtime overrides are no longer supported — this file is ignored. \
                     Delete it to silence this warning.",
                    stale.display()
                );
            });
        }
    }
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
/// - Otherwise → treat as a builtin prompt file path. Falls back to empty string
///   with a warning if not found.
pub fn resolve_prompt(prompt_source: &str) -> String {
    let trimmed = prompt_source.trim();

    // Inline: contains newline (YAML block scalar or explicit \n)
    if trimmed.contains('\n') {
        return trimmed.to_string();
    }

    // Builtin file embeds — resolved at compile time via a match.
    if let Some(content) = builtin_prompt(trimmed) {
        return content.to_string();
    }

    eprintln!("warning: prompt file not found: {trimmed}");
    String::new()
}

/// Returns builtin prompt file content by relative path.
/// These are compiled into the binary via include_str!.
fn builtin_prompt(rel_path: &str) -> Option<&'static str> {
    match rel_path {
        "prompts/analyst.md" => Some(include_str!("../../pipelines/prompts/analyst.md")),
        "prompts/engineer.md" => Some(include_str!("../../pipelines/prompts/engineer.md")),
        "prompts/reviewer.md" => Some(include_str!("../../pipelines/prompts/reviewer.md")),
        "prompts/qa.md" => Some(include_str!("../../pipelines/prompts/qa.md")),
        "prompts/devops.md" => Some(include_str!("../../pipelines/prompts/devops.md")),
        "prompts/deployer.md" => Some(include_str!("../../pipelines/prompts/deployer.md")),
        _ => None,
    }
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
    fn load_named_default_succeeds() {
        let config = load_named("default").unwrap();
        assert_eq!(config.pipeline, "default");
        assert!(!config.stages.is_empty());
    }

    #[test]
    fn load_named_economy_succeeds() {
        let config = load_named("economy").unwrap();
        assert_eq!(config.pipeline, "economy");
        assert!(!config.stages.is_empty());
        // Economy pipeline engineer uses codex runtime
        let engineer = config.stage("engineer").unwrap();
        assert_eq!(engineer.runtime, Some(crate::models::AgentRuntime::Codex));
    }

    #[test]
    fn load_named_unknown_falls_back_to_default() {
        // Non-builtin names without a matching ~/.werma/pipelines/{name}.yaml
        // fall back to the default pipeline with a warning (no error).
        let result = load_named("nonexistent-pipeline");
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.pipeline, "default");
    }

    #[test]
    fn economy_pipeline_stages_match_default() {
        // Economy pipeline should have the same stage names as default
        let default = load_named("default").unwrap();
        let economy = load_named("economy").unwrap();
        let default_stages: Vec<&str> = default.stages.keys().map(String::as_str).collect();
        let economy_stages: Vec<&str> = economy.stages.keys().map(String::as_str).collect();
        assert_eq!(default_stages, economy_stages);
    }

    #[test]
    fn economy_pipeline_uses_sonnet_for_analyst() {
        let config = load_named("economy").unwrap();
        let analyst = config.stage("analyst").unwrap();
        assert_eq!(analyst.model, "sonnet");
    }

    #[test]
    fn economy_pipeline_reviewer_uses_codex() {
        let config = load_named("economy").unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        assert_eq!(reviewer.runtime, Some(crate::models::AgentRuntime::Codex));
    }

    #[test]
    fn economy_pipeline_deployer_uses_codex() {
        let config = load_named("economy").unwrap();
        let deployer = config.stage("deployer").unwrap();
        assert_eq!(deployer.runtime, Some(crate::models::AgentRuntime::Codex));
    }

    #[test]
    fn load_named_honeyjourney_succeeds() {
        let config = load_named("honeyjourney").unwrap();
        assert_eq!(config.pipeline, "honeyjourney");
        assert!(!config.stages.is_empty());
    }

    #[test]
    fn honeyjourney_pipeline_engineer_uses_qwen() {
        let config = load_named("honeyjourney").unwrap();
        let engineer = config.stage("engineer").unwrap();
        assert_eq!(
            engineer.runtime,
            Some(crate::models::AgentRuntime::QwenCode)
        );
    }

    #[test]
    fn honeyjourney_pipeline_reviewer_uses_gemini() {
        let config = load_named("honeyjourney").unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        assert_eq!(
            reviewer.runtime,
            Some(crate::models::AgentRuntime::GeminiCli)
        );
    }

    #[test]
    fn honeyjourney_pipeline_analyst_uses_claude_code() {
        let config = load_named("honeyjourney").unwrap();
        let analyst = config.stage("analyst").unwrap();
        assert_eq!(analyst.runtime, None); // no runtime override = claude-code default
        assert_eq!(analyst.model, "sonnet");
    }

    #[test]
    fn honeyjourney_pipeline_deployer_uses_claude_code() {
        let config = load_named("honeyjourney").unwrap();
        let deployer = config.stage("deployer").unwrap();
        assert_eq!(deployer.runtime, None); // no runtime override = claude-code default
        assert_eq!(deployer.model, "sonnet");
    }

    #[test]
    fn honeyjourney_pipeline_stages_match_default() {
        let default = load_named("default").unwrap();
        let honeyjourney = load_named("honeyjourney").unwrap();
        let default_stages: Vec<&str> = default.stages.keys().map(String::as_str).collect();
        let honeyjourney_stages: Vec<&str> =
            honeyjourney.stages.keys().map(String::as_str).collect();
        assert_eq!(default_stages, honeyjourney_stages);
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
    fn resolve_builtin_analyst_prompt() {
        let content = builtin_prompt("prompts/analyst.md");
        assert!(content.is_some());
        assert!(!content.unwrap().is_empty());
    }

    #[test]
    fn resolve_builtin_devops_prompt() {
        let content = builtin_prompt("prompts/devops.md");
        assert!(content.is_some());
    }

    #[test]
    fn load_named_from_file() {
        let home = dirs::home_dir().unwrap();
        let pipelines_dir = home.join(".werma/pipelines");
        std::fs::create_dir_all(&pipelines_dir).unwrap();

        let economy_path = pipelines_dir.join("test-economy.yaml");
        let yaml = r#"
pipeline: test-economy
stages:
  engineer:
    agent: pipeline-engineer
    model: sonnet
"#;
        std::fs::write(&economy_path, yaml).unwrap();

        let config = load_named("test-economy").unwrap();
        assert_eq!(config.pipeline, "test-economy");
        assert!(config.stages.contains_key("engineer"));

        // Cleanup
        let _ = std::fs::remove_file(&economy_path);
    }

    #[test]
    fn load_for_working_dir_uses_default_when_no_override() {
        // With no pipeline override configured, should return default.
        let config = load_for_working_dir("~/projects/some-repo").unwrap();
        assert_eq!(config.pipeline, "default");
    }
}
