//! Plain SQL migration writer (`<out_dir>/<YYYYMMDDHHMMSS>_auditstream.sql`).
//!
//! Wraps all generated statements in `BEGIN; ... COMMIT;` so the file runs atomically with
//! `psql -v ON_ERROR_STOP=1 -f <file>`.

use std::fs;
use std::path::{Path, PathBuf};

use super::{
    Manifest, MigrationError, MigrationOptions, PlannedMigration, PreviewOutcome, WriteOutcome,
    header_comment, next_unique_timestamp, parse_manifest_from_file, plan_migration, preview_with,
};

pub(crate) fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
    let newest = find_newest_sql_migration(&options.out_dir)?;
    Ok(preview_with(options, newest, render_plain_sql))
}

pub(crate) fn write(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    let newest = find_newest_sql_migration(&options.out_dir)?;
    let (rendered, dropped_tables) = match plan_migration(options, newest) {
        PlannedMigration::UpToDate { existing } => return Ok(WriteOutcome::UpToDate { existing }),
        PlannedMigration::Changed {
            manifest,
            dropped_tables,
            statements,
        } => (render_plain_sql(&manifest, &statements), dropped_tables),
    };

    fs::create_dir_all(&options.out_dir)?;
    let ts = next_unique_timestamp(options.now, |ts| {
        options
            .out_dir
            .join(format!("{ts}_auditstream.sql"))
            .exists()
    });
    let file_path = options.out_dir.join(format!("{ts}_auditstream.sql"));
    fs::write(&file_path, rendered)?;

    Ok(WriteOutcome::Written {
        files: vec![file_path],
        dropped_tables,
    })
}

pub(crate) fn render_plain_sql(manifest: &Manifest, statements: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&header_comment(manifest));
    out.push_str("\n\nBEGIN;\n\n");
    for (idx, stmt) in statements.iter().enumerate() {
        if idx > 0 {
            out.push_str("\n\n");
        }
        out.push_str(stmt);
        out.push(';');
    }
    out.push_str("\n\nCOMMIT;\n");
    out
}

fn find_newest_sql_migration(
    out_dir: &Path,
) -> Result<Option<(PathBuf, Manifest)>, MigrationError> {
    if !out_dir.exists() {
        return Ok(None);
    }

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(out_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
        {
            files.push((name, path));
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (_, sql_path) in files.into_iter().rev() {
        if let Some(manifest) = parse_manifest_from_file(&sql_path)? {
            return Ok(Some((sql_path, manifest)));
        }
    }

    Ok(None)
}
