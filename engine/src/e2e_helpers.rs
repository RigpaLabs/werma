//! E2E test helpers — preflight checks, GitHub/Linear helpers, cleanup utilities.
//!
//! Guarded by `#[cfg(all(test, feature = "e2e"))]` — never compiled into release binary.
//! Uses raw GraphQL for Linear operations (not LinearClient) to avoid polluting
//! the production API surface with test-only methods.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use uuid::Uuid;

// ── Constants ────────────────────────────────────────────────────────────

pub const TEST_REPO: &str = "RigpaLabs/werma-test";
pub const TEST_TEAM_ID: &str = "f81facea-4e3c-4088-962a-711c99db0d6f";
const LINEAR_API: &str = "https://api.linear.app/graphql";

// ── Preflight ────────────────────────────────────────────────────────────

/// Verify all prerequisites for e2e tests. Panics with clear message if anything is missing.
pub fn e2e_preflight() {
    // 1. WERMA_E2E=1 must be set (conscious opt-in to real API calls)
    assert_eq!(
        std::env::var("WERMA_E2E").unwrap_or_default(),
        "1",
        "Set WERMA_E2E=1 to run e2e tests"
    );

    // 2. gh CLI must be authenticated
    let gh_status = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .expect("gh CLI not found — install GitHub CLI");
    assert!(
        gh_status.status.success(),
        "gh auth not configured — run `gh auth login`"
    );

    // 3. LINEAR_API_KEY must be available
    assert!(
        linear_api_key().is_ok(),
        "LINEAR_API_KEY not set — export it or add to ~/.werma/.env"
    );
}

// ── Unique naming ────────────────────────────────────────────────────────

/// Generate a unique name with UUID suffix for test isolation.
pub fn unique_name(prefix: &str) -> String {
    let short = &Uuid::new_v4().to_string()[..8];
    format!("{prefix}-{short}")
}

// ── Git/GitHub helpers ───────────────────────────────────────────────────

/// Run a git command in the given directory. Returns stdout on success.
pub fn run_git(args: &[&str], dir: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run a gh CLI command. Returns stdout on success.
pub fn run_gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .with_context(|| format!("gh {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Clone the test repo into a tempdir. Returns (TempDir, path to checkout).
pub fn clone_test_repo() -> Result<(tempfile::TempDir, PathBuf)> {
    let tmp = tempfile::tempdir().context("creating tempdir")?;
    let checkout = tmp.path().join("werma-test");

    let url = format!("https://github.com/{TEST_REPO}.git");
    run_git(
        &["clone", &url, &checkout.to_string_lossy()],
        tmp.path(),
    )?;

    Ok((tmp, checkout))
}

/// Create a test branch with a dummy commit and push it.
pub fn create_test_branch(dir: &Path, branch: &str) -> Result<()> {
    run_git(&["checkout", "-b", branch], dir)?;

    // Create a dummy file so there's something to commit
    let dummy = dir.join(format!("{branch}.txt"));
    std::fs::write(&dummy, format!("e2e test branch: {branch}\n"))?;

    run_git(&["add", "."], dir)?;
    run_git(&["commit", "-m", &format!("e2e: {branch}")], dir)?;
    run_git(&["push", "-u", "origin", branch], dir)?;
    Ok(())
}

/// Extract PR number from a GitHub PR URL (e.g. "https://github.com/org/repo/pull/42" → "42").
pub fn pr_number_from_url(url: &str) -> Option<String> {
    url.rsplit('/')
        .next()
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

// ── Linear helpers (raw GraphQL) ─────────────────────────────────────────

/// Get LINEAR_API_KEY from environment or ~/.werma/.env.
pub fn linear_api_key() -> Result<String> {
    std::env::var("LINEAR_API_KEY")
        .or_else(|_| std::env::var("WERMA_LINEAR_API_KEY"))
        .or_else(|_| {
            crate::config::read_env_file_key("LINEAR_API_KEY")
                .map_err(|_| std::env::VarError::NotPresent)
        })
        .map_err(|_| anyhow::anyhow!("LINEAR_API_KEY not available"))
}

/// Execute a raw GraphQL query against the Linear API.
fn linear_graphql(
    client: &reqwest::blocking::Client,
    api_key: &str,
    query: &str,
    variables: &serde_json::Value,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "query": query,
        "variables": variables,
    });

    let resp = client
        .post(LINEAR_API)
        .header("Authorization", api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("Linear API request")?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().context("parsing Linear response")?;

    if !status.is_success() {
        bail!("Linear API error (HTTP {status}): {json}");
    }
    if let Some(errors) = json.get("errors") {
        bail!("Linear GraphQL errors: {errors}");
    }

    Ok(json)
}

/// Create a test issue in the Test team. Returns (uuid, identifier like "TEST-42").
pub fn create_test_issue(title: &str) -> Result<(String, String)> {
    let api_key = linear_api_key()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let result = linear_graphql(
        &client,
        &api_key,
        r#"mutation($title: String!, $teamId: String!) {
            issueCreate(input: { title: $title, teamId: $teamId }) {
                success
                issue { id identifier }
            }
        }"#,
        &serde_json::json!({
            "title": title,
            "teamId": TEST_TEAM_ID,
        }),
    )?;

    let issue = &result["data"]["issueCreate"]["issue"];
    let uuid = issue["id"]
        .as_str()
        .context("missing issue id")?
        .to_string();
    let identifier = issue["identifier"]
        .as_str()
        .context("missing issue identifier")?
        .to_string();

    Ok((uuid, identifier))
}

/// Archive (soft-delete) a test issue. Best-effort — ignores errors.
pub fn archive_test_issue(uuid: &str) {
    let Ok(api_key) = linear_api_key() else {
        return;
    };
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return;
    };

    let _ = linear_graphql(
        &client,
        &api_key,
        r#"mutation($id: String!) {
            issueArchive(id: $id) { success }
        }"#,
        &serde_json::json!({"id": uuid}),
    );
}

/// Get the current workflow state name for an issue (e.g. "Backlog", "In Progress").
pub fn get_issue_state(uuid: &str) -> Result<String> {
    let api_key = linear_api_key()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let result = linear_graphql(
        &client,
        &api_key,
        r#"query($id: String!) {
            issue(id: $id) { state { name } }
        }"#,
        &serde_json::json!({"id": uuid}),
    )?;

    result["data"]["issue"]["state"]["name"]
        .as_str()
        .map(str::to_string)
        .context("missing state name")
}

/// Get all workflow states for the Test team. Returns vec of (state_id, state_name).
pub fn get_team_states() -> Result<Vec<(String, String)>> {
    let api_key = linear_api_key()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let result = linear_graphql(
        &client,
        &api_key,
        r#"query($teamId: String!) {
            team(id: $teamId) {
                states { nodes { id name } }
            }
        }"#,
        &serde_json::json!({"teamId": TEST_TEAM_ID}),
    )?;

    let nodes = result["data"]["team"]["states"]["nodes"]
        .as_array()
        .context("missing states")?;

    let mut states = Vec::new();
    for node in nodes {
        let id = node["id"].as_str().unwrap_or_default().to_string();
        let name = node["name"].as_str().unwrap_or_default().to_string();
        states.push((id, name));
    }
    Ok(states)
}

/// Move an issue to a new state by state UUID (raw mutation).
pub fn move_test_issue(issue_uuid: &str, state_id: &str) -> Result<()> {
    let api_key = linear_api_key()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    linear_graphql(
        &client,
        &api_key,
        r#"mutation($id: String!, $stateId: String!) {
            issueUpdate(id: $id, input: { stateId: $stateId }) { success }
        }"#,
        &serde_json::json!({"id": issue_uuid, "stateId": state_id}),
    )?;

    Ok(())
}
