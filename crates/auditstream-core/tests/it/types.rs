use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use auditstream_core::{
    Actor, ActorPayload, ActorSource, ActorType, Batch, CURRENT_SCHEMA_VERSION, Context, CoreError,
    Cursor, Event, MAX_PAYLOAD_BYTES, NestedActor, Op, Source, validate_actor_payload,
};
use proptest::prelude::*;
use serde_json::Value;
use serde_json::value::RawValue;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use uuid::Uuid;

fn spec_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
        .join("../..")
        .join("spec")
}

fn load_schema(rel: &str) -> jsonschema::Validator {
    let raw = fs::read_to_string(spec_dir().join(rel)).expect("read schema file");
    let schema: Value = serde_json::from_str(&raw).expect("parse schema JSON");
    jsonschema::validator_for(&schema).expect("compile JSON schema")
}

fn raw_json(s: &str) -> Box<RawValue> {
    RawValue::from_string(s.to_owned()).expect("valid raw json")
}

#[test]
fn actor_matches_spec_schema_and_key_order() {
    let validator = load_schema("schemas/actor.schema.json");

    let actor = Actor {
        actor_type: ActorType::AiAgent,
        id: "billing-agent".to_owned(),
        on_behalf_of: Some(NestedActor::new(ActorType::User, "usr_42")),
        tenant: Some("org_acme".to_owned()),
    };
    let context = Context {
        request_id: Some("req_01".to_owned()),
        source: Some("api".to_owned()),
        ip: None,
        user_agent: None,
    };

    let json = validate_actor_payload(&actor, Some(&context)).expect("valid payload");
    assert_eq!(
        json,
        r#"{"type":"ai_agent","id":"billing-agent","onBehalfOf":{"type":"user","id":"usr_42"},"tenant":"org_acme","context":{"requestId":"req_01","source":"api"}}"#
    );

    let value: Value = serde_json::from_str(&json).expect("parse serialized actor payload");
    assert!(
        validator.is_valid(&value),
        "actor payload failed actor.schema.json"
    );

    // Also validate bare Actor without context.
    let actor_json = serde_json::to_string(&actor).expect("serialize actor");
    assert_eq!(
        actor_json,
        r#"{"type":"ai_agent","id":"billing-agent","onBehalfOf":{"type":"user","id":"usr_42"},"tenant":"org_acme"}"#
    );
    let actor_val: Value = serde_json::from_str(&actor_json).expect("parse actor json");
    assert!(validator.is_valid(&actor_val));
}

#[test]
fn nested_on_behalf_of_fails_to_deserialize() {
    let deep = r#"{
        "type": "ai_agent",
        "id": "agent_a",
        "onBehalfOf": {
            "type": "ai_agent",
            "id": "agent_b",
            "onBehalfOf": { "type": "user", "id": "usr_1" }
        }
    }"#;
    let err = serde_json::from_str::<Actor>(deep).expect_err("deep onBehalfOf must fail");
    assert!(
        err.to_string().contains("onBehalfOf"),
        "unexpected error: {err}"
    );
}

#[test]
fn actor_ignores_unknown_keys_for_forward_compatibility() {
    let raw = r#"{
        "type": "user",
        "id": "usr_42",
        "futureField": 123,
        "onBehalfOf": { "type": "service", "id": "svc_1", "extra": true },
        "context": { "requestId": "req_1", "futureCtx": "ok" }
    }"#;
    let payload: ActorPayload = serde_json::from_str(raw).expect("unknown keys must be ignored");
    assert_eq!(payload.actor_type, ActorType::User);
    assert_eq!(payload.id, "usr_42");
    assert_eq!(
        payload.on_behalf_of.as_ref().map(|n| n.id.as_str()),
        Some("svc_1")
    );
    assert_eq!(
        payload
            .context
            .as_ref()
            .and_then(|c| c.request_id.as_deref()),
        Some("req_1")
    );
}

#[test]
fn payload_size_limit_enforces_exact_512_and_513_utf8_bytes() {
    // '{"type":"user","id":""}' = 23 bytes; 244 * "é" (488 bytes) + "a" (1 byte) = 512 bytes.
    let mut id_512 = "é".repeat(244);
    id_512.push('a');

    let actor_512 = Actor::new(ActorType::User, id_512.clone());
    let json_512 = validate_actor_payload(&actor_512, None).expect("512 bytes must pass");
    assert_eq!(json_512.len(), MAX_PAYLOAD_BYTES);

    let mut id_513 = id_512;
    id_513.push('b');
    let actor_513 = Actor::new(ActorType::User, id_513);
    let err = validate_actor_payload(&actor_513, None).expect_err("513 bytes must fail");
    assert!(
        matches!(
            err,
            CoreError::PayloadTooLarge {
                size: 513,
                limit: 512
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn op_maps_dml_variants_and_app_actions_in_both_directions() {
    for (wire, expected) in [
        ("INSERT", Op::Insert),
        ("UPDATE", Op::Update),
        ("DELETE", Op::Delete),
        ("TRUNCATE", Op::Truncate),
        ("invoice.exported", Op::App("invoice.exported".to_owned())),
    ] {
        let parsed: Op = serde_json::from_str(&format!("\"{wire}\"")).expect("deserialize op");
        assert_eq!(parsed, expected);
        assert_eq!(Op::from_str(wire).expect("from_str"), expected);
        assert_eq!(parsed.as_str(), wire);
        assert_eq!(
            serde_json::to_string(&parsed).expect("serialize op"),
            format!("\"{wire}\"")
        );
    }

    assert!(matches!(
        Op::app("INSERT"),
        Err(CoreError::ReservedAction { .. })
    ));
    assert!(matches!(
        Op::app(" INSERT "),
        Err(CoreError::ReservedAction { .. })
    ));
    assert!(matches!(
        Op::app("delete"),
        Err(CoreError::ReservedAction { .. })
    ));
    assert!(matches!(Op::app("   "), Err(CoreError::EmptyAction)));
    assert!(matches!(Op::from_str("   "), Err(CoreError::EmptyAction)));
    assert!(matches!(
        Op::from_str("insert"),
        Err(CoreError::ReservedAction { .. })
    ));
    assert!(matches!(
        Op::from_str(" UPDATE "),
        Err(CoreError::ReservedAction { .. })
    ));
    assert!(matches!(
        Op::app("a".repeat(65)),
        Err(CoreError::ActionTooLong { len: 65, max: 64 })
    ));
    assert_eq!(
        Op::app("invoice.exported").expect("valid app action"),
        Op::App("invoice.exported".to_owned())
    );
}

#[test]
fn event_preserves_arbitrary_precision_numeric_and_validates_against_event_schema() {
    let validator = load_schema("schemas/event.schema.json");
    let occurred_at = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 24).expect("valid date"),
        Time::from_hms_micro(10, 15, 30, 123_456).expect("valid time"),
    )
    .assume_utc();

    let event = Event {
        event_id: Uuid::parse_str("019ab123-4567-7000-8000-000000000001").expect("valid uuid"),
        occurred_at,
        source: Source::Db,
        schema_name: Some("public".to_owned()),
        table_name: Some("invoice".to_owned()),
        op: Op::Update,
        record_id: Some(raw_json(r#"{"id":101}"#)),
        before: Some(raw_json(
            r#"{"id":101,"total":12345678901234567890.123456789}"#,
        )),
        after: Some(raw_json(
            r#"{"id":101,"total":12345678901234567890.987654321}"#,
        )),
        changed: Some(vec!["total".to_owned()]),
        actor: Some(raw_json(r#"{"type":"user","id":"usr_42"}"#)),
        context: Some(raw_json(r#"{"requestId":"req_1","source":"api"}"#)),
        actor_source: Some(ActorSource::Comment),
        db_user: "app_user".to_owned(),
        schema_version: CURRENT_SCHEMA_VERSION,
    };

    let serialized = serde_json::to_string(&event).expect("serialize event");
    assert!(
        serialized.contains("12345678901234567890.123456789"),
        "arbitrary-precision number in before was altered: {serialized}"
    );
    assert!(
        serialized.contains("2026-11-24T10:15:30.123456Z"),
        "occurred_at must use UTC microsecond RFC 3339 format: {serialized}"
    );

    let value: Value = serde_json::from_str(&serialized).expect("parse serialized event");
    assert!(
        validator.is_valid(&value),
        "serialized Event failed event.schema.json"
    );

    // Closed actor.type enum parity in event.schema.json (top-level and nested onBehalfOf).
    let mut bad_actor = value.clone();
    bad_actor["actor"] = serde_json::json!({"type": "bot", "id": "b_1"});
    assert!(
        !validator.is_valid(&bad_actor),
        "event.schema.json must reject unknown actor.type"
    );
    let mut bad_obo = value.clone();
    bad_obo["actor"] = serde_json::json!({
        "type": "ai_agent",
        "id": "a_1",
        "onBehalfOf": {"type": "bot", "id": "b_1"}
    });
    assert!(
        !validator.is_valid(&bad_obo),
        "event.schema.json must reject unknown actor.onBehalfOf.type"
    );

    let round_tripped: Event = serde_json::from_str(&serialized).expect("deserialize event");
    assert_eq!(round_tripped, event);
    assert!(
        round_tripped
            .before
            .as_deref()
            .expect("before present")
            .get()
            .contains("12345678901234567890.123456789")
    );
}

#[test]
fn cursor_orders_by_xact_id_then_id_and_batch_holds_cursor() {
    let c1 = Cursor::new(100, 50);
    let c2 = Cursor::new(100, 51);
    let c3 = Cursor::new(101, 1);

    assert!(c1 < c2);
    assert!(c2 < c3);
    assert!(c1 < c3);

    let empty = Batch::new(vec![], None);
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert_eq!(empty.cursor, None);
}

#[test]
fn deserializes_every_spec_example_and_validates_schemas_on_disk() {
    let actor_validator = load_schema("schemas/actor.schema.json");
    let event_validator = load_schema("schemas/event.schema.json");

    // Inline example from spec/actor.md.
    let handbook_example = r#"{
        "type": "ai_agent",
        "id": "billing-agent",
        "onBehalfOf": { "type": "user", "id": "usr_42" },
        "tenant": "org_acme"
    }"#;
    let parsed_handbook: ActorPayload =
        serde_json::from_str(handbook_example).expect("deserialize spec/actor.md example");
    let reserialized = parsed_handbook.validate().expect("validate example");
    let val: Value = serde_json::from_str(&reserialized).expect("parse json");
    assert!(actor_validator.is_valid(&val));

    // Every valid actor example on disk.
    let actor_valid_dir = spec_dir().join("schemas/examples/actor/valid");
    let mut actor_valid_count = 0usize;
    for entry in fs::read_dir(&actor_valid_dir).expect("read actor/valid") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path).expect("read actor example");
        let parsed: ActorPayload = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("failed to deserialize {}: {e}", path.display()));
        let canonical = parsed
            .validate()
            .unwrap_or_else(|e| panic!("failed to validate {}: {e}", path.display()));
        let v: Value = serde_json::from_str(&canonical).expect("parse canonical");
        assert!(
            actor_validator.is_valid(&v),
            "reserialized {} failed actor.schema.json",
            path.display()
        );
        actor_valid_count += 1;
    }
    assert!(actor_valid_count >= 7);

    // Every invalid actor example on disk must fail deserialization or validation.
    let actor_invalid_dir = spec_dir().join("schemas/examples/actor/invalid");
    let mut actor_invalid_count = 0usize;
    for entry in fs::read_dir(&actor_invalid_dir).expect("read actor/invalid") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path).expect("read invalid actor example");
        let ok = serde_json::from_str::<ActorPayload>(&raw)
            .ok()
            .and_then(|p| p.validate().ok())
            .is_some();
        assert!(
            !ok,
            "invalid actor example {} unexpectedly passed",
            path.display()
        );
        actor_invalid_count += 1;
    }
    assert!(actor_invalid_count >= 7);

    // Every valid event example on disk.
    let event_valid_dir = spec_dir().join("schemas/examples/event/valid");
    let mut event_valid_count = 0usize;
    for entry in fs::read_dir(&event_valid_dir).expect("read event/valid") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path).expect("read event example");
        let event: Event = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("failed to deserialize {}: {e}", path.display()));
        let encoded = serde_json::to_string(&event).expect("serialize event");
        let v: Value = serde_json::from_str(&encoded).expect("parse event json");
        assert!(
            event_validator.is_valid(&v),
            "reserialized {} failed event.schema.json",
            path.display()
        );
        let round_tripped: Event = serde_json::from_str(&encoded).expect("round-trip event");
        assert_eq!(round_tripped, event);
        event_valid_count += 1;
    }
    assert_eq!(event_valid_count, 4);

    // Every invalid event example on disk must fail deserialization.
    let event_invalid_dir = spec_dir().join("schemas/examples/event/invalid");
    let mut event_invalid_count = 0usize;
    for entry in fs::read_dir(&event_invalid_dir).expect("read event/invalid") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path).expect("read invalid event example");
        assert!(
            serde_json::from_str::<Event>(&raw).is_err(),
            "invalid event example {} unexpectedly deserialized",
            path.display()
        );
        event_invalid_count += 1;
    }
    assert_eq!(event_invalid_count, 2);
}

fn arb_actor_type() -> impl Strategy<Value = ActorType> {
    prop_oneof![
        Just(ActorType::User),
        Just(ActorType::ApiKey),
        Just(ActorType::AiAgent),
        Just(ActorType::Service),
        Just(ActorType::System),
    ]
}

fn arb_non_empty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.:-]{1,24}"
}

fn arb_nested_actor() -> impl Strategy<Value = NestedActor> {
    (
        arb_actor_type(),
        arb_non_empty_string(),
        proptest::option::of(arb_non_empty_string()),
    )
        .prop_map(|(actor_type, id, tenant)| NestedActor {
            actor_type,
            id,
            tenant,
        })
}

fn arb_actor() -> impl Strategy<Value = Actor> {
    (
        arb_actor_type(),
        arb_non_empty_string(),
        proptest::option::of(arb_nested_actor()),
        proptest::option::of(arb_non_empty_string()),
    )
        .prop_map(|(actor_type, id, on_behalf_of, tenant)| Actor {
            actor_type,
            id,
            on_behalf_of,
            tenant,
        })
}

fn arb_context() -> impl Strategy<Value = Context> {
    (
        proptest::option::of(arb_non_empty_string()),
        proptest::option::of(arb_non_empty_string()),
        proptest::option::of(arb_non_empty_string()),
        proptest::option::of(arb_non_empty_string()),
    )
        .prop_map(|(request_id, source, ip, user_agent)| Context {
            request_id,
            source,
            ip,
            user_agent,
        })
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Insert),
        Just(Op::Update),
        Just(Op::Delete),
        Just(Op::Truncate),
        "[a-z]{2,12}\\.[a-z]{2,12}".prop_map(Op::App),
    ]
}

fn arb_source() -> impl Strategy<Value = Source> {
    prop_oneof![Just(Source::Db), Just(Source::App)]
}

fn arb_actor_source() -> impl Strategy<Value = ActorSource> {
    prop_oneof![
        Just(ActorSource::SetConfig),
        Just(ActorSource::Comment),
        Just(ActorSource::Jwt),
        Just(ActorSource::Invalid),
    ]
}

fn arb_timestamp() -> impl Strategy<Value = OffsetDateTime> {
    (0_i64..=2_500_000_000_i64, 0_u32..=999_999_u32).prop_map(|(secs, micros)| {
        OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(secs) * 1_000_000_000 + i128::from(micros) * 1_000,
        )
        .expect("valid timestamp")
    })
}

fn arb_raw_obj() -> impl Strategy<Value = Box<RawValue>> {
    (arb_non_empty_string(), -10_000_i64..=10_000_i64)
        .prop_map(|(k, v)| raw_json(&format!(r#"{{"{k}":{v}}}"#)))
}

fn arb_event() -> impl Strategy<Value = Event> {
    (
        (
            any::<u128>(),
            arb_timestamp(),
            arb_source(),
            proptest::option::of(arb_non_empty_string()),
            proptest::option::of(arb_non_empty_string()),
            arb_op(),
            proptest::option::of(arb_raw_obj()),
            proptest::option::of(arb_raw_obj()),
        ),
        (
            proptest::option::of(arb_raw_obj()),
            proptest::option::of(proptest::collection::vec(arb_non_empty_string(), 0..4)),
            proptest::option::of(arb_raw_obj()),
            proptest::option::of(arb_raw_obj()),
            proptest::option::of(arb_actor_source()),
            arb_non_empty_string(),
            1_u16..=10_u16,
        ),
    )
        .prop_map(
            |(
                (event_id, occurred_at, source, schema_name, table_name, op, record_id, before),
                (after, changed, actor, context, actor_source, db_user, schema_version),
            )| Event {
                event_id: Uuid::from_u128(event_id),
                occurred_at,
                source,
                schema_name,
                table_name,
                op,
                record_id,
                before,
                after,
                changed,
                actor,
                context,
                actor_source,
                db_user,
                schema_version,
            },
        )
}

fn arb_cursor() -> impl Strategy<Value = Cursor> {
    (any::<u64>(), any::<i64>()).prop_map(|(xact_id, id)| Cursor::new(xact_id, id))
}

proptest! {
    #[test]
    fn round_trips_actor(actor in arb_actor()) {
        let json = serde_json::to_string(&actor).expect("serialize actor");
        let parsed: Actor = serde_json::from_str(&json).expect("deserialize actor");
        prop_assert_eq!(parsed, actor);
    }

    #[test]
    fn round_trips_context(context in arb_context()) {
        let json = serde_json::to_string(&context).expect("serialize context");
        let parsed: Context = serde_json::from_str(&json).expect("deserialize context");
        prop_assert_eq!(parsed, context);
    }

    #[test]
    fn round_trips_event(event in arb_event()) {
        let json = serde_json::to_string(&event).expect("serialize event");
        let parsed: Event = serde_json::from_str(&json).expect("deserialize event");
        prop_assert_eq!(parsed, event);
    }

    #[test]
    fn round_trips_cursor(cursor in arb_cursor()) {
        let json = serde_json::to_string(&cursor).expect("serialize cursor");
        let parsed: Cursor = serde_json::from_str(&json).expect("deserialize cursor");
        prop_assert_eq!(parsed, cursor);
    }
}
