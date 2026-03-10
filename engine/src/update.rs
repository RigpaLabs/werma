use anyhow::{Context, Result, bail};

const GITHUB_REPO: &str = "RigpaLabs/werma";

/// Read GitHub token from GITHUB_TOKEN or GH_TOKEN (gh CLI convention).
fn github_token() -> Result<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| {
            anyhow::anyhow!(
                "no GitHub token found — set GITHUB_TOKEN or GH_TOKEN for private repo access"
            )
        })
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

/// Fetch the latest release tag from GitHub API.
fn latest_release_tag(token: &str) -> Result<(String, String)> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");

    let client = reqwest::blocking::Client::builder()
        .user_agent("werma-updater")
        .build()
        .context("failed to build HTTP client")?;

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

    let body = json["body"].as_str().unwrap_or("").to_string();

    Ok((tag, body))
}

/// Self-update werma binary from GitHub Releases.
pub fn update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    println!("Checking for updates...");

    let token = github_token()?;

    let (latest_tag, _release_notes) = latest_release_tag(&token)?;
    let latest_version = latest_tag.strip_prefix('v').unwrap_or(&latest_tag);

    if latest_version == current {
        println!("Already up to date (v{current})");
        return Ok(());
    }

    println!("New version available: {latest_tag} (current: v{current})");

    let target = current_target();
    if target == "unknown" {
        bail!("unsupported platform — download manually from GitHub Releases");
    }

    let artifact_name = format!("werma-{target}");
    let download_url = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/{latest_tag}/{artifact_name}.tar.gz"
    );

    println!("Downloading {artifact_name}...");
    let client = reqwest::blocking::Client::builder()
        .user_agent("werma-updater")
        .build()
        .context("failed to build HTTP client")?;

    let resp = client
        .get(&download_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/octet-stream")
        .send()
        .context("failed to download release")?;

    if !resp.status().is_success() {
        bail!("download failed ({}): {}", resp.status(), download_url);
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
                    msg.contains("GITHUB_TOKEN") && msg.contains("GH_TOKEN"),
                    "error should mention both env vars, got: {msg}"
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
