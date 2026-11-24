//! Placeholder RFC 4180 CSV (`csv`) sink; implemented in issue E2.

use std::future::{Future, ready};
use std::path::PathBuf;

use auditstream_core::{Batch, Sink, SinkError};

/// RFC 4180 CSV sink writing exported events to a file or stdout.
///
/// This is a placeholder stub wired into [`AnySink`](crate::AnySink); the streaming
/// writer implementation lands in issue E2 (`#31`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvSink {
    path: Option<PathBuf>,
}

impl CsvSink {
    /// Opens a placeholder [`CsvSink`] targeting `path` (or stdout when `None`).
    ///
    /// # Errors
    ///
    /// Currently succeeds to construct the placeholder; writing or flushing returns
    /// [`SinkError::Unimplemented`] referencing E2.
    pub fn open(path: Option<PathBuf>) -> Result<Self, SinkError> {
        Ok(Self { path })
    }

    /// Returns the configured destination path, or `None` for stdout.
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }
}

impl Sink for CsvSink {
    fn write(&mut self, _batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send {
        ready(Err(SinkError::Unimplemented(
            "CsvSink::write is not yet implemented (see issue E2)",
        )))
    }

    fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send {
        ready(Err(SinkError::Unimplemented(
            "CsvSink::flush is not yet implemented (see issue E2)",
        )))
    }
}
