//! Monthly partition maintenance for `{schema}.changes`.
//!
//! Keeps monthly partitions (`changes_YYYY_MM`) created ahead of the database's UTC clock
//! so captured history rows never land in `{schema}.changes_default`.
//!
//! Partition creation runs inside a single transaction serialized by a schema-scoped
//! transaction advisory lock (`pg_advisory_xact_lock`), uses `SET LOCAL lock_timeout`
//! (default `'5s'`) so long transactions cannot stall application writes behind an
//! `ACCESS EXCLUSIVE` wait queue, writes explicit UTC `+00` partition bounds immune to
//! session `TimeZone`, and sets every partition owner to `auditstream_owner`.

use std::collections::HashSet;
use std::fmt;

use sqlx::{AssertSqlSafe, FromRow, PgPool};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

use crate::ddl::{DEFAULT_SCHEMA, OWNER_ROLE, quote_ident, quote_literal};

/// Default number of future months covered ahead of the current UTC month.
pub const DEFAULT_MONTHS_AHEAD: u32 = 3;

/// Default PostgreSQL `lock_timeout` applied while creating monthly partitions.
pub const DEFAULT_LOCK_TIMEOUT: &str = "5s";

/// SQLSTATE for `lock_not_available` (including `lock_timeout`).
const SQLSTATE_LOCK_NOT_AVAILABLE: &str = "55P03";

/// SQLSTATE for `check_violation` (raised when `DEFAULT` holds rows overlapping a new partition).
const SQLSTATE_CHECK_VIOLATION: &str = "23514";

/// Calendar year and month in UTC (`month` is `1..=12`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct YearMonth {
    /// Four-digit calendar year.
    pub year: i32,
    /// Calendar month in `1..=12`.
    pub month: u8,
}

impl YearMonth {
    /// Creates a [`YearMonth`] if `month` is in `1..=12`.
    #[must_use]
    pub const fn new(year: i32, month: u8) -> Option<Self> {
        if month >= 1 && month <= 12 {
            Some(Self { year, month })
        } else {
            None
        }
    }

    /// Returns the UTC calendar month containing `dt`.
    #[must_use]
    pub fn from_utc_datetime(dt: OffsetDateTime) -> Self {
        let utc = dt.to_offset(UtcOffset::UTC);
        Self {
            year: utc.year(),
            month: u8::from(utc.month()),
        }
    }

    /// Returns the immediately following calendar month (rolling December to January).
    #[must_use]
    pub const fn next(self) -> Self {
        if self.month == 12 {
            Self {
                year: self.year + 1,
                month: 1,
            }
        } else {
            Self {
                year: self.year,
                month: self.month + 1,
            }
        }
    }

    /// Returns the number of whole months from `self` to `other` (`other - self`).
    #[must_use]
    pub const fn months_until(self, other: Self) -> i32 {
        (other.year - self.year) * 12 + (other.month as i32 - self.month as i32)
    }

    /// Partition table name (`changes_YYYY_MM`).
    #[must_use]
    pub fn partition_name(self) -> String {
        format!("changes_{:04}_{:02}", self.year, self.month)
    }

    /// Parses a monthly partition name (`changes_YYYY_MM`), returning `None` for
    /// `changes_default` or unrelated table names.
    #[must_use]
    pub fn parse_partition_name(name: &str) -> Option<Self> {
        let rest = name.strip_prefix("changes_")?;
        let (y_str, m_str) = rest.split_once('_')?;
        if y_str.len() != 4 || m_str.len() != 2 {
            return None;
        }
        let year: i32 = y_str.parse().ok()?;
        let month: u8 = m_str.parse().ok()?;
        Self::new(year, month)
    }

    /// Returns explicit UTC range bounds `("[YYYY-MM-01 00:00:00+00]", "[next 00:00:00+00]")`
    /// immune to the PostgreSQL session `TimeZone`.
    #[must_use]
    pub fn utc_bounds(self) -> (String, String) {
        let next = self.next();
        (
            format!("{:04}-{:02}-01 00:00:00+00", self.year, self.month),
            format!("{:04}-{:02}-01 00:00:00+00", next.year, next.month),
        )
    }
}

impl fmt::Display for YearMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

/// Computes the ordered target months from `now` (interpreted in UTC) through
/// `months_ahead` calendar months in the future (inclusive).
///
/// For example, `target_months(2026-11-15T12:00:00Z, 3)` returns
/// `[2026-11, 2026-12, 2027-01, 2027-02]`.
#[must_use]
pub fn target_months(now: OffsetDateTime, months_ahead: u32) -> Vec<YearMonth> {
    target_months_from(YearMonth::from_utc_datetime(now), months_ahead)
}

/// Computes the ordered target months from `start` through `months_ahead` months ahead.
#[must_use]
pub fn target_months_from(start: YearMonth, months_ahead: u32) -> Vec<YearMonth> {
    let mut out = Vec::with_capacity(usize::try_from(months_ahead).unwrap_or(0) + 1);
    let mut cur = start;
    out.push(cur);
    for _ in 0..months_ahead {
        cur = cur.next();
        out.push(cur);
    }
    out
}

/// Errors returned by [`ensure_ahead`], [`ensure_ahead_with_options`], and [`coverage`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PartitionError {
    /// Acquiring the parent table lock exceeded `lock_timeout`; safe to retry later.
    #[error("timed out waiting for partition lock after {lock_timeout}; retry later")]
    LockTimeout {
        /// Configured PostgreSQL `lock_timeout` interval (for example `"5s"`).
        lock_timeout: String,
    },

    /// A PostgreSQL connection or query error occurred.
    #[error("partition maintenance failed: {0}")]
    Database(#[from] sqlx::Error),
}

impl PartitionError {
    /// Returns `true` when the error is transient (such as a lock timeout) and safe to retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::LockTimeout { .. })
    }
}

/// Reason a target month was skipped by [`ensure_ahead`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The monthly partition `changes_YYYY_MM` already exists.
    AlreadyExists,
    /// `changes_default` holds rows whose `occurred_at` falls in this month, preventing
    /// PostgreSQL from attaching a partition until those rows are moved out of `DEFAULT`.
    DefaultPartitionConflict {
        /// Exact count of rows in `changes_default` falling within this month's UTC bounds.
        row_count: i64,
    },
}

/// A target month that [`ensure_ahead`] did not create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedMonth {
    /// Target calendar month in UTC.
    pub month: YearMonth,
    /// Why this month was skipped.
    pub reason: SkipReason,
}

/// Result returned by [`ensure_ahead`] and [`ensure_ahead_with_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureReport {
    /// Current UTC calendar month read from the database clock (`now()`).
    pub current_month: YearMonth,
    /// Months whose partitions were newly created during this call.
    pub created: Vec<YearMonth>,
    /// Months that were skipped (either already present or blocked by rows in `DEFAULT`).
    pub skipped: Vec<SkippedMonth>,
}

/// Coverage snapshot returned by [`coverage`] for `auditstream status` and `auditstream verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionCoverage {
    /// Current UTC calendar month read from the database clock (`now()`).
    pub current_month: YearMonth,
    /// Latest calendar month covered by an attached `changes_YYYY_MM` partition, if any.
    pub last_covered_month: Option<YearMonth>,
    /// Whole months covered strictly ahead of `current_month` (negative if behind, `None` if no monthly partitions exist).
    pub months_ahead: Option<i32>,
    /// Sorted list of attached monthly partition names (`changes_YYYY_MM`).
    pub monthly_partitions: Vec<String>,
    /// Exact number of rows stored in `{schema}.changes_default`.
    pub default_row_count: i64,
}

/// Options controlling [`ensure_ahead_with_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureOptions {
    /// Audit history schema name (defaults to [`DEFAULT_SCHEMA`], `"auditstream"`).
    pub schema: String,
    /// Number of calendar months to ensure ahead of the current UTC month (defaults to `3`).
    pub months_ahead: u32,
    /// PostgreSQL `lock_timeout` interval applied locally inside the maintenance transaction
    /// (defaults to `"5s"`).
    pub lock_timeout: String,
}

impl Default for EnsureOptions {
    fn default() -> Self {
        Self {
            schema: DEFAULT_SCHEMA.to_owned(),
            months_ahead: DEFAULT_MONTHS_AHEAD,
            lock_timeout: DEFAULT_LOCK_TIMEOUT.to_owned(),
        }
    }
}

impl EnsureOptions {
    /// Creates [`EnsureOptions`] for `schema` and `months_ahead` using [`DEFAULT_LOCK_TIMEOUT`].
    pub fn new(schema: impl Into<String>, months_ahead: u32) -> Self {
        Self {
            schema: schema.into(),
            months_ahead,
            lock_timeout: DEFAULT_LOCK_TIMEOUT.to_owned(),
        }
    }

    /// Overrides the PostgreSQL `lock_timeout` interval (for example `"500ms"` or `"5s"`).
    #[must_use]
    pub fn with_lock_timeout(mut self, lock_timeout: impl Into<String>) -> Self {
        self.lock_timeout = lock_timeout.into();
        self
    }
}

/// Creates every missing monthly partition (`changes_YYYY_MM`) from the database clock's
/// current UTC month through `months` ahead (inclusive) using [`DEFAULT_LOCK_TIMEOUT`].
///
/// - Uses explicit UTC `+00` partition bounds immune to session `TimeZone`.
/// - Serializes concurrent calls with a transaction advisory lock.
/// - Sets every new partition's owner to `auditstream_owner` (supporting non-superuser
///   migrators via a temporary self-grant revoked before commit).
/// - Skips any month blocked by rows in `changes_default` (reporting the conflicting row
///   count) while still creating all other missing months.
///
/// # Errors
///
/// - [`PartitionError::LockTimeout`] if waiting for the parent lock exceeds `5s` (no partial
///   partition is left behind).
/// - [`PartitionError::Database`] if connecting or running maintenance SQL fails.
pub async fn ensure_ahead(
    pool: &PgPool,
    schema: &str,
    months: u32,
) -> Result<EnsureReport, PartitionError> {
    ensure_ahead_with_options(pool, &EnsureOptions::new(schema, months)).await
}

/// Same as [`ensure_ahead`], allowing a custom `lock_timeout` via [`EnsureOptions`].
///
/// # Errors
///
/// - [`PartitionError::LockTimeout`] if waiting for the parent lock exceeds `options.lock_timeout`.
/// - [`PartitionError::Database`] if connecting or running maintenance SQL fails.
pub async fn ensure_ahead_with_options(
    pool: &PgPool,
    options: &EnsureOptions,
) -> Result<EnsureReport, PartitionError> {
    let mut tx = pool.begin().await?;

    let lock_key = partition_lock_key(&options.schema);
    sqlx::query("SELECT pg_catalog.pg_advisory_xact_lock($1)")
        .bind(lock_key)
        .execute(&mut *tx)
        .await?;

    let current_month = fetch_db_utc_month(&mut tx).await?;
    let existing = fetch_existing_monthly_partitions(&mut tx, &options.schema).await?;
    let existing_set: HashSet<String> = existing.into_iter().collect();

    let lock_timeout_sql = format!(
        "SET LOCAL lock_timeout = {}",
        quote_literal(&options.lock_timeout)
    );
    sqlx::query(AssertSqlSafe(lock_timeout_sql))
        .execute(&mut *tx)
        .await?;

    let granted_self = ensure_owner_membership(&mut tx).await?;

    let schema_q = quote_ident(&options.schema);
    let owner_q = quote_ident(OWNER_ROLE);

    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for ym in target_months_from(current_month, options.months_ahead) {
        let part_name = ym.partition_name();
        if existing_set.contains(&part_name) {
            skipped.push(SkippedMonth {
                month: ym,
                reason: SkipReason::AlreadyExists,
            });
            continue;
        }

        let (start_utc, end_utc) = ym.utc_bounds();
        let part_q = quote_ident(&part_name);
        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {schema_q}.{part_q} \
             PARTITION OF {schema_q}.changes \
             FOR VALUES FROM ({}) TO ({})",
            quote_literal(&start_utc),
            quote_literal(&end_utc),
        );

        sqlx::query("SAVEPOINT auditstream_create_part")
            .execute(&mut *tx)
            .await?;

        match sqlx::query(AssertSqlSafe(create_sql))
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {
                let alter_sql = format!("ALTER TABLE {schema_q}.{part_q} OWNER TO {owner_q}");
                sqlx::query(AssertSqlSafe(alter_sql))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("RELEASE SAVEPOINT auditstream_create_part")
                    .execute(&mut *tx)
                    .await?;
                created.push(ym);
            }
            Err(err) if is_sqlstate(&err, SQLSTATE_CHECK_VIOLATION) => {
                sqlx::query("ROLLBACK TO SAVEPOINT auditstream_create_part")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("RELEASE SAVEPOINT auditstream_create_part")
                    .execute(&mut *tx)
                    .await?;

                let row_count =
                    count_default_rows_in_range(&mut tx, &options.schema, &start_utc, &end_utc)
                        .await?;
                skipped.push(SkippedMonth {
                    month: ym,
                    reason: SkipReason::DefaultPartitionConflict { row_count },
                });
            }
            Err(err) if is_sqlstate(&err, SQLSTATE_LOCK_NOT_AVAILABLE) => {
                tx.rollback().await?;
                return Err(PartitionError::LockTimeout {
                    lock_timeout: options.lock_timeout.clone(),
                });
            }
            Err(err) => {
                tx.rollback().await?;
                return Err(PartitionError::Database(err));
            }
        }
    }

    if granted_self {
        revoke_owner_membership(&mut tx).await?;
    }

    tx.commit().await?;

    Ok(EnsureReport {
        current_month,
        created,
        skipped,
    })
}

/// Inspects attached monthly partitions and `{schema}.changes_default` row count using the
/// database clock in UTC, for `auditstream status` and `auditstream verify`.
///
/// Runs inside a `READ ONLY` transaction.
///
/// # Errors
///
/// Returns [`PartitionError::Database`] if querying PostgreSQL catalogs or `changes_default` fails.
pub async fn coverage(pool: &PgPool, schema: &str) -> Result<PartitionCoverage, PartitionError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;

    let current_month = fetch_db_utc_month(&mut tx).await?;
    let mut monthly_partitions = fetch_existing_monthly_partitions(&mut tx, schema).await?;
    monthly_partitions.sort();

    let last_covered_month = monthly_partitions
        .iter()
        .filter_map(|name| YearMonth::parse_partition_name(name))
        .max();
    let months_ahead = last_covered_month.map(|last| current_month.months_until(last));

    let default_row_count = count_all_default_rows(&mut tx, schema).await?;

    tx.commit().await?;

    Ok(PartitionCoverage {
        current_month,
        last_covered_month,
        months_ahead,
        monthly_partitions,
        default_row_count,
    })
}

#[derive(Debug, FromRow)]
struct UtcMonthRow {
    year: i32,
    month: i32,
}

async fn fetch_db_utc_month(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<YearMonth, sqlx::Error> {
    let row: UtcMonthRow = sqlx::query_as(
        "SELECT \
           EXTRACT(YEAR FROM pg_catalog.now() AT TIME ZONE 'UTC')::int AS year, \
           EXTRACT(MONTH FROM pg_catalog.now() AT TIME ZONE 'UTC')::int AS month",
    )
    .fetch_one(&mut **tx)
    .await?;

    let month = u8::try_from(row.month).expect("PostgreSQL UTC month is always in 1..=12");
    Ok(YearMonth::new(row.year, month).expect("PostgreSQL UTC month is valid"))
}

async fn fetch_existing_monthly_partitions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT child.relname \
           FROM pg_catalog.pg_inherits i \
           JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent \
           JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace \
           JOIN pg_catalog.pg_class child ON child.oid = i.inhrelid \
           JOIN pg_catalog.pg_namespace cn ON cn.oid = child.relnamespace \
          WHERE pn.nspname = $1 \
            AND parent.relname = 'changes' \
            AND cn.nspname = $1 \
            AND child.relname ~ '^changes_[0-9]{4}_[0-9]{2}$' \
          ORDER BY child.relname ASC",
    )
    .bind(schema)
    .fetch_all(&mut **tx)
    .await?;

    Ok(names)
}

/// Cheap catalog-only summary of `{schema}.changes` for `auditstream status` and `verify`.
///
/// Never runs `count(*)` over monthly partitions; uses `pg_class.reltuples`,
/// `pg_total_relation_size()`, and `EXISTS (... LIMIT 1)` on `changes_default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTableStats {
    /// Whether `{schema}.changes` exists in `pg_catalog.pg_class`.
    pub exists: bool,
    /// Current UTC calendar month according to PostgreSQL `now() AT TIME ZONE 'UTC'`.
    pub current_month: Option<YearMonth>,
    /// Existing monthly partitions (`changes_YYYY_MM`) sorted chronologically.
    pub monthly_partitions: Vec<YearMonth>,
    /// Count of contiguous monthly partitions present strictly after `current_month`.
    pub months_covered_ahead: u32,
    /// Whether `{schema}.changes_default` exists and holds at least one row.
    pub default_has_rows: bool,
    /// Catalog row estimate summed across all child partitions (`GREATEST(reltuples, 0)`).
    pub estimated_rows: i64,
    /// Total relation size (heap + indexes + TOAST) in bytes summed across all child partitions.
    pub total_size_bytes: i64,
}

#[derive(Debug, FromRow)]
struct HistoryChildStatRow {
    relname: String,
    est_rows: i64,
    total_bytes: i64,
}

/// Inspects `{schema}.changes` cheaply using PostgreSQL system catalogs (never scanning
/// monthly partitions with `count(*)`).
///
/// # Errors
///
/// Returns [`PartitionError::Database`] if querying PostgreSQL fails.
pub async fn history_stats(
    pool: &PgPool,
    schema: &str,
) -> Result<HistoryTableStats, PartitionError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;

    let parent_oid: Option<i64> = sqlx::query_scalar(
        "SELECT c.oid::bigint \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = $1 \
            AND c.relname = 'changes' \
            AND c.relkind IN ('r', 'p')",
    )
    .bind(schema)
    .fetch_optional(&mut *tx)
    .await?;

    if parent_oid.is_none() {
        tx.commit().await?;
        return Ok(HistoryTableStats {
            exists: false,
            current_month: None,
            monthly_partitions: Vec::new(),
            months_covered_ahead: 0,
            default_has_rows: false,
            estimated_rows: 0,
            total_size_bytes: 0,
        });
    }

    let current_month = fetch_db_utc_month(&mut tx).await?;
    let children: Vec<HistoryChildStatRow> = sqlx::query_as(
        "SELECT \
           child.relname, \
           GREATEST(child.reltuples::bigint, 0) AS est_rows, \
           pg_catalog.pg_total_relation_size(child.oid)::bigint AS total_bytes \
         FROM pg_catalog.pg_inherits i \
         JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent \
         JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace \
         JOIN pg_catalog.pg_class child ON child.oid = i.inhrelid \
        WHERE pn.nspname = $1 \
          AND parent.relname = 'changes' \
        ORDER BY child.relname ASC",
    )
    .bind(schema)
    .fetch_all(&mut *tx)
    .await?;

    let mut monthly_partitions = Vec::new();
    let mut has_default_table = false;
    let mut estimated_rows: i64 = 0;
    let mut total_size_bytes: i64 = 0;

    for child in &children {
        estimated_rows = estimated_rows.saturating_add(child.est_rows);
        total_size_bytes = total_size_bytes.saturating_add(child.total_bytes);
        if child.relname == "changes_default" {
            has_default_table = true;
        } else if let Some(ym) = YearMonth::parse_partition_name(&child.relname) {
            monthly_partitions.push(ym);
        }
    }
    monthly_partitions.sort();
    monthly_partitions.dedup();

    let present_set: HashSet<YearMonth> = monthly_partitions.iter().copied().collect();
    let mut months_covered_ahead: u32 = 0;
    let mut probe = current_month.next();
    while present_set.contains(&probe) {
        months_covered_ahead = months_covered_ahead.saturating_add(1);
        probe = probe.next();
    }

    let default_has_rows = if has_default_table {
        let schema_q = quote_ident(schema);
        let sql = format!("SELECT EXISTS (SELECT 1 FROM ONLY {schema_q}.changes_default LIMIT 1)");
        sqlx::query_scalar(AssertSqlSafe(sql))
            .fetch_one(&mut *tx)
            .await?
    } else {
        false
    };

    tx.commit().await?;

    Ok(HistoryTableStats {
        exists: true,
        current_month: Some(current_month),
        monthly_partitions,
        months_covered_ahead,
        default_has_rows,
        estimated_rows,
        total_size_bytes,
    })
}

async fn count_default_rows_in_range(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema: &str,
    start_utc: &str,
    end_utc: &str,
) -> Result<i64, sqlx::Error> {
    let schema_q = quote_ident(schema);
    let sql = format!(
        "SELECT count(*)::bigint \
           FROM ONLY {schema_q}.changes_default \
          WHERE occurred_at >= $1::timestamptz \
            AND occurred_at < $2::timestamptz"
    );
    sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(start_utc)
        .bind(end_utc)
        .fetch_one(&mut **tx)
        .await
}

async fn count_all_default_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema: &str,
) -> Result<i64, sqlx::Error> {
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT c.oid::bigint \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = $1 \
            AND c.relname = 'changes_default' \
            AND c.relkind = 'r'",
    )
    .bind(schema)
    .fetch_optional(&mut **tx)
    .await?;

    if exists.is_none() {
        return Ok(0);
    }

    let schema_q = quote_ident(schema);
    let sql = format!("SELECT count(*)::bigint FROM ONLY {schema_q}.changes_default");
    sqlx::query_scalar(AssertSqlSafe(sql))
        .fetch_one(&mut **tx)
        .await
}

async fn ensure_owner_membership(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<bool, sqlx::Error> {
    let has_role: bool =
        sqlx::query_scalar("SELECT pg_catalog.pg_has_role(current_user, $1, 'USAGE')")
            .bind(OWNER_ROLE)
            .fetch_one(&mut **tx)
            .await?;

    if has_role {
        return Ok(false);
    }

    let owner_q = quote_ident(OWNER_ROLE);
    let sql = format!(
        "DO $$ BEGIN EXECUTE pg_catalog.format('GRANT {owner_q} TO %I', current_user); END $$"
    );
    sqlx::query(AssertSqlSafe(sql)).execute(&mut **tx).await?;
    Ok(true)
}

async fn revoke_owner_membership(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    let owner_q = quote_ident(OWNER_ROLE);
    let sql = format!(
        "DO $$ BEGIN EXECUTE pg_catalog.format('REVOKE {owner_q} FROM %I', current_user); END $$"
    );
    sqlx::query(AssertSqlSafe(sql)).execute(&mut **tx).await?;
    Ok(())
}

fn is_sqlstate(err: &sqlx::Error, expected: &str) -> bool {
    err.as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == expected)
}

/// Deterministic FNV-1a 64-bit hash salted with `"auditstream:partitions:"` so concurrent
/// partition maintenance calls on the same schema serialize on one advisory lock key.
fn partition_lock_key(schema: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in b"auditstream:partitions:"
        .iter()
        .chain(schema.as_bytes().iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    i64::from_ne_bytes(hash.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

    use super::*;

    fn utc(year: i32, month: Month, day: u8, hour: u8, min: u8, sec: u8) -> OffsetDateTime {
        PrimitiveDateTime::new(
            Date::from_calendar_date(year, month, day).expect("valid date"),
            Time::from_hms(hour, min, sec).expect("valid time"),
        )
        .assume_utc()
    }

    #[test]
    fn target_months_handles_december_to_january_rollover() {
        let nov = utc(2026, Month::November, 15, 10, 0, 0);
        let months = target_months(nov, 3);
        assert_eq!(
            months,
            vec![
                YearMonth::new(2026, 11).unwrap(),
                YearMonth::new(2026, 12).unwrap(),
                YearMonth::new(2027, 1).unwrap(),
                YearMonth::new(2027, 2).unwrap(),
            ]
        );
        assert_eq!(months[1].partition_name(), "changes_2026_12");
        assert_eq!(
            months[1].utc_bounds(),
            (
                "2026-12-01 00:00:00+00".to_owned(),
                "2027-01-01 00:00:00+00".to_owned(),
            )
        );
    }

    #[test]
    fn target_months_handles_last_minute_of_month_and_non_utc_offset() {
        let dec_last_min = utc(2026, Month::December, 31, 23, 59, 59);
        assert_eq!(
            target_months(dec_last_min, 3),
            vec![
                YearMonth::new(2026, 12).unwrap(),
                YearMonth::new(2027, 1).unwrap(),
                YearMonth::new(2027, 2).unwrap(),
                YearMonth::new(2027, 3).unwrap(),
            ]
        );

        // 2026-12-31 22:30:00 -03:00 is 2027-01-01 01:30:00 UTC -> starts in January 2027.
        let sp_offset = UtcOffset::from_hms(-3, 0, 0).expect("valid offset");
        let sp_time = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::December, 31).expect("valid date"),
            Time::from_hms(22, 30, 0).expect("valid time"),
        )
        .assume_offset(sp_offset);
        assert_eq!(
            target_months(sp_time, 2),
            vec![
                YearMonth::new(2027, 1).unwrap(),
                YearMonth::new(2027, 2).unwrap(),
                YearMonth::new(2027, 3).unwrap(),
            ]
        );
    }

    #[test]
    fn parse_partition_name_handles_valid_and_non_monthly_names() {
        assert_eq!(
            YearMonth::parse_partition_name("changes_2026_02"),
            YearMonth::new(2026, 2)
        );
        assert_eq!(YearMonth::parse_partition_name("changes_default"), None);
        assert_eq!(YearMonth::parse_partition_name("changes_2026_13"), None);
        assert_eq!(YearMonth::parse_partition_name("other_2026_01"), None);
    }
}
