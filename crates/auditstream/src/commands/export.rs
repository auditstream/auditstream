//! `auditstream export` command stub (implemented in F5).

use std::path::PathBuf;

use anyhow::anyhow;
use clap::Args as ClapArgs;

use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt};

/// Arguments for `auditstream export`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Path to persist the export cursor.
    #[arg(long, value_name = "PATH")]
    pub(crate) cursor_file: Option<PathBuf>,

    /// Maximum events per export batch.
    #[arg(long, value_name = "N")]
    pub(crate) batch_size: Option<usize>,

    /// Output format (for example `jsonl` or `csv`).
    #[arg(long, value_name = "FORMAT")]
    pub(crate) format: Option<String>,

    /// Output file path (defaults to stdout).
    #[arg(long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,
}

/// Runs the `export` command stub.
pub(crate) fn run(_args: &Args, _config: &Config) -> anyhow::Result<()> {
    Err(anyhow!("`export` is not implemented yet")).exit_code(Exit::Internal)
}
