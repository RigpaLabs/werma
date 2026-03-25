pub(crate) mod api;
pub(crate) mod config;
pub(crate) mod helpers;
mod queries;
mod setup;
mod sync;

// Re-export the primary public API — callers use `crate::linear::*` unchanged.
pub use api::{LinearApi, LinearClient};
pub use config::configured_team_keys;
pub use helpers::{infer_working_dir, is_manual_issue, validate_working_dir};

#[cfg(test)]
pub mod fakes;

#[cfg(test)]
mod tests;
