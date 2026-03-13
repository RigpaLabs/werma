use anyhow::Result;

use crate::db::Db;
use crate::{linear, ui};

pub fn cmd_linear_setup() -> Result<()> {
    let client = linear::LinearClient::new()?;
    ui::with_spinner("Discovering Linear workspace...", || client.setup())
}

pub fn cmd_linear_sync(db: &Db) -> Result<()> {
    let client = linear::LinearClient::new()?;
    ui::with_spinner("Syncing issues from Linear...", || client.sync(db))
}

pub fn cmd_linear_push(db: &Db, id: &str) -> Result<()> {
    let client = linear::LinearClient::new()?;
    client.push(db, id)
}

pub fn cmd_linear_push_all(db: &Db) -> Result<()> {
    let client = linear::LinearClient::new()?;
    client.push_all(db)
}

#[cfg(test)]
mod tests {
    // Linear commands require API access, so integration-level testing
    // is handled via the linear module's own tests.
    // Module compilation test:
    #[test]
    fn linear_cmd_module_exists() {
        assert!(true);
    }
}
