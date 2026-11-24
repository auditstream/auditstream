use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use auditstream_core::{
    ActorSource, Batch, CURRENT_SCHEMA_VERSION, Cursor, Event, EventSource, Op, Sink, SinkError,
    Source, SourceError,
};
use auditstream_export::error::ExportError;
use auditstream_export::jsonl::JsonLinesSink;
use auditstream_export::pipeline::{Options, run};
use proptest::prelude::*;
use serde_json::value::RawValue;
use time::{Date, Month, PrimitiveDateTime, Time};
use uuid::Uuid;

fn make_event(seq: usize) -> Event {
    let seq_u128 = u128::try_from(seq).expect("seq fits u128");
    let ts = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 24).expect("date"),
        Time::from_hms_micro(12, 0, 0, u32::try_from(seq % 1_000_000).expect("micro"))
            .expect("time"),
    )
    .assume_utc();
    Event {
        event_id: Uuid::from_u128(seq_u128 + 1),
        occurred_at: ts,
        source: Source::Db,
        schema_name: Some("public".to_owned()),
        table_name: Some("invoice".to_owned()),
        op: Op::Insert,
        record_id: Some(
            RawValue::from_string(format!(r#"{{"id":{}}}"#, seq + 1)).expect("record_id"),
        ),
        before: None,
        after: Some(
            RawValue::from_string(format!(r#"{{"id":{},"total":10.50}}"#, seq + 1)).expect("after"),
        ),
        changed: None,
        actor: Some(
            RawValue::from_string(r#"{"type":"user","id":"u_1"}"#.to_owned()).expect("actor"),
        ),
        context: None,
        actor_source: Some(ActorSource::Comment),
        db_user: "app_user".to_owned(),
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    NextBatch(usize),
    Write(usize),
    Flush,
    Ack(Cursor),
}

struct RecordingSource {
    batches: Vec<Batch>,
    idx: usize,
    fail_next_batch: bool,
    fail_ack: bool,
    log: Arc<Mutex<Vec<Call>>>,
}

impl EventSource for RecordingSource {
    fn next_batch(
        &mut self,
        max: usize,
    ) -> impl Future<Output = Result<Option<Batch>, SourceError>> {
        self.log
            .lock()
            .expect("log lock")
            .push(Call::NextBatch(max));
        if self.fail_next_batch {
            return std::future::ready(Err(SourceError::backend(
                "injected next_batch error",
                io::Error::other("source read boom"),
            )));
        }
        if self.idx >= self.batches.len() {
            return std::future::ready(Ok(None));
        }
        let batch = self.batches[self.idx].clone();
        self.idx += 1;
        std::future::ready(Ok(Some(batch)))
    }

    fn ack(&mut self, upto: &Cursor) -> impl Future<Output = Result<(), SourceError>> {
        self.log.lock().expect("log lock").push(Call::Ack(*upto));
        if self.fail_ack {
            return std::future::ready(Err(SourceError::backend(
                "injected ack error",
                io::Error::other("ack boom"),
            )));
        }
        std::future::ready(Ok(()))
    }
}

struct RecordingSink {
    fail_write: bool,
    fail_flush: bool,
    log: Arc<Mutex<Vec<Call>>>,
}

impl Sink for RecordingSink {
    fn write(&mut self, batch: &Batch) -> impl Future<Output = Result<(), SinkError>> {
        self.log
            .lock()
            .expect("log lock")
            .push(Call::Write(batch.events.len()));
        if self.fail_write {
            return std::future::ready(Err(SinkError::Io(io::Error::other("write boom"))));
        }
        std::future::ready(Ok(()))
    }

    fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> {
        self.log.lock().expect("log lock").push(Call::Flush);
        if self.fail_flush {
            return std::future::ready(Err(SinkError::Io(io::Error::other("flush boom"))));
        }
        std::future::ready(Ok(()))
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn recording_doubles_verify_exact_sequence_on_success_and_each_failure_mode() {
    let c1 = Cursor::new(10, 1);
    let c2 = Cursor::new(20, 2);
    let b1 = Batch::new(vec![make_event(0), make_event(1)], Some(c1));
    let b2 = Batch::new(vec![make_event(2)], Some(c2));

    // 1. Happy path: next_batch -> write -> flush -> ack per batch, then None.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut source = RecordingSource {
        batches: vec![b1.clone(), b2.clone()],
        idx: 0,
        fail_next_batch: false,
        fail_ack: false,
        log: Arc::clone(&log),
    };
    let mut sink = RecordingSink {
        fail_write: false,
        fail_flush: false,
        log: Arc::clone(&log),
    };
    let report = run(&mut source, &mut sink, &Options::with_batch_size(2))
        .await
        .expect("happy run");
    assert_eq!(report.batches, 2);
    assert_eq!(report.events, 3);
    assert_eq!(report.last_cursor, Some(c2));
    assert_eq!(
        *log.lock().expect("log"),
        vec![
            Call::NextBatch(2),
            Call::Write(2),
            Call::Flush,
            Call::Ack(c1),
            Call::NextBatch(2),
            Call::Write(1),
            Call::Flush,
            Call::Ack(c2),
            Call::NextBatch(2),
        ]
    );

    // 2. Failed write skips flush AND ack.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut source = RecordingSource {
        batches: vec![b1.clone()],
        idx: 0,
        fail_next_batch: false,
        fail_ack: false,
        log: Arc::clone(&log),
    };
    let mut sink = RecordingSink {
        fail_write: true,
        fail_flush: false,
        log: Arc::clone(&log),
    };
    let err = run(&mut source, &mut sink, &Options::with_batch_size(2))
        .await
        .expect_err("write must fail");
    assert!(matches!(err, ExportError::Sink(_)));
    assert_eq!(
        *log.lock().expect("log"),
        vec![Call::NextBatch(2), Call::Write(2)]
    );

    // 3. Failed flush runs write, skips ack.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut source = RecordingSource {
        batches: vec![b1.clone()],
        idx: 0,
        fail_next_batch: false,
        fail_ack: false,
        log: Arc::clone(&log),
    };
    let mut sink = RecordingSink {
        fail_write: false,
        fail_flush: true,
        log: Arc::clone(&log),
    };
    let err = run(&mut source, &mut sink, &Options::with_batch_size(2))
        .await
        .expect_err("flush must fail");
    assert!(matches!(err, ExportError::Sink(_)));
    assert_eq!(
        *log.lock().expect("log"),
        vec![Call::NextBatch(2), Call::Write(2), Call::Flush]
    );

    // 4. Failed source read skips write, flush, ack.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut source = RecordingSource {
        batches: vec![b1.clone()],
        idx: 0,
        fail_next_batch: true,
        fail_ack: false,
        log: Arc::clone(&log),
    };
    let mut sink = RecordingSink {
        fail_write: false,
        fail_flush: false,
        log: Arc::clone(&log),
    };
    let err = run(&mut source, &mut sink, &Options::with_batch_size(2))
        .await
        .expect_err("source read must fail");
    assert!(matches!(err, ExportError::Source(_)));
    assert_eq!(*log.lock().expect("log"), vec![Call::NextBatch(2)]);

    // 5. Failed ack runs after write & flush and returns ExportError::Ack.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut source = RecordingSource {
        batches: vec![b1],
        idx: 0,
        fail_next_batch: false,
        fail_ack: true,
        log: Arc::clone(&log),
    };
    let mut sink = RecordingSink {
        fail_write: false,
        fail_flush: false,
        log: Arc::clone(&log),
    };
    let err = run(&mut source, &mut sink, &Options::with_batch_size(2))
        .await
        .expect_err("ack must fail");
    assert!(matches!(err, ExportError::Ack(_)));
    assert_eq!(
        *log.lock().expect("log"),
        vec![
            Call::NextBatch(2),
            Call::Write(2),
            Call::Flush,
            Call::Ack(c1)
        ]
    );

    // 6. Zero batch_size returns ExportError::InvalidOptions without panicking.
    let err = run(&mut source, &mut sink, &Options::with_batch_size(0))
        .await
        .expect_err("zero batch_size must fail");
    assert!(matches!(err, ExportError::InvalidOptions(_)));
}

/// In-memory [`EventSource`] whose acknowledged cursor survives restarts via `acked_store`.
struct MemorySource {
    records: Vec<(Cursor, Event)>,
    read_pos: Cursor,
    acked_store: Arc<Mutex<Cursor>>,
    fail_ack_on_batch: Option<usize>,
    emitted_batches: usize,
}

impl MemorySource {
    fn from_store(
        records: Vec<(Cursor, Event)>,
        acked_store: Arc<Mutex<Cursor>>,
        fail_ack_on_batch: Option<usize>,
    ) -> Self {
        let start = *acked_store.lock().expect("acked lock");
        Self {
            records,
            read_pos: start,
            acked_store,
            fail_ack_on_batch,
            emitted_batches: 0,
        }
    }
}

impl EventSource for MemorySource {
    fn next_batch(
        &mut self,
        max: usize,
    ) -> impl Future<Output = Result<Option<Batch>, SourceError>> {
        let mut events = Vec::new();
        let mut last_cursor = None;

        for (cursor, event) in &self.records {
            if *cursor <= self.read_pos {
                continue;
            }
            events.push(event.clone());
            last_cursor = Some(*cursor);
            if events.len() == max {
                break;
            }
        }

        if events.is_empty() {
            return std::future::ready(Ok(None));
        }

        let cursor = last_cursor.expect("non-empty batch has cursor");
        self.read_pos = cursor;
        self.emitted_batches += 1;
        std::future::ready(Ok(Some(Batch::new(events, Some(cursor)))))
    }

    fn ack(&mut self, upto: &Cursor) -> impl Future<Output = Result<(), SourceError>> {
        if self.fail_ack_on_batch == Some(self.emitted_batches) {
            return std::future::ready(Err(SourceError::backend(
                "injected ack failure",
                io::Error::other("cursor store unavailable"),
            )));
        }
        let mut guard = self.acked_store.lock().expect("acked lock");
        if *upto > *guard {
            *guard = *upto;
        }
        std::future::ready(Ok(()))
    }
}

#[derive(Debug, Clone, Copy)]
enum CrashPoint {
    InWrite,
    AfterWrite,
    InFlush,
    AfterFlush,
    InAck,
    DropMidFlush,
}

/// In-memory [`Sink`] that separates volatile buffered events from durable flushed output
/// and can inject failures or a pending future at a chosen batch number.
struct FaultySink {
    volatile: Vec<Event>,
    durable: Arc<Mutex<Vec<Event>>>,
    write_calls: usize,
    flush_calls: usize,
    fault_batch: Option<usize>,
    crash_point: Option<CrashPoint>,
}

impl FaultySink {
    fn new(
        durable: Arc<Mutex<Vec<Event>>>,
        fault_batch: Option<usize>,
        crash_point: Option<CrashPoint>,
    ) -> Self {
        Self {
            volatile: Vec::new(),
            durable,
            write_calls: 0,
            flush_calls: 0,
            fault_batch,
            crash_point,
        }
    }
}

struct YieldForever;

impl Future for YieldForever {
    type Output = Result<(), SinkError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Sink for FaultySink {
    fn write(&mut self, batch: &Batch) -> impl Future<Output = Result<(), SinkError>> {
        self.write_calls += 1;
        if self.fault_batch == Some(self.write_calls) {
            match self.crash_point {
                Some(CrashPoint::InWrite) => {
                    return std::future::ready(Err(SinkError::Io(io::Error::other(
                        "crash in write",
                    ))));
                }
                Some(CrashPoint::AfterWrite) => {
                    self.volatile.extend(batch.events.iter().cloned());
                    return std::future::ready(Err(SinkError::Io(io::Error::other(
                        "crash after write",
                    ))));
                }
                _ => {}
            }
        }
        self.volatile.extend(batch.events.iter().cloned());
        std::future::ready(Ok(()))
    }

    async fn flush(&mut self) -> Result<(), SinkError> {
        self.flush_calls += 1;
        if self.fault_batch == Some(self.flush_calls) {
            match self.crash_point {
                Some(CrashPoint::InFlush) => {
                    return Err(SinkError::Io(io::Error::other("crash in flush")));
                }
                Some(CrashPoint::AfterFlush) => {
                    self.durable
                        .lock()
                        .expect("durable lock")
                        .append(&mut self.volatile);
                    return Err(SinkError::Io(io::Error::other("crash after flush")));
                }
                Some(CrashPoint::DropMidFlush) => {
                    // Simulate mid-flush stall where the future is dropped before completing.
                    return YieldForever.await;
                }
                _ => {}
            }
        }
        self.durable
            .lock()
            .expect("durable lock")
            .append(&mut self.volatile);
        Ok(())
    }
}

fn crash_point_strategy() -> impl Strategy<Value = CrashPoint> {
    prop_oneof![
        Just(CrashPoint::InWrite),
        Just(CrashPoint::AfterWrite),
        Just(CrashPoint::InFlush),
        Just(CrashPoint::AfterFlush),
        Just(CrashPoint::InAck),
        Just(CrashPoint::DropMidFlush),
    ]
}

proptest! {
    #[test]
    fn crash_and_restart_preserves_every_event_in_source_order_and_only_in_flight_batch_duplicates(
        event_count in 1usize..=40,
        batch_size in 1usize..=11,
        crash_batch_raw in 0usize..=40,
        crash_point in crash_point_strategy(),
    ) {
        let total_batches = event_count.div_ceil(batch_size);
        let crash_batch = (crash_batch_raw % total_batches) + 1; // 1-indexed batch number

        let records: Vec<(Cursor, Event)> = (0..event_count)
            .map(|i| {
                let id = i64::try_from(i + 1).expect("id");
                let xact = u64::try_from(100 + (i / 3)).expect("xact");
                (Cursor::new(xact, id), make_event(i))
            })
            .collect();

        let in_flight_start = (crash_batch - 1) * batch_size;
        let in_flight_end = (in_flight_start + batch_size).min(event_count);
        let in_flight_ids: HashSet<Uuid> = records[in_flight_start..in_flight_end]
            .iter()
            .map(|(_, ev)| ev.event_id)
            .collect();

        let acked_store = Arc::new(Mutex::new(Cursor::new(0, 0)));
        let durable = Arc::new(Mutex::new(Vec::<Event>::new()));

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("tokio rt");

        // Phase 1: run until the chosen crash point triggers.
        match crash_point {
            CrashPoint::DropMidFlush => {
                let mut source = MemorySource::from_store(records.clone(), Arc::clone(&acked_store), None);
                let mut sink = FaultySink::new(Arc::clone(&durable), Some(crash_batch), Some(crash_point));
                let waker = Waker::noop();
                let mut cx = Context::from_waker(waker);
                let opts = Options::with_batch_size(batch_size);
                let mut fut = Box::pin(run(&mut source, &mut sink, &opts));
                let poll_res = fut.as_mut().poll(&mut cx);
                prop_assert!(
                    poll_res.is_pending(),
                    "DropMidFlush must pause mid-flush before being dropped"
                );
                // Dropping `fut` + `sink` discards volatile data without acking the in-flight batch.
                drop(fut);
            }
            CrashPoint::InAck => {
                let mut source = MemorySource::from_store(
                    records.clone(),
                    Arc::clone(&acked_store),
                    Some(crash_batch),
                );
                let mut sink = FaultySink::new(Arc::clone(&durable), None, None);
                let res = rt.block_on(run(&mut source, &mut sink, &Options::with_batch_size(batch_size)));
                prop_assert!(matches!(res, Err(ExportError::Ack(_))));
            }
            _ => {
                let mut source = MemorySource::from_store(records.clone(), Arc::clone(&acked_store), None);
                let mut sink = FaultySink::new(Arc::clone(&durable), Some(crash_batch), Some(crash_point));
                let res = rt.block_on(run(&mut source, &mut sink, &Options::with_batch_size(batch_size)));
                prop_assert!(matches!(res, Err(ExportError::Sink(_))));
            }
        }

        // Phase 2: restart from the persisted acked cursor with a healthy sink sharing `durable`.
        let mut restarted_source = MemorySource::from_store(records.clone(), Arc::clone(&acked_store), None);
        let mut healthy_sink = FaultySink::new(Arc::clone(&durable), None, None);
        let report = rt
            .block_on(run(
                &mut restarted_source,
                &mut healthy_sink,
                &Options::with_batch_size(batch_size),
            ))
            .expect("restarted export succeeds");
        prop_assert_eq!(report.last_cursor, Some(records.last().expect("non-empty").0));

        // Verify at-least-once guarantees on durable output.
        let durable_events = durable.lock().expect("durable").clone();
        let expected_order: Vec<Uuid> = records.iter().map(|(_, ev)| ev.event_id).collect();

        let mut counts: HashMap<Uuid, usize> = HashMap::new();
        let mut first_seen_order: Vec<Uuid> = Vec::with_capacity(event_count);
        for ev in &durable_events {
            let entry = counts.entry(ev.event_id).or_insert(0);
            if *entry == 0 {
                first_seen_order.push(ev.event_id);
            }
            *entry += 1;
        }

        // Every event_id present in exact source order of first appearance!
        prop_assert_eq!(first_seen_order, expected_order);

        // Only events in the single in-flight batch at the crash may appear twice (never >2).
        for (id, count) in counts {
            if in_flight_ids.contains(&id) {
                prop_assert!(
                    count == 1 || count == 2,
                    "in-flight event {} appeared {} times",
                    id,
                    count
                );
            } else {
                prop_assert_eq!(
                    count,
                    1,
                    "non-in-flight event {} must appear exactly once",
                    id
                );
            }
        }
    }
}

#[tokio::test]
async fn pipeline_streams_into_real_jsonl_sink_one_line_per_event_in_source_order() {
    let records: Vec<(Cursor, Event)> = (0..7)
        .map(|i| {
            let id = i64::try_from(i + 1).expect("id");
            let xact = u64::try_from(500 + i).expect("xact");
            (Cursor::new(xact, id), make_event(i))
        })
        .collect();
    let acked = Arc::new(Mutex::new(Cursor::new(0, 0)));
    let mut source = MemorySource::from_store(records.clone(), Arc::clone(&acked), None);
    let mut sink = JsonLinesSink::new(Vec::<u8>::new());

    let report = run(&mut source, &mut sink, &Options::with_batch_size(3))
        .await
        .expect("jsonl pipeline run");

    assert_eq!(report.batches, 3);
    assert_eq!(report.events, 7);
    assert_eq!(report.last_cursor, Some(records[6].0));
    assert_eq!(*acked.lock().expect("acked"), records[6].0);

    let output = String::from_utf8(sink.into_inner()).expect("utf8 jsonl");
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 7);

    for (idx, line) in lines.iter().enumerate() {
        let parsed: Event = serde_json::from_str(line).expect("valid event json line");
        assert_eq!(parsed.event_id, records[idx].1.event_id);
        assert_eq!(parsed, records[idx].1);
    }
}
