use anyhow::Result;

use crate::daemon;

pub fn cmd_daemon_install() -> Result<()> {
    daemon::install()
}

pub fn cmd_daemon_uninstall() -> Result<()> {
    daemon::uninstall()
}

pub fn cmd_daemon_run() -> Result<()> {
    let dir = crate::werma_dir()?;
    daemon::run(&dir)
}
