//! Export sinks (JSON Lines, CSV) and batch export pipeline.

use thiserror as _;
use tokio as _;

/// Placeholder marker for workspace bootstrap verification.
pub const CRATE_NAME: &str = "auditstream-export";

/// Re-verify dependency wiring to `auditstream-core`.
pub fn core_crate_name() -> &'static str {
    auditstream_core::CRATE_NAME
}
