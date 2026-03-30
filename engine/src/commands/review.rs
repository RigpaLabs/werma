use anyhow::{Result, bail};

use crate::db::Db;
use crate::runner;

use super::display::*;

/// Parse review target into a PR number (if applicable) and a descriptive label.
pub fn parse_review_target(target: &str) -> (Option<u32>, String) {
    // #123 format
    if let Some(num_str) = target.strip_prefix('#')
        && let Ok(n) = num_str.parse::<u32>()
    {
        return (Some(n), format!("PR #{n}"));
    }
    // Plain number
    if let Ok(n) = target.parse::<u32>() {
        return (Some(n), format!("PR #{n}"));
    }
    // URL containing /pull/123
    if target.contains("/pull/")
        && let Some(num_str) = target.rsplit('/').next()
        && let Ok(n) = num_str.parse::<u32>()
    {
        return (Some(n), format!("PR #{n}"));
    }
    // Branch name
    (None, format!("branch {target}"))
}

pub fn cmd_review(
    db: &Db,
    werma_dir: &std::path::Path,
    target: Option<&str>,
    dir: Option<&str>,
    force: bool,
) -> Result<()> {
    let working_dir = match dir {
        Some(d) => expand_tilde(d),
        None => default_working_dir(),
    };

    let (pr_number, label) = match target {
        Some(t) => parse_review_target(t),
        None => (None, "current changes".to_string()),
    };

    // Dedup: block if a review for this target is already running/pending
    if !force && db.has_active_review_task(&working_dir, &label)? {
        println!("review already active for {label} — skipping");
        println!("  (use --force to create another)");
        return Ok(());
    }

    // Info: mention if this PR was previously reviewed (completed)
    if let Some(n) = pr_number {
        let pr_key = format!("{working_dir}:{n}");
        if db.is_pr_reviewed(&pr_key)? {
            println!("note: {label} was previously reviewed — creating new review");
        }
    }

    // Capture diff as context file
    let logs_dir = werma_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;

    let task_id = db.next_task_id()?;
    let diff_path = logs_dir.join(format!("{task_id}-review-diff.patch"));

    let diff_cmd = if let Some(n) = pr_number {
        format!("cd '{working_dir}' && gh pr diff {n}")
    } else if let Some(t) = target {
        format!("cd '{working_dir}' && git diff main...{t}")
    } else {
        format!("cd '{working_dir}' && git diff main...HEAD")
    };

    let diff_output = std::process::Command::new("bash")
        .args(["-c", &diff_cmd])
        .output();

    match diff_output {
        Ok(out) if out.status.success() => {
            let diff = String::from_utf8_lossy(&out.stdout);
            if diff.trim().is_empty() {
                bail!("no diff found for {label}");
            }
            std::fs::write(&diff_path, diff.as_bytes())?;
            println!("captured diff: {} lines", diff.lines().count());
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("failed to get diff for {label}: {stderr}");
        }
        Err(e) => bail!("failed to run diff command: {e}"),
    }

    // Build review prompt (gh_post is injected into the LLM prompt, not executed directly)
    let gh_post = if let Some(n) = pr_number {
        format!(
            "6. **Post review as PR comment:**\n\
             ```\n\
             gh pr comment {n} --body \"<your review markdown>\"\n\
             ```\n\
             Include all findings, verdict, and summary in the comment.\n"
        )
    } else {
        String::new()
    };
    let prompt = format!(
        "# Code Review: {label}\n\n\
         Review the code diff provided in the context file.\n\n\
         ## Review Protocol\n\
         1. Read the diff carefully\n\
         2. Check for bugs, security issues, missing tests, style violations\n\
         3. Classify each finding as **blocker** or **nit**\n\
         4. APPROVE if no blockers, REJECT only on blockers\n\
         5. Read the full source files for important findings — the diff alone may lack context\n\
         {gh_post}\
         ## Output Format\n\
         - Each finding: `file:line — [blocker|nit] description`\n\
         - End with: REVIEW_VERDICT=APPROVED or REVIEW_VERDICT=REJECTED\n\
         - If rejected, clearly explain what must change"
    );

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let allowed_tools = runner::tools_for_type("pipeline-reviewer", false);

    let task = crate::models::Task {
        id: task_id.clone(),
        status: crate::models::Status::Pending,
        priority: 1,
        created_at: now,
        started_at: None,
        finished_at: None,
        task_type: "pipeline-reviewer".to_string(),
        prompt,
        output_path: String::new(),
        working_dir,
        model: "sonnet".to_string(),
        max_turns: default_turns("pipeline-reviewer"),
        allowed_tools,
        session_id: String::new(),
        linear_issue_id: String::new(),
        linear_pushed: false,
        pipeline_stage: String::new(),
        depends_on: vec![],
        context_files: vec![diff_path.to_string_lossy().to_string()],
        repo_hash: crate::runtime_repo_hash(),
        estimate: 0,
        retry_count: 0,
        retry_after: None,
        cost_usd: None,
        turns_used: 0,
        handoff_content: String::new(),
        runtime: crate::models::AgentRuntime::default(),
    };

    db.insert_task(&task)?;

    // Mark PR as reviewed for dedup
    if let Some(n) = pr_number {
        let pr_key = format!("{}:{}", task.working_dir, n);
        db.mark_pr_reviewed(&pr_key)?;
    }

    // Launch immediately
    match runner::run_task(db, &task, werma_dir) {
        Ok(Some(id)) => println!("review launched: {id} ({label})"),
        Ok(None) => println!("review queued: {task_id} ({label})"),
        Err(e) => eprintln!("review launch failed: {e} (task {task_id} is queued)"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_review_target_pr_hash() {
        let (n, label) = parse_review_target("#42");
        assert_eq!(n, Some(42));
        assert_eq!(label, "PR #42");
    }

    #[test]
    fn parse_review_target_plain_number() {
        let (n, label) = parse_review_target("7");
        assert_eq!(n, Some(7));
        assert_eq!(label, "PR #7");
    }

    #[test]
    fn parse_review_target_url() {
        let (n, label) = parse_review_target("https://github.com/org/repo/pull/99");
        assert_eq!(n, Some(99));
        assert_eq!(label, "PR #99");
    }

    #[test]
    fn parse_review_target_branch() {
        let (n, label) = parse_review_target("feat/new-thing");
        assert_eq!(n, None);
        assert_eq!(label, "branch feat/new-thing");
    }

    #[test]
    fn parse_review_target_hash_non_numeric() {
        let (n, label) = parse_review_target("#abc");
        assert_eq!(n, None);
        assert_eq!(label, "branch #abc");
    }

    #[test]
    fn parse_review_target_url_no_number() {
        let (n, label) = parse_review_target("https://github.com/org/repo/pull/abc");
        assert_eq!(n, None);
        assert_eq!(label, "branch https://github.com/org/repo/pull/abc");
    }
}
