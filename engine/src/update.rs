use anyhow::{Context, Result, bail};

const GITHUB_REPO: &str = "RigpaLabs/werma";

/// Read GitHub token from GITHUB_TOKEN, GH_TOKEN, or `gh auth token` fallback.
fn github_token() -> Result<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        return Ok(token);
    }
    if let Ok(token) = std::env::var("GH_TOKEN") {
        return Ok(token);
    }

    // Fallback: ask gh CLI for its stored token
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output();

    if let Ok(output) = output
        && output.status.success()
    {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    bail!("no GitHub token found — set GITHUB_TOKEN, GH_TOKEN, or log in with `gh auth login`")
}

/// Platform target triple for the current binary.
fn current_target() -> &'static str {
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "aarch64-apple-darwin"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        "x86_64-apple-darwin"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        "x86_64-unknown-linux-gnu"
    } else {
        "unknown"
    }
}

struct ReleaseInfo {
    tag: String,
    /// API URL for downloading the matching asset (if found).
    asset_api_url: Option<String>,
}

/// Build a shared HTTP client with auth.
fn api_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("werma-updater")
        .build()
        .context("failed to build HTTP client")
}

/// Fetch the latest release info from GitHub API.
/// Finds the asset matching `werma-{target}.tar.gz` and returns its API URL.
fn latest_release(token: &str, target: &str) -> Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");

    let client = api_client()?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .context("failed to fetch latest release")?;

    if !resp.status().is_success() {
        bail!(
            "GitHub API returned {}: check that {} has releases",
            resp.status(),
            GITHUB_REPO
        );
    }

    let json: serde_json::Value = resp.json().context("failed to parse release JSON")?;

    let tag = json["tag_name"]
        .as_str()
        .context("no tag_name in release")?
        .to_string();

    // Find asset by name pattern: werma-{target}.tar.gz
    let expected_name = format!("werma-{target}.tar.gz");
    let asset_api_url = json["assets"].as_array().and_then(|assets| {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            if name == expected_name {
                // Use the "url" field (api.github.com/...assets/{id}), NOT browser_download_url
                a["url"].as_str().map(std::string::ToString::to_string)
            } else {
                None
            }
        })
    });

    Ok(ReleaseInfo { tag, asset_api_url })
}

/// Self-update werma binary from GitHub Releases.
pub fn update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    println!("Checking for updates...");

    let token = github_token()?;

    let target = current_target();
    if target == "unknown" {
        bail!("unsupported platform — download manually from GitHub Releases");
    }

    let release = latest_release(&token, target)?;
    let latest_tag = &release.tag;
    let latest_version = latest_tag.strip_prefix('v').unwrap_or(latest_tag);

    if latest_version == current {
        println!("Already up to date (v{current})");
        return Ok(());
    }

    println!("New version available: {latest_tag} (current: v{current})");

    let asset_url = release.asset_api_url.context(format!(
        "no asset werma-{target}.tar.gz found in release {latest_tag}",
    ))?;

    let artifact_name = format!("werma-{target}");
    println!("Downloading {artifact_name}...");

    // Download via API URL — works for both public and private repos.
    // GitHub returns 302 → signed S3 URL for the actual binary.
    let client = api_client()?;

    let resp = client
        .get(&asset_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/octet-stream")
        .send()
        .context("failed to download release")?;

    if !resp.status().is_success() {
        bail!("download failed ({}): {}", resp.status(), asset_url);
    }

    let bytes = resp.bytes().context("failed to read download")?;

    // Extract tar.gz to temp dir
    let tmp_dir = tempfile::tempdir().context("failed to create temp dir")?;
    let archive_path = tmp_dir.path().join(format!("{artifact_name}.tar.gz"));
    std::fs::write(&archive_path, &bytes)?;

    let status = std::process::Command::new("tar")
        .args(["xzf", &archive_path.to_string_lossy()])
        .current_dir(tmp_dir.path())
        .status()
        .context("failed to extract archive")?;

    if !status.success() {
        bail!("tar extraction failed");
    }

    let new_binary = tmp_dir.path().join("werma");
    if !new_binary.exists() {
        bail!("extracted archive does not contain 'werma' binary");
    }

    // Find current binary location
    let current_exe =
        std::env::current_exe().context("cannot determine current executable path")?;
    let current_exe = current_exe.canonicalize().unwrap_or(current_exe);

    println!("Replacing {}...", current_exe.display());

    // Atomic replace: rename old, copy new, remove old
    let backup_path = current_exe.with_extension("old");

    // Remove stale backup if exists
    let _ = std::fs::remove_file(&backup_path);

    std::fs::rename(&current_exe, &backup_path).context("failed to backup current binary")?;

    match std::fs::copy(&new_binary, &current_exe) {
        Ok(_) => {
            // Set executable permission
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&current_exe, std::fs::Permissions::from_mode(0o755))?;
            }
            // Remove backup
            let _ = std::fs::remove_file(&backup_path);
            println!("Updated to {latest_tag}");
        }
        Err(e) => {
            // Restore backup on failure
            let _ = std::fs::rename(&backup_path, &current_exe);
            bail!("failed to install new binary: {e}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_token_reads_from_env() {
        // Can't mutate env in tests — std::env::set_var requires unsafe since Rust 1.80
        // (soundness with multi-threaded access). Just verify the function
        // returns Ok when a token env var is set (CI sets GITHUB_TOKEN)
        // or Err with a helpful message when neither is set.
        match github_token() {
            Ok(token) => assert!(!token.is_empty(), "token should not be empty"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("GITHUB_TOKEN") || msg.contains("gh auth login"),
                    "error should mention env vars or gh auth, got: {msg}"
                );
            }
        }
    }

    #[test]
    fn current_target_is_known() {
        let target = current_target();
        // In CI/test, should always be one of the known targets
        assert_ne!(target, "unknown", "running on unsupported platform");
    }
}
