use std::fs;
use std::path::PathBuf;

use auditstream_core::{
    Actor, ActorPayload, ActorSource, ActorType, Context, CoreError, DecodedActor, NestedActor,
    SET_CONFIG_SQL, decode, encode, encode_with_context, resolve_precedence, set_config,
    set_config_with_context, tag, tag_with_context,
};
use proptest::prelude::*;
use serde::Deserialize;
use serde_json::Value;

fn conformance_transport_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
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
fn tag_and_decode_handle_trailing_line_comments_and_pg_quoting_lexically() {
    let actor = Actor::new(ActorType::User, "usr_42");
    let encoded = encode(&actor).expect("encode usr_42");
    let fake_encoded = encode(&Actor::new(ActorType::User, "usr_fake")).expect("encode fake");

    let trailing_line_comments = [
        "SELECT 1 -- user note".to_owned(),
        "SELECT 1 --".to_owned(),
        format!("SELECT 1 -- /*auditstream='{fake_encoded}'*/"),
        format!("SELECT 1 /*auditstream='{fake_encoded}'*/ -- trailing comment"),
        "SELECT 1 -- /*traceparent='00-abc'*/".to_owned(),
        "SELECT $1 -- positional param is not dollar-quote".to_owned(),
        "SELECT $foo -- unclosed dollar tag is not dollar-quote".to_owned(),
        "SELECT col$a$ FROM t -- ident dollar is not dollar-quote".to_owned(),
        r"SELECT 'backslash \' -- standard_conforming_strings ends string here".to_owned(),
        r"SELECT E'\\' -- E-string ended after escaped backslash".to_owned(),
        "SELECT E'a'\n'b\\' ' || 'x' -- trailing after E-string quotecontinue".to_owned(),
        "SELECT E'a' -- join\n'b\\' ' || 'x' -- trailing after commented quotecontinue".to_owned(),
        r#"SELECT "col" -- after double-quoted ident"#.to_owned(),
        "SELECT 1 /* block */ -- line after block".to_owned(),
        "SELECT $tag$body$tag$ -- after dollar-quoted string".to_owned(),
    ];

    for sql in &trailing_line_comments {
        assert_eq!(decode(sql), DecodedActor::Absent, "decode({sql:?})");
        let tagged = tag(sql, &actor).expect("tag trailing line comment");
        assert_eq!(
            tagged,
            format!("{sql}\n/*auditstream='{encoded}'*/"),
            "tag({sql:?})"
        );
        assert_eq!(
            decode(&tagged).into_actor().expect("parse"),
            Some(actor.clone()),
            "round-trip({sql:?})"
        );
        assert_eq!(
            tag(&tagged, &Actor::new(ActorType::Service, "other")).expect("idempotent re-tag"),
            tagged,
            "no double tag({sql:?})"
        );
    }

    let non_trailing_line_comments = [
        "SELECT 1 -- closed with LF\n",
        "SELECT 1 -- closed with CR\r",
        "SELECT '-- inside single quotes'",
        "SELECT 'it''s -- still in single quotes'",
        r"SELECT E'escaped \' -- still in E-string'",
        "SELECT E'it''s -- doubled quote still in E-string'",
        r#"SELECT "col--name" FROM t"#,
        r#"SELECT "a""--b" FROM t"#,
        "SELECT $$-- inside dollar quote$$",
        "SELECT $tag$-- inside tagged dollar quote$tag$",
        "SELECT $a$ $b$ -- inside $a$",
        "SELECT 1 /* -- inside block comment */",
        "SELECT 1 /* outer /* inner */ -- still in outer block */",
    ];

    for sql in non_trailing_line_comments {
        assert_eq!(decode(sql), DecodedActor::Absent, "decode({sql:?})");
        let tagged = tag(sql, &actor).expect("tag non-trailing line comment");
        assert_eq!(
            tagged,
            format!("{sql} /*auditstream='{encoded}'*/"),
            "tag({sql:?})"
        );
        assert_eq!(
            decode(&tagged).into_actor().expect("parse"),
            Some(actor.clone())
        );
    }

    // `--` inside an unclosed block comment is not a line comment (appends with space, not LF).
    let unclosed_block = "SELECT 1 /* unclosed block -- not a line comment";
    assert_eq!(decode(unclosed_block), DecodedActor::Absent);
    assert_eq!(
        tag(unclosed_block, &actor).expect("tag unclosed block"),
        format!("{unclosed_block} /*auditstream='{encoded}'*/")
    );
}

#[test]
fn tag_and_decode_parse_sqlcommenter_pairs_lexically_without_suppressing_tagging() {
    let actor = Actor::new(ActorType::User, "usr_real");
    let encoded = encode(&actor).expect("encode usr_real");
    let fake_encoded = encode(&Actor::new(ActorType::User, "usr_fake")).expect("encode fake");

    // Embedded `,auditstream='` in another sqlcommenter value, malformed comments, free-form
    // comments, nested block comments, unclosed quotes, and non-ASCII trailing whitespace
    // must decode as Absent and must NOT suppress tagging.
    let absent_cases = [
        "SELECT 1 /*route='foo,auditstream=',other='bar'*/".to_owned(),
        "SELECT 1 /*a_route='foo,auditstream=',other='bar'*/".to_owned(),
        "SELECT 1 /*note='hello,auditstream=''abc'''*/".to_owned(),
        format!("SELECT 1 /* free text ,auditstream='{fake_encoded}' */"),
        format!("SELECT 1 /*garbage,auditstream='{fake_encoded}'*/"),
        format!("SELECT 1 /*auditstream='{fake_encoded}',*/"),
        format!("SELECT 1 /*,auditstream='{fake_encoded}'*/"),
        format!("SELECT 1 /*auditstream='{fake_encoded}',auditstream='{fake_encoded}',garbage*/"),
        format!("SELECT 1 /* outer /*auditstream='{fake_encoded}'*/ */"),
        format!("SELECT 'unclosed /*auditstream='{fake_encoded}'*/"),
        format!("SELECT E'unclosed /*auditstream='{fake_encoded}'*/"),
        format!("SELECT $$unclosed /*auditstream='{fake_encoded}'*/"),
        format!("SELECT 1 /*auditstream='{fake_encoded}'*/\u{00A0}"),
        format!("SELECT 1 /*auditstream='{fake_encoded}'*/\u{2028}"),
        format!("SELECT 1 /*auditstream='{fake_encoded}'*/\u{FEFF}"),
        "/*/*/".to_owned(),
        "/*/".to_owned(),
        "/**/".to_owned(),
        "SELECT 1 /*auditstream='*/".to_owned(),
    ];

    for sql in &absent_cases {
        assert_eq!(decode(sql), DecodedActor::Absent, "decode({sql:?})");
        let tagged = tag(sql, &actor).expect("tag absent case");
        assert_ne!(tagged, *sql, "must not suppress tagging for {sql:?}");
        if !sql.starts_with("SELECT 'unclosed")
            && !sql.starts_with("SELECT E'unclosed")
            && !sql.starts_with("SELECT $$unclosed")
            && !sql.starts_with("/*/")
        {
            assert_eq!(
                decode(&tagged).into_actor().expect("parse"),
                Some(actor.clone()),
                "tagged output must decode to active actor for {sql:?} -> {tagged:?}"
            );
        }
    }

    // Merging into `a_route='foo,auditstream='` (key < "auditstream") falls back to appending
    // a standalone comment so legacy `strpos` readers never hit `,auditstream='` inside `a_route`.
    let collision_sql = "UPDATE invoice SET status = 'ok' /*a_route='foo,auditstream='*/;";
    let collision_tagged = tag(collision_sql, &actor).expect("tag collision sql");
    assert_eq!(
        collision_tagged,
        format!("{collision_sql} /*auditstream='{encoded}'*/")
    );
    assert_eq!(
        decode(&collision_tagged).into_actor().expect("parse"),
        Some(actor.clone())
    );

    // Invalid or duplicate `auditstream` tags decode as Invalid, yet still do NOT suppress
    // tagging so the active actor wins in the final trailing comment.
    let invalid_cases = [
        "SELECT 1 /*auditstream=''*/".to_owned(),
        "SELECT 1 /*auditstream='!!!notbase64'*/".to_owned(),
        "SELECT 1 /*auditstream='bm90IGpzb24'*/".to_owned(),
        format!("SELECT 1 /*auditstream='{fake_encoded}',auditstream='{fake_encoded}'*/"),
        format!(
            "SELECT 1 /*route='O\\'Reilly',auditstream='{fake_encoded}',auditstream='{fake_encoded}'*/"
        ),
    ];

    for sql in &invalid_cases {
        assert_eq!(decode(sql), DecodedActor::Invalid, "decode({sql:?})");
        let tagged = tag(sql, &actor).expect("tag invalid case");
        assert_eq!(tagged, format!("{sql} /*auditstream='{encoded}'*/"));
        assert_eq!(
            decode(&tagged).into_actor().expect("parse"),
            Some(actor.clone()),
            "re-tagged invalid comment must resolve active actor: {sql:?}"
        );
    }

    // Valid sqlcommenter comments with commas in values, Drizzle/Prisma key chars, and `\'` escapes
    // decode cleanly and are recognized as already tagged (never double-tagged).
    let existing = Actor::new(ActorType::User, "usr_42");
    let existing_b64 = encode(&existing).expect("encode usr_42");
    let valid_already_tagged = [
        format!("SELECT 1 /*route='a,b',auditstream='{existing_b64}'*/"),
        format!("SELECT 1 /*db.driver='pg',auditstream='{existing_b64}'*/"),
        format!("SELECT 1 /*k%20v='1',app~1='2',k!*(x)='3',auditstream='{existing_b64}'*/"),
        format!("SELECT 1 /*a\\'b='v',auditstream='{existing_b64}'*/"),
        format!("SELECT 1 /*route='O\\'Reilly',auditstream='{existing_b64}'*/"),
        format!("SELECT 1 /*route='O\\'Reilly',path='a\\\\',auditstream='{existing_b64}'*/"),
        format!("SELECT 1 /*x='foo\\',auditstream='{existing_b64}'*/"),
        format!("SELECT 1 /*auditstream='{existing_b64}',x='foo\\'*/"),
        format!("SELECT E'a'\n'b\\' ' || 'x' /*auditstream='{existing_b64}'*/"),
        format!("SELECT 1 -- note\n/*auditstream='{existing_b64}'*/ \t\r\n\x0c\x0b;"),
    ];

    for sql in &valid_already_tagged {
        assert_eq!(
            decode(sql).into_actor().expect("parse"),
            Some(existing.clone()),
            "decode({sql:?})"
        );
        assert_eq!(
            tag(sql, &actor).expect("skip already tagged"),
            *sql,
            "must not double-tag valid comment {sql:?}"
        );
    }
}

#[test]
fn tag_preserves_4096_char_tail_window_on_long_sqlcommenter_merge_and_boundaries() {
    let actor = Actor::new(ActorType::User, "usr_42");
    let encoded = encode(&actor).expect("encode usr_42");
    // Merged comment `/*auditstream='<39>',z_trace='<N>'*/;` has overhead:
    // `/*` (2) + `auditstream='` (13) + 39 + `',z_trace='` (11) + `'` (1) + `*/` (2) + `;` (1) = 69 chars.
    let overhead = 4 + "auditstream='".len() + encoded.len() + "',z_trace='".len() + 1 + 1;

    // Exact 4,096 chars (ASCII): merges cleanly and stays inside PostgreSQL's 4,096-char window.
    let exact_len = 4096 - overhead;
    let exact_sql = format!(
        "UPDATE invoice SET status = 'ok' /*z_trace='{}'*/;",
        "a".repeat(exact_len)
    );
    let exact_tagged = tag(&exact_sql, &actor).expect("merge at exact 4096 boundary");
    let exact_comment_start = exact_tagged.rfind("/*").expect("opening /*");
    assert_eq!(exact_tagged[exact_comment_start..].chars().count(), 4096);
    assert!(
        exact_tagged.contains(&format!("/*auditstream='{encoded}',z_trace='")),
        "should merge when merged comment + suffix is exactly 4096 chars"
    );
    assert_eq!(
        decode(&exact_tagged).into_actor().expect("parse"),
        Some(actor.clone())
    );
    assert_eq!(
        tag(&exact_tagged, &Actor::new(ActorType::Service, "other")).expect("no double tag"),
        exact_tagged
    );

    // 4,097 chars (1 char past window): MUST NOT merge (would evict `/*`), appends standalone tag.
    let over_sql = format!(
        "UPDATE invoice SET status = 'ok' /*z_trace='{}'*/;",
        "a".repeat(exact_len + 1)
    );
    let over_tagged = tag(&over_sql, &actor).expect("append when merge exceeds 4096");
    assert_eq!(
        over_tagged,
        format!("{over_sql} /*auditstream='{encoded}'*/")
    );
    assert_eq!(
        decode(&over_tagged).into_actor().expect("parse"),
        Some(actor.clone())
    );

    // Exact 4,096 vs 4,097 Unicode scalar values with 4-byte astral emoji (16,000+ UTF-8 bytes!).
    let emoji_exact = format!(
        "UPDATE invoice SET status = 'ok' /*z_trace='{}'*/;",
        "🚀".repeat(exact_len)
    );
    let emoji_exact_tagged = tag(&emoji_exact, &actor).expect("merge emoji at 4096 chars");
    let emoji_start = emoji_exact_tagged.rfind("/*").expect("opening /*");
    assert_eq!(emoji_exact_tagged[emoji_start..].chars().count(), 4096);
    assert!(emoji_exact_tagged[emoji_start..].len() > 12_000);
    assert_eq!(
        decode(&emoji_exact_tagged).into_actor().expect("parse"),
        Some(actor.clone())
    );

    let emoji_over = format!(
        "UPDATE invoice SET status = 'ok' /*z_trace='{}'*/;",
        "🚀".repeat(exact_len + 1)
    );
    let emoji_over_tagged = tag(&emoji_over, &actor).expect("append emoji at 4097 chars");
    assert_eq!(
        emoji_over_tagged,
        format!("{emoji_over} /*auditstream='{encoded}'*/")
    );
    assert_eq!(
        decode(&emoji_over_tagged).into_actor().expect("parse"),
        Some(actor.clone())
    );

    // Existing comment already outside 4,096-char tail window (even with a valid `auditstream` key)
    // decodes as Absent and MUST be tagged with a new standalone trailing comment.
    let evicted = format!(
        "UPDATE invoice SET status = 'ok' /*auditstream='{encoded}',z_pad='{}'*/",
        "z".repeat(4100)
    );
    assert_eq!(decode(&evicted), DecodedActor::Absent);
    let retagged = tag(&evicted, &actor).expect("tag evicted comment");
    assert_eq!(retagged, format!("{evicted} /*auditstream='{encoded}'*/"));
    assert_eq!(
        decode(&retagged).into_actor().expect("parse"),
        Some(actor.clone())
    );

    // Huge trailing whitespace suffix pushing merged comment past 4,096 chars also falls back to append.
    let huge_suffix = format!(
        "UPDATE invoice SET status = 'ok' /*traceparent='00-abc'*/{}",
        " ".repeat(4080)
    );
    let huge_suffix_tagged = tag(&huge_suffix, &actor).expect("tag huge suffix");
    assert_eq!(
        huge_suffix_tagged,
        format!("{huge_suffix} /*auditstream='{encoded}'*/")
    );
    assert_eq!(
        decode(&huge_suffix_tagged).into_actor().expect("parse"),
        Some(actor)
    );
}

#[test]
fn decode_and_tag_reject_star_adjacent_block_comments_matching_pg_resolve_actor() {
    let actor = Actor::new(ActorType::User, "usr_real");
    let encoded = encode(&actor).expect("encode usr_real");
    let fake = encode(&Actor::new(ActorType::User, "usr_spoof")).expect("encode spoof");

    // `RETURNING */*traceparent='00-abc'*/` is valid PG SQL, but `/*` immediately follows `*`,
    // which PG `resolve_actor()` rejects per spec/transport.md. `tag()` must append a standalone
    // space-delimited comment rather than merging after `*`.
    let merge_candidate = "UPDATE invoice SET status = 'ok' RETURNING */*traceparent='00-abc'*/;";
    let tagged_merge = tag(merge_candidate, &actor).expect("append after star-adjacent comment");
    assert_eq!(
        tagged_merge,
        format!("{merge_candidate} /*auditstream='{encoded}'*/")
    );
    assert_eq!(
        decode(&tagged_merge).into_actor().expect("parse"),
        Some(actor.clone())
    );

    // Star-adjacent `/*auditstream='...'*/` must decode as Absent and must not suppress tagging.
    let star_tagged =
        format!("UPDATE invoice SET status = 'ok' RETURNING */*auditstream='{fake}'*/;");
    assert_eq!(decode(&star_tagged), DecodedActor::Absent);
    let retagged = tag(&star_tagged, &actor).expect("tag star-adjacent auditstream");
    assert_eq!(
        retagged,
        format!("{star_tagged} /*auditstream='{encoded}'*/")
    );
    assert_eq!(
        decode(&retagged).into_actor().expect("parse"),
        Some(actor.clone())
    );

    // Exact 4,096-char boundary nested comment `/* /* */*auditstream='...'*/<spaces>` (where `/`
    // of `*/` is char 4,096 from end and `*` of `*/` is char 4,097) must decode as Absent and must
    // not suppress tagging.
    let spoof_after_slash = format!("*auditstream='{fake}'*/");
    let pad_spaces = " ".repeat(4096 - 1 - spoof_after_slash.len());
    let boundary_nested =
        format!("UPDATE invoice SET status = 'ok' /* /* */{spoof_after_slash}{pad_spaces}");
    assert_eq!(decode(&boundary_nested), DecodedActor::Absent);
    let tagged_nested = tag(&boundary_nested, &actor).expect("tag boundary nested spoof");
    assert_eq!(
        decode(&tagged_nested).into_actor().expect("parse"),
        Some(actor)
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

    let nul_actor = Actor::new(ActorType::User, "usr\0_42");
    assert!(matches!(
        encode(&nul_actor),
        Err(CoreError::NullByteInField { field: "actor.id" })
    ));
    assert!(matches!(
        set_config(&nul_actor),
        Err(CoreError::NullByteInField { field: "actor.id" })
    ));
    assert!(matches!(
        tag("SELECT 1", &nul_actor),
        Err(CoreError::NullByteInField { field: "actor.id" })
    ));

    let nul_ctx = Context {
        request_id: Some("req\0_1".to_owned()),
        ..Context::default()
    };
    assert!(matches!(
        encode_with_context(&actor, Some(&nul_ctx)),
        Err(CoreError::NullByteInField {
            field: "context.requestId"
        })
    ));
    assert!(matches!(
        set_config_with_context(&actor, Some(&nul_ctx)),
        Err(CoreError::NullByteInField {
            field: "context.requestId"
        })
    ));
    assert!(matches!(
        tag_with_context("SELECT 1", &actor, Some(&nul_ctx)),
        Err(CoreError::NullByteInField {
            field: "context.requestId"
        })
    ));

    let mut nul_obo = Actor::new(ActorType::AiAgent, "agent_1");
    nul_obo.on_behalf_of = Some(NestedActor::new(ActorType::User, "usr\0_9"));
    assert!(matches!(
        encode_with_context(&nul_obo, None),
        Err(CoreError::NullByteInField {
            field: "onBehalfOf.id"
        })
    ));

    // Escaped backslash followed by literal `u0000` (`\\u0000`) is NOT a NUL escape and MUST succeed.
    let res_literal_backslash = resolve_precedence(
        "SELECT 1",
        Some(r#"{"type":"user","id":"a\\u0000b"}"#),
        None,
    );
    assert_eq!(
        res_literal_backslash.actor_source,
        Some(ActorSource::SetConfig)
    );
    assert_eq!(
        res_literal_backslash.actor,
        Some(serde_json::json!({"type": "user", "id": r"a\u0000b"}))
    );

    // Precedence rejects NUL in both set_config and jwt_claims (raw byte, JSON \u0000 escape,
    // or shadowed duplicate keys).
    let res_raw_sc = resolve_precedence(
        "SELECT 1",
        Some("{\"type\":\"user\",\"id\":\"u\u{0000}1\"}"),
        None,
    );
    assert_eq!(res_raw_sc.actor_source, Some(ActorSource::Invalid));
    assert_eq!(res_raw_sc.actor, None);

    let res_dup_sc = resolve_precedence(
        "SELECT 1",
        Some(r#"{"type":"user","id":"u\u00001","id":"u1"}"#),
        None,
    );
    assert_eq!(res_dup_sc.actor_source, Some(ActorSource::Invalid));
    assert_eq!(res_dup_sc.actor, None);

    let res_jwt_extra = resolve_precedence(
        "SELECT 1",
        None,
        Some(r#"{"sub":"u1","extra":"bad\u0000val"}"#),
    );
    assert_eq!(res_jwt_extra.actor_source, Some(ActorSource::Invalid));
    assert_eq!(res_jwt_extra.actor, None);
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

fn arb_sql_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("UPDATE invoice SET status = 'ok'".to_owned()),
        Just("UPDATE invoice SET status = 'it''s -- still quoted'".to_owned()),
        Just(r"UPDATE invoice SET status = E'a\' -- esc'".to_owned()),
        Just("UPDATE invoice SET status = E'a'\n'b\\' ' || 'x'".to_owned()),
        Just("UPDATE invoice SET status = $$-- dollar$$".to_owned()),
        Just("UPDATE invoice SET status = $café$/*not comment*/$café$".to_owned()),
        Just(r#"UPDATE "col--x" SET status = 'ok'"#.to_owned()),
        Just("UPDATE invoice SET status = 'ok' /* outer /* inner */ -- nested */".to_owned()),
        Just("UPDATE invoice SET status = 'ok' -- trailing note".to_owned()),
        Just(
            "UPDATE invoice SET status = 'ok' -- /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/"
                .to_owned(),
        ),
        Just("UPDATE invoice SET status = 'ok' /*traceparent='00-abc-01'*/;".to_owned()),
        Just("UPDATE invoice SET status = 'ok' /*route='foo,auditstream=',other='bar'*/".to_owned()),
        Just("UPDATE invoice SET status = 'ok' /*a_route='foo,auditstream='*/;".to_owned()),
        Just("UPDATE invoice SET status = 'ok' /*route='O\\'Reilly'*/".to_owned()),
        Just("UPDATE invoice SET status = 'ok' /* free text ,auditstream='bad' */".to_owned()),
        Just("UPDATE invoice SET status = 'ok' /*auditstream='!!!bad'*/".to_owned()),
        Just(format!(
            "UPDATE invoice SET status = 'ok' /*z_trace='{}'*/;",
            "z".repeat(4050)
        )),
        Just(format!(
            "UPDATE invoice SET status = 'ok' /*z_trace='{}'*/;",
            "🚀".repeat(3990)
        )),
    ]
}

proptest! {
    #[test]
    fn round_trips_tag_and_decode_for_any_valid_actor(
        actor in arb_actor(),
        context in proptest::option::of(arb_context()),
        sql in arb_sql_fragment()
    ) {
        let encoded = encode_with_context(&actor, context.as_ref()).expect("valid small actor");
        prop_assert!(!encoded.contains("*/"));
        prop_assert!(!encoded.contains('='));
        prop_assert!(!encoded.contains('/'));
        prop_assert!(!encoded.contains('+'));

        let tagged = tag_with_context(&sql, &actor, context.as_ref()).expect("tag sql");
        let decoded = decode(&tagged);
        let expected_ctx = context
            .as_ref()
            .map(|c| serde_json::to_value(c).expect("serialize context"));
        match decoded {
            DecodedActor::Valid {
                actor: actual_actor,
                context: actual_ctx,
            } => {
                let parsed: Actor = serde_json::from_value(actual_actor).expect("parse actor");
                prop_assert_eq!(parsed, actor);
                prop_assert_eq!(actual_ctx, expected_ctx);
            }
            other => prop_assert!(false, "expected Valid, got {other:?} for {tagged:?}"),
        }

        // Re-tagging an already-tagged statement MUST be idempotent.
        let other = Actor::new(ActorType::Service, "other_svc");
        let retagged = tag_with_context(&tagged, &other, None).expect("retag idempotent");
        prop_assert_eq!(retagged, tagged);
    }

    #[test]
    fn decode_and_tag_never_panic_on_arbitrary_utf8_sql(
        raw_sql in ".*",
        actor in arb_actor(),
    ) {
        let _ = decode(&raw_sql);
        let _ = tag(&raw_sql, &actor).expect("tag arbitrary utf8");
    }
}
