//! Pure event redaction helpers that mask or remove configured columns before events
//! leave the engine.
//!
//! Edits operate directly on raw JSON top-level object slices so untouched columns retain
//! their exact original key order and exact numeric text (without rounding or reformatting).
//!
//! Redaction fails closed: if a rule matches an event whose `before` or `after` payload is
//! not a valid JSON object (for example, an application event whose `data` is a JSON array),
//! [`CompiledRedaction::apply`] and [`apply`] return a [`RedactError`] without mutating the
//! event rather than silently emitting unredacted data.

use std::borrow::Cow;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use thiserror::Error;
use uuid::Uuid;

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

/// Errors returned when validating redaction rules or applying them to an [`Event`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RedactError {
    /// A rule had neither `table` nor `action` set.
    #[error(
        "redaction rule {rule_index} must set either `table` (use \"*\" for all events) or `action`"
    )]
    MissingTarget {
        /// Zero-based index of the invalid rule.
        rule_index: usize,
    },

    /// A rule set both `table` and `action`, which is ambiguous.
    #[error("redaction rule {rule_index} cannot set both `table` and `action`")]
    AmbiguousTarget {
        /// Zero-based index of the invalid rule.
        rule_index: usize,
    },

    /// A rule's `table` or `action` target was empty or malformed.
    #[error("redaction rule {rule_index} has invalid `{field}` target {target:?}: {reason}")]
    InvalidTarget {
        /// Zero-based index of the invalid rule.
        rule_index: usize,
        /// Target field name (`"table"` or `"action"`).
        field: &'static str,
        /// The offending target value.
        target: String,
        /// Reason the target was rejected.
        reason: &'static str,
    },

    /// A rule listed no columns to redact.
    #[error("redaction rule {rule_index} must specify at least one column")]
    EmptyColumns {
        /// Zero-based index of the invalid rule.
        rule_index: usize,
    },

    /// A rule included an empty or whitespace-only column name.
    #[error("redaction rule {rule_index} contains a blank column name")]
    BlankColumn {
        /// Zero-based index of the invalid rule.
        rule_index: usize,
    },

    /// A rule included a dot-delimited nested JSON path, which is not supported.
    #[error(
        "redaction rule {rule_index} column {column:?} uses nested JSON path syntax, which is not supported; only top-level keys are supported"
    )]
    NestedPathUnsupported {
        /// Zero-based index of the invalid rule.
        rule_index: usize,
        /// The offending column specification.
        column: String,
    },

    /// A matching rule encountered a non-object JSON payload in `before` or `after`.
    #[error(
        "cannot redact {field} payload on event {event_id}: expected a JSON object, got {actual_kind}"
    )]
    NonObjectPayload {
        /// The `event_id` of the affected [`Event`].
        event_id: Uuid,
        /// Event field name (`"before"` or `"after"`).
        field: &'static str,
        /// Detected JSON value kind (`"array"`, `"string"`, `"number"`, `"boolean"`, or `"null"`).
        actual_kind: &'static str,
    },

    /// A matching rule encountered a malformed top-level JSON object in `before` or `after`.
    #[error("cannot redact {field} payload on event {event_id}: malformed JSON object ({detail})")]
    MalformedPayload {
        /// The `event_id` of the affected [`Event`].
        event_id: Uuid,
        /// Event field name (`"before"` or `"after"`).
        field: &'static str,
        /// Diagnostic description of the parse failure.
        detail: String,
    },
}

/// Action applied to each matching column during [`apply`] or [`CompiledRedaction::apply`].
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
/// Exactly one of `table` or `action` must be set:
///
/// - When `table` is `Some("*")`, the rule applies to all `db` and `app` events.
/// - When `table` is `Some("invoice")`, the rule matches `db` events with `table_name = "invoice"`.
/// - When `table` is `Some("public.invoice")`, the rule matches `db` events with matching
///   `schema_name` and `table_name`.
/// - When `action` is `Some("invoice.exported")`, the rule matches `app` events with matching `op`.
/// - When `action` is `Some("*")`, the rule matches all `app` events.
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

    /// Optional application event action name (`op`) for `source = "app"` events, or `"*"`
    /// for all `app` events.
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

    fn validate_at(&self, rule_index: usize) -> Result<RuleTarget<'_>, RedactError> {
        let target = match (self.table.as_deref(), self.action.as_deref()) {
            (None, None) => return Err(RedactError::MissingTarget { rule_index }),
            (Some(_), Some(_)) => return Err(RedactError::AmbiguousTarget { rule_index }),
            (Some(table), None) => validate_table_target(rule_index, table)?,
            (None, Some(action)) => validate_action_target(rule_index, action)?,
        };

        if self.columns.is_empty() {
            return Err(RedactError::EmptyColumns { rule_index });
        }

        for col in &self.columns {
            if col.trim().is_empty() {
                return Err(RedactError::BlankColumn { rule_index });
            }
            if col.contains('.') {
                return Err(RedactError::NestedPathUnsupported {
                    rule_index,
                    column: col.clone(),
                });
            }
        }

        Ok(target)
    }
}

enum RuleTarget<'a> {
    Global,
    Table(&'a str),
    SchemaTable(&'a str, &'a str),
    AppAll,
    AppAction(&'a str),
}

fn validate_table_target(rule_index: usize, table: &str) -> Result<RuleTarget<'_>, RedactError> {
    if table.trim().is_empty() {
        return Err(RedactError::InvalidTarget {
            rule_index,
            field: "table",
            target: table.to_owned(),
            reason: "must not be empty or whitespace-only",
        });
    }
    if table != table.trim() {
        return Err(RedactError::InvalidTarget {
            rule_index,
            field: "table",
            target: table.to_owned(),
            reason: "must not have leading or trailing whitespace",
        });
    }
    if table == "*" {
        return Ok(RuleTarget::Global);
    }
    if let Some((schema, tbl)) = table.split_once('.') {
        if schema.is_empty() || tbl.is_empty() || tbl.contains('.') {
            return Err(RedactError::InvalidTarget {
                rule_index,
                field: "table",
                target: table.to_owned(),
                reason: "qualified table must be `schema.table` with non-empty parts",
            });
        }
        if schema == "*" || tbl == "*" {
            return Err(RedactError::InvalidTarget {
                rule_index,
                field: "table",
                target: table.to_owned(),
                reason: "wildcards are only supported as `table = \"*\"`",
            });
        }
        Ok(RuleTarget::SchemaTable(schema, tbl))
    } else {
        Ok(RuleTarget::Table(table))
    }
}

fn validate_action_target(rule_index: usize, action: &str) -> Result<RuleTarget<'_>, RedactError> {
    if action.trim().is_empty() {
        return Err(RedactError::InvalidTarget {
            rule_index,
            field: "action",
            target: action.to_owned(),
            reason: "must not be empty or whitespace-only",
        });
    }
    if action != action.trim() {
        return Err(RedactError::InvalidTarget {
            rule_index,
            field: "action",
            target: action.to_owned(),
            reason: "must not have leading or trailing whitespace",
        });
    }
    if action == "*" {
        Ok(RuleTarget::AppAll)
    } else {
        Ok(RuleTarget::AppAction(action))
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
/// assert!(redaction.validate().is_ok());
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

    /// Validates all configured redaction rules without compiling lookup indexes.
    ///
    /// # Errors
    ///
    /// Returns [`RedactError`] if any rule omits both `table` and `action`, sets both
    /// `table` and `action`, uses an empty or malformed target, specifies no columns or a
    /// blank column name, or uses dot-path nested JSON syntax (such as `"metadata.ssn"`).
    ///
    /// # Examples
    ///
    /// ```
    /// use auditstream_core::redact::{RedactError, Redaction, RedactionMode, RedactionRule};
    ///
    /// let bad = Redaction::new(vec![
    ///     RedactionRule::for_table("invoice", ["metadata.ssn"], RedactionMode::Mask),
    /// ]);
    /// assert!(matches!(bad.validate(), Err(RedactError::NestedPathUnsupported { .. })));
    /// ```
    pub fn validate(&self) -> Result<(), RedactError> {
        for (rule_index, rule) in self.rules.iter().enumerate() {
            rule.validate_at(rule_index)?;
        }
        Ok(())
    }

    /// Validates and compiles this configuration into a [`CompiledRedaction`] for fast,
    /// zero-allocation per-event rule matching across batches.
    ///
    /// # Errors
    ///
    /// Returns [`RedactError`] for the same validation failures as [`Redaction::validate`].
    pub fn compile(&self) -> Result<CompiledRedaction, RedactError> {
        CompiledRedaction::compile(self)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ColumnActions {
    columns: HashMap<String, RedactionMode>,
    has_remove: bool,
}

impl ColumnActions {
    fn insert(&mut self, column: &str, mode: RedactionMode) {
        if mode == RedactionMode::Remove {
            self.has_remove = true;
        }
        match self.columns.get_mut(column) {
            Some(existing) => {
                if mode == RedactionMode::Remove {
                    *existing = RedactionMode::Remove;
                }
            }
            None => {
                self.columns.insert(column.to_owned(), mode);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

#[derive(Clone, Copy)]
struct ActiveActions<'a> {
    sets: [Option<&'a ColumnActions>; 3],
}

impl ActiveActions<'_> {
    fn is_empty(self) -> bool {
        self.sets
            .iter()
            .all(|s| s.is_none_or(ColumnActions::is_empty))
    }

    fn mode_for(self, col: &str) -> Option<RedactionMode> {
        let mut matched = None;
        for set in self.sets.into_iter().flatten() {
            if let Some(&mode) = set.columns.get(col) {
                if mode == RedactionMode::Remove {
                    return Some(RedactionMode::Remove);
                }
                matched = Some(RedactionMode::Mask);
            }
        }
        matched
    }

    fn has_any_remove(self) -> bool {
        self.sets.into_iter().flatten().any(|c| c.has_remove)
    }
}

/// Pre-validated, pre-indexed redaction policy for applying rules across event batches
/// without per-event rule-set allocations.
///
/// Construct once via [`CompiledRedaction::compile`] (or [`Redaction::compile`]) and reuse
/// across every [`Event`] in an export pipeline.
///
/// # Examples
///
/// ```
/// use auditstream_core::{CURRENT_SCHEMA_VERSION, Event, Op, Source};
/// use auditstream_core::redact::{CompiledRedaction, Redaction, RedactionMode, RedactionRule};
/// use serde_json::value::RawValue;
/// use time::OffsetDateTime;
/// use uuid::Uuid;
///
/// let compiled = CompiledRedaction::compile(&Redaction::new(vec![
///     RedactionRule::for_table("invoice", ["secret"], RedactionMode::Mask),
/// ])).unwrap();
///
/// let mut event = Event {
///     event_id: Uuid::nil(),
///     occurred_at: OffsetDateTime::UNIX_EPOCH,
///     source: Source::Db,
///     schema_name: Some("public".to_owned()),
///     table_name: Some("invoice".to_owned()),
///     op: Op::Update,
///     record_id: Some(RawValue::from_string(r#"{"id":1}"#.to_owned()).unwrap()),
///     before: None,
///     after: Some(RawValue::from_string(r#"{"z":1.10,"secret":"new"}"#.to_owned()).unwrap()),
///     changed: Some(vec!["secret".to_owned()]),
///     actor: None,
///     context: None,
///     actor_source: None,
///     db_user: "app_user".to_owned(),
///     schema_version: CURRENT_SCHEMA_VERSION,
/// };
///
/// compiled.apply(&mut event).unwrap();
/// assert_eq!(event.after.as_deref().unwrap().get(), r#"{"z":1.10,"secret":"[redacted]"}"#);
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompiledRedaction {
    global: ColumnActions,
    by_table: HashMap<String, ColumnActions>,
    by_schema_table: HashMap<String, HashMap<String, ColumnActions>>,
    app_all: ColumnActions,
    by_action: HashMap<String, ColumnActions>,
}

impl CompiledRedaction {
    /// Validates and compiles `redaction` into an indexed policy ready for batch application.
    ///
    /// # Errors
    ///
    /// Returns [`RedactError`] if any rule in `redaction` is invalid (see [`Redaction::validate`]).
    pub fn compile(redaction: &Redaction) -> Result<Self, RedactError> {
        let mut compiled = Self::default();

        for (rule_index, rule) in redaction.rules.iter().enumerate() {
            let target = rule.validate_at(rule_index)?;
            let bucket = match target {
                RuleTarget::Global => &mut compiled.global,
                RuleTarget::Table(table) => compiled.by_table.entry(table.to_owned()).or_default(),
                RuleTarget::SchemaTable(schema, table) => compiled
                    .by_schema_table
                    .entry(schema.to_owned())
                    .or_default()
                    .entry(table.to_owned())
                    .or_default(),
                RuleTarget::AppAll => &mut compiled.app_all,
                RuleTarget::AppAction(action) => {
                    compiled.by_action.entry(action.to_owned()).or_default()
                }
            };

            for col in &rule.columns {
                bucket.insert(col, rule.mode);
            }
        }

        Ok(compiled)
    }

    /// Returns `true` when no redaction rules are compiled.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty()
            && self.by_table.is_empty()
            && self.by_schema_table.is_empty()
            && self.app_all.is_empty()
            && self.by_action.is_empty()
    }

    /// Applies all matching compiled rules to `event` in place.
    ///
    /// - Modifies `before` and `after` for both `db` and `app` events while leaving `record_id`
    ///   untouched.
    /// - Untouched keys keep their exact original order and exact number text.
    /// - Masked keys remain in `changed`; removed keys are dropped from `changed`.
    /// - When both a `mask` and a `remove` rule match the same column on an event, `remove` wins.
    /// - Idempotent: applying the same [`CompiledRedaction`] twice produces the same [`Event`].
    /// - Atomic on error: if redacting `before` or `after` fails, `event` is left unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`RedactError::NonObjectPayload`] or [`RedactError::MalformedPayload`] if at
    /// least one rule matches `event` and `event.before` or `event.after` is not a valid JSON
    /// object.
    pub fn apply(&self, event: &mut Event) -> Result<(), RedactError> {
        let actions = self.active_actions(event);
        if actions.is_empty() {
            return Ok(());
        }

        let updated_before = match event.before.as_deref() {
            Some(before) => redact_raw_object(before.get(), actions, event.event_id, "before")?,
            None => None,
        };

        let updated_after = match event.after.as_deref() {
            Some(after) => redact_raw_object(after.get(), actions, event.event_id, "after")?,
            None => None,
        };

        if let Some(before) = updated_before {
            event.before = Some(before);
        }
        if let Some(after) = updated_after {
            event.after = Some(after);
        }

        if actions.has_any_remove()
            && let Some(ref mut changed) = event.changed
        {
            changed.retain(|col| actions.mode_for(col.as_str()) != Some(RedactionMode::Remove));
        }

        Ok(())
    }

    fn active_actions(&self, event: &Event) -> ActiveActions<'_> {
        let global = (!self.global.is_empty()).then_some(&self.global);
        match event.source {
            Source::Db => {
                let by_table = event
                    .table_name
                    .as_deref()
                    .and_then(|tbl| self.by_table.get(tbl));
                let by_schema_table = event
                    .schema_name
                    .as_deref()
                    .zip(event.table_name.as_deref())
                    .and_then(|(schema, tbl)| self.by_schema_table.get(schema)?.get(tbl));
                ActiveActions {
                    sets: [global, by_table, by_schema_table],
                }
            }
            Source::App => {
                let app_all = (!self.app_all.is_empty()).then_some(&self.app_all);
                let by_action = self.by_action.get(event.op.as_str());
                ActiveActions {
                    sets: [global, app_all, by_action],
                }
            }
        }
    }
}

/// Validates `redaction` and applies all matching rules to `event` in place.
///
/// For batch processing (such as export pipelines), compile rules once with
/// [`CompiledRedaction::compile`] and call [`CompiledRedaction::apply`] per event to avoid
/// recompiling rules on each event.
///
/// - Modifies `before` and `after` for both `db` and `app` events while leaving `record_id`
///   untouched.
/// - Untouched keys keep their exact original order and exact number text.
/// - Masked keys remain in `changed`; removed keys are dropped from `changed`.
/// - When both a `mask` and a `remove` rule match the same column on an event, `remove` wins.
/// - Idempotent: applying the same [`Redaction`] twice produces the same [`Event`].
///
/// # Errors
///
/// Returns [`RedactError`] if any rule in `redaction` is invalid (see [`Redaction::validate`])
/// or if a matching rule encounters a non-object or malformed JSON payload in `before` or
/// `after`.
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
/// apply(&mut event, &redaction).unwrap();
///
/// assert_eq!(event.after.as_deref().unwrap().get(), r#"{"z":1.10,"secret":"[redacted]"}"#);
/// assert_eq!(event.changed, Some(vec!["secret".to_owned()]));
/// ```
pub fn apply(event: &mut Event, redaction: &Redaction) -> Result<(), RedactError> {
    CompiledRedaction::compile(redaction)?.apply(event)
}

#[derive(Debug)]
enum PayloadParseError {
    NonObject(&'static str),
    Malformed(String),
}

/// Rewrites top-level entries of a JSON object preserving exact key order and raw value slices
/// for untouched keys.
///
/// Returns `Ok(None)` when `raw` is a valid JSON object and no keys matched any active rule.
/// Fails closed with `Err(RedactError)` if `raw` is not a JSON object or fails parsing.
fn redact_raw_object(
    raw: &str,
    actions: ActiveActions<'_>,
    event_id: Uuid,
    field: &'static str,
) -> Result<Option<Box<RawValue>>, RedactError> {
    let entries = parse_top_level_object_entries(raw).map_err(|err| match err {
        PayloadParseError::NonObject(actual_kind) => RedactError::NonObjectPayload {
            event_id,
            field,
            actual_kind,
        },
        PayloadParseError::Malformed(detail) => RedactError::MalformedPayload {
            event_id,
            field,
            detail,
        },
    })?;

    let mut any_hit = false;
    for entry in &entries {
        if actions.mode_for(entry.key.as_ref()).is_some() {
            any_hit = true;
            break;
        }
    }
    if !any_hit {
        return Ok(None);
    }

    let mut out = String::with_capacity(raw.len());
    out.push('{');
    let mut first = true;
    for entry in entries {
        let mode = actions.mode_for(entry.key.as_ref());
        if mode == Some(RedactionMode::Remove) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(entry.raw_key);
        out.push(':');
        if mode == Some(RedactionMode::Mask) {
            out.push('"');
            out.push_str(MASK_PLACEHOLDER);
            out.push('"');
        } else {
            out.push_str(entry.raw_val);
        }
    }
    out.push('}');

    let raw_value = RawValue::from_string(out).map_err(|err| RedactError::MalformedPayload {
        event_id,
        field,
        detail: err.to_string(),
    })?;
    Ok(Some(raw_value))
}

struct RawEntry<'a> {
    key: Cow<'a, str>,
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
/// exclusive end index right after the closing `b'"'`, plus whether any backslash escape was seen.
fn scan_json_string(bytes: &[u8], start: usize) -> Option<(usize, bool)> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut i = start + 1;
    let mut has_escape = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                has_escape = true;
                if i + 1 >= bytes.len() {
                    return None;
                }
                i += 2;
            }
            b'"' => return Some((i + 1, has_escape)),
            b if b < 0x20 => return None,
            _ => {
                i += 1;
            }
        }
    }
    None
}

fn scan_digits(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    i
}

fn is_valid_json_number(slice: &str) -> bool {
    let bytes = slice.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'-'));
    match bytes.get(i) {
        Some(&b'0') => i += 1,
        Some(b'1'..=b'9') => i = scan_digits(bytes, i + 1),
        _ => return false,
    }
    if bytes.get(i) == Some(&b'.') {
        let frac_end = scan_digits(bytes, i + 1);
        if frac_end == i + 1 {
            return false;
        }
        i = frac_end;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        i += usize::from(matches!(bytes.get(i), Some(b'+' | b'-')));
        let exp_end = scan_digits(bytes, i);
        if exp_end == i {
            return false;
        }
        i = exp_end;
    }
    i == bytes.len()
}

fn scan_json_container(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut stack = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = scan_json_string(bytes, i)?.0,
            b'{' => {
                stack.push(b'}');
                i += 1;
            }
            b'[' => {
                stack.push(b']');
                i += 1;
            }
            b'}' | b']' => {
                if stack.pop() != Some(bytes[i]) {
                    return None;
                }
                i += 1;
                if stack.is_empty() {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn scan_json_literal(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut i = start;
    while i < bytes.len() && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
    {
        i += 1;
    }
    let literal = &raw[start..i];
    if !literal.is_empty()
        && (matches!(literal, "true" | "false" | "null") || is_valid_json_number(literal))
    {
        Some(i)
    } else {
        None
    }
}

/// Scans any single JSON value starting at `start` and returns the exclusive end index.
fn scan_json_value(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    match *bytes.get(start)? {
        b'"' => scan_json_string(bytes, start).map(|(end, _)| end),
        b'{' | b'[' => scan_json_container(bytes, start),
        _ => scan_json_literal(raw, start),
    }
}

fn expect_leading_object_brace(bytes: &[u8], i: usize) -> Result<(), PayloadParseError> {
    match bytes.get(i) {
        Some(&b'{') => Ok(()),
        Some(&b'[') => Err(PayloadParseError::NonObject("array")),
        Some(&b'"') => Err(PayloadParseError::NonObject("string")),
        Some(b'-' | b'0'..=b'9') => Err(PayloadParseError::NonObject("number")),
        Some(b't' | b'f') => Err(PayloadParseError::NonObject("boolean")),
        Some(&b'n') => Err(PayloadParseError::NonObject("null")),
        None => Err(PayloadParseError::Malformed(
            "empty JSON payload".to_owned(),
        )),
        Some(_) => Err(PayloadParseError::Malformed(
            "unexpected leading token".to_owned(),
        )),
    }
}

fn parse_object_entry(raw: &str, start: usize) -> Result<(RawEntry<'_>, usize), PayloadParseError> {
    let bytes = raw.as_bytes();
    let key_start = skip_ws(bytes, start);
    let (key_end, has_escape) = scan_json_string(bytes, key_start).ok_or_else(|| {
        PayloadParseError::Malformed("expected JSON object key string".to_owned())
    })?;
    let raw_key = &raw[key_start..key_end];
    let key = if has_escape {
        let decoded: String = serde_json::from_str(raw_key)
            .map_err(|err| PayloadParseError::Malformed(format!("invalid key escape: {err}")))?;
        Cow::Owned(decoded)
    } else {
        Cow::Borrowed(&raw_key[1..raw_key.len() - 1])
    };

    let colon = skip_ws(bytes, key_end);
    if bytes.get(colon) != Some(&b':') {
        return Err(PayloadParseError::Malformed(
            "expected ':' after object key".to_owned(),
        ));
    }

    let val_start = skip_ws(bytes, colon + 1);
    let val_end = scan_json_value(raw, val_start).ok_or_else(|| {
        PayloadParseError::Malformed("invalid JSON value in object entry".to_owned())
    })?;
    let entry = RawEntry {
        key,
        raw_key,
        raw_val: &raw[val_start..val_end],
    };
    Ok((entry, val_end))
}

fn parse_top_level_object_entries(raw: &str) -> Result<Vec<RawEntry<'_>>, PayloadParseError> {
    let bytes = raw.as_bytes();
    let start = skip_ws(bytes, 0);
    expect_leading_object_brace(bytes, start)?;

    let mut i = skip_ws(bytes, start + 1);
    let mut entries = Vec::new();
    if bytes.get(i) == Some(&b'}') {
        if skip_ws(bytes, i + 1) != bytes.len() {
            return Err(PayloadParseError::Malformed(
                "trailing characters after JSON object".to_owned(),
            ));
        }
        return Ok(entries);
    }

    loop {
        let (entry, val_end) = parse_object_entry(raw, i)?;
        entries.push(entry);

        let next = skip_ws(bytes, val_end);
        match bytes.get(next) {
            Some(&b',') => i = next + 1,
            Some(&b'}') => {
                if skip_ws(bytes, next + 1) != bytes.len() {
                    return Err(PayloadParseError::Malformed(
                        "trailing characters after JSON object".to_owned(),
                    ));
                }
                return Ok(entries);
            }
            _ => {
                return Err(PayloadParseError::Malformed(
                    "expected ',' or '}' after object entry".to_owned(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_rejects_malformed_top_level_objects() {
        let cases = [
            "",
            "   ",
            "{",
            r#"{"a": 1,}"#,
            r#"{"a": 1} trailing"#,
            "{} trailing",
            r#"{"a": tru}"#,
            r#"{"a": [1, 2}"#,
            r#"{"a": [1, 2"#,
            r#"{"a": 1.}"#,
            r#"{"a": 1e}"#,
            r#"{"a": 01}"#,
            r#"{"a": -}"#,
            r#"{"a" 1}"#,
            r#"{"a": }"#,
            r#"{"a": "unterminated\"#,
            "{\"\x01\": 1}",
            "undefined",
        ];

        for raw in cases {
            assert!(
                matches!(
                    parse_top_level_object_entries(raw),
                    Err(PayloadParseError::Malformed(_))
                ),
                "expected Malformed for {raw:?}"
            );
        }
    }

    #[test]
    fn parser_accepts_nested_containers_escapes_and_json_numbers() {
        let raw = r#"{"nested":{"arr":[1,-0.5,1e-5,2E+3,true,false,null,"a\"b}c"]},"empty":{}}"#;
        let entries = parse_top_level_object_entries(raw).expect("valid nested object");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "nested");
        assert_eq!(entries[1].key, "empty");
    }

    #[test]
    fn parser_classifies_non_object_json_values() {
        for (raw, expected) in [
            ("[]", "array"),
            (r#"  [{"ssn":"1"}]"#, "array"),
            (r#""text""#, "string"),
            ("123.45", "number"),
            ("-1", "number"),
            ("true", "boolean"),
            ("false", "boolean"),
            ("null", "null"),
        ] {
            assert!(
                matches!(
                    parse_top_level_object_entries(raw),
                    Err(PayloadParseError::NonObject(kind)) if kind == expected
                ),
                "expected NonObject({expected}) for {raw:?}"
            );
        }
    }
}
