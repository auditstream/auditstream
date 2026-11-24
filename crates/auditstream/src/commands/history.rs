//! `auditstream history` command stub (implemented in F4).

use anyhow::anyhow;
use clap::Args as ClapArgs;

use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt};

/// Arguments for `auditstream history`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {}

/// Runs the `history` command stub.
pub(crate) fn run(_args: &Args, _config: &Config) -> anyhow::Result<()> {
    Err(anyhow!("`history` is not implemented yet")).exit_code(Exit::Internal)
}
