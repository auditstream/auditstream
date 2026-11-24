//! Core domain types, port traits, actor transport codec, diff and redaction helpers.

use serde as _;
use serde_json as _;
use thiserror as _;

/// Placeholder marker for workspace bootstrap verification.
pub const CRATE_NAME: &str = "auditstream-core";
