//! Placeholder JSON Lines (`jsonl`) sink; implemented in issue E1.

use std::future::{Future, ready};
use std::path::PathBuf;

use auditstream_core::{Batch, Sink, SinkError};

/// JSON Lines sink writing exported events to a file or stdout.
///
/// This is a placeholder stub wired into [`AnySink`](crate::AnySink); the streaming
/// writer implementation lands in issue E1 (`#30`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonLinesSink {
    path: Option<PathBuf>,
}

impl JsonLinesSink {
    /// Opens a placeholder [`JsonLinesSink`] targeting `path` (or stdout when `None`).
    ///
    /// # Errors
    ///
    /// Currently succeeds to construct the placeholder; writing or flushing returns
    /// [`SinkError::Unimplemented`] referencing E1.
    pub fn open(path: Option<PathBuf>) -> Result<Self, SinkError> {
        Ok(Self { path })
    }

    /// Returns the configured destination path, or `None` for stdout.
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }
}

impl Sink for JsonLinesSink {
    fn write(&mut self, _batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send {
        ready(Err(SinkError::Unimplemented(
            "JsonLinesSink::write is not yet implemented (see issue E1)",
        )))
    }

    fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send {
        ready(Err(SinkError::Unimplemented(
            "JsonLinesSink::flush is not yet implemented (see issue E1)",
        )))
    }
}
