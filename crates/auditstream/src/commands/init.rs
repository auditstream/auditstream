//! `auditstream init` command implementation.
//!
//! Introspects PostgreSQL via `auditstream-pg`, validates the chosen tables and excluded columns,
//! writes or previews migrations for Prisma, Drizzle, or plain SQL, and creates or updates
//! `auditstream.toml` using `toml_edit` without touching comments, other keys, or secrets.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use auditstream_pg::ddl::DdlOptions;
use auditstream_pg::introspect::{self, IntrospectError, IntrospectOptions, TableInfo, TableKind};
use auditstream_pg::migrations::{
    self, MigrationFormat as PgMigrationFormat, MigrationOptions, PreviewOutcome, WriteOutcome,
};
use clap::Args as ClapArgs;
use time::OffsetDateTime;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use crate::commands::common::{
    connect_database, parse_exclude_refs, parse_table_refs, require_database_url,
    run_async as run_command_async, sanitize_terminal, table_trigger_spec,
};
use crate::config::{Config, MigrationFormat};
use crate::exit::{Exit, ExitCodeExt, write_stdout};

const PRISMA_CONFIG_FILES: [&str; 6] = [
    "prisma.config.ts",
    "prisma.config.js",
    "prisma.config.mjs",
    "prisma.config.cjs",
    "prisma.config.mts",
    "prisma.config.cts",
];

const DRIZZLE_CONFIG_FILES: [&str; 7] = [
    "drizzle.config.ts",
    "drizzle.config.js",
    "drizzle.config.mjs",
    "drizzle.config.cjs",
    "drizzle.config.mts",
    "drizzle.config.cts",
    "drizzle.config.json",
];

/// Arguments for `auditstream init`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Table to audit (`[schema.]table`, defaults to `public`), repeatable.
    #[arg(long, value_name = "[SCHEMA.]TABLE")]
    pub(crate) table: Vec<String>,

    /// Column to exclude from row images (`schema.table.column`), repeatable.
    #[arg(long, value_name = "SCHEMA.TABLE.COLUMN")]
    pub(crate) exclude: Vec<String>,

    /// Migration format to generate (`prisma`, `drizzle`, or `sql`).
    #[arg(long, value_enum)]
    pub(crate) migrations: Option<MigrationFormat>,

    /// Output directory for generated migration files (defaults per format).
    #[arg(long, value_name = "DIR")]
    pub(crate) out: Option<PathBuf>,

    /// Print generated migration SQL to stdout without writing any files or updating config.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// Runs the `init` command.
pub(crate) fn run(args: &Args, config: &Config) -> anyhow::Result<()> {
    run_command_async(run_async(args, config))
}

#[allow(clippy::too_many_lines)]
async fn run_async(args: &Args, config: &Config) -> anyhow::Result<()> {
    let db_url = require_database_url(config)?;

    // Parse table and exclusion specs up front so syntax mistakes exit 2 immediately.
    // CLI `--table` flags default unqualified names to `public.<table>`; config entries stay explicit.
    let parsed_tables = parse_table_refs(&config.capture.tables, args.table.is_empty())?;
    let parsed_excludes = parse_exclude_refs(&config.capture.exclude, args.exclude.is_empty())?;

    let pool = connect_database(db_url).await?;

    let introspect_opts = IntrospectOptions::with_audit_schema(&config.schema);

    // When no tables are selected, introspect all user schemas, list them on stderr, and exit 2.
    if parsed_tables.is_empty() {
        let schemas = introspect::user_schemas(&pool, &introspect_opts)
            .await
            .map_err(|err| anyhow!("failed to list database schemas: {err}"))
            .exit_code(Exit::Database)?;
        let schema_refs: Vec<&str> = schemas.iter().map(String::as_str).collect();
        let discovered = if schema_refs.is_empty() {
            Vec::new()
        } else {
            introspect::tables_with_options(&pool, &schema_refs, &introspect_opts)
                .await
                .map_err(|err| anyhow!("failed to introspect database tables: {err}"))
                .exit_code(Exit::Database)?
        };
        pool.close().await;

        if discovered.is_empty() {
            eprintln!("Available tables: (none)");
        } else {
            eprintln!("Available tables:");
            for table in &discovered {
                eprintln!(
                    "  {}.{}",
                    sanitize_terminal(&table.schema),
                    sanitize_terminal(&table.name)
                );
            }
        }
        return Err(anyhow!(
            "no tables selected: pass --table <[schema.]table> or set capture.tables in auditstream.toml"
        ))
        .exit_code(Exit::Usage);
    }

    let format = match config.migrations.format {
        Some(format) => format,
        None => match detect_migration_format(Path::new(".")) {
            Ok(detected) => {
                eprintln!("detected migration format: {}", format_name(detected));
                detected
            }
            Err(err) => {
                pool.close().await;
                return Err(err);
            }
        },
    };

    // Collect every unique schema referenced by --table or --exclude.
    let mut referenced_schemas: BTreeSet<&str> = BTreeSet::new();
    for (schema, _) in &parsed_tables {
        referenced_schemas.insert(schema.as_str());
    }
    for (schema, _, _) in &parsed_excludes {
        referenced_schemas.insert(schema.as_str());
    }
    let schema_list: Vec<&str> = referenced_schemas.into_iter().collect();

    let introspected = introspect::tables_with_options(&pool, &schema_list, &introspect_opts)
        .await
        .map_err(|err| match err {
            IntrospectError::UnknownSchema { schema } => {
                crate::exit::tag_error(anyhow!("unknown schema `{schema}`"), Exit::Usage)
            }
            other => crate::exit::tag_error(
                anyhow!("failed to introspect database tables: {other}"),
                Exit::Database,
            ),
        })?;
    pool.close().await;

    let by_key: BTreeMap<(&str, &str), &TableInfo> = introspected
        .iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();

    // Deduplicate and validate selected tables in deterministic order.
    let mut selected_set: BTreeSet<(String, String)> = BTreeSet::new();
    for (schema, table) in &parsed_tables {
        if !by_key.contains_key(&(schema.as_str(), table.as_str())) {
            return Err(anyhow!("unknown table `{schema}.{table}`")).exit_code(Exit::Usage);
        }
        selected_set.insert((schema.clone(), table.clone()));
    }

    // Validate excluded columns against introspected table metadata and selected tables.
    let mut excludes_by_table: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (schema, table, column) in &parsed_excludes {
        let Some(info) = by_key.get(&(schema.as_str(), table.as_str())) else {
            return Err(anyhow!(
                "unknown table `{schema}.{table}` in excluded column `{schema}.{table}.{column}`"
            ))
            .exit_code(Exit::Usage);
        };
        if !selected_set.contains(&(schema.clone(), table.clone())) {
            return Err(anyhow!(
                "excluded column `{schema}.{table}.{column}` refers to table `{schema}.{table}` which is not selected"
            ))
            .exit_code(Exit::Usage);
        }
        if !info.columns.iter().any(|c| c.name == *column) {
            return Err(anyhow!("unknown column `{schema}.{table}.{column}`"))
                .exit_code(Exit::Usage);
        }
        excludes_by_table
            .entry((schema.clone(), table.clone()))
            .or_default()
            .insert(column.clone());
    }

    // Build deterministic TableTriggerSpecs + canonical config lists.
    let mut trigger_specs = Vec::with_capacity(selected_set.len());
    let mut canonical_tables = Vec::with_capacity(selected_set.len());
    let mut canonical_excludes = Vec::new();

    for (schema, table) in &selected_set {
        let info = by_key[&(schema.as_str(), table.as_str())];
        let excluded_cols: Vec<String> = excludes_by_table
            .get(&(schema.clone(), table.clone()))
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();

        canonical_tables.push(format!("{schema}.{table}"));
        for col in &excluded_cols {
            canonical_excludes.push(format!("{schema}.{table}.{col}"));
        }

        trigger_specs.push(table_trigger_spec(
            schema.clone(),
            table.clone(),
            info.primary_key.clone(),
            excluded_cols,
            info.kind == TableKind::Partitioned,
        ));
    }

    let out_dir = config
        .migrations
        .dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(default_out_dir(format)));
    let pg_format = to_pg_format(format);
    let ddl = DdlOptions::new(config.schema.clone(), Vec::new());
    let migration_opts = MigrationOptions::new(
        pg_format,
        &out_dir,
        OffsetDateTime::now_utc(),
        ddl,
        trigger_specs,
    );

    if args.dry_run {
        match migrations::preview(&migration_opts).exit_code(Exit::Io)? {
            PreviewOutcome::Sql { sql, .. } => {
                let mut out = io::stdout().lock();
                if sql.ends_with('\n') {
                    write_stdout(&mut out, sql.as_bytes())?;
                } else {
                    let mut buf = sql.into_bytes();
                    buf.push(b'\n');
                    write_stdout(&mut out, &buf)?;
                }
            }
            PreviewOutcome::UpToDate { existing } => {
                eprintln!("up to date ({})", existing.display());
            }
        }
        return Ok(());
    }

    match migrations::write(&migration_opts).exit_code(Exit::Io)? {
        WriteOutcome::UpToDate { existing } => {
            eprintln!("up to date ({})", existing.display());
        }
        WriteOutcome::Written { files, .. } => {
            update_config_file(
                &config.file_path,
                &canonical_tables,
                &canonical_excludes,
                format,
                &out_dir,
            )?;

            tracing::info!(
                config = %config.file_path.display(),
                files = files.len(),
                "wrote auditstream migration and updated configuration"
            );

            let mut out = io::stdout().lock();
            for path in &files {
                let line = format!("{}\n", path.display());
                if !write_stdout(&mut out, line.as_bytes())? {
                    break;
                }
            }
        }
    }

    Ok(())
}

const fn default_out_dir(format: MigrationFormat) -> &'static str {
    match format {
        MigrationFormat::Prisma => "prisma/migrations",
        MigrationFormat::Drizzle => "drizzle",
        MigrationFormat::Sql => "migrations",
    }
}

const fn format_name(format: MigrationFormat) -> &'static str {
    match format {
        MigrationFormat::Prisma => "prisma",
        MigrationFormat::Drizzle => "drizzle",
        MigrationFormat::Sql => "sql",
    }
}

const fn to_pg_format(format: MigrationFormat) -> PgMigrationFormat {
    match format {
        MigrationFormat::Prisma => PgMigrationFormat::Prisma,
        MigrationFormat::Drizzle => PgMigrationFormat::Drizzle,
        MigrationFormat::Sql => PgMigrationFormat::Sql,
    }
}

fn string_array(items: &[String]) -> Array {
    let mut array = Array::new();
    for item in items {
        array.push(Value::from(item.as_str()));
    }
    array
}

fn ensure_table<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut Table {
    if !doc.contains_key(key) || !doc[key].is_table() {
        let mut tbl = Table::new();
        tbl.set_implicit(false);
        doc[key] = Item::Table(tbl);
    }
    doc[key]
        .as_table_mut()
        .expect("table item was just inserted")
}

fn update_config_file(
    path: &Path,
    tables: &[String],
    exclude: &[String],
    format: MigrationFormat,
    dir: &Path,
) -> anyhow::Result<()> {
    let mut doc = if path.exists() {
        let raw = fs::read_to_string(path)
            .map_err(|err| anyhow!("failed to read {}: {err}", path.display()))
            .exit_code(Exit::Io)?;
        raw.parse::<DocumentMut>()
            .map_err(|err| anyhow!("failed to parse {}: {err}", path.display()))
            .exit_code(Exit::Usage)?
    } else {
        DocumentMut::new()
    };

    let capture = ensure_table(&mut doc, "capture");
    capture["tables"] = value(string_array(tables));
    capture["exclude"] = value(string_array(exclude));

    let migrations = ensure_table(&mut doc, "migrations");
    migrations["format"] = value(format_name(format));
    let dir_str = dir.to_string_lossy();
    migrations["dir"] = value(dir_str.as_ref());

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|err| anyhow!("failed to create directory {}: {err}", parent.display()))
            .exit_code(Exit::Io)?;
    }

    let rendered = doc.to_string();
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("auditstream.toml"),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, rendered.as_bytes())
        .map_err(|err| anyhow!("failed to write {}: {err}", tmp_path.display()))
        .exit_code(Exit::Io)?;
    fs::rename(&tmp_path, path)
        .map_err(|err| anyhow!("failed to replace {}: {err}", path.display()))
        .exit_code(Exit::Io)?;

    Ok(())
}

fn detect_migration_format(dir: &Path) -> anyhow::Result<MigrationFormat> {
    let has_prisma = dir.join("prisma").is_dir()
        || PRISMA_CONFIG_FILES
            .iter()
            .any(|name| dir.join(name).is_file());
    let has_drizzle = dir.join("drizzle").is_dir()
        || DRIZZLE_CONFIG_FILES
            .iter()
            .any(|name| dir.join(name).is_file());

    match (has_prisma, has_drizzle) {
        (true, true) => Err(anyhow!(
            "multiple migration layouts detected (prisma and drizzle): pass --migrations prisma|drizzle|sql or set migrations.format in auditstream.toml"
        ))
        .exit_code(Exit::Usage),
        (true, false) => Ok(MigrationFormat::Prisma),
        (false, true) => Ok(MigrationFormat::Drizzle),
        (false, false) if dir.join("migrations").is_dir() => Ok(MigrationFormat::Sql),
        (false, false) => Err(anyhow!(
            "missing migration format: pass --migrations prisma|drizzle|sql or set migrations.format in auditstream.toml"
        ))
        .exit_code(Exit::Usage),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::detect_migration_format;
    use crate::config::MigrationFormat;
    use crate::exit::{Exit, exit_code_for};

    #[test]
    fn detect_migration_format_identifies_prisma_drizzle_and_sql_layouts() {
        let prisma_dir = tempdir().unwrap();
        fs::create_dir(prisma_dir.path().join("prisma")).unwrap();
        assert_eq!(
            detect_migration_format(prisma_dir.path()).unwrap(),
            MigrationFormat::Prisma
        );

        let prisma_cfg = tempdir().unwrap();
        fs::write(prisma_cfg.path().join("prisma.config.ts"), "").unwrap();
        assert_eq!(
            detect_migration_format(prisma_cfg.path()).unwrap(),
            MigrationFormat::Prisma
        );

        let drizzle_cfg = tempdir().unwrap();
        fs::write(drizzle_cfg.path().join("drizzle.config.ts"), "").unwrap();
        assert_eq!(
            detect_migration_format(drizzle_cfg.path()).unwrap(),
            MigrationFormat::Drizzle
        );

        let drizzle_dir = tempdir().unwrap();
        fs::create_dir(drizzle_dir.path().join("drizzle")).unwrap();
        assert_eq!(
            detect_migration_format(drizzle_dir.path()).unwrap(),
            MigrationFormat::Drizzle
        );

        // Framework marker takes precedence over a generic migrations/ folder.
        let drizzle_and_migrations = tempdir().unwrap();
        fs::write(drizzle_and_migrations.path().join("drizzle.config.mjs"), "").unwrap();
        fs::create_dir(drizzle_and_migrations.path().join("migrations")).unwrap();
        assert_eq!(
            detect_migration_format(drizzle_and_migrations.path()).unwrap(),
            MigrationFormat::Drizzle
        );

        let sql_dir = tempdir().unwrap();
        fs::create_dir(sql_dir.path().join("migrations")).unwrap();
        assert_eq!(
            detect_migration_format(sql_dir.path()).unwrap(),
            MigrationFormat::Sql
        );
    }

    #[test]
    fn detect_migration_format_rejects_ambiguous_or_unknown_layouts() {
        let both = tempdir().unwrap();
        fs::create_dir(both.path().join("prisma")).unwrap();
        fs::write(both.path().join("drizzle.config.ts"), "").unwrap();
        let err = detect_migration_format(both.path()).unwrap_err();
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("multiple migration layouts"));

        let empty = tempdir().unwrap();
        let err = detect_migration_format(empty.path()).unwrap_err();
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("missing migration format"));
    }
}
