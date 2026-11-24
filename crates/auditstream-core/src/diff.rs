//! Pure diff helpers agreeing with the PostgreSQL trigger's `changed` column.
//!
//! Comparing row snapshots follows PostgreSQL `jsonb` semantics:
//!
//! - Top-level keys are ordered by UTF-8 byte length first, then bytewise (`jsonb` key order:
//!   for example `id`, `total`, `status`).
//! - Numeric values compare by mathematical value (`1.0 == 1.00`, `-0 == 0`) with arbitrary
//!   precision preserved beyond IEEE-754 `f64`.
//! - A missing key and a key whose value is JSON `null` are distinct.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde_json::Value;
use serde_json::value::RawValue;

/// Compares two object keys using PostgreSQL `jsonb` key order: shorter UTF-8 byte length
/// first, then bytewise lexicographic order.
///
/// # Examples
///
/// ```
/// use auditstream_core::diff::jsonb_key_cmp;
///
/// let mut keys = ["status", "total", "id"];
/// keys.sort_by(|a, b| jsonb_key_cmp(a, b));
/// assert_eq!(keys, ["id", "total", "status"]);
/// ```
pub fn jsonb_key_cmp(a: &str, b: &str) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

/// Computes the top-level keys whose values differ between `before` and `after`, returned in
/// PostgreSQL `jsonb` key order.
///
/// Returns `None` unless both `before` and `after` are `Some` and parse as JSON objects.
/// Numbers compare by exact mathematical value (so `1.0` equals `1.00`, while precision beyond
/// `f64` is never lost), and a missing key is distinct from a key set to JSON `null`.
///
/// # Examples
///
/// ```
/// use auditstream_core::diff::changed;
/// use serde_json::value::RawValue;
///
/// let before = RawValue::from_string(
///     r#"{"id":1,"status":"open","total":10.0}"#.to_owned(),
/// ).unwrap();
/// let after = RawValue::from_string(
///     r#"{"id":1,"status":"paid","total":10.00}"#.to_owned(),
/// ).unwrap();
///
/// assert_eq!(
///     changed(Some(&before), Some(&after)),
///     Some(vec!["status".to_owned()])
/// );
/// assert_eq!(changed(None, Some(&after)), None);
/// ```
pub fn changed(before: Option<&RawValue>, after: Option<&RawValue>) -> Option<Vec<String>> {
    let (Some(before_raw), Some(after_raw)) = (before, after) else {
        return None;
    };

    let Ok(Value::Object(before_map)) = serde_json::from_str::<Value>(before_raw.get()) else {
        return None;
    };
    let Ok(Value::Object(after_map)) = serde_json::from_str::<Value>(after_raw.get()) else {
        return None;
    };

    let mut all_keys = BTreeSet::new();
    for k in before_map.keys() {
        all_keys.insert(k.as_str());
    }
    for k in after_map.keys() {
        all_keys.insert(k.as_str());
    }

    let mut diff_keys: Vec<String> = all_keys
        .into_iter()
        .filter(|key| match (before_map.get(*key), after_map.get(*key)) {
            (Some(left), Some(right)) => !jsonb_values_equal(left, right),
            (None, None) => false,
            _ => true,
        })
        .map(str::to_owned)
        .collect();

    diff_keys.sort_by(|a, b| jsonb_key_cmp(a, b));
    Some(diff_keys)
}

fn jsonb_values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => {
            canonical_decimal(a.as_str()) == canonical_decimal(b.as_str())
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| jsonb_values_equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|bv| jsonb_values_equal(v, bv)))
        }
        _ => false,
    }
}

/// Exact canonical representation of a JSON decimal number: `(-1)^negative * digits * 10^exponent`,
/// with `digits` stripped of leading and trailing zeros, and zero normalized to `(false, b"0", 0)`.
#[derive(Debug, PartialEq, Eq)]
struct CanonicalDecimal {
    negative: bool,
    digits: Vec<u8>,
    exponent: i128,
}

fn canonical_decimal(raw: &str) -> CanonicalDecimal {
    let s = raw.trim();
    let (negative, unsigned) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };

    let (mantissa, explicit_exp) = match unsigned.find(['e', 'E']) {
        Some(idx) => {
            let exp_part = &unsigned[idx + 1..];
            let parsed_exp = exp_part.parse::<i128>().unwrap_or(0);
            (&unsigned[..idx], parsed_exp)
        }
        None => (unsigned, 0),
    };

    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((int_p, frac_p)) => (int_p, frac_p),
        None => (mantissa, ""),
    };

    let mut digits = Vec::with_capacity(int_part.len() + frac_part.len());
    digits.extend_from_slice(int_part.as_bytes());
    digits.extend_from_slice(frac_part.as_bytes());

    let frac_len = i128::try_from(frac_part.len()).unwrap_or(0);
    let mut exponent = explicit_exp.saturating_sub(frac_len);

    // Trim leading zeros.
    let first_non_zero = digits.iter().position(|&b| b != b'0');
    let Some(start) = first_non_zero else {
        return CanonicalDecimal {
            negative: false,
            digits: vec![b'0'],
            exponent: 0,
        };
    };

    // Trim trailing zeros and fold them into the base-10 exponent.
    let last_non_zero = digits
        .iter()
        .rposition(|&b| b != b'0')
        .unwrap_or(digits.len() - 1);
    let trailing_zeros = i128::try_from(digits.len() - 1 - last_non_zero).unwrap_or(0);
    exponent = exponent.saturating_add(trailing_zeros);

    CanonicalDecimal {
        negative,
        digits: digits[start..=last_non_zero].to_vec(),
        exponent,
    }
}
