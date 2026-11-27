//! Streaming JSON Lines (`jsonl`) export sink matching `spec/export.md`.

use std::path::Path;

use auditstream_core::{Batch, Sink, SinkError};
use tokio::fs::File;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter, Stdout};

/// Streaming JSON Lines sink writing each [`Event`](auditstream_core::Event) as one compact
/// UTF-8 JSON line terminated with `\n`.
///
/// Serializes [`Event`](auditstream_core::Event) directly through `serde_json` (with
/// `raw_value` and `arbitrary_precision`) and strips insignificant ASCII whitespace outside
/// JSON string literals in place, so JSON columns (`record_id`, `before`, `after`, `actor`,
/// `context`) always emit on a single line without rounding numeric values or reordering
/// object keys.
#[derive(Debug)]
pub struct JsonLinesSink<W> {
    writer: BufWriter<W>,
    line_buf: Vec<u8>,
}

impl<W: AsyncWrite + Unpin + Send> JsonLinesSink<W> {
    /// Creates a new [`JsonLinesSink`] wrapping any asynchronous writer `W`.
    pub fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::new(writer),
            line_buf: Vec::with_capacity(512),
        }
    }

    /// Returns a shared reference to the underlying writer.
    pub fn get_ref(&self) -> &W {
        self.writer.get_ref()
    }

    /// Returns a mutable reference to the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        self.writer.get_mut()
    }

    /// Consumes the sink and returns the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer.into_inner()
    }
}

impl<W: AsyncWrite + Unpin + Send> Sink for JsonLinesSink<W> {
    async fn write(&mut self, batch: &Batch) -> Result<(), SinkError> {
        for event in &batch.events {
            self.line_buf.clear();
            serde_json::to_writer(&mut self.line_buf, event)?;
            crate::compact_json_bytes_in_place(&mut self.line_buf);
            self.line_buf.push(b'\n');
            self.writer.write_all(&self.line_buf).await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), SinkError> {
        self.writer.flush().await?;
        Ok(())
    }
}

/// Runtime-opened JSON Lines destination used by [`AnySink`](crate::AnySink).
///
/// For file targets, [`Sink::flush`] flushes buffered bytes and calls `sync_data()` so
/// an export cursor acknowledged after flush never points past data lost in a crash.
#[derive(Debug)]
pub enum OpenedJsonLinesSink {
    /// Writes JSON Lines to standard output.
    Stdout(JsonLinesSink<Stdout>),
    /// Appends JSON Lines to a durable file and calls `sync_data()` on flush.
    File(JsonLinesSink<File>),
}

impl OpenedJsonLinesSink {
    /// Opens a JSON Lines sink targeting `path` (creating or appending) or stdout when `None`.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError::Io`] if opening or creating the file fails.
    pub fn open(path: Option<&Path>) -> Result<Self, SinkError> {
        match path {
            None => Ok(Self::Stdout(JsonLinesSink::new(tokio::io::stdout()))),
            Some(file_path) => {
                let std_file = crate::open_append_file(file_path)?;
                Ok(Self::File(JsonLinesSink::new(File::from_std(std_file))))
            }
        }
    }
}

crate::impl_opened_sink!(OpenedJsonLinesSink);
