//! Commit-safe history table reader and export cursor (`PgEventSource`) matching
//! `spec/history-table.md` and `spec/export.md`.
//!
//! Export reads advance a 64-bit transaction-ID (`xid8`) snapshot window:
//!
//! ```text
//! WHERE ((xact_id > $cursor_xact) OR (xact_id = $cursor_xact AND id > $cursor_id))
//!   AND xact_id < pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())
//! ORDER BY xact_id ASC, id ASC
//! ```
//!
//! Transactions below `pg_snapshot_xmin` have already committed or rolled back, so rows in
//! the window can never appear retroactively when concurrent transactions commit out of
//! insert-`id` order.

use std::fmt;

use auditstream_core::{ActorSource, Batch, Cursor, Event, EventSource, Op, Source, SourceError};
use serde_json::value::RawValue;
use sqlx::{AssertSqlSafe, FromRow, PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::ddl::{DEFAULT_SCHEMA, quote_ident};

#[derive(Debug)]
struct RowDecodeError(String);

impl fmt::Display for RowDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RowDecodeError {}

/// Default row limit returned by [`lookup`].
pub const DEFAULT_LOOKUP_LIMIT: i64 = 50;

/// Maximum row limit accepted by [`lookup`].
pub const MAX_LOOKUP_LIMIT: i64 = 1000;

/// Record-ID filter mode for point-in-time lookups (`auditstream history --record-id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordIdFilter {
    /// Exact JSONB equality (`record_id = $1::jsonb`).
    Exact(String),
    /// Scalar value matching any single-key JSONB `record_id` object whose sole value equals
    /// `$1::jsonb` (preserving JSONB type distinction between e.g. `1` and `"1"`).
    Scalar(String),
    /// JSONB object containment (`record_id @> $1::jsonb`), supporting partial or full composite keys.
    Contains(String),
}

/// Optional filters applied when reading batches from [`PgEventSource`].
///
/// Filtered-out rows inside a drained `xid8` snapshot window still advance the returned
/// [`Cursor`] so callers (such as `auditstream export`) can persist progress past skipped rows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExportFilter {
    /// Inclusive lower bound on `occurred_at` (`occurred_at >= from`).
    pub from: Option<OffsetDateTime>,
    /// Inclusive upper bound on `occurred_at` (`occurred_at <= to`).
    pub to: Option<OffsetDateTime>,
    /// Exclusive upper bound on `occurred_at` (`occurred_at < until`, used by CLI `--until`).
    pub until: Option<OffsetDateTime>,
    /// Filter matching `actor->>'id'` only.
    pub actor_id: Option<String>,
    /// Filter matching `actor->>'id'` OR `actor->'onBehalfOf'->>'id'` (used by CLI `--actor`).
    pub actor_or_on_behalf_of: Option<String>,
    /// Filter matching `source` (`db` or `app`).
    pub source: Option<Source>,
}

/// Filter parameters for point-in-time history lookups via [`lookup`] (newest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    /// Optional audited table name (`table_name`).
    pub table: Option<String>,
    /// Optional audited table schema (`schema_name`).
    pub schema_name: Option<String>,
    /// Optional primary key JSON object (`record_id = $param::jsonb`).
    pub record_id: Option<String>,
    /// Optional flexible `record_id` filter supporting scalar single-key and `@>` containment.
    pub record_id_filter: Option<RecordIdFilter>,
    /// Optional actor identifier (`actor->>'id'` only).
    pub actor_id: Option<String>,
    /// Optional actor identifier matching `actor->>'id'` OR `actor->'onBehalfOf'->>'id'`.
    pub actor_or_on_behalf_of: Option<String>,
    /// Inclusive lower bound on `occurred_at` (`occurred_at >= from`).
    pub from: Option<OffsetDateTime>,
    /// Inclusive upper bound on `occurred_at` (`occurred_at <= to`).
    pub to: Option<OffsetDateTime>,
    /// Exclusive upper bound on `occurred_at` (`occurred_at < until`, used by CLI `--until`).
    pub until: Option<OffsetDateTime>,
    /// Optional source filter (`db` or `app`).
    pub source: Option<Source>,
    /// Maximum number of rows to return (clamped to `1..=1000`, defaulting to `50`).
    pub limit: i64,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            table: None,
            schema_name: None,
            record_id: None,
            record_id_filter: None,
            actor_id: None,
            actor_or_on_behalf_of: None,
            from: None,
            to: None,
            until: None,
            source: None,
            limit: DEFAULT_LOOKUP_LIMIT,
        }
    }
}

/// Commit-safe PostgreSQL implementation of [`EventSource`] backed by an `xid8` snapshot
/// window over `{schema}.changes`.
///
/// Requires only `USAGE` on the audit schema and `SELECT` on `{schema}.changes`.
#[derive(Debug)]
pub struct PgEventSource {
    pool: PgPool,
    audit_schema: String,
    acked: Cursor,
    position: Cursor,
    window_end: Option<u64>,
    filter: ExportFilter,
}

impl PgEventSource {
    /// Creates a new [`PgEventSource`] resuming from `start_cursor` against the configured
    /// audit schema (default [`DEFAULT_SCHEMA`]).
    pub fn new(
        pool: PgPool,
        audit_schema: impl Into<String>,
        start_cursor: Cursor,
        filter: ExportFilter,
    ) -> Self {
        Self {
            pool,
            audit_schema: audit_schema.into(),
            acked: start_cursor,
            position: start_cursor,
            window_end: None,
            filter,
        }
    }

    /// Creates a [`PgEventSource`] targeting the default `"auditstream"` schema with no filters.
    pub fn with_default_schema(pool: PgPool, start_cursor: Cursor) -> Self {
        Self::new(pool, DEFAULT_SCHEMA, start_cursor, ExportFilter::default())
    }

    /// Returns the highest acknowledged cursor recorded by [`EventSource::ack`].
    pub fn acked_cursor(&self) -> Cursor {
        self.acked
    }

    /// Returns the current internal read position (which may be ahead of [`Self::acked_cursor`]
    /// until the caller flushes and calls [`EventSource::ack`]).
    pub fn current_position(&self) -> Cursor {
        self.position
    }

    async fn fetch_snapshot_xmin(&self) -> Result<u64, SourceError> {
        let xmin_str: String = sqlx::query_scalar(
            "SELECT pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())::text",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|err| SourceError::backend("failed to fetch pg_snapshot_xmin", err))?;

        xmin_str.parse::<u64>().map_err(|err| {
            SourceError::decode(format!("invalid pg_snapshot_xmin `{xmin_str}`"), err)
        })
    }

    async fn window_has_any_rows(&self, xmin: u64) -> Result<bool, SourceError> {
        let schema_q = quote_ident(&self.audit_schema);
        let sql = format!(
            "SELECT EXISTS (\
               SELECT 1 FROM {schema_q}.changes \
               WHERE ((xact_id > $1::text::xid8) OR (xact_id = $1::text::xid8 AND id > $2)) \
                 AND xact_id < $3::text::xid8\
             )"
        );
        sqlx::query_scalar(AssertSqlSafe(sql))
            .bind(self.position.xact_id.to_string())
            .bind(self.position.id)
            .bind(xmin.to_string())
            .fetch_one(&self.pool)
            .await
            .map_err(|err| SourceError::backend("failed to probe history window", err))
    }
    fn build_window_query(&self, xmin: u64, limit: i64) -> QueryBuilder<Postgres> {
        let schema_q = quote_ident(&self.audit_schema);
        let mut qb = QueryBuilder::<Postgres>::new(format!(
            "SELECT \
               id, \
               event_id::text AS event_id, \
               pg_catalog.to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS occurred_at, \
               xact_id::text AS xact_id, \
               source, \
               schema_name, \
               table_name, \
               op, \
               record_id::text AS record_id, \
               before::text AS before, \
               after::text AS after, \
               changed, \
               actor::text AS actor, \
               context::text AS context, \
               actor_source, \
               db_user, \
               schema_version \
             FROM {schema_q}.changes \
             WHERE ((xact_id > "
        ));
        qb.push_bind(self.position.xact_id.to_string());
        qb.push("::text::xid8) OR (xact_id = ");
        qb.push_bind(self.position.xact_id.to_string());
        qb.push("::text::xid8 AND id > ");
        qb.push_bind(self.position.id);
        qb.push(")) AND xact_id < ");
        qb.push_bind(xmin.to_string());
        qb.push("::text::xid8");

        if let Some(from) = self.filter.from {
            qb.push(" AND occurred_at >= ");
            qb.push_bind(format_rfc3339_utc(from));
            qb.push("::timestamptz");
        }
        if let Some(to) = self.filter.to {
            qb.push(" AND occurred_at <= ");
            qb.push_bind(format_rfc3339_utc(to));
            qb.push("::timestamptz");
        }
        if let Some(until) = self.filter.until {
            qb.push(" AND occurred_at < ");
            qb.push_bind(format_rfc3339_utc(until));
            qb.push("::timestamptz");
        }
        if let Some(ref actor_id) = self.filter.actor_id {
            qb.push(" AND actor->>'id' = ");
            qb.push_bind(actor_id.clone());
        }
        if let Some(ref actor_any) = self.filter.actor_or_on_behalf_of {
            qb.push(" AND (actor->>'id' = ");
            qb.push_bind(actor_any.clone());
            qb.push(" OR actor->'onBehalfOf'->>'id' = ");
            qb.push_bind(actor_any.clone());
            qb.push(")");
        }
        if let Some(source) = self.filter.source {
            qb.push(" AND source = ");
            qb.push_bind(source_as_str(source));
        }

        qb.push(" ORDER BY xact_id ASC, id ASC LIMIT ");
        qb.push_bind(limit);
        qb
    }
}

impl EventSource for PgEventSource {
    async fn next_batch(&mut self, max: usize) -> Result<Option<Batch>, SourceError> {
        if max == 0 {
            return Ok(None);
        }

        let xmin = match self.window_end {
            Some(end) => end,
            None => self.fetch_snapshot_xmin().await?,
        };

        if self.position.xact_id >= xmin {
            self.window_end = None;
            return Ok(None);
        }

        let limit_i64 = i64::try_from(max).unwrap_or(i64::MAX - 1).saturating_add(1);
        let mut qb = self.build_window_query(xmin, limit_i64);

        let mut rows: Vec<ChangeRow> = qb
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(|err| SourceError::backend("failed to query history batch", err))?;

        if rows.is_empty() {
            // No rows matched the filter. Check whether unfiltered rows existed in this window:
            // if so, advance past the drained window so callers can store the updated cursor.
            let had_filtered_rows = self.window_has_any_rows(xmin).await?;
            self.window_end = None;
            if had_filtered_rows {
                let next_cursor = Cursor::new(xmin, 0);
                self.position = next_cursor;
                return Ok(Some(Batch::new(Vec::new(), Some(next_cursor))));
            }
            return Ok(None);
        }

        let has_more = rows.len() > max;
        if has_more {
            rows.truncate(max);
        }

        let mut last_row_cursor = self.position;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let row_xact = row.xact_id.parse::<u64>().map_err(|err| {
                SourceError::decode(format!("invalid xact_id `{}`", row.xact_id), err)
            })?;
            last_row_cursor = Cursor::new(row_xact, row.id);
            events.push(row.into_event()?);
        }

        let next_cursor = if has_more {
            self.window_end = Some(xmin);
            last_row_cursor
        } else {
            self.window_end = None;
            Cursor::new(xmin, 0)
        };
        self.position = next_cursor;

        Ok(Some(Batch::new(events, Some(next_cursor))))
    }

    fn ack(&mut self, upto: &Cursor) -> impl std::future::Future<Output = Result<(), SourceError>> {
        if *upto > self.acked {
            self.acked = *upto;
        }
        std::future::ready(Ok(()))
    }
}

/// Reads history rows matching `query`, ordered newest first (`occurred_at DESC, id DESC`),
/// for point-in-time lookups (such as `auditstream history`).
///
/// Requires only `USAGE` on `audit_schema` and `SELECT` on `{audit_schema}.changes`.
///
/// # Errors
///
/// Returns [`SourceError::Backend`] on database query failures or [`SourceError::Decode`]
/// if a stored row violates the history contract.
pub async fn lookup(
    pool: &PgPool,
    audit_schema: &str,
    query: &HistoryQuery,
) -> Result<Vec<Event>, SourceError> {
    let schema_q = quote_ident(audit_schema);
    let limit = query.limit.clamp(1, MAX_LOOKUP_LIMIT);

    let mut qb = QueryBuilder::<Postgres>::new(format!(
        "SELECT \
           id, \
           event_id::text AS event_id, \
           pg_catalog.to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS occurred_at, \
           xact_id::text AS xact_id, \
           source, \
           schema_name, \
           table_name, \
           op, \
           record_id::text AS record_id, \
           before::text AS before, \
           after::text AS after, \
           changed, \
           actor::text AS actor, \
           context::text AS context, \
           actor_source, \
           db_user, \
           schema_version \
         FROM {schema_q}.changes \
         WHERE true"
    ));

    if let Some(ref schema_name) = query.schema_name {
        qb.push(" AND schema_name = ");
        qb.push_bind(schema_name.clone());
    }
    if let Some(ref table) = query.table {
        qb.push(" AND table_name = ");
        qb.push_bind(table.clone());
    }
    if let Some(ref record_id) = query.record_id {
        qb.push(" AND record_id = ");
        qb.push_bind(record_id.clone());
        qb.push("::jsonb");
    }
    if let Some(ref filter) = query.record_id_filter {
        match filter {
            RecordIdFilter::Exact(json) => {
                qb.push(" AND record_id = ");
                qb.push_bind(json.clone());
                qb.push("::jsonb");
            }
            RecordIdFilter::Contains(json_obj) => {
                qb.push(" AND record_id @> ");
                qb.push_bind(json_obj.clone());
                qb.push("::jsonb");
            }
            RecordIdFilter::Scalar(json_scalar) => {
                qb.push(
                    " AND pg_catalog.jsonb_typeof(record_id) = 'object' \
                      AND (SELECT count(*) FROM pg_catalog.jsonb_object_keys(record_id)) = 1 \
                      AND EXISTS (SELECT 1 FROM pg_catalog.jsonb_each(record_id) AS e(k, v) WHERE e.v = ",
                );
                qb.push_bind(json_scalar.clone());
                qb.push("::jsonb)");
            }
        }
    }
    if let Some(ref actor_id) = query.actor_id {
        qb.push(" AND actor->>'id' = ");
        qb.push_bind(actor_id.clone());
    }
    if let Some(ref actor_any) = query.actor_or_on_behalf_of {
        qb.push(" AND (actor->>'id' = ");
        qb.push_bind(actor_any.clone());
        qb.push(" OR actor->'onBehalfOf'->>'id' = ");
        qb.push_bind(actor_any.clone());
        qb.push(")");
    }
    if let Some(from) = query.from {
        qb.push(" AND occurred_at >= ");
        qb.push_bind(format_rfc3339_utc(from));
        qb.push("::timestamptz");
    }
    if let Some(to) = query.to {
        qb.push(" AND occurred_at <= ");
        qb.push_bind(format_rfc3339_utc(to));
        qb.push("::timestamptz");
    }
    if let Some(until) = query.until {
        qb.push(" AND occurred_at < ");
        qb.push_bind(format_rfc3339_utc(until));
        qb.push("::timestamptz");
    }
    if let Some(source) = query.source {
        qb.push(" AND source = ");
        qb.push_bind(source_as_str(source));
    }

    qb.push(" ORDER BY occurred_at DESC, id DESC LIMIT ");
    qb.push_bind(limit);

    let rows: Vec<ChangeRow> = qb
        .build_query_as()
        .fetch_all(pool)
        .await
        .map_err(|err| SourceError::backend("failed to query history lookup", err))?;

    rows.into_iter().map(ChangeRow::into_event).collect()
}

#[derive(Debug, FromRow)]
struct ChangeRow {
    id: i64,
    event_id: String,
    occurred_at: String,
    xact_id: String,
    source: String,
    schema_name: Option<String>,
    table_name: Option<String>,
    op: String,
    record_id: Option<String>,
    before: Option<String>,
    after: Option<String>,
    changed: Option<Vec<String>>,
    actor: Option<String>,
    context: Option<String>,
    actor_source: Option<String>,
    db_user: String,
    schema_version: i16,
}

impl ChangeRow {
    fn into_event(self) -> Result<Event, SourceError> {
        let event_id = Uuid::parse_str(&self.event_id).map_err(|err| {
            SourceError::decode(format!("invalid event_id `{}`", self.event_id), err)
        })?;
        let occurred_at = OffsetDateTime::parse(&self.occurred_at, &Rfc3339).map_err(|err| {
            SourceError::decode(format!("invalid occurred_at `{}`", self.occurred_at), err)
        })?;
        let source = match self.source.as_str() {
            "db" => Source::Db,
            "app" => Source::App,
            other => {
                return Err(SourceError::decode(
                    format!("unknown source `{other}`"),
                    RowDecodeError(format!("unknown source `{other}`")),
                ));
            }
        };
        let op = match self.op.as_str() {
            "INSERT" => Op::Insert,
            "UPDATE" => Op::Update,
            "DELETE" => Op::Delete,
            "TRUNCATE" => Op::Truncate,
            other => Op::app(other)
                .map_err(|err| SourceError::decode(format!("invalid op `{other}`"), err))?,
        };
        let actor_source = match self.actor_source.as_deref() {
            None => None,
            Some("set_config") => Some(ActorSource::SetConfig),
            Some("comment") => Some(ActorSource::Comment),
            Some("jwt") => Some(ActorSource::Jwt),
            Some("invalid") => Some(ActorSource::Invalid),
            Some(other) => {
                return Err(SourceError::decode(
                    format!("unknown actor_source `{other}`"),
                    RowDecodeError(format!("unknown actor_source `{other}`")),
                ));
            }
        };
        let schema_version = u16::try_from(self.schema_version).map_err(|err| {
            SourceError::decode(
                format!("invalid schema_version `{}`", self.schema_version),
                err,
            )
        })?;

        Ok(Event {
            event_id,
            occurred_at,
            source,
            schema_name: self.schema_name,
            table_name: self.table_name,
            op,
            record_id: parse_optional_raw_json("record_id", self.record_id)?,
            before: parse_optional_raw_json("before", self.before)?,
            after: parse_optional_raw_json("after", self.after)?,
            changed: self.changed,
            actor: parse_optional_raw_json("actor", self.actor)?,
            context: parse_optional_raw_json("context", self.context)?,
            actor_source,
            db_user: self.db_user,
            schema_version,
        })
    }
}

fn parse_optional_raw_json(
    field: &str,
    raw: Option<String>,
) -> Result<Option<Box<RawValue>>, SourceError> {
    match raw {
        None => Ok(None),
        Some(text) => {
            let compact = compact_pg_jsonb_text(&text);
            RawValue::from_string(compact)
                .map(Some)
                .map_err(|err| SourceError::decode(format!("invalid jsonb in `{field}`"), err))
        }
    }
}

/// Strips insignificant whitespace emitted by PostgreSQL `jsonb::text` outside string literals
/// without parsing numeric values through IEEE-754 floats or reordering keys.
fn compact_pg_jsonb_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if !matches!(ch, ' ' | '\t' | '\n' | '\r') {
            out.push(ch);
        }
    }
    out
}

fn format_rfc3339_utc(dt: OffsetDateTime) -> String {
    let utc = dt.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.microsecond(),
    )
}

fn source_as_str(source: Source) -> &'static str {
    match source {
        Source::Db => "db",
        Source::App => "app",
    }
}
