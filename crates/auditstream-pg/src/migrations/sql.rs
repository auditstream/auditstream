//! Plain SQL migration writer (`<out_dir>/<YYYYMMDDHHMMSS>_auditstream.sql`).
//!
//! Wraps all generated statements in `BEGIN; ... COMMIT;` so the file runs atomically with
//! `psql -v ON_ERROR_STOP=1 -f <file>`.

use std::fs;
use std::path::{Path, PathBuf};

use super::{
    Manifest, MigrationError, MigrationOptions, PreviewOutcome, WriteOutcome, build_manifest,
    compose_statements, compute_dropped_tables, header_comment, next_unique_timestamp,
    parse_manifest_from_file,
};

pub(crate) fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
    let current = build_manifest(options);
    let newest = find_newest_sql_migration(&options.out_dir)?;

    if let Some((ref existing_path, ref prev_manifest)) = newest
        && prev_manifest.canonical_json == current.canonical_json
        && options.removed_tables.is_empty()
    {
        return Ok(PreviewOutcome::UpToDate {
            existing: existing_path.clone(),
        });
    }

    let dropped_tables = compute_dropped_tables(
        newest.as_ref().map(|(_, m)| m),
        &current,
        &options.removed_tables,
    );
    let statements = compose_statements(options, &dropped_tables);
    Ok(PreviewOutcome::Sql {
        sql: render_plain_sql(&current, &statements),
        dropped_tables,
    })
}

pub(crate) fn write(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    let current = build_manifest(options);
    let newest = find_newest_sql_migration(&options.out_dir)?;

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
    let rendered = render_plain_sql(&current, &statements);

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
