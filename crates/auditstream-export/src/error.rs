//! Error types returned by the `auditstream-export` batch pipeline.

use auditstream_core::{SinkError, SourceError};
use thiserror::Error;

/// Errors returned by [`crate::pipeline::run`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExportError {
    /// Reading the next batch from the [`EventSource`](auditstream_core::EventSource) failed.
    #[error("export source read failed: {0}")]
    Source(#[source] SourceError),

    /// Writing a batch to or flushing the [`Sink`](auditstream_core::Sink) failed.
    #[error("export sink failed: {0}")]
    Sink(#[from] SinkError),

    /// Acknowledging the cursor on the [`EventSource`](auditstream_core::EventSource) failed
    /// after the batch had already been flushed to the sink.
    #[error("export cursor acknowledgement failed: {0}")]
    Ack(#[source] SourceError),

    /// Invalid pipeline configuration was supplied (for example `batch_size == 0`).
    #[error("invalid export pipeline options: {0}")]
    InvalidOptions(String),
}
