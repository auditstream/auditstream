use std::collections::VecDeque;
use std::error::Error as StdError;
use std::future::{Future, ready};
use std::io;
use std::pin::pin;
use std::task::{Context as TaskContext, Poll, Waker};

use auditstream_core::{
    ActorSource, Batch, CURRENT_SCHEMA_VERSION, Cursor, Event, EventSource, Op, Sink, SinkError,
    Source, SourceError,
};
use time::{Date, Month, PrimitiveDateTime, Time};
use uuid::Uuid;

fn block_on_ready<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);
    match pin!(fut).poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("in-memory future unexpectedly returned Pending"),
    }
}

fn assert_send<T: Send>(_: &T) {}
fn assert_error_bounds<E: StdError + Send + Sync + 'static>() {}

fn sample_event(id_byte: u8) -> Event {
    let occurred_at = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 24).expect("valid date"),
        Time::from_hms_micro(12, 0, u8::from(id_byte > 0), 0).expect("valid time"),
    )
    .assume_utc();

    Event {
        event_id: Uuid::from_bytes([id_byte; 16]),
        occurred_at,
        source: Source::Db,
        schema_name: Some("public".to_owned()),
        table_name: Some("invoice".to_owned()),
        op: Op::Insert,
        record_id: None,
        before: None,
        after: None,
        changed: None,
        actor: None,
        context: None,
        actor_source: Some(ActorSource::Comment),
        db_user: "app_user".to_owned(),
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

struct MemorySource {
    pending: VecDeque<Batch>,
    acked: Vec<Cursor>,
}

impl EventSource for MemorySource {
    fn next_batch(
        &mut self,
        max: usize,
    ) -> impl Future<Output = Result<Option<Batch>, SourceError>> + Send {
        let next = self.pending.pop_front().map(|mut batch| {
            batch.events.truncate(max);
            batch
        });
        ready(Ok(next))
    }

    fn ack(&mut self, upto: &Cursor) -> impl Future<Output = Result<(), SourceError>> + Send {
        self.acked.push(*upto);
        ready(Ok(()))
    }
}

struct MemorySink {
    buffered: Vec<Event>,
    flushed: Vec<Event>,
    flush_count: usize,
}

impl Sink for MemorySink {
    fn write(&mut self, batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send {
        self.buffered.extend(batch.events.iter().cloned());
        ready(Ok(()))
    }

    fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send {
        self.flushed.append(&mut self.buffered);
        self.flush_count += 1;
        ready(Ok(()))
    }
}

#[test]
fn moves_two_batches_through_in_memory_source_and_sink_with_send_futures() {
    let cursor_1 = Cursor::new(100, 1);
    let cursor_2 = Cursor::new(101, 2);

    let batch_1 = Batch::new(vec![sample_event(1)], Some(cursor_1));
    let batch_2 = Batch::new(vec![sample_event(2), sample_event(3)], Some(cursor_2));

    let mut source = MemorySource {
        pending: VecDeque::from([batch_1, batch_2]),
        acked: Vec::new(),
    };
    let mut sink = MemorySink {
        buffered: Vec::new(),
        flushed: Vec::new(),
        flush_count: 0,
    };

    let mut batches_moved = 0usize;
    loop {
        let next_fut = source.next_batch(10);
        assert_send(&next_fut);
        let Some(batch) = block_on_ready(next_fut).expect("next_batch ok") else {
            break;
        };

        let write_fut = sink.write(&batch);
        assert_send(&write_fut);
        block_on_ready(write_fut).expect("write ok");

        let flush_fut = sink.flush();
        assert_send(&flush_fut);
        block_on_ready(flush_fut).expect("flush ok");

        if let Some(cursor) = batch.cursor {
            let ack_fut = source.ack(&cursor);
            assert_send(&ack_fut);
            block_on_ready(ack_fut).expect("ack ok");
        }
        batches_moved += 1;
    }

    assert_eq!(batches_moved, 2);
    assert_eq!(sink.flush_count, 2);
    assert_eq!(sink.buffered.len(), 0);
    assert_eq!(sink.flushed.len(), 3);
    assert_eq!(source.acked, vec![cursor_1, cursor_2]);
}

#[test]
fn port_errors_are_send_sync_static_and_preserve_source_chain() {
    assert_error_bounds::<SourceError>();
    assert_error_bounds::<SinkError>();

    let io_err = io::Error::new(io::ErrorKind::BrokenPipe, "connection reset");
    let src_err = SourceError::backend("reading history window", io_err);
    assert!(src_err.to_string().contains("reading history window"));
    let cause = src_err.source().expect("source chain preserved");
    assert!(cause.to_string().contains("connection reset"));

    let sink_io = SinkError::from(io::Error::new(io::ErrorKind::PermissionDenied, "read-only"));
    assert!(sink_io.source().is_some());

    let sink_backend = SinkError::backend(
        "uploading chunk",
        io::Error::new(io::ErrorKind::TimedOut, "gateway timeout"),
    );
    assert!(
        sink_backend
            .source()
            .expect("sink backend source preserved")
            .to_string()
            .contains("gateway timeout")
    );
}
