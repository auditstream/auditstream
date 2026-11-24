//! Drizzle migration writer supporting both Drizzle 0.45 (`meta/_journal.json` +
//! `<NNNN>_auditstream.sql`) and Drizzle 1.x (`<YYYYMMDDHHMMSS>_auditstream/migration.sql`)
//! layouts.
//!
//! Every SQL statement is placed in its own `--> statement-breakpoint` chunk so Drizzle's
//! migrator sends each statement individually over the extended protocol.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{
    DrizzleLayout, Manifest, MigrationError, MigrationOptions, WriteOutcome, build_manifest,
    compose_statements, compute_dropped_tables, header_comment, next_unique_timestamp,
    parse_manifest_from_file,
};

/// Statement separator recognized by both Drizzle 0.45 and Drizzle 1.x migrators.
pub const STATEMENT_BREAKPOINT: &str = "--> statement-breakpoint";

/// Detects the Drizzle layout for `out_dir`: returns [`DrizzleLayout::V045`] when
/// `meta/_journal.json` exists, otherwise [`DrizzleLayout::V1`].
#[must_use]
pub fn detect_layout(out_dir: &Path) -> DrizzleLayout {
    if out_dir.join("meta").join("_journal.json").is_file() {
        DrizzleLayout::V045
    } else {
        DrizzleLayout::V1
    }
}

pub(crate) fn write(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    let layout = options
        .drizzle_layout
        .unwrap_or_else(|| detect_layout(&options.out_dir));
    match layout {
        DrizzleLayout::V045 => write_v045(options),
        DrizzleLayout::V1 => write_v1(options),
    }
}

fn write_v045(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    let current = build_manifest(options);
    let meta_dir = options.out_dir.join("meta");
    let journal_path = meta_dir.join("_journal.json");

    let mut journal: Value = if journal_path.is_file() {
        let raw = fs::read_to_string(&journal_path)?;
        serde_json::from_str(&raw).map_err(|err| {
            MigrationError::InvalidMetadata(format!(
                "invalid Drizzle journal {}: {err}",
                journal_path.display()
            ))
        })?
    } else {
        json!({
            "version": "7",
            "dialect": "postgresql",
            "entries": []
        })
    };

    let entries = journal
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            MigrationError::InvalidMetadata(format!(
                "Drizzle journal {} is missing an `entries` array",
                journal_path.display()
            ))
        })?;

    let mut newest: Option<(PathBuf, Manifest)> = None;
    let mut max_idx: Option<u64> = None;
    let mut max_when: Option<i64> = None;

    for entry in entries {
        if let Some(idx) = entry.get("idx").and_then(Value::as_u64) {
            max_idx = Some(max_idx.map_or(idx, |m| m.max(idx)));
        }
        if let Some(when) = entry.get("when").and_then(Value::as_i64) {
            max_when = Some(max_when.map_or(when, |m| m.max(when)));
        }
    }

    for entry in entries.iter().rev() {
        if let Some(tag) = entry.get("tag").and_then(Value::as_str) {
            let sql_path = options.out_dir.join(format!("{tag}.sql"));
            if sql_path.is_file()
                && let Some(manifest) = parse_manifest_from_file(&sql_path)?
            {
                newest = Some((sql_path, manifest));
                break;
            }
        }
    }

    if let Some((ref existing_path, ref prev_manifest)) = newest
        && prev_manifest.canonical_json == current.canonical_json
        && options.removed_tables.is_empty()
    {
        return Ok(WriteOutcome::UpToDate {
            existing: existing_path.clone(),
        });
    }

    let dropped_tables = compute_dropped_tables(
        newest.as_ref().map(|(_, m)| m),
        &current,
        &options.removed_tables,
    );
    let statements = compose_statements(options, &dropped_tables);
    let rendered = render_drizzle_sql(&current, &statements);

    let next_idx = max_idx.map_or(0, |m| m + 1);
    let now_millis = i64::try_from(options.now.unix_timestamp_nanos() / 1_000_000).unwrap_or(0);
    let next_when = max_when.map_or(now_millis, |m| now_millis.max(m + 1));
    let tag = format!("{next_idx:04}_auditstream");

    fs::create_dir_all(&meta_dir)?;
    let sql_path = options.out_dir.join(format!("{tag}.sql"));
    fs::write(&sql_path, rendered)?;

    let entries_mut = journal
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .expect("validated entries array");
    entries_mut.push(json!({
        "idx": next_idx,
        "version": "7",
        "when": next_when,
        "tag": tag,
        "breakpoints": true
    }));

    let mut journal_json = serde_json::to_string_pretty(&journal)
        .expect("Drizzle journal JSON serialization cannot fail");
    journal_json.push('\n');
    fs::write(&journal_path, journal_json)?;

    Ok(WriteOutcome::Written {
        files: vec![sql_path, journal_path],
        dropped_tables,
    })
}

fn write_v1(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    let current = build_manifest(options);
    let newest = find_newest_v1_migration(&options.out_dir)?;

    if let Some((ref existing_path, ref prev_manifest)) = newest
        && prev_manifest.canonical_json == current.canonical_json
        && options.removed_tables.is_empty()
    {
        return Ok(WriteOutcome::UpToDate {
            existing: existing_path.clone(),
        });
    }

    let dropped_tables = compute_dropped_tables(
        newest.as_ref().map(|(_, m)| m),
        &current,
        &options.removed_tables,
    );
    let statements = compose_statements(options, &dropped_tables);
    let rendered = render_drizzle_sql(&current, &statements);

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

pub(crate) fn render_drizzle_sql(manifest: &Manifest, statements: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&header_comment(manifest));
    out.push('\n');
    for (idx, stmt) in statements.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
            out.push_str(STATEMENT_BREAKPOINT);
            out.push('\n');
        }
        out.push_str(stmt);
        out.push(';');
    }
    out.push('\n');
    out
}

fn find_newest_v1_migration(out_dir: &Path) -> Result<Option<(PathBuf, Manifest)>, MigrationError> {
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
