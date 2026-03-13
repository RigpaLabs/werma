use anyhow::Result;

use crate::db::Db;
use crate::{backup, dashboard, migrate, update};

pub fn cmd_version() {
    println!("werma {}", crate::version_string());
}

pub fn cmd_dash(db: &Db) -> Result<()> {
    dashboard::show_dashboard(db)
}

pub fn cmd_backup() -> Result<()> {
    let dir = crate::werma_dir()?;
    backup::backup_db(&dir)?;
    Ok(())
}

pub fn cmd_migrate(db: &Db) -> Result<()> {
    migrate::migrate(db)
}

pub fn cmd_update() -> Result<()> {
    update::update()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_version_does_not_panic() {
        // Just verify it doesn't panic
        cmd_version();
    }

    #[test]
    fn cmd_dash_works_with_empty_db() {
        let db = Db::open_in_memory().unwrap();
        cmd_dash(&db).unwrap();
    }
}
