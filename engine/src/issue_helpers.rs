//! Tracker-agnostic helpers for issue classification and routing.
//!
//! These functions operate on raw label strings and work with any issue tracker
//! (Linear, GitHub Issues). They are re-exported here from their canonical
//! implementation so that pipeline code does not need to import from
//! `crate::linear`, which is a Linear-specific module.

pub use crate::linear::{infer_working_dir, is_manual_issue, validate_working_dir};
