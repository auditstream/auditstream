//! Export pipeline port traits (`EventSource` and `Sink`).
//!
//! Delivery is pull-based and **at least once**: pipelines read a [`Batch`] from an
//! [`EventSource`], pass it to [`Sink::write`], flush via [`Sink::flush`], and call
//! [`EventSource::ack`] **only after `flush()` succeeds**. If a process fails after
//! flushing and before acknowledging the cursor, a subsequent run may re-emit the batch,
//! and downstream consumers deduplicate on `event_id`.

use std::future::Future;

use crate::{Batch, Cursor, SinkError, SourceError};

/// Pull-based source of committed audit event batches.
///
/// Implementations provide **at-least-once delivery** ordered by `(xact_id, id)`.
/// Callers MUST invoke [`EventSource::ack`] **only after** the destination [`Sink::flush`]
/// has succeeded; acknowledging earlier would risk losing un-flushed events on crash.
pub trait EventSource: Send {
    /// Reads up to `max` committed events strictly after the current acknowledged cursor.
    ///
    /// Returns `Ok(None)` when no more committed events are currently available in the
    /// snapshot window.
    fn next_batch(
        &mut self,
        max: usize,
    ) -> impl Future<Output = Result<Option<Batch>, SourceError>> + Send;

    /// Persists the acknowledged cursor up to `upto`.
    ///
    /// Callers MUST call this only after [`Sink::flush`] has succeeded for the batch ending
    /// at `upto`, preserving Auditstream's at-least-once delivery guarantee.
    fn ack(&mut self, upto: &Cursor) -> impl Future<Output = Result<(), SourceError>> + Send;
}

/// Destination sink receiving exported audit event batches.
///
/// Export pipelines provide **at-least-once delivery**: callers write one or more batches
/// via [`Sink::write`], call [`Sink::flush`] to ensure durability, and only then call
/// [`EventSource::ack`].
///
/// # Examples
///
/// ```
/// use std::future::{Future, ready};
/// use std::pin::pin;
/// use std::task::{Context as TaskContext, Poll, Waker};
/// use auditstream_core::{Batch, Sink, SinkError};
///
/// struct CountingSink {
///     written: usize,
///     flushed: bool,
/// }
///
/// impl Sink for CountingSink {
///     fn write(&mut self, batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send {
///         self.written += batch.len();
///         self.flushed = false;
///         ready(Ok(()))
///     }
///
///     fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send {
///         self.flushed = true;
///         ready(Ok(()))
///     }
/// }
///
/// let mut sink = CountingSink { written: 0, flushed: false };
/// let waker = Waker::noop();
/// let mut cx = TaskContext::from_waker(waker);
///
/// let batch = Batch::default();
/// assert!(matches!(pin!(sink.write(&batch)).poll(&mut cx), Poll::Ready(Ok(()))));
/// assert!(matches!(pin!(sink.flush()).poll(&mut cx), Poll::Ready(Ok(()))));
/// assert!(sink.flushed);
/// ```
pub trait Sink: Send {
    /// Writes all events in `batch` to the sink buffer or destination.
    ///
    /// Data is not guaranteed durable until [`Sink::flush`] succeeds.
    fn write(&mut self, batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send;

    /// Flushes all previously written batches to durable storage.
    ///
    /// Callers MUST wait for `flush()` to return `Ok(())` before calling [`EventSource::ack`].
    fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send;
}
