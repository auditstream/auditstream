//! Pure event redaction helpers that mask or remove configured columns before events
//! leave the engine.
//!
//! Edits operate directly on raw JSON top-level object slices so untouched columns retain
//! their exact original key order and exact numeric text (without rounding or reformatting).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::{Event, Source};

/// Replacement JSON string emitted by [`RedactionMode::Mask`].
///
/// # Examples
///
/// ```
/// use auditstream_core::redact::MASK_PLACEHOLDER;
///
/// assert_eq!(MASK_PLACEHOLDER, "[redacted]");
/// ```
pub const MASK_PLACEHOLDER: &str = "[redacted]";

/// Action applied to each matching column during [`apply`].
///
/// # Examples
///
/// ```
/// use auditstream_core::redact::RedactionMode;
///
/// let mode: RedactionMode = serde_json::from_str("\"mask\"").unwrap();
/// assert_eq!(mode, RedactionMode::Mask);
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionMode {
    /// Replace the column's value with `"[redacted]"` while keeping the key in `changed`.
    #[default]
    Mask,
    /// Remove the column from `before` and `after` and drop it from `changed`.
    Remove,
}

/// Redaction rule targeting a database table, an application action, or all events.
///
/// - When `table` is `Some("*")` or both `table` and `action` are `None`, the rule applies
///   to all `db` and `app` events.
/// - When `table` is `Some("invoice")`, the rule matches `db` events with `table_name = "invoice"`.
/// - When `table` is `Some("public.invoice")`, the rule matches `db` events with matching
///   `schema_name` and `table_name`.
/// - When `action` is `Some("invoice.exported")`, the rule matches `app` events with matching `op`.
///
/// # Examples
///
/// ```
/// use auditstream_core::redact::{RedactionMode, RedactionRule};
///
/// let rule = RedactionRule::for_table("invoice", ["ssn", "tax_id"], RedactionMode::Mask);
/// assert_eq!(rule.table.as_deref(), Some("invoice"));
/// assert_eq!(rule.columns, vec!["ssn", "tax_id"]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactionRule {
    /// Target table name (`"invoice"` or `"public.invoice"`), or `"*"` for all events.
    #[serde(default)]
    pub table: Option<String>,

    /// Optional application event action name (`op`) for `source = "app"` events.
    #[serde(default)]
    pub action: Option<String>,

    /// Top-level column/field names inside `before` and `after` to redact.
    pub columns: Vec<String>,

    /// Redaction behavior (`mask` or `remove`, defaulting to `mask`).
    #[serde(default)]
    pub mode: RedactionMode,
}

impl RedactionRule {
    /// Creates a rule matching a database table (either `"table"` or `"schema.table"`).
    ///
    /// # Examples
    ///
    /// ```
    /// use auditstream_core::redact::{RedactionMode, RedactionRule};
    ///
    /// let rule = RedactionRule::for_table("users", ["password_hash"], RedactionMode::Remove);
    /// assert_eq!(rule.mode, RedactionMode::Remove);
    /// ```
    pub fn for_table<I, S>(table: impl Into<String>, columns: I, mode: RedactionMode) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            table: Some(table.into()),
            action: None,
            columns: columns.into_iter().map(Into::into).collect(),
            mode,
        }
    }

    /// Creates a rule matching an application event (`source = "app"`) by action name (`op`).
    ///
    /// # Examples
    ///
    /// ```
    /// use auditstream_core::redact::{RedactionMode, RedactionRule};
    ///
    /// let rule = RedactionRule::for_app_action("invoice.sent", ["recipient_email"], RedactionMode::Mask);
    /// assert_eq!(rule.action.as_deref(), Some("invoice.sent"));
    /// ```
    pub fn for_app_action<I, S>(action: impl Into<String>, columns: I, mode: RedactionMode) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            table: None,
            action: Some(action.into()),
            columns: columns.into_iter().map(Into::into).collect(),
            mode,
        }
    }

    /// Creates a rule matching all `db` and `app` events.
    ///
    /// # Examples
    ///
    /// ```
    /// use auditstream_core::redact::{RedactionMode, RedactionRule};
    ///
    /// let rule = RedactionRule::for_all(["secret_token"], RedactionMode::Mask);
    /// assert_eq!(rule.table.as_deref(), Some("*"));
    /// ```
    pub fn for_all<I, S>(columns: I, mode: RedactionMode) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            table: Some("*".to_owned()),
            action: None,
            columns: columns.into_iter().map(Into::into).collect(),
            mode,
        }
    }

    fn matches(&self, event: &Event) -> bool {
        if self.table.is_none() && self.action.is_none() {
            return true;
        }
        if self.table.as_deref() == Some("*") {
            return true;
        }
        if let Some(ref rule_table) = self.table
            && event.source == Source::Db
        {
            if let Some((rule_schema, rule_tbl)) = rule_table.split_once('.') {
                if event.schema_name.as_deref() == Some(rule_schema)
                    && event.table_name.as_deref() == Some(rule_tbl)
                {
                    return true;
                }
            } else if event.table_name.as_deref() == Some(rule_table.as_str()) {
                return true;
            }
        }
        if let Some(ref rule_action) = self.action
            && event.source == Source::App
            && (rule_action == "*" || event.op.as_str() == rule_action)
        {
            return true;
        }
        false
    }
}

/// Ordered collection of [`RedactionRule`]s deserializable from configuration (such as
/// `auditstream.toml`).
///
/// # Examples
///
/// ```
/// use auditstream_core::redact::{Redaction, RedactionMode, RedactionRule};
///
/// let redaction = Redaction::new(vec![
///     RedactionRule::for_table("invoice", ["ssn"], RedactionMode::Mask),
/// ]);
/// assert!(!redaction.is_empty());
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Redaction {
    /// Rules applied to matching events.
    #[serde(default)]
    pub rules: Vec<RedactionRule>,
}

impl Redaction {
    /// Creates a [`Redaction`] configuration from a list of rules.
    ///
    /// # Examples
    ///
    /// ```
    /// use auditstream_core::redact::{Redaction, RedactionMode, RedactionRule};
    ///
    /// let redaction = Redaction::new(vec![
    ///     RedactionRule::for_table("invoice", ["ssn"], RedactionMode::Mask),
    /// ]);
    /// assert_eq!(redaction.rules.len(), 1);
    /// ```
    pub fn new(rules: Vec<RedactionRule>) -> Self {
        Self { rules }
    }

    /// Returns `true` if no redaction rules are configured.
    ///
    /// # Examples
    ///
    /// ```
    /// use auditstream_core::redact::Redaction;
    ///
    /// assert!(Redaction::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Applies all matching rules in `redaction` to `event` in place.
///
/// - Modifies `before` and `after` for both `db` and `app` events while leaving `record_id`
///   untouched.
/// - Untouched keys keep their exact original order and exact number text.
/// - Masked keys remain in `changed`; removed keys are dropped from `changed`.
/// - When both a `mask` and a `remove` rule match the same column on an event, `remove` wins.
/// - Idempotent: applying the same [`Redaction`] twice produces the same [`Event`].
///
/// # Examples
///
/// ```
/// use auditstream_core::{CURRENT_SCHEMA_VERSION, Event, Op, Source};
/// use auditstream_core::redact::{Redaction, RedactionMode, RedactionRule, apply};
/// use serde_json::value::RawValue;
/// use time::OffsetDateTime;
/// use uuid::Uuid;
///
/// let mut event = Event {
///     event_id: Uuid::nil(),
///     occurred_at: OffsetDateTime::UNIX_EPOCH,
///     source: Source::Db,
///     schema_name: Some("public".to_owned()),
///     table_name: Some("invoice".to_owned()),
///     op: Op::Update,
///     record_id: Some(RawValue::from_string(r#"{"id":1}"#.to_owned()).unwrap()),
///     before: Some(RawValue::from_string(r#"{"z":1.10,"secret":"old"}"#.to_owned()).unwrap()),
///     after: Some(RawValue::from_string(r#"{"z":1.10,"secret":"new"}"#.to_owned()).unwrap()),
///     changed: Some(vec!["secret".to_owned()]),
///     actor: None,
///     context: None,
///     actor_source: None,
///     db_user: "app_user".to_owned(),
///     schema_version: CURRENT_SCHEMA_VERSION,
/// };
///
/// let redaction = Redaction::new(vec![
///     RedactionRule::for_table("invoice", ["secret"], RedactionMode::Mask),
/// ]);
/// apply(&mut event, &redaction);
///
/// assert_eq!(event.after.as_deref().unwrap().get(), r#"{"z":1.10,"secret":"[redacted]"}"#);
/// assert_eq!(event.changed, Some(vec!["secret".to_owned()]));
/// ```
pub fn apply(event: &mut Event, redaction: &Redaction) {
    let mut masked_cols = HashSet::<&str>::new();
    let mut removed_cols = HashSet::<&str>::new();

    for rule in &redaction.rules {
        if !rule.matches(event) {
            continue;
        }
        for col in &rule.columns {
            match rule.mode {
                RedactionMode::Mask => {
                    masked_cols.insert(col.as_str());
                }
                RedactionMode::Remove => {
                    removed_cols.insert(col.as_str());
                }
            }
        }
    }

    if masked_cols.is_empty() && removed_cols.is_empty() {
        return;
    }

    if let Some(ref before) = event.before
        && let Some(updated) = redact_raw_object(before.get(), &masked_cols, &removed_cols)
    {
        event.before = Some(updated);
    }

    if let Some(ref after) = event.after
        && let Some(updated) = redact_raw_object(after.get(), &masked_cols, &removed_cols)
    {
        event.after = Some(updated);
    }

    if !removed_cols.is_empty()
        && let Some(ref mut changed) = event.changed
    {
        changed.retain(|col| !removed_cols.contains(col.as_str()));
    }
}

/// Rewrites top-level entries of a JSON object preserving exact key order and raw value slices
/// for untouched keys. Returns `None` if `raw` is not a JSON object or no keys needed rewriting.
fn redact_raw_object(
    raw: &str,
    masked: &HashSet<&str>,
    removed: &HashSet<&str>,
) -> Option<Box<RawValue>> {
    let entries = parse_top_level_object_entries(raw)?;

    let mut any_hit = false;
    for entry in &entries {
        if removed.contains(entry.key.as_str()) || masked.contains(entry.key.as_str()) {
            any_hit = true;
            break;
        }
    }
    if !any_hit {
        return None;
    }

    let mut out = String::with_capacity(raw.len());
    out.push('{');
    let mut first = true;
    for entry in entries {
        if removed.contains(entry.key.as_str()) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(entry.raw_key);
        out.push(':');
        if masked.contains(entry.key.as_str()) {
            out.push('"');
            out.push_str(MASK_PLACEHOLDER);
            out.push('"');
        } else {
            out.push_str(entry.raw_val);
        }
    }
    out.push('}');

    RawValue::from_string(out).ok()
}

struct RawEntry<'a> {
    key: String,
    raw_key: &'a str,
    raw_val: &'a str,
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Scans a JSON string token starting at `start` (which must point to `b'"'`) and returns the
/// exclusive end index right after the closing `b'"'`.
fn scan_json_string(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 2;
            }
            b'"' => return Some(i + 1),
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Scans any single JSON value starting at `start` and returns the exclusive end index.
fn scan_json_value(bytes: &[u8], start: usize) -> Option<usize> {
    let first = *bytes.get(start)?;
    match first {
        b'"' => scan_json_string(bytes, start),
        b'{' | b'[' => {
            let mut i = start;
            let mut depth = 0usize;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => {
                        i = scan_json_string(bytes, i)?;
                    }
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        i += 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            None
        }
        _ => {
            let mut i = start;
            while i < bytes.len()
                && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            (i > start).then_some(i)
        }
    }
}

fn parse_top_level_object_entries(raw: &str) -> Option<Vec<RawEntry<'_>>> {
    let bytes = raw.as_bytes();
    let mut i = skip_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_ws(bytes, i + 1);

    let mut entries = Vec::new();
    if bytes.get(i) == Some(&b'}') {
        return Some(entries);
    }

    loop {
        let key_start = skip_ws(bytes, i);
        let key_end = scan_json_string(bytes, key_start)?;
        let raw_key = &raw[key_start..key_end];
        let key: String = serde_json::from_str(raw_key).ok()?;

        let colon = skip_ws(bytes, key_end);
        if bytes.get(colon) != Some(&b':') {
            return None;
        }

        let val_start = skip_ws(bytes, colon + 1);
        let val_end = scan_json_value(bytes, val_start)?;
        let raw_val = &raw[val_start..val_end];

        entries.push(RawEntry {
            key,
            raw_key,
            raw_val,
        });

        let next = skip_ws(bytes, val_end);
        match bytes.get(next) {
            Some(&b',') => {
                i = next + 1;
            }
            Some(&b'}') => {
                return Some(entries);
            }
            _ => return None,
        }
    }
}
