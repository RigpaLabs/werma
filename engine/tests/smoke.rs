//! Smoke tests for the werma binary.
//!
//! These tests verify the compiled binary starts, parses CLI args correctly,
//! and handles basic commands without panicking or crashing.
//! Each test runs in an isolated temp HOME to avoid touching real ~/.werma/ data.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Create a werma command with an isolated HOME directory.
/// This ensures tests don't read/write the real ~/.werma/.
fn werma_cmd(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("werma").expect("binary not found");
    cmd.env("HOME", home.path());
    // Prevent Linear API calls and .env loading from real config
    cmd.env_remove("LINEAR_API_KEY");
    cmd
}

#[test]
fn version_exits_0() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("werma"));
}

#[test]
fn help_exits_0_and_lists_commands() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("add"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("pipeline"))
        .stdout(predicate::str::contains("daemon"));
}

#[test]
fn status_with_empty_db() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home).arg("st").assert().success();
}

#[test]
fn list_with_empty_db() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home).arg("list").assert().success();
}

#[test]
fn pipeline_show() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home)
        .args(["pipeline", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("analyst").or(predicate::str::contains("engineer")));
}

#[test]
fn pipeline_validate() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home)
        .args(["pipeline", "validate"])
        .assert()
        .success();
}

#[test]
fn dash_with_empty_db() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home).arg("dash").assert().success();
}

#[test]
fn sched_ls_with_empty_db() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home).args(["sched", "ls"]).assert().success();
}

#[test]
fn version_subcommand() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home)
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("werma"));
}

#[test]
fn auto_creates_werma_dir() {
    let home = TempDir::new().unwrap();
    let werma_dir = home.path().join(".werma");
    assert!(!werma_dir.exists(), ".werma should not exist before test");

    werma_cmd(&home).arg("st").assert().success();

    assert!(werma_dir.exists(), ".werma should be auto-created");
    assert!(werma_dir.join("logs").exists(), "logs subdir should exist");
    assert!(
        werma_dir.join("backups").exists(),
        "backups subdir should exist"
    );
}

#[test]
fn no_env_file_does_not_crash() {
    let home = TempDir::new().unwrap();
    // No .env file in the temp home — binary should handle gracefully
    werma_cmd(&home).arg("list").assert().success();
}

#[test]
fn unknown_command_exits_nonzero() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home)
        .arg("nonexistent-command")
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn list_alias_ls() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home).arg("ls").assert().success();
}

#[test]
fn status_alias_st() {
    let home = TempDir::new().unwrap();
    werma_cmd(&home).arg("st").assert().success();
}
