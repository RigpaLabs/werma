use anyhow::Result;

use crate::config::UserConfig;

/// Display current configuration: repo mappings and limits.
pub fn cmd_config_show() -> Result<()> {
    let cfg = UserConfig::load();

    println!("# Repo Mappings");
    println!();

    let repos = cfg.all_repos();
    let mut entries: Vec<_> = repos.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    for (name, dir) in &entries {
        let source = if cfg.repos.contains_key(name.as_str()) {
            "config"
        } else {
            "convention"
        };
        println!("  {name:<20} → {dir}  ({source})");
    }

    println!();
    println!("# Settings");
    println!();
    println!(
        "  completed_limit: {}",
        match cfg.resolved_completed_limit() {
            Some(n) => n.to_string(),
            None => "unlimited".to_string(),
        }
    );

    println!();
    println!("Config file: ~/.werma/config.toml");

    Ok(())
}
