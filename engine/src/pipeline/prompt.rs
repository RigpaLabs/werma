use std::collections::HashMap;

use indexmap::IndexMap;

/// Render a prompt template by substituting `{key}` placeholders.
///
/// - Replaces every `{key}` with the corresponding value from `vars`.
/// - Unknown keys are left as-is (no panic, no error).
/// - Order: vars take precedence; templates are merged in first.
pub fn render_prompt(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{key}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

/// Build the variable map for prompt rendering, merging pipeline templates + runtime vars.
///
/// `templates` from the pipeline config are inserted first (lower priority).
/// Runtime vars (issue data, stage data) override any template with the same key.
/// Computed variables (e.g. `nit_policy`) are derived after merging.
pub fn build_vars(
    templates: &IndexMap<String, String>,
    runtime: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut vars: HashMap<String, String> = templates
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    vars.extend(runtime.iter().map(|(k, v)| (k.clone(), v.clone())));
    sanitize_text_vars(&mut vars);
    compute_derived_vars(&mut vars);
    vars
}

/// Unescape literal `\n` and `\t` sequences in text-heavy template variables.
///
/// Issue descriptions from Linear often contain literal `\n` escape sequences
/// (backslash followed by 'n') instead of actual newlines, producing unreadable
/// walls of text. This sanitizes them at the source before prompt rendering.
fn sanitize_text_vars(vars: &mut HashMap<String, String>) {
    const TEXT_KEYS: &[&str] = &["issue_description", "previous_output", "rejection_feedback"];
    for key in TEXT_KEYS {
        if let Some(val) = vars.get_mut(*key) {
            let sanitized = val.replace("\\n", "\n").replace("\\t", "\t");
            if sanitized != *val {
                *val = sanitized;
            }
        }
    }
}

/// Derive computed template variables from existing ones.
///
/// Currently handles:
/// - `nit_policy`: generated from `nit_threshold`. When threshold=0, nits are informational
///   only. When threshold>=1, produces reject/approve rules with the threshold value.
fn compute_derived_vars(vars: &mut HashMap<String, String>) {
    let threshold: u32 = vars
        .get("nit_threshold")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let policy = if threshold == 0 {
        "   - Nits are informational only — list them but do not reject based on nit count alone\n   - **APPROVE** if no blockers".to_string()
    } else if threshold == 1 {
        "   - **REJECT** if there are any nits (strict quality bar)\n   - **APPROVE** if no blockers and no nits".to_string()
    } else {
        format!(
            "   - **REJECT** if there are {threshold}+ nits (accumulation of small issues signals low quality)\n   - **APPROVE** if no blockers and fewer than {threshold} nits"
        )
    };
    vars.entry("nit_policy".to_string()).or_insert(policy);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn simple_substitution() {
        let template = "Hello {name}, your issue is {issue_id}.";
        let v = vars(&[("name", "Ar"), ("issue_id", "RIG-65")]);
        assert_eq!(
            render_prompt(template, &v),
            "Hello Ar, your issue is RIG-65."
        );
    }

    #[test]
    fn unknown_keys_left_as_is() {
        let template = "Issue {issue_id}: {unknown_var}";
        let v = vars(&[("issue_id", "RIG-65")]);
        let result = render_prompt(template, &v);
        assert_eq!(result, "Issue RIG-65: {unknown_var}");
    }

    #[test]
    fn empty_vars_leaves_template_intact() {
        let template = "No vars here, just text.";
        let v = vars(&[]);
        assert_eq!(render_prompt(template, &v), template);
    }

    #[test]
    fn multiple_occurrences_replaced() {
        let template = "{x} and {x} again";
        let v = vars(&[("x", "foo")]);
        assert_eq!(render_prompt(template, &v), "foo and foo again");
    }

    #[test]
    fn multiline_template() {
        let template = "# Title\n\nIssue: {issue_id}\nTitle: {issue_title}\n";
        let v = vars(&[("issue_id", "RIG-1"), ("issue_title", "My issue")]);
        let result = render_prompt(template, &v);
        assert!(result.contains("RIG-1"));
        assert!(result.contains("My issue"));
    }

    #[test]
    fn build_vars_runtime_overrides_templates() {
        let mut templates = IndexMap::new();
        templates.insert(
            "verdict_instruction".to_string(),
            "default text".to_string(),
        );
        templates.insert("shared_key".to_string(), "from template".to_string());

        let runtime = vars(&[
            ("issue_id", "RIG-99"),
            ("shared_key", "from runtime"), // should override template
        ]);

        let result = build_vars(&templates, &runtime);
        assert_eq!(result["issue_id"], "RIG-99");
        assert_eq!(result["verdict_instruction"], "default text"); // from template
        assert_eq!(result["shared_key"], "from runtime"); // runtime wins
    }

    #[test]
    fn build_vars_empty_templates() {
        let templates = IndexMap::new();
        let runtime = vars(&[("issue_id", "RIG-1")]);
        let result = build_vars(&templates, &runtime);
        assert_eq!(result["issue_id"], "RIG-1");
    }

    #[test]
    fn render_with_empty_string_value() {
        let template = "Before {x} after";
        let v = vars(&[("x", "")]);
        assert_eq!(render_prompt(template, &v), "Before  after");
    }

    #[test]
    fn nit_policy_threshold_zero_is_informational() {
        let mut templates = IndexMap::new();
        templates.insert("nit_threshold".to_string(), "0".to_string());
        let runtime = vars(&[]);
        let result = build_vars(&templates, &runtime);
        let policy = &result["nit_policy"];
        assert!(
            policy.contains("informational only"),
            "threshold=0 should produce informational policy, got: {policy}"
        );
        assert!(
            !policy.contains("REJECT"),
            "threshold=0 should not mention REJECT, got: {policy}"
        );
    }

    #[test]
    fn nit_policy_threshold_nonzero() {
        let mut templates = IndexMap::new();
        templates.insert("nit_threshold".to_string(), "3".to_string());
        let runtime = vars(&[]);
        let result = build_vars(&templates, &runtime);
        let policy = &result["nit_policy"];
        assert!(policy.contains("3+ nits"), "should contain threshold value");
        assert!(
            policy.contains("fewer than 3"),
            "should contain approve condition"
        );
    }

    #[test]
    fn nit_policy_not_overridden_by_computed() {
        let mut templates = IndexMap::new();
        templates.insert("nit_threshold".to_string(), "3".to_string());
        templates.insert("nit_policy".to_string(), "custom policy".to_string());
        let runtime = vars(&[]);
        let result = build_vars(&templates, &runtime);
        assert_eq!(
            result["nit_policy"], "custom policy",
            "explicit nit_policy should not be overridden"
        );
    }

    #[test]
    fn nit_policy_invalid_threshold_falls_back_to_default() {
        let mut templates = IndexMap::new();
        templates.insert("nit_threshold".to_string(), "abc".to_string());
        let runtime = vars(&[]);
        let result = build_vars(&templates, &runtime);
        let policy = &result["nit_policy"];
        // Invalid parse falls back to 1 (strict default — special-cased phrasing)
        assert!(
            policy.contains("any nits"),
            "invalid threshold should fall back to 1, got: {policy}"
        );
    }

    #[test]
    fn nit_policy_threshold_one_natural_phrasing() {
        let mut templates = IndexMap::new();
        templates.insert("nit_threshold".to_string(), "1".to_string());
        let runtime = vars(&[]);
        let result = build_vars(&templates, &runtime);
        let policy = &result["nit_policy"];
        assert!(
            policy.contains("any nits"),
            "threshold=1 should use natural phrasing, got: {policy}"
        );
        assert!(
            policy.contains("no nits"),
            "threshold=1 approve should say 'no nits', got: {policy}"
        );
        assert!(
            !policy.contains("fewer than 1"),
            "threshold=1 should not use 'fewer than 1', got: {policy}"
        );
    }

    #[test]
    fn nit_policy_defaults_when_nit_threshold_absent() {
        let templates = IndexMap::new();
        let runtime = vars(&[("issue_id", "RIG-1")]);
        let result = build_vars(&templates, &runtime);
        let policy = &result["nit_policy"];
        // Should default to threshold=1 (strict)
        assert!(
            policy.contains("any nits"),
            "absent nit_threshold should produce default policy, got: {policy}"
        );
    }

    #[test]
    fn sanitize_unescapes_literal_newlines_in_description() {
        let templates = IndexMap::new();
        let runtime = vars(&[
            ("issue_id", "RIG-184"),
            (
                "issue_description",
                "Problem\\n\\nReviewer prompt only instructs to run `git diff`\\nSecond line",
            ),
        ]);
        let result = build_vars(&templates, &runtime);
        assert_eq!(
            result["issue_description"],
            "Problem\n\nReviewer prompt only instructs to run `git diff`\nSecond line"
        );
    }

    #[test]
    fn sanitize_unescapes_tabs() {
        let templates = IndexMap::new();
        let runtime = vars(&[("issue_description", "col1\\tcol2\\tcol3")]);
        let result = build_vars(&templates, &runtime);
        assert_eq!(result["issue_description"], "col1\tcol2\tcol3");
    }

    #[test]
    fn sanitize_applies_to_previous_output_and_rejection_feedback() {
        let templates = IndexMap::new();
        let runtime = vars(&[
            ("previous_output", "line1\\nline2"),
            ("rejection_feedback", "fix1\\nfix2"),
        ]);
        let result = build_vars(&templates, &runtime);
        assert_eq!(result["previous_output"], "line1\nline2");
        assert_eq!(result["rejection_feedback"], "fix1\nfix2");
    }

    #[test]
    fn sanitize_leaves_real_newlines_intact() {
        let templates = IndexMap::new();
        let runtime = vars(&[("issue_description", "already\nhas\nnewlines")]);
        let result = build_vars(&templates, &runtime);
        assert_eq!(result["issue_description"], "already\nhas\nnewlines");
    }

    #[test]
    fn sanitize_does_not_affect_non_text_vars() {
        let templates = IndexMap::new();
        let runtime = vars(&[("issue_id", "RIG\\n184")]);
        let result = build_vars(&templates, &runtime);
        assert_eq!(result["issue_id"], "RIG\\n184"); // should NOT be unescaped
    }

    #[test]
    fn no_false_partial_matches() {
        // {issue} should not match {issue_id}
        let template = "{issue} {issue_id}";
        let v = vars(&[("issue_id", "RIG-1")]);
        let result = render_prompt(template, &v);
        assert_eq!(result, "{issue} RIG-1");
    }
}
