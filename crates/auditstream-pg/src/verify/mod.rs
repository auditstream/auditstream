//! Read-only drift detection for `auditstream verify`.
//!
//! Compares the live PostgreSQL catalog against what [`crate::ddl`] and [`crate::triggers`]
//! generate, reporting missing objects (including `auditstream_owner`, attached `changes_default`,
//! the history primary key `changes_pkey`, and required history indexes), disabled triggers
//! (including per-partition clones of row-level `UPDATE` triggers), outdated function
//! bodies/signatures, history indexes/`changes_pkey`, history partition `relkind`/DEFAULT naming,
//! or trigger definitions (including changed `excluded_columns` or `primary_key`), invalid
//! exclusions (primary key or renamed/dropped columns), unsafe ownership, privileges, role
//! attributes/memberships (including recursive PG 16+ indirect `ADMIN` chains),
//! `session_replication_role` parameter grants or `replica` overrides, history tampering
//! (triggers including internal RI triggers, rewrite rules, unexpected Row-Level Security on
//! `changes` or its partitions, or unexpected helper function overloads), partitioned tables whose
//! direct partition writes bypass statement triggers, and rows stranded in `changes_default`.
//!
//! Every catalog inspection runs inside a `READ ONLY` transaction (`SET TRANSACTION READ ONLY`),
//! so [`check`] and [`check_with_options`] can never mutate the database they inspect.

use std::collections::{HashMap, HashSet};

use sqlx::{AssertSqlSafe, FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;

use crate::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
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

macro_rules! with_history_tree {
    ($select:literal) => {
        concat!(
            "WITH RECURSIVE tree(relid) AS ( \
               SELECT c.oid \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                WHERE n.nspname = $1 \
                  AND c.relname = 'changes' \
                  AND c.relkind IN ('r', 'p') \
               UNION \
               SELECT i.inhrelid \
                 FROM tree \
                 JOIN pg_catalog.pg_inherits i ON i.inhparent = tree.relid \
             ) ",
            $select
        )
    };
}

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
#[non_exhaustive]
pub enum FindingKind {
    /// An expected owner role, history table, `changes_default` partition, primary key, history
    /// index, function, audited table, or trigger does not exist (or is detached).
    Missing,
    /// A trigger (or a partition's cloned `UPDATE` trigger) is disabled (`D`) or replica-only (`R`).
    Disabled,
    /// A function body, security/configuration/signature attribute, history index, history partition
    /// `relkind`/name, or trigger definition/arguments differs from the generated SQL.
    Outdated,
    /// An audited table is partitioned (`relkind = 'p'`); writes naming a partition directly
    /// bypass parent statement-level `INSERT`, `DELETE`, and `TRUNCATE` triggers.
    PartitionedTable,
    /// The `changes_default` partition holds rows, blocking monthly partition creation.
    DefaultPartitionRows,
    /// An excluded column (either in configuration or on an installed trigger) either targets a
    /// primary key column (causing `NULL` entries in `record_id`) or does not exist on the live
    /// table (for example after a column rename or drop).
    InvalidExclusion,
    /// An Auditstream schema, table, child partition, identity sequence, function, owner role
    /// (`auditstream_owner`), or `session_replication_role` parameter/default/session setting has
    /// unsafe privileges, attributes, `replica` overrides, or role memberships.
    UnsafePrivileges,
    /// An Auditstream schema, table, child partition, identity sequence, or function is not
    /// owned by `auditstream_owner`.
    InvalidOwnership,
    /// An unexpected trigger (including internal RI triggers), rewrite rule, or Row-Level Security
    /// setting/policy is attached to `{audit_schema}.changes` or one of its child partitions, or
    /// an unexpected helper function overload exists in `{audit_schema}`.
    TamperedHistory,
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

/// Summary of rows currently held in the attached `DEFAULT` partition of `{audit_schema}.changes`
/// (canonically `{audit_schema}.changes_default`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultPartitionStatus {
    /// Exact number of rows stored in the attached `DEFAULT` partition.
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
    /// Inspection stats for the attached `DEFAULT` partition of `{audit_schema}.changes`, when
    /// one is attached and safe to probe (omitted when missing/detached or when any foreign
    /// partition or unexpected Row-Level Security is detected on the history partition tree).
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

    let report = run_checks(&mut tx, options).await?;
    tx.commit().await?;
    Ok(report)
}

/// Internal integration-test hook for running read-only verification queries inside a
/// caller-managed transaction so tests can inspect uncommitted cluster-wide catalog mutations
/// (`auditstream_owner`, `pg_parameter_acl`) and roll them back without affecting concurrent tests.
#[doc(hidden)]
pub async fn check_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    options: &VerifyOptions,
) -> Result<VerifyReport, VerifyError> {
    run_checks(tx, options).await.map_err(VerifyError::from)
}

async fn run_checks(
    tx: &mut Transaction<'_, Postgres>,
    options: &VerifyOptions,
) -> Result<VerifyReport, sqlx::Error> {
    let mut findings = Vec::new();
    let expected_bodies = ExpectedFunctionBodies::from_generator(&options.audit_schema);

    check_owner_role_security(tx, &mut findings).await?;
    check_session_replication_role_acl(tx, &mut findings).await?;
    check_schema_security(tx, &options.audit_schema, &mut findings).await?;
    let history_exists = check_history_table(tx, &options.audit_schema, &mut findings).await?;
    let default_partition = if history_exists {
        let history_has_foreign =
            check_history_security(tx, &options.audit_schema, &mut findings).await?;
        let history_has_rls =
            check_history_tampering(tx, &options.audit_schema, &mut findings).await?;
        check_history_indexes(tx, &options.audit_schema, &mut findings).await?;
        check_default_partition(
            tx,
            &options.audit_schema,
            history_has_rls || history_has_foreign,
            &mut findings,
        )
        .await?
    } else {
        None
    };

    let function_specs = [
        FunctionCheckSpec {
            name: "resolve_actor",
            pronargs: 0,
            expected_nargdefaults: 0,
            expected_arg_defaults: None,
            expected_rettype: "record",
            expected_retset: false,
            expected_volatile: "s",
            expected_arg_names: &["actor", "context", "actor_source"],
            expected_arg_modes: &["o", "o", "o"],
            expected_all_arg_types: &["pg_catalog.jsonb", "pg_catalog.jsonb", "pg_catalog.text"],
            expected_body: &expected_bodies.resolve_actor,
            allow_non_owner_execute: false,
        },
        FunctionCheckSpec {
            name: "capture",
            pronargs: 0,
            expected_nargdefaults: 0,
            expected_arg_defaults: None,
            expected_rettype: "trigger",
            expected_retset: false,
            expected_volatile: "v",
            expected_arg_names: &[],
            expected_arg_modes: &[],
            expected_all_arg_types: &[],
            expected_body: &expected_bodies.capture,
            allow_non_owner_execute: false,
        },
        FunctionCheckSpec {
            name: "log_event",
            pronargs: 3,
            expected_nargdefaults: 2,
            expected_arg_defaults: Some("NULL::jsonb, NULL::jsonb"),
            expected_rettype: "uuid",
            expected_retset: false,
            expected_volatile: "v",
            expected_arg_names: &["action", "target", "data"],
            expected_arg_modes: &[],
            expected_all_arg_types: &[],
            expected_body: &expected_bodies.log_event,
            allow_non_owner_execute: true,
        },
    ];
    for spec in function_specs {
        check_function(tx, &options.audit_schema, spec, &mut findings).await?;
    }
    check_unexpected_function_overloads(tx, &options.audit_schema, &mut findings).await?;

    check_tables(tx, &options.audit_schema, &options.tables, &mut findings).await?;

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
    log_event: String,
}

impl ExpectedFunctionBodies {
    fn from_generator(audit_schema: &str) -> Self {
        let trigger_stmts = triggers::generate(&TriggerOptions::new(audit_schema, Vec::new()));
        let ddl_stmts = ddl::generate(&DdlOptions::new(audit_schema, Vec::new()));
        let prefix = format!("CREATE OR REPLACE FUNCTION {}.", quote_ident(audit_schema));
        let mut resolve_actor = None;
        let mut capture = None;
        let mut log_event = None;

        for stmt in trigger_stmts.iter().chain(ddl_stmts.iter()) {
            let Some(rest) = stmt.strip_prefix(&prefix) else {
                continue;
            };
            if rest.starts_with("resolve_actor(") {
                resolve_actor = extract_function_body(rest);
            } else if rest.starts_with("capture(") {
                capture = extract_function_body(rest);
            } else if rest.starts_with("log_event(") {
                log_event = extract_function_body(rest);
            }
        }

        Self {
            resolve_actor: resolve_actor
                .expect("triggers::generate must emit resolve_actor() with dollar-quoted body"),
            capture: capture
                .expect("triggers::generate must emit capture() with dollar-quoted body"),
            log_event: log_event
                .expect("ddl::generate must emit log_event() with dollar-quoted body"),
        }
    }
}

fn extract_function_body(sql: &str) -> Option<String> {
    let (_, after_as) = sql.split_once("\nAS $")?;
    let tag_end = after_as.find('$')?;
    let tag = format!("${}$", &after_as[..tag_end]);
    let start = sql.find(&tag)? + tag.len();
    let rest = &sql[start..];
    let end = rest.rfind(&tag)?;
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
#[allow(clippy::struct_excessive_bools)]
struct OwnerRoleRow {
    oid: i64,
    rolcanlogin: bool,
    rolsuper: bool,
    rolcreaterole: bool,
    rolcreatedb: bool,
    rolreplication: bool,
    rolbypassrls: bool,
    server_version_num: i32,
}

#[derive(Debug, FromRow)]
#[allow(clippy::struct_excessive_bools)]
struct OwnerMemberPg16Row {
    member_name: String,
    admin_option: bool,
    inherit_option: bool,
    set_option: bool,
    rolsuper: bool,
    rolcreaterole: bool,
}

#[derive(Debug, FromRow)]
struct OwnerMemberPg15Row {
    member_name: String,
    admin_option: bool,
}

#[derive(Debug, FromRow)]
struct OwnerIndirectAdminRow {
    via_role: String,
    grantee_name: String,
}

#[derive(Debug, FromRow)]
struct SessionRoleOverrideRow {
    role_scope: String,
    db_scope: String,
    setting: String,
}

#[derive(Debug, FromRow)]
struct AclGrantRow {
    grantee: String,
    privilege_type: String,
    is_grantable: bool,
}

#[derive(Debug, FromRow)]
struct OwnedObjectRow {
    oid: i64,
    schema_name: String,
    object_name: String,
    owner_name: String,
}

#[derive(Debug, FromRow)]
struct ChildPartitionRow {
    oid: i64,
    schema_name: String,
    object_name: String,
    owner_name: String,
    relkind: String,
}

#[derive(Debug, FromRow)]
struct HistoryTamperRow {
    schema: String,
    table: String,
    object: String,
    kind: String,
}

#[derive(Debug, FromRow)]
struct HistoryRlsRow {
    schema_name: String,
    table_name: String,
    relrowsecurity: bool,
    relforcerowsecurity: bool,
    policy_count: i64,
}

#[derive(Debug, FromRow)]
#[allow(clippy::struct_excessive_bools)]
struct HistoryIndexRow {
    index_name: String,
    access_method: String,
    indisvalid: bool,
    indisready: bool,
    indisprimary: bool,
    indisunique: bool,
    has_include_columns: bool,
    has_nondefault_options: bool,
    has_nondefault_opclass: bool,
    key_columns: Vec<String>,
    predicate: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ExpectedHistoryIndex {
    name: &'static str,
    access_method: &'static str,
    key_columns: &'static [&'static str],
    predicate: Option<&'static str>,
    is_unique: bool,
}

const EXPECTED_HISTORY_PRIMARY_KEY: ExpectedHistoryIndex = ExpectedHistoryIndex {
    name: "changes_pkey",
    access_method: "btree",
    key_columns: &["id", "occurred_at"],
    predicate: None,
    is_unique: true,
};

const EXPECTED_HISTORY_INDEXES: [ExpectedHistoryIndex; 3] = [
    ExpectedHistoryIndex {
        name: "changes_occurred_at_brin_idx",
        access_method: "brin",
        key_columns: &["occurred_at"],
        predicate: None,
        is_unique: false,
    },
    ExpectedHistoryIndex {
        name: "changes_xact_id_idx",
        access_method: "btree",
        key_columns: &["xact_id", "id"],
        predicate: None,
        is_unique: false,
    },
    ExpectedHistoryIndex {
        name: "changes_lookup_idx",
        access_method: "hash",
        key_columns: &["record_id"],
        predicate: Some("record_id IS NOT NULL"),
        is_unique: false,
    },
];

#[derive(Debug, FromRow)]
#[allow(clippy::struct_excessive_bools)]
struct ProcRow {
    oid: i64,
    prosrc: String,
    prosecdef: bool,
    proisstrict: bool,
    proleakproof: bool,
    provolatile: String,
    proparallel: String,
    prosupport: i64,
    langname: String,
    proconfig: Option<Vec<String>>,
    owner_name: String,
    rettype_schema: String,
    rettype_name: String,
    proretset: bool,
    pronargdefaults: i16,
    arg_defaults: Option<String>,
    arg_names: Vec<String>,
    arg_modes: Vec<String>,
    all_arg_types: Vec<String>,
}

#[derive(Debug, FromRow)]
struct UnexpectedOverloadRow {
    proname: String,
    identity_args: String,
}

struct FunctionCheckSpec<'a> {
    name: &'a str,
    pronargs: i16,
    expected_nargdefaults: i16,
    expected_arg_defaults: Option<&'a str>,
    expected_rettype: &'a str,
    expected_retset: bool,
    expected_volatile: &'a str,
    expected_arg_names: &'a [&'a str],
    expected_arg_modes: &'a [&'a str],
    expected_all_arg_types: &'a [&'a str],
    expected_body: &'a str,
    allow_non_owner_execute: bool,
}

fn push_owner_privilege_finding(findings: &mut Vec<Finding>, detail: String) {
    findings.push(Finding {
        kind: FindingKind::UnsafePrivileges,
        severity: Severity::Error,
        table: None,
        object: OWNER_ROLE.to_owned(),
        detail,
    });
}

async fn check_owner_role_security(
    tx: &mut Transaction<'_, Postgres>,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let row: Option<OwnerRoleRow> = sqlx::query_as(
        "SELECT \
           r.oid::bigint AS oid, \
           r.rolcanlogin, \
           r.rolsuper, \
           r.rolcreaterole, \
           r.rolcreatedb, \
           r.rolreplication, \
           r.rolbypassrls, \
           pg_catalog.current_setting('server_version_num')::int AS server_version_num \
         FROM pg_catalog.pg_roles r \
        WHERE r.rolname = $1",
    )
    .bind(OWNER_ROLE)
    .fetch_optional(&mut **tx)
    .await?;

    let Some(role) = row else {
        findings.push(Finding {
            kind: FindingKind::Missing,
            severity: Severity::Error,
            table: None,
            object: OWNER_ROLE.to_owned(),
            detail: format!("owner role `{OWNER_ROLE}` is missing"),
        });
        return Ok(());
    };

    let unsafe_attrs = unsafe_owner_role_attributes(&role);
    if !unsafe_attrs.is_empty() {
        push_owner_privilege_finding(
            findings,
            format!(
                "owner role `{OWNER_ROLE}` has unsafe attribute(s): {}",
                unsafe_attrs.join(", ")
            ),
        );
    }

    check_owner_role_memberships(tx, role.oid, role.server_version_num, findings).await?;
    check_owner_role_granted_roles(tx, role.oid, findings).await?;
    Ok(())
}

fn unsafe_owner_role_attributes(role: &OwnerRoleRow) -> Vec<&'static str> {
    let mut attrs = Vec::new();
    if role.rolcanlogin {
        attrs.push("LOGIN");
    }
    if role.rolsuper {
        attrs.push("SUPERUSER");
    }
    if role.rolcreaterole {
        attrs.push("CREATEROLE");
    }
    if role.rolcreatedb {
        attrs.push("CREATEDB");
    }
    if role.rolreplication {
        attrs.push("REPLICATION");
    }
    if role.rolbypassrls {
        attrs.push("BYPASSRLS");
    }
    attrs
}

async fn check_owner_role_memberships(
    tx: &mut Transaction<'_, Postgres>,
    owner_oid: i64,
    server_version_num: i32,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let mut unsafe_members: Vec<String> = if server_version_num >= 160_000 {
        let rows: Vec<OwnerMemberPg16Row> = sqlx::query_as(
            "SELECT \
               pg_catalog.pg_get_userbyid(m.member)::text AS member_name, \
               m.admin_option, \
               m.inherit_option, \
               m.set_option, \
               u.rolsuper, \
               u.rolcreaterole \
             FROM pg_catalog.pg_auth_members m \
             JOIN pg_catalog.pg_roles u ON u.oid = m.member \
            WHERE m.roleid = $1::pg_catalog.oid \
            ORDER BY 1, m.grantor",
        )
        .bind(owner_oid)
        .fetch_all(&mut **tx)
        .await?;
        let mut bad: Vec<String> = rows.iter().filter_map(format_unsafe_member_pg16).collect();
        let indirect: Vec<OwnerIndirectAdminRow> = sqlx::query_as(
            "WITH RECURSIVE admin_chain(root_role, member_oid, pure_admin_only) AS ( \
               SELECT \
                 m.member, \
                 up.member, \
                 (NOT up.inherit_option AND NOT up.set_option AND up.admin_option) \
               FROM pg_catalog.pg_auth_members m \
               JOIN pg_catalog.pg_roles admin_role ON admin_role.oid = m.member \
               JOIN pg_catalog.pg_auth_members up ON up.roleid = m.member \
              WHERE m.roleid = $1::pg_catalog.oid \
                AND NOT m.inherit_option \
                AND NOT m.set_option \
                AND m.admin_option \
                AND (admin_role.rolcreaterole OR admin_role.rolsuper) \
                AND (up.inherit_option OR up.set_option OR up.admin_option) \
               UNION \
               SELECT \
                 ac.root_role, \
                 up.member, \
                 (ac.pure_admin_only AND NOT up.inherit_option AND NOT up.set_option AND up.admin_option) \
               FROM admin_chain ac \
               JOIN pg_catalog.pg_auth_members up ON up.roleid = ac.member_oid \
              WHERE up.inherit_option OR up.set_option OR up.admin_option \
             ) \
             SELECT DISTINCT \
               pg_catalog.pg_get_userbyid(ac.root_role)::text AS via_role, \
               u.rolname::text AS grantee_name \
             FROM admin_chain ac \
             JOIN pg_catalog.pg_roles u ON u.oid = ac.member_oid \
            WHERE ac.member_oid <> ac.root_role \
              AND NOT u.rolsuper \
              AND NOT (ac.pure_admin_only AND u.rolcreaterole) \
            ORDER BY 2, 1",
        )
        .bind(owner_oid)
        .fetch_all(&mut **tx)
        .await?;
        for row in indirect {
            bad.push(format!(
                "{} (indirect ADMIN via {})",
                row.grantee_name, row.via_role
            ));
        }
        bad
    } else {
        let rows: Vec<OwnerMemberPg15Row> = sqlx::query_as(
            "SELECT \
               pg_catalog.pg_get_userbyid(m.member)::text AS member_name, \
               m.admin_option \
             FROM pg_catalog.pg_auth_members m \
            WHERE m.roleid = $1::pg_catalog.oid \
            ORDER BY 1, m.grantor",
        )
        .bind(owner_oid)
        .fetch_all(&mut **tx)
        .await?;
        rows.iter()
            .map(|r| {
                if r.admin_option {
                    format!("{} (MEMBER, ADMIN)", r.member_name)
                } else {
                    format!("{} (MEMBER)", r.member_name)
                }
            })
            .collect()
    };

    unsafe_members.sort();
    unsafe_members.dedup();

    if !unsafe_members.is_empty() {
        push_owner_privilege_finding(
            findings,
            format!(
                "owner role `{OWNER_ROLE}` has unsafe role membership(s): {}",
                unsafe_members.join(", ")
            ),
        );
    }
    Ok(())
}

fn format_unsafe_member_pg16(row: &OwnerMemberPg16Row) -> Option<String> {
    // On PostgreSQL 16+, a CREATEROLE (or superuser) role creating `auditstream_owner` with
    // `createrole_self_grant = ''` holds a mandatory ADMIN-only membership row without INHERIT
    // or SET. Any INHERIT/SET option, or ADMIN/MEMBER granted to a non-CREATEROLE non-superuser
    // role, is unsafe.
    let is_safe_creator_admin = !row.inherit_option
        && !row.set_option
        && row.admin_option
        && (row.rolcreaterole || row.rolsuper);
    if is_safe_creator_admin {
        return None;
    }

    let mut opts = Vec::new();
    if row.inherit_option {
        opts.push("INHERIT");
    }
    if row.set_option {
        opts.push("SET");
    }
    if row.admin_option {
        opts.push("ADMIN");
    }
    if opts.is_empty() {
        opts.push("MEMBER");
    }
    Some(format!("{} ({})", row.member_name, opts.join(", ")))
}

async fn check_owner_role_granted_roles(
    tx: &mut Transaction<'_, Postgres>,
    owner_oid: i64,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let granted_roles: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT pg_catalog.pg_get_userbyid(m.roleid)::text AS role_name \
           FROM pg_catalog.pg_auth_members m \
          WHERE m.member = $1::pg_catalog.oid \
          ORDER BY 1",
    )
    .bind(owner_oid)
    .fetch_all(&mut **tx)
    .await?;

    if !granted_roles.is_empty() {
        push_owner_privilege_finding(
            findings,
            format!(
                "owner role `{OWNER_ROLE}` is unexpectedly a member of role(s): {}",
                granted_roles.join(", ")
            ),
        );
    }
    Ok(())
}

fn push_session_replication_finding(findings: &mut Vec<Finding>, detail: String) {
    findings.push(Finding {
        kind: FindingKind::UnsafePrivileges,
        severity: Severity::Error,
        table: None,
        object: "session_replication_role".to_owned(),
        detail,
    });
}

async fn check_session_replication_role_acl(
    tx: &mut Transaction<'_, Postgres>,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let unsafe_grants: Vec<AclGrantRow> = sqlx::query_as(
        "SELECT \
           CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
           a.privilege_type::text AS privilege_type, \
           a.is_grantable \
         FROM pg_catalog.pg_parameter_acl pa \
         CROSS JOIN LATERAL pg_catalog.aclexplode(pa.paracl) AS a \
         LEFT JOIN pg_catalog.pg_roles r ON r.oid = a.grantee \
        WHERE pg_catalog.lower(pa.parname) = 'session_replication_role' \
          AND pa.paracl IS NOT NULL \
          AND ( \
            a.grantee = 0 \
            OR NOT COALESCE(r.rolsuper, false) \
            OR EXISTS ( \
              SELECT 1 FROM pg_catalog.pg_roles u \
               WHERE NOT u.rolsuper \
                 AND pg_catalog.pg_has_role(u.oid, a.grantee, 'USAGE') \
            ) \
          ) \
        ORDER BY 1, 2",
    )
    .fetch_all(&mut **tx)
    .await?;

    if !unsafe_grants.is_empty() {
        let summary = format_acl_grants(&unsafe_grants);
        push_session_replication_finding(
            findings,
            format!(
                "parameter `session_replication_role` grants unsafe non-superuser privilege(s) (allows disabling audit triggers): {summary}"
            ),
        );
    }

    let active_role: String =
        sqlx::query_scalar("SELECT pg_catalog.current_setting('session_replication_role')::text")
            .fetch_one(&mut **tx)
            .await?;
    if active_role.eq_ignore_ascii_case("replica") {
        push_session_replication_finding(
            findings,
            format!(
                "active session `session_replication_role` is `{active_role}`, which disables origin audit triggers"
            ),
        );
    }

    let overrides: Vec<SessionRoleOverrideRow> = sqlx::query_as(
        "SELECT \
           CASE WHEN s.setrole = 0 THEN 'ALL' ELSE pg_catalog.pg_get_userbyid(s.setrole)::text END AS role_scope, \
           CASE WHEN s.setdatabase = 0 THEN 'ALL' ELSE d.datname::text END AS db_scope, \
           cfg AS setting \
         FROM pg_catalog.pg_db_role_setting s \
         LEFT JOIN pg_catalog.pg_database d ON d.oid = s.setdatabase \
         CROSS JOIN LATERAL pg_catalog.unnest(s.setconfig) AS cfg \
        WHERE ( \
          s.setdatabase = 0 \
          OR s.setdatabase = ( \
            SELECT cur.oid FROM pg_catalog.pg_database cur \
             WHERE cur.datname = pg_catalog.current_database() \
          ) \
        ) \
          AND pg_catalog.lower(pg_catalog.split_part(cfg, '=', 1)) = 'session_replication_role' \
          AND pg_catalog.lower(pg_catalog.btrim(pg_catalog.split_part(cfg, '=', 2), E' \\t''\"')) = 'replica' \
        ORDER BY 1, 2, 3",
    )
    .fetch_all(&mut **tx)
    .await?;

    if !overrides.is_empty() {
        let summary: Vec<String> = overrides
            .iter()
            .map(|o| format!("role={} db={} ({})", o.role_scope, o.db_scope, o.setting))
            .collect();
        push_session_replication_finding(
            findings,
            format!(
                "database/role default setting sets `session_replication_role` to `replica` (disables audit triggers): {}",
                summary.join(", ")
            ),
        );
    }
    Ok(())
}

async fn check_schema_security(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT pg_catalog.pg_get_userbyid(nspowner)::text \
           FROM pg_catalog.pg_namespace \
          WHERE nspname = $1",
    )
    .bind(audit_schema)
    .fetch_optional(&mut **tx)
    .await?;

    let Some(owner_name) = owner else {
        return Ok(());
    };

    if owner_name != OWNER_ROLE {
        findings.push(Finding {
            kind: FindingKind::InvalidOwnership,
            severity: Severity::Error,
            table: None,
            object: audit_schema.to_owned(),
            detail: format!(
                "audit schema `{audit_schema}` is owned by `{owner_name}`, expected `{OWNER_ROLE}`"
            ),
        });
    }

    let unsafe_grants: Vec<AclGrantRow> = sqlx::query_as(
        "SELECT \
           CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
           a.privilege_type::text AS privilege_type, \
           a.is_grantable \
         FROM pg_catalog.pg_namespace n \
         CROSS JOIN LATERAL pg_catalog.aclexplode( \
           COALESCE(n.nspacl, pg_catalog.acldefault('n', n.nspowner)) \
         ) AS a \
        WHERE n.nspname = $1 \
          AND a.grantee <> n.nspowner \
          AND (a.grantee = 0 OR a.privilege_type <> 'USAGE' OR a.is_grantable) \
        ORDER BY 1, 2",
    )
    .bind(audit_schema)
    .fetch_all(&mut **tx)
    .await?;

    push_unsafe_acl_finding(findings, None, audit_schema, "audit schema", &unsafe_grants);
    Ok(())
}

async fn check_history_security(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<bool, sqlx::Error> {
    check_parent_changes_security(tx, audit_schema, findings).await?;
    let has_foreign = check_child_partitions_security(tx, audit_schema, findings).await?;
    check_identity_sequences_security(tx, audit_schema, findings).await?;
    Ok(has_foreign)
}

async fn check_parent_changes_security(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let row: Option<OwnedObjectRow> = sqlx::query_as(
        "SELECT \
           c.oid::bigint AS oid, \
           n.nspname AS schema_name, \
           c.relname AS object_name, \
           pg_catalog.pg_get_userbyid(c.relowner)::text AS owner_name \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
        WHERE n.nspname = $1 \
          AND c.relname = 'changes' \
          AND c.relkind IN ('r', 'p')",
    )
    .bind(audit_schema)
    .fetch_optional(&mut **tx)
    .await?;

    let Some(parent) = row else {
        return Ok(());
    };
    let object = format!("{}.{}", parent.schema_name, parent.object_name);
    if parent.owner_name != OWNER_ROLE {
        findings.push(Finding {
            kind: FindingKind::InvalidOwnership,
            severity: Severity::Error,
            table: Some(object.clone()),
            object: object.clone(),
            detail: format!(
                "history table `{object}` is owned by `{}`, expected `{OWNER_ROLE}`",
                parent.owner_name
            ),
        });
    }

    let unsafe_grants: Vec<AclGrantRow> = sqlx::query_as(
        "SELECT grantee, privilege_type, is_grantable FROM ( \
           SELECT \
             CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
             a.privilege_type::text AS privilege_type, \
             a.is_grantable \
           FROM pg_catalog.pg_class c \
           CROSS JOIN LATERAL pg_catalog.aclexplode( \
             COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner)) \
           ) AS a \
          WHERE c.oid = $1::pg_catalog.oid \
            AND a.grantee <> c.relowner \
            AND (a.grantee = 0 OR a.privilege_type <> 'SELECT' OR a.is_grantable) \
           UNION ALL \
           SELECT \
             CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
             a.privilege_type::text || ' (' || att.attname || ')' AS privilege_type, \
             a.is_grantable \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_attribute att \
             ON att.attrelid = c.oid \
            AND att.attnum > 0 \
            AND NOT att.attisdropped \
            AND att.attacl IS NOT NULL \
           CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a \
          WHERE c.oid = $1::pg_catalog.oid \
            AND a.grantee <> c.relowner \
            AND (a.grantee = 0 OR a.privilege_type <> 'SELECT' OR a.is_grantable) \
         ) AS all_unsafe \
         ORDER BY 1, 2",
    )
    .bind(parent.oid)
    .fetch_all(&mut **tx)
    .await?;

    push_unsafe_acl_finding(
        findings,
        Some(object.clone()),
        &object,
        "history table",
        &unsafe_grants,
    );
    Ok(())
}

async fn check_child_partitions_security(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<bool, sqlx::Error> {
    let children: Vec<ChildPartitionRow> = sqlx::query_as(
        "WITH RECURSIVE tree(relid) AS ( \
           SELECT i.inhrelid \
             FROM pg_catalog.pg_inherits i \
             JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent \
             JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace \
            WHERE pn.nspname = $1 \
              AND parent.relname = 'changes' \
           UNION \
           SELECT i.inhrelid \
             FROM tree \
             JOIN pg_catalog.pg_inherits i ON i.inhparent = tree.relid \
         ) \
         SELECT \
           c.oid::bigint AS oid, \
           cn.nspname AS schema_name, \
           c.relname AS object_name, \
           pg_catalog.pg_get_userbyid(c.relowner)::text AS owner_name, \
           c.relkind::text AS relkind \
         FROM tree \
         JOIN pg_catalog.pg_class c ON c.oid = tree.relid \
         JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace \
        ORDER BY cn.nspname, c.relname",
    )
    .bind(audit_schema)
    .fetch_all(&mut **tx)
    .await?;

    let mut has_foreign = false;
    for child in children {
        let object = format!("{}.{}", child.schema_name, child.object_name);
        if child.relkind == "f" {
            has_foreign = true;
        }
        if child.relkind != "r" {
            findings.push(Finding {
                kind: FindingKind::Outdated,
                severity: Severity::Error,
                table: Some(object.clone()),
                object: object.clone(),
                detail: format!(
                    "history partition `{object}` has relkind '{}', expected regular table ('r')",
                    child.relkind
                ),
            });
        }
        if child.owner_name != OWNER_ROLE {
            findings.push(Finding {
                kind: FindingKind::InvalidOwnership,
                severity: Severity::Error,
                table: Some(object.clone()),
                object: object.clone(),
                detail: format!(
                    "history partition `{object}` is owned by `{}`, expected `{OWNER_ROLE}`",
                    child.owner_name
                ),
            });
        }

        let unsafe_grants: Vec<AclGrantRow> = sqlx::query_as(
            "SELECT grantee, privilege_type, is_grantable FROM ( \
               SELECT \
                 CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
                 a.privilege_type::text AS privilege_type, \
                 a.is_grantable \
               FROM pg_catalog.pg_class c \
               CROSS JOIN LATERAL pg_catalog.aclexplode( \
                 COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner)) \
               ) AS a \
              WHERE c.oid = $1::pg_catalog.oid \
                AND a.grantee <> c.relowner \
               UNION ALL \
               SELECT \
                 CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
                 a.privilege_type::text || ' (' || att.attname || ')' AS privilege_type, \
                 a.is_grantable \
               FROM pg_catalog.pg_class c \
               JOIN pg_catalog.pg_attribute att \
                 ON att.attrelid = c.oid \
                AND att.attnum > 0 \
                AND NOT att.attisdropped \
                AND att.attacl IS NOT NULL \
               CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a \
              WHERE c.oid = $1::pg_catalog.oid \
                AND a.grantee <> c.relowner \
             ) AS all_unsafe \
             ORDER BY 1, 2",
        )
        .bind(child.oid)
        .fetch_all(&mut **tx)
        .await?;

        push_unsafe_acl_finding(
            findings,
            Some(object.clone()),
            &object,
            "history partition",
            &unsafe_grants,
        );
    }

    Ok(has_foreign)
}

async fn check_identity_sequences_security(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let sequences: Vec<OwnedObjectRow> = sqlx::query_as(
        "SELECT \
           s.oid::bigint AS oid, \
           sn.nspname AS schema_name, \
           s.relname AS object_name, \
           pg_catalog.pg_get_userbyid(s.relowner)::text AS owner_name \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_depend d \
           ON d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass \
          AND d.refobjid = c.oid \
          AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass \
          AND d.deptype IN ('a', 'i') \
         JOIN pg_catalog.pg_class s ON s.oid = d.objid AND s.relkind = 'S' \
         JOIN pg_catalog.pg_namespace sn ON sn.oid = s.relnamespace \
        WHERE n.nspname = $1 \
          AND c.relname = 'changes' \
        ORDER BY sn.nspname, s.relname",
    )
    .bind(audit_schema)
    .fetch_all(&mut **tx)
    .await?;

    for seq in sequences {
        let object = format!("{}.{}", seq.schema_name, seq.object_name);
        if seq.owner_name != OWNER_ROLE {
            findings.push(Finding {
                kind: FindingKind::InvalidOwnership,
                severity: Severity::Error,
                table: None,
                object: object.clone(),
                detail: format!(
                    "history identity sequence `{object}` is owned by `{}`, expected `{OWNER_ROLE}`",
                    seq.owner_name
                ),
            });
        }

        let unsafe_grants: Vec<AclGrantRow> = sqlx::query_as(
            "SELECT \
               CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
               a.privilege_type::text AS privilege_type, \
               a.is_grantable \
             FROM pg_catalog.pg_class s \
             CROSS JOIN LATERAL pg_catalog.aclexplode( \
               COALESCE(s.relacl, pg_catalog.acldefault('s', s.relowner)) \
             ) AS a \
            WHERE s.oid = $1::pg_catalog.oid \
              AND a.grantee <> s.relowner \
            ORDER BY 1, 2",
        )
        .bind(seq.oid)
        .fetch_all(&mut **tx)
        .await?;

        push_unsafe_acl_finding(
            findings,
            None,
            &object,
            "history identity sequence",
            &unsafe_grants,
        );
    }

    Ok(())
}

async fn check_history_tampering(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<bool, sqlx::Error> {
    check_history_triggers_and_rules(tx, audit_schema, findings).await?;
    check_history_rls(tx, audit_schema, findings).await
}

async fn check_history_triggers_and_rules(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let rows: Vec<HistoryTamperRow> = sqlx::query_as(with_history_tree!(
        "SELECT \
           cn.nspname::text AS schema, \
           c.relname::text AS table, \
           t.tgname::text AS object, \
           'trigger'::text AS kind \
         FROM tree \
         JOIN pg_catalog.pg_class c ON c.oid = tree.relid \
         JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace \
         JOIN pg_catalog.pg_trigger t ON t.tgrelid = c.oid \
        WHERE t.tgparentid = 0 \
         UNION ALL \
         SELECT \
           cn.nspname::text AS schema, \
           c.relname::text AS table, \
           r.rulename::text AS object, \
           'rule'::text AS kind \
         FROM tree \
         JOIN pg_catalog.pg_class c ON c.oid = tree.relid \
         JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace \
         JOIN pg_catalog.pg_rewrite r ON r.ev_class = c.oid \
        WHERE r.rulename <> '_RETURN' \
        ORDER BY 1, 2, 4, 3"
    ))
    .bind(audit_schema)
    .fetch_all(&mut **tx)
    .await?;

    for row in rows {
        let fq_rel = format!("{}.{}", row.schema, row.table);
        let detail = if row.kind == "trigger" {
            format!(
                "unexpected trigger `{}` on history table `{fq_rel}` can alter or drop audit rows",
                row.object
            )
        } else {
            format!(
                "unexpected rewrite rule `{}` on history table `{fq_rel}` can suppress or alter audit rows",
                row.object
            )
        };
        findings.push(Finding {
            kind: FindingKind::TamperedHistory,
            severity: Severity::Error,
            table: Some(fq_rel.clone()),
            object: format!("{fq_rel}.{}", row.object),
            detail,
        });
    }
    Ok(())
}

async fn check_history_rls(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<bool, sqlx::Error> {
    let rows: Vec<HistoryRlsRow> = sqlx::query_as(with_history_tree!(
        "SELECT \
           cn.nspname AS schema_name, \
           c.relname AS table_name, \
           c.relrowsecurity, \
           c.relforcerowsecurity, \
           (SELECT pg_catalog.count(*)::bigint FROM pg_catalog.pg_policy pol WHERE pol.polrelid = c.oid) AS policy_count \
         FROM tree \
         JOIN pg_catalog.pg_class c ON c.oid = tree.relid \
         JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace \
        WHERE c.relrowsecurity \
           OR c.relforcerowsecurity \
           OR EXISTS (SELECT 1 FROM pg_catalog.pg_policy pol WHERE pol.polrelid = c.oid) \
        ORDER BY cn.nspname, c.relname"
    ))
    .bind(audit_schema)
    .fetch_all(&mut **tx)
    .await?;

    let has_rls = !rows.is_empty();
    for row in rows {
        let fq_rel = format!("{}.{}", row.schema_name, row.table_name);
        let mut flags = Vec::new();
        if row.relrowsecurity {
            flags.push("ENABLE ROW LEVEL SECURITY".to_owned());
        }
        if row.relforcerowsecurity {
            flags.push("FORCE ROW LEVEL SECURITY".to_owned());
        }
        if row.policy_count > 0 {
            flags.push(format!("{} RLS policy/policies", row.policy_count));
        }
        findings.push(Finding {
            kind: FindingKind::TamperedHistory,
            severity: Severity::Error,
            table: Some(fq_rel.clone()),
            object: fq_rel.clone(),
            detail: format!(
                "unexpected Row-Level Security on history table `{fq_rel}` ({})",
                flags.join(", ")
            ),
        });
    }
    Ok(has_rls)
}

async fn check_history_indexes(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let expected_names: Vec<&str> = EXPECTED_HISTORY_INDEXES.iter().map(|i| i.name).collect();
    let rows: Vec<HistoryIndexRow> = sqlx::query_as(
        "SELECT \
           ic.relname AS index_name, \
           am.amname AS access_method, \
           i.indisvalid, \
           i.indisready, \
           i.indisprimary, \
           i.indisunique, \
           i.indnatts <> i.indnkeyatts AS has_include_columns, \
           EXISTS ( \
             SELECT 1 FROM pg_catalog.unnest(i.indoption::pg_catalog.int2[]) AS opt \
              WHERE opt <> 0 \
           ) AS has_nondefault_options, \
           EXISTS ( \
             SELECT 1 FROM pg_catalog.unnest(i.indclass::pg_catalog.oid[]) AS cls \
             JOIN pg_catalog.pg_opclass oc ON oc.oid = cls \
              WHERE NOT oc.opcdefault \
           ) AS has_nondefault_opclass, \
           COALESCE( \
             ( \
               SELECT pg_catalog.array_agg( \
                 CASE \
                   WHEN k.attnum = 0 THEN '<expr>' \
                   ELSE COALESCE(a.attname::text, '<unknown>') \
                 END \
                 ORDER BY k.ord \
               ) \
               FROM pg_catalog.unnest(i.indkey::pg_catalog.int2[]) WITH ORDINALITY AS k(attnum, ord) \
               LEFT JOIN pg_catalog.pg_attribute a \
                 ON a.attrelid = i.indrelid \
                AND a.attnum = k.attnum \
                AND NOT a.attisdropped \
               WHERE k.ord <= i.indnkeyatts \
             ), \
             ARRAY[]::text[] \
           ) AS key_columns, \
           pg_catalog.pg_get_expr(i.indpred, i.indrelid) AS predicate \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class tbl ON tbl.oid = i.indrelid \
         JOIN pg_catalog.pg_namespace tn ON tn.oid = tbl.relnamespace \
         JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_catalog.pg_namespace in_ns ON in_ns.oid = ic.relnamespace \
         JOIN pg_catalog.pg_am am ON am.oid = ic.relam \
        WHERE tn.nspname = $1 \
          AND tbl.relname = 'changes' \
          AND tbl.relkind IN ('r', 'p') \
          AND in_ns.nspname = $1 \
          AND (i.indisprimary OR ic.relname = ANY($2)) \
        ORDER BY ic.relname",
    )
    .bind(audit_schema)
    .bind(&expected_names)
    .fetch_all(&mut **tx)
    .await?;

    let history_table = format!("{audit_schema}.changes");
    let pk_row = rows.iter().find(|row| row.indisprimary);
    check_expected_history_index(
        pk_row,
        EXPECTED_HISTORY_PRIMARY_KEY,
        audit_schema,
        &history_table,
        findings,
    );

    let by_name: HashMap<&str, &HistoryIndexRow> = rows
        .iter()
        .map(|row| (row.index_name.as_str(), row))
        .collect();
    for expected in EXPECTED_HISTORY_INDEXES {
        check_expected_history_index(
            by_name.get(expected.name).copied(),
            expected,
            audit_schema,
            &history_table,
            findings,
        );
    }
    Ok(())
}

fn check_expected_history_index(
    actual: Option<&HistoryIndexRow>,
    expected: ExpectedHistoryIndex,
    audit_schema: &str,
    history_table: &str,
    findings: &mut Vec<Finding>,
) {
    let object = format!("{audit_schema}.{}", expected.name);
    let Some(actual) = actual else {
        findings.push(Finding {
            kind: FindingKind::Missing,
            severity: Severity::Error,
            table: Some(history_table.to_owned()),
            object: object.clone(),
            detail: format!("required history index `{object}` on `{history_table}` is missing"),
        });
        return;
    };

    let diffs = diff_history_index(actual, expected);
    if !diffs.is_empty() {
        findings.push(Finding {
            kind: FindingKind::Outdated,
            severity: Severity::Error,
            table: Some(history_table.to_owned()),
            object: object.clone(),
            detail: format!(
                "history index `{object}` on `{history_table}` is invalid or outdated: {}",
                diffs.join("; ")
            ),
        });
    }
}

fn diff_history_index(actual: &HistoryIndexRow, expected: ExpectedHistoryIndex) -> Vec<String> {
    let mut diffs = Vec::new();
    if !actual.indisvalid || !actual.indisready {
        diffs.push("index is invalid (not valid across all partitions)".to_owned());
    }
    if actual.access_method != expected.access_method {
        diffs.push(format!(
            "access method is `{}`, expected `{}`",
            actual.access_method, expected.access_method
        ));
    }
    let expected_cols: Vec<&str> = expected.key_columns.to_vec();
    let actual_cols: Vec<&str> = actual.key_columns.iter().map(String::as_str).collect();
    if actual_cols != expected_cols {
        diffs.push(format!(
            "key columns are ({}), expected ({})",
            actual_cols.join(", "),
            expected_cols.join(", ")
        ));
    }
    let actual_pred = actual.predicate.as_deref().map(normalize_sql_predicate);
    let expected_pred = expected.predicate.map(normalize_sql_predicate);
    if actual_pred != expected_pred {
        diffs.push(format!(
            "predicate is {actual_pred:?}, expected {expected_pred:?}"
        ));
    }
    diff_history_index_flags(actual, expected.is_unique, &mut diffs);
    diffs
}

fn diff_history_index_flags(
    actual: &HistoryIndexRow,
    expected_unique: bool,
    diffs: &mut Vec<String>,
) {
    if actual.indisunique != expected_unique {
        diffs.push(if expected_unique {
            "index is missing UNIQUE constraint".to_owned()
        } else {
            "index is unexpectedly UNIQUE".to_owned()
        });
    }
    if actual.has_include_columns {
        diffs.push("index has unexpected INCLUDE columns".to_owned());
    }
    if actual.has_nondefault_options {
        diffs.push("index has non-default sort/nulls ordering".to_owned());
    }
    if actual.has_nondefault_opclass {
        diffs.push("index uses a non-default operator class".to_owned());
    }
}

fn normalize_sql_predicate(raw: &str) -> String {
    let mut trimmed = raw.trim();
    while has_outer_matching_parens(trimmed) {
        trimmed = trimmed[1..trimmed.len() - 1].trim();
    }
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_outer_matching_parens(expr: &str) -> bool {
    if !(expr.starts_with('(') && expr.ends_with(')')) {
        return false;
    }
    let mut depth: usize = 0;
    for (idx, ch) in expr.char_indices() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth = depth.saturating_sub(1);
            if depth == 0 && idx < expr.len() - 1 {
                return false;
            }
        }
    }
    depth == 0
}

fn format_acl_grants(grants: &[AclGrantRow]) -> String {
    grants
        .iter()
        .map(|g| {
            if g.is_grantable {
                format!("{} ({} WITH GRANT OPTION)", g.grantee, g.privilege_type)
            } else {
                format!("{} ({})", g.grantee, g.privilege_type)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_unsafe_acl_finding(
    findings: &mut Vec<Finding>,
    table: Option<String>,
    object: &str,
    label: &str,
    grants: &[AclGrantRow],
) {
    if grants.is_empty() {
        return;
    }
    findings.push(Finding {
        kind: FindingKind::UnsafePrivileges,
        severity: Severity::Error,
        table,
        object: object.to_owned(),
        detail: format!(
            "{label} `{object}` has unsafe non-owner privilege(s): {}",
            format_acl_grants(grants)
        ),
    });
}

async fn fetch_function_row(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    spec: &FunctionCheckSpec<'_>,
) -> Result<Option<ProcRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT \
           p.oid::bigint AS oid, \
           p.prosrc, \
           p.prosecdef, \
           p.proisstrict, \
           p.proleakproof, \
           p.provolatile::text AS provolatile, \
           p.proparallel::text AS proparallel, \
           p.prosupport::bigint AS prosupport, \
           l.lanname AS langname, \
           p.proconfig, \
           pg_catalog.pg_get_userbyid(p.proowner)::text AS owner_name, \
           tn.nspname::text AS rettype_schema, \
           t.typname::text AS rettype_name, \
           p.proretset, \
           p.pronargdefaults, \
           pg_catalog.pg_get_expr(p.proargdefaults, 0) AS arg_defaults, \
           COALESCE(p.proargnames, ARRAY[]::text[]) AS arg_names, \
           COALESCE( \
             ( \
               SELECT pg_catalog.array_agg(m::text ORDER BY u.ord) \
                 FROM pg_catalog.unnest(p.proargmodes) WITH ORDINALITY AS u(m, ord) \
             ), \
             ARRAY[]::text[] \
           ) AS arg_modes, \
           COALESCE( \
             ( \
               SELECT pg_catalog.array_agg(tn2.nspname::text || '.' || t2.typname::text ORDER BY k.ord) \
                 FROM pg_catalog.unnest(p.proallargtypes) WITH ORDINALITY AS k(typid, ord) \
                 JOIN pg_catalog.pg_type t2 ON t2.oid = k.typid \
                 JOIN pg_catalog.pg_namespace tn2 ON tn2.oid = t2.typnamespace \
             ), \
             ARRAY[]::text[] \
           ) AS all_arg_types \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
         JOIN pg_catalog.pg_type t ON t.oid = p.prorettype \
         JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace \
        WHERE n.nspname = $1 \
          AND p.proname = $2 \
          AND p.prokind = 'f' \
          AND p.pronargs = $3 \
          AND ( \
            $3 = 0 \
            OR p.proargtypes = (ARRAY[ \
              'pg_catalog.text'::pg_catalog.regtype::pg_catalog.oid, \
              'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid, \
              'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid \
            ])::pg_catalog.oidvector \
          )",
    )
    .bind(audit_schema)
    .bind(spec.name)
    .bind(spec.pronargs)
    .fetch_optional(&mut **tx)
    .await
}

async fn check_function(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    spec: FunctionCheckSpec<'_>,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let object = if spec.pronargs == 0 {
        format!("{audit_schema}.{}()", spec.name)
    } else {
        format!("{audit_schema}.{}(text, jsonb, jsonb)", spec.name)
    };
    let Some(proc_row) = fetch_function_row(tx, audit_schema, &spec).await? else {
        findings.push(Finding {
            kind: FindingKind::Missing,
            severity: Severity::Error,
            table: None,
            object: object.clone(),
            detail: format!("function `{object}` is missing"),
        });
        return Ok(());
    };

    let diffs = diff_function_row(&proc_row, &spec);
    if !diffs.is_empty() {
        findings.push(Finding {
            kind: FindingKind::Outdated,
            severity: Severity::Error,
            table: None,
            object: object.clone(),
            detail: format!("function `{object}` is outdated: {}", diffs.join("; ")),
        });
    }

    if proc_row.owner_name != OWNER_ROLE {
        findings.push(Finding {
            kind: FindingKind::InvalidOwnership,
            severity: Severity::Error,
            table: None,
            object: object.clone(),
            detail: format!(
                "function `{object}` is owned by `{}`, expected `{OWNER_ROLE}`",
                proc_row.owner_name
            ),
        });
    }

    check_function_acls(
        tx,
        proc_row.oid,
        &object,
        spec.allow_non_owner_execute,
        findings,
    )
    .await?;

    Ok(())
}

async fn check_unexpected_function_overloads(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let overloads: Vec<UnexpectedOverloadRow> = sqlx::query_as(
        "SELECT \
           p.proname::text AS proname, \
           pg_catalog.pg_get_function_identity_arguments(p.oid) AS identity_args \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
        WHERE n.nspname = $1 \
          AND p.proname IN ('resolve_actor', 'capture', 'log_event') \
          AND NOT ( \
            p.prokind = 'f' \
            AND ( \
              (p.proname IN ('resolve_actor', 'capture') AND p.pronargs = 0) \
              OR ( \
                p.proname = 'log_event' \
                AND p.pronargs = 3 \
                AND p.proargtypes = (ARRAY[ \
                  'pg_catalog.text'::pg_catalog.regtype::pg_catalog.oid, \
                  'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid, \
                  'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid \
                ])::pg_catalog.oidvector \
              ) \
            ) \
          ) \
        ORDER BY p.proname, 2",
    )
    .bind(audit_schema)
    .fetch_all(&mut **tx)
    .await?;

    for row in overloads {
        let object = format!("{audit_schema}.{}({})", row.proname, row.identity_args);
        findings.push(Finding {
            kind: FindingKind::TamperedHistory,
            severity: Severity::Error,
            table: None,
            object: object.clone(),
            detail: format!(
                "unexpected function overload `{object}` in audit schema `{audit_schema}` can shadow or disrupt Auditstream functions"
            ),
        });
    }
    Ok(())
}

fn diff_function_row(proc_row: &ProcRow, spec: &FunctionCheckSpec<'_>) -> Vec<String> {
    let mut diffs = Vec::new();
    if proc_row.prosrc.trim() != spec.expected_body.trim() {
        diffs.push("function body differs from generated SQL".to_owned());
    }
    if !proc_row.prosecdef {
        diffs.push("function is missing SECURITY DEFINER".to_owned());
    }
    if proc_row.langname != "plpgsql" {
        diffs.push("function language is not plpgsql".to_owned());
    }
    diff_function_attributes(proc_row, spec, &mut diffs);
    diff_function_signature(proc_row, spec, &mut diffs);
    diff_function_proconfig(proc_row.proconfig.as_deref(), &mut diffs);
    diffs
}

fn diff_function_attributes(
    proc_row: &ProcRow,
    spec: &FunctionCheckSpec<'_>,
    diffs: &mut Vec<String>,
) {
    if proc_row.proisstrict {
        diffs.push("function is STRICT (silently returns NULL on NULL arguments)".to_owned());
    }
    if proc_row.provolatile != spec.expected_volatile {
        diffs.push(format!(
            "volatility is `{}`, expected `{}`",
            proc_row.provolatile, spec.expected_volatile
        ));
    }
    if proc_row.proparallel != "u" {
        diffs.push(format!(
            "parallel safety is `{}`, expected `u` (PARALLEL UNSAFE)",
            proc_row.proparallel
        ));
    }
    if proc_row.proleakproof {
        diffs.push("function is unexpectedly LEAKPROOF".to_owned());
    }
    if proc_row.prosupport != 0 {
        diffs.push("function has an unexpected planner SUPPORT function".to_owned());
    }
}

fn diff_function_signature(
    proc_row: &ProcRow,
    spec: &FunctionCheckSpec<'_>,
    diffs: &mut Vec<String>,
) {
    if proc_row.rettype_schema != "pg_catalog"
        || proc_row.rettype_name != spec.expected_rettype
        || proc_row.proretset != spec.expected_retset
    {
        diffs.push(format!(
            "return type is {}{}.{}, expected {}pg_catalog.{}",
            if proc_row.proretset { "SETOF " } else { "" },
            proc_row.rettype_schema,
            proc_row.rettype_name,
            if spec.expected_retset { "SETOF " } else { "" },
            spec.expected_rettype
        ));
    }
    if proc_row.pronargdefaults != spec.expected_nargdefaults {
        diffs.push(format!(
            "default argument count is {}, expected {}",
            proc_row.pronargdefaults, spec.expected_nargdefaults
        ));
    }
    let actual_defaults = proc_row
        .arg_defaults
        .as_deref()
        .map(normalize_sql_predicate);
    let expected_defaults = spec.expected_arg_defaults.map(normalize_sql_predicate);
    if actual_defaults != expected_defaults {
        diffs.push(format!(
            "default argument expressions are {actual_defaults:?}, expected {expected_defaults:?}"
        ));
    }
    let arg_names: Vec<&str> = proc_row.arg_names.iter().map(String::as_str).collect();
    if arg_names != spec.expected_arg_names {
        diffs.push(format!(
            "parameter names are {arg_names:?}, expected {:?}",
            spec.expected_arg_names
        ));
    }
    let arg_modes: Vec<&str> = proc_row.arg_modes.iter().map(String::as_str).collect();
    if arg_modes != spec.expected_arg_modes {
        diffs.push(format!(
            "parameter modes are {arg_modes:?}, expected {:?}",
            spec.expected_arg_modes
        ));
    }
    let all_arg_types: Vec<&str> = proc_row.all_arg_types.iter().map(String::as_str).collect();
    if all_arg_types != spec.expected_all_arg_types {
        diffs.push(format!(
            "parameter types are {all_arg_types:?}, expected {:?}",
            spec.expected_all_arg_types
        ));
    }
}

fn diff_function_proconfig(proconfig: Option<&[String]>, diffs: &mut Vec<String>) {
    let entries = proconfig.unwrap_or(&[]);
    let has_expected_search_path = entries.iter().any(|entry| entry == EXPECTED_SEARCH_PATH);
    if !has_expected_search_path {
        diffs.push("function is missing `SET search_path = pg_catalog, pg_temp`".to_owned());
    }
    let unexpected: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.as_str() != EXPECTED_SEARCH_PATH)
        .map(String::as_str)
        .collect();
    if !unexpected.is_empty() {
        diffs.push(format!(
            "function has unexpected configuration parameter(s): {}",
            unexpected.join(", ")
        ));
    }
}

async fn check_function_acls(
    tx: &mut Transaction<'_, Postgres>,
    proc_oid: i64,
    object: &str,
    allow_non_owner_execute: bool,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let query = if allow_non_owner_execute {
        "SELECT \
           CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
           a.privilege_type::text AS privilege_type, \
           a.is_grantable \
         FROM pg_catalog.pg_proc p \
         CROSS JOIN LATERAL pg_catalog.aclexplode( \
           COALESCE(p.proacl, pg_catalog.acldefault('f', p.proowner)) \
         ) AS a \
        WHERE p.oid = $1::pg_catalog.oid \
          AND a.grantee <> p.proowner \
          AND (a.grantee = 0 OR a.is_grantable) \
        ORDER BY 1, 2"
    } else {
        "SELECT \
           CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
           a.privilege_type::text AS privilege_type, \
           a.is_grantable \
         FROM pg_catalog.pg_proc p \
         CROSS JOIN LATERAL pg_catalog.aclexplode( \
           COALESCE(p.proacl, pg_catalog.acldefault('f', p.proowner)) \
         ) AS a \
        WHERE p.oid = $1::pg_catalog.oid \
          AND a.grantee <> p.proowner \
        ORDER BY 1, 2"
    };

    let unsafe_grants: Vec<AclGrantRow> = sqlx::query_as(query)
        .bind(proc_oid)
        .fetch_all(&mut **tx)
        .await?;

    push_unsafe_acl_finding(findings, None, object, "function", &unsafe_grants);
    Ok(())
}

#[derive(Debug, FromRow)]
struct AttachedDefaultPartitionRow {
    oid: i64,
    schema_name: String,
    table_name: String,
    relkind: String,
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
    skip_data_probe: bool,
    findings: &mut Vec<Finding>,
) -> Result<Option<DefaultPartitionStatus>, sqlx::Error> {
    let attached: Option<AttachedDefaultPartitionRow> = sqlx::query_as(
        "SELECT \
           child.oid::bigint AS oid, \
           cn.nspname AS schema_name, \
           child.relname AS table_name, \
           child.relkind::text AS relkind \
         FROM pg_catalog.pg_inherits i \
         JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent \
         JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace \
         JOIN pg_catalog.pg_class child ON child.oid = i.inhrelid \
         JOIN pg_catalog.pg_namespace cn ON cn.oid = child.relnamespace \
        WHERE pn.nspname = $1 \
          AND parent.relname = 'changes' \
          AND parent.relkind = 'p' \
          AND child.relispartition \
          AND pg_catalog.pg_get_expr(child.relpartbound, child.oid) = 'DEFAULT'",
    )
    .bind(audit_schema)
    .fetch_optional(&mut **tx)
    .await?;

    let expected_object = format!("{audit_schema}.changes_default");
    let Some(default_part) = attached else {
        report_missing_default_partition(tx, audit_schema, &expected_object, findings).await?;
        return Ok(None);
    };

    let attached_object = format!("{}.{}", default_part.schema_name, default_part.table_name);
    if attached_object != expected_object {
        findings.push(Finding {
            kind: FindingKind::Outdated,
            severity: Severity::Error,
            table: Some(expected_object.clone()),
            object: expected_object.clone(),
            detail: format!(
                "DEFAULT partition attached to `{audit_schema}.changes` is `{attached_object}`, expected `{expected_object}`"
            ),
        });
    }

    // Non-regular child partitions (`relkind <> 'r'`) are already reported across the full
    // partition tree by `check_child_partitions_security`. Skip querying `{schema}.changes`
    // whenever any foreign partition or unexpected Row-Level Security exists anywhere in the
    // history tree (PostgreSQL plans every partition on `tableoid` predicates, so probing would
    // invoke untrusted FDW hooks or RLS policies).
    if skip_data_probe {
        return Ok(None);
    }

    let status = query_default_partition_stats(tx, audit_schema, &default_part).await?;

    if status.row_count > 0 {
        let oldest = status.oldest_occurred_at.as_deref().unwrap_or("unknown");
        let newest = status.newest_occurred_at.as_deref().unwrap_or("unknown");
        findings.push(Finding {
            kind: FindingKind::DefaultPartitionRows,
            severity: Severity::Error,
            table: Some(attached_object.clone()),
            object: attached_object.clone(),
            detail: format!(
                "DEFAULT partition `{attached_object}` holds {} row(s) (oldest: {oldest}, newest: {newest}); monthly partitions cannot be created for covered months until those rows are moved",
                status.row_count
            ),
        });
    }

    Ok(Some(status))
}

async fn query_default_partition_stats(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    default_part: &AttachedDefaultPartitionRow,
) -> Result<DefaultPartitionStatus, sqlx::Error> {
    let tableoid_filter = if default_part.relkind == "p" {
        let leaf_oids: Vec<i64> = sqlx::query_scalar(
            "WITH RECURSIVE sub(relid) AS ( \
               SELECT $1::pg_catalog.oid \
               UNION \
               SELECT i.inhrelid \
                 FROM sub \
                 JOIN pg_catalog.pg_inherits i ON i.inhparent = sub.relid \
             ) \
             SELECT c.oid::bigint \
               FROM sub \
               JOIN pg_catalog.pg_class c ON c.oid = sub.relid \
              WHERE c.relkind = 'r' \
              ORDER BY c.oid",
        )
        .bind(default_part.oid)
        .fetch_all(&mut **tx)
        .await?;
        if leaf_oids.is_empty() {
            return Ok(DefaultPartitionStatus {
                row_count: 0,
                oldest_occurred_at: None,
                newest_occurred_at: None,
            });
        }
        let oid_list = leaf_oids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("tableoid = ANY(ARRAY[{oid_list}]::pg_catalog.oid[])")
    } else {
        format!("tableoid = {}::pg_catalog.oid", default_part.oid)
    };

    let schema_q = quote_ident(audit_schema);
    // Query through parent `{schema}.changes` filtered by constant DEFAULT leaf OID(s):
    // PostgreSQL checks SELECT only on the parent table (so non-superusers with parent SELECT
    // succeed while child partitions stay zero-grant). Callers skip this probe whenever any
    // foreign partition or unexpected RLS exists on the history partition tree.
    let sql = format!(
        "SELECT \
           pg_catalog.count(*)::bigint AS row_count, \
           pg_catalog.to_char(pg_catalog.min(occurred_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS oldest, \
           pg_catalog.to_char(pg_catalog.max(occurred_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS newest \
         FROM {schema_q}.changes \
        WHERE {tableoid_filter}"
    );

    let row: DefaultPartitionRow = sqlx::query_as(AssertSqlSafe(sql))
        .fetch_one(&mut **tx)
        .await?;

    Ok(DefaultPartitionStatus {
        row_count: row.row_count,
        oldest_occurred_at: row.oldest,
        newest_occurred_at: row.newest,
    })
}

async fn report_missing_default_partition(
    tx: &mut Transaction<'_, Postgres>,
    audit_schema: &str,
    expected_object: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), sqlx::Error> {
    let standalone_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
           SELECT 1 \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = $1 \
              AND c.relname = 'changes_default' \
              AND c.relkind IN ('r', 'p') \
         )",
    )
    .bind(audit_schema)
    .fetch_one(&mut **tx)
    .await?;

    let detail = if standalone_exists {
        format!(
            "DEFAULT partition `{expected_object}` exists as an unattached table and is not attached to `{audit_schema}.changes`"
        )
    } else {
        format!(
            "DEFAULT partition `{expected_object}` is missing (not attached to `{audit_schema}.changes`)"
        )
    };
    findings.push(Finding {
        kind: FindingKind::Missing,
        severity: Severity::Error,
        table: Some(expected_object.to_owned()),
        object: expected_object.to_owned(),
        detail,
    });
    Ok(())
}

#[derive(Debug, FromRow)]
struct TableLookupRow {
    oid: i64,
    schema_name: String,
    table_name: String,
    relkind: String,
}

#[derive(Debug, FromRow)]
#[allow(clippy::struct_excessive_bools)]
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
    has_column_filter: bool,
    tgdeferrable: bool,
    tginitdeferred: bool,
    is_constraint: bool,
}

#[derive(Debug, FromRow)]
struct PartitionCloneRow {
    parent_relid: i64,
    child_schema: String,
    child_table: String,
    tgname: String,
    tgenabled: String,
}

#[derive(Debug, FromRow)]
struct TableColumnRow {
    attrelid: i64,
    attname: String,
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
    let columns_by_rel = fetch_table_columns(tx, &present_oids).await?;

    let partitioned_oids: Vec<i64> = tables
        .iter()
        .filter_map(|spec| table_map.get(&(spec.schema.clone(), spec.table.clone())))
        .filter(|r| r.relkind == "p")
        .map(|r| r.oid)
        .collect();

    let partition_clones =
        fetch_partition_update_clones(tx, &partitioned_oids, audit_schema).await?;

    let empty_columns = HashSet::new();
    let ctx = TableCheckContext {
        audit_schema,
        table_map: &table_map,
        triggers_by_rel: &triggers_by_rel,
        columns_by_rel: &columns_by_rel,
        empty_columns: &empty_columns,
        partition_clones: &partition_clones,
    };
    for spec in tables {
        check_single_table(spec, &ctx, findings);
    }

    Ok(())
}

struct TableCheckContext<'a> {
    audit_schema: &'a str,
    table_map: &'a HashMap<(String, String), TableLookupRow>,
    triggers_by_rel: &'a HashMap<i64, HashMap<String, TriggerCatalogRow>>,
    columns_by_rel: &'a HashMap<i64, HashSet<String>>,
    empty_columns: &'a HashSet<String>,
    partition_clones: &'a HashMap<i64, Vec<PartitionCloneRow>>,
}

async fn fetch_table_columns(
    tx: &mut Transaction<'_, Postgres>,
    table_oids: &[i64],
) -> Result<HashMap<i64, HashSet<String>>, sqlx::Error> {
    if table_oids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows: Vec<TableColumnRow> = sqlx::query_as(
        "SELECT \
           a.attrelid::bigint AS attrelid, \
           a.attname::text AS attname \
         FROM pg_catalog.pg_attribute a \
        WHERE a.attrelid = ANY(ARRAY(SELECT x::pg_catalog.oid FROM pg_catalog.unnest($1::bigint[]) AS x)) \
          AND a.attnum > 0 \
          AND NOT a.attisdropped \
        ORDER BY a.attrelid, a.attnum",
    )
    .bind(table_oids)
    .fetch_all(&mut **tx)
    .await?;

    let mut by_rel: HashMap<i64, HashSet<String>> = HashMap::new();
    for row in rows {
        by_rel.entry(row.attrelid).or_default().insert(row.attname);
    }
    Ok(by_rel)
}

fn check_single_table(
    spec: &TableTriggerSpec,
    ctx: &TableCheckContext<'_>,
    findings: &mut Vec<Finding>,
) {
    let fq_table = format!("{}.{}", spec.schema, spec.table);
    let Some(cat) = ctx
        .table_map
        .get(&(spec.schema.clone(), spec.table.clone()))
    else {
        findings.push(Finding {
            kind: FindingKind::Missing,
            severity: Severity::Error,
            table: Some(fq_table.clone()),
            object: fq_table.clone(),
            detail: format!("audited table `{fq_table}` does not exist"),
        });
        return;
    };

    let live_columns = ctx
        .columns_by_rel
        .get(&cat.oid)
        .unwrap_or(ctx.empty_columns);

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

    check_config_exclusions(spec, live_columns, &fq_table, findings);

    let rel_triggers = ctx.triggers_by_rel.get(&cat.oid);
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

        check_live_trigger_exclusions(trig, &fq_table, &object, spec, live_columns, findings);

        let diffs = diff_trigger(trig, expected, ctx.audit_schema, &expected_args);
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

    if is_partitioned_live && let Some(clones) = ctx.partition_clones.get(&cat.oid) {
        check_partition_update_clones(&fq_table, clones, findings);
    }
}

fn push_invalid_exclusion(
    findings: &mut Vec<Finding>,
    fq_table: &str,
    object: &str,
    detail: String,
) {
    findings.push(Finding {
        kind: FindingKind::InvalidExclusion,
        severity: Severity::Error,
        table: Some(fq_table.to_owned()),
        object: object.to_owned(),
        detail,
    });
}

fn check_config_exclusions(
    spec: &TableTriggerSpec,
    live_columns: &HashSet<String>,
    fq_table: &str,
    findings: &mut Vec<Finding>,
) {
    let config_pk_excluded = spec.excluded_primary_key_columns();
    if !config_pk_excluded.is_empty() {
        push_invalid_exclusion(
            findings,
            fq_table,
            fq_table,
            format!(
                "primary key column(s) {} cannot be excluded on `{fq_table}` because `record_id` requires them",
                config_pk_excluded.join(", ")
            ),
        );
    }

    let missing_cols = missing_excluded_columns(&spec.excluded_columns, live_columns);
    if !missing_cols.is_empty() {
        push_invalid_exclusion(
            findings,
            fq_table,
            fq_table,
            format!(
                "excluded column(s) {} do not exist on `{fq_table}` (column may have been renamed or dropped)",
                missing_cols.join(", ")
            ),
        );
    }
}

fn missing_excluded_columns<'a>(
    excluded: &'a [String],
    live_columns: &HashSet<String>,
) -> Vec<&'a str> {
    let mut missing: Vec<&'a str> = excluded
        .iter()
        .filter(|col| !live_columns.contains(col.as_str()))
        .map(String::as_str)
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}

fn check_partition_update_clones(
    fq_table: &str,
    clones: &[PartitionCloneRow],
    findings: &mut Vec<Finding>,
) {
    for clone in clones {
        if let Some(state_label) = disabled_state_label(&clone.tgenabled) {
            let child_fq = format!("{}.{}", clone.child_schema, clone.child_table);
            findings.push(Finding {
                kind: FindingKind::Disabled,
                severity: Severity::Error,
                table: Some(fq_table.to_owned()),
                object: format!("{child_fq}.{}", clone.tgname),
                detail: format!(
                    "cloned trigger `{}` on child partition `{child_fq}` (of `{fq_table}`) is {state_label} (`tgenabled = '{}'`)",
                    clone.tgname, clone.tgenabled
                ),
            });
        }
    }
}

fn check_live_trigger_exclusions(
    trig: &TriggerCatalogRow,
    fq_table: &str,
    object: &str,
    spec: &TableTriggerSpec,
    live_columns: &HashSet<String>,
    findings: &mut Vec<Finding>,
) {
    let live_args = decode_tgargs(&trig.tgargs, trig.tgnargs);
    let Some(live_excluded) = live_args
        .get(1)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
    else {
        return;
    };

    if !spec.primary_key.is_empty() {
        let mut bad_live: Vec<&str> = live_excluded
            .iter()
            .filter(|col| spec.primary_key.iter().any(|pk| pk == *col))
            .map(String::as_str)
            .collect();
        bad_live.sort_unstable();
        bad_live.dedup();
        if !bad_live.is_empty() {
            push_invalid_exclusion(
                findings,
                fq_table,
                object,
                format!(
                    "installed trigger `{}` on `{fq_table}` excludes primary key column(s) {}",
                    trig.tgname,
                    bad_live.join(", ")
                ),
            );
        }
    }

    let missing_live = missing_excluded_columns(&live_excluded, live_columns);
    if !missing_live.is_empty() {
        push_invalid_exclusion(
            findings,
            fq_table,
            object,
            format!(
                "installed trigger `{}` on `{fq_table}` excludes non-existent column(s) {} (column may have been renamed or dropped)",
                trig.tgname,
                missing_live.join(", ")
            ),
        );
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
           t.tgqual IS NOT NULL AS has_when, \
           pg_catalog.cardinality(t.tgattr::pg_catalog.int2[]) > 0 AS has_column_filter, \
           t.tgdeferrable, \
           t.tginitdeferred, \
           t.tgconstraint <> 0 AS is_constraint \
         FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
         JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
        WHERE t.tgrelid = ANY(ARRAY(SELECT x::pg_catalog.oid FROM pg_catalog.unnest($1::bigint[]) AS x)) \
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
    audit_schema: &str,
) -> Result<HashMap<i64, Vec<PartitionCloneRow>>, sqlx::Error> {
    if partitioned_oids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows: Vec<PartitionCloneRow> = sqlx::query_as(
        "WITH RECURSIVE tree(root_oid, child_oid) AS ( \
           SELECT i.inhparent, i.inhrelid \
             FROM pg_catalog.pg_inherits i \
            WHERE i.inhparent = ANY(ARRAY(SELECT x::pg_catalog.oid FROM pg_catalog.unnest($1::bigint[]) AS x)) \
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
         LEFT JOIN pg_catalog.pg_trigger pt ON pt.oid = t.tgparentid \
         JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
         JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
        WHERE NOT t.tgisinternal \
          AND ( \
            t.tgname = 'auditstream_update' \
            OR pt.tgname = 'auditstream_update' \
            OR (pn.nspname = $2 AND p.proname = 'capture') \
          )",
    )
    .bind(partitioned_oids)
    .bind(audit_schema)
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
    let pk_json = triggers::encode_json_string_array(&spec.primary_key);
    let mut excluded = spec.excluded_columns.clone();
    excluded.sort();
    excluded.dedup();
    let ex_json = triggers::encode_json_string_array(&excluded);
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

    diff_trigger_modifiers(actual, &mut diffs);
    diff_trigger_args(actual, expected_args, &mut diffs);

    diffs
}

fn diff_trigger_args(
    actual: &TriggerCatalogRow,
    expected_args: &[String],
    diffs: &mut Vec<String>,
) {
    let actual_args = decode_tgargs(&actual.tgargs, actual.tgnargs);
    if actual_args == expected_args {
        return;
    }
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

fn diff_trigger_modifiers(actual: &TriggerCatalogRow, diffs: &mut Vec<String>) {
    if actual.has_when {
        diffs.push("unexpected WHEN clause".to_owned());
    }
    if actual.has_column_filter {
        diffs.push("unexpected UPDATE OF column filter".to_owned());
    }
    if actual.tgdeferrable || actual.tginitdeferred {
        diffs.push("unexpected DEFERRABLE/INITIALLY DEFERRED trigger timing".to_owned());
    }
    if actual.is_constraint {
        diffs.push("unexpected CONSTRAINT trigger".to_owned());
    }
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
        for schema in [
            DEFAULT_SCHEMA,
            "x.capture(",
            "x.resolve_actor(",
            "x.log_event(",
            "audit\nstream",
            "x\nAS $fn$",
        ] {
            let bodies = ExpectedFunctionBodies::from_generator(schema);
            assert!(
                bodies.resolve_actor.contains("auditstream.actor"),
                "schema {schema:?}"
            );
            assert!(
                bodies.capture.contains("TG_OP = 'TRUNCATE'"),
                "schema {schema:?}"
            );
            assert!(bodies.log_event.contains("v_event_id"), "schema {schema:?}");
        }
    }

    #[test]
    fn diff_function_proconfig_enforces_exact_search_path() {
        let mut ok_diffs = Vec::new();
        diff_function_proconfig(Some(&[EXPECTED_SEARCH_PATH.to_owned()]), &mut ok_diffs);
        assert!(ok_diffs.is_empty(), "{ok_diffs:?}");

        let mut extra_diffs = Vec::new();
        diff_function_proconfig(
            Some(&[
                EXPECTED_SEARCH_PATH.to_owned(),
                "auditstream.actor={\"type\":\"user\",\"id\":\"spoof\"}".to_owned(),
            ]),
            &mut extra_diffs,
        );
        assert_eq!(extra_diffs.len(), 1);
        assert!(extra_diffs[0].contains("auditstream.actor="));

        let mut missing_diffs = Vec::new();
        diff_function_proconfig(None, &mut missing_diffs);
        assert_eq!(missing_diffs.len(), 1);
        assert!(missing_diffs[0].contains("missing `SET search_path"));

        let mut wrong_diffs = Vec::new();
        diff_function_proconfig(Some(&["search_path=public".to_owned()]), &mut wrong_diffs);
        assert_eq!(wrong_diffs.len(), 2);
        assert!(wrong_diffs[0].contains("missing `SET search_path"));
        assert!(wrong_diffs[1].contains("search_path=public"));
    }

    #[test]
    fn normalize_sql_predicate_strips_only_enclosing_parens() {
        assert_eq!(
            normalize_sql_predicate("((record_id   IS  NOT NULL))"),
            "record_id IS NOT NULL"
        );
        assert_eq!(
            normalize_sql_predicate("(record_id IS NOT NULL) AND (op <> 'x')"),
            "(record_id IS NOT NULL) AND (op <> 'x')"
        );
        assert_eq!(
            normalize_sql_predicate("record_id IS NOT NULL"),
            "record_id IS NOT NULL"
        );
    }

    #[test]
    fn diff_history_index_catches_invalid_am_columns_predicate_and_flags() {
        let expected = EXPECTED_HISTORY_INDEXES[2];
        let clean = HistoryIndexRow {
            index_name: "changes_lookup_idx".to_owned(),
            access_method: "hash".to_owned(),
            indisvalid: true,
            indisready: true,
            indisprimary: false,
            indisunique: false,
            has_include_columns: false,
            has_nondefault_options: false,
            has_nondefault_opclass: false,
            key_columns: vec!["record_id".to_owned()],
            predicate: Some("(record_id IS NOT NULL)".to_owned()),
        };
        assert_eq!(diff_history_index(&clean, expected), Vec::<String>::new());

        let broken = HistoryIndexRow {
            index_name: "changes_lookup_idx".to_owned(),
            access_method: "btree".to_owned(),
            indisvalid: false,
            indisready: true,
            indisprimary: false,
            indisunique: true,
            has_include_columns: true,
            has_nondefault_options: true,
            has_nondefault_opclass: true,
            key_columns: vec!["table_name".to_owned()],
            predicate: None,
        };
        let diffs = diff_history_index(&broken, expected);
        assert_eq!(diffs.len(), 8, "{diffs:?}");
        assert!(diffs[0].contains("invalid"));
        assert!(diffs[1].contains("btree"));
        assert!(diffs[2].contains("table_name"));
        assert!(diffs[3].contains("predicate"));
        assert!(diffs[4].contains("UNIQUE"));
        assert!(diffs[5].contains("INCLUDE"));
        assert!(diffs[6].contains("sort/nulls"));
        assert!(diffs[7].contains("operator class"));

        let non_unique_pk = HistoryIndexRow {
            index_name: "changes_pkey".to_owned(),
            access_method: "btree".to_owned(),
            indisvalid: true,
            indisready: true,
            indisprimary: true,
            indisunique: false,
            has_include_columns: false,
            has_nondefault_options: false,
            has_nondefault_opclass: false,
            key_columns: vec!["id".to_owned(), "occurred_at".to_owned()],
            predicate: None,
        };
        let pk_diffs = diff_history_index(&non_unique_pk, EXPECTED_HISTORY_PRIMARY_KEY);
        assert_eq!(pk_diffs, vec!["index is missing UNIQUE constraint"]);
    }

    #[test]
    fn diff_function_row_catches_strict_volatility_defaults_and_signature_drift() {
        let spec = FunctionCheckSpec {
            name: "log_event",
            pronargs: 3,
            expected_nargdefaults: 2,
            expected_arg_defaults: Some("NULL::jsonb, NULL::jsonb"),
            expected_rettype: "uuid",
            expected_retset: false,
            expected_volatile: "v",
            expected_arg_names: &["action", "target", "data"],
            expected_arg_modes: &[],
            expected_all_arg_types: &[],
            expected_body: "BEGIN RETURN gen_random_uuid(); END",
            allow_non_owner_execute: true,
        };
        let clean = ProcRow {
            oid: 1,
            prosrc: "BEGIN RETURN gen_random_uuid(); END".to_owned(),
            prosecdef: true,
            proisstrict: false,
            proleakproof: false,
            provolatile: "v".to_owned(),
            proparallel: "u".to_owned(),
            prosupport: 0,
            langname: "plpgsql".to_owned(),
            proconfig: Some(vec![EXPECTED_SEARCH_PATH.to_owned()]),
            owner_name: OWNER_ROLE.to_owned(),
            rettype_schema: "pg_catalog".to_owned(),
            rettype_name: "uuid".to_owned(),
            proretset: false,
            pronargdefaults: 2,
            arg_defaults: Some("NULL::jsonb, NULL::jsonb".to_owned()),
            arg_names: vec!["action".to_owned(), "target".to_owned(), "data".to_owned()],
            arg_modes: vec![],
            all_arg_types: vec![],
        };
        assert_eq!(diff_function_row(&clean, &spec), Vec::<String>::new());

        let tampered = ProcRow {
            proisstrict: true,
            proleakproof: true,
            provolatile: "i".to_owned(),
            proparallel: "s".to_owned(),
            prosupport: 42,
            langname: "sql".to_owned(),
            rettype_name: "text".to_owned(),
            proretset: true,
            arg_defaults: Some("'{}'::jsonb, NULL::jsonb".to_owned()),
            arg_names: vec!["action".to_owned(), "data".to_owned(), "target".to_owned()],
            arg_modes: vec!["o".to_owned()],
            all_arg_types: vec!["pg_catalog.text".to_owned()],
            ..clean
        };
        let diffs = diff_function_row(&tampered, &spec);
        assert!(diffs.iter().any(|d| d.contains("plpgsql")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("STRICT")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("volatility")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("parallel")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("LEAKPROOF")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("SUPPORT")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("return type")), "{diffs:?}");
        assert!(
            diffs
                .iter()
                .any(|d| d.contains("default argument expressions")),
            "{diffs:?}"
        );
        assert!(
            diffs.iter().any(|d| d.contains("parameter names")),
            "{diffs:?}"
        );
        assert!(
            diffs.iter().any(|d| d.contains("parameter modes")),
            "{diffs:?}"
        );
        assert!(
            diffs.iter().any(|d| d.contains("parameter types")),
            "{diffs:?}"
        );
    }

    #[test]
    fn diff_trigger_modifiers_catches_when_column_filter_deferrable_and_constraint() {
        let row = TriggerCatalogRow {
            tgrelid: 1,
            tgname: "auditstream_update".to_owned(),
            tgenabled: "O".to_owned(),
            tgtype: TRIGGER_TYPE_ROW | TRIGGER_TYPE_UPDATE,
            tgnargs: 0,
            tgargs: Vec::new(),
            tgnewtable: None,
            tgoldtable: None,
            func_schema: DEFAULT_SCHEMA.to_owned(),
            func_name: "capture".to_owned(),
            has_when: true,
            has_column_filter: true,
            tgdeferrable: true,
            tginitdeferred: true,
            is_constraint: true,
        };
        let mut diffs = Vec::new();
        diff_trigger_modifiers(&row, &mut diffs);
        assert_eq!(diffs.len(), 4, "{diffs:?}");
        assert!(diffs[0].contains("WHEN"));
        assert!(diffs[1].contains("UPDATE OF"));
        assert!(diffs[2].contains("DEFERRABLE"));
        assert!(diffs[3].contains("CONSTRAINT"));
    }

    #[test]
    fn owner_role_security_helpers_distinguish_safe_creator_admin_and_unsafe_flags() {
        let clean_role = OwnerRoleRow {
            oid: 1,
            rolcanlogin: false,
            rolsuper: false,
            rolcreaterole: false,
            rolcreatedb: false,
            rolreplication: false,
            rolbypassrls: false,
            server_version_num: 170_000,
        };
        assert_eq!(
            unsafe_owner_role_attributes(&clean_role),
            Vec::<&str>::new()
        );

        let bad_role = OwnerRoleRow {
            rolcanlogin: true,
            rolsuper: true,
            rolcreaterole: true,
            rolcreatedb: true,
            rolreplication: true,
            rolbypassrls: true,
            ..clean_role
        };
        assert_eq!(
            unsafe_owner_role_attributes(&bad_role),
            vec![
                "LOGIN",
                "SUPERUSER",
                "CREATEROLE",
                "CREATEDB",
                "REPLICATION",
                "BYPASSRLS"
            ]
        );

        let safe_pg16_creator = OwnerMemberPg16Row {
            member_name: "migrator".to_owned(),
            admin_option: true,
            inherit_option: false,
            set_option: false,
            rolsuper: false,
            rolcreaterole: true,
        };
        assert_eq!(format_unsafe_member_pg16(&safe_pg16_creator), None);

        let unsafe_set = OwnerMemberPg16Row {
            set_option: true,
            ..safe_pg16_creator
        };
        assert_eq!(
            format_unsafe_member_pg16(&unsafe_set).as_deref(),
            Some("migrator (SET, ADMIN)")
        );

        let unsafe_non_createrole_admin = OwnerMemberPg16Row {
            member_name: "app".to_owned(),
            admin_option: true,
            inherit_option: false,
            set_option: false,
            rolsuper: false,
            rolcreaterole: false,
        };
        assert_eq!(
            format_unsafe_member_pg16(&unsafe_non_createrole_admin).as_deref(),
            Some("app (ADMIN)")
        );

        let unsafe_bare_member = OwnerMemberPg16Row {
            member_name: "stale".to_owned(),
            admin_option: false,
            inherit_option: false,
            set_option: false,
            rolsuper: false,
            rolcreaterole: true,
        };
        assert_eq!(
            format_unsafe_member_pg16(&unsafe_bare_member).as_deref(),
            Some("stale (MEMBER)")
        );
    }

    #[test]
    fn missing_excluded_columns_reports_renamed_or_dropped_columns_deterministically() {
        let live: HashSet<String> = ["id", "status", "secret_hash"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let excluded = vec![
            "status".to_owned(),
            "old_b".to_owned(),
            "old_a".to_owned(),
            "old_b".to_owned(),
        ];
        assert_eq!(
            missing_excluded_columns(&excluded, &live),
            vec!["old_a", "old_b"]
        );
    }
}
