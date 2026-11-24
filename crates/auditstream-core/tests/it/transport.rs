use std::fs;
use std::path::{Path, PathBuf};

use auditstream_core::{
    Actor, ActorPayload, ActorSource, ActorType, Context, CoreError, DecodedActor, NestedActor,
    SET_CONFIG_SQL, decode, encode, encode_with_context, resolve_precedence, set_config,
    set_config_with_context, tag, tag_with_context,
};
use proptest::prelude::*;
use serde::Deserialize;
use serde_json::Value;

fn conformance_transport_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("spec/conformance/transport")
}

#[derive(Deserialize)]
struct EncodeFixtureFile {
    spec: String,
    cases: Vec<EncodeCase>,
}

#[derive(Deserialize)]
struct EncodeCase {
    name: String,
    input: EncodeInput,
    expected: EncodeExpected,
}

#[derive(Deserialize)]
struct EncodeInput {
    actor: Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EncodeExpected {
    Ok {
        canonical_json: String,
        encoded: String,
        comment: String,
    },
    Err {
        error: String,
    },
}

#[derive(Deserialize)]
struct DecodeFixtureFile {
    spec: String,
    cases: Vec<DecodeCase>,
}

#[derive(Deserialize)]
struct DecodeCase {
    name: String,
    input: DecodeInput,
    expected: DecodeExpected,
}

#[derive(Deserialize)]
struct DecodeInput {
    sql: String,
}

#[derive(Deserialize)]
struct DecodeExpected {
    actor: Option<Value>,
    context: Option<Value>,
    actor_source: Option<ActorSource>,
}

#[derive(Deserialize)]
struct PrecedenceFixtureFile {
    spec: String,
    cases: Vec<PrecedenceCase>,
}

#[derive(Deserialize)]
struct PrecedenceCase {
    name: String,
    input: PrecedenceInput,
    expected: DecodeExpected,
}

#[derive(Deserialize)]
struct PrecedenceInput {
    sql: String,
    set_config: Option<String>,
    jwt_claims: Option<String>,
}

#[test]
fn passes_every_conformance_transport_encode_fixture() {
    let path = conformance_transport_dir().join("encode.json");
    let raw = fs::read_to_string(&path).expect("read transport/encode.json");
    let file: EncodeFixtureFile = serde_json::from_str(&raw).expect("parse encode.json");
    assert_eq!(file.spec, "0.1.0");
    assert!(file.cases.len() >= 10);

    for case in file.cases {
        let parsed = serde_json::from_value::<ActorPayload>(case.input.actor);
        match case.expected {
            EncodeExpected::Ok {
                canonical_json,
                encoded,
                comment,
            } => {
                let payload = parsed
                    .unwrap_or_else(|e| panic!("case '{}' failed to parse input: {e}", case.name));
                let (actor, context) = payload.into_parts();
                let actual_encoded = encode_with_context(&actor, context.as_ref())
                    .unwrap_or_else(|e| panic!("case '{}' failed to encode: {e}", case.name));
                assert_eq!(actual_encoded, encoded, "case '{}'", case.name);

                let stmt = set_config_with_context(&actor, context.as_ref())
                    .unwrap_or_else(|e| panic!("case '{}' set_config failed: {e}", case.name));
                assert_eq!(stmt.sql, SET_CONFIG_SQL);
                assert_eq!(stmt.param, canonical_json, "case '{}'", case.name);

                let tagged = tag_with_context("SELECT 1", &actor, context.as_ref())
                    .unwrap_or_else(|e| panic!("case '{}' tag failed: {e}", case.name));
                assert_eq!(
                    tagged,
                    format!("SELECT 1 {comment}"),
                    "case '{}'",
                    case.name
                );
            }
            EncodeExpected::Err { error } => {
                let result = parsed.map_err(|e| e.to_string()).and_then(|payload| {
                    let (actor, context) = payload.into_parts();
                    encode_with_context(&actor, context.as_ref()).map_err(|e| e.to_string())
                });
                assert!(
                    result.is_err(),
                    "case '{}' expected error '{error}' but succeeded",
                    case.name
                );
            }
        }
    }
}

#[test]
fn passes_every_conformance_transport_decode_fixture() {
    let path = conformance_transport_dir().join("decode.json");
    let raw = fs::read_to_string(&path).expect("read transport/decode.json");
    let file: DecodeFixtureFile = serde_json::from_str(&raw).expect("parse decode.json");
    assert_eq!(file.spec, "0.1.0");
    assert!(file.cases.len() >= 10);

    for case in file.cases {
        let actual = decode(&case.input.sql);
        match case.expected.actor_source {
            None => assert_eq!(actual, DecodedActor::Absent, "case '{}'", case.name),
            Some(ActorSource::Invalid) => {
                assert_eq!(actual, DecodedActor::Invalid, "case '{}'", case.name);
            }
            Some(ActorSource::Comment) => {
                assert_eq!(
                    actual,
                    DecodedActor::Valid {
                        actor: case.expected.actor.expect("valid actor expected"),
                        context: case.expected.context,
                    },
                    "case '{}'",
                    case.name
                );
            }
            other => panic!("unexpected actor_source in decode.json: {other:?}"),
        }
    }
}

#[test]
fn passes_every_conformance_transport_precedence_fixture() {
    let path = conformance_transport_dir().join("precedence.json");
    let raw = fs::read_to_string(&path).expect("read transport/precedence.json");
    let file: PrecedenceFixtureFile = serde_json::from_str(&raw).expect("parse precedence.json");
    assert_eq!(file.spec, "0.1.0");
    assert!(file.cases.len() >= 5);

    for case in file.cases {
        let actual = resolve_precedence(
            &case.input.sql,
            case.input.set_config.as_deref(),
            case.input.jwt_claims.as_deref(),
        );
        assert_eq!(
            actual.actor_source, case.expected.actor_source,
            "case '{}' actor_source",
            case.name
        );
        assert_eq!(
            actual.actor, case.expected.actor,
            "case '{}' actor",
            case.name
        );
        assert_eq!(
            actual.context, case.expected.context,
            "case '{}' context",
            case.name
        );
    }
}

#[test]
fn tag_merges_into_sqlcommenter_sorted_and_never_double_tags() {
    let actor = Actor::new(ActorType::User, "usr_42");
    let expected_b64 = "eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0";
    assert_eq!(encode(&actor).expect("encode worked example"), expected_b64);

    // Merges into existing sqlcommenter comment, sorting keys lexicographically and preserving `;`.
    let with_trace = "UPDATE invoice SET status = $1 /*traceparent='00-abc-01'*/;";
    let merged = tag(with_trace, &actor).expect("merge tag");
    assert_eq!(
        merged,
        format!(
            "UPDATE invoice SET status = $1 /*auditstream='{expected_b64}',traceparent='00-abc-01'*/;"
        )
    );

    // Never double-tags a statement already carrying an auditstream key.
    let second = tag(&merged, &Actor::new(ActorType::Service, "other")).expect("no double tag");
    assert_eq!(second, merged);

    // Appends cleanly after a non-sqlcommenter comment.
    let freeform = "SELECT /* index hint */ 1";
    let appended = tag(freeform, &actor).expect("append after freeform comment");
    assert_eq!(
        appended,
        format!("SELECT /* index hint */ 1 /*auditstream='{expected_b64}'*/")
    );
}

#[test]
fn decode_handles_multibyte_chars_across_4096_char_window_and_kit_t4_spoofs() {
    let actor = Actor::new(ActorType::User, "usr_window");
    let encoded = encode(&actor).expect("encode");
    // Prefix with > 4,096 multi-byte UTF-8 characters (3 bytes each = > 12,000 bytes).
    let prefix = format!("SELECT '{}'", "日".repeat(4500));
    let sql = format!("{prefix} /*auditstream='{encoded}'*/ ;  \n");
    let decoded = decode(&sql);
    assert_eq!(
        decoded.into_actor().expect("parse actor"),
        Some(actor),
        "must slice last 4096 chars without panicking on UTF-8 boundaries"
    );

    // Kit T4 spoofs inside string literals must decode as Absent.
    let spoof_1 = "UPDATE invoice SET status='/*auditstream=''eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ''*/' WHERE id=3;";
    let spoof_2 = "UPDATE invoice SET status='x' WHERE status='/*auditstream=''eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ''*/';";
    assert_eq!(decode(spoof_1), DecodedActor::Absent);
    assert_eq!(decode(spoof_2), DecodedActor::Absent);
}

#[test]
fn set_config_uses_bound_parameter_and_rejects_oversized_payload() {
    let actor = Actor::new(ActorType::User, "usr_42");
    let stmt = set_config(&actor).expect("set_config");
    assert_eq!(stmt.sql, "SELECT set_config('auditstream.actor', $1, true)");
    assert!(!stmt.sql.contains("usr_42"));
    assert_eq!(stmt.param, r#"{"type":"user","id":"usr_42"}"#);

    let oversized = Actor::new(ActorType::User, "x".repeat(500));
    assert!(matches!(
        encode(&oversized),
        Err(CoreError::PayloadTooLarge { .. })
    ));
    assert!(matches!(
        set_config(&oversized),
        Err(CoreError::PayloadTooLarge { .. })
    ));
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

fn arb_id() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.:é日-]{1,20}"
}

fn arb_nested_actor() -> impl Strategy<Value = NestedActor> {
    (arb_actor_type(), arb_id(), proptest::option::of(arb_id())).prop_map(
        |(actor_type, id, tenant)| NestedActor {
            actor_type,
            id,
            tenant,
        },
    )
}

fn arb_actor() -> impl Strategy<Value = Actor> {
    (
        arb_actor_type(),
        arb_id(),
        proptest::option::of(arb_nested_actor()),
        proptest::option::of(arb_id()),
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
        proptest::option::of(arb_id()),
        proptest::option::of(arb_id()),
        proptest::option::of(arb_id()),
        proptest::option::of(arb_id()),
    )
        .prop_map(|(request_id, source, ip, user_agent)| Context {
            request_id,
            source,
            ip,
            user_agent,
        })
}

proptest! {
    #[test]
    fn round_trips_tag_and_decode_for_any_valid_actor(
        actor in arb_actor(),
        context in proptest::option::of(arb_context()),
        sql in "(SELECT 1|UPDATE invoice SET status = 'ok'|DELETE FROM line_item WHERE id = 1)"
    ) {
        let encoded = encode_with_context(&actor, context.as_ref()).expect("valid small actor");
        prop_assert!(!encoded.contains("*/"));
        prop_assert!(!encoded.contains('='));
        prop_assert!(!encoded.contains('/'));
        prop_assert!(!encoded.contains('+'));

        let tagged = tag_with_context(&sql, &actor, context.as_ref()).expect("tag sql");
        let decoded = decode(&tagged);
        let parsed = decoded.into_actor().expect("parse decoded actor");
        prop_assert_eq!(parsed, Some(actor));
    }
}
