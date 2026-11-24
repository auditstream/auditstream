//! Domain and port error types for `auditstream-core`.

use std::error::Error as StdError;

use thiserror::Error;

/// Boxed backend error type kept runtime- and driver-agnostic in `auditstream-core`.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

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

/// Errors returned by [`EventSource`](crate::EventSource) implementations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SourceError {
    /// An underlying storage or database driver error occurred.
    #[error("event source backend error: {message}")]
    Backend {
        /// Context describing the failed operation.
        message: String,
        /// Underlying backend error preserving the error source chain.
        #[source]
        source: BoxError,
    },

    /// Persisted or supplied cursor state was invalid.
    #[error("invalid export cursor: {0}")]
    InvalidCursor(String),

    /// A stored row could not be decoded into a domain [`Event`](crate::Event).
    #[error("failed to decode event row: {message}")]
    Decode {
        /// Context describing the decode failure.
        message: String,
        /// Underlying decode error preserving the error source chain.
        #[source]
        source: BoxError,
    },
}

impl SourceError {
    /// Wraps any `Send + Sync + 'static` backend error with context while preserving `.source()`.
    pub fn backend<E>(message: impl Into<String>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::Backend {
            message: message.into(),
            source: Box::new(source),
        }
    }

    /// Wraps a row-decoding error while preserving `.source()`.
    pub fn decode<E>(message: impl Into<String>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::Decode {
            message: message.into(),
            source: Box::new(source),
        }
    }
}

/// Errors returned by [`Sink`](crate::Sink) implementations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SinkError {
    /// An I/O error occurred while opening, writing, or flushing the sink.
    #[error("sink I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serializing an event batch failed.
    #[error("sink serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// An underlying sink backend error occurred.
    #[error("sink backend error: {message}")]
    Backend {
        /// Context describing the failed operation.
        message: String,
        /// Underlying backend error preserving the error source chain.
        #[source]
        source: BoxError,
    },

    /// A sink backend placeholder was invoked before its implementation landed.
    #[error("sink not yet implemented: {0}")]
    Unimplemented(&'static str),
}

impl SinkError {
    /// Wraps any `Send + Sync + 'static` backend error with context while preserving `.source()`.
    pub fn backend<E>(message: impl Into<String>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::Backend {
            message: message.into(),
            source: Box::new(source),
        }
    }
}
