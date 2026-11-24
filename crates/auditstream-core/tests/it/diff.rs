use auditstream_core::diff::{changed, jsonb_key_cmp};
use proptest::prelude::*;
use serde_json::value::RawValue;

fn raw(s: &str) -> Box<RawValue> {
    RawValue::from_string(s.to_owned()).expect("valid json")
}

#[test]
fn returns_none_when_either_image_is_absent_or_not_an_object() {
    let obj = raw(r#"{"id":1}"#);
    let arr = raw("[1,2]");
    assert_eq!(changed(None, None), None);
    assert_eq!(changed(None, Some(&obj)), None);
    assert_eq!(changed(Some(&obj), None), None);
    assert_eq!(changed(Some(&arr), Some(&obj)), None);
    assert_eq!(changed(Some(&obj), Some(&arr)), None);
}

#[test]
fn table_driven_key_order_and_edge_cases() {
    struct Case {
        name: &'static str,
        before: &'static str,
        after: &'static str,
        expected: Vec<&'static str>,
    }

    let cases = [
        Case {
            name: "jsonb key order: shorter first then bytewise (id, total, status)",
            before: r#"{"status":"open","total":10,"id":1,"aa":1,"b":1}"#,
            after: r#"{"status":"paid","total":20,"id":2,"aa":2,"b":2}"#,
            expected: vec!["b", "aa", "id", "total", "status"],
        },
        Case {
            name: "numbers equal by value: 1.0 == 1.00 == 1 == 1e0 and -0 == 0.00",
            before: r#"{"a":1.0,"b":1,"c":100e-2,"d":-0.0,"e":0}"#,
            after: r#"{"a":1.00,"b":1e0,"c":1.000,"d":0,"e":0.00}"#,
            expected: vec![],
        },
        Case {
            name: "precision beyond f64 preserved: 40-digit decimals differing in last digit",
            before: r#"{"amount":123456789012345678901234567890.1234567890,"same":123456789012345678901234567890.500}"#,
            after: r#"{"amount":123456789012345678901234567890.1234567891,"same":123456789012345678901234567890.5}"#,
            expected: vec!["amount"],
        },
        Case {
            name: "u64 overflow integers differing by 1",
            before: r#"{"big":18446744073709551616}"#,
            after: r#"{"big":18446744073709551617}"#,
            expected: vec!["big"],
        },
        Case {
            name: "added key appears in changed",
            before: r#"{"id":1}"#,
            after: r#"{"id":1,"status":"open"}"#,
            expected: vec!["status"],
        },
        Case {
            name: "removed key appears in changed",
            before: r#"{"id":1,"legacy":"x"}"#,
            after: r#"{"id":1}"#,
            expected: vec!["legacy"],
        },
        Case {
            name: "null versus missing key are distinct in both directions",
            before: r#"{"a":null}"#,
            after: r#"{"b":null}"#,
            expected: vec!["a", "b"],
        },
        Case {
            name: "nested object change detected while equal nested object with different key order is unchanged",
            before: r#"{"same":{"x":1.0,"y":2},"changed":{"a":{"k":1}},"arr":[1.0,2]}"#,
            after: r#"{"same":{"y":2,"x":1.00},"changed":{"a":{"k":2}},"arr":[1.00,2]}"#,
            expected: vec!["changed"],
        },
        Case {
            name: "identical images give an empty list",
            before: r#"{"id":1,"status":"paid","meta":{"tags":["a","b"]}}"#,
            after: r#"{"meta":{"tags":["a","b"]},"status":"paid","id":1}"#,
            expected: vec![],
        },
    ];

    for case in cases {
        let before = raw(case.before);
        let after = raw(case.after);
        let actual = changed(Some(&before), Some(&after)).expect("both are objects");
        let expected: Vec<String> = case.expected.into_iter().map(str::to_owned).collect();
        assert_eq!(actual, expected, "case failed: {}", case.name);
    }
}

#[test]
fn jsonb_key_cmp_uses_utf8_byte_length_then_lexicographic() {
    let mut keys = vec!["é", "aa", "b", "status", "total", "id"];
    // byte lengths: "b" (1), "aa" (2), "id" (2), "é" (2, 0xC3 0xA9), "total" (5), "status" (6)
    keys.sort_by(|a, b| jsonb_key_cmp(a, b));
    assert_eq!(keys, vec!["b", "aa", "id", "é", "total", "status"]);
}

proptest! {
    #[test]
    fn changed_on_identical_json_object_is_always_empty(
        entries in proptest::collection::btree_map("[a-z_]{1,12}", any::<i64>(), 0..10)
    ) {
        let json = serde_json::to_string(&entries).expect("serialize map");
        let snapshot = raw(&json);
        prop_assert_eq!(changed(Some(&snapshot), Some(&snapshot)), Some(Vec::new()));
    }
}
