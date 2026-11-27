use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use auditstream_core::{
    ActorSource, Batch, CURRENT_SCHEMA_VERSION, Cursor, Event, Op, Sink, SinkError, Source,
};
use auditstream_export::{AnySink, CSV_HEADERS, CsvSink, SinkConfig};
use proptest::prelude::*;
use serde_json::Value;
use serde_json::value::RawValue;
use tempfile::tempdir;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use tokio::io::AsyncWrite;
use uuid::Uuid;

fn raw_json(s: &str) -> Box<RawValue> {
    RawValue::from_string(s.to_owned()).expect("valid raw json")
}

fn sample_time(sec: u8) -> OffsetDateTime {
    PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 24).expect("valid date"),
        Time::from_hms_micro(10, 15, sec, 123_456).expect("valid time"),
    )
    .assume_utc()
}

fn sample_events_all_five_kinds() -> Vec<Event> {
    vec![
        Event {
            event_id: Uuid::from_u128(1),
            occurred_at: sample_time(1),
            source: Source::Db,
            schema_name: Some("public".to_owned()),
            table_name: Some("invoice".to_owned()),
            op: Op::Insert,
            record_id: Some(raw_json(r#"{"id":1}"#)),
            before: None,
            after: Some(raw_json(r#"{"id":1,"status":"open","total":100}"#)),
            changed: None,
            actor: Some(raw_json(r#"{"type":"user","id":"usr_1"}"#)),
            context: Some(raw_json(r#"{"requestId":"req_1","source":"api"}"#)),
            actor_source: Some(ActorSource::Comment),
            db_user: "app_user".to_owned(),
            schema_version: CURRENT_SCHEMA_VERSION,
        },
        Event {
            event_id: Uuid::from_u128(2),
            occurred_at: sample_time(2),
            source: Source::Db,
            schema_name: Some("public".to_owned()),
            table_name: Some("invoice".to_owned()),
            op: Op::Update,
            record_id: Some(raw_json(r#"{"id":1}"#)),
            before: Some(raw_json(r#"{"id":1,"status":"open"}"#)),
            after: Some(raw_json(r#"{"id":1,"status":"paid"}"#)),
            changed: Some(vec!["status".to_owned()]),
            actor: Some(raw_json(r#"{"type":"system","id":"nightly"}"#)),
            context: None,
            actor_source: Some(ActorSource::SetConfig),
            db_user: "app_user".to_owned(),
            schema_version: CURRENT_SCHEMA_VERSION,
        },
        Event {
            event_id: Uuid::from_u128(3),
            occurred_at: sample_time(3),
            source: Source::Db,
            schema_name: Some("public".to_owned()),
            table_name: Some("invoice".to_owned()),
            op: Op::Delete,
            record_id: Some(raw_json(r#"{"id":1}"#)),
            before: Some(raw_json(r#"{"id":1,"status":"paid"}"#)),
            after: None,
            changed: None,
            actor: Some(raw_json(r#"{"type":"user","id":"usr_1"}"#)),
            context: None,
            actor_source: Some(ActorSource::Jwt),
            db_user: "app_user".to_owned(),
            schema_version: CURRENT_SCHEMA_VERSION,
        },
        Event {
            event_id: Uuid::from_u128(4),
            occurred_at: sample_time(4),
            source: Source::Db,
            schema_name: Some("public".to_owned()),
            table_name: Some("line_item".to_owned()),
            op: Op::Truncate,
            record_id: None,
            before: None,
            after: None,
            changed: None,
            actor: None,
            context: None,
            actor_source: None,
            db_user: "app_user".to_owned(),
            schema_version: CURRENT_SCHEMA_VERSION,
        },
        Event {
            event_id: Uuid::from_u128(5),
            occurred_at: sample_time(5),
            source: Source::App,
            schema_name: None,
            table_name: None,
            op: Op::app("invoice.exported").expect("valid app op"),
            record_id: Some(raw_json(r#"{"id":1}"#)),
            before: None,
            after: Some(raw_json(r#"{"format":"pdf"}"#)),
            changed: None,
            actor: Some(raw_json(r#"{"type":"service","id":"exporter"}"#)),
            context: Some(raw_json(r#"{"requestId":"req_2","source":"worker"}"#)),
            actor_source: Some(ActorSource::Comment),
            db_user: "app_user".to_owned(),
            schema_version: CURRENT_SCHEMA_VERSION,
        },
    ]
}

fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
}

#[test]
fn header_column_set_equals_event_schema_properties() {
    let schema_path = manifest_dir()
        .join("../..")
        .join("spec/schemas/event.schema.json");
    let raw = fs::read_to_string(&schema_path).expect("read event.schema.json");
    let schema: Value = serde_json::from_str(&raw).expect("parse event.schema.json");
    let schema_props: BTreeSet<&str> = schema["properties"]
        .as_object()
        .expect("properties object")
        .keys()
        .map(String::as_str)
        .collect();
    let header_set: BTreeSet<&str> = CSV_HEADERS.into_iter().collect();

    assert_eq!(header_set, schema_props);
    assert_eq!(CSV_HEADERS.len(), schema_props.len());
}

#[tokio::test]
async fn matches_spec_records_csv_example_byte_for_byte_with_crlf() {
    let spec_dir = manifest_dir()
        .join("../..")
        .join("spec/schemas/examples/event");
    let valid_files = [
        "valid/db-insert.json",
        "valid/db-update.json",
        "valid/db-truncate.json",
        "valid/app-event.json",
    ];
    let events: Vec<Event> = valid_files
        .iter()
        .map(|rel| {
            let raw = fs::read_to_string(spec_dir.join(rel)).expect("read valid example");
            serde_json::from_str::<Event>(&raw).expect("parse example Event")
        })
        .collect();

    let mut sink = CsvSink::new(Vec::<u8>::new());
    sink.write(&Batch::new(events, Some(Cursor::new(1, 4))))
        .await
        .expect("write");
    sink.flush().await.expect("flush");
    let actual = String::from_utf8(sink.into_inner()).expect("valid utf8");

    let expected_raw = fs::read_to_string(spec_dir.join("records.csv")).expect("read records.csv");
    // Normalize expected in case git checkout converted CRLF to LF on disk, then verify actual has CRLF.
    let expected_crlf = format!(
        "{}\r\n",
        expected_raw
            .replace("\r\n", "\n")
            .trim_end_matches('\n')
            .replace('\n', "\r\n")
    );

    assert_eq!(actual, expected_crlf);
}

#[tokio::test]
async fn emits_all_five_event_kinds_with_crlf_unquoted_scalars_and_compact_json_columns() {
    let events = sample_events_all_five_kinds();
    let mut sink = CsvSink::new(Vec::<u8>::new());
    sink.write(&Batch::new(events.clone(), Some(Cursor::new(10, 5))))
        .await
        .expect("write batch");
    sink.flush().await.expect("flush");

    let raw = String::from_utf8(sink.into_inner()).expect("valid utf8");
    assert!(raw.ends_with("\r\n"), "CSV output must end with CRLF");
    for line in raw.split("\r\n").filter(|l| !l.is_empty()) {
        assert!(!line.contains('\n'), "unexpected bare LF in line: {line}");
    }

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(raw.as_bytes());
    let headers: Vec<&str> = reader.headers().expect("headers").iter().collect();
    assert_eq!(headers, CSV_HEADERS);

    let records: Vec<csv::StringRecord> = reader
        .records()
        .collect::<Result<_, _>>()
        .expect("parse all CSV records");
    assert_eq!(records.len(), 5);

    // Row 0: INSERT
    assert_eq!(&records[0][0], events[0].event_id.to_string());
    assert_eq!(&records[0][1], "2026-11-24T10:15:01.123456Z");
    assert_eq!(&records[0][2], "db");
    assert_eq!(&records[0][3], "public");
    assert_eq!(&records[0][4], "invoice");
    assert_eq!(&records[0][5], "INSERT");
    assert_eq!(&records[0][6], r#"{"id":1}"#);
    assert_eq!(&records[0][7], "");
    assert_eq!(&records[0][8], r#"{"id":1,"status":"open","total":100}"#);
    assert_eq!(&records[0][9], "");
    assert_eq!(&records[0][10], r#"{"type":"user","id":"usr_1"}"#);
    assert_eq!(&records[0][11], r#"{"requestId":"req_1","source":"api"}"#);
    assert_eq!(&records[0][12], "comment");
    assert_eq!(&records[0][13], "app_user");
    assert_eq!(&records[0][14], "1");

    // Row 1: UPDATE
    assert_eq!(&records[1][5], "UPDATE");
    assert_eq!(&records[1][7], r#"{"id":1,"status":"open"}"#);
    assert_eq!(&records[1][8], r#"{"id":1,"status":"paid"}"#);
    assert_eq!(&records[1][9], r#"["status"]"#);
    assert_eq!(&records[1][12], "set_config");

    // Row 2: DELETE
    assert_eq!(&records[2][5], "DELETE");
    assert_eq!(&records[2][7], r#"{"id":1,"status":"paid"}"#);
    assert_eq!(&records[2][8], "");
    assert_eq!(&records[2][12], "jwt");

    // Row 3: TRUNCATE
    assert_eq!(&records[3][5], "TRUNCATE");
    assert_eq!(&records[3][6], "");
    assert_eq!(&records[3][7], "");
    assert_eq!(&records[3][8], "");
    assert_eq!(&records[3][10], "");
    assert_eq!(&records[3][12], "");

    // Row 4: app event
    assert_eq!(&records[4][2], "app");
    assert_eq!(&records[4][3], "");
    assert_eq!(&records[4][4], "");
    assert_eq!(&records[4][5], "invoice.exported");
    assert_eq!(&records[4][6], r#"{"id":1}"#);
    assert_eq!(&records[4][8], r#"{"format":"pdf"}"#);
}

#[tokio::test]
async fn preserves_40_digit_numeric_trailing_zero_decimal_and_u64_overflow_byte_for_byte() {
    let huge_numeric = "123456789012345678901234567890.1234567890";
    let trailing_zero = "1.10";
    let above_u64_max = "18446744073709551616999999";

    let after_raw = format!(
        r#"{{"huge":{huge_numeric},"price":{trailing_zero},"big_int":{above_u64_max},"z_key":1,"a_key":2}}"#
    );
    let mut event = sample_events_all_five_kinds().remove(0);
    event.after = Some(raw_json(&after_raw));

    let mut sink = CsvSink::new(Vec::<u8>::new());
    sink.write(&Batch::new(vec![event], Some(Cursor::new(1, 1))))
        .await
        .expect("write");
    sink.flush().await.expect("flush");

    let output = sink.into_inner();
    let mut reader = csv::Reader::from_reader(output.as_slice());
    let record = reader
        .records()
        .next()
        .expect("one record")
        .expect("valid csv");
    assert_eq!(&record[8], after_raw);
}

#[tokio::test]
async fn formula_prefix_characters_are_not_escaped_and_rfc4180_quoting_rules_hold() {
    let mut event = sample_events_all_five_kinds().remove(4);
    event.op = Op::app("=SUM(A1:A2),\"+-@").expect("valid app op");
    event.db_user = "@admin,\"+-\r\nuser+1".to_owned();

    let mut sink = CsvSink::new(Vec::<u8>::new());
    sink.write(&Batch::new(vec![event], Some(Cursor::new(1, 1))))
        .await
        .expect("write");
    sink.flush().await.expect("flush");

    let output = String::from_utf8(sink.into_inner()).expect("utf8");
    // Quotes inside field must be doubled in RFC 4180 output, and embedded CRLF stays quoted.
    assert!(output.contains("\"=SUM(A1:A2),\"\"+-@\""));
    assert!(output.contains("\"@admin,\"\"+-\r\nuser+1\""));

    let mut reader = csv::Reader::from_reader(output.as_bytes());
    let record = reader
        .records()
        .next()
        .expect("one record")
        .expect("valid csv");
    assert_eq!(&record[5], "=SUM(A1:A2),\"+-@");
    assert_eq!(&record[13], "@admin,\"+-\r\nuser+1");
}

#[tokio::test]
async fn header_written_once_for_stdout_empty_file_and_omitted_on_non_empty_file_append() {
    let events = sample_events_all_five_kinds();

    // Case 1: in-memory / stdout writer across two batches writes header once.
    {
        let mut sink = CsvSink::new(Vec::<u8>::new());
        sink.write(&Batch::new(events[0..2].to_vec(), Some(Cursor::new(1, 2))))
            .await
            .expect("batch 1");
        sink.write(&Batch::new(events[2..4].to_vec(), Some(Cursor::new(2, 4))))
            .await
            .expect("batch 2");
        sink.flush().await.expect("flush");
        let text = String::from_utf8(sink.into_inner()).expect("utf8");
        let header_line = CSV_HEADERS.join(",");
        assert_eq!(text.matches(&header_line).count(), 1);
    }

    // Case 2: empty file via AnySink::open writes header once.
    let dir = tempdir().expect("tempdir");
    let file_path = dir.path().join("audit.csv");
    let cfg: SinkConfig = toml::from_str(&format!(
        "type = \"csv\"\npath = {:?}\n",
        file_path.to_str().expect("utf8 path")
    ))
    .expect("parse TOML SinkConfig");

    {
        let mut sink = AnySink::open(&cfg).expect("open new empty csv file");
        sink.write(&Batch::new(events[0..2].to_vec(), Some(Cursor::new(10, 2))))
            .await
            .expect("write 1");
        sink.flush().await.expect("flush + sync_data 1");
    }

    // Case 3: re-opening the non-empty file appends rows WITHOUT duplicating the header.
    {
        let mut sink = AnySink::open(&cfg).expect("reopen non-empty csv file");
        sink.write(&Batch::new(events[2..5].to_vec(), Some(Cursor::new(11, 5))))
            .await
            .expect("write 2");
        sink.flush().await.expect("flush + sync_data 2");
    }

    let content = fs::read_to_string(&file_path).expect("read csv file");
    let header_line = CSV_HEADERS.join(",");
    assert_eq!(
        content.matches(&header_line).count(),
        1,
        "header must not be duplicated on append"
    );

    let mut reader = csv::Reader::from_reader(content.as_bytes());
    let records: Vec<csv::StringRecord> = reader
        .records()
        .collect::<Result<_, _>>()
        .expect("parse appended csv");
    assert_eq!(records.len(), 5);
    for (idx, record) in records.iter().enumerate() {
        assert_eq!(&record[0], events[idx].event_id.to_string());
    }
}

struct FailingWriter {
    fail_on_write: bool,
    fail_on_flush: bool,
}

impl AsyncWrite for FailingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.fail_on_write {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "write boom")))
        } else {
            Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if self.fail_on_flush {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::WriteZero, "flush boom")))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn io_failures_on_write_and_flush_return_sink_error() {
    let big_batch = Batch::new(
        std::iter::repeat_with(|| sample_events_all_five_kinds().remove(0))
            .take(64)
            .collect(),
        Some(Cursor::new(1, 64)),
    );

    let mut failing_write = CsvSink::new(FailingWriter {
        fail_on_write: true,
        fail_on_flush: false,
    });
    let write_err = failing_write
        .write(&big_batch)
        .await
        .expect_err("write failure must return SinkError");
    assert!(matches!(write_err, SinkError::Io(ref e) if e.kind() == io::ErrorKind::BrokenPipe));

    let small_batch = Batch::new(
        vec![sample_events_all_five_kinds().remove(0)],
        Some(Cursor::new(1, 1)),
    );
    let mut failing_flush = CsvSink::new(FailingWriter {
        fail_on_write: false,
        fail_on_flush: true,
    });
    failing_flush
        .write(&small_batch)
        .await
        .expect("buffered write ok");
    let flush_err = failing_flush
        .flush()
        .await
        .expect_err("flush failure must return SinkError");
    assert!(matches!(flush_err, SinkError::Io(ref e) if e.kind() == io::ErrorKind::WriteZero));
}

fn arb_csv_tricky_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just(','),
            Just('"'),
            Just('\r'),
            Just('\n'),
            Just('='),
            Just('+'),
            Just('-'),
            Just('@'),
            Just('é'),
            Just('日'),
            proptest::char::range('a', 'z'),
        ],
        1..32,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #[test]
    fn strict_csv_reader_recovers_arbitrary_commas_quotes_and_line_breaks_unchanged(
        cases in proptest::collection::vec((arb_csv_tricky_text(), arb_csv_tricky_text()), 1..6)
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let events: Vec<Event> = cases
            .iter()
            .enumerate()
            .map(|(idx, (db_user, note))| {
                let mut ev = sample_events_all_five_kinds().remove(0);
                ev.event_id = Uuid::from_u128(idx as u128 + 1);
                ev.db_user.clone_from(db_user);
                let after_json = serde_json::json!({ "note": note }).to_string();
                ev.after = Some(raw_json(&after_json));
                ev
            })
            .collect();

        let bytes = rt.block_on(async {
            let mut sink = CsvSink::new(Vec::<u8>::new());
            let cursor_id = i64::try_from(events.len()).expect("len fits i64");
            sink.write(&Batch::new(events.clone(), Some(Cursor::new(1, cursor_id))))
                .await
                .expect("write");
            sink.flush().await.expect("flush");
            sink.into_inner()
        });

        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(false)
            .from_reader(bytes.as_slice());

        let records: Vec<csv::StringRecord> = reader
            .records()
            .collect::<Result<_, _>>()
            .expect("strict RFC 4180 parse");
        prop_assert_eq!(records.len(), events.len());

        for (record, expected) in records.iter().zip(events.iter()) {
            prop_assert_eq!(record.len(), CSV_HEADERS.len());
            prop_assert_eq!(&record[0], expected.event_id.to_string());
            prop_assert_eq!(&record[8], expected.after.as_deref().expect("after").get());
            prop_assert_eq!(&record[13], expected.db_user.as_str());
        }
    }
}
