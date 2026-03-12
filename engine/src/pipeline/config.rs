use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Default global max concurrent pipeline tasks.
pub const DEFAULT_GLOBAL_MAX_CONCURRENT: u32 = 5;

fn default_global_max_concurrent() -> u32 {
    DEFAULT_GLOBAL_MAX_CONCURRENT
}

/// Top-level pipeline configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PipelineConfig {
    pub pipeline: String,
    #[serde(default)]
    pub description: String,
    /// Global limit on total concurrent pipeline tasks (across all stages).
    #[serde(default = "default_global_max_concurrent")]
    pub max_concurrent: u32,
    /// Reusable template snippets available as `{key}` in all prompts.
    #[serde(default)]
    pub templates: IndexMap<String, String>,
    /// Ordered map of stage name → stage config.
    pub stages: IndexMap<String, StageConfig>,
}

/// Configuration for a single pipeline stage.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StageConfig {
    /// One or more Linear status keys that trigger polling for this stage.
    /// Absent means this stage is spawned-only (not polled directly).
    #[serde(default)]
    pub linear_status: Option<OneOrMany>,
    /// Agent type string (e.g. "pipeline-reviewer").
    pub agent: String,
    /// Model short name: "opus" | "sonnet" | "haiku".
    pub model: String,
    /// Fallback model if primary is rate-limited.
    #[serde(default)]
    pub fallback: Option<String>,
    /// Deprecated: per-stage max_concurrent is ignored. Use pipeline-level max_concurrent.
    /// Kept for backward compatibility with existing YAML configs.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    /// How to handle issues with the `manual` label.
    #[serde(default)]
    pub manual: ManualBehavior,
    /// Prompt: inline (contains '\n') or file path relative to pipelines dir.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Verdict → transition mapping.
    #[serde(default)]
    pub transitions: IndexMap<String, Transition>,

    // ─── Cost optimization fields (RIG-183) ─────────────────────────────
    /// Model for re-review rounds (2+). Uses cheaper model to verify fixes.
    #[serde(default)]
    pub recheck_model: Option<String>,
    /// Max review rejection cycles before escalating to Blocked.
    #[serde(default)]
    pub max_review_rounds: Option<u32>,
    /// Max turns passed to `claude --max-turns`. Overrides default_turns().
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Lighter model for low-SP tasks (e.g. sonnet instead of opus).
    #[serde(default)]
    pub light_model: Option<String>,
    /// SP threshold: if task estimate <= this, use light_model. Default: 2.
    #[serde(default)]
    pub light_threshold: Option<u32>,
}

/// Behavior when processing an issue that has the `manual` label.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ManualBehavior {
    /// Skip this stage entirely for manual issues (execution stages).
    #[default]
    Skip,
    /// Process manual issues the same as agent issues (review/qa stages).
    Process,
}

/// A pipeline transition triggered by a verdict.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Transition {
    /// Linear status to move the issue to.
    pub status: String,
    /// Optional: name of the next stage to spawn a task for.
    #[serde(default)]
    pub spawn: Option<String>,
}

/// Deserializes either a single string or an array of strings.
#[derive(Debug, Clone, Serialize)]
pub struct OneOrMany(pub Vec<String>);

impl<'de> Deserialize<'de> for OneOrMany {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OneOrMany;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a string or list of strings")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<OneOrMany, E> {
                Ok(OneOrMany(vec![v.to_string()]))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<OneOrMany, A::Error> {
                let mut vals = Vec::new();
                while let Some(v) = seq.next_element::<String>()? {
                    vals.push(v);
                }
                Ok(OneOrMany(vals))
            }
        }
        d.deserialize_any(Visitor)
    }
}

impl PipelineConfig {
    /// Returns all Linear status keys mapped by stage name (only polled stages).
    pub fn poll_stages(&self) -> Vec<(&str, &StageConfig)> {
        self.stages
            .iter()
            .filter(|(_, s)| s.linear_status.is_some())
            .map(|(name, s)| (name.as_str(), s))
            .collect()
    }

    /// Look up a stage by name.
    pub fn stage(&self, name: &str) -> Option<&StageConfig> {
        self.stages.get(name)
    }

    /// Find which stages handle a given Linear status key.
    /// Returns all matches (stages can share a status key via OneOrMany).
    #[allow(dead_code)]
    pub fn stage_for_status(&self, status_key: &str) -> Vec<(&str, &StageConfig)> {
        self.stages
            .iter()
            .filter(|(_, s)| {
                s.linear_status
                    .as_ref()
                    .is_some_and(|om| om.0.iter().any(|k| k == status_key))
            })
            .map(|(name, s)| (name.as_str(), s))
            .collect()
    }
}

impl StageConfig {
    /// Returns true if this stage should be skipped for manual issues.
    pub fn skip_manual(&self) -> bool {
        self.manual == ManualBehavior::Skip
    }

    /// Find the transition for a given verdict (case-insensitive).
    pub fn transition_for(&self, verdict: &str) -> Option<&Transition> {
        let v = verdict.to_lowercase();
        self.transitions.get(&v)
    }

    /// Returns all Linear status keys for this stage (empty vec if not polled).
    pub fn status_keys(&self) -> Vec<&str> {
        self.linear_status
            .as_ref()
            .map(|om| om.0.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Pick the effective model for this stage given the task context.
    ///
    /// - For reviewer stages: uses `recheck_model` on round 2+ (re-reviews).
    /// - For engineer stages: uses `light_model` for low-SP tasks.
    /// - Falls back to `self.model` otherwise.
    pub fn effective_model(&self, estimate: i32, review_round: i64) -> &str {
        // Re-review: cheaper model to just verify fixes
        if review_round >= 1
            && let Some(ref m) = self.recheck_model
        {
            return m;
        }
        // Light model for simple tasks
        let threshold = self.light_threshold.unwrap_or(2) as i32;
        if estimate > 0
            && estimate <= threshold
            && let Some(ref m) = self.light_model
        {
            return m;
        }
        &self.model
    }

    /// Returns the configured max_review_rounds, or None if unlimited.
    pub fn review_round_limit(&self) -> Option<u32> {
        self.max_review_rounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_yaml() -> &'static str {
        r#"
pipeline: default
description: Test pipeline

templates:
  verdict_instruction: "Output verdict on the LAST line"

stages:
  analyst:
    linear_status: todo
    agent: pipeline-analyst
    model: opus
    manual: skip
    prompt: |
      Analyze issue {issue_id}: {issue_title}
    transitions:
      done:
        status: in_progress
        spawn: engineer

  engineer:
    agent: pipeline-engineer
    model: opus
    fallback: sonnet    light_model: sonnet
    light_threshold: 2
    max_turns: 40
    manual: skip
    transitions:
      done:
        status: review

  reviewer:
    linear_status: review
    agent: pipeline-reviewer
    model: sonnet
    recheck_model: haiku
    max_review_rounds: 3
    max_turns: 15
    manual: process
    prompt: prompts/reviewer.md
    transitions:
      approved:
        status: qa
      rejected:
        status: in_progress
        spawn: engineer

  devops:
    linear_status: [ready, deploy]
    agent: pipeline-devops
    model: sonnet
    manual: skip
    transitions:
      done:
        status: done
      failed:
        status: failed
"#
    }

    #[test]
    fn deserialize_full_config() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        assert_eq!(config.pipeline, "default");
        assert_eq!(config.stages.len(), 4);
    }

    #[test]
    fn one_or_many_single_string() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let analyst = config.stage("analyst").unwrap();
        let keys = analyst.status_keys();
        assert_eq!(keys, vec!["todo"]);
    }

    #[test]
    fn one_or_many_array() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let devops = config.stage("devops").unwrap();
        let keys = devops.status_keys();
        assert_eq!(keys, vec!["ready", "deploy"]);
    }

    #[test]
    fn spawned_only_stage_not_polled() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let engineer = config.stage("engineer").unwrap();
        assert!(engineer.linear_status.is_none());
        assert!(engineer.status_keys().is_empty());
    }

    #[test]
    fn poll_stages_excludes_spawned_only() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let polled: Vec<_> = config.poll_stages();
        let names: Vec<&str> = polled.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"analyst"));
        assert!(names.contains(&"reviewer"));
        assert!(names.contains(&"devops"));
        assert!(!names.contains(&"engineer")); // spawned only
    }

    #[test]
    fn manual_behavior_defaults_to_skip() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let analyst = config.stage("analyst").unwrap();
        assert_eq!(analyst.manual, ManualBehavior::Skip);
        assert!(analyst.skip_manual());
    }

    #[test]
    fn manual_behavior_process() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        assert_eq!(reviewer.manual, ManualBehavior::Process);
        assert!(!reviewer.skip_manual());
    }

    #[test]
    fn transition_for_verdict_case_insensitive() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let reviewer = config.stage("reviewer").unwrap();

        let t = reviewer.transition_for("APPROVED").unwrap();
        assert_eq!(t.status, "qa");
        assert!(t.spawn.is_none());

        let t2 = reviewer.transition_for("rejected").unwrap();
        assert_eq!(t2.status, "in_progress");
        assert_eq!(t2.spawn.as_deref(), Some("engineer"));
    }

    #[test]
    fn transition_for_unknown_verdict_returns_none() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        assert!(reviewer.transition_for("UNKNOWN").is_none());
    }

    #[test]
    fn stage_for_status_returns_matching_stages() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();

        let results = config.stage_for_status("todo");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "analyst");

        let results = config.stage_for_status("ready");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "devops");

        let results = config.stage_for_status("deploy");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "devops");
    }

    #[test]
    fn max_concurrent_defaults_when_absent() {
        let yaml = r#"
pipeline: minimal
stages:
  test:
    agent: pipeline-test
    model: sonnet
"#;
        let config: PipelineConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.max_concurrent, DEFAULT_GLOBAL_MAX_CONCURRENT);
    }

    #[test]
    fn max_concurrent_explicit_value() {
        let yaml = r#"
pipeline: custom
max_concurrent: 2
stages:
  test:
    agent: pipeline-test
    model: sonnet
"#;
        let config: PipelineConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.max_concurrent, 2);
    }

    #[test]
    fn templates_are_loaded() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        assert!(config.templates.contains_key("verdict_instruction"));
    }

    #[test]
    fn inline_prompt_detected_by_newline() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let analyst = config.stage("analyst").unwrap();
        let p = analyst.prompt.as_deref().unwrap_or("");
        assert!(p.contains('\n'), "inline prompt should contain newline");
    }

    #[test]
    fn file_prompt_no_newline() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        let p = reviewer.prompt.as_deref().unwrap_or("");
        assert!(!p.contains('\n'), "file path should have no newline");
        assert_eq!(p, "prompts/reviewer.md");
    }

    #[test]
    fn spawn_in_transition() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let analyst = config.stage("analyst").unwrap();
        let t = analyst.transition_for("done").unwrap();
        assert_eq!(t.spawn.as_deref(), Some("engineer"));
    }

    #[test]
    fn fallback_model_parsed() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let engineer = config.stage("engineer").unwrap();
        assert_eq!(engineer.fallback.as_deref(), Some("sonnet"));

        let analyst = config.stage("analyst").unwrap();
        assert!(analyst.fallback.is_none());
    }

    #[test]
    fn recheck_model_deserialized() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        assert_eq!(reviewer.recheck_model.as_deref(), Some("haiku"));
        assert_eq!(reviewer.max_review_rounds, Some(3));
        assert_eq!(reviewer.max_turns, Some(15));
    }

    #[test]
    fn light_model_deserialized() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let engineer = config.stage("engineer").unwrap();
        assert_eq!(engineer.light_model.as_deref(), Some("sonnet"));
        assert_eq!(engineer.light_threshold, Some(2));
        assert_eq!(engineer.max_turns, Some(40));
    }

    #[test]
    fn effective_model_uses_recheck_on_round2() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        // Round 0 (first review): uses base model
        assert_eq!(reviewer.effective_model(0, 0), "sonnet");
        // Round 1+ (re-review): uses recheck_model
        assert_eq!(reviewer.effective_model(0, 1), "haiku");
        assert_eq!(reviewer.effective_model(0, 3), "haiku");
    }

    #[test]
    fn effective_model_uses_light_for_low_sp() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let engineer = config.stage("engineer").unwrap();
        // Low SP (1-2): uses light_model
        assert_eq!(engineer.effective_model(1, 0), "sonnet");
        assert_eq!(engineer.effective_model(2, 0), "sonnet");
        // High SP (3+): uses base model
        assert_eq!(engineer.effective_model(3, 0), "opus");
        assert_eq!(engineer.effective_model(5, 0), "opus");
        // No estimate (0): uses base model
        assert_eq!(engineer.effective_model(0, 0), "opus");
    }

    #[test]
    fn effective_model_no_light_config_uses_base() {
        let config: PipelineConfig = serde_yaml::from_str(sample_yaml()).unwrap();
        let analyst = config.stage("analyst").unwrap();
        // No light_model configured: always uses base
        assert_eq!(analyst.effective_model(1, 0), "opus");
    }

    #[test]
    fn new_fields_default_to_none() {
        let yaml = r#"
pipeline: minimal
stages:
  test:
    agent: pipeline-test
    model: sonnet
"#;
        let config: PipelineConfig = serde_yaml::from_str(yaml).unwrap();
        let stage = config.stage("test").unwrap();
        assert!(stage.recheck_model.is_none());
        assert!(stage.max_review_rounds.is_none());
        assert!(stage.max_turns.is_none());
        assert!(stage.light_model.is_none());
        assert!(stage.light_threshold.is_none());
    }

    #[test]
    fn validation_missing_spawn_target() {
        // A config referencing a non-existent stage in spawn should be detectable
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
        spawn: nonexistent_stage
"#;
        let config: PipelineConfig = serde_yaml::from_str(yaml).unwrap();
        let reviewer = config.stage("reviewer").unwrap();
        let t = reviewer.transition_for("rejected").unwrap();
        // The spawn target references a stage that doesn't exist
        let spawn_name = t.spawn.as_deref().unwrap();
        assert!(
            config.stage(spawn_name).is_none(),
            "spawn target should not exist in this config"
        );
    }
}
