//! Prisma 7 migration writer (`<out_dir>/<YYYYMMDDHHMMSS>_auditstream/migration.sql`).
//!
//! Never modifies `migration_lock.toml` or any existing migration folder.

use super::{
    Manifest, MigrationError, MigrationOptions, PlannedMigration, PreviewOutcome, WriteOutcome,
    find_newest_folder_migration, header_comment, plan_migration, preview_with,
    write_folder_migration,
};

pub(crate) fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
    let newest = find_newest_folder_migration(&options.out_dir)?;
    Ok(preview_with(options, newest, render_sql))
}

pub(crate) fn write(options: &MigrationOptions) -> Result<WriteOutcome, MigrationError> {
    let newest = find_newest_folder_migration(&options.out_dir)?;
    match plan_migration(options, newest) {
        PlannedMigration::UpToDate { existing } => Ok(WriteOutcome::UpToDate { existing }),
        PlannedMigration::Changed {
            manifest,
            dropped_tables,
            statements,
        } => {
            let rendered = render_sql(&manifest, &statements);
            write_folder_migration(options, &rendered, dropped_tables)
        }
    }
}

/// Prisma runs each file as one implicit transaction and then records the outcome in
/// `_prisma_migrations` on the same connection. An explicit `BEGIN` would leave that connection
/// in an aborted transaction after a failure, hiding the error and its log from Prisma.
pub(crate) fn render_sql(manifest: &Manifest, statements: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&header_comment(manifest));
    out.push_str("\n\n");
    for (idx, stmt) in statements.iter().enumerate() {
        if idx > 0 {
            out.push_str("\n\n");
        }
        out.push_str(stmt);
        out.push(';');
    }
    out.push('\n');
    out
}
