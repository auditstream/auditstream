//! Pull-based batch export pipeline (`next_batch` -> `write` -> `flush` -> `ack`).
//!
//! Delivery is **at least once**: a batch cursor is acknowledged on the [`EventSource`]
//! only after [`Sink::flush`] returns `Ok(())`. If a process crashes or fails after flushing
//! and before acknowledging, a subsequent run re-exports the in-flight batch and downstream
//! consumers deduplicate using each event's stable `event_id`.

use auditstream_core::{CompiledRedaction, Cursor, EventSource, Sink};

use crate::error::ExportError;

/// Default maximum number of events fetched per batch.
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// Configuration options for [`run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Maximum number of events requested per [`EventSource::next_batch`] call (must be `>= 1`).
    pub batch_size: usize,
    /// Optional pre-compiled redaction rules applied to every event before [`Sink::write`].
    pub redaction: Option<CompiledRedaction>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            redaction: None,
        }
    }
}

impl Options {
    /// Creates pipeline [`Options`] with a custom `batch_size`.
    #[must_use]
    pub const fn with_batch_size(batch_size: usize) -> Self {
        Self {
            batch_size,
            redaction: None,
        }
    }

    /// Attaches pre-compiled redaction rules applied to every batch before [`Sink::write`].
    ///
    /// Empty rule sets are normalized to `None`.
    #[must_use]
    pub fn with_redaction(mut self, redaction: CompiledRedaction) -> Self {
        self.redaction = (!redaction.is_empty()).then_some(redaction);
        self
    }
}

/// Summary returned when [`run`] drains all currently ready batches from an [`EventSource`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Total number of batches returned by the source and processed to completion.
    pub batches: usize,
    /// Total number of events written and flushed across all processed batches.
    pub events: usize,
    /// Last cursor acknowledged on the source, or `None` if no cursor was returned.
    pub last_cursor: Option<Cursor>,
}

/// Runs a pull-based export loop over `source` and `sink` until `source.next_batch` returns
/// `Ok(None)`.
///
/// # Delivery guarantee
///
/// Delivery is **at least once**: for each batch, `run` executes:
///
/// 1. `source.next_batch(options.batch_size)`
/// 2. `redaction.apply(event)` for each event in the batch (when `options.redaction` is set)
/// 3. `sink.write(&batch)`
/// 4. `sink.flush()`
/// 5. `source.ack(&cursor)` (when the batch carries a cursor)
///
/// `source.ack` is invoked **only after** `sink.flush` has returned `Ok(())`. A fail-closed
/// redaction error aborts before `sink.write`, `sink.flush`, and `source.ack`. A failed
/// `write` skips both `flush` and `ack`; a failed `flush` skips `ack`. If a failure or crash
/// happens after `flush` persists data and before `ack` records the cursor, restarting from
/// the last persisted cursor re-emits the in-flight batch and never skips events. Downstream
/// consumers must deduplicate by `event_id`.
///
/// # Errors
///
/// - [`ExportError::InvalidOptions`] if `options.batch_size == 0`.
/// - [`ExportError::Source`] if [`EventSource::next_batch`] fails.
/// - [`ExportError::Redact`] if a matching redaction rule fails closed on a non-object or
///   malformed JSON payload (aborts before `sink.write` and `source.ack`).
/// - [`ExportError::Sink`] if [`Sink::write`] or [`Sink::flush`] fails.
/// - [`ExportError::Ack`] if [`EventSource::ack`] fails after a successful flush.
///
/// # Examples
///
/// ```
/// use std::future::{Future, ready};
/// use auditstream_core::{Batch, Cursor, EventSource, Sink, SinkError, SourceError};
/// use auditstream_export::pipeline::{Options, run};
///
/// struct EmptySource;
/// impl EventSource for EmptySource {
///     fn next_batch(&mut self, _max: usize) -> impl Future<Output = Result<Option<Batch>, SourceError>> + Send {
///         ready(Ok(None))
///     }
///     fn ack(&mut self, _upto: &Cursor) -> impl Future<Output = Result<(), SourceError>> + Send {
///         ready(Ok(()))
///     }
/// }
///
/// struct NoopSink;
/// impl Sink for NoopSink {
///     fn write(&mut self, _batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send {
///         ready(Ok(()))
///     }
///     fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send {
///         ready(Ok(()))
///     }
/// }
///
/// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
/// let mut source = EmptySource;
/// let mut sink = NoopSink;
/// let report = run(&mut source, &mut sink, &Options::default()).await.unwrap();
/// assert_eq!(report.batches, 0);
/// assert_eq!(report.events, 0);
/// assert_eq!(report.last_cursor, None);
/// # });
/// ```
pub async fn run<Src, Snk>(
    source: &mut Src,
    sink: &mut Snk,
    options: &Options,
) -> Result<Report, ExportError>
where
    Src: EventSource,
    Snk: Sink,
{
    if options.batch_size == 0 {
        return Err(ExportError::InvalidOptions(
            "batch_size must be greater than 0".to_owned(),
        ));
    }

    let mut report = Report::default();

    while let Some(mut batch) = source
        .next_batch(options.batch_size)
        .await
        .map_err(ExportError::Source)?
    {
        if let Some(ref redaction) = options.redaction {
            for event in &mut batch.events {
                redaction.apply(event)?;
            }
        }

        sink.write(&batch).await?;
        sink.flush().await?;

        if let Some(ref cursor) = batch.cursor {
            source.ack(cursor).await.map_err(ExportError::Ack)?;
            report.last_cursor = Some(*cursor);
        }

        report.batches += 1;
        report.events += batch.events.len();
    }

    Ok(report)
}
