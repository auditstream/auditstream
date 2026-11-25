//! Migration writers for Prisma, Drizzle (0.45 and 1.x layouts), and plain SQL.
//!
//! Composes [`crate::ddl::generate`] and [`crate::triggers::generate`] (plus idempotent
//! `DROP TRIGGER IF EXISTS` statements when tables are removed from a previous Auditstream
//! selection) into deterministic, re-runnable migration files that each ORM's own migrator
//! executes unchanged.

pub mod drizzle;
mod function_digest;
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

const SET_LOCK_TIMEOUT: &str = "DO $$
BEGIN
  IF pg_catalog.current_setting('lock_timeout') = '0' THEN
    SET LOCAL lock_timeout = '5s';
  END IF;
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

/// Renders the migration SQL that [`write()`] would generate without creating directories or
/// writing any files on disk.
///
/// # Errors
///
/// Returns [`MigrationError::Io`] on filesystem read errors or [`MigrationError::InvalidMetadata`]
/// if an existing Drizzle `meta/_journal.json` or Auditstream manifest cannot be parsed.
pub fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
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
/// if an existing Drizzle `meta/_journal.json` cannot be parsed.
pub fn write(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
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

fn build_manifest(
    options: &MigrationOptions,
    tables: BTreeSet<(String, String)>,
    statements: &[String],
) -> Manifest {
    let mut app_roles = options.ddl.app_roles.clone();
    app_roles.sort();
    app_roles.dedup();

    let mut tables_sorted = options.tables.clone();
    tables_sorted.sort_by(|a, b| a.schema.cmp(&b.schema).then_with(|| a.table.cmp(&b.table)));

    let tables_json: Vec<Value> = tables_sorted
        .iter()
        .map(|spec| {
            let mut excl = spec.excluded_columns.clone();
            excl.sort();
            excl.dedup();
            json!({
                "excluded_columns": excl,
                "is_partitioned": spec.is_partitioned,
                "primary_key": spec.primary_key,
                "schema": spec.schema,
                "table": spec.table,
            })
        })
        .collect();

    let value = json!({
        "app_roles": app_roles,
        "function_sql_sha256": function_digest::from_statements(statements),
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
    let mut ddl_stmts = ddl::generate(&options.ddl);
    // Keep D2's temporary `auditstream_owner` self-grant active while D3 creates/alters functions,
    // then run D2's cleanup statement at the very end.
    let role_cleanup = ddl_stmts.pop();

    let mut tables_sorted = options.tables.clone();
    tables_sorted.sort_by(|a, b| a.schema.cmp(&b.schema).then_with(|| a.table.cmp(&b.table)));

    let trigger_stmts = triggers::generate(&TriggerOptions::new(
        options.ddl.schema.clone(),
        tables_sorted,
    ));

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

    let drizzle_check = (options.format == MigrationFormat::Drizzle)
        .then(|| drizzle::CHECK_LOCK_TIMEOUT.to_owned());
    std::iter::once(SET_LOCK_TIMEOUT.to_owned())
        .chain(drizzle_check)
        .chain(ddl_stmts)
        .chain(trigger_stmts)
        .chain(drop_stmts)
        .chain(role_cleanup)
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
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let migration_sql = path.join("migration.sql");
        if migration_sql.is_file() {
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
        options.out_dir.join(format!("{ts}_auditstream")).exists()
    });
    let folder = options.out_dir.join(format!("{ts}_auditstream"));
    fs::create_dir(&folder)?;
    let file_path = folder.join("migration.sql");
    fs::write(&file_path, rendered)?;

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
