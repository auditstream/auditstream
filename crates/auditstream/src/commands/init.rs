//! `auditstream init` command implementation.
//!
//! Introspects PostgreSQL via `auditstream-pg`, validates the chosen tables and excluded columns,
//! writes or previews migrations for Prisma, Drizzle, or plain SQL, and creates or updates
//! `auditstream.toml` using `toml_edit` without touching comments, other keys, or secrets.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
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
use uuid::Uuid;

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

    // When no tables are selected, introspect all user schemas, list them on stderr, and exit 2.
    if parsed_tables.is_empty() {
        let pool = connect_database(db_url).await?;
        let introspect_opts = IntrospectOptions::with_audit_schema(&config.schema);
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

    // Resolve or autodetect the migration format offline before opening a database connection
    // so missing or ambiguous layouts exit 2 immediately without requiring a live Postgres instance.
    let (format, autodetected) = match config.migrations.format {
        Some(format) => (format, false),
        None => (detect_migration_format(Path::new("."))?, true),
    };

    let pool = connect_database(db_url).await?;
    if autodetected {
        eprintln!("detected migration format: {}", format_name(format));
    }
    let introspect_opts = IntrospectOptions::with_audit_schema(&config.schema);

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
        if info.primary_key.iter().any(|pk| pk == column) {
            return Err(anyhow!(
                "cannot exclude primary key column `{schema}.{table}.{column}`: primary key columns are required to build `record_id`"
            ))
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
                eprintln!(
                    "up to date ({})",
                    sanitize_terminal(&existing.display().to_string())
                );
            }
        }
        return Ok(());
    }

    match migrations::write(&migration_opts).exit_code(Exit::Io)? {
        WriteOutcome::UpToDate { existing } => {
            eprintln!(
                "up to date ({})",
                sanitize_terminal(&existing.display().to_string())
            );
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

fn read_existing_config_doc(path: &Path) -> anyhow::Result<DocumentMut> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(anyhow!(
            "refusing to follow symlink at config path {}",
            path.display()
        ))
        .exit_code(Exit::Io),
        Ok(meta) if !meta.file_type().is_file() => Err(anyhow!(
            "config path {} is not a regular file",
            path.display()
        ))
        .exit_code(Exit::Io),
        Ok(_) => {
            let mut read_opts = OpenOptions::new();
            read_opts.read(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::custom_flags(&mut read_opts, libc::O_NOFOLLOW);
            let mut existing = read_opts
                .open(path)
                .map_err(|err| anyhow!("failed to read {}: {err}", path.display()))
                .exit_code(Exit::Io)?;
            if !existing
                .metadata()
                .map_err(|err| anyhow!("failed to inspect {}: {err}", path.display()))
                .exit_code(Exit::Io)?
                .file_type()
                .is_file()
            {
                return Err(anyhow!(
                    "config path {} is not a regular file",
                    path.display()
                ))
                .exit_code(Exit::Io);
            }
            let mut raw = String::new();
            existing
                .read_to_string(&mut raw)
                .map_err(|err| anyhow!("failed to read {}: {err}", path.display()))
                .exit_code(Exit::Io)?;
            raw.parse::<DocumentMut>()
                .map_err(|err| anyhow!("failed to parse {}: {}", path.display(), err.message()))
                .exit_code(Exit::Usage)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(anyhow!("failed to inspect {}: {err}", path.display())).exit_code(Exit::Io),
    }
}

fn update_config_file(
    path: &Path,
    tables: &[String],
    exclude: &[String],
    format: MigrationFormat,
    dir: &Path,
) -> anyhow::Result<()> {
    let mut doc = read_existing_config_doc(path)?;

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
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("auditstream.toml");

    // Exclusive creation with random entropy and mode 0600 on Unix prevents symlink TOCTOU and
    // predictable-path collisions while keeping `database_url` secrets private.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    let mut attempt = 0_u32;
    let (tmp_path, mut file) = loop {
        let candidate =
            path.with_file_name(format!(".{file_name}.tmp.{}", Uuid::new_v4().simple()));
        match options.open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempt < 64 => {
                attempt += 1;
            }
            Err(err) => {
                return Err(anyhow!("failed to create {}: {err}", candidate.display()))
                    .exit_code(Exit::Io);
            }
        }
    };

    if let Err(err) = file
        .write_all(rendered.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(anyhow!("failed to write {}: {err}", tmp_path.display())).exit_code(Exit::Io);
    }
    drop(file);

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(anyhow!("failed to replace {}: {err}", path.display())).exit_code(Exit::Io);
    }

    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let _ = fs::File::open(parent).and_then(|dir_file| dir_file.sync_all());
    }

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

    #[cfg(unix)]
    #[test]
    fn update_config_file_preserves_0600_and_resists_symlink_at_temp_path() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::path::Path;

        use super::update_config_file;

        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("auditstream.toml");
        fs::write(
            &cfg_path,
            "database_url = \"postgres://u:p@localhost/db\"\n",
        )
        .unwrap();
        fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o600)).unwrap();

        // Pre-plant symlinks at both the legacy PID path and attempt-0 path pointing to a victim file.
        let victim = dir.path().join("victim.txt");
        fs::write(&victim, "DO_NOT_OVERWRITE").unwrap();
        let pid = std::process::id();
        symlink(
            &victim,
            dir.path().join(format!(".auditstream.toml.tmp.{pid}")),
        )
        .unwrap();
        symlink(
            &victim,
            dir.path().join(format!(".auditstream.toml.tmp.{pid}.0")),
        )
        .unwrap();

        update_config_file(
            &cfg_path,
            &["public.invoice".to_owned()],
            &[],
            MigrationFormat::Sql,
            Path::new("migrations"),
        )
        .expect("update_config_file succeeds");

        let mode = fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "auditstream.toml must retain mode 0600, got {mode:o}"
        );
        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "DO_NOT_OVERWRITE",
            "symlink target must never be overwritten"
        );
        let updated = fs::read_to_string(&cfg_path).unwrap();
        assert!(
            updated.contains("database_url = \"postgres://u:p@localhost/db\""),
            "database_url must be preserved: {updated}"
        );
        assert!(updated.contains("public.invoice"), "{updated}");
        assert!(updated.contains("migrations"), "{updated}");

        // Fresh config file creation also uses mode 0600.
        let fresh_path = dir.path().join("fresh.toml");
        update_config_file(
            &fresh_path,
            &["public.invoice".to_owned()],
            &[],
            MigrationFormat::Sql,
            Path::new("migrations"),
        )
        .expect("create fresh config");
        let fresh_mode = fs::metadata(&fresh_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            fresh_mode, 0o600,
            "new auditstream.toml must be created with mode 0600, got {fresh_mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_config_file_refuses_target_symlinks_and_never_leaks_toml_secrets() {
        use std::os::unix::fs::symlink;
        use std::path::Path;

        use super::update_config_file;

        let dir = tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        fs::write(&victim, "DO_NOT_OVERWRITE").unwrap();

        let sym_live = dir.path().join("live_sym.toml");
        symlink(&victim, &sym_live).unwrap();
        let err = update_config_file(
            &sym_live,
            &["public.invoice".to_owned()],
            &[],
            MigrationFormat::Sql,
            Path::new("migrations"),
        )
        .expect_err("live symlink config target must be rejected");
        assert!(err.to_string().contains("symlink"), "{err}");

        let sym_dangling = dir.path().join("dangling_sym.toml");
        symlink(dir.path().join("does-not-exist.toml"), &sym_dangling).unwrap();
        let err = update_config_file(
            &sym_dangling,
            &["public.invoice".to_owned()],
            &[],
            MigrationFormat::Sql,
            Path::new("migrations"),
        )
        .expect_err("dangling symlink config target must be rejected");
        assert!(err.to_string().contains("symlink"), "{err}");

        let secret = "TOML_EDIT_SECRET_PW_777";
        let broken_path = dir.path().join("broken.toml");
        fs::write(
            &broken_path,
            format!("database_url = \"postgresql://app:{secret}@db.example.com/prod\n"),
        )
        .unwrap();
        let err = update_config_file(
            &broken_path,
            &["public.invoice".to_owned()],
            &[],
            MigrationFormat::Sql,
            Path::new("migrations"),
        )
        .expect_err("broken TOML must fail");
        assert!(
            !err.to_string().contains(secret),
            "toml_edit parse error leaked secret: {err}"
        );
    }
}
