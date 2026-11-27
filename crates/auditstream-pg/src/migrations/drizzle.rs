//! Drizzle migration writer supporting both Drizzle 0.45 (`meta/_journal.json` +
//! `<NNNN>_auditstream.sql`) and Drizzle 1.x (`<YYYYMMDDHHMMSS>_auditstream/migration.sql`)
//! layouts.
//!
//! Every SQL statement is placed in its own `--> statement-breakpoint` chunk so Drizzle's
//! migrator sends each statement individually over the extended protocol.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    DrizzleLayout, Manifest, MigrationError, MigrationOptions, PlannedMigration, PreviewOutcome,
    WriteOutcome, find_newest_folder_migration, header_comment, parse_manifest_from_file,
    plan_migration, reject_symlink, write_folder_migration, write_new_file,
};

pub(crate) fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
    let layout = options
        .drizzle_layout
        .unwrap_or_else(|| detect_layout(&options.out_dir));
    let newest = match layout {
        DrizzleLayout::V045 => find_newest_v045_migration(&options.out_dir)?,
        DrizzleLayout::V1 => find_newest_folder_migration(&options.out_dir)?,
    };

    match plan_migration(options, newest) {
        PlannedMigration::UpToDate { existing } => Ok(PreviewOutcome::UpToDate { existing }),
        PlannedMigration::Changed {
            manifest,
            dropped_tables,
            statements,
        } => Ok(PreviewOutcome::Sql {
            sql: render_drizzle_sql(&manifest, &statements)?,
            dropped_tables,
        }),
    }
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

fn is_windows_reserved_device_name(stem: &str) -> bool {
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn validate_drizzle_tag(tag: &str, journal_path: &Path) -> Result<(), MigrationError> {
    let stem = tag.split('.').next().unwrap_or("").trim_end_matches(' ');
    if tag.is_empty()
        || tag == "."
        || tag.contains("..")
        || tag.contains(['/', '\\', '\0', ':'])
        || tag.chars().any(char::is_control)
        || is_windows_reserved_device_name(stem)
    {
        return Err(MigrationError::InvalidMetadata(format!(
            "invalid Drizzle journal tag {tag:?} in {}: must be a single relative filename without path separators, `..`, or reserved device names",
            journal_path.display()
        )));
    }
    Ok(())
}

struct ParsedJournalEntries {
    tags: Vec<String>,
    tag_set: HashSet<String>,
    max_idx: Option<u64>,
    max_when: Option<i64>,
}

fn parse_journal_entries(
    entries: &[Value],
    journal_path: &Path,
) -> Result<ParsedJournalEntries, MigrationError> {
    let mut tags = Vec::with_capacity(entries.len());
    let mut tag_set = HashSet::with_capacity(entries.len());
    let mut max_idx: Option<u64> = None;
    let mut max_when: Option<i64> = None;

    for entry in entries {
        let Some(obj) = entry.as_object() else {
            return Err(MigrationError::InvalidMetadata(format!(
                "Drizzle journal {} contains a non-object entry",
                journal_path.display()
            )));
        };
        let Some(tag) = obj.get("tag").and_then(Value::as_str) else {
            return Err(MigrationError::InvalidMetadata(format!(
                "Drizzle journal {} entry is missing a string `tag`",
                journal_path.display()
            )));
        };
        validate_drizzle_tag(tag, journal_path)?;
        tags.push(tag.to_owned());
        tag_set.insert(tag.to_owned());

        if let Some(idx_val) = obj.get("idx") {
            let Some(idx) = idx_val.as_u64() else {
                return Err(MigrationError::InvalidMetadata(format!(
                    "Drizzle journal {} contains invalid `idx` value",
                    journal_path.display()
                )));
            };
            max_idx = Some(max_idx.map_or(idx, |m| m.max(idx)));
        }
        if let Some(when_val) = obj.get("when") {
            let Some(when) = when_val.as_i64() else {
                return Err(MigrationError::InvalidMetadata(format!(
                    "Drizzle journal {} contains invalid `when` value",
                    journal_path.display()
                )));
            };
            max_when = Some(max_when.map_or(when, |m| m.max(when)));
        }
    }

    Ok(ParsedJournalEntries {
        tags,
        tag_set,
        max_idx,
        max_when,
    })
}

fn find_newest_v045_in_tags(
    out_dir: &Path,
    tags: &[String],
) -> Result<Option<(PathBuf, Manifest)>, MigrationError> {
    for tag in tags.iter().rev() {
        let sql_path = out_dir.join(format!("{tag}.sql"));
        if let Some(meta) = reject_symlink(&sql_path)?
            && meta.is_file()
            && let Some(manifest) = parse_manifest_from_file(&sql_path)?
        {
            return Ok(Some((sql_path, manifest)));
        }
    }
    Ok(None)
}

fn find_newest_v045_migration(
    out_dir: &Path,
) -> Result<Option<(PathBuf, Manifest)>, MigrationError> {
    let meta_dir = out_dir.join("meta");
    if reject_symlink(&meta_dir)?.is_none() {
        return Ok(None);
    }
    let journal_path = meta_dir.join("_journal.json");
    let Some(journal_meta) = reject_symlink(&journal_path)? else {
        return Ok(None);
    };
    if !journal_meta.is_file() {
        return Ok(None);
    }
    let journal = read_journal_json(&journal_path)?;
    let entries = journal_entries(&journal, &journal_path)?;
    let parsed = parse_journal_entries(entries, &journal_path)?;
    find_newest_v045_in_tags(out_dir, &parsed.tags)
}

/// Statement separator recognized by both Drizzle 0.45 and Drizzle 1.x migrators.
pub const STATEMENT_BREAKPOINT: &str = "--> statement-breakpoint";

/// Detects the Drizzle layout for `out_dir`: returns [`DrizzleLayout::V045`] when
/// `meta/_journal.json` (or a `meta` symlink) is present, otherwise [`DrizzleLayout::V1`].
#[must_use]
pub fn detect_layout(out_dir: &Path) -> DrizzleLayout {
    let meta_dir = out_dir.join("meta");
    if meta_dir
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
        || meta_dir.join("_journal.json").symlink_metadata().is_ok()
    {
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

fn next_free_v045_idx_and_tag(
    out_dir: &Path,
    journal_path: &Path,
    parsed: &ParsedJournalEntries,
) -> Result<(u64, String), MigrationError> {
    let mut next_idx = match parsed.max_idx {
        None => 0,
        Some(m) => m.checked_add(1).ok_or_else(|| {
            MigrationError::InvalidMetadata(format!(
                "Drizzle journal {} idx overflow",
                journal_path.display()
            ))
        })?,
    };
    loop {
        let tag = format!("{next_idx:04}_auditstream");
        let sql_path = out_dir.join(format!("{tag}.sql"));
        if !parsed.tag_set.contains(&tag) && sql_path.symlink_metadata().is_err() {
            return Ok((next_idx, tag));
        }
        next_idx = next_idx.checked_add(1).ok_or_else(|| {
            MigrationError::InvalidMetadata(format!(
                "Drizzle journal {} idx overflow",
                journal_path.display()
            ))
        })?;
    }
}

fn write_journal_atomic(
    meta_dir: &Path,
    journal_path: &Path,
    journal_json: &str,
) -> Result<(), MigrationError> {
    let _ = reject_symlink(meta_dir)?;
    let _ = reject_symlink(journal_path)?;
    let tmp_path = meta_dir.join(format!(".journal.{}.tmp", Uuid::new_v4().simple()));
    write_new_file(&tmp_path, journal_json)?;
    if let Err(err) = fs::rename(&tmp_path, journal_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(MigrationError::Io(err));
    }
    Ok(())
}

fn write_v045(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    let meta_dir = options.out_dir.join("meta");
    let journal_path = meta_dir.join("_journal.json");

    let mut journal: Value = if reject_symlink(&meta_dir)?.is_some()
        && let Some(meta) = reject_symlink(&journal_path)?
        && meta.is_file()
    {
        read_journal_json(&journal_path)?
    } else {
        json!({
            "version": "7",
            "dialect": "postgresql",
            "entries": []
        })
    };

    let entries = journal_entries(&journal, &journal_path)?;
    let parsed = parse_journal_entries(entries, &journal_path)?;

    let newest = find_newest_v045_in_tags(&options.out_dir, &parsed.tags)?;
    let (rendered, dropped_tables) = match plan_migration(options, newest) {
        PlannedMigration::UpToDate { existing } => return Ok(WriteOutcome::UpToDate { existing }),
        PlannedMigration::Changed {
            manifest,
            dropped_tables,
            statements,
        } => (render_drizzle_sql(&manifest, &statements)?, dropped_tables),
    };

    let (next_idx, tag) = next_free_v045_idx_and_tag(&options.out_dir, &journal_path, &parsed)?;
    let now_millis = i64::try_from(options.now.unix_timestamp_nanos() / 1_000_000).unwrap_or(0);
    let next_when = match parsed.max_when {
        None => now_millis,
        Some(m) => {
            let min_next = m.checked_add(1).ok_or_else(|| {
                MigrationError::InvalidMetadata(format!(
                    "Drizzle journal {} when overflow",
                    journal_path.display()
                ))
            })?;
            now_millis.max(min_next)
        }
    };

    fs::create_dir_all(&meta_dir)?;
    let sql_path = options.out_dir.join(format!("{tag}.sql"));
    write_new_file(&sql_path, &rendered)?;

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
    write_journal_atomic(&meta_dir, &journal_path, &journal_json)?;

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
            let rendered = render_drizzle_sql(&manifest, &statements)?;
            write_folder_migration(options, &rendered, dropped_tables)
        }
    }
}

pub(crate) fn render_drizzle_sql(
    manifest: &Manifest,
    statements: &[String],
) -> Result<String, MigrationError> {
    let header = header_comment(manifest);
    if header.contains(STATEMENT_BREAKPOINT)
        || statements.iter().any(|s| s.contains(STATEMENT_BREAKPOINT))
    {
        return Err(MigrationError::InvalidMetadata(format!(
            "Drizzle migration cannot contain reserved `{STATEMENT_BREAKPOINT}` separator inside identifiers or manifest"
        )));
    }
    let mut out = String::new();
    out.push_str(&header);
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
    Ok(out)
}
