//! Commit-safe history table reader and export cursor (`PgEventSource`) matching
//! `spec/history-table.md` and `spec/export.md`.
//!
//! Export reads advance a 64-bit transaction-ID (`xid8`) snapshot window:
//!
//! ```text
//! WHERE (xact_id, id) > ($cursor_xact, $cursor_id)
//!   AND xact_id < pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())
//! ORDER BY changes.xact_id ASC, changes.id ASC
//! ```
//!
//! Transactions below `pg_snapshot_xmin` have already committed or rolled back, so rows in
//! the window can never appear retroactively when concurrent transactions commit out of
//! insert-`id` order.

use std::collections::VecDeque;
use std::fmt;

use auditstream_core::{ActorSource, Batch, Cursor, Event, EventSource, Op, Source, SourceError};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor, FromRow, PgPool, Postgres, QueryBuilder};
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

/// Server-side PostgreSQL identity for the connected database and audit history table.
///
/// Resumable readers such as `auditstream export` store this identity in checkpoints so
/// resuming works across equivalent client endpoints (such as `localhost` vs `127.0.0.1`,
/// `PgBouncer`, port-forwards, or verified zero-loss HA failover) while rejecting a different
/// cluster, a different database, a recreated `{audit_schema}.changes` table, or a diverged
/// timeline/PITR history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseIdentity {
    /// Cluster-wide `pg_control_system().system_identifier`, when the role has execute access.
    pub system_identifier: Option<String>,
    /// Current PostgreSQL database name (`current_database()`).
    pub name: String,
    /// PostgreSQL catalog OID of the current database (`pg_database.oid`).
    pub oid: u32,
    /// PostgreSQL catalog OID of `{audit_schema}.changes`, if present.
    pub history_table_oid: Option<u32>,
    /// Active PostgreSQL WAL timeline identifier (`timeline_id`), when readable by the role.
    pub timeline_id: Option<u32>,
}

/// Immutable boundary row observed at or before an export checkpoint cursor.
///
/// Persisted alongside the export [`Cursor`] so resuming across a PostgreSQL `timeline_id`
/// change (or when `timeline_id` is unavailable on restricted managed roles) can distinguish a
/// verified zero-loss HA failover (row still matches `event_id`) from a PITR rollback, async
/// failover data loss, or purged history partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryAnchor {
    /// 64-bit PostgreSQL transaction ID (`xid8`) of the anchored history row.
    pub xact_id: u64,
    /// Monotonic row identifier (`id`) of the anchored history row.
    pub id: i64,
    /// Stable UUID (`event_id`) stored on that history row.
    pub event_id: Uuid,
}

/// Snapshot lag and compliance-stall telemetry for an export stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportLag {
    /// Current cluster-wide `pg_snapshot_xmin(pg_current_snapshot())`.
    pub snapshot_xmin: u64,
    /// Age in seconds of the oldest committed row after `cursor` blocked behind `snapshot_xmin`,
    /// or `None` when no committed rows are currently pinned behind an open transaction.
    pub oldest_unexported_age_secs: Option<f64>,
    /// `true` when at least one committed row after `cursor` is blocked behind `snapshot_xmin`.
    pub stalled: bool,
}

/// Parameters for verifying resumable export continuity against PostgreSQL before reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeVerification {
    /// Persisted export cursor position.
    pub cursor: Cursor,
    /// Persisted boundary row anchor, if recorded by the previous export.
    pub anchor: Option<BoundaryAnchor>,
    /// Persisted `timeline_id` from the saved checkpoint, if known.
    pub saved_timeline_id: Option<u32>,
    /// Current `timeline_id` reported by [`database_identity`], if known.
    pub current_timeline_id: Option<u32>,
}

const CAP_WALFILE: i32 = 1;
const CAP_CHECKPOINT: i32 = 2;
const CAP_WAL_RECEIVER: i32 = 4;

/// Parses the 32-bit PostgreSQL timeline identifier from a 24-hex-digit WAL segment filename
/// (for example `"000000010000000000000001"` -> `Some(1)`), rejecting invalid timeline `0`.
fn parse_timeline_from_walfile(name: &str) -> Option<u32> {
    if name.len() != 24 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(&name[..8], 16)
        .ok()
        .filter(|&tl| tl != 0)
}

/// Converts a PostgreSQL `int4` (`TimeLineID` returned as signed 32-bit integer by
/// `pg_control_checkpoint()` and `pg_stat_wal_receiver`) into its unsigned 32-bit representation,
/// rejecting invalid timeline `0`.
fn pg_int4_to_timeline_id(raw: i32) -> Option<u32> {
    let tl = u32::from_ne_bytes(raw.to_ne_bytes());
    (tl != 0).then_some(tl)
}

fn sqlstate_code(err: &sqlx::Error) -> Option<std::borrow::Cow<'_, str>> {
    err.as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
}

fn is_optional_sqlstate_code(code: &str) -> bool {
    matches!(code, "42501" | "55000" | "0A000" | "42883")
}

fn is_recovery_sqlstate_code(code: &str) -> bool {
    code == "55000"
}

/// Returns `true` only for expected PostgreSQL SQLSTATE errors when optional control/WAL
/// functions are unavailable to the role or unsupported in the current server/proxy mode.
fn is_optional_catalog_sqlstate(err: &sqlx::Error) -> bool {
    sqlstate_code(err)
        .as_deref()
        .is_some_and(is_optional_sqlstate_code)
}

fn is_recovery_in_progress_sqlstate(err: &sqlx::Error) -> bool {
    sqlstate_code(err)
        .as_deref()
        .is_some_and(is_recovery_sqlstate_code)
}

fn handle_optional_catalog_result<T>(
    result: Result<Option<T>, sqlx::Error>,
    context: &'static str,
) -> Result<Option<T>, SourceError> {
    match result {
        Ok(val) => Ok(val),
        Err(err) if is_optional_catalog_sqlstate(&err) => Ok(None),
        Err(err) => Err(SourceError::backend(context, err)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalfileProbeOutcome {
    Timeline(u32),
    Fallback,
    RecoveryInProgress,
}

fn classify_walfile_result(
    res: Result<Option<String>, sqlx::Error>,
) -> Result<WalfileProbeOutcome, SourceError> {
    match res {
        Ok(Some(walfile)) => Ok(parse_timeline_from_walfile(&walfile)
            .map_or(WalfileProbeOutcome::Fallback, WalfileProbeOutcome::Timeline)),
        Ok(None) => Ok(WalfileProbeOutcome::Fallback),
        Err(err) if is_recovery_in_progress_sqlstate(&err) => {
            Ok(WalfileProbeOutcome::RecoveryInProgress)
        }
        Err(err) if is_optional_catalog_sqlstate(&err) => Ok(WalfileProbeOutcome::Fallback),
        Err(err) => Err(SourceError::backend(
            "failed to query current WAL timeline_id",
            err,
        )),
    }
}

async fn query_standby_timeline_id(
    conn: &mut PgConnection,
    caps: i32,
) -> Result<Option<u32>, SourceError> {
    if (caps & CAP_WAL_RECEIVER) == 0 {
        return Ok(None);
    }
    let res = sqlx::query_scalar::<_, i32>(
        "SELECT received_tli FROM pg_catalog.pg_stat_wal_receiver \
         WHERE received_tli IS NOT NULL LIMIT 1",
    )
    .fetch_optional(conn)
    .await;
    Ok(
        handle_optional_catalog_result(res, "failed to query pg_stat_wal_receiver timeline_id")?
            .and_then(pg_int4_to_timeline_id),
    )
}

async fn query_checkpoint_timeline_id(
    conn: &mut PgConnection,
    caps: i32,
) -> Result<Option<u32>, SourceError> {
    if (caps & CAP_CHECKPOINT) == 0 {
        return Ok(None);
    }
    let res =
        sqlx::query_scalar::<_, i32>("SELECT timeline_id FROM pg_catalog.pg_control_checkpoint()")
            .fetch_optional(conn)
            .await;
    Ok(
        handle_optional_catalog_result(res, "failed to query pg_control_checkpoint timeline_id")?
            .and_then(pg_int4_to_timeline_id),
    )
}

async fn resolve_timeline_id(
    conn: &mut PgConnection,
    in_recovery: bool,
    caps: i32,
) -> Result<Option<u32>, SourceError> {
    if in_recovery {
        return query_standby_timeline_id(conn, caps).await;
    }

    if (caps & CAP_WALFILE) != 0 {
        let res = sqlx::query_scalar::<_, String>(
            "SELECT pg_catalog.pg_walfile_name(pg_catalog.pg_current_wal_lsn())",
        )
        .fetch_optional(&mut *conn)
        .await;
        match classify_walfile_result(res)? {
            WalfileProbeOutcome::Timeline(tl) => return Ok(Some(tl)),
            WalfileProbeOutcome::RecoveryInProgress => return Ok(None),
            WalfileProbeOutcome::Fallback => {}
        }
    }

    query_checkpoint_timeline_id(conn, caps).await
}

/// Queries the server-side identity of the connected PostgreSQL database and
/// `{audit_schema}.changes` on a single pooled connection.
///
/// Requires no elevated privileges: `pg_database`, `pg_namespace`, `pg_class`,
/// `pg_is_in_recovery`, and `has_function_privilege` are readable by `PUBLIC`.
/// `pg_control_system()` and `pg_control_checkpoint()` are queried only when the connected role
/// has `EXECUTE` permission on them. When the server is not in recovery, `timeline_id` prefers
/// `pg_walfile_name(pg_current_wal_lsn())` (which reflects fast promotion immediately and is
/// readable without `pg_monitor`), falling back to `pg_control_checkpoint().timeline_id` or `None`.
/// On standbys (`pg_is_in_recovery() = true`), `timeline_id` reads `pg_stat_wal_receiver.received_tli`
/// when readable rather than stale restartpoint metadata in `pg_control_checkpoint()`.
///
/// Note: statements run as autocommit queries on one checked-out pool slot (compatible with
/// `PgBouncer` transaction pooling; avoid targeting a heterogeneous multi-replica load balancer).
///
/// # Errors
///
/// Returns [`SourceError::Backend`] if the catalog query or any non-privilege optional probe fails,
/// or [`SourceError::Decode`] if a returned catalog OID falls outside `u32`.
pub async fn database_identity(
    pool: &PgPool,
    audit_schema: &str,
) -> Result<DatabaseIdentity, SourceError> {
    #[derive(FromRow)]
    struct IdentityRow {
        name: String,
        oid: i64,
        history_table_oid: Option<i64>,
        can_read_system_id: bool,
        in_recovery: bool,
        timeline_caps: i32,
    }

    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| SourceError::backend("failed to acquire database connection", err))?;

    let row: IdentityRow = sqlx::query_as(
        "SELECT \
           d.datname::text AS name, \
           d.oid::bigint AS oid, \
           c.oid::bigint AS history_table_oid, \
           pg_catalog.has_function_privilege('pg_catalog.pg_control_system()', 'EXECUTE') \
             AS can_read_system_id, \
           pg_catalog.pg_is_in_recovery() AS in_recovery, \
           ( \
             (CASE WHEN pg_catalog.has_function_privilege('pg_catalog.pg_current_wal_lsn()', 'EXECUTE') \
                    AND pg_catalog.has_function_privilege('pg_catalog.pg_walfile_name(pg_catalog.pg_lsn)', 'EXECUTE') \
                   THEN 1 ELSE 0 END) \
             + (CASE WHEN pg_catalog.has_function_privilege('pg_catalog.pg_control_checkpoint()', 'EXECUTE') \
                   THEN 2 ELSE 0 END) \
             + (CASE WHEN pg_catalog.has_table_privilege('pg_catalog.pg_stat_wal_receiver', 'SELECT') \
                   THEN 4 ELSE 0 END) \
           )::integer AS timeline_caps \
         FROM pg_catalog.pg_database d \
         LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = $1 \
         LEFT JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = 'changes' \
         WHERE d.datname = pg_catalog.current_database()",
    )
    .bind(audit_schema)
    .fetch_one(&mut *conn)
    .await
    .map_err(|err| SourceError::backend("failed to query database identity", err))?;

    let oid = u32::try_from(row.oid)
        .map_err(|err| SourceError::decode(format!("invalid database oid `{}`", row.oid), err))?;
    let history_table_oid = row
        .history_table_oid
        .map(|raw| {
            u32::try_from(raw).map_err(|err| {
                SourceError::decode(format!("invalid history table oid `{raw}`"), err)
            })
        })
        .transpose()?;

    let system_identifier = if row.can_read_system_id {
        let res = sqlx::query_scalar::<_, String>(
            "SELECT system_identifier::text FROM pg_catalog.pg_control_system()",
        )
        .fetch_optional(&mut *conn)
        .await;
        handle_optional_catalog_result(res, "failed to query pg_control_system system_identifier")?
    } else {
        None
    };

    let timeline_id = resolve_timeline_id(&mut conn, row.in_recovery, row.timeline_caps).await?;

    Ok(DatabaseIdentity {
        system_identifier,
        name: row.name,
        oid,
        history_table_oid,
        timeline_id,
    })
}

fn validate_cursor(cursor: Cursor) -> Result<(), SourceError> {
    if cursor.id < 0 || (cursor.xact_id == 0 && cursor.id != 0) {
        return Err(SourceError::InvalidCursor(format!(
            "invalid saved cursor ({}, {}): id must be non-negative and must be 0 when xact_id is 0",
            cursor.xact_id, cursor.id
        )));
    }
    Ok(())
}

fn validate_anchor(anchor: BoundaryAnchor, cursor: Cursor, xmin: u64) -> Result<(), SourceError> {
    if anchor.xact_id == 0 || anchor.id <= 0 {
        return Err(SourceError::InvalidCursor(format!(
            "invalid boundary row anchor ({}, {}): xact_id and id must be positive",
            anchor.xact_id, anchor.id
        )));
    }
    if anchor.xact_id >= xmin {
        return Err(SourceError::InvalidCursor(format!(
            "boundary row anchor ({}, {}) is at or ahead of PostgreSQL snapshot_xmin ({xmin}); \
             verify audit continuity and re-run with --from-start",
            anchor.xact_id, anchor.id
        )));
    }
    let at_or_before_cursor = if cursor.id == 0 {
        anchor.xact_id < cursor.xact_id
    } else {
        Cursor::new(anchor.xact_id, anchor.id) <= cursor
    };
    if !at_or_before_cursor {
        return Err(SourceError::InvalidCursor(format!(
            "boundary row anchor ({}, {}) is ahead of saved cursor ({}, {}); \
             checkpoint is inconsistent — verify audit continuity and re-run with --from-start",
            anchor.xact_id, anchor.id, cursor.xact_id, cursor.id
        )));
    }
    Ok(())
}

fn validate_timeline_transition(
    check: ResumeVerification,
) -> Result<Option<(u32, u32)>, SourceError> {
    if check.saved_timeline_id == Some(0) || check.current_timeline_id == Some(0) {
        return Err(SourceError::InvalidCursor(
            "invalid PostgreSQL timeline_id `0`: valid timeline identifiers start at 1".to_owned(),
        ));
    }

    match (check.saved_timeline_id, check.current_timeline_id) {
        (Some(old), Some(new)) if new < old => Err(SourceError::InvalidCursor(format!(
            "PostgreSQL timeline_id regressed from {old} to {new} (possible split-brain \
             reconnection to a fenced old primary or stale restore); verify cluster topology \
             and re-run with --from-start if recovery was intentional"
        ))),
        (Some(old), Some(new)) if new > old => Ok(Some((old, new))),
        (Some(old), None) if old > 1 => Err(SourceError::InvalidCursor(format!(
            "saved checkpoint recorded PostgreSQL timeline_id {old}, but current timeline_id \
             could not be determined (cannot rule out split-brain reconnection to an older \
             timeline); grant pg_read_all_stats on standbys, connect to the primary, or verify \
             topology and re-run with --from-start"
        ))),
        (Some(old), None) if check.anchor.is_none() && check.cursor > Cursor::new(0, 0) => {
            Err(SourceError::InvalidCursor(format!(
                "saved checkpoint recorded PostgreSQL timeline_id {old} without a boundary row \
                 anchor and current timeline_id could not be determined; verify audit continuity \
                 and re-run with --from-start"
            )))
        }
        _ => Ok(None),
    }
}

fn cursor_is_ahead_of_xmin(cursor: Cursor, xmin: u64) -> bool {
    cursor.xact_id > xmin || (cursor.xact_id == xmin && cursor.id > 0)
}

fn cursor_ahead_error(cursor: Cursor, xmin: u64, in_recovery: bool) -> SourceError {
    if in_recovery {
        SourceError::InvalidCursor(format!(
            "saved cursor ({}, {}) is ahead of PostgreSQL snapshot_xmin ({xmin}) on a standby in recovery \
             (pg_is_in_recovery() = true); wait for replication to catch up or connect to the primary",
            cursor.xact_id, cursor.id
        ))
    } else {
        SourceError::InvalidCursor(format!(
            "saved cursor ({}, {}) is ahead of PostgreSQL snapshot_xmin ({xmin}); this indicates a failover \
             with data loss (RPO > 0), point-in-time recovery (PITR), database restore, or corrupt cursor — \
             verify audit continuity and re-run with --from-start if recovery was intentional",
            cursor.xact_id, cursor.id
        ))
    }
}

async fn fetch_snapshot_status(conn: &mut PgConnection) -> Result<(u64, bool), SourceError> {
    #[derive(FromRow)]
    struct SnapshotRow {
        xmin: String,
        in_recovery: bool,
    }

    let row: SnapshotRow = sqlx::query_as(
        "SELECT \
           pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())::text AS xmin, \
           pg_catalog.pg_is_in_recovery() AS in_recovery",
    )
    .fetch_one(conn)
    .await
    .map_err(|err| SourceError::backend("failed to fetch pg_snapshot_xmin", err))?;

    let xmin = row.xmin.parse::<u64>().map_err(|err| {
        SourceError::decode(format!("invalid pg_snapshot_xmin `{}`", row.xmin), err)
    })?;

    Ok((xmin, row.in_recovery))
}

async fn verify_boundary_anchor(
    conn: &mut PgConnection,
    audit_schema: &str,
    anchor: BoundaryAnchor,
    timeline_change: Option<(u32, u32)>,
) -> Result<(), SourceError> {
    let schema_q = quote_ident(audit_schema);
    let sql = format!(
        "SELECT event_id::text FROM {schema_q}.changes \
         WHERE xact_id = $1::text::xid8 AND id = $2 \
         LIMIT 1"
    );
    let observed: Option<String> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(anchor.xact_id.to_string())
        .bind(anchor.id)
        .fetch_optional(conn)
        .await
        .map_err(|err| SourceError::backend("failed to verify export boundary anchor", err))?;

    let timeline_note = match timeline_change {
        Some((old, new)) => format!(" after PostgreSQL timeline_id changed from {old} to {new}"),
        None => String::new(),
    };

    let Some(raw_uuid) = observed else {
        return Err(SourceError::InvalidCursor(format!(
            "boundary row anchor ({}, {}, event_id {}) is missing from {audit_schema}.changes{timeline_note} \
             (possible PITR rollback, async failover data loss, or dropped partition; verify audit \
             continuity and re-run with --from-start)",
            anchor.xact_id, anchor.id, anchor.event_id
        )));
    };

    let actual = Uuid::parse_str(&raw_uuid)
        .map_err(|err| SourceError::decode(format!("invalid anchor event_id `{raw_uuid}`"), err))?;
    if actual != anchor.event_id {
        return Err(SourceError::InvalidCursor(format!(
            "boundary row anchor ({}, {}) diverged{timeline_note}: expected event_id {}, \
             found {actual} (confirmed PITR or timeline divergence; verify audit continuity \
             and re-run with --from-start)",
            anchor.xact_id, anchor.id, anchor.event_id
        )));
    }
    Ok(())
}

/// Verifies that no unexpected audit rows appeared in the half-open drained gap
/// `(anchor, (cursor.xact_id, 0))`.
///
/// For any drained-window checkpoint (`cursor.id == 0`), `PgEventSource` records `anchor` as the
/// supremum row below `cursor.xact_id`, and `pg_snapshot_xmin()` guarantees no transaction below
/// `cursor.xact_id` can commit retroactively on a non-diverged history. A row found in this gap
/// therefore proves a PITR rollback, async failover (RPO > 0), or snapshot restore that reused
/// transaction IDs between `anchor.xact_id` and `cursor.xact_id`.
async fn verify_drained_gap_empty(
    conn: &mut PgConnection,
    audit_schema: &str,
    anchor: BoundaryAnchor,
    cursor: Cursor,
    timeline_change: Option<(u32, u32)>,
) -> Result<(), SourceError> {
    let schema_q = quote_ident(audit_schema);
    let sql = format!(
        "SELECT 1 FROM {schema_q}.changes \
         WHERE (xact_id, id) > ($1::text::xid8, $2) \
           AND xact_id < $3::text::xid8 \
         LIMIT 1"
    );
    let found: Option<i32> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(anchor.xact_id.to_string())
        .bind(anchor.id)
        .bind(cursor.xact_id.to_string())
        .fetch_optional(conn)
        .await
        .map_err(|err| SourceError::backend("failed to verify drained export gap", err))?;

    if found.is_some() {
        let timeline_note = match timeline_change {
            Some((old, new)) => {
                format!(" after PostgreSQL timeline_id changed from {old} to {new}")
            }
            None => String::new(),
        };
        return Err(SourceError::InvalidCursor(format!(
            "unexpected audit row(s) appeared between boundary row anchor ({}, {}) and saved \
             cursor ({}, {}){timeline_note} (confirmed PITR, async failover data loss, or snapshot \
             rollback; verify audit continuity and re-run with --from-start)",
            anchor.xact_id, anchor.id, cursor.xact_id, cursor.id
        )));
    }
    Ok(())
}

/// Verifies resumable export continuity on a single pooled PostgreSQL connection before reading.
///
/// 1. Rejects malformed cursors (`id < 0`, or `xact_id == 0 && id != 0`), invalid `timeline_id == 0`,
///    and cursors ahead of `pg_snapshot_xmin()` (`cursor.xact_id > xmin`, or
///    `cursor.xact_id == xmin && cursor.id > 0`), surfacing a standby replication-lag hint when
///    `pg_is_in_recovery()` is `true` and a failover/PITR hint otherwise.
/// 2. Rejects backward `timeline_id` regressions (`current < saved`) and unreadable current
///    timelines after a failover (`saved > 1`) or without a boundary anchor on non-zero cursors
///    to guard against split-brain reconnection to a fenced old primary.
/// 3. When a [`BoundaryAnchor`] is present, validates that it lies at or before `cursor` and
///    below `snapshot_xmin`, verifies via `changes_xact_id_idx` that `(anchor.xact_id, anchor.id)`
///    still exists in `{audit_schema}.changes` with the exact same `event_id`, and for drained-window
///    checkpoints (`cursor.id == 0`) verifies that no unexpected rows appeared in the half-open gap
///    `(anchor, (cursor.xact_id, 0))` — allowing verified zero-loss HA failovers across forward
///    `timeline_id` increments while failing loudly on confirmed PITR divergence, snapshot rollback,
///    async failover data loss, or purged history.
/// 4. Rejects a forward `timeline_id` increment on a non-zero cursor when no [`BoundaryAnchor`] is
///    available.
///
/// # Errors
///
/// Returns [`SourceError::InvalidCursor`] on cursor/timeline/anchor divergence,
/// [`SourceError::Backend`] on database query failures, or [`SourceError::Decode`] on malformed
/// catalog/row values.
pub async fn verify_resume_state(
    pool: &PgPool,
    audit_schema: &str,
    check: ResumeVerification,
) -> Result<(), SourceError> {
    validate_cursor(check.cursor)?;

    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| SourceError::backend("failed to acquire database connection", err))?;

    let (xmin, in_recovery) = fetch_snapshot_status(&mut conn).await?;
    if cursor_is_ahead_of_xmin(check.cursor, xmin) {
        return Err(cursor_ahead_error(check.cursor, xmin, in_recovery));
    }

    let timeline_change = validate_timeline_transition(check)?;

    if let Some(anchor) = check.anchor {
        validate_anchor(anchor, check.cursor, xmin)?;
        verify_boundary_anchor(&mut conn, audit_schema, anchor, timeline_change).await?;
        if check.cursor.id == 0 {
            verify_drained_gap_empty(
                &mut conn,
                audit_schema,
                anchor,
                check.cursor,
                timeline_change,
            )
            .await?;
        }
    } else if let Some((old, new)) = timeline_change
        && check.cursor > Cursor::new(0, 0)
    {
        return Err(SourceError::InvalidCursor(format!(
            "PostgreSQL timeline_id changed from {old} to {new} and saved cursor ({}, {}) has no \
             boundary row anchor to verify continuity; verify audit history and re-run with --from-start",
            check.cursor.xact_id, check.cursor.id
        )));
    }

    Ok(())
}

fn clamp_lag_age_secs(raw: f64) -> f64 {
    if raw.is_finite() { raw.max(0.0) } else { 0.0 }
}

/// Queries live snapshot lag and compliance-stall telemetry after draining up to `cursor`.
///
/// Runs inside a single `REPEATABLE READ READ ONLY` transaction so `pg_snapshot_xmin()` and the
/// bounded tail lookup execute against the same snapshot while passing `snapshot_xmin` as a bound
/// parameter for index-assisted range scans over `changes_xact_id_idx`.
///
/// Requires only `USAGE` on `audit_schema` and `SELECT` on `{audit_schema}.changes`.
///
/// # Errors
///
/// Returns [`SourceError::InvalidCursor`] if `cursor` is malformed, [`SourceError::Backend`] on
/// database query failures, or [`SourceError::Decode`] if `pg_snapshot_xmin()` cannot be parsed.
pub async fn fetch_export_lag(
    pool: &PgPool,
    audit_schema: &str,
    cursor: Cursor,
) -> Result<ExportLag, SourceError> {
    validate_cursor(cursor)?;

    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| SourceError::backend("failed to acquire database connection", err))?;
    let mut tx = conn
        .begin()
        .await
        .map_err(|err| SourceError::backend("failed to begin export lag transaction", err))?;
    (&mut *tx)
        .execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await
        .map_err(|err| SourceError::backend("failed to configure export lag transaction", err))?;

    let raw_xmin: String = sqlx::query_scalar(
        "SELECT pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())::text",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| SourceError::backend("failed to query export lag snapshot_xmin", err))?;

    let snapshot_xmin = raw_xmin.parse::<u64>().map_err(|err| {
        SourceError::decode(format!("invalid pg_snapshot_xmin `{raw_xmin}`"), err)
    })?;

    let schema_q = quote_ident(audit_schema);
    let sql = format!(
        "SELECT pg_catalog.date_part('epoch', pg_catalog.clock_timestamp() - pg_catalog.min(c.occurred_at)) \
         FROM {schema_q}.changes c \
         WHERE c.xact_id >= $1::text::xid8 \
           AND (c.xact_id, c.id) > ($2::text::xid8, $3)"
    );
    let raw_age: Option<f64> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(raw_xmin)
        .bind(cursor.xact_id.to_string())
        .bind(cursor.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| SourceError::backend("failed to query export lag telemetry", err))?;

    tx.commit()
        .await
        .map_err(|err| SourceError::backend("failed to close export lag transaction", err))?;

    let oldest_unexported_age_secs = raw_age.map(clamp_lag_age_secs);
    let stalled = oldest_unexported_age_secs.is_some();

    Ok(ExportLag {
        snapshot_xmin,
        oldest_unexported_age_secs,
        stalled,
    })
}

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
    /// Filter matching `actor->>'tenant'` only.
    pub tenant: Option<String>,
    /// Filter matching `actor->>'tenant'` OR `actor->'onBehalfOf'->>'tenant'` (used by CLI `--tenant`).
    pub tenant_or_on_behalf_of: Option<String>,
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
    /// Optional tenant identifier (`actor->>'tenant'` only).
    pub tenant: Option<String>,
    /// Optional tenant identifier matching `actor->>'tenant'` OR `actor->'onBehalfOf'->>'tenant'`.
    pub tenant_or_on_behalf_of: Option<String>,
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
            tenant: None,
            tenant_or_on_behalf_of: None,
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
/// Keeps one checked-out pooled connection across paginated reads of the same snapshot window
/// (returning it as soon as the window drains, errors, or receives `next_batch(0)`). Avoid
/// pointing exports at a heterogeneous multi-replica load balancer whose replicas have diverging
/// `pg_snapshot_xmin()` horizons.
#[derive(Debug)]
pub struct PgEventSource {
    pool: PgPool,
    audit_schema: String,
    acked: Cursor,
    position: Cursor,
    active_window: Option<(u64, PoolConnection<Postgres>)>,
    pending_anchors: VecDeque<(Cursor, BoundaryAnchor)>,
    acked_anchor: Option<BoundaryAnchor>,
    last_snapshot_xmin: Option<u64>,
    filter: ExportFilter,
}

fn promote_acked_anchors(
    pending: &mut VecDeque<(Cursor, BoundaryAnchor)>,
    upto: Cursor,
) -> Option<BoundaryAnchor> {
    let mut promoted = None;
    while let Some((batch_cursor, _)) = pending.front() {
        if *batch_cursor <= upto {
            if let Some((_, anchor)) = pending.pop_front() {
                promoted = Some(anchor);
            }
        } else {
            break;
        }
    }
    promoted
}

fn validate_ack_cursor(acked: Cursor, position: Cursor, upto: Cursor) -> Result<bool, SourceError> {
    validate_cursor(upto)?;
    if upto > position {
        return Err(SourceError::InvalidCursor(format!(
            "cannot acknowledge cursor ({}, {}) ahead of current reader position ({}, {})",
            upto.xact_id, upto.id, position.xact_id, position.id
        )));
    }
    Ok(upto > acked)
}

fn validate_decoded_row_key(xact_id: u64, id: i64) -> Result<(), SourceError> {
    if xact_id == 0 || id <= 0 {
        return Err(SourceError::decode(
            format!("invalid history row key ({xact_id}, {id}): xact_id and id must be positive"),
            RowDecodeError(format!(
                "history row key ({xact_id}, {id}) must have positive xact_id and id"
            )),
        ));
    }
    Ok(())
}

fn decode_batch_rows(
    rows: Vec<ChangeRow>,
    initial_cursor: Cursor,
) -> Result<(Vec<Event>, Cursor, Option<BoundaryAnchor>), SourceError> {
    let mut last_row_cursor = initial_cursor;
    let mut last_row_anchor: Option<BoundaryAnchor> = None;
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let row_xact = row.xact_id.parse::<u64>().map_err(|err| {
            SourceError::decode(format!("invalid xact_id `{}`", row.xact_id), err)
        })?;
        let row_id = row.id;
        validate_decoded_row_key(row_xact, row_id)?;
        let event = row.into_event()?;
        last_row_cursor = Cursor::new(row_xact, row_id);
        last_row_anchor = Some(BoundaryAnchor {
            xact_id: row_xact,
            id: row_id,
            event_id: event.event_id,
        });
        events.push(event);
    }
    Ok((events, last_row_cursor, last_row_anchor))
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
            active_window: None,
            pending_anchors: VecDeque::new(),
            acked_anchor: None,
            last_snapshot_xmin: None,
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

    /// Returns the latest acknowledged [`BoundaryAnchor`] recorded after [`EventSource::ack`].
    pub fn acked_anchor(&self) -> Option<BoundaryAnchor> {
        self.acked_anchor
    }

    /// Returns the most recent `pg_snapshot_xmin()` observed by [`EventSource::next_batch`].
    pub fn last_snapshot_xmin(&self) -> Option<u64> {
        self.last_snapshot_xmin
    }

    /// Returns the current internal read position (which may be ahead of [`Self::acked_cursor`]
    /// until the caller flushes and calls [`EventSource::ack`]).
    pub fn current_position(&self) -> Cursor {
        self.position
    }

    fn has_active_filter(&self) -> bool {
        self.filter != ExportFilter::default()
    }

    async fn acquire_window(
        &mut self,
    ) -> Result<(PoolConnection<Postgres>, u64, bool), SourceError> {
        if let Some((end, conn)) = self.active_window.take() {
            return Ok((conn, end, false));
        }
        let mut conn =
            self.pool.acquire().await.map_err(|err| {
                SourceError::backend("failed to acquire database connection", err)
            })?;
        let (xmin, in_recovery) = fetch_snapshot_status(&mut conn).await?;
        self.last_snapshot_xmin = Some(xmin);
        Ok((conn, xmin, in_recovery))
    }

    async fn latest_row_in_window_after(
        &self,
        conn: &mut PgConnection,
        after: Cursor,
        xmin: u64,
    ) -> Result<Option<BoundaryAnchor>, SourceError> {
        #[derive(FromRow)]
        struct AnchorRow {
            xact_id: String,
            id: i64,
            event_id: String,
        }

        let sql = latest_row_in_window_sql(&self.audit_schema);
        let row: Option<AnchorRow> = sqlx::query_as(AssertSqlSafe(sql))
            .bind(after.xact_id.to_string())
            .bind(after.id)
            .bind(xmin.to_string())
            .fetch_optional(conn)
            .await
            .map_err(|err| SourceError::backend("failed to probe history window", err))?;

        row.map(|r| {
            let xact_id = r.xact_id.parse::<u64>().map_err(|err| {
                SourceError::decode(format!("invalid xact_id `{}`", r.xact_id), err)
            })?;
            validate_decoded_row_key(xact_id, r.id)?;
            let event_id = Uuid::parse_str(&r.event_id).map_err(|err| {
                SourceError::decode(format!("invalid event_id `{}`", r.event_id), err)
            })?;
            Ok(BoundaryAnchor {
                xact_id,
                id: r.id,
                event_id,
            })
        })
        .transpose()
    }

    fn build_window_query_sql(
        audit_schema: &str,
        position: Cursor,
        filter: &ExportFilter,
        xmin: u64,
        limit: i64,
    ) -> QueryBuilder<Postgres> {
        let schema_q = quote_ident(audit_schema);
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
             WHERE (xact_id, id) > ("
        ));
        qb.push_bind(position.xact_id.to_string());
        qb.push("::text::xid8, ");
        qb.push_bind(position.id);
        qb.push(") AND xact_id < ");
        qb.push_bind(xmin.to_string());
        qb.push("::text::xid8");

        if let Some(from) = filter.from {
            qb.push(" AND occurred_at >= ");
            qb.push_bind(format_rfc3339_utc(from));
            qb.push("::timestamptz");
        }
        if let Some(to) = filter.to {
            qb.push(" AND occurred_at <= ");
            qb.push_bind(format_rfc3339_utc(to));
            qb.push("::timestamptz");
        }
        if let Some(until) = filter.until {
            qb.push(" AND occurred_at < ");
            qb.push_bind(format_rfc3339_utc(until));
            qb.push("::timestamptz");
        }
        push_actor_and_tenant_filters(
            &mut qb,
            filter.actor_id.as_deref(),
            filter.actor_or_on_behalf_of.as_deref(),
            filter.tenant.as_deref(),
            filter.tenant_or_on_behalf_of.as_deref(),
        );
        if let Some(source) = filter.source {
            qb.push(" AND source = ");
            qb.push_bind(source_as_str(source));
        }

        // Table-qualify ORDER BY columns so PostgreSQL orders by native `xid8`/`bigint` columns
        // rather than resolving unqualified `xact_id` to the `xact_id::text AS xact_id` SELECT alias.
        qb.push(" ORDER BY changes.xact_id ASC, changes.id ASC LIMIT ");
        qb.push_bind(limit);
        qb
    }

    async fn resolve_filtered_tail_anchor(
        &self,
        conn: &mut PgConnection,
        after: Cursor,
        xmin: u64,
        fallback: Option<BoundaryAnchor>,
    ) -> Result<Option<BoundaryAnchor>, SourceError> {
        if !self.has_active_filter() {
            return Ok(fallback);
        }
        let tail = self.latest_row_in_window_after(conn, after, xmin).await?;
        Ok(tail.or(fallback))
    }
}

fn latest_row_in_window_sql(audit_schema: &str) -> String {
    let schema_q = quote_ident(audit_schema);
    // Table-qualify ORDER BY columns so PostgreSQL orders by native `xid8`/`bigint` columns
    // rather than resolving unqualified `xact_id` to the `xact_id::text AS xact_id` SELECT alias.
    format!(
        "SELECT xact_id::text AS xact_id, id, event_id::text AS event_id \
         FROM {schema_q}.changes \
         WHERE (xact_id, id) > ($1::text::xid8, $2) \
           AND xact_id < $3::text::xid8 \
         ORDER BY changes.xact_id DESC, changes.id DESC \
         LIMIT 1"
    )
}

impl EventSource for PgEventSource {
    async fn next_batch(&mut self, max: usize) -> Result<Option<Batch>, SourceError> {
        if max == 0 {
            self.active_window = None;
            return Ok(None);
        }
        if let Err(err) = validate_cursor(self.position) {
            self.active_window = None;
            return Err(err);
        }

        let (mut conn, xmin, in_recovery) = self.acquire_window().await?;
        if cursor_is_ahead_of_xmin(self.position, xmin) {
            return Err(cursor_ahead_error(self.position, xmin, in_recovery));
        }
        if self.position.xact_id == xmin {
            return Ok(None);
        }

        let limit_i64 = i64::try_from(max).unwrap_or(i64::MAX - 1).saturating_add(1);
        let mut qb = Self::build_window_query_sql(
            &self.audit_schema,
            self.position,
            &self.filter,
            xmin,
            limit_i64,
        );
        let mut rows: Vec<ChangeRow> = qb
            .build_query_as()
            .fetch_all(&mut *conn)
            .await
            .map_err(|err| SourceError::backend("failed to query history batch", err))?;

        if rows.is_empty() {
            let skipped_anchor = self
                .resolve_filtered_tail_anchor(&mut conn, self.position, xmin, None)
                .await?;
            return Ok(skipped_anchor.map(|anchor| {
                let next_cursor = Cursor::new(xmin, 0);
                self.position = next_cursor;
                self.pending_anchors.push_back((next_cursor, anchor));
                Batch::new(Vec::new(), Some(next_cursor))
            }));
        }

        let has_more = rows.len() > max;
        rows.truncate(max);

        let (events, last_row_cursor, mut last_row_anchor) =
            decode_batch_rows(rows, self.position)?;

        let next_cursor = if has_more {
            self.active_window = Some((xmin, conn));
            last_row_cursor
        } else {
            last_row_anchor = self
                .resolve_filtered_tail_anchor(&mut conn, last_row_cursor, xmin, last_row_anchor)
                .await?;
            Cursor::new(xmin, 0)
        };
        if let Some(anchor) = last_row_anchor {
            self.pending_anchors.push_back((next_cursor, anchor));
        }
        self.position = next_cursor;

        Ok(Some(Batch::new(events, Some(next_cursor))))
    }

    fn ack(&mut self, upto: &Cursor) -> impl std::future::Future<Output = Result<(), SourceError>> {
        let result = validate_ack_cursor(self.acked, self.position, *upto).map(|should_advance| {
            if should_advance {
                self.acked = *upto;
                if let Some(anchor) = promote_acked_anchors(&mut self.pending_anchors, *upto) {
                    self.acked_anchor = Some(anchor);
                }
            }
        });
        std::future::ready(result)
    }
}

/// Reads history rows matching `query`, ordered newest first (`changes.occurred_at DESC, changes.id DESC`),
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
    let mut qb = build_lookup_query_sql(audit_schema, query);

    let rows: Vec<ChangeRow> = qb
        .build_query_as()
        .fetch_all(pool)
        .await
        .map_err(|err| SourceError::backend("failed to query history lookup", err))?;

    rows.into_iter().map(ChangeRow::into_event).collect()
}

fn build_lookup_query_sql(audit_schema: &str, query: &HistoryQuery) -> QueryBuilder<Postgres> {
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
    push_actor_and_tenant_filters(
        &mut qb,
        query.actor_id.as_deref(),
        query.actor_or_on_behalf_of.as_deref(),
        query.tenant.as_deref(),
        query.tenant_or_on_behalf_of.as_deref(),
    );
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

    // Table-qualify ORDER BY columns so PostgreSQL orders by native `timestamptz`/`bigint`
    // columns rather than resolving unqualified `occurred_at` to the `to_char(...) AS occurred_at`
    // SELECT alias.
    qb.push(" ORDER BY changes.occurred_at DESC, changes.id DESC LIMIT ");
    qb.push_bind(limit);
    qb
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
        if self.schema_version < 1 {
            return Err(SourceError::decode(
                format!("invalid schema_version `{}`", self.schema_version),
                RowDecodeError(format!(
                    "schema_version must be >= 1, got {}",
                    self.schema_version
                )),
            ));
        }
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

fn push_actor_and_tenant_filters(
    qb: &mut QueryBuilder<Postgres>,
    actor_id: Option<&str>,
    actor_or_on_behalf_of: Option<&str>,
    tenant: Option<&str>,
    tenant_or_on_behalf_of: Option<&str>,
) {
    if let Some(id) = actor_id {
        qb.push(" AND actor->>'id' = ");
        qb.push_bind(id.to_owned());
    }
    if let Some(id) = actor_or_on_behalf_of {
        qb.push(" AND (actor->>'id' = ");
        qb.push_bind(id.to_owned());
        qb.push(" OR actor->'onBehalfOf'->>'id' = ");
        qb.push_bind(id.to_owned());
        qb.push(")");
    }
    if let Some(id) = tenant {
        qb.push(" AND actor->>'tenant' = ");
        qb.push_bind(id.to_owned());
    }
    if let Some(id) = tenant_or_on_behalf_of {
        qb.push(" AND (actor->>'tenant' = ");
        qb.push_bind(id.to_owned());
        qb.push(" OR actor->'onBehalfOf'->>'tenant' = ");
        qb.push_bind(id.to_owned());
        qb.push(")");
    }
}

fn source_as_str(source: Source) -> &'static str {
    match source {
        Source::Db => "db",
        Source::App => "app",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(schema_version: i16) -> ChangeRow {
        ChangeRow {
            id: 1,
            event_id: "019ab123-4567-7000-8000-000000000001".to_owned(),
            occurred_at: "2026-11-24T10:15:30.123456Z".to_owned(),
            xact_id: "100".to_owned(),
            source: "app".to_owned(),
            schema_name: None,
            table_name: None,
            op: "invoice.exported".to_owned(),
            record_id: None,
            before: None,
            after: None,
            changed: None,
            actor: None,
            context: None,
            actor_source: None,
            db_user: "app_user".to_owned(),
            schema_version,
        }
    }

    #[test]
    fn into_event_rejects_schema_version_below_one() {
        for invalid in [0_i16, -1_i16] {
            let err = sample_row(invalid)
                .into_event()
                .expect_err("schema_version < 1 must fail");
            assert!(
                err.to_string().contains("schema_version"),
                "unexpected error for schema_version={invalid}: {err}"
            );
        }

        let valid = sample_row(1)
            .into_event()
            .expect("schema_version = 1 must succeed");
        assert_eq!(valid.schema_version, 1);
    }

    #[test]
    fn parse_timeline_from_walfile_extracts_valid_timelines_and_rejects_malformed() {
        assert_eq!(
            parse_timeline_from_walfile("000000010000000000000001"),
            Some(1)
        );
        assert_eq!(
            parse_timeline_from_walfile("0000002A00000001000000FF"),
            Some(0x2A)
        );
        assert_eq!(
            parse_timeline_from_walfile("ffffffff0000000000000000"),
            Some(u32::MAX)
        );
        for bad in [
            "",
            "000000000000000000000001",
            "00000001",
            "00000001000000000000000",
            "0000000100000000000000010",
            "0000000G0000000000000001",
            "00000001.history",
        ] {
            assert_eq!(parse_timeline_from_walfile(bad), None, "{bad:?}");
        }

        assert_eq!(pg_int4_to_timeline_id(0), None);
        assert_eq!(pg_int4_to_timeline_id(1), Some(1));
        assert_eq!(pg_int4_to_timeline_id(i32::MAX), Some(i32::MAX as u32));
        assert_eq!(pg_int4_to_timeline_id(i32::MIN), Some(0x8000_0000));
        assert_eq!(pg_int4_to_timeline_id(-1), Some(u32::MAX));
    }

    #[test]
    fn validate_cursor_and_anchor_enforce_strict_invariants() {
        validate_cursor(Cursor::new(0, 0)).expect("initial cursor valid");
        validate_cursor(Cursor::new(10, 0)).expect("window boundary cursor valid");
        validate_cursor(Cursor::new(10, 5)).expect("mid-window cursor valid");

        for bad in [Cursor::new(0, 1), Cursor::new(0, -1), Cursor::new(10, -1)] {
            let err = validate_cursor(bad).expect_err("malformed cursor must fail");
            assert!(matches!(err, SourceError::InvalidCursor(_)), "{err:?}");
        }

        let good_anchor = BoundaryAnchor {
            xact_id: 9,
            id: 4,
            event_id: Uuid::nil(),
        };
        validate_anchor(good_anchor, Cursor::new(10, 0), 10)
            .expect("anchor strictly below drained window cursor valid");
        validate_anchor(good_anchor, Cursor::new(9, 4), 10)
            .expect("anchor equal to mid-window cursor valid");
        validate_anchor(good_anchor, Cursor::new(9, 5), 10)
            .expect("anchor before mid-window cursor valid");

        for (anchor, cursor, xmin) in [
            (
                BoundaryAnchor {
                    xact_id: 0,
                    id: 1,
                    event_id: Uuid::nil(),
                },
                Cursor::new(10, 0),
                10,
            ),
            (
                BoundaryAnchor {
                    xact_id: 5,
                    id: 0,
                    event_id: Uuid::nil(),
                },
                Cursor::new(10, 0),
                10,
            ),
            (
                BoundaryAnchor {
                    xact_id: 5,
                    id: -3,
                    event_id: Uuid::nil(),
                },
                Cursor::new(10, 0),
                10,
            ),
            (good_anchor, Cursor::new(0, 0), 10),
            (good_anchor, Cursor::new(9, 0), 10),
            (good_anchor, Cursor::new(9, 3), 10),
            (good_anchor, Cursor::new(10, 0), 9),
        ] {
            let err = validate_anchor(anchor, cursor, xmin)
                .expect_err("invalid or out-of-bounds anchor must fail");
            assert!(matches!(err, SourceError::InvalidCursor(_)), "{err:?}");
        }
    }

    #[test]
    fn promote_acked_anchors_advances_only_up_to_acknowledged_cursor() {
        let a1 = BoundaryAnchor {
            xact_id: 10,
            id: 1,
            event_id: Uuid::from_u128(1),
        };
        let a2 = BoundaryAnchor {
            xact_id: 12,
            id: 2,
            event_id: Uuid::from_u128(2),
        };
        let mut pending = VecDeque::from([(Cursor::new(10, 1), a1), (Cursor::new(15, 0), a2)]);

        assert_eq!(
            promote_acked_anchors(&mut pending, Cursor::new(9, 99)),
            None
        );
        assert_eq!(pending.len(), 2);

        assert_eq!(
            promote_acked_anchors(&mut pending, Cursor::new(10, 1)),
            Some(a1)
        );
        assert_eq!(pending.len(), 1);

        assert_eq!(
            promote_acked_anchors(&mut pending, Cursor::new(14, 99)),
            None
        );
        assert_eq!(pending.len(), 1);

        assert_eq!(
            promote_acked_anchors(&mut pending, Cursor::new(15, 0)),
            Some(a2)
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn cursor_ahead_of_xmin_distinguishes_caught_up_from_future_cursor() {
        assert!(!cursor_is_ahead_of_xmin(Cursor::new(0, 0), 3));
        assert!(!cursor_is_ahead_of_xmin(Cursor::new(499, 99), 500));
        assert!(!cursor_is_ahead_of_xmin(Cursor::new(500, 0), 500));
        assert!(cursor_is_ahead_of_xmin(Cursor::new(500, 1), 500));
        assert!(cursor_is_ahead_of_xmin(Cursor::new(501, 0), 500));

        let standby_err = cursor_ahead_error(Cursor::new(501, 0), 500, true);
        assert!(matches!(standby_err, SourceError::InvalidCursor(_)));
        assert!(standby_err.to_string().contains("pg_is_in_recovery()"));
        assert!(standby_err.to_string().contains("501"));
        assert!(standby_err.to_string().contains("500"));

        let primary_err = cursor_ahead_error(Cursor::new(501, 0), 500, false);
        assert!(matches!(primary_err, SourceError::InvalidCursor(_)));
        assert!(primary_err.to_string().contains("PITR"));
        assert!(primary_err.to_string().contains("--from-start"));
    }

    #[test]
    fn clamp_lag_age_secs_handles_negative_and_non_finite_values() {
        assert!((clamp_lag_age_secs(12.5) - 12.5).abs() < f64::EPSILON);
        assert!(clamp_lag_age_secs(0.0).abs() < f64::EPSILON);
        assert!(clamp_lag_age_secs(-3.2).abs() < f64::EPSILON);
        assert!(clamp_lag_age_secs(f64::NAN).abs() < f64::EPSILON);
        assert!(clamp_lag_age_secs(f64::INFINITY).abs() < f64::EPSILON);
        assert!(clamp_lag_age_secs(f64::NEG_INFINITY).abs() < f64::EPSILON);
    }

    #[test]
    fn optional_catalog_sqlstate_and_error_propagation_rules() {
        for code in ["42501", "55000", "0A000", "42883"] {
            assert!(is_optional_sqlstate_code(code), "{code}");
        }
        for code in ["57P01", "57014", "08006", "42601", "28P01"] {
            assert!(!is_optional_sqlstate_code(code), "{code}");
        }
        assert!(is_recovery_sqlstate_code("55000"));
        assert!(!is_recovery_sqlstate_code("42501"));

        let ok = handle_optional_catalog_result::<i32>(Ok(Some(7)), "ctx").expect("ok");
        assert_eq!(ok, Some(7));

        let io_err = sqlx::Error::Io(std::io::Error::other("connection reset"));
        assert!(!is_optional_catalog_sqlstate(&io_err));
        assert!(!is_recovery_in_progress_sqlstate(&io_err));
        let propagated =
            handle_optional_catalog_result::<i32>(Err(io_err), "probe failed").expect_err("err");
        assert!(matches!(propagated, SourceError::Backend { .. }));

        assert_eq!(
            classify_walfile_result(Ok(Some("000000020000000000000001".to_owned())))
                .expect("valid walfile"),
            WalfileProbeOutcome::Timeline(2)
        );
        assert_eq!(
            classify_walfile_result(Ok(Some("000000000000000000000001".to_owned())))
                .expect("zero timeline falls back"),
            WalfileProbeOutcome::Fallback
        );
        assert_eq!(
            classify_walfile_result(Ok(None)).expect("empty falls back"),
            WalfileProbeOutcome::Fallback
        );
        let wal_io_err = sqlx::Error::Io(std::io::Error::other("network drop"));
        assert!(matches!(
            classify_walfile_result(Err(wal_io_err)).expect_err("real error propagates"),
            SourceError::Backend { .. }
        ));
    }

    #[test]
    fn validate_timeline_transition_enforces_forward_only_and_split_brain_guards() {
        let anchor = Some(BoundaryAnchor {
            xact_id: 9,
            id: 1,
            event_id: Uuid::nil(),
        });
        let cursor = Cursor::new(10, 0);

        // Same timeline or legacy upgrade from None -> Ok(None).
        assert_eq!(
            validate_timeline_transition(ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: Some(1),
                current_timeline_id: Some(1),
            })
            .expect("same timeline"),
            None
        );
        assert_eq!(
            validate_timeline_transition(ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: None,
                current_timeline_id: Some(1),
            })
            .expect("legacy upgrade"),
            None
        );

        // Forward promotion -> Ok(Some((old, new))).
        assert_eq!(
            validate_timeline_transition(ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: Some(1),
                current_timeline_id: Some(2),
            })
            .expect("forward promotion"),
            Some((1, 2))
        );

        // Saved timeline 1 with None current timeline allowed when anchor exists or cursor is zero.
        assert_eq!(
            validate_timeline_transition(ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: Some(1),
                current_timeline_id: None,
            })
            .expect("timeline 1 with anchor and None current"),
            None
        );
        assert_eq!(
            validate_timeline_transition(ResumeVerification {
                cursor: Cursor::new(0, 0),
                anchor: None,
                saved_timeline_id: Some(1),
                current_timeline_id: None,
            })
            .expect("timeline 1 with zero cursor and None current"),
            None
        );

        // Rejected cases: timeline 0, backward regression, post-failover None (even at zero cursor),
        // and unanchored None on non-zero cursor.
        for bad in [
            ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: Some(0),
                current_timeline_id: Some(1),
            },
            ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: Some(1),
                current_timeline_id: Some(0),
            },
            ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: Some(2),
                current_timeline_id: Some(1),
            },
            ResumeVerification {
                cursor,
                anchor,
                saved_timeline_id: Some(2),
                current_timeline_id: None,
            },
            ResumeVerification {
                cursor: Cursor::new(0, 0),
                anchor: None,
                saved_timeline_id: Some(2),
                current_timeline_id: None,
            },
            ResumeVerification {
                cursor,
                anchor: None,
                saved_timeline_id: Some(1),
                current_timeline_id: None,
            },
        ] {
            let err = validate_timeline_transition(bad).expect_err("must reject");
            assert!(matches!(err, SourceError::InvalidCursor(_)), "{err:?}");
        }
    }

    #[test]
    fn validate_ack_cursor_and_decode_batch_rows_enforce_bounds() {
        assert!(
            !validate_ack_cursor(Cursor::new(10, 0), Cursor::new(20, 0), Cursor::new(10, 0))
                .expect("idempotent ack")
        );
        assert!(
            validate_ack_cursor(Cursor::new(10, 0), Cursor::new(20, 0), Cursor::new(15, 0))
                .expect("valid advancing ack")
        );
        for bad in [
            Cursor::new(0, 1),
            Cursor::new(15, -1),
            Cursor::new(20, 1),
            Cursor::new(21, 0),
        ] {
            let err = validate_ack_cursor(Cursor::new(10, 0), Cursor::new(20, 0), bad)
                .expect_err("invalid or ahead ack must fail");
            assert!(matches!(err, SourceError::InvalidCursor(_)), "{err:?}");
        }

        let (events, last_cursor, anchor) =
            decode_batch_rows(vec![sample_row(1)], Cursor::new(0, 0)).expect("decode valid row");
        assert_eq!(events.len(), 1);
        assert_eq!(last_cursor, Cursor::new(100, 1));
        assert_eq!(anchor.expect("anchor").event_id, events[0].event_id);

        let mut bad_row = sample_row(1);
        bad_row.xact_id = "not_a_number".to_owned();
        let err = decode_batch_rows(vec![bad_row], Cursor::new(0, 0))
            .expect_err("non-numeric xact_id must fail");
        assert!(matches!(err, SourceError::Decode { .. }));

        for (xact, id) in [("0", 1), ("100", 0), ("100", -5)] {
            let mut corrupt = sample_row(1);
            corrupt.xact_id = xact.to_owned();
            corrupt.id = id;
            let err = decode_batch_rows(vec![corrupt], Cursor::new(0, 0))
                .expect_err("zero/negative history key must fail decode");
            assert!(matches!(err, SourceError::Decode { .. }), "{err:?}");
        }
    }

    #[test]
    fn reader_queries_qualify_order_by_columns_to_avoid_text_alias_sort() {
        let latest_sql = latest_row_in_window_sql(DEFAULT_SCHEMA);
        assert!(
            latest_sql.contains(" ORDER BY changes.xact_id DESC, changes.id DESC LIMIT 1"),
            "latest-row anchor query must qualify ORDER BY columns: {latest_sql}"
        );

        let window_qb = PgEventSource::build_window_query_sql(
            DEFAULT_SCHEMA,
            Cursor::new(0, 0),
            &ExportFilter::default(),
            1000,
            10,
        );
        let window_raw = window_qb.sql();
        let window_sql = window_raw.as_str();
        assert!(
            window_sql.contains(" ORDER BY changes.xact_id ASC, changes.id ASC LIMIT "),
            "window query must qualify ORDER BY columns: {window_sql}"
        );

        let lookup_qb = build_lookup_query_sql(DEFAULT_SCHEMA, &HistoryQuery::default());
        let lookup_raw = lookup_qb.sql();
        let lookup_sql = lookup_raw.as_str();
        assert!(
            lookup_sql.contains(" ORDER BY changes.occurred_at DESC, changes.id DESC LIMIT "),
            "lookup query must qualify ORDER BY columns: {lookup_sql}"
        );
    }
}
