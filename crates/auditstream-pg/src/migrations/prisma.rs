//! Prisma 7 migration writer (`<out_dir>/<YYYYMMDDHHMMSS>_auditstream/migration.sql`).
//!
//! Never modifies `migration_lock.toml` or any existing migration folder.

use super::sql::render_transaction;
use super::{
    MigrationError, MigrationOptions, PlannedMigration, PreviewOutcome, WriteOutcome,
    find_newest_folder_migration, plan_migration, preview_with, write_folder_migration,
};

pub(crate) fn preview(options: &MigrationOptions) -> Result<PreviewOutcome, MigrationError> {
    let newest = find_newest_folder_migration(&options.out_dir)?;
    Ok(preview_with(options, newest, render_transaction))
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
            let rendered = render_transaction(&manifest, &statements);
            write_folder_migration(options, &rendered, dropped_tables)
        }
    }
}
