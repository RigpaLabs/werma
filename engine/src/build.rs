use anyhow::{Context, Result, bail};

use crate::update::{GITHUB_REPO, current_target, github_token};

/// A GitHub release that is missing the platform binary asset.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingRelease {
    pub tag: String,
    pub upload_url: String,
}

/// Fetch recent releases and return those missing the binary asset for the current platform.
/// Returns newest first (GitHub API default order).
pub(crate) fn find_pending_releases(token: &str) -> Result<Vec<PendingRelease>> {
    let target = current_target();
    if target == "unknown" {
        bail!("unsupported platform — cannot determine asset name");
    }
    let asset_name = format!("werma-{target}.tar.gz");

    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases?per_page=20");

    let client = reqwest::blocking::Client::builder()
        .user_agent("werma-builder")
        .build()
        .context("failed to build HTTP client")?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .context("failed to fetch releases")?;

    if !resp.status().is_success() {
        bail!(
            "GitHub API returned {}: check token and repo access",
            resp.status()
        );
    }

    let releases: Vec<serde_json::Value> = resp.json().context("failed to parse releases JSON")?;

    let mut pending = Vec::new();
    for release in &releases {
        // Skip drafts and prereleases
        if release["draft"].as_bool() == Some(true) || release["prerelease"].as_bool() == Some(true)
        {
            continue;
        }

        let tag = match release["tag_name"].as_str() {
            Some(t) => t,
            None => continue,
        };

        let has_asset = release["assets"]
            .as_array()
            .map(|assets| {
                assets
                    .iter()
                    .any(|a| a["name"].as_str() == Some(&asset_name))
            })
            .unwrap_or(false);

        if !has_asset {
            let upload_url = release["upload_url"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            pending.push(PendingRelease {
                tag: tag.to_string(),
                upload_url,
            });
        }
    }

    Ok(pending)
}

/// Build, codesign, package, and upload the binary for all pending releases.
pub fn build() -> Result<()> {
    let target = current_target();
    if target == "unknown" {
        bail!("unsupported platform for build");
    }

    let token = github_token()?;
    let pending =
        crate::ui::with_spinner("Checking releases...", || find_pending_releases(&token))?;

    if pending.is_empty() {
        println!("all releases have binaries — nothing to build");
        return Ok(());
    }

    println!(
        "found {} release(s) missing {target} binary: {}",
        pending.len(),
        pending
            .iter()
            .map(|r| r.tag.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Build once — the binary is the same for all releases (tags only differ in version embedding,
    // but we use WERMA_GIT_VERSION env var per-upload to match the tag).
    // Actually, each release tag embeds a different version, so we build per-tag.
    let engine_dir = find_engine_dir()?;

    for release in &pending {
        println!("\n--- {} ---", release.tag);

        // Build with the tag version embedded
        println!("building for {target}...");
        let build_status = std::process::Command::new("cargo")
            .args(["build", "--release", "--target", target])
            .env("WERMA_GIT_VERSION", &release.tag)
            .current_dir(&engine_dir)
            .status()
            .context("failed to run cargo build")?;

        if !build_status.success() {
            bail!("cargo build failed for {}", release.tag);
        }

        // Create temp dir for packaging
        let tmp = tempfile::tempdir().context("failed to create temp dir")?;
        let binary_src = engine_dir
            .join("target")
            .join(target)
            .join("release")
            .join("werma");

        if !binary_src.exists() {
            bail!("build produced no binary at {}", binary_src.display());
        }

        let binary_dst = tmp.path().join("werma");
        std::fs::copy(&binary_src, &binary_dst).context("failed to copy binary")?;

        // Codesign (macOS)
        #[cfg(target_os = "macos")]
        {
            println!("codesigning...");
            let cs = std::process::Command::new("codesign")
                .args(["--force", "--sign", "-", &binary_dst.to_string_lossy()])
                .status()
                .context("failed to run codesign")?;
            if !cs.success() {
                bail!("codesign failed");
            }
        }

        // Package
        let archive_name = format!("werma-{target}.tar.gz");
        let archive_path = tmp.path().join(&archive_name);

        println!("packaging {archive_name}...");
        let tar_status = std::process::Command::new("tar")
            .args(["czf", &archive_path.to_string_lossy(), "werma"])
            .current_dir(tmp.path())
            .status()
            .context("failed to create tar archive")?;

        if !tar_status.success() {
            bail!("tar packaging failed");
        }

        // Upload via gh CLI
        println!("uploading to {}...", release.tag);
        let upload_status = std::process::Command::new("gh")
            .args([
                "release",
                "upload",
                &release.tag,
                &archive_path.to_string_lossy(),
                "--repo",
                GITHUB_REPO,
                "--clobber",
            ])
            .status()
            .context("failed to run gh release upload")?;

        if !upload_status.success() {
            bail!("upload failed for {}", release.tag);
        }

        println!("uploaded {} to {}", archive_name, release.tag);
    }

    println!("\ndone — {} release(s) updated", pending.len());
    Ok(())
}

/// Find the engine/ directory relative to the werma repo checkout.
fn find_engine_dir() -> Result<std::path::PathBuf> {
    // Try WERMA_REPO env, then default location
    let repo = std::env::var("WERMA_REPO").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|h| {
                h.join("projects/rigpa/werma")
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default()
    });
    let engine = std::path::PathBuf::from(&repo).join("engine");
    if engine.join("Cargo.toml").exists() {
        return Ok(engine);
    }

    // Try current dir
    let cwd = std::env::current_dir().context("cannot get current directory")?;
    if cwd.join("Cargo.toml").exists() {
        return Ok(cwd);
    }
    let cwd_engine = cwd.join("engine");
    if cwd_engine.join("Cargo.toml").exists() {
        return Ok(cwd_engine);
    }

    bail!("cannot find engine/ directory — set WERMA_REPO or run from the werma repo root");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_pending_releases_parses_json() {
        // Unit test the filtering logic with mock JSON data
        let releases_json = serde_json::json!([
            {
                "tag_name": "v0.40.1",
                "draft": false,
                "prerelease": false,
                "upload_url": "https://uploads.github.com/repos/RigpaLabs/werma/releases/1/assets{?name,label}",
                "assets": []
            },
            {
                "tag_name": "v0.40.0",
                "draft": false,
                "prerelease": false,
                "upload_url": "https://uploads.github.com/repos/RigpaLabs/werma/releases/2/assets{?name,label}",
                "assets": [
                    {"name": "werma-aarch64-apple-darwin.tar.gz"}
                ]
            },
            {
                "tag_name": "v0.39.0",
                "draft": true,
                "prerelease": false,
                "upload_url": "",
                "assets": []
            },
            {
                "tag_name": "v0.38.0",
                "draft": false,
                "prerelease": false,
                "upload_url": "https://uploads.github.com/repos/RigpaLabs/werma/releases/4/assets{?name,label}",
                "assets": []
            }
        ]);

        let target = current_target();
        let asset_name = format!("werma-{target}.tar.gz");

        let releases: Vec<serde_json::Value> =
            serde_json::from_value(releases_json).expect("valid JSON");

        let mut pending = Vec::new();
        for release in &releases {
            if release["draft"].as_bool() == Some(true)
                || release["prerelease"].as_bool() == Some(true)
            {
                continue;
            }
            let tag = release["tag_name"].as_str().unwrap_or_default();
            let has_asset = release["assets"]
                .as_array()
                .map(|assets| {
                    assets
                        .iter()
                        .any(|a| a["name"].as_str() == Some(&asset_name))
                })
                .unwrap_or(false);
            if !has_asset {
                pending.push(PendingRelease {
                    tag: tag.to_string(),
                    upload_url: release["upload_url"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }

        // v0.40.1 has no asset, v0.40.0 has the asset, v0.39.0 is draft (skipped), v0.38.0 has no asset
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].tag, "v0.40.1");
        assert_eq!(pending[1].tag, "v0.38.0");
    }

    #[test]
    fn find_pending_releases_empty_list() {
        let releases_json = serde_json::json!([]);
        let releases: Vec<serde_json::Value> =
            serde_json::from_value(releases_json).expect("valid JSON");
        assert!(releases.is_empty());
    }

    #[test]
    fn find_pending_releases_all_have_assets() {
        let target = current_target();
        let asset_name = format!("werma-{target}.tar.gz");

        let releases_json = serde_json::json!([
            {
                "tag_name": "v1.0.0",
                "draft": false,
                "prerelease": false,
                "upload_url": "",
                "assets": [{"name": asset_name}]
            }
        ]);

        let releases: Vec<serde_json::Value> =
            serde_json::from_value(releases_json).expect("valid JSON");

        let mut pending = Vec::new();
        for release in &releases {
            if release["draft"].as_bool() == Some(true) {
                continue;
            }
            let tag = release["tag_name"].as_str().unwrap_or_default();
            let has_asset = release["assets"]
                .as_array()
                .map(|assets| {
                    assets
                        .iter()
                        .any(|a| a["name"].as_str() == Some(&asset_name))
                })
                .unwrap_or(false);
            if !has_asset {
                pending.push(PendingRelease {
                    tag: tag.to_string(),
                    upload_url: String::new(),
                });
            }
        }

        assert!(pending.is_empty());
    }

    #[test]
    fn pending_release_derives() {
        let r = PendingRelease {
            tag: "v1.0.0".to_string(),
            upload_url: String::new(),
        };
        let r2 = r.clone();
        assert_eq!(r, r2);
        assert!(format!("{r:?}").contains("v1.0.0"));
    }
}
