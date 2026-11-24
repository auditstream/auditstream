//! Exported audit event record matching `spec/export.md` and `spec/schemas/event.schema.json`.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{ActorSource, Op, Source};

/// Initial `schema_version` value defined by Auditstream Specification 0.1.0.
pub const CURRENT_SCHEMA_VERSION: u16 = 1;

/// Exported audit event record containing the 15 normative fields in fixed `snake_case` order.
///
/// JSON columns (`record_id`, `before`, `after`, `actor`, `context`) are stored as raw JSON
/// (`Box<RawValue>`) so `auditstream export` passes arbitrary-precision PostgreSQL `numeric`
/// values and stored actor payloads through byte-for-byte without rounding or rewriting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Stable globally unique event identifier for downstream deduplication.
    pub event_id: Uuid,

    /// UTC timestamp when the row was recorded (serialized as RFC 3339 with microseconds).
    #[serde(with = "rfc3339_micros")]
    pub occurred_at: OffsetDateTime,

    /// Event origin (`db` or `app`).
    pub source: Source,

    /// Audited table schema name (`None` for `app` events).
    pub schema_name: Option<String>,

    /// Audited table name (`None` for `app` events).
    pub table_name: Option<String>,

    /// Database DML operation or application event action name.
    pub op: Op,

    /// Primary key JSON object (`None` for tables without a PK, `TRUNCATE`, or untargeted `app` events).
    pub record_id: Option<Box<RawValue>>,

    /// Pre-change row snapshot as raw JSON (`None` for `INSERT`, `TRUNCATE`, and `app` events).
    pub before: Option<Box<RawValue>>,

    /// Post-change row snapshot or `app` event data as raw JSON (`None` for `DELETE` and `TRUNCATE`).
    pub after: Option<Box<RawValue>>,

    /// Column names changed by an `UPDATE` (`None` for other operations).
    pub changed: Option<Vec<String>>,

    /// Resolved actor object as raw JSON (`None` if no actor was resolved or input was invalid).
    pub actor: Option<Box<RawValue>>,

    /// Resolved execution context object as raw JSON (`None` if absent).
    pub context: Option<Box<RawValue>>,

    /// Transport mechanism that resolved the actor (`None` if no actor was supplied).
    pub actor_source: Option<ActorSource>,

    /// PostgreSQL `session_user` that executed the statement.
    pub db_user: String,

    /// History record schema version (starts at `1`).
    pub schema_version: u16,
}

impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        self.event_id == other.event_id
            && self.occurred_at == other.occurred_at
            && self.source == other.source
            && self.schema_name == other.schema_name
            && self.table_name == other.table_name
            && self.op == other.op
            && raw_opt_eq(self.record_id.as_deref(), other.record_id.as_deref())
            && raw_opt_eq(self.before.as_deref(), other.before.as_deref())
            && raw_opt_eq(self.after.as_deref(), other.after.as_deref())
            && self.changed == other.changed
            && raw_opt_eq(self.actor.as_deref(), other.actor.as_deref())
            && raw_opt_eq(self.context.as_deref(), other.context.as_deref())
            && self.actor_source == other.actor_source
            && self.db_user == other.db_user
            && self.schema_version == other.schema_version
    }
}

impl Eq for Event {}

fn raw_opt_eq(a: Option<&RawValue>, b: Option<&RawValue>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(left), Some(right)) => left.get() == right.get(),
        _ => false,
    }
}

mod rfc3339_micros {
    use serde::{Deserialize, Deserializer, Serializer, de};
    use time::format_description::well_known::Rfc3339;
    use time::{OffsetDateTime, UtcOffset};

    pub(super) fn serialize<S>(dt: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let utc = dt.to_offset(UtcOffset::UTC);
        let formatted = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            utc.year(),
            u8::from(utc.month()),
            utc.day(),
            utc.hour(),
            utc.minute(),
            utc.second(),
            utc.microsecond(),
        );
        serializer.serialize_str(&formatted)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        OffsetDateTime::parse(&raw, &Rfc3339)
            .map(|dt| dt.to_offset(UtcOffset::UTC))
            .map_err(de::Error::custom)
    }
}
