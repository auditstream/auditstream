use std::fs;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use auditstream_core::{
    ActorSource, Batch, CURRENT_SCHEMA_VERSION, Cursor, Event, Op, Sink, SinkError, Source,
};
use auditstream_export::{AnySink, JsonLinesSink, SinkConfig};
use proptest::prelude::*;
use serde_json::Value;
use serde_json::value::RawValue;
use tempfile::tempdir;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use tokio::io::AsyncWrite;
use uuid::Uuid;

fn load_event_schema() -> jsonschema::Validator {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
    let path = manifest_dir
        .join("../..")
        .join("spec/schemas/event.schema.json");
    let raw = fs::read_to_string(&path).expect("read event.schema.json");
    let schema: Value = serde_json::from_str(&raw).expect("parse event.schema.json");
    jsonschema::validator_for(&schema).expect("compile event.schema.json")
}

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

#[tokio::test]
async fn emits_one_compact_line_per_event_matching_event_schema() {
    let validator = load_event_schema();
    let events = sample_events_all_five_kinds();
    let batch = Batch::new(events.clone(), Some(Cursor::new(100, 5)));

    let mut sink = JsonLinesSink::new(Vec::<u8>::new());
    sink.write(&batch).await.expect("write batch");
    sink.flush().await.expect("flush sink");

    let output = String::from_utf8(sink.into_inner()).expect("valid utf-8");
    assert!(output.ends_with('\n'));
    assert!(!output.contains("\r\n"), "JSON Lines must use LF endings");

    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 5);

    for (idx, line) in lines.iter().enumerate() {
        let value: Value = serde_json::from_str(line).expect("parse line JSON");
        assert!(
            validator.is_valid(&value),
            "line {idx} failed event.schema.json: {line}"
        );
        let parsed: Event = serde_json::from_str(line).expect("round-trip Event");
        assert_eq!(parsed, events[idx], "batch order preserved at line {idx}");
    }
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

    let mut sink = JsonLinesSink::new(Vec::<u8>::new());
    sink.write(&Batch::new(vec![event], Some(Cursor::new(1, 1))))
        .await
        .expect("write");
    sink.flush().await.expect("flush");

    let output = String::from_utf8(sink.into_inner()).expect("utf8");
    assert!(
        output.contains(&after_raw),
        "raw after JSON altered: {output}"
    );
    assert!(output.contains(huge_numeric));
    assert!(output.contains(r#""price":1.10"#));
    assert!(output.contains(above_u64_max));
}

#[tokio::test]
async fn compacts_pretty_printed_raw_values_into_single_line_preserving_numbers_keys_and_strings() {
    let validator = load_event_schema();
    let huge_numeric = "123456789012345678901234567890.1234567890";
    let trailing_zero = "1.10";
    let above_u64_max = "18446744073709551616999999";

    let mut db_event = sample_events_all_five_kinds().remove(1);
    db_event.record_id = Some(raw_json(" \r\n\t {\n  \"id\" : 1\n}\r\n "));
    db_event.before = Some(raw_json(
        "{\r\n\t\"status\": \"  in   progress  \",\r\n\t\"path\": \"C:\\\\dir\\\\\"\r\n}",
    ));
    db_event.after = Some(raw_json(&format!(
        " \n{{\n  \"huge\": {huge_numeric},\n  \"price\": {trailing_zero},\n  \"big_int\": {above_u64_max},\n  \"neg_zero\": -0.00,\n  \"sci\": 1e+30,\n  \"z_key\": 1,\n  \"a_key\": 2,\n  \"note\": \"line1\\nline2\\r\\n\\t\\\"quoted\\\" \\\\ end\",\n  \"slashes\": \"a\\\\\\\"b\",\n  \"dir\": \"trailing\\\\\",\n  \"unicode\": \"é 日 🚀 \u{2028}\u{2029}   spaces\"\n}}\r\n"
    )));
    db_event.actor = Some(raw_json("{\n  \"type\": \"user\",\n  \"id\": \"usr 1\"\n}"));
    db_event.context = Some(raw_json(
        "{\r\n  \"requestId\": \"req_1\",\r\n  \"source\": \"api gateway\"\r\n}",
    ));

    let mut app_event = sample_events_all_five_kinds().remove(4);
    app_event.record_id = Some(raw_json("{\n  \"id\": \"inv_42\"\n}"));
    app_event.after = Some(raw_json(
        " \r\n[\n  1.10,\n  \"a \\\"b\\\" c\",\n  {\n    \"k\" : \"v  w\"\n  }\n]\r\n ",
    ));

    let events = vec![db_event, app_event];
    let mut sink = JsonLinesSink::new(Vec::<u8>::new());
    sink.write(&Batch::new(events.clone(), Some(Cursor::new(10, 2))))
        .await
        .expect("write pretty-printed batch");
    sink.flush().await.expect("flush");

    let output = String::from_utf8(sink.into_inner()).expect("valid utf-8");
    assert!(output.ends_with('\n'));
    assert_eq!(
        output.bytes().filter(|&b| b == b'\n').count(),
        events.len(),
        "each event must occupy exactly one LF-terminated line: {output:?}"
    );
    assert!(
        !output.contains('\r'),
        "raw CR must never appear in JSONL output: {output:?}"
    );
    assert!(
        !output.contains('\t'),
        "raw TAB must never appear in JSONL output: {output:?}"
    );

    let lines: Vec<&str> = output.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), events.len());

    // Line 0: schema-valid, arbitrary-precision numbers and key order preserved byte-for-byte.
    let db_val: Value = serde_json::from_str(lines[0]).expect("parse line 0 JSON");
    assert!(
        validator.is_valid(&db_val),
        "line 0 failed event.schema.json: {}",
        lines[0]
    );
    assert!(lines[0].contains(&format!(r#""huge":{huge_numeric}"#)));
    assert!(lines[0].contains(&format!(r#""price":{trailing_zero}"#)));
    assert!(lines[0].contains(&format!(r#""big_int":{above_u64_max}"#)));
    assert!(lines[0].contains(r#""neg_zero":-0.00"#));
    assert!(lines[0].contains(r#""sci":1e+30"#));
    assert!(lines[0].contains(r#""z_key":1,"a_key":2"#));
    assert!(lines[0].contains(r#""dir":"trailing\\""#));
    assert!(lines[0].contains(r#""status":"  in   progress  ""#));

    // Line 1: pretty-printed non-object app payload compacted without mutating string spaces/numbers.
    assert!(lines[1].contains(r#""after":[1.10,"a \"b\" c",{"k":"v  w"}]"#));

    // Every RawValue field round-trips to the exact same JSON Value as the pretty-printed input.
    for (line, expected) in lines.iter().zip(events.iter()) {
        let parsed: Event = serde_json::from_str(line).expect("parse Event from compacted line");
        for (got_field, exp_field) in [
            (parsed.record_id.as_deref(), expected.record_id.as_deref()),
            (parsed.before.as_deref(), expected.before.as_deref()),
            (parsed.after.as_deref(), expected.after.as_deref()),
            (parsed.actor.as_deref(), expected.actor.as_deref()),
            (parsed.context.as_deref(), expected.context.as_deref()),
        ] {
            let got_val: Option<Value> =
                got_field.map(|raw| serde_json::from_str(raw.get()).expect("valid got json"));
            let exp_val: Option<Value> =
                exp_field.map(|raw| serde_json::from_str(raw.get()).expect("valid exp json"));
            assert_eq!(got_val, exp_val);
        }
    }
}

#[tokio::test]
async fn any_sink_file_target_creates_appends_and_syncs_on_flush() {
    let dir = tempdir().expect("tempdir");
    let file_path = dir.path().join("audit.jsonl");

    let events = sample_events_all_five_kinds();
    let cfg = SinkConfig::Jsonl {
        path: Some(file_path.clone()),
    };

    // First open: creates file and writes 2 events.
    {
        let mut sink = AnySink::open(&cfg).expect("open create");
        sink.write(&Batch::new(events[0..2].to_vec(), Some(Cursor::new(10, 2))))
            .await
            .expect("write first");
        sink.flush().await.expect("flush + sync_data first");
    }

    // Second open: appends 3 more events without truncating earlier data.
    {
        let mut sink = AnySink::open(&cfg).expect("open append");
        sink.write(&Batch::new(events[2..5].to_vec(), Some(Cursor::new(11, 5))))
            .await
            .expect("write second");
        sink.flush().await.expect("flush + sync_data second");
    }

    let content = fs::read_to_string(&file_path).expect("read jsonl file");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 5);
    for (idx, line) in lines.iter().enumerate() {
        let parsed: Event = serde_json::from_str(line).expect("parse line");
        assert_eq!(parsed, events[idx]);
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
async fn io_failures_on_write_and_flush_return_sink_error_without_panicking() {
    let big_batch = Batch::new(
        std::iter::repeat_with(|| sample_events_all_five_kinds().remove(0))
            .take(64)
            .collect(),
        Some(Cursor::new(1, 64)),
    );

    // 1. Failure during write (buffer overflows 8 KiB BufWriter and triggers poll_write error).
    let mut failing_write = JsonLinesSink::new(FailingWriter {
        fail_on_write: true,
        fail_on_flush: false,
    });
    let write_err = failing_write
        .write(&big_batch)
        .await
        .expect_err("write error must propagate");
    assert!(matches!(write_err, SinkError::Io(ref e) if e.kind() == io::ErrorKind::BrokenPipe));

    // 2. Failure during flush.
    let small_batch = Batch::new(
        vec![sample_events_all_five_kinds().remove(0)],
        Some(Cursor::new(1, 1)),
    );
    let mut failing_flush = JsonLinesSink::new(FailingWriter {
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
        .expect_err("flush error must propagate");
    assert!(matches!(flush_err, SinkError::Io(ref e) if e.kind() == io::ErrorKind::WriteZero));
}

#[test]
fn sink_config_round_trips_through_toml() {
    for cfg in [
        SinkConfig::Jsonl { path: None },
        SinkConfig::Jsonl {
            path: Some(PathBuf::from("/var/log/auditstream.jsonl")),
        },
        SinkConfig::Csv { path: None },
        SinkConfig::Csv {
            path: Some(PathBuf::from("/var/log/auditstream.csv")),
        },
    ] {
        let encoded = toml::to_string(&cfg).expect("serialize SinkConfig to TOML");
        let decoded: SinkConfig = toml::from_str(&encoded).expect("deserialize SinkConfig");
        assert_eq!(decoded, cfg);
    }
}

fn arb_tricky_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just(' '),
            Just('\t'),
            Just('\n'),
            Just('\r'),
            Just('\u{2028}'),
            Just('\u{2029}'),
            Just('"'),
            Just('\\'),
            Just('é'),
            Just('日'),
            Just('🚀'),
            proptest::char::range('a', 'z'),
        ],
        1..32,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #[test]
    fn line_count_always_equals_event_count_for_newlines_u2028_and_unicode(
        texts in proptest::collection::vec(arb_tricky_text(), 1..8)
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let events: Vec<Event> = texts
            .iter()
            .enumerate()
            .map(|(idx, text)| {
                let mut ev = sample_events_all_five_kinds().remove(0);
                ev.event_id = Uuid::from_u128(idx as u128 + 1);
                let after_json = serde_json::json!({ "note": text }).to_string();
                ev.after = Some(raw_json(&after_json));
                ev
            })
            .collect();

        let output = rt.block_on(async {
            let mut sink = JsonLinesSink::new(Vec::<u8>::new());
            let cursor_id = i64::try_from(events.len()).expect("len fits i64");
            sink.write(&Batch::new(events.clone(), Some(Cursor::new(1, cursor_id))))
                .await
                .expect("write");
            sink.flush().await.expect("flush");
            String::from_utf8(sink.into_inner()).expect("valid utf-8")
        });

        let line_count = output.bytes().filter(|&b| b == b'\n').count();
        prop_assert_eq!(line_count, events.len());
        let lines: Vec<&str> = output.split('\n').filter(|l| !l.is_empty()).collect();
        prop_assert_eq!(lines.len(), events.len());
        for (line, expected) in lines.iter().zip(events.iter()) {
            let parsed: Event = serde_json::from_str(line).expect("each line is valid Event JSON");
            prop_assert_eq!(&parsed, expected);
        }
    }

    #[test]
    fn pretty_printed_raw_values_always_emit_one_valid_line_per_event(
        texts in proptest::collection::vec(arb_tricky_text(), 1..8)
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let events: Vec<Event> = texts
            .iter()
            .enumerate()
            .map(|(idx, text)| {
                let mut ev = sample_events_all_five_kinds().remove(1);
                ev.event_id = Uuid::from_u128(idx as u128 + 1);
                let pretty_obj = serde_json::to_string_pretty(&serde_json::json!({
                    "id": idx,
                    "note": text,
                    "nested": { "k": text }
                }))
                .expect("pretty json");
                // Embed CRLF + TAB inside the JSON object itself (`serde_json::to_string_pretty`
                // escapes all newlines inside string values as `\\n`, so raw `\n` is structural).
                let crlf_tab_obj = pretty_obj.replace('\n', "\r\n\t");
                ev.record_id = Some(raw_json(&format!("{{\r\n\t\"id\": {idx}\r\n}}")));
                ev.before = Some(raw_json(&crlf_tab_obj));
                ev.after = Some(raw_json(&crlf_tab_obj));
                ev.actor = Some(raw_json(&serde_json::to_string_pretty(&serde_json::json!({
                    "type": "user",
                    "id": text
                })).expect("pretty actor")));
                ev.context = Some(raw_json(&serde_json::to_string_pretty(&serde_json::json!({
                    "requestId": text
                })).expect("pretty context")));
                ev
            })
            .collect();

        let output = rt.block_on(async {
            let mut sink = JsonLinesSink::new(Vec::<u8>::new());
            let cursor_id = i64::try_from(events.len()).expect("len fits i64");
            sink.write(&Batch::new(events.clone(), Some(Cursor::new(1, cursor_id))))
                .await
                .expect("write");
            sink.flush().await.expect("flush");
            String::from_utf8(sink.into_inner()).expect("valid utf-8")
        });

        prop_assert!(output.ends_with('\n'));
        prop_assert_eq!(output.bytes().filter(|&b| b == b'\n').count(), events.len());
        prop_assert!(!output.contains('\r'));
        prop_assert!(!output.contains('\t'));
        let lines: Vec<&str> = output.split('\n').filter(|l| !l.is_empty()).collect();
        prop_assert_eq!(lines.len(), events.len());
        for (line, expected) in lines.iter().zip(events.iter()) {
            let parsed: Event = serde_json::from_str(line).expect("each line is valid Event JSON");
            for (got_f, exp_f) in [
                (parsed.record_id.as_deref(), expected.record_id.as_deref()),
                (parsed.before.as_deref(), expected.before.as_deref()),
                (parsed.after.as_deref(), expected.after.as_deref()),
                (parsed.actor.as_deref(), expected.actor.as_deref()),
                (parsed.context.as_deref(), expected.context.as_deref()),
            ] {
                let got_val: Option<Value> =
                    got_f.map(|raw| serde_json::from_str(raw.get()).expect("got json"));
                let exp_val: Option<Value> =
                    exp_f.map(|raw| serde_json::from_str(raw.get()).expect("exp json"));
                prop_assert_eq!(got_val, exp_val);
            }
        }
    }
}
