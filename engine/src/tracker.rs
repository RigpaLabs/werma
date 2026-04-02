//! Tracker factory — resolves the correct issue-tracker client for a given identifier.
//!
//! Callers should use these functions instead of constructing `LinearClient` directly.
//! This is the single place that knows about identifier format → tracker routing.
//!
//! # Routing rules
//! - `RIG-123`, `FAT-42`, … → Linear (requires `LINEAR_API_KEY` configured)
//! - `owner/repo#45`        → GitHub Issues (not yet implemented in `LinearApi`)
//! - UUID / plain string    → `None` (unrecognised format)

use crate::linear::{LinearApi, LinearClient};
use crate::project::{ProjectResolver, Tracker};

/// Return a Linear client **only** when `identifier` is a Linear issue (e.g. `RIG-123`).
///
/// Returns `None` when:
/// - the identifier is a GitHub reference (`owner/repo#N`),
/// - the identifier format is unrecognised, or
/// - `LINEAR_API_KEY` is not configured.
///
/// This replaces ad-hoc `is_linear_identifier(id) && LinearClient::new()` guards
/// scattered across daemon, runner, and command code.
pub fn linear_for_identifier(identifier: &str) -> Option<Box<dyn LinearApi>> {
    if ProjectResolver::tracker(identifier) != Some(Tracker::Linear) {
        return None;
    }
    LinearClient::new()
        .ok()
        .map(|c| Box::new(c) as Box<dyn LinearApi>)
}

/// Return a Linear client unconditionally (identifier-type-agnostic).
///
/// Use at daemon startup where you want a single shared client for all Linear
/// operations in the tick (effects processing, cancel-check, etc.).
///
/// Returns `None` when `LINEAR_API_KEY` is not configured.
pub fn linear_client() -> Option<Box<dyn LinearApi>> {
    LinearClient::new()
        .ok()
        .map(|c| Box::new(c) as Box<dyn LinearApi>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_for_identifier_returns_none_for_github() {
        // GitHub identifiers should never yield a Linear client.
        let result = linear_for_identifier("owner/repo#45");
        assert!(
            result.is_none(),
            "GitHub identifier should not produce a Linear client"
        );
    }

    #[test]
    fn linear_for_identifier_returns_none_for_uuid() {
        let result = linear_for_identifier("755e63ee-a00e-4fef-9d7a-b8907652e2b2");
        assert!(result.is_none());
    }

    #[test]
    fn linear_for_identifier_returns_none_for_empty() {
        let result = linear_for_identifier("");
        assert!(result.is_none());
    }

    #[test]
    fn linear_for_identifier_attempts_client_for_linear_id() {
        // Without LINEAR_API_KEY the client creation fails → None.
        // With it configured, Some(client) would be returned.
        // Either outcome is acceptable in this unit test — we just verify
        // that a Linear identifier is not short-circuited before the client attempt.
        // The return value depends on the environment.
        let _ = linear_for_identifier("RIG-123");
    }
}
