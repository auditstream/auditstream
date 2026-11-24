//! Streaming RFC 4180 CSV (`csv`) export sink matching `spec/export.md`.
//!
//! Records are encoded in the 15-column order fixed by `spec/export.md` with CRLF (`\r\n`)
//! terminators and UTF-8 without BOM.
//!
//! # Formula injection policy
//!
//! Field values starting with `=`, `+`, `-`, or `@` are written **as is** without prefixing
//! or escaping, because altering cell contents would corrupt the audit record. Downstream
//! spreadsheet consumers should import columns as plain text rather than evaluating formulas.

use std::borrow::Cow;
use std::io;
use std::path::Path;

use auditstream_core::{ActorSource, Batch, Event, Sink, SinkError, Source};
use csv::{Terminator, WriterBuilder};
use time::{OffsetDateTime, UtcOffset};
use tokio::fs::File;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter, Stdout};

/// Normative CSV header column names in the exact order required by `spec/export.md`.
pub const CSV_HEADERS: [&str; 15] = [
    "event_id",
    "occurred_at",
    "source",
    "schema_name",
    "table_name",
    "op",
    "record_id",
    "before",
    "after",
    "changed",
    "actor",
    "context",
    "actor_source",
    "db_user",
    "schema_version",
];

/// Streaming RFC 4180 CSV sink writing each [`Event`] as one CRLF-terminated record.
///
/// - JSON-valued columns (`record_id`, `before`, `after`, `actor`, `context`) are taken
///   directly from [`serde_json::value::RawValue`] without going through `serde_json::Value`,
///   so arbitrary-precision PostgreSQL `numeric` values and object key order survive
///   byte-for-byte.
/// - SQL `NULL` (`None`) is emitted as an empty unquoted field.
/// - Values beginning with `=`, `+`, `-`, or `@` are written as is without formula escaping,
///   because mutating values would alter audit data.
#[derive(Debug)]
pub struct CsvSink<W> {
    writer: BufWriter<W>,
    header_written: bool,
    batch_buf: Vec<u8>,
}

impl<W: AsyncWrite + Unpin + Send> CsvSink<W> {
    /// Creates a new [`CsvSink`] wrapping any asynchronous writer `W` that writes the
    /// [`CSV_HEADERS`] row before the first emitted record.
    pub fn new(writer: W) -> Self {
        Self::with_header(writer, true)
    }

    /// Creates a new [`CsvSink`] controlling whether the header row should be written before
    /// the first emitted record (`false` when appending to an already non-empty file).
    pub fn with_header(writer: W, write_header: bool) -> Self {
        Self {
            writer: BufWriter::new(writer),
            header_written: !write_header,
            batch_buf: Vec::with_capacity(4096),
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

impl<W: AsyncWrite + Unpin + Send> Sink for CsvSink<W> {
    async fn write(&mut self, batch: &Batch) -> Result<(), SinkError> {
        if batch.events.is_empty() {
            return Ok(());
        }

        self.batch_buf.clear();
        {
            let mut csv_writer = WriterBuilder::new()
                .terminator(Terminator::CRLF)
                .from_writer(&mut self.batch_buf);

            if !self.header_written {
                csv_writer
                    .write_record(CSV_HEADERS)
                    .map_err(csv_to_sink_error)?;
                self.header_written = true;
            }

            for event in &batch.events {
                write_event_record(&mut csv_writer, event)?;
            }
            csv_writer.flush().map_err(SinkError::Io)?;
        }

        self.writer.write_all(&self.batch_buf).await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), SinkError> {
        self.writer.flush().await?;
        Ok(())
    }
}

fn write_event_record<W: io::Write>(
    writer: &mut csv::Writer<W>,
    event: &Event,
) -> Result<(), SinkError> {
    let event_id = event.event_id.to_string();
    let occurred_at = format_rfc3339_micros(event.occurred_at);
    let source = match event.source {
        Source::Db => "db",
        Source::App => "app",
    };
    let schema_name = event.schema_name.as_deref().unwrap_or("");
    let table_name = event.table_name.as_deref().unwrap_or("");
    let op = event.op.as_str();
    let record_id = event
        .record_id
        .as_deref()
        .map_or(Cow::Borrowed(""), |raw| compact_json(raw.get()));
    let before = event
        .before
        .as_deref()
        .map_or(Cow::Borrowed(""), |raw| compact_json(raw.get()));
    let after = event
        .after
        .as_deref()
        .map_or(Cow::Borrowed(""), |raw| compact_json(raw.get()));
    let changed = match &event.changed {
        Some(cols) => serde_json::to_string(cols)?,
        None => String::new(),
    };
    let actor = event
        .actor
        .as_deref()
        .map_or(Cow::Borrowed(""), |raw| compact_json(raw.get()));
    let context = event
        .context
        .as_deref()
        .map_or(Cow::Borrowed(""), |raw| compact_json(raw.get()));
    let actor_source = match event.actor_source {
        Some(ActorSource::SetConfig) => "set_config",
        Some(ActorSource::Comment) => "comment",
        Some(ActorSource::Jwt) => "jwt",
        Some(ActorSource::Invalid) => "invalid",
        None => "",
    };
    let schema_version = event.schema_version.to_string();

    writer
        .write_record([
            event_id.as_str(),
            occurred_at.as_str(),
            source,
            schema_name,
            table_name,
            op,
            record_id.as_ref(),
            before.as_ref(),
            after.as_ref(),
            changed.as_str(),
            actor.as_ref(),
            context.as_ref(),
            actor_source,
            event.db_user.as_str(),
            schema_version.as_str(),
        ])
        .map_err(csv_to_sink_error)?;
    Ok(())
}

/// Strips insignificant ASCII whitespace outside JSON string literals while keeping key order
/// and numeric literals byte-for-byte intact without parsing into `serde_json::Value`.
fn compact_json(raw: &str) -> Cow<'_, str> {
    if !raw
        .bytes()
        .any(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
    {
        return Cow::Borrowed(raw);
    }

    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if !matches!(ch, ' ' | '\n' | '\r' | '\t') {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

fn format_rfc3339_micros(dt: OffsetDateTime) -> String {
    let utc = dt.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.microsecond(),
    )
}

fn csv_to_sink_error(err: csv::Error) -> SinkError {
    match err.into_kind() {
        csv::ErrorKind::Io(io_err) => SinkError::Io(io_err),
        other => SinkError::Io(io::Error::other(format!("csv encoding failed: {other:?}"))),
    }
}

/// Runtime-opened CSV destination used by [`AnySink`](crate::AnySink).
///
/// Writes [`CSV_HEADERS`] once when targeting stdout or an empty file, and omits the header
/// when appending to a non-empty file. For file targets, [`Sink::flush`] flushes buffered
/// bytes and calls `sync_data()` so export cursors acknowledged after flush stay durable.
#[derive(Debug)]
pub enum OpenedCsvSink {
    /// Writes CSV records to standard output.
    Stdout(CsvSink<Stdout>),
    /// Appends CSV records to a durable file and calls `sync_data()` on flush.
    File(CsvSink<File>),
}

impl OpenedCsvSink {
    /// Opens a CSV sink targeting `path` (creating or appending) or stdout when `None`.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError::Io`] if opening, creating, or inspecting the file fails.
    pub fn open(path: Option<&Path>) -> Result<Self, SinkError> {
        match path {
            None => Ok(Self::Stdout(CsvSink::new(tokio::io::stdout()))),
            Some(file_path) => {
                let std_file = crate::open_append_file(file_path)?;
                let is_empty = std_file.metadata()?.len() == 0;
                Ok(Self::File(CsvSink::with_header(
                    File::from_std(std_file),
                    is_empty,
                )))
            }
        }
    }
}

crate::impl_opened_sink!(OpenedCsvSink);
