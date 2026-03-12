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
    // Fallback: try gh CLI
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stderr(std::process::Stdio::null())
        .output();
    if let Ok(out) = output
        && out.status.success()
    {
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
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
    asset_api_url: Option<String>,
}

/// Fetch the latest release info from GitHub API.
fn latest_release(token: &str) -> Result<ReleaseInfo> {
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

    let target = current_target();
    let asset_name = format!("werma-{target}.tar.gz");

    // Find the matching asset and use its API URL (works for private repos).
    // The "url" field is the API endpoint; "browser_download_url" returns 404 for private repos.
    let asset_api_url = json["assets"]
        .as_array()
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a["name"].as_str() == Some(&asset_name))
        })
        .and_then(|a| a["url"].as_str())
        .map(String::from);

    Ok(ReleaseInfo { tag, asset_api_url })
}

/// Download a release asset and extract the binary to a temp directory.
/// Returns the temp dir (must be kept alive) and the path to the extracted binary.
fn download_and_extract(
    token: &str,
    release: &ReleaseInfo,
) -> Result<(tempfile::TempDir, std::path::PathBuf)> {
    let target = current_target();
    if target == "unknown" {
        bail!("unsupported platform — download manually from GitHub Releases");
    }

    let artifact_name = format!("werma-{target}");

    let download_url = match &release.asset_api_url {
        Some(url) => url.clone(),
        None => format!(
            "https://github.com/{GITHUB_REPO}/releases/download/{}/{artifact_name}.tar.gz",
            release.tag,
        ),
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent("werma-updater")
        .build()
        .context("failed to build HTTP client")?;

    let resp = client
        .get(&download_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/octet-stream")
        .send()
        .with_context(|| format!("failed to download from {download_url}"))?;

    if !resp.status().is_success() {
        bail!("download failed ({}): {download_url}", resp.status());
    }

    let bytes = resp.bytes().context("failed to read download")?;

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

    Ok((tmp_dir, new_binary))
}

/// Replace the current binary with a new one.
/// Handles backup, copy, chmod, codesign (macOS), and rollback on failure.
/// Returns `Ok(true)` if codesign succeeded (or N/A), `Ok(false)` if codesign failed.
fn install_binary(new_binary: &std::path::Path) -> Result<bool> {
    let current_exe =
        std::env::current_exe().context("cannot determine current executable path")?;
    let current_exe = current_exe.canonicalize().unwrap_or(current_exe);

    let backup_path = current_exe.with_extension("old");
    let _ = std::fs::remove_file(&backup_path);

    std::fs::rename(&current_exe, &backup_path).context("failed to backup current binary")?;

    match std::fs::copy(new_binary, &current_exe) {
        Ok(_) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&current_exe, std::fs::Permissions::from_mode(0o755))?;
            }

            #[cfg(target_os = "macos")]
            let codesign_ok = {
                let codesign = std::process::Command::new("codesign")
                    .args(["--force", "--sign", "-", &current_exe.to_string_lossy()])
                    .output();
                matches!(codesign, Ok(ref out) if out.status.success())
            };
            #[cfg(not(target_os = "macos"))]
            let codesign_ok = true;

            let _ = std::fs::remove_file(&backup_path);
            Ok(codesign_ok)
        }
        Err(e) => {
            let _ = std::fs::rename(&backup_path, &current_exe);
            bail!("failed to install new binary: {e}");
        }
    }
}

/// Check for a new release and apply the update silently.
/// Returns `Ok(true)` if a new version was installed (caller should restart),
/// `Ok(false)` if already up to date, or `Err` on failure.
///
/// Designed for daemon use — no spinners, no println, no interactive UI.
pub fn check_and_apply_update() -> Result<bool> {
    let current = env!("CARGO_PKG_VERSION");
    let token = github_token()?;
    let release = latest_release(&token)?;
    let latest_version = release.tag.strip_prefix('v').unwrap_or(&release.tag);

    if latest_version == current {
        return Ok(false);
    }

    let (_tmp_dir, new_binary) = download_and_extract(&token, &release)?;
    let codesign_ok = install_binary(&new_binary)?;
    if !codesign_ok {
        // Binary installed but codesign failed — macOS will SIGKILL it on next exec.
        // Log and bail so the daemon does not restart into a broken binary.
        anyhow::bail!("update applied but codesign failed — run: codesign --force --sign - $(which werma)");
    }
    Ok(true)
}

/// Self-update werma binary from GitHub Releases.
pub fn update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("current: v{current}");

    let token = github_token()?;
    let release = crate::ui::with_spinner("Checking for updates...", || latest_release(&token))?;
    let latest_version = release.tag.strip_prefix('v').unwrap_or(&release.tag);

    if latest_version == current {
        println!("already up to date (v{current})");
        return Ok(());
    }

    println!("new version: {} (current: v{current})", release.tag);

    let artifact_name = format!("werma-{}", current_target());
    let pb = crate::ui::waiting_spinner(&format!("Downloading {artifact_name}..."));
    let (_tmp_dir, new_binary) = download_and_extract(&token, &release)?;
    pb.finish_and_clear();

    let current_exe =
        std::env::current_exe().context("cannot determine current executable path")?;
    let current_exe = current_exe.canonicalize().unwrap_or(current_exe);
    println!("replacing {}...", current_exe.display());

    let codesign_ok = install_binary(&new_binary)?;

    if codesign_ok {
        println!("updated to {}", release.tag);
    } else {
        println!(
            "updated to {} (warning: manual codesign required — run: codesign --force --sign - {})",
            release.tag,
            current_exe.display()
        );
    }

    // Restart daemon if it's running so it picks up the new binary.
    restart_daemon_if_running();

    Ok(())
}

const LAUNCHD_LABEL: &str = "io.rigpalabs.werma.daemon";

/// Check if the werma daemon is running via launchctl and restart it if so.
/// Uses `launchctl kickstart -k` which kills the running process and immediately
/// restarts it — launchd's KeepAlive ensures it comes back with the new binary.
fn restart_daemon_if_running() {
    // Check if daemon is loaded in launchctl
    let check = std::process::Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output();

    let is_running = matches!(check, Ok(ref o) if o.status.success());

    if !is_running {
        println!("daemon not running, skipping restart");
        return;
    }

    // Get current user UID for the gui/ domain target (same approach as daemon.rs)
    let uid = std::process::Command::new("id")
        .args(["-u"])
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(501);
    let service_target = format!("gui/{uid}/{LAUNCHD_LABEL}");

    println!("restarting daemon...");
    let result = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &service_target])
        .output();

    match result {
        Ok(o) if o.status.success() => {
            println!("daemon restarted with new binary");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // Fallback: try stop + start (older macOS or different launchd version)
            eprintln!("kickstart failed ({stderr}), trying stop+start...");
            let _ = std::process::Command::new("launchctl")
                .args(["stop", LAUNCHD_LABEL])
                .output();
            // launchd KeepAlive=true will auto-restart after stop
            println!("daemon stopped (launchd will auto-restart with new binary)");
        }
        Err(e) => {
            eprintln!("warning: could not restart daemon: {e}");
            eprintln!("run manually: launchctl kickstart -k {service_target}");
        }
    }
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
                    msg.contains("GITHUB_TOKEN")
                        && msg.contains("GH_TOKEN")
                        && msg.contains("gh auth login"),
                    "error should mention both env vars and gh auth login, got: {msg}"
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
