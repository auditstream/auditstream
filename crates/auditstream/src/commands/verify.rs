//! `auditstream verify` command stub (implemented in F3).

use anyhow::anyhow;
use clap::Args as ClapArgs;

use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt};

/// Arguments for `auditstream verify`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {}

/// Runs the `verify` command stub.
pub(crate) fn run(_args: &Args, _config: &Config) -> anyhow::Result<()> {
    Err(anyhow!("`verify` is not implemented yet")).exit_code(Exit::Internal)
}
