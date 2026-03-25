use anyhow::Result;

use crate::db::Db;
use crate::models::Status;

pub fn show_dashboard(db: &Db) -> Result<()> {
    println!();
    println!(
        "\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}"
    );
    println!(" Werma Dashboard");
    println!(
        "\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}"
    );

    show_agents(db)?;
    show_containers();
    show_schedules(db)?;

    println!();
    let sep = "\u{2550}".repeat(39);
    println!("{sep}");
    Ok(())
}

fn show_agents(db: &Db) -> Result<()> {
    let (p, r, c, f) = db.task_counts()?;

    println!();
    println!(" \u{2500}\u{2500} Agents \u{2500}\u{2500}");
    println!(" \u{25cb} {p} pending  \u{25c9} {r} running  \u{2713} {c} done  \u{2717} {f} failed");

    // Show tmux sessions
    let output = std::process::Command::new("tmux").args(["ls"]).output();
    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let sessions: Vec<&str> = stdout.lines().filter(|l| l.starts_with("werma-")).collect();
        if !sessions.is_empty() {
            println!();
            println!(" tmux:");
            for s in &sessions {
                println!("   {s}");
            }
        }
    }

    // Show running task details
    let running = db.list_tasks(Some(Status::Running))?;
    if !running.is_empty() {
        println!();
        println!(" running:");
        for task in &running {
            let linear = if task.linear_issue_id.is_empty() {
                String::new()
            } else {
                format!(" [{}]", task.linear_issue_id)
            };
            let retry_info = if task.retry_count > 0 {
                format!(" (retry #{})", task.retry_count)
            } else {
                String::new()
            };
            let prompt_preview = truncate_line(&task.prompt, 40);
            println!(
                "   {} {}{}{} {}",
                task.id, task.task_type, linear, retry_info, prompt_preview
            );
        }
    }

    // Show pending tasks waiting for retry
    let pending = db.list_tasks(Some(Status::Pending))?;
    let retry_pending: Vec<_> = pending.iter().filter(|t| t.retry_after.is_some()).collect();
    if !retry_pending.is_empty() {
        println!();
        println!(" retry pending:");
        for task in &retry_pending {
            let linear = if task.linear_issue_id.is_empty() {
                String::new()
            } else {
                format!(" [{}]", task.linear_issue_id)
            };
            let after = task.retry_after.as_deref().unwrap_or("?");
            println!(
                "   {} {}{} retry #{} (after {})",
                task.id, task.task_type, linear, task.retry_count, after
            );
        }
    }

    Ok(())
}

fn show_containers() {
    println!();
    println!(" \u{2500}\u{2500} Containers \u{2500}\u{2500}");

    println!(" Vultr Tokyo:");
    let fathom = ssh_docker_status("fathom-root");
    for line in fathom.lines() {
        println!("   {line}");
    }

    println!(" HT VPS:");
    let ht = ssh_docker_status("ht-root");
    for line in ht.lines() {
        println!("   {line}");
    }
}

fn show_schedules(db: &Db) -> Result<()> {
    let schedules = db.list_schedules()?;

    println!();
    println!(" \u{2500}\u{2500} Schedules \u{2500}\u{2500}");

    if schedules.is_empty() {
        println!("   (none)");
    } else {
        let enabled = schedules.iter().filter(|s| s.enabled).count();
        println!("   {}/{} enabled", enabled, schedules.len());
    }

    // Check daemon status
    let daemon_status = check_daemon_status();
    println!("   daemon: {daemon_status}");

    Ok(())
}

fn ssh_docker_status(host: &str) -> String {
    let output = std::process::Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            host,
            "docker ps --format 'table {{.Names}}\\t{{.Status}}\\t{{.Image}}'",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "(unreachable)".to_string(),
    }
}

fn check_daemon_status() -> &'static str {
    let output = std::process::Command::new("launchctl")
        .args(["list", "io.rigpalabs.werma.daemon"])
        .output();

    match output {
        Ok(o) if o.status.success() => "running",
        _ => "stopped",
    }
}

pub fn truncate_line(s: &str, max: usize) -> String {
    crate::commands::display::truncate(s, max)
}
