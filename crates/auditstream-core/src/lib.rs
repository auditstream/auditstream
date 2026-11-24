//! Core domain types, port traits, actor transport codec, diff and redaction helpers.
//!
//! All domain types match Auditstream Specification `0.1.0` (`spec/`). This crate is pure
//! data and logic and never depends on async runtimes, database drivers, or I/O crates.

pub mod actor;
pub mod batch;
pub mod context;
pub mod cursor;
pub mod error;
pub mod event;
pub mod op;
pub mod ports;

pub use crate::actor::{
    Actor, ActorPayload, ActorType, MAX_PAYLOAD_BYTES, NestedActor, validate_actor_payload,
};
pub use crate::batch::Batch;
pub use crate::context::Context;
pub use crate::cursor::Cursor;
pub use crate::error::{BoxError, CoreError, SinkError, SourceError};
pub use crate::event::{CURRENT_SCHEMA_VERSION, Event};
pub use crate::op::{ActorSource, MAX_ACTION_CHARS, Op, Source};
pub use crate::ports::{EventSource, Sink};

/// Crate identifier retained for workspace smoke checks.
pub const CRATE_NAME: &str = "auditstream-core";
