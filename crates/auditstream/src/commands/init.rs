//! `auditstream init` command stub (implemented in F2).

use std::path::PathBuf;

use anyhow::anyhow;
use clap::Args as ClapArgs;

use crate::config::{Config, MigrationFormat};
use crate::exit::{Exit, ExitCodeExt};

/// Arguments for `auditstream init`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Table to audit (`schema.table`), repeatable.
    #[arg(long, value_name = "SCHEMA.TABLE")]
    pub(crate) table: Vec<String>,

    /// Column to exclude from row images (`schema.table.column`), repeatable.
    #[arg(long, value_name = "SCHEMA.TABLE.COLUMN")]
    pub(crate) exclude: Vec<String>,

    /// Migration format to generate.
    #[arg(long, value_enum)]
    pub(crate) migrations: Option<MigrationFormat>,

    /// Output directory for generated migration files.
    #[arg(long, value_name = "DIR")]
    pub(crate) out: Option<PathBuf>,
}

/// Runs the `init` command stub.
pub(crate) fn run(_args: &Args, _config: &Config) -> anyhow::Result<()> {
    Err(anyhow!("`init` is not implemented yet")).exit_code(Exit::Internal)
}
