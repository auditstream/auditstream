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
    DrizzleLayout, Manifest, MigrationError, MigrationOptions, PlannedMigration, PreviewOutcome,
    WriteOutcome, find_newest_folder_migration, header_comment, parse_manifest_from_file,
    plan_migration, preview_with, write_folder_migration,
};

pub(crate) fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
    let layout = options
        .drizzle_layout
        .unwrap_or_else(|| detect_layout(&options.out_dir));
    let newest = match layout {
        DrizzleLayout::V045 => find_newest_v045_migration(&options.out_dir)?,
        DrizzleLayout::V1 => find_newest_folder_migration(&options.out_dir)?,
    };

    Ok(preview_with(options, newest, render_drizzle_sql))
}

fn read_journal_json(journal_path: &Path) -> Result<Value, MigrationError> {
    let raw = fs::read_to_string(journal_path)?;
    serde_json::from_str(&raw).map_err(|err| {
        MigrationError::InvalidMetadata(format!(
            "invalid Drizzle journal {}: {err}",
            journal_path.display()
        ))
    })
}

fn journal_entries<'a>(
    journal: &'a Value,
    journal_path: &Path,
) -> Result<&'a [Value], MigrationError> {
    journal
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            MigrationError::InvalidMetadata(format!(
                "Drizzle journal {} is missing an `entries` array",
                journal_path.display()
            ))
        })
}

fn find_newest_v045_in_entries(
    out_dir: &Path,
    entries: &[Value],
) -> Result<Option<(PathBuf, Manifest)>, MigrationError> {
    for entry in entries.iter().rev() {
        if let Some(tag) = entry.get("tag").and_then(Value::as_str) {
            let sql_path = out_dir.join(format!("{tag}.sql"));
            if sql_path.is_file()
                && let Some(manifest) = parse_manifest_from_file(&sql_path)?
            {
                return Ok(Some((sql_path, manifest)));
            }
        }
    }
    Ok(None)
}

fn find_newest_v045_migration(
    out_dir: &Path,
) -> Result<Option<(PathBuf, Manifest)>, MigrationError> {
    let journal_path = out_dir.join("meta").join("_journal.json");
    if !journal_path.is_file() {
        return Ok(None);
    }
    let journal = read_journal_json(&journal_path)?;
    let entries = journal_entries(&journal, &journal_path)?;
    find_newest_v045_in_entries(out_dir, entries)
}

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
    let meta_dir = options.out_dir.join("meta");
    let journal_path = meta_dir.join("_journal.json");

    let mut journal: Value = if journal_path.is_file() {
        read_journal_json(&journal_path)?
    } else {
        json!({
            "version": "7",
            "dialect": "postgresql",
            "entries": []
        })
    };

    let entries = journal_entries(&journal, &journal_path)?;
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

    let newest = find_newest_v045_in_entries(&options.out_dir, entries)?;
    let (rendered, dropped_tables) = match plan_migration(options, newest) {
        PlannedMigration::UpToDate { existing } => return Ok(WriteOutcome::UpToDate { existing }),
        PlannedMigration::Changed {
            manifest,
            dropped_tables,
            statements,
        } => (render_drizzle_sql(&manifest, &statements), dropped_tables),
    };

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
    let newest = find_newest_folder_migration(&options.out_dir)?;
    match plan_migration(options, newest) {
        PlannedMigration::UpToDate { existing } => Ok(WriteOutcome::UpToDate { existing }),
        PlannedMigration::Changed {
            manifest,
            dropped_tables,
            statements,
        } => {
            let rendered = render_drizzle_sql(&manifest, &statements);
            write_folder_migration(options, &rendered, dropped_tables)
        }
    }
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
