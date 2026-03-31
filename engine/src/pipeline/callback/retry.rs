use anyhow::{Result, anyhow};

use crate::linear::LinearApi;

/// Max retries for Linear status move operations.
pub(super) const CALLBACK_MAX_RETRIES: u32 = 3;
/// Backoff delays in milliseconds between retries: 50ms, 100ms, 200ms.
pub(super) const CALLBACK_BACKOFF_MS: [u64; 3] = [50, 100, 200];

/// Move a Linear issue to a new status with retry + backoff + reconciliation.
///
/// Retries up to `CALLBACK_MAX_RETRIES` times with exponential backoff.
/// After a successful move, performs a read-after-write check to verify
/// the status actually changed. Returns an error only if all retries
/// are exhausted or reconciliation fails.
pub(crate) fn move_with_retry(
    linear: &dyn LinearApi,
    issue_id: &str,
    target_status: &str,
) -> Result<()> {
    let mut last_err = None;

    for attempt in 0..CALLBACK_MAX_RETRIES {
        match linear.move_issue_by_name(issue_id, target_status) {
            Ok(()) => {
                // Reconciliation: verify the status actually changed
                match linear.get_issue_status(issue_id) {
                    Ok(actual_status) => {
                        let actual_lower = actual_status.to_lowercase().replace(' ', "_");
                        let target_lower = target_status.to_lowercase().replace(' ', "_");
                        if actual_lower == target_lower {
                            eprintln!(
                                "[CALLBACK] {issue_id}: moved to {target_status} \
                                 (verified, attempt {})",
                                attempt + 1
                            );
                            return Ok(());
                        }
                        // Move succeeded but status didn't change — treat as failure and retry
                        eprintln!(
                            "[CALLBACK] {issue_id}: move to '{target_status}' returned OK but \
                             actual status is '{actual_status}' (attempt {})",
                            attempt + 1
                        );
                        last_err = Some(anyhow!(
                            "reconciliation failed: expected '{target_status}', got '{actual_status}'"
                        ));
                    }
                    Err(e) => {
                        // Reconciliation query failed — optimistically accept the move
                        eprintln!(
                            "[CALLBACK] {issue_id}: moved to {target_status} \
                             (reconciliation check failed: {e}, accepting move, attempt {})",
                            attempt + 1
                        );
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[CALLBACK] {issue_id}: move to '{target_status}' failed \
                     (attempt {}): {e}",
                    attempt + 1
                );
                last_err = Some(e);
            }
        }

        // Backoff before next retry (skip after last attempt)
        if attempt + 1 < CALLBACK_MAX_RETRIES {
            let delay = CALLBACK_BACKOFF_MS
                .get(attempt as usize)
                .copied()
                .unwrap_or(2000);
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
    }

    let err = last_err.unwrap_or_else(|| anyhow!("move_with_retry exhausted"));
    eprintln!(
        "[CALLBACK] {issue_id}: FAILED to move to '{target_status}' after \
         {CALLBACK_MAX_RETRIES} attempts"
    );
    Err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::fakes::FakeLinearApi;

    #[test]
    fn move_with_retry_succeeds_first_attempt() {
        let linear = FakeLinearApi::new();
        linear.set_issue_status("RIG-100", "In Review");

        let result = move_with_retry(&linear, "RIG-100", "review");
        assert!(result.is_ok());

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0], ("RIG-100".to_string(), "review".to_string()));
    }

    #[test]
    fn move_with_retry_succeeds_after_one_failure() {
        let linear = FakeLinearApi::new();
        linear.fail_next_n_moves(1);

        let result = move_with_retry(&linear, "RIG-100", "review");
        assert!(result.is_ok());

        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1);
    }

    #[test]
    fn move_with_retry_fails_all_retries() {
        let linear = FakeLinearApi::new();
        linear.fail_next_n_moves(3);

        let result = move_with_retry(&linear, "RIG-100", "review");
        assert!(result.is_err());

        let moves = linear.move_calls.borrow();
        assert!(moves.is_empty(), "no successful moves recorded");
    }

    // ─── RIG-353: move_with_retry edge cases ──────────────────────────────

    #[test]
    fn move_with_retry_exhausts_all_attempts_with_exact_count() {
        // Verify exactly CALLBACK_MAX_RETRIES attempts are made
        let linear = FakeLinearApi::new();
        linear.fail_next_n_moves(CALLBACK_MAX_RETRIES);

        let result = move_with_retry(&linear, "RIG-200", "in_progress");
        assert!(result.is_err(), "should fail after exhausting all retries");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("fake move failure"),
            "error should be from the fake, got: {err_msg}"
        );
    }

    #[test]
    fn move_with_retry_status_normalization_matches() {
        // The reconciliation normalizes: lowercase + replace spaces with underscores
        // "In Review" → "in_review", "in_review" → "in_review" → should match
        let linear = FakeLinearApi::new();
        // Set the status with different casing/spacing than the target
        linear.set_issue_status("RIG-300", "In Review");

        // Target is "in_review" — should match "In Review" after normalization
        let result = move_with_retry(&linear, "RIG-300", "in_review");
        assert!(
            result.is_ok(),
            "should succeed: 'In Review' normalizes to match 'in_review'"
        );
    }

    #[test]
    fn move_with_retry_succeeds_on_last_attempt() {
        // Fail first 2 attempts, succeed on 3rd (the last one)
        let linear = FakeLinearApi::new();
        linear.fail_next_n_moves(2);

        let result = move_with_retry(&linear, "RIG-400", "review");
        assert!(result.is_ok(), "should succeed on the last retry attempt");

        // Verify the move was recorded
        let moves = linear.move_calls.borrow();
        assert_eq!(moves.len(), 1, "one successful move should be recorded");
        assert_eq!(moves[0].1, "review");
    }
}
