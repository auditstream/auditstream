use auditstream_core::redact::{MASK_PLACEHOLDER, Redaction, RedactionMode, RedactionRule, apply};
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

    apply(&mut event, &redaction);

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
fn applies_to_app_events_by_action_or_wildcard_and_leaves_other_tables_untouched() {
    // 1. Rules for another table leave db event completely untouched.
    let mut db_event = sample_db_event();
    let original_db = db_event.clone();
    let other_table = Redaction::new(vec![RedactionRule::for_table(
        "line_item",
        ["ssn", "token"],
        RedactionMode::Remove,
    )]);
    apply(&mut db_event, &other_table);
    assert_eq!(db_event, original_db);

    // 2. App event matched by action name and wildcard rule.
    let mut app_event = sample_app_event();
    let app_rules = Redaction::new(vec![
        RedactionRule::for_app_action("invoice.exported", ["recipient"], RedactionMode::Mask),
        RedactionRule::for_all(["api_secret"], RedactionMode::Remove),
    ]);
    apply(&mut app_event, &app_rules);

    assert_eq!(
        app_event.after.as_deref().expect("after").get(),
        r#"{"recipient":"[redacted]","amount":9.50}"#
    );
    assert_eq!(
        app_event.record_id.as_deref().expect("record_id").get(),
        r#"{"id":42,"ssn":"keep-pk"}"#
    );
}

#[test]
fn redaction_deserializes_with_serde_for_cli_config() {
    let json = r#"{
      "rules": [
        { "table": "invoice", "columns": ["ssn"], "mode": "mask" },
        { "table": "public.users", "columns": ["password_hash"], "mode": "remove" },
        { "action": "invoice.exported", "columns": ["recipient"] }
      ]
    }"#;
    let parsed: Redaction = serde_json::from_str(json).expect("deserialize Redaction");
    assert_eq!(parsed.rules.len(), 3);
    assert_eq!(parsed.rules[0].mode, RedactionMode::Mask);
    assert_eq!(parsed.rules[1].mode, RedactionMode::Remove);
    assert_eq!(parsed.rules[2].mode, RedactionMode::Mask);
    assert_eq!(parsed.rules[2].action.as_deref(), Some("invoice.exported"));
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
        let redaction = Redaction::new(vec![
            RedactionRule::for_table("invoice", ["secret_field"], mode),
        ]);

        apply(&mut event, &redaction);
        let after_first = event.clone();
        apply(&mut event, &redaction);

        // Idempotent!
        prop_assert_eq!(&event, &after_first);

        // Serialized event JSON NEVER contains the original secret value!
        let serialized = serde_json::to_string(&event).expect("serialize redacted event");
        prop_assert!(!serialized.contains(&secret));
    }
}
