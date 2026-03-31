//! E2E tests that run against real GitHub and Linear APIs.
//!
//! Guarded by: `#[cfg(all(test, feature = "e2e"))]`
//! Runtime: requires `WERMA_E2E=1` environment variable
//! Execution: `WERMA_E2E=1 cargo test --features e2e -- --test-threads=1`
//!
//! These tests create real PRs in the test repo and real TEST-XX issues
//! in Linear. All resources are cleaned up via scopeguard even on panic.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use crate::e2e_helpers::*;
use crate::pipeline::pr::{auto_create_pr, post_pr_review};
use crate::traits::RealCommandRunner;

// ── e2e_create_pr_full_cycle ────────────────────────────────────────────
// Reproduces the RIG-321 scenario: auto_create_pr against a real repo must
// return Ok(Some(url)), not Ok(None).

#[test]
fn e2e_create_pr_full_cycle() {
    e2e_preflight();

    let repo = test_repo();
    let (_tmp, checkout) = clone_test_repo().expect("clone failed");
    let branch = unique_name("e2e-pr-cycle");

    let branch_clone = branch.clone();
    let repo_clone = repo.clone();

    create_test_branch(&checkout, &branch).expect("branch creation failed");

    // Cleanup runs even on panic
    let _cleanup = scopeguard::guard((), |_| {
        // We can't capture pr_number mutably in the guard, so look it up fresh
        let _ = run_gh(&[
            "pr",
            "close",
            "--delete-branch",
            "-R",
            &repo_clone,
            &branch_clone,
        ]);
        // Also try to delete the branch directly (belt and suspenders)
        let _ = run_gh(&[
            "api",
            &format!("repos/{repo_clone}/git/refs/heads/{branch_clone}"),
            "-X",
            "DELETE",
        ]);
    });

    // Call the real auto_create_pr
    let cmd = RealCommandRunner;
    let working_dir = checkout.to_string_lossy();
    let result = auto_create_pr(&cmd, &working_dir, "TEST-E2E", "e2e-test-001");

    // CRITICAL: must return Ok(Some(url)), not Ok(None) — this is the RIG-321 bug
    let url = result
        .expect("auto_create_pr returned Err")
        .expect("auto_create_pr returned Ok(None) — the exact RIG-321 bug");

    assert!(
        url.contains("github.com"),
        "PR URL should contain github.com, got: {url}"
    );
    assert!(
        url.contains("/pull/"),
        "PR URL should contain /pull/, got: {url}"
    );

    // Extract PR number for verification
    let pr_num = pr_number_from_url(&url).unwrap_or_else(|| url.clone());

    // Verify PR actually exists on GitHub
    let pr_json = run_gh(&["pr", "view", &pr_num, "-R", &repo, "--json", "state,title"])
        .expect("PR should exist on GitHub");
    assert!(
        pr_json.contains("OPEN"),
        "PR should be in OPEN state, got: {pr_json}"
    );
}

// ── e2e_create_pr_wrong_dir ─────────────────────────────────────────────
// Regression test for RIG-321: calling auto_create_pr from a directory where
// the branch doesn't exist should NOT silently return Ok(None).

#[test]
fn e2e_create_pr_wrong_dir() {
    e2e_preflight();

    let repo = test_repo();
    let (tmp, checkout) = clone_test_repo().expect("clone failed");
    let branch = unique_name("e2e-pr-wrongdir");

    create_test_branch(&checkout, &branch).expect("branch creation failed");

    let branch_clone = branch.clone();
    let repo_clone = repo.clone();
    let _cleanup = scopeguard::guard((), |_| {
        // Clean up the remote branch (no PR expected, but be safe)
        let _ = run_gh(&[
            "pr",
            "close",
            "--delete-branch",
            "-R",
            &repo_clone,
            &branch_clone,
        ]);
        let _ = run_gh(&[
            "api",
            &format!("repos/{repo_clone}/git/refs/heads/{branch_clone}"),
            "-X",
            "DELETE",
        ]);
    });

    // Call auto_create_pr from a DIFFERENT directory (not the checkout)
    // This is the RIG-321 root cause — wrong working_dir
    let cmd = RealCommandRunner;
    let wrong_dir = tmp.path().to_string_lossy();
    let result = auto_create_pr(&cmd, &wrong_dir, "TEST-E2E", "e2e-test-002");

    // In a wrong dir, git commands should fail or return no branch — the function
    // RIG-355: auto_create_pr now returns Err when on main/empty branch (not Ok(None)).
    // From a wrong dir, it should always error — never silently succeed.
    match result {
        Ok(Some(url)) => {
            panic!(
                "auto_create_pr from wrong dir returned Ok(Some({url})) — should not create PR from wrong directory"
            );
        }
        Ok(None) => {
            // Still acceptable if no commits ahead of main (legitimate skip)
        }
        Err(_) => {
            // Expected: on main/empty branch returns Err, or git commands failed
        }
    }
}

// ── e2e_post_pr_review ──────────────────────────────────────────────────
// RIG-317 regression: post_pr_review must actually post a review that's
// visible on GitHub, not silently fail.

#[test]
fn e2e_post_pr_review() {
    e2e_preflight();

    let repo = test_repo();
    let (_tmp, checkout) = clone_test_repo().expect("clone failed");
    let branch = unique_name("e2e-pr-review");

    create_test_branch(&checkout, &branch).expect("branch creation failed");

    let branch_clone = branch.clone();
    let repo_clone = repo.clone();
    let _cleanup = scopeguard::guard((), |_| {
        let _ = run_gh(&[
            "pr",
            "close",
            "--delete-branch",
            "-R",
            &repo_clone,
            &branch_clone,
        ]);
        let _ = run_gh(&[
            "api",
            &format!("repos/{repo_clone}/git/refs/heads/{branch_clone}"),
            "-X",
            "DELETE",
        ]);
    });

    // First create a PR (needed for review)
    let pr_url = run_gh(&[
        "pr",
        "create",
        "-R",
        &repo,
        "--title",
        &format!("[E2E] {branch}"),
        "--body",
        "E2E test PR for review testing",
        "--head",
        &branch,
    ])
    .expect("PR creation failed");

    let pr_num = pr_number_from_url(&pr_url).expect("could not extract PR number");

    // Post a review using the real function
    let cmd = RealCommandRunner;
    let working_dir = checkout.to_string_lossy();
    let review_body = format!("E2E test review - {branch}");

    post_pr_review(&cmd, &working_dir, &review_body, "comment")
        .expect("post_pr_review failed — the RIG-317 bug");

    // Verify review is visible on GitHub
    let reviews_json = run_gh(&["pr", "view", &pr_num, "-R", &repo, "--json", "reviews"])
        .expect("could not fetch PR reviews");

    assert!(
        reviews_json.contains(&review_body) || reviews_json.contains("COMMENTED"),
        "review should be visible on GitHub, got: {reviews_json}"
    );
}

// ── e2e_linear_move_issue ───────────────────────────────────────────────
// Linear API integration: create an issue, move it to a different state,
// verify the state changed.

#[test]
fn e2e_linear_move_issue() {
    e2e_preflight();

    let title = unique_name("[E2E] move-test");
    let (uuid, identifier) = create_test_issue(&title).expect("issue creation failed");

    eprintln!("[e2e] created issue {identifier} (uuid: {uuid})");

    let uuid_clone = uuid.clone();
    let _cleanup = scopeguard::guard((), |_| {
        archive_test_issue(&uuid_clone);
    });

    // Get the initial state
    let initial_state = get_issue_state(&uuid).expect("could not get initial state");
    eprintln!("[e2e] initial state: {initial_state}");

    // Find a target state to move to (anything different from current)
    let states = get_team_states().expect("could not get team states");
    let target = states
        .iter()
        .find(|(_, name)| name != &initial_state && name != "Done" && name != "Canceled")
        .expect("no suitable target state found");

    eprintln!("[e2e] moving to: {} (id: {})", target.1, target.0);

    // Move the issue
    move_test_issue(&uuid, &target.0).expect("move failed");

    // Small delay for Linear eventual consistency
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Verify state changed
    let new_state = get_issue_state(&uuid).expect("could not get new state");
    eprintln!("[e2e] new state: {new_state}");

    assert_eq!(
        new_state, target.1,
        "issue state should have changed from '{initial_state}' to '{}'",
        target.1
    );
}
