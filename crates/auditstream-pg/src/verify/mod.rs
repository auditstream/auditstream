//! Read-only drift detection for `auditstream verify`.
//!
//! Compares the live PostgreSQL catalog against what [`crate::ddl`] and [`crate::triggers`]
//! generate, reporting missing objects, disabled triggers (including per-partition clones of
//! row-level `UPDATE` triggers), outdated function bodies or trigger definitions (including
//! changed `excluded_columns` or `primary_key`), partitioned tables whose direct partition
//! writes bypass statement triggers, and rows stranded in `changes_default`.
//!
//! Every catalog inspection runs inside a `READ ONLY` transaction (`SET TRANSACTION READ ONLY`),
//! so [`check`] and [`check_with_options`] can never mutate the database they inspect.

use std::collections::{HashMap, HashSet};

use sqlx::{AssertSqlSafe, FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;

use crate::ddl::{DEFAULT_SCHEMA, quote_ident};
use crate::triggers::{self, TableTriggerSpec, TriggerOptions};

const TRIGGER_TYPE_ROW: i16 = 1 << 0;
const TRIGGER_TYPE_BEFORE: i16 = 1 << 1;
const TRIGGER_TYPE_INSERT: i16 = 1 << 2;
const TRIGGER_TYPE_DELETE: i16 = 1 << 3;
const TRIGGER_TYPE_UPDATE: i16 = 1 << 4;
const TRIGGER_TYPE_TRUNCATE: i16 = 1 << 5;
const TRIGGER_TYPE_INSTEAD: i16 = 1 << 6;
const TRIGGER_EVENTS_MASK: i16 =
    TRIGGER_TYPE_INSERT | TRIGGER_TYPE_DELETE | TRIGGER_TYPE_UPDATE | TRIGGER_TYPE_TRUNCATE;

const EXPECTED_SEARCH_PATH: &str = "search_path=pg_catalog, pg_temp";

/// Errors returned while running capture drift verification.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// A PostgreSQL connection or catalog query failed.
    #[error("database verification failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Severity of a verification [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Capture is broken, incomplete, or unsafe; `auditstream verify` exits non-zero.
    Error,
    /// Advisory condition operators should be aware of (such as direct-partition write caveats).
    Warning,
}

/// Machine-readable category of a verification [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    /// An expected history table, function, audited table, or trigger does not exist.
    Missing,
    /// A trigger (or a partition's cloned `UPDATE` trigger) is disabled (`D`) or replica-only (`R`).
    Disabled,
    /// A function body, security attribute, or trigger definition/arguments differs from D3's output.
    Outdated,
    /// An audited table is partitioned (`relkind = 'p'`); writes naming a partition directly
    /// bypass parent statement-level `INSERT`, `DELETE`, and `TRUNCATE` triggers.
    PartitionedTable,
    /// The `changes_default` partition holds rows, blocking monthly partition creation.
    DefaultPartitionRows,
}

/// Single drift finding returned by [`check`] and [`check_with_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Category of drift detected.
    pub kind: FindingKind,
    /// Error or warning severity.
    pub severity: Severity,
    /// Qualified table (`schema.table`) associated with this finding, if table-scoped.
    pub table: Option<String>,
    /// Fully-qualified object name (for example `public.invoice.auditstream_update` or
    /// `auditstream.capture()`).
    pub object: String,
    /// Human-readable description naming the exact mismatch or remediation detail.
    pub detail: String,
}

/// Summary of rows currently held in `{audit_schema}.changes_default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultPartitionStatus {
    /// Exact number of rows stored in `changes_default`.
    pub row_count: i64,
    /// Oldest `occurred_at` timestamp (RFC 3339 UTC), present when `row_count > 0`.
    pub oldest_occurred_at: Option<String>,
    /// Newest `occurred_at` timestamp (RFC 3339 UTC), present when `row_count > 0`.
    pub newest_occurred_at: Option<String>,
}

/// Deterministic report returned by [`check`] and [`check_with_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Findings sorted deterministically (`Error` before `Warning`, then `table`, `object`, `kind`, `detail`).
    pub findings: Vec<Finding>,
    /// Inspection stats for `{audit_schema}.changes_default`, when that partition exists.
    pub default_partition: Option<DefaultPartitionStatus>,
}

impl VerifyReport {
    /// Returns `true` when there are no findings (neither errors nor warnings).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// Returns `true` when at least one [`Severity::Error`] finding is present.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    /// Iterates over [`Severity::Error`] findings in report order.
    pub fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
    }

    /// Iterates over [`Severity::Warning`] findings in report order.
    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
    }
}

/// Configuration for [`check_with_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Audit history schema name (defaults to [`DEFAULT_SCHEMA`], `"auditstream"`).
    pub audit_schema: String,
    /// Expected audited table trigger specifications.
    pub tables: Vec<TableTriggerSpec>,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            audit_schema: DEFAULT_SCHEMA.to_owned(),
            tables: Vec::new(),
        }
    }
}

impl VerifyOptions {
    /// Creates [`VerifyOptions`] for the given audit schema and audited tables.
    pub fn new(audit_schema: impl Into<String>, tables: Vec<TableTriggerSpec>) -> Self {
        Self {
            audit_schema: audit_schema.into(),
            tables,
        }
    }
}

/// Live Auditstream trigger installation metadata discovered on a single user table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledTableCapture {
    /// Table schema name.
    pub schema: String,
    /// Table name.
    pub table: String,
    /// Whether the table is a partitioned parent (`relkind = 'p'`).
    pub is_partitioned: bool,
    /// Primary key column names decoded from the trigger's first `TG_ARGV` JSON array.
    pub primary_key: Vec<String>,
    /// Excluded column names decoded from the trigger's second `TG_ARGV` JSON array.
    pub excluded_columns: Vec<String>,
    /// Installed `auditstream_*` triggers on this table as sorted `(trigger_name, tgenabled)` pairs.
    pub triggers: Vec<(String, String)>,
}

/// Discovers every top-level user table that currently has at least one non-internal
/// `auditstream_*` trigger pointing at `{audit_schema}.capture()`, inside a `READ ONLY`
/// transaction.
///
/// Used by `auditstream status` to report trigger state and excluded columns and by
/// `auditstream verify` when no `capture.tables` selection is configured.
///
/// # Errors
///
/// Returns [`VerifyError::Database`] if querying PostgreSQL catalogs fails.
pub async fn installed_captures(
    pool: &PgPool,
    audit_schema: &str,
) -> Result<Vec<InstalledTableCapture>, VerifyError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;

    let rows: Vec<InstalledTriggerRow> = sqlx::query_as(
        "SELECT \
           n.nspname AS schema_name, \
           c.relname AS table_name, \
           c.relkind::text AS relkind, \
           t.tgname, \
           t.tgenabled::text AS tgenabled, \
           t.tgnargs, \
           t.tgargs \
         FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
         JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
        WHERE NOT t.tgisinternal \
          AND NOT c.relispartition \
          AND c.relkind IN ('r', 'p') \
          AND t.tgname LIKE 'auditstream\\_%' ESCAPE '\\' \
          AND pn.nspname = $1 \
          AND p.proname = 'capture' \
        ORDER BY n.nspname ASC, c.relname ASC, t.tgname ASC",
    )
    .bind(audit_schema)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut grouped: HashMap<(String, String), InstalledTableCapture> = HashMap::new();
    for row in rows {
        let key = (row.schema_name.clone(), row.table_name.clone());
        let args = decode_tgargs(&row.tgargs, row.tgnargs);
        let pk: Vec<String> = args
            .first()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let excluded: Vec<String> = args
            .get(1)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let entry = grouped.entry(key).or_insert_with(|| InstalledTableCapture {
            schema: row.schema_name,
            table: row.table_name,
            is_partitioned: row.relkind == "p",
            primary_key: pk,
            excluded_columns: excluded,
            triggers: Vec::new(),
        });
        entry.triggers.push((row.tgname, row.tgenabled));
    }

    let mut result: Vec<InstalledTableCapture> = grouped.into_values().collect();
    for item in &mut result {
        item.triggers.sort();
    }
    result.sort_by(|a, b| a.schema.cmp(&b.schema).then_with(|| a.table.cmp(&b.table)));
    Ok(result)
}

#[derive(Debug, FromRow)]
struct InstalledTriggerRow {
    schema_name: String,
    table_name: String,
    relkind: String,
    tgname: String,
    tgenabled: String,
    tgnargs: i16,
    tgargs: Vec<u8>,
}

/// Verifies capture installation against `tables` in the default `"auditstream"` schema.
///
/// Runs inside a `READ ONLY` PostgreSQL transaction and returns findings in a deterministic
/// order. A fresh D2 + D3 install on non-partitioned tables returns an empty finding list.
///
/// # Errors
///
/// Returns [`VerifyError::Database`] if connecting or querying PostgreSQL catalogs fails.
pub async fn check(
    pool: &PgPool,
    tables: &[TableTriggerSpec],
) -> Result<VerifyReport, VerifyError> {
    check_with_options(pool, &VerifyOptions::new(DEFAULT_SCHEMA, tables.to_vec())).await
}

/// Verifies capture installation using custom [`VerifyOptions`] (including a custom audit schema).
///
/// Runs inside a `READ ONLY` PostgreSQL transaction and returns findings in a deterministic order.
///
/// # Errors
///
/// Returns [`VerifyError::Database`] if connecting or querying PostgreSQL catalogs fails.
pub async fn check_with_options(
    pool: &PgPool,
    options: &VerifyOptions,
) -> Result<VerifyReport, VerifyError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;

    let mut findings = Vec::new();
    let expected_bodies = ExpectedFunctionBodies::from_generator(&options.audit_schema);

    let history_exists = check_history_table(&mut tx, &options.audit_schema, &mut findings).await?;
    check_function(
        &mut tx,
        &options.audit_schema,
        "resolve_actor",
        &expected_bodies.resolve_actor,
        &mut findings,
    )
    .await?;
    check_function(
        &mut tx,
        &options.audit_schema,
        "capture",
        &expected_bodies.capture,
        &mut findings,
    )
    .await?;

    let default_partition = if history_exists {
        check_default_partition(&mut tx, &options.audit_schema, &mut findings).await?
    } else {
        None
    };

    check_tables(
        &mut tx,
        &options.audit_schema,
        &options.tables,
        &mut findings,
    )
    .await?;

    tx.commit().await?;

    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.table.cmp(&b.table))
            .then_with(|| a.object.cmp(&b.object))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.detail.cmp(&b.detail))
    });

    Ok(VerifyReport {
        findings,
        default_partition,
    })
}

struct ExpectedFunctionBodies {
    resolve_actor: String,
    capture: String,
}

impl ExpectedFunctionBodies {
    fn from_generator(audit_schema: &str) -> Self {
        let stmts = triggers::generate(&TriggerOptions::new(audit_schema, Vec::new()));
        let mut resolve_actor = None;
        let mut capture = None;

        for stmt in &stmts {
            let Some(first_line) = stmt.lines().next() else {
                continue;
            };
            if first_line.starts_with("CREATE OR REPLACE FUNCTION ") {
                if first_line.contains(".resolve_actor(") {
                    resolve_actor = extract_dollar_quoted_body(stmt, "$fn$");
                } else if first_line.contains(".capture(") {
                    capture = extract_dollar_quoted_body(stmt, "$fn$");
                }
            }
        }

        Self {
            resolve_actor: resolve_actor
                .expect("triggers::generate must emit resolve_actor() with $fn$ body"),
            capture: capture.expect("triggers::generate must emit capture() with $fn$ body"),
        }
    }
}

fn extract_dollar_quoted_body(sql: &str, tag: &str) -> Option<String> {
    let start = sql.find(tag)? + tag.len();
    let rest = &sql[start..];
    let end = rest.rfind(tag)?;
    Some(rest[..end].to_owned())
}

async fn check_history_table(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<bool, sqlx::Error> {
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT c.oid::bigint \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = $1 \
            AND c.relname = 'changes' \
            AND c.relkind IN ('r', 'p')",
    )
    .bind(audit_schema)
    .fetch_optional(&mut **tx)
    .await?;

    if exists.is_some() {
        return Ok(true);
    }

    let object = format!("{audit_schema}.changes");
    findings.push(Finding {
        kind: FindingKind::Missing,
        severity: Severity::Error,
        table: Some(object.clone()),
        object: object.clone(),
        detail: format!("history table `{object}` is missing"),
    });
    Ok(false)
}

#[derive(Debug, FromRow)]
struct ProcRow {
    prosrc: String,
    prosecdef: bool,
    langname: String,
    proconfig: Option<Vec<String>>,
}

async fn check_function(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    func_name: &str,
    expected_body: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let object = format!("{audit_schema}.{func_name}()");
    let row: Option<ProcRow> = sqlx::query_as(
        "SELECT \
           p.prosrc, \
           p.prosecdef, \
           l.lanname AS langname, \
           p.proconfig \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
        WHERE n.nspname = $1 \
          AND p.proname = $2 \
          AND p.pronargs = 0",
    )
    .bind(audit_schema)
    .bind(func_name)
    .fetch_optional(&mut **tx)
    .await?;

    let Some(proc_row) = row else {
        findings.push(Finding {
            kind: FindingKind::Missing,
            severity: Severity::Error,
            table: None,
            object: object.clone(),
            detail: format!("capture function `{object}` is missing"),
        });
        return Ok(());
    };

    let mut diffs = Vec::new();
    if proc_row.prosrc.trim() != expected_body.trim() {
        diffs.push("function body differs from generated SQL");
    }
    if !proc_row.prosecdef {
        diffs.push("function is missing SECURITY DEFINER");
    }
    if proc_row.langname != "plpgsql" {
        diffs.push("function language is not plpgsql");
    }
    let has_search_path = proc_row
        .proconfig
        .as_deref()
        .is_some_and(|cfg| cfg.iter().any(|entry| entry == EXPECTED_SEARCH_PATH));
    if !has_search_path {
        diffs.push("function is missing `SET search_path = pg_catalog, pg_temp`");
    }

    if !diffs.is_empty() {
        findings.push(Finding {
            kind: FindingKind::Outdated,
            severity: Severity::Error,
            table: None,
            object: object.clone(),
            detail: format!("function `{object}` is outdated: {}", diffs.join("; ")),
        });
    }

    Ok(())
}

#[derive(Debug, FromRow)]
struct DefaultPartitionRow {
    row_count: i64,
    oldest: Option<String>,
    newest: Option<String>,
}

async fn check_default_partition(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<Option<DefaultPartitionStatus>, sqlx::Error> {
    let default_exists: Option<i64> = sqlx::query_scalar(
        "SELECT c.oid::bigint \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = $1 \
            AND c.relname = 'changes_default' \
            AND c.relkind = 'r'",
    )
    .bind(audit_schema)
    .fetch_optional(&mut **tx)
    .await?;

    if default_exists.is_none() {
        return Ok(None);
    }

    let schema_q = quote_ident(audit_schema);
    // Query ONLY the DEFAULT partition so monthly partitions are never scanned.
    let sql = format!(
        "SELECT \
           count(*)::bigint AS row_count, \
           to_char(min(occurred_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS oldest, \
           to_char(max(occurred_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS newest \
         FROM ONLY {schema_q}.changes_default"
    );

    let row: DefaultPartitionRow = sqlx::query_as(AssertSqlSafe(sql))
        .fetch_one(&mut **tx)
        .await?;

    let status = DefaultPartitionStatus {
        row_count: row.row_count,
        oldest_occurred_at: row.oldest,
        newest_occurred_at: row.newest,
    };

    if status.row_count > 0 {
        let object = format!("{audit_schema}.changes_default");
        let oldest = status.oldest_occurred_at.as_deref().unwrap_or("unknown");
        let newest = status.newest_occurred_at.as_deref().unwrap_or("unknown");
        findings.push(Finding {
            kind: FindingKind::DefaultPartitionRows,
            severity: Severity::Error,
            table: Some(object.clone()),
            object: object.clone(),
            detail: format!(
                "DEFAULT partition `{object}` holds {} row(s) (oldest: {oldest}, newest: {newest}); monthly partitions cannot be created for covered months until those rows are moved",
                status.row_count
            ),
        });
    }

    Ok(Some(status))
}

#[derive(Debug, FromRow)]
struct TableLookupRow {
    oid: i64,
    schema_name: String,
    table_name: String,
    relkind: String,
}

#[derive(Debug, FromRow)]
struct TriggerCatalogRow {
    tgrelid: i64,
    tgname: String,
    tgenabled: String,
    tgtype: i16,
    tgnargs: i16,
    tgargs: Vec<u8>,
    tgnewtable: Option<String>,
    tgoldtable: Option<String>,
    func_schema: String,
    func_name: String,
    has_when: bool,
}

#[derive(Debug, FromRow)]
struct PartitionCloneRow {
    parent_relid: i64,
    child_schema: String,
    child_table: String,
    tgname: String,
    tgenabled: String,
}

async fn check_tables(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    tables: &[TableTriggerSpec],
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    if tables.is_empty() {
        return Ok(());
    }

    let schemas: Vec<String> = tables
        .iter()
        .map(|t| t.schema.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let names: Vec<String> = tables
        .iter()
        .map(|t| t.table.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let catalog_tables: Vec<TableLookupRow> = sqlx::query_as(
        "SELECT \
           c.oid::bigint AS oid, \
           n.nspname AS schema_name, \
           c.relname AS table_name, \
           c.relkind::text AS relkind \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
        WHERE n.nspname = ANY($1) \
          AND c.relname = ANY($2) \
          AND c.relkind IN ('r', 'p')",
    )
    .bind(&schemas)
    .bind(&names)
    .fetch_all(&mut **tx)
    .await?;

    let mut table_map: HashMap<(String, String), TableLookupRow> = HashMap::new();
    for row in catalog_tables {
        table_map.insert((row.schema_name.clone(), row.table_name.clone()), row);
    }

    let present_oids: Vec<i64> = tables
        .iter()
        .filter_map(|spec| table_map.get(&(spec.schema.clone(), spec.table.clone())))
        .map(|r| r.oid)
        .collect();

    let triggers_by_rel = fetch_table_triggers(tx, &present_oids).await?;

    let partitioned_oids: Vec<i64> = tables
        .iter()
        .filter_map(|spec| table_map.get(&(spec.schema.clone(), spec.table.clone())))
        .filter(|r| r.relkind == "p")
        .map(|r| r.oid)
        .collect();

    let partition_clones = fetch_partition_update_clones(tx, &partitioned_oids).await?;

    for spec in tables {
        check_single_table(
            spec,
            audit_schema,
            &table_map,
            &triggers_by_rel,
            &partition_clones,
            findings,
        );
    }

    Ok(())
}

fn check_single_table(
    spec: &TableTriggerSpec,
    audit_schema: &str,
    table_map: &HashMap<(String, String), TableLookupRow>,
    triggers_by_rel: &HashMap<i64, HashMap<String, TriggerCatalogRow>>,
    partition_clones: &HashMap<i64, Vec<PartitionCloneRow>>,
    findings: &mut Vec<Finding>,
) {
    let fq_table = format!("{}.{}", spec.schema, spec.table);
    let Some(cat) = table_map.get(&(spec.schema.clone(), spec.table.clone())) else {
        findings.push(Finding {
            kind: FindingKind::Missing,
            severity: Severity::Error,
            table: Some(fq_table.clone()),
            object: fq_table.clone(),
            detail: format!("audited table `{fq_table}` does not exist"),
        });
        return;
    };

    let is_partitioned_live = cat.relkind == "p";
    if is_partitioned_live {
        findings.push(Finding {
            kind: FindingKind::PartitionedTable,
            severity: Severity::Warning,
            table: Some(fq_table.clone()),
            object: fq_table.clone(),
            detail: format!(
                "table `{fq_table}` is partitioned; statements naming a child partition directly bypass parent statement-level INSERT, DELETE, and TRUNCATE triggers"
            ),
        });
    }

    let rel_triggers = triggers_by_rel.get(&cat.oid);
    let expected_args = expected_trigger_args(spec, is_partitioned_live);

    for expected in EXPECTED_TRIGGERS {
        let object = format!("{fq_table}.{}", expected.name);
        let actual = rel_triggers.and_then(|map| map.get(expected.name));

        let Some(trig) = actual else {
            findings.push(Finding {
                kind: FindingKind::Missing,
                severity: Severity::Error,
                table: Some(fq_table.clone()),
                object: object.clone(),
                detail: format!(
                    "trigger `{}` ({}) is missing on `{fq_table}`",
                    expected.name, expected.event_label
                ),
            });
            continue;
        };

        if let Some(state_label) = disabled_state_label(&trig.tgenabled) {
            findings.push(Finding {
                kind: FindingKind::Disabled,
                severity: Severity::Error,
                table: Some(fq_table.clone()),
                object: object.clone(),
                detail: format!(
                    "trigger `{}` on `{fq_table}` is {state_label} (`tgenabled = '{}'`)",
                    expected.name, trig.tgenabled
                ),
            });
        }

        let diffs = diff_trigger(trig, expected, audit_schema, &expected_args);
        if !diffs.is_empty() {
            findings.push(Finding {
                kind: FindingKind::Outdated,
                severity: Severity::Error,
                table: Some(fq_table.clone()),
                object,
                detail: format!(
                    "trigger `{}` on `{fq_table}` is outdated: {}",
                    expected.name,
                    diffs.join("; ")
                ),
            });
        }
    }

    if is_partitioned_live && let Some(clones) = partition_clones.get(&cat.oid) {
        for clone in clones {
            if let Some(state_label) = disabled_state_label(&clone.tgenabled) {
                let child_fq = format!("{}.{}", clone.child_schema, clone.child_table);
                findings.push(Finding {
                    kind: FindingKind::Disabled,
                    severity: Severity::Error,
                    table: Some(fq_table.clone()),
                    object: format!("{child_fq}.{}", clone.tgname),
                    detail: format!(
                        "cloned trigger `{}` on child partition `{child_fq}` (of `{fq_table}`) is {state_label} (`tgenabled = '{}'`)",
                        clone.tgname, clone.tgenabled
                    ),
                });
            }
        }
    }
}

async fn fetch_table_triggers(
    tx: &mut Transaction<'_, Postgres>,
    table_oids: &[i64],
) -> Result<HashMap<i64, HashMap<String, TriggerCatalogRow>>, sqlx::Error> {
    if table_oids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows: Vec<TriggerCatalogRow> = sqlx::query_as(
        "SELECT \
           t.tgrelid::bigint AS tgrelid, \
           t.tgname, \
           t.tgenabled::text AS tgenabled, \
           t.tgtype, \
           t.tgnargs, \
           t.tgargs, \
           t.tgnewtable, \
           t.tgoldtable, \
           pn.nspname AS func_schema, \
           p.proname AS func_name, \
           t.tgqual IS NOT NULL AS has_when \
         FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
         JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
        WHERE t.tgrelid = ANY($1) \
          AND NOT t.tgisinternal",
    )
    .bind(table_oids)
    .fetch_all(&mut **tx)
    .await?;

    let mut by_rel: HashMap<i64, HashMap<String, TriggerCatalogRow>> = HashMap::new();
    for row in rows {
        by_rel
            .entry(row.tgrelid)
            .or_default()
            .insert(row.tgname.clone(), row);
    }
    Ok(by_rel)
}

async fn fetch_partition_update_clones(
    tx: &mut Transaction<'_, Postgres>,
    partitioned_oids: &[i64],
) -> Result<HashMap<i64, Vec<PartitionCloneRow>>, sqlx::Error> {
    if partitioned_oids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows: Vec<PartitionCloneRow> = sqlx::query_as(
        "WITH RECURSIVE tree(root_oid, child_oid) AS ( \
           SELECT i.inhparent, i.inhrelid \
             FROM pg_catalog.pg_inherits i \
            WHERE i.inhparent = ANY($1) \
           UNION ALL \
           SELECT tree.root_oid, i.inhrelid \
             FROM tree \
             JOIN pg_catalog.pg_inherits i ON i.inhparent = tree.child_oid \
         ) \
         SELECT \
           tree.root_oid::bigint AS parent_relid, \
           cn.nspname AS child_schema, \
           c.relname AS child_table, \
           t.tgname, \
           t.tgenabled::text AS tgenabled \
         FROM tree \
         JOIN pg_catalog.pg_class c ON c.oid = tree.child_oid \
         JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace \
         JOIN pg_catalog.pg_trigger t ON t.tgrelid = tree.child_oid \
        WHERE NOT t.tgisinternal \
          AND (t.tgparentid <> 0 OR t.tgname = 'auditstream_update')",
    )
    .bind(partitioned_oids)
    .fetch_all(&mut **tx)
    .await?;

    let mut by_parent: HashMap<i64, Vec<PartitionCloneRow>> = HashMap::new();
    for row in rows {
        by_parent.entry(row.parent_relid).or_default().push(row);
    }
    Ok(by_parent)
}

#[derive(Debug, Clone, Copy)]
struct ExpectedTrigger {
    name: &'static str,
    event_label: &'static str,
    event_bit: i16,
    for_each_row: bool,
    new_table: Option<&'static str>,
    old_table: Option<&'static str>,
}

const EXPECTED_TRIGGERS: [ExpectedTrigger; 4] = [
    ExpectedTrigger {
        name: "auditstream_insert",
        event_label: "AFTER INSERT",
        event_bit: TRIGGER_TYPE_INSERT,
        for_each_row: false,
        new_table: Some("new_rows"),
        old_table: None,
    },
    ExpectedTrigger {
        name: "auditstream_update",
        event_label: "AFTER UPDATE",
        event_bit: TRIGGER_TYPE_UPDATE,
        for_each_row: true,
        new_table: None,
        old_table: None,
    },
    ExpectedTrigger {
        name: "auditstream_delete",
        event_label: "AFTER DELETE",
        event_bit: TRIGGER_TYPE_DELETE,
        for_each_row: false,
        new_table: None,
        old_table: Some("old_rows"),
    },
    ExpectedTrigger {
        name: "auditstream_truncate",
        event_label: "AFTER TRUNCATE",
        event_bit: TRIGGER_TYPE_TRUNCATE,
        for_each_row: false,
        new_table: None,
        old_table: None,
    },
];

fn disabled_state_label(tgenabled: &str) -> Option<&'static str> {
    match tgenabled {
        "D" => Some("disabled"),
        "R" => Some("replica-only (disabled for normal sessions)"),
        _ => None,
    }
}

fn expected_trigger_args(spec: &TableTriggerSpec, is_partitioned: bool) -> Vec<String> {
    let pk_json = serde_json::to_string(&spec.primary_key)
        .expect("primary_key JSON serialization cannot fail");
    let ex_json = serde_json::to_string(&spec.excluded_columns)
        .expect("excluded_columns JSON serialization cannot fail");
    if spec.is_partitioned || is_partitioned {
        vec![pk_json, ex_json, spec.schema.clone(), spec.table.clone()]
    } else {
        vec![pk_json, ex_json]
    }
}

fn decode_tgargs(bytes: &[u8], expected_count: i16) -> Vec<String> {
    let count = usize::try_from(expected_count).unwrap_or(0);
    if count == 0 {
        return Vec::new();
    }
    bytes
        .split(|&b| b == 0)
        .take(count)
        .map(|slice| String::from_utf8_lossy(slice).into_owned())
        .collect()
}

fn diff_trigger(
    actual: &TriggerCatalogRow,
    expected: ExpectedTrigger,
    audit_schema: &str,
    expected_args: &[String],
) -> Vec<String> {
    let mut diffs = Vec::new();

    let is_row = (actual.tgtype & TRIGGER_TYPE_ROW) != 0;
    if is_row != expected.for_each_row {
        diffs.push(format!(
            "level is FOR EACH {}, expected FOR EACH {}",
            if is_row { "ROW" } else { "STATEMENT" },
            if expected.for_each_row {
                "ROW"
            } else {
                "STATEMENT"
            }
        ));
    }

    let is_before = (actual.tgtype & TRIGGER_TYPE_BEFORE) != 0;
    let is_instead = (actual.tgtype & TRIGGER_TYPE_INSTEAD) != 0;
    if is_before || is_instead {
        let timing = if is_instead { "INSTEAD OF" } else { "BEFORE" };
        diffs.push(format!("timing is {timing}, expected AFTER"));
    }

    let actual_events = actual.tgtype & TRIGGER_EVENTS_MASK;
    if actual_events != expected.event_bit {
        diffs.push(format!(
            "events are {}, expected {}",
            format_event_bits(actual_events),
            format_event_bits(expected.event_bit)
        ));
    }

    if actual.func_schema != audit_schema || actual.func_name != "capture" {
        diffs.push(format!(
            "function is `{}.{}()`, expected `{audit_schema}.capture()`",
            actual.func_schema, actual.func_name
        ));
    }

    if actual.tgnewtable.as_deref() != expected.new_table {
        diffs.push(format!(
            "REFERENCING NEW TABLE is {:?}, expected {:?}",
            actual.tgnewtable, expected.new_table
        ));
    }
    if actual.tgoldtable.as_deref() != expected.old_table {
        diffs.push(format!(
            "REFERENCING OLD TABLE is {:?}, expected {:?}",
            actual.tgoldtable, expected.old_table
        ));
    }

    if actual.has_when {
        diffs.push("unexpected WHEN clause".to_owned());
    }

    let actual_args = decode_tgargs(&actual.tgargs, actual.tgnargs);
    if actual_args != expected_args {
        if actual_args.len() >= 2
            && expected_args.len() >= 2
            && actual_args[0] == expected_args[0]
            && actual_args[1] != expected_args[1]
            && actual_args.len() == expected_args.len()
        {
            diffs.push(format!(
                "arguments (excluded columns) are {}, expected {}",
                actual_args[1], expected_args[1]
            ));
        } else {
            diffs.push(format!(
                "arguments are {actual_args:?}, expected {expected_args:?}"
            ));
        }
    }

    diffs
}

fn format_event_bits(bits: i16) -> String {
    let mut parts = Vec::new();
    if (bits & TRIGGER_TYPE_INSERT) != 0 {
        parts.push("INSERT");
    }
    if (bits & TRIGGER_TYPE_UPDATE) != 0 {
        parts.push("UPDATE");
    }
    if (bits & TRIGGER_TYPE_DELETE) != 0 {
        parts.push("DELETE");
    }
    if (bits & TRIGGER_TYPE_TRUNCATE) != 0 {
        parts.push("TRUNCATE");
    }
    if parts.is_empty() {
        "NONE".to_owned()
    } else {
        parts.join(" OR ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_tgargs_parses_nul_separated_arguments() {
        let raw = b"[\"id\"]\0[\"password_hash\"]\0";
        assert_eq!(
            decode_tgargs(raw, 2),
            vec!["[\"id\"]".to_owned(), "[\"password_hash\"]".to_owned()]
        );
        assert_eq!(decode_tgargs(b"", 0), Vec::<String>::new());
    }

    #[test]
    fn expected_function_bodies_extract_cleanly_from_generator() {
        let bodies = ExpectedFunctionBodies::from_generator(DEFAULT_SCHEMA);
        assert!(bodies.resolve_actor.contains("auditstream.actor"));
        assert!(bodies.capture.contains("TG_OP = 'TRUNCATE'"));
    }
}
