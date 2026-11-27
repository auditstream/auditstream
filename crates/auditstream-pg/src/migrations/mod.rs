//! Migration writers for Prisma, Drizzle (0.45 and 1.x layouts), and plain SQL.
//!
//! Composes [`crate::ddl::generate`] and [`crate::triggers::generate`] (plus idempotent
//! `DROP TRIGGER IF EXISTS` statements when tables are removed from a previous Auditstream
//! selection) into deterministic, re-runnable migration files that each ORM's own migrator
//! executes unchanged.

mod definition_digest;
pub mod drizzle;
pub mod prisma;
pub mod sql;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use auditstream_core::CURRENT_SCHEMA_VERSION;
use serde_json::{Value, json};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

use crate::ddl::{self, DdlOptions, quote_ident};
use crate::triggers::{self, TableTriggerSpec, TriggerOptions};

/// Prefix identifying the machine-readable manifest comment emitted at the top of every
/// Auditstream-generated migration file.
pub(crate) const MANIFEST_PREFIX: &str = "-- auditstream:manifest ";

// Marks the enclosing transaction and bounds every lock wait when the connection has no timeout.
// The fallback flag lets the last statement restore the setting for any later migration in the
// same transaction, because Drizzle's transactional migrators apply pending migrations together.
const SET_LOCK_TIMEOUT: &str = "DO $$
BEGIN
  PERFORM pg_catalog.set_config('auditstream._migration_tx', 'on', true);
  IF pg_catalog.current_setting('lock_timeout') = '0' THEN
    SET LOCAL lock_timeout = '5s';
    PERFORM pg_catalog.set_config('auditstream._lock_timeout_fallback', 'on', true);
  END IF;
END
$$";

// Rejects runners that commit each statement separately (such as the Drizzle Neon HTTP migrator or
// `psql -f` on a Prisma file), which discard transaction-local settings, break DDL atomicity, and
// lose session state needed to revoke the temporary `auditstream_owner` self-grant on PG 16+.
const REQUIRE_TRANSACTION: &str = "DO $$
BEGIN
  IF pg_catalog.current_setting('auditstream._migration_tx', true) IS DISTINCT FROM 'on' THEN
    RAISE EXCEPTION 'Auditstream migrations must run inside a single transaction'
      USING HINT = 'Use a transactional migrator (for example Prisma Migrate, psql --single-transaction, or Drizzle with node-postgres, postgres.js, or neon-serverless) instead of an autocommit/HTTP runner.';
  END IF;
END
$$";

const RESTORE_LOCK_TIMEOUT: &str = "DO $$
BEGIN
  IF pg_catalog.current_setting('auditstream._lock_timeout_fallback', true) = 'on' THEN
    SET LOCAL lock_timeout = 0;
    PERFORM pg_catalog.set_config('auditstream._lock_timeout_fallback', 'off', true);
  END IF;
  PERFORM pg_catalog.set_config('auditstream._migration_tx', 'off', true);
END
$$";

/// Trigger names installed by [`crate::triggers::generate`] on each audited table.
const AUDIT_TRIGGER_NAMES: [&str; 4] = [
    "auditstream_insert",
    "auditstream_update",
    "auditstream_delete",
    "auditstream_truncate",
];

/// Target migration output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationFormat {
    /// Prisma 7 migration folder: `<out_dir>/<YYYYMMDDHHMMSS>_auditstream/migration.sql`.
    Prisma,
    /// Drizzle migration (auto-detects 0.45 `meta/_journal.json` vs 1.x folder layout).
    Drizzle,
    /// Plain SQL file wrapped in `BEGIN; ... COMMIT;` for `psql -v ON_ERROR_STOP=1 -f`.
    Sql,
}

/// Drizzle migration directory layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrizzleLayout {
    /// Drizzle 0.45 layout: `meta/_journal.json` + `<NNNN>_auditstream.sql`.
    V045,
    /// Drizzle 1.x layout: `<YYYYMMDDHHMMSS>_auditstream/migration.sql` (no `meta/_journal.json`).
    V1,
}

/// Errors returned while inspecting or writing migration files.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MigrationError {
    /// Reading or writing migration files on disk failed.
    #[error("migration I/O error: {0}")]
    Io(#[from] io::Error),

    /// Parsing an existing `meta/_journal.json` or manifest failed.
    #[error("invalid migration metadata: {0}")]
    InvalidMetadata(String),
}

/// Configuration for [`write()`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOptions {
    /// Target migration format (`prisma`, `drizzle`, or `sql`).
    pub format: MigrationFormat,
    /// Target migration directory (created if missing).
    pub out_dir: PathBuf,
    /// Deterministic UTC timestamp used to name new migrations and Drizzle journal `when`.
    pub now: OffsetDateTime,
    /// History schema DDL options (audit schema name and application roles).
    pub ddl: DdlOptions,
    /// Audited tables to attach capture triggers to.
    pub tables: Vec<TableTriggerSpec>,
    /// Optional explicit `(schema, table)` pairs whose Auditstream triggers should be dropped
    /// in addition to any tables automatically detected as removed from the previous migration.
    pub removed_tables: Vec<(String, String)>,
    /// Optional override for Drizzle layout; when `None`, auto-detects `V045` if
    /// `<out_dir>/meta/_journal.json` exists and `V1` otherwise.
    pub drizzle_layout: Option<DrizzleLayout>,
}

impl MigrationOptions {
    /// Creates [`MigrationOptions`] with deterministic timestamp `now` and empty `removed_tables`.
    #[must_use]
    pub fn new(
        format: MigrationFormat,
        out_dir: impl Into<PathBuf>,
        now: OffsetDateTime,
        ddl: DdlOptions,
        tables: Vec<TableTriggerSpec>,
    ) -> Self {
        Self {
            format,
            out_dir: out_dir.into(),
            now,
            ddl,
            tables,
            removed_tables: Vec::new(),
            drizzle_layout: None,
        }
    }

    /// Forces a specific [`DrizzleLayout`] instead of auto-detecting from `meta/_journal.json`.
    #[must_use]
    pub const fn with_drizzle_layout(mut self, layout: DrizzleLayout) -> Self {
        self.drizzle_layout = Some(layout);
        self
    }

    /// Adds explicit `(schema, table)` pairs to drop Auditstream triggers from.
    #[must_use]
    pub fn with_removed_tables(mut self, removed: Vec<(String, String)>) -> Self {
        self.removed_tables = removed;
        self
    }
}

/// Outcome of calling [`write()`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// One or more new migration files were written (and any journal updated).
    Written {
        /// Paths written or updated on disk.
        files: Vec<PathBuf>,
        /// `(schema, table)` pairs whose Auditstream triggers were dropped in this migration.
        dropped_tables: Vec<(String, String)>,
    },
    /// The newest existing Auditstream migration already matches the requested selection;
    /// nothing was written and all existing files remain byte-identical.
    UpToDate {
        /// Path of the matching existing Auditstream migration SQL file.
        existing: PathBuf,
    },
}

/// Outcome of calling [`preview()`] (`--dry-run`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewOutcome {
    /// New migration SQL that [`write()`] would emit, without modifying the filesystem.
    Sql {
        /// Rendered migration SQL text.
        sql: String,
        /// `(schema, table)` pairs whose Auditstream triggers would be dropped.
        dropped_tables: Vec<(String, String)>,
    },
    /// The newest existing Auditstream migration already matches the requested selection.
    UpToDate {
        /// Path of the matching existing Auditstream migration SQL file.
        existing: PathBuf,
    },
}

fn validate_tables(tables: &[TableTriggerSpec]) -> Result<(), MigrationError> {
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for spec in tables {
        if !seen.insert((spec.schema.as_str(), spec.table.as_str())) {
            return Err(MigrationError::InvalidMetadata(format!(
                "duplicate table specification for `{}.{}`",
                spec.schema, spec.table,
            )));
        }
        let pk_excluded = spec.excluded_primary_key_columns();
        if !pk_excluded.is_empty() {
            return Err(MigrationError::InvalidMetadata(format!(
                "cannot exclude primary key column(s) {} on `{}.{}`: primary key columns are required to build `record_id`",
                pk_excluded.join(", "),
                spec.schema,
                spec.table,
            )));
        }
    }
    Ok(())
}

fn validate_ddl(ddl: &DdlOptions) -> Result<(), MigrationError> {
    ddl::validate_schema_name(&ddl.schema).map_err(MigrationError::InvalidMetadata)
}

fn validate_options(options: &MigrationOptions) -> Result<(), MigrationError> {
    validate_ddl(&options.ddl)?;
    validate_tables(&options.tables)
}

/// Composes D2 history schema DDL and D3 capture triggers into one ordered, deterministic
/// sequence of single SQL statements without touching the filesystem or requiring a timestamp.
///
/// The returned sequence matches the exact production composition emitted by [`write()`] and
/// [`preview()`]: it sets the transaction guard and `lock_timeout` fallback, installs the history
/// schema and functions, attaches capture triggers before enabling LZ4 TOAST compression (when
/// supported), creating future monthly partitions, and reconciling leaf BRIN `autosummarize = on`
/// options (avoiding lock-order deadlocks with concurrent writers), revokes any temporary
/// `auditstream_owner` self-grant, and restores `lock_timeout`.
///
/// Callers MUST execute the returned statements sequentially inside a single transaction
/// (for example via `sqlx::Transaction`, a transactional migrator, or [`render_install_sql()`]);
/// autocommit execution is rejected before any role or schema DDL runs.
///
/// # Errors
///
/// Returns [`MigrationError::InvalidMetadata`] if `ddl.schema` fails
/// [`crate::ddl::validate_schema_name`], `tables` contains duplicate `(schema, table)` entries, or
/// any table specification excludes one of its own primary key columns.
pub fn install_statements(
    ddl: &DdlOptions,
    tables: &[TableTriggerSpec],
) -> Result<Vec<String>, MigrationError> {
    validate_ddl(ddl)?;
    validate_tables(tables)?;
    Ok(compose_statements_inner(ddl, tables, &[]))
}

/// Renders a self-contained, transaction-wrapped plain SQL script (`BEGIN; ... COMMIT;`) that
/// installs or idempotently reconciles the D2 history schema DDL and D3 capture triggers.
///
/// Output can be piped directly into `psql -v ON_ERROR_STOP=1` without manually stitching
/// [`crate::ddl::generate()`] and [`crate::triggers::generate()`] or wrapping in a transaction.
///
/// # Errors
///
/// Returns [`MigrationError::InvalidMetadata`] if `ddl.schema` fails
/// [`crate::ddl::validate_schema_name`], `tables` contains duplicate `(schema, table)` entries, or
/// any table specification excludes one of its own primary key columns.
pub fn render_install_sql(
    ddl: &DdlOptions,
    tables: &[TableTriggerSpec],
) -> Result<String, MigrationError> {
    let statements = install_statements(ddl, tables)?;
    Ok(sql::format_transaction_script(&statements))
}

/// Renders the migration SQL that [`write()`] would generate without creating directories or
/// writing any files on disk.
///
/// # Errors
///
/// Returns [`MigrationError::Io`] on filesystem read errors or [`MigrationError::InvalidMetadata`]
/// if `options.ddl.schema`, an existing Drizzle `meta/_journal.json`, Auditstream manifest,
/// duplicate `(schema, table)` entry, or table exclusion spec is invalid.
pub fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
    validate_options(options)?;
    match options.format {
        MigrationFormat::Prisma => prisma::preview(options),
        MigrationFormat::Drizzle => drizzle::preview(options),
        MigrationFormat::Sql => sql::preview(options),
    }
}

/// Writes an Auditstream migration in the configured format (`prisma`, `drizzle`, or `sql`).
///
/// - Composes D2 history schema DDL and D3 capture triggers, keeping D2's temporary
///   `auditstream_owner` self-grant active until after D3 creates/alters functions so
///   PostgreSQL 16+ non-superuser migrators succeed.
/// - Re-running with the same selection and function SQL writes nothing and returns [`WriteOutcome::UpToDate`].
/// - Re-running with a changed selection leaves all existing migration files byte-identical,
///   emits `DROP TRIGGER IF EXISTS` statements for tables removed from the previous selection
///   (preserving their recorded history), and writes one new migration.
///
/// # Errors
///
/// Returns [`MigrationError::Io`] on filesystem errors or [`MigrationError::InvalidMetadata`]
/// if `options.ddl.schema`, an existing Drizzle `meta/_journal.json`, duplicate `(schema, table)`
/// entry, or table exclusion spec is invalid.
pub fn write(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    validate_options(options)?;
    match options.format {
        MigrationFormat::Prisma => prisma::write(options),
        MigrationFormat::Drizzle => drizzle::write(options),
        MigrationFormat::Sql => sql::write(options),
    }
}

/// Normalized manifest describing an Auditstream migration's selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) canonical_json: String,
    pub(crate) tables: BTreeSet<(String, String)>,
}

fn canonicalize_tables(tables: &[TableTriggerSpec]) -> Vec<TableTriggerSpec> {
    let mut sorted = tables.to_vec();
    sorted.sort_by(|a, b| a.schema.cmp(&b.schema).then_with(|| a.table.cmp(&b.table)));
    for spec in &mut sorted {
        spec.excluded_columns.sort();
        spec.excluded_columns.dedup();
    }
    sorted
}

fn build_manifest(
    options: &MigrationOptions,
    tables: BTreeSet<(String, String)>,
    statements: &[String],
) -> Manifest {
    let mut app_roles = options.ddl.app_roles.clone();
    app_roles.sort();
    app_roles.dedup();

    let tables_sorted = canonicalize_tables(&options.tables);

    let tables_json: Vec<Value> = tables_sorted
        .iter()
        .map(|spec| {
            json!({
                "excluded_columns": spec.excluded_columns,
                "is_partitioned": spec.is_partitioned,
                "primary_key": spec.primary_key,
                "schema": spec.schema,
                "table": spec.table,
            })
        })
        .collect();

    let value = json!({
        "app_roles": app_roles,
        "definitions_sha256": definition_digest::from_statements(statements),
        "schema": options.ddl.schema,
        "schema_version": CURRENT_SCHEMA_VERSION,
        "tables": tables_json,
    });

    Manifest {
        canonical_json: serde_json::to_string(&value)
            .expect("manifest JSON serialization cannot fail"),
        tables,
    }
}

pub(crate) fn parse_manifest_from_file(path: &Path) -> Result<Option<Manifest>, MigrationError> {
    let content = fs::read_to_string(path)?;
    for line in content.lines().take(8) {
        if let Some(raw) = line.strip_prefix(MANIFEST_PREFIX) {
            let trimmed = raw.trim();
            let parsed: Value = serde_json::from_str(trimmed).map_err(|err| {
                MigrationError::InvalidMetadata(format!(
                    "malformed auditstream manifest in {}: {err}",
                    path.display()
                ))
            })?;
            let mut table_pairs = BTreeSet::new();
            if let Some(arr) = parsed.get("tables").and_then(Value::as_array) {
                for item in arr {
                    if let (Some(schema), Some(table)) = (
                        item.get("schema").and_then(Value::as_str),
                        item.get("table").and_then(Value::as_str),
                    ) {
                        table_pairs.insert((schema.to_owned(), table.to_owned()));
                    }
                }
            }
            return Ok(Some(Manifest {
                canonical_json: trimmed.to_owned(),
                tables: table_pairs,
            }));
        }
    }
    Ok(None)
}

fn compute_dropped_tables(
    previous: Option<&Manifest>,
    selected: &BTreeSet<(String, String)>,
    explicit_removed: &[(String, String)],
) -> Vec<(String, String)> {
    let mut removed: BTreeSet<_> = explicit_removed.iter().cloned().collect();
    if let Some(previous) = previous {
        removed.extend(previous.tables.iter().cloned());
    }
    removed.retain(|table| !selected.contains(table));
    removed.into_iter().collect()
}

/// Standard two-line header comment prepended to every generated migration file.
pub(crate) fn header_comment(manifest: &Manifest) -> String {
    format!(
        "-- Generated by auditstream (history schema_version = {CURRENT_SCHEMA_VERSION}). Do not edit.\n\
         {MANIFEST_PREFIX}{}",
        manifest.canonical_json
    )
}

/// Composes D2 DDL, D3 triggers, optional `DROP TRIGGER IF EXISTS` for removed tables,
/// and D2's final self-grant revocation cleanup into one ordered list of single SQL statements.
pub(crate) fn compose_statements(
    options: &MigrationOptions,
    dropped_tables: &[(String, String)],
) -> Vec<String> {
    compose_statements_inner(&options.ddl, &options.tables, dropped_tables)
}

fn compose_statements_inner(
    ddl: &DdlOptions,
    tables: &[TableTriggerSpec],
    dropped_tables: &[(String, String)],
) -> Vec<String> {
    // Keep D2's temporary `auditstream_owner` self-grant active while D3 creates/alters functions,
    // and run LZ4 TOAST compression, future monthly partition creation, and leaf BRIN
    // `autosummarize = on` reconciliation after triggers/drops so holding a lock on `changes`
    // cannot deadlock with in-flight writers holding an audited table.
    let ddl_parts = ddl::generate_parts(ddl);
    let tables_sorted = canonicalize_tables(tables);

    let trigger_stmts =
        triggers::generate_body(&TriggerOptions::new(ddl.schema.clone(), tables_sorted));

    let mut drop_stmts = Vec::with_capacity(dropped_tables.len() * AUDIT_TRIGGER_NAMES.len());
    for (schema, table) in dropped_tables {
        let schema_q = quote_ident(schema);
        let table_q = quote_ident(table);
        for trigger_name in AUDIT_TRIGGER_NAMES {
            drop_stmts.push(format!(
                "DROP TRIGGER IF EXISTS {trigger_name} ON {schema_q}.{table_q}"
            ));
        }
    }

    std::iter::once(SET_LOCK_TIMEOUT.to_owned())
        .chain(std::iter::once(REQUIRE_TRANSACTION.to_owned()))
        .chain(ddl_parts.setup)
        .chain(trigger_stmts)
        .chain(drop_stmts)
        .chain(std::iter::once(ddl_parts.toast_compression))
        .chain(std::iter::once(ddl_parts.future_partitions))
        .chain(std::iter::once(ddl_parts.brin_autosummarize))
        .chain(std::iter::once(ddl_parts.role_cleanup))
        .chain(std::iter::once(RESTORE_LOCK_TIMEOUT.to_owned()))
        .collect()
}

/// Formats `dt` in UTC as `YYYYMMDDHHMMSS` (matching Prisma and Drizzle 1.x migration folder names).
pub(crate) fn format_timestamp(dt: OffsetDateTime) -> String {
    let utc = dt.to_offset(UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
}

/// Planned migration state after comparing the current options against the newest existing
/// Auditstream migration on disk.
pub(crate) enum PlannedMigration {
    /// Existing migration already matches the requested selection.
    UpToDate { existing: PathBuf },
    /// A new migration must be rendered with these statements.
    Changed {
        manifest: Manifest,
        dropped_tables: Vec<(String, String)>,
        statements: Vec<String>,
    },
}

pub(crate) fn plan_migration(
    options: &MigrationOptions,
    newest: Option<(PathBuf, Manifest)>,
) -> PlannedMigration {
    let selected = options
        .tables
        .iter()
        .map(|t| (t.schema.clone(), t.table.clone()))
        .collect();
    let previous = newest.as_ref().map(|(_, manifest)| manifest);
    let dropped_tables = compute_dropped_tables(previous, &selected, &options.removed_tables);
    let statements = compose_statements(options, &dropped_tables);
    let current = build_manifest(options, selected, &statements);
    if let Some((existing, previous)) = newest
        && previous.canonical_json == current.canonical_json
        && options.removed_tables.is_empty()
    {
        return PlannedMigration::UpToDate { existing };
    }
    PlannedMigration::Changed {
        manifest: current,
        dropped_tables,
        statements,
    }
}

pub(crate) fn preview_with(
    options: &MigrationOptions,
    newest: Option<(PathBuf, Manifest)>,
    render: impl FnOnce(&Manifest, &[String]) -> String,
) -> PreviewOutcome {
    match plan_migration(options, newest) {
        PlannedMigration::UpToDate { existing } => PreviewOutcome::UpToDate { existing },
        PlannedMigration::Changed {
            manifest,
            dropped_tables,
            statements,
        } => PreviewOutcome::Sql {
            sql: render(&manifest, &statements),
            dropped_tables,
        },
    }
}

/// Inspects `path` via `symlink_metadata`, refusing to follow symlinks.
pub(crate) fn reject_symlink(path: &Path) -> Result<Option<fs::Metadata>, MigrationError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(MigrationError::InvalidMetadata(format!(
            "refusing to follow symlink at {}",
            path.display()
        ))),
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(MigrationError::Io(err)),
    }
}

/// Atomically creates a brand-new regular file without following symlinks or overwriting existing
/// files (`O_CREAT | O_EXCL`).
pub(crate) fn write_new_file(path: &Path, content: &str) -> Result<(), MigrationError> {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

/// Scans `<out_dir>/<name>/migration.sql` folders (Prisma and Drizzle 1.x layouts) and returns
/// the lexicographically newest folder containing an Auditstream manifest comment.
pub(crate) fn find_newest_folder_migration(
    out_dir: &Path,
) -> Result<Option<(PathBuf, Manifest)>, MigrationError> {
    if !out_dir.exists() {
        return Ok(None);
    }

    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(out_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(meta) = reject_symlink(&path)? else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let migration_sql = path.join("migration.sql");
        if let Some(sql_meta) = reject_symlink(&migration_sql)?
            && sql_meta.is_file()
        {
            dirs.push((name, migration_sql));
        }
    }

    dirs.sort_by(|a, b| a.0.cmp(&b.0));

    for (_, sql_path) in dirs.into_iter().rev() {
        if let Some(manifest) = parse_manifest_from_file(&sql_path)? {
            return Ok(Some((sql_path, manifest)));
        }
    }

    Ok(None)
}

/// Creates a new `<out_dir>/<YYYYMMDDHHMMSS>_auditstream/migration.sql` folder and writes
/// `rendered` SQL into it.
pub(crate) fn write_folder_migration(
    options: &MigrationOptions,
    rendered: &str,
    dropped_tables: Vec<(String, String)>,
) -> Result<WriteOutcome, MigrationError> {
    fs::create_dir_all(&options.out_dir)?;
    let ts = next_unique_timestamp(options.now, |ts| {
        options
            .out_dir
            .join(format!("{ts}_auditstream"))
            .symlink_metadata()
            .is_ok()
    });
    let folder = options.out_dir.join(format!("{ts}_auditstream"));
    fs::create_dir(&folder)?;
    let file_path = folder.join("migration.sql");
    write_new_file(&file_path, rendered)?;

    Ok(WriteOutcome::Written {
        files: vec![file_path],
        dropped_tables,
    })
}

/// Finds a unique timestamp prefix `YYYYMMDDHHMMSS` strictly greater than any existing name
/// colliding with `format_name(ts)`.
pub(crate) fn next_unique_timestamp(
    now: OffsetDateTime,
    mut exists: impl FnMut(&str) -> bool,
) -> String {
    let mut candidate = now;
    loop {
        let ts = format_timestamp(candidate);
        if !exists(&ts) {
            return ts;
        }
        candidate += time::Duration::seconds(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Month, PrimitiveDateTime, Time};

    fn sample_tables() -> Vec<TableTriggerSpec> {
        vec![
            TableTriggerSpec::new(
                "public",
                "invoice",
                vec!["id".to_owned()],
                vec!["password_hash".to_owned()],
            ),
            TableTriggerSpec::new("public", "line_item", vec!["id".to_owned()], Vec::new()),
        ]
    }

    #[test]
    fn install_statements_matches_canonical_migration_composition_and_order() {
        let ddl = DdlOptions::new(ddl::DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
        let tables = sample_tables();
        let stmts = install_statements(&ddl, &tables).expect("compose install statements");

        let fixed_now = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::November, 26).expect("date"),
            Time::from_hms(12, 0, 0).expect("time"),
        )
        .assume_utc();
        let ddl_parts = ddl::generate_parts(&ddl);
        let opts = MigrationOptions::new(
            MigrationFormat::Sql,
            PathBuf::from("unused"),
            fixed_now,
            ddl,
            tables,
        );
        assert_eq!(stmts, compose_statements(&opts, &[]));

        assert_eq!(stmts.first().map(String::as_str), Some(SET_LOCK_TIMEOUT));
        assert_eq!(stmts.get(1).map(String::as_str), Some(REQUIRE_TRANSACTION));
        assert_eq!(stmts.last().map(String::as_str), Some(RESTORE_LOCK_TIMEOUT));

        // Role elevation runs once (no duplicate triggers::generate self-grant block).
        assert_eq!(
            stmts
                .iter()
                .filter(|s| s.contains("auditstream._granted_self"))
                .count(),
            2,
            "expected exactly setup + cleanup for DDL role grant"
        );
        assert!(
            !stmts
                .iter()
                .any(|s| s.contains("auditstream._triggers_granted_self")),
            "canonical composition must not emit duplicate trigger role self-grant"
        );

        // Triggers and drops must run before LZ4 TOAST compression, future monthly partitions,
        // leaf BRIN `autosummarize = on` reconciliation, and role cleanup (`audited_table -> changes`
        // lock ordering).
        let with_drops = compose_statements_inner(
            &DdlOptions::new(ddl::DEFAULT_SCHEMA, vec!["app_user".to_owned()]),
            &sample_tables(),
            &[("public".to_owned(), "old_table".to_owned())],
        );
        let last_trigger_idx = with_drops
            .iter()
            .rposition(|s| s.starts_with("CREATE OR REPLACE TRIGGER "))
            .expect("trigger statement");
        let last_drop_idx = with_drops
            .iter()
            .rposition(|s| s.starts_with("DROP TRIGGER IF EXISTS "))
            .expect("drop trigger statement");
        let toast_idx = with_drops
            .iter()
            .position(|s| {
                s.contains("default_toast_compression") && s.contains("SET COMPRESSION lz4")
            })
            .expect("LZ4 TOAST compression statement");
        let future_part_idx = with_drops
            .iter()
            .position(|s| *s == ddl_parts.future_partitions)
            .expect("future partitions statement");
        let brin_idx = with_drops
            .iter()
            .position(|s| s.contains("SET (autosummarize = on)"))
            .expect("BRIN autosummarize statement");
        let cleanup_idx = with_drops
            .iter()
            .position(|s| *s == ddl_parts.role_cleanup)
            .expect("role cleanup statement");
        assert!(last_trigger_idx < last_drop_idx);
        assert!(last_drop_idx < toast_idx);
        assert!(toast_idx < future_part_idx);
        assert!(future_part_idx < brin_idx);
        assert!(brin_idx < cleanup_idx);
        assert!(
            with_drops.iter().any(|s| {
                s.contains("changes_occurred_at_brin_idx")
                    && s.contains("USING brin (occurred_at) WITH (autosummarize = on)")
            }),
            "fresh BRIN index creation must set WITH (autosummarize = on)"
        );

        // Capability check must precede role switch and ALTER TABLE in the LZ4 block.
        let toast_sql = &with_drops[toast_idx];
        let guard_pos = toast_sql
            .find("default_toast_compression")
            .expect("pg_settings capability guard");
        let role_pos = toast_sql
            .find("SET LOCAL ROLE")
            .expect("auditstream_owner role switch");
        let alter_pos = toast_sql
            .find("SET COMPRESSION lz4")
            .expect("ALTER TABLE SET COMPRESSION lz4");
        assert!(guard_pos < role_pos && role_pos < alter_pos);
    }

    #[test]
    fn install_statements_and_render_install_sql_are_deterministic_and_validate_pk() {
        let ddl = DdlOptions::default();
        let ordered = vec![
            TableTriggerSpec::new(
                "public",
                "alpha",
                vec!["id".to_owned()],
                vec!["a".to_owned(), "b".to_owned()],
            ),
            TableTriggerSpec::new("public", "beta", vec!["id".to_owned()], Vec::new()),
        ];
        let reordered = vec![
            TableTriggerSpec::new("public", "beta", vec!["id".to_owned()], Vec::new()),
            TableTriggerSpec::new(
                "public",
                "alpha",
                vec!["id".to_owned()],
                vec!["b".to_owned(), "a".to_owned(), "a".to_owned()],
            ),
        ];

        assert_eq!(
            install_statements(&ddl, &ordered).expect("ordered"),
            install_statements(&ddl, &reordered).expect("reordered")
        );

        let sql = render_install_sql(&ddl, &ordered).expect("render install sql");
        assert_eq!(
            sql,
            render_install_sql(&ddl, &reordered).expect("reordered sql")
        );
        assert!(sql.starts_with("BEGIN;\n\n"));
        assert!(sql.ends_with("\n\nCOMMIT;\n"));
        assert!(!sql.contains("-- auditstream:manifest"));

        let bad = vec![TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["id".to_owned()],
        )];
        let err1 = install_statements(&ddl, &bad).expect_err("reject excluded PK");
        assert!(err1.to_string().contains("primary key"), "{err1}");
        let err2 = render_install_sql(&ddl, &bad).expect_err("reject excluded PK");
        assert!(err2.to_string().contains("primary key"), "{err2}");

        let duplicate_conflicting = vec![
            TableTriggerSpec::new(
                "public",
                "invoice",
                vec!["id".to_owned()],
                vec!["password_hash".to_owned()],
            ),
            TableTriggerSpec::new(
                "public",
                "invoice",
                vec!["id".to_owned()],
                vec!["ssn".to_owned()],
            ),
        ];
        let dup_err1 = install_statements(&ddl, &duplicate_conflicting)
            .expect_err("reject duplicate (schema, table) specs");
        assert!(
            dup_err1
                .to_string()
                .contains("duplicate table specification for `public.invoice`"),
            "{dup_err1}"
        );
        let dup_err2 = render_install_sql(&ddl, &duplicate_conflicting)
            .expect_err("render_install_sql must reject duplicate (schema, table) specs");
        assert!(
            dup_err2
                .to_string()
                .contains("duplicate table specification for `public.invoice`"),
            "{dup_err2}"
        );
        let fixed_now = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::November, 26).expect("date"),
            Time::from_hms(12, 0, 0).expect("time"),
        )
        .assume_utc();
        let dup_opts = MigrationOptions::new(
            MigrationFormat::Sql,
            PathBuf::from("unused"),
            fixed_now,
            ddl,
            duplicate_conflicting,
        );
        let dup_err3 =
            preview(&dup_opts).expect_err("preview must reject duplicate (schema, table) specs");
        assert!(
            dup_err3
                .to_string()
                .contains("duplicate table specification for `public.invoice`"),
            "{dup_err3}"
        );
    }
}
