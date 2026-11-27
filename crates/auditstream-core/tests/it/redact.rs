use auditstream_core::redact::{
    CompiledRedaction, MASK_PLACEHOLDER, RedactError, Redaction, RedactionMode, RedactionRule,
    apply,
};
use auditstream_core::{ActorSource, CURRENT_SCHEMA_VERSION, Event, Op, Source};
use proptest::prelude::*;
use serde_json::value::RawValue;
use time::{Date, Month, PrimitiveDateTime, Time};
use uuid::Uuid;

fn raw(s: &str) -> Box<RawValue> {
    RawValue::from_string(s.to_owned()).expect("valid raw json")
}

fn sample_db_event() -> Event {
    let ts = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 24).expect("date"),
        Time::from_hms_micro(10, 15, 0, 123_456).expect("time"),
    )
    .assume_utc();

    Event {
        event_id: Uuid::from_u128(1),
        occurred_at: ts,
        source: Source::Db,
        schema_name: Some("public".to_owned()),
        table_name: Some("invoice".to_owned()),
        op: Op::Update,
        record_id: Some(raw(r#"{"id":42,"ssn":"keep-pk"}"#)),
        before: Some(raw(
            r#"{"z_total":123456789012345678901234567890.1234567890,"ssn":"111-11-1111","token":"tok_old","a_price":1.10}"#,
        )),
        after: Some(raw(
            r#"{"z_total":123456789012345678901234567890.1234567890,"ssn":"222-22-2222","token":"tok_new","a_price":1.10}"#,
        )),
        changed: Some(vec![
            "ssn".to_owned(),
            "token".to_owned(),
            "z_total".to_owned(),
        ]),
        actor: Some(raw(r#"{"type":"user","id":"usr_1"}"#)),
        context: None,
        actor_source: Some(ActorSource::Comment),
        db_user: "app_user".to_owned(),
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

fn sample_app_event() -> Event {
    let mut ev = sample_db_event();
    ev.source = Source::App;
    ev.schema_name = None;
    ev.table_name = None;
    ev.op = Op::app("invoice.exported").expect("valid app op");
    ev.before = None;
    ev.changed = None;
    ev.after = Some(raw(
        r#"{"recipient":"alice@example.com","api_secret":"sk_live_999","amount":9.50}"#,
    ));
    ev
}

#[test]
fn mask_and_remove_update_before_after_and_changed_while_preserving_record_id_key_order_and_numbers()
 {
    let mut event = sample_db_event();
    let redaction = Redaction::new(vec![
        RedactionRule::for_table("invoice", ["ssn"], RedactionMode::Mask),
        RedactionRule::for_table("invoice", ["token"], RedactionMode::Remove),
    ]);

    apply(&mut event, &redaction).expect("valid redaction");

    // record_id is NEVER modified even when it shares a key name (`ssn`).
    assert_eq!(
        event.record_id.as_deref().expect("record_id").get(),
        r#"{"id":42,"ssn":"keep-pk"}"#
    );

    // Exact original key order (`z_total`, `ssn`, `a_price`) and exact numeric text survive!
    let expected_json = format!(
        r#"{{"z_total":123456789012345678901234567890.1234567890,"ssn":"{MASK_PLACEHOLDER}","a_price":1.10}}"#
    );
    assert_eq!(
        event.before.as_deref().expect("before").get(),
        expected_json
    );
    assert_eq!(event.after.as_deref().expect("after").get(), expected_json);

    // Masked key stays in `changed`; removed key is dropped from `changed`.
    assert_eq!(
        event.changed,
        Some(vec!["ssn".to_owned(), "z_total".to_owned()])
    );
}

#[test]
fn remove_overrides_mask_across_global_table_and_schema_qualified_rules() {
    let mut event = sample_db_event();
    let compiled = CompiledRedaction::compile(&Redaction::new(vec![
        RedactionRule::for_all(["ssn", "token"], RedactionMode::Mask),
        RedactionRule::for_table("public.invoice", ["ssn"], RedactionMode::Remove),
        RedactionRule::for_table("invoice", ["ssn"], RedactionMode::Mask),
    ]))
    .expect("compile rules");

    compiled.apply(&mut event).expect("apply compiled");

    assert_eq!(
        event.after.as_deref().expect("after").get(),
        r#"{"z_total":123456789012345678901234567890.1234567890,"token":"[redacted]","a_price":1.10}"#
    );
    assert_eq!(
        event.changed,
        Some(vec!["token".to_owned(), "z_total".to_owned()])
    );
}

#[test]
fn applies_to_app_events_by_action_or_wildcard_and_leaves_other_tables_untouched() {
    // 1. Rules for another table leave db event completely untouched.
    let mut db_event = sample_db_event();
    let original_db = db_event.clone();
    let other_table = CompiledRedaction::compile(&Redaction::new(vec![RedactionRule::for_table(
        "line_item",
        ["ssn", "token"],
        RedactionMode::Remove,
    )]))
    .expect("compile");
    other_table.apply(&mut db_event).expect("apply");
    assert_eq!(db_event, original_db);

    // 2. App event matched by action name, app wildcard, and global wildcard.
    let mut app_event = sample_app_event();
    let app_rules = CompiledRedaction::compile(&Redaction::new(vec![
        RedactionRule::for_app_action("invoice.exported", ["recipient"], RedactionMode::Mask),
        RedactionRule::for_all(["api_secret"], RedactionMode::Remove),
    ]))
    .expect("compile");
    app_rules.apply(&mut app_event).expect("apply");

    assert_eq!(
        app_event.after.as_deref().expect("after").get(),
        r#"{"recipient":"[redacted]","amount":9.50}"#
    );
    assert_eq!(
        app_event.record_id.as_deref().expect("record_id").get(),
        r#"{"id":42,"ssn":"keep-pk"}"#
    );

    // 3. Action wildcard matches all app events, never db events.
    let app_wildcard =
        CompiledRedaction::compile(&Redaction::new(vec![RedactionRule::for_app_action(
            "*",
            ["amount"],
            RedactionMode::Mask,
        )]))
        .expect("compile");
    app_wildcard.apply(&mut app_event).expect("apply");
    assert_eq!(
        app_event.after.as_deref().expect("after").get(),
        r#"{"recipient":"[redacted]","amount":"[redacted]"}"#
    );

    let mut db_untouched = sample_db_event();
    let db_before = db_untouched.clone();
    app_wildcard.apply(&mut db_untouched).expect("apply");
    assert_eq!(db_untouched, db_before);
}

#[test]
fn fails_closed_on_non_object_payloads_when_rule_matches_and_preserves_event() {
    let compiled =
        CompiledRedaction::compile(&Redaction::new(vec![RedactionRule::for_app_action(
            "invoice.exported",
            ["ssn"],
            RedactionMode::Mask,
        )]))
        .expect("compile");

    for (payload, expected_kind) in [
        (r#"[{"ssn":"123-45-6789"}]"#, "array"),
        (r#""123-45-6789""#, "string"),
        ("42", "number"),
        ("true", "boolean"),
        ("null", "null"),
    ] {
        let mut app_event = sample_app_event();
        app_event.after = Some(raw(payload));
        let snapshot = app_event.clone();

        let err = compiled
            .apply(&mut app_event)
            .expect_err("non-object JSON payload must fail closed");
        assert_eq!(
            err,
            RedactError::NonObjectPayload {
                event_id: app_event.event_id,
                field: "after",
                actual_kind: expected_kind,
            }
        );
        // Strong exception safety: event is not mutated on failure.
        assert_eq!(app_event, snapshot);
    }

    // When both `before` (valid object) and `after` (array) are present and `after` fails,
    // neither `before` nor `changed` is partially mutated.
    let mut db_event = sample_db_event();
    db_event.after = Some(raw(r#"[{"ssn":"222-22-2222"}]"#));
    let snapshot = db_event.clone();
    let db_rule = CompiledRedaction::compile(&Redaction::new(vec![RedactionRule::for_table(
        "invoice",
        ["ssn", "token"],
        RedactionMode::Remove,
    )]))
    .expect("compile");
    let err = db_rule
        .apply(&mut db_event)
        .expect_err("non-object after must fail closed");
    assert!(matches!(
        err,
        RedactError::NonObjectPayload {
            field: "after",
            actual_kind: "array",
            ..
        }
    ));
    assert_eq!(db_event, snapshot);

    // Non-object app payload is untouched when NO rule matches it.
    let mut unmatched = sample_app_event();
    unmatched.after = Some(raw(r#"[{"ssn":"123-45-6789"}]"#));
    let other_rule = CompiledRedaction::compile(&Redaction::new(vec![RedactionRule::for_table(
        "invoice",
        ["ssn"],
        RedactionMode::Mask,
    )]))
    .expect("compile");
    other_rule.apply(&mut unmatched).expect("unmatched rule");
}

#[test]
fn redacts_unicode_escaped_json_keys_and_handles_empty_objects() {
    let mut event = sample_db_event();
    event.before = Some(raw("{}"));
    // "\u0073sn" unescapes to "ssn"
    event.after = Some(raw(r#"{"\u0073sn":"123-45-6789","keep":1.10}"#));
    event.changed = Some(vec!["ssn".to_owned()]);

    let compiled = CompiledRedaction::compile(&Redaction::new(vec![RedactionRule::for_table(
        "invoice",
        ["ssn"],
        RedactionMode::Mask,
    )]))
    .expect("compile");

    compiled.apply(&mut event).expect("apply");
    assert_eq!(event.before.as_deref().expect("before").get(), "{}");
    assert_eq!(
        event.after.as_deref().expect("after").get(),
        r#"{"\u0073sn":"[redacted]","keep":1.10}"#
    );
}

#[test]
fn validate_and_compile_reject_every_invalid_rule_shape() {
    // 1. Neither table nor action set.
    let missing_target = Redaction::new(vec![RedactionRule {
        table: None,
        action: None,
        columns: vec!["ssn".to_owned()],
        mode: RedactionMode::Mask,
    }]);
    assert_eq!(
        missing_target.validate(),
        Err(RedactError::MissingTarget { rule_index: 0 })
    );
    assert_eq!(
        CompiledRedaction::compile(&missing_target),
        Err(RedactError::MissingTarget { rule_index: 0 })
    );

    // 2. Both table and action set.
    let both_targets = Redaction::new(vec![RedactionRule {
        table: Some("invoice".to_owned()),
        action: Some("invoice.exported".to_owned()),
        columns: vec!["ssn".to_owned()],
        mode: RedactionMode::Mask,
    }]);
    assert_eq!(
        both_targets.validate(),
        Err(RedactError::AmbiguousTarget { rule_index: 0 })
    );

    // 3. Empty columns list.
    let empty_cols = Redaction::new(vec![RedactionRule::for_table(
        "invoice",
        Vec::<String>::new(),
        RedactionMode::Mask,
    )]);
    assert_eq!(
        empty_cols.validate(),
        Err(RedactError::EmptyColumns { rule_index: 0 })
    );

    // 4. Blank column name (empty and whitespace-only).
    for blank in ["", "   "] {
        let blank_col = Redaction::new(vec![RedactionRule::for_table(
            "invoice",
            ["ssn", blank],
            RedactionMode::Mask,
        )]);
        assert_eq!(
            blank_col.validate(),
            Err(RedactError::BlankColumn { rule_index: 0 })
        );
    }

    // 5. Nested JSON dot-path syntax.
    let nested = Redaction::new(vec![RedactionRule::for_table(
        "invoice",
        ["metadata.ssn"],
        RedactionMode::Mask,
    )]);
    let err = nested.validate().expect_err("dot path must fail");
    assert_eq!(
        err,
        RedactError::NestedPathUnsupported {
            rule_index: 0,
            column: "metadata.ssn".to_owned(),
        }
    );
    assert!(
        err.to_string().contains("nested JSON path"),
        "expected clear nested path error, got: {err}"
    );
}

#[test]
fn validate_and_compile_reject_malformed_table_and_action_targets() {
    // Blank or malformed table / action targets.
    for bad_table in [
        "",
        "   ",
        "public.",
        ".invoice",
        "a.b.c",
        "public.*",
        "*.invoice",
    ] {
        let bad = Redaction::new(vec![RedactionRule::for_table(
            bad_table,
            ["ssn"],
            RedactionMode::Mask,
        )]);
        assert!(
            matches!(
                bad.validate(),
                Err(RedactError::InvalidTarget {
                    rule_index: 0,
                    field: "table",
                    ..
                })
            ),
            "expected InvalidTarget for table={bad_table:?}"
        );
    }

    let overlong_action = "a".repeat(65);
    for bad_action in [
        "",
        "   ",
        " invoice.sent",
        "invoice.sent ",
        "\u{00A0}invoice.sent",
        "invoice\nsent",
        "invoice\tsent",
        "invoice\x7fsent",
        "invoice\u{0085}sent",
        "INSERT",
        "insert",
        "Update",
        "DELETE",
        "TRUNCATE",
        overlong_action.as_str(),
    ] {
        let bad = Redaction::new(vec![RedactionRule::for_app_action(
            bad_action,
            ["ssn"],
            RedactionMode::Mask,
        )]);
        assert!(
            matches!(
                bad.validate(),
                Err(RedactError::InvalidTarget {
                    rule_index: 0,
                    field: "action",
                    ..
                })
            ),
            "expected InvalidTarget for action={bad_action:?}"
        );
    }

    // 7. Empty rule list is valid and produces an empty CompiledRedaction.
    let empty = Redaction::default();
    assert!(empty.validate().is_ok());
    let compiled = empty.compile().expect("compile empty");
    assert!(compiled.is_empty());

    // 8. Matching rule where no target column is present leaves payload untouched.
    let mut untouched = sample_db_event();
    let before_snap = untouched.clone();
    let no_hit = Redaction::new(vec![RedactionRule::for_table(
        "invoice",
        ["nonexistent_col"],
        RedactionMode::Mask,
    )])
    .compile()
    .expect("compile");
    no_hit.apply(&mut untouched).expect("no-hit apply");
    assert_eq!(untouched, before_snap);
}

#[test]
fn redaction_deserializes_with_serde_for_cli_config() {
    let json = r#"{
      "rules": [
        { "table": "invoice", "columns": ["ssn"], "mode": "mask" },
        { "table": "public.users", "columns": ["password_hash"], "mode": "remove" },
        { "action": "invoice.exported", "columns": ["recipient"] },
        { "table": "*", "columns": ["secret_token"] }
      ]
    }"#;
    let parsed: Redaction = serde_json::from_str(json).expect("deserialize Redaction");
    assert_eq!(parsed.rules.len(), 4);
    assert_eq!(parsed.rules[0].mode, RedactionMode::Mask);
    assert_eq!(parsed.rules[1].mode, RedactionMode::Remove);
    assert_eq!(parsed.rules[2].mode, RedactionMode::Mask);
    assert_eq!(parsed.rules[2].action.as_deref(), Some("invoice.exported"));
    assert!(parsed.validate().is_ok());
}

proptest! {
    #[test]
    fn redaction_is_idempotent_and_masked_values_never_leak_into_serialized_event(
        secret in "[A-Za-z0-9_]{12,32}",
        mode_remove in any::<bool>()
    ) {
        let mut event = sample_db_event();
        let before_str = serde_json::json!({ "keep": 1, "secret_field": secret }).to_string();
        let after_str = serde_json::json!({ "keep": 2, "secret_field": format!("{secret}_v2") }).to_string();
        event.before = Some(raw(&before_str));
        event.after = Some(raw(&after_str));
        event.changed = Some(vec!["keep".to_owned(), "secret_field".to_owned()]);

        let mode = if mode_remove {
            RedactionMode::Remove
        } else {
            RedactionMode::Mask
        };
        let compiled = CompiledRedaction::compile(&Redaction::new(vec![
            RedactionRule::for_table("invoice", ["secret_field"], mode),
        ]))
        .expect("compile");

        compiled.apply(&mut event).expect("first apply");
        let after_first = event.clone();
        compiled.apply(&mut event).expect("second apply");

        // Idempotent!
        prop_assert_eq!(&event, &after_first);

        // Serialized event JSON NEVER contains the original secret value!
        let serialized = serde_json::to_string(&event).expect("serialize redacted event");
        prop_assert!(!serialized.contains(&secret));
    }
}
