use anyhow::{Result, anyhow};

use crate::linear::LinearApi;

/// Max retries for Linear status move operations.
const CALLBACK_MAX_RETRIES: u32 = 3;
/// Backoff delays in milliseconds between retries: 50ms, 100ms, 200ms.
const CALLBACK_BACKOFF_MS: [u64; 3] = [50, 100, 200];

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
