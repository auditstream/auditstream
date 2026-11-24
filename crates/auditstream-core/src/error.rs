//! Domain error types for `auditstream-core`.

use thiserror::Error;

/// Errors returned when constructing or validating Auditstream domain types.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Actor `id` was empty.
    #[error("actor id must not be empty")]
    EmptyActorId,

    /// Actor `tenant` was present but empty.
    #[error("actor tenant must not be empty")]
    EmptyTenant,

    /// Nested `onBehalfOf` exceeded the one-level nesting limit.
    #[error("actor onBehalfOf cannot be nested more than one level deep")]
    NestedOnBehalfOf,

    /// A present optional context field was empty.
    #[error("context.{field} must not be empty")]
    EmptyContextField {
        /// The offending context field name.
        field: &'static str,
    },

    /// Serialized actor plus context JSON exceeded the 512-byte UTF-8 limit.
    #[error("serialized actor payload is {size} UTF-8 bytes, exceeding the {limit}-byte limit")]
    PayloadTooLarge {
        /// Actual compact UTF-8 JSON byte length.
        size: usize,
        /// Maximum allowed byte length (`512`).
        limit: usize,
    },

    /// Application event action was empty or whitespace-only.
    #[error("event action must not be empty or whitespace-only")]
    EmptyAction,

    /// Application event action exceeded the 64-character limit.
    #[error("event action length {len} exceeds maximum of {max} characters")]
    ActionTooLong {
        /// Actual character count.
        len: usize,
        /// Maximum allowed character count (`64`).
        max: usize,
    },

    /// Application event action collided with a reserved SQL DML operation name.
    #[error("event action '{action}' is reserved for database capture operations")]
    ReservedAction {
        /// The rejected action name.
        action: String,
    },

    /// Underlying JSON serialization or deserialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
