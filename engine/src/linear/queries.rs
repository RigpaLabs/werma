use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::api::LinearClient;
use super::config::{load_config, team_key_from_identifier};

impl LinearClient {
    /// Get issues filtered by status name, across all configured teams.
    pub fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>> {
        let config = load_config()?;
        let mut all_issues = Vec::new();

        for team in &config.teams {
            let state_id = match team.statuses.get(status_name) {
                Some(id) if !id.is_empty() => id.clone(),
                _ => continue,
            };

            let data = self.query(
                r#"query($teamId: ID!, $stateId: ID!) {
                    issues(
                        filter: {
                            team: { id: { eq: $teamId } },
                            state: { id: { eq: $stateId } }
                        },
                        orderBy: updatedAt
                    ) {
                        nodes {
                            id
                            identifier
                            title
                            description
                            priority
                            estimate
                            state { type }
                            labels { nodes { name } }
                        }
                    }
                }"#,
                &json!({"teamId": team.team_id, "stateId": state_id}),
            )?;

            if let Some(issues) = data["issues"]["nodes"].as_array() {
                all_issues.extend(issues.clone());
            }
        }

        Ok(all_issues)
    }

    /// Get issues filtered by label name, across all configured teams.
    pub fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<Value>> {
        let config = load_config()?;
        let mut all_issues = Vec::new();

        for team in &config.teams {
            let data = self.query(
                r#"query($teamId: ID!, $label: String!) {
                    issues(
                        filter: {
                            team: { id: { eq: $teamId } },
                            labels: { some: { name: { eqIgnoreCase: $label } } }
                        },
                        orderBy: updatedAt
                    ) {
                        nodes {
                            id
                            identifier
                            title
                            description
                            priority
                            estimate
                            state { type }
                            labels { nodes { id name } }
                        }
                    }
                }"#,
                &json!({"teamId": team.team_id, "label": label_name}),
            )?;

            if let Some(issues) = data["issues"]["nodes"].as_array() {
                all_issues.extend(issues.clone());
            }
        }

        Ok(all_issues)
    }

    /// Remove a label from an issue by label name.
    pub fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;

        // First, get the issue's current labels to find the label ID
        let data = self.query(
            r#"query($id: String!) {
                issue(id: $id) {
                    labels { nodes { id name } }
                }
            }"#,
            &json!({"id": uuid}),
        )?;

        let labels = data["issue"]["labels"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // Collect label IDs, excluding the one to remove
        let remaining_ids: Vec<String> = labels
            .iter()
            .filter(|l| {
                !l["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(label_name))
            })
            .filter_map(|l| l["id"].as_str().map(String::from))
            .collect();

        // Update issue with remaining labels
        self.query(
            r#"mutation($id: String!, $labelIds: [String!]!) {
                issueUpdate(id: $id, input: { labelIds: $labelIds }) { success }
            }"#,
            &json!({"id": uuid, "labelIds": remaining_ids}),
        )?;

        Ok(())
    }

    /// Add a label to an issue by label name.
    pub fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        let uuid = self.resolve_uuid(issue_id)?;
        let config = load_config()?;
        let team_key = team_key_from_identifier(issue_id);
        let team = config
            .team_by_key(&team_key)
            .or(config.primary_team())
            .context("no teams configured")?;

        // Find the label ID by name from team labels, and get the issue's current labels
        let data = self.query(
            r#"query($issueId: ID!, $teamId: ID!, $name: String!) {
                issue(id: $issueId) {
                    labels { nodes { id } }
                }
                issueLabels(filter: { team: { id: { eq: $teamId } }, name: { eq: $name } }) {
                    nodes { id }
                }
            }"#,
            &json!({"issueId": uuid, "teamId": team.team_id, "name": label_name}),
        )?;

        let new_label_id = data["issueLabels"]["nodes"][0]["id"]
            .as_str()
            .with_context(|| format!("label '{label_name}' not found in team labels"))?;

        let mut label_ids: Vec<String> = data["issue"]["labels"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|l| l["id"].as_str().map(String::from))
            .collect();

        if !label_ids.iter().any(|id| id == new_label_id) {
            label_ids.push(new_label_id.to_string());
        }

        self.query(
            r#"mutation($id: String!, $labelIds: [String!]!) {
                issueUpdate(id: $id, input: { labelIds: $labelIds }) { success }
            }"#,
            &json!({"id": uuid, "labelIds": label_ids}),
        )?;

        Ok(())
    }
}
