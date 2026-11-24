//! Postgres introspection, DDL and trigger generation, migration writers and history reader.

use sqlx as _;
use thiserror as _;
use tokio as _;

/// Placeholder marker for workspace bootstrap verification.
pub const CRATE_NAME: &str = "auditstream-pg";

/// Re-verify dependency wiring to `auditstream-core`.
pub fn core_crate_name() -> &'static str {
    auditstream_core::CRATE_NAME
}
