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
    let targets = target_months_from(current_month, options.months_ahead);

    if targets
        .iter()
        .all(|ym| existing_set.contains(&ym.partition_name()))
    {
        tx.commit().await?;
        let skipped = targets
            .into_iter()
            .map(|month| SkippedMonth {
                month,
                reason: SkipReason::AlreadyExists,
            })
            .collect();
        return Ok(EnsureReport {
            current_month,
            created: Vec::new(),
            skipped,
        });
    }

    let lock_timeout_sql = format!(
        "SET LOCAL lock_timeout = {}",
        quote_literal(&options.lock_timeout)
    );
    sqlx::query(AssertSqlSafe(lock_timeout_sql))
        .execute(&mut *tx)
        .await?;

    let grant_mode = ensure_owner_membership(&mut tx).await?;
    let saved_role = enter_owner_role(&mut tx).await?;

    let schema_q = quote_ident(&options.schema);

    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for ym in targets {
        let part_name = ym.partition_name();
        if existing_set.contains(&part_name) {
            skipped.push(SkippedMonth {
                month: ym,
                reason: SkipReason::AlreadyExists,
            });
            continue;
        }

        match create_partition_with_savepoint(&mut tx, &options.schema, &schema_q, ym, &part_name)
            .await
        {
            Ok(Some(row_count)) => skipped.push(SkippedMonth {
                month: ym,
                reason: SkipReason::DefaultPartitionConflict { row_count },
            }),
            Ok(None) => created.push(ym),
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

    restore_role(&mut tx, &saved_role).await?;
    revoke_owner_membership(&mut tx, grant_mode).await?;

    tx.commit().await?;

    Ok(EnsureReport {
        current_month,
        created,
        skipped,
    })
}

async fn create_partition_with_savepoint(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema: &str,
    schema_q: &str,
    ym: YearMonth,
    part_name: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let (start_utc, end_utc) = ym.utc_bounds();
    let part_q = quote_ident(part_name);
    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {schema_q}.{part_q} \
         PARTITION OF {schema_q}.changes \
         FOR VALUES FROM ({}) TO ({})",
        quote_literal(&start_utc),
        quote_literal(&end_utc),
    );

    sqlx::query("SAVEPOINT auditstream_create_part")
        .execute(&mut **tx)
        .await?;

    match sqlx::query(AssertSqlSafe(create_sql))
        .execute(&mut **tx)
        .await
    {
        Ok(_) => {
            sanitize_child_partition_acl(tx, schema, part_name).await?;
            sqlx::query("RELEASE SAVEPOINT auditstream_create_part")
                .execute(&mut **tx)
                .await?;
            Ok(None)
        }
        Err(err) if is_sqlstate(&err, SQLSTATE_CHECK_VIOLATION) => {
            sqlx::query("ROLLBACK TO SAVEPOINT auditstream_create_part")
                .execute(&mut **tx)
                .await?;
            sqlx::query("RELEASE SAVEPOINT auditstream_create_part")
                .execute(&mut **tx)
                .await?;
            let row_count = count_default_rows_in_range(tx, schema, &start_utc, &end_utc).await?;
            Ok(Some(row_count))
        }
        Err(err) => Err(err),
    }
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
    /// Whether an attached `DEFAULT` partition of `{schema}.changes` holds at least one row
    /// (`false` when no `DEFAULT` partition is attached or when probing is skipped because any
    /// foreign partition or unexpected Row-Level Security exists on the history partition tree).
    pub default_has_rows: bool,
    /// Catalog row estimate summed across all regular descendant partitions (`GREATEST(reltuples, 0)`).
    pub estimated_rows: i64,
    /// Total relation size (heap + indexes + TOAST) in bytes summed across all regular descendant
    /// partitions.
    pub total_size_bytes: i64,
}

#[derive(Debug, FromRow)]
struct HistoryParentStatRow {
    has_rls: bool,
}

#[derive(Debug, FromRow)]
struct HistoryChildStatRow {
    oid: i64,
    depth: i32,
    under_default: bool,
    relispartition: bool,
    schema_name: String,
    relname: String,
    relkind: String,
    has_rls: bool,
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

    let parent: Option<HistoryParentStatRow> = sqlx::query_as(
        "SELECT \
           ( \
             c.relrowsecurity \
             OR c.relforcerowsecurity \
             OR EXISTS (SELECT 1 FROM pg_catalog.pg_policy pol WHERE pol.polrelid = c.oid) \
           ) AS has_rls \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
        WHERE n.nspname = $1 \
          AND c.relname = 'changes' \
          AND c.relkind IN ('r', 'p')",
    )
    .bind(schema)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(parent) = parent else {
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
    };

    let current_month = fetch_db_utc_month(&mut tx).await?;
    let children = fetch_history_child_stats(&mut tx, schema).await?;

    let mut monthly_partitions = Vec::new();
    let mut default_leaf_oids: Vec<i64> = Vec::new();
    let mut any_child_has_rls = false;
    let mut any_child_is_foreign = false;
    let mut estimated_rows: i64 = 0;
    let mut total_size_bytes: i64 = 0;

    for child in &children {
        any_child_has_rls |= child.has_rls;
        any_child_is_foreign |= child.relkind == "f";
        estimated_rows = estimated_rows.saturating_add(child.est_rows);
        total_size_bytes = total_size_bytes.saturating_add(child.total_bytes);
        if child.under_default {
            if child.relkind == "r" {
                default_leaf_oids.push(child.oid);
            }
        } else if child.depth == 1
            && child.relispartition
            && child.schema_name == schema
            && child.relkind == "r"
            && let Some(ym) = YearMonth::parse_partition_name(&child.relname)
        {
            monthly_partitions.push(ym);
        }
    }
    monthly_partitions.sort();
    monthly_partitions.dedup();
    default_leaf_oids.sort_unstable();
    default_leaf_oids.dedup();

    let present_set: HashSet<YearMonth> = monthly_partitions.iter().copied().collect();
    let mut months_covered_ahead: u32 = 0;
    let mut probe = current_month.next();
    while present_set.contains(&probe) {
        months_covered_ahead = months_covered_ahead.saturating_add(1);
        probe = probe.next();
    }

    // Query through parent `{schema}.changes` filtered by constant DEFAULT leaf OID(s) so
    // PostgreSQL checks SELECT only on the parent table while child partitions stay zero-grant.
    // Skip when any foreign partition or unexpected RLS exists anywhere on the history partition
    // tree so untrusted FDW hooks or RLS policies are never invoked.
    let default_has_rows = if !parent.has_rls && !any_child_has_rls && !any_child_is_foreign {
        probe_default_has_rows(&mut tx, schema, &default_leaf_oids).await?
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

async fn probe_default_has_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema: &str,
    default_leaf_oids: &[i64],
) -> Result<bool, sqlx::Error> {
    if default_leaf_oids.is_empty() {
        return Ok(false);
    }
    let schema_q = quote_ident(schema);
    let oid_list = default_leaf_oids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM {schema_q}.changes WHERE tableoid = ANY(ARRAY[{oid_list}]::pg_catalog.oid[]) LIMIT 1)"
    );
    sqlx::query_scalar(AssertSqlSafe(sql))
        .fetch_one(&mut **tx)
        .await
}

async fn fetch_history_child_stats(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema: &str,
) -> Result<Vec<HistoryChildStatRow>, sqlx::Error> {
    sqlx::query_as(
        "WITH RECURSIVE tree(relid, depth, under_default) AS ( \
           SELECT \
             child.oid, \
             1::int, \
             (child.relispartition AND pg_catalog.pg_get_expr(child.relpartbound, child.oid) = 'DEFAULT') \
           FROM pg_catalog.pg_inherits i \
           JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent \
           JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace \
           JOIN pg_catalog.pg_class child ON child.oid = i.inhrelid \
          WHERE pn.nspname = $1 \
            AND parent.relname = 'changes' \
           UNION \
           SELECT i.inhrelid, tree.depth + 1, tree.under_default \
             FROM tree \
             JOIN pg_catalog.pg_inherits i ON i.inhparent = tree.relid \
         ), \
         deduped AS ( \
           SELECT \
             tree.relid, \
             pg_catalog.min(tree.depth)::int AS depth, \
             pg_catalog.bool_or(tree.under_default) AS under_default \
           FROM tree \
          GROUP BY tree.relid \
         ) \
         SELECT \
           c.oid::bigint AS oid, \
           deduped.depth, \
           deduped.under_default, \
           c.relispartition, \
           cn.nspname::text AS schema_name, \
           c.relname::text AS relname, \
           c.relkind::text AS relkind, \
           ( \
             c.relrowsecurity \
             OR c.relforcerowsecurity \
             OR EXISTS (SELECT 1 FROM pg_catalog.pg_policy pol WHERE pol.polrelid = c.oid) \
           ) AS has_rls, \
           CASE \
             WHEN c.relkind = 'r' THEN GREATEST(c.reltuples::bigint, 0) \
             ELSE 0::bigint \
           END AS est_rows, \
           CASE \
             WHEN c.relkind = 'r' THEN pg_catalog.pg_total_relation_size(c.oid)::bigint \
             ELSE 0::bigint \
           END AS total_bytes \
         FROM deduped \
         JOIN pg_catalog.pg_class c ON c.oid = deduped.relid \
         JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace \
        ORDER BY deduped.depth ASC, cn.nspname ASC, c.relname ASC",
    )
    .bind(schema)
    .fetch_all(&mut **tx)
    .await
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

    let Some(default_oid) = exists else {
        return Ok(0);
    };

    let schema_q = quote_ident(schema);
    let sql = format!(
        "SELECT count(*)::bigint FROM {schema_q}.changes WHERE tableoid = {default_oid}::pg_catalog.oid"
    );
    sqlx::query_scalar(AssertSqlSafe(sql))
        .fetch_one(&mut **tx)
        .await
}

async fn enter_owner_role(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(String, String), sqlx::Error> {
    let saved: (String, String) = sqlx::query_as("SELECT current_user::text, session_user::text")
        .fetch_one(&mut **tx)
        .await?;
    let set_owner_sql = format!("SET LOCAL ROLE {}", quote_ident(OWNER_ROLE));
    sqlx::query(AssertSqlSafe(set_owner_sql))
        .execute(&mut **tx)
        .await?;
    Ok(saved)
}

async fn restore_role(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    (prev_role, session_role): &(String, String),
) -> Result<(), sqlx::Error> {
    sqlx::query("SET LOCAL ROLE NONE")
        .execute(&mut **tx)
        .await?;
    if prev_role != session_role {
        let restore_sql = format!("SET LOCAL ROLE {}", quote_ident(prev_role));
        sqlx::query(AssertSqlSafe(restore_sql))
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn sanitize_child_partition_acl(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema: &str,
    part_name: &str,
) -> Result<(), sqlx::Error> {
    let grantees: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT grantee::bigint, \
                CASE WHEN grantee = 0 THEN NULL ELSE pg_catalog.pg_get_userbyid(grantee)::text END \
           FROM ( \
             SELECT a.grantee \
               FROM pg_catalog.pg_class c \
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
               CROSS JOIN LATERAL pg_catalog.aclexplode( \
                 COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner)) \
               ) AS a \
              WHERE n.nspname = $1 \
                AND c.relname = $2 \
                AND a.grantee <> c.relowner \
             UNION \
             SELECT a.grantee \
               FROM pg_catalog.pg_class c \
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_catalog.pg_attribute att \
                 ON att.attrelid = c.oid \
                AND att.attnum > 0 \
                AND NOT att.attisdropped \
                AND att.attacl IS NOT NULL \
               CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a \
              WHERE n.nspname = $1 \
                AND c.relname = $2 \
                AND a.grantee <> c.relowner \
           ) AS all_grants \
          ORDER BY 1",
    )
    .bind(schema)
    .bind(part_name)
    .fetch_all(&mut **tx)
    .await?;

    let schema_q = quote_ident(schema);
    let part_q = quote_ident(part_name);
    for (grantee_oid, grantee_name) in grantees {
        let revoke_sql = if grantee_oid == 0 {
            format!("REVOKE ALL ON TABLE {schema_q}.{part_q} FROM PUBLIC CASCADE")
        } else if let Some(role) = grantee_name {
            format!(
                "REVOKE ALL ON TABLE {schema_q}.{part_q} FROM {} CASCADE",
                quote_ident(&role)
            )
        } else {
            continue;
        };
        sqlx::query(AssertSqlSafe(revoke_sql))
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerGrantMode {
    None,
    SelfRow(Option<String>),
    SetOption(String),
}

async fn ensure_owner_membership(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<OwnerGrantMode, sqlx::Error> {
    let server_version_num: i32 =
        sqlx::query_scalar("SELECT pg_catalog.current_setting('server_version_num')::int")
            .fetch_one(&mut **tx)
            .await?;
    let owner_q = quote_ident(OWNER_ROLE);

    if server_version_num >= 160_000 {
        let has_set: bool =
            sqlx::query_scalar("SELECT pg_catalog.pg_has_role(current_user, $1, 'SET')")
                .bind(OWNER_ROLE)
                .fetch_one(&mut **tx)
                .await?;
        if has_set {
            return Ok(OwnerGrantMode::None);
        }

        // Resolve the deterministic ADMIN grantor role (`current_user` directly, or an inherited
        // group role holding ADMIN OPTION) and pass `GRANTED BY` explicitly so PostgreSQL never
        // clobbers or deletes pre-existing group-granted memberships on `GRANT`/`REVOKE`.
        let grantor: Option<(i64, String)> = sqlx::query_as(
            "SELECT m.member::bigint, pg_catalog.pg_get_userbyid(m.member)::text \
               FROM pg_catalog.pg_auth_members m \
               JOIN pg_catalog.pg_roles r ON r.oid = m.roleid \
               JOIN pg_catalog.pg_roles u ON u.rolname = current_user \
              WHERE r.rolname = $1 \
                AND m.admin_option \
                AND (m.member = u.oid OR pg_catalog.pg_has_role(u.oid, m.member, 'USAGE')) \
              ORDER BY (m.member = u.oid) DESC, m.member \
              LIMIT 1",
        )
        .bind(OWNER_ROLE)
        .fetch_optional(&mut **tx)
        .await?;

        let Some((grantor_oid, grantor_name)) = grantor else {
            // Let PostgreSQL raise its standard 42501 error for callers lacking ADMIN OPTION.
            let sql = format!(
                "DO $$ BEGIN EXECUTE pg_catalog.format('GRANT {owner_q} TO %I WITH INHERIT FALSE, SET TRUE', current_user); END $$"
            );
            sqlx::query(AssertSqlSafe(sql)).execute(&mut **tx).await?;
            return Ok(OwnerGrantMode::SelfRow(None));
        };

        let has_grantor_row: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
               SELECT 1 \
                 FROM pg_catalog.pg_auth_members m \
                 JOIN pg_catalog.pg_roles r ON r.oid = m.roleid \
                 JOIN pg_catalog.pg_roles u ON u.oid = m.member \
                WHERE r.rolname = $1 \
                  AND u.rolname = current_user \
                  AND m.grantor = $2::pg_catalog.oid \
             )",
        )
        .bind(OWNER_ROLE)
        .bind(grantor_oid)
        .fetch_one(&mut **tx)
        .await?;

        let current_user_name: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&mut **tx)
            .await?;
        let user_q = quote_ident(&current_user_name);
        let grantor_q = quote_ident(&grantor_name);
        if !has_grantor_row {
            let sql = format!(
                "GRANT {owner_q} TO {user_q} WITH INHERIT FALSE, SET TRUE GRANTED BY {grantor_q}"
            );
            sqlx::query(AssertSqlSafe(sql)).execute(&mut **tx).await?;
            return Ok(OwnerGrantMode::SelfRow(Some(grantor_name)));
        }

        let sql = format!("GRANT {owner_q} TO {user_q} WITH SET TRUE GRANTED BY {grantor_q}");
        sqlx::query(AssertSqlSafe(sql)).execute(&mut **tx).await?;
        return Ok(OwnerGrantMode::SetOption(grantor_name));
    }

    let has_member: bool =
        sqlx::query_scalar("SELECT pg_catalog.pg_has_role(current_user, $1, 'MEMBER')")
            .bind(OWNER_ROLE)
            .fetch_one(&mut **tx)
            .await?;
    if !has_member {
        let current_user_name: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&mut **tx)
            .await?;
        let user_q = quote_ident(&current_user_name);
        let sql = format!("GRANT {owner_q} TO {user_q}");
        sqlx::query(AssertSqlSafe(sql)).execute(&mut **tx).await?;
        return Ok(OwnerGrantMode::SelfRow(None));
    }

    Ok(OwnerGrantMode::None)
}

async fn revoke_owner_membership(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mode: OwnerGrantMode,
) -> Result<(), sqlx::Error> {
    if matches!(mode, OwnerGrantMode::None) {
        return Ok(());
    }
    let current_user_name: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut **tx)
        .await?;
    let owner_q = quote_ident(OWNER_ROLE);
    let user_q = quote_ident(&current_user_name);
    let sql = match mode {
        OwnerGrantMode::None => return Ok(()),
        OwnerGrantMode::SelfRow(Some(grantor)) => {
            let grantor_q = quote_ident(&grantor);
            format!("REVOKE {owner_q} FROM {user_q} GRANTED BY {grantor_q}")
        }
        OwnerGrantMode::SelfRow(None) => format!("REVOKE {owner_q} FROM {user_q}"),
        OwnerGrantMode::SetOption(grantor) => {
            let grantor_q = quote_ident(&grantor);
            format!("REVOKE SET OPTION FOR {owner_q} FROM {user_q} GRANTED BY {grantor_q}")
        }
    };
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
