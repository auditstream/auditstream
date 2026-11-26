//! Command-line argument definitions (`clap` derive API).

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

use crate::commands;

/// Top-level `auditstream` CLI arguments (public contract: `docs/reference/cli/README.md`).
#[derive(Debug, Parser)]
#[command(
    name = "auditstream",
    version,
    about = "Open-source database audit trail engine and CLI"
)]
pub(crate) struct Cli {
    /// Path to the configuration file (defaults to ./auditstream.toml if present).
    #[arg(long, global = true, env = "AUDITSTREAM_CONFIG", value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    /// Postgres connection URL (also reads `DATABASE_URL` as fallback).
    #[arg(
        long,
        global = true,
        env = "AUDITSTREAM_DATABASE_URL",
        hide_env_values = true,
        value_name = "URL"
    )]
    pub(crate) database_url: Option<String>,

    /// Audit schema name (defaults to `auditstream`).
    #[arg(long, global = true, env = "AUDITSTREAM_SCHEMA", value_name = "SCHEMA")]
    pub(crate) schema: Option<String>,

    /// Increase log verbosity (`-v` info, `-vv` debug).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub(crate) verbose: u8,

    /// Silence warnings and info logs (errors only).
    #[arg(short, long, global = true)]
    pub(crate) quiet: bool,

    /// Subcommand to execute.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Top-level CLI subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Generate audit schema and trigger migrations.
    Init(commands::init::Args),
    /// Show audit schema, trigger and partition status.
    Status(commands::status::Args),
    /// Verify triggers, schema integrity and default partition health.
    Verify(commands::verify::Args),
    /// Inspect recorded audit history and diffs.
    History(commands::history::Args),
    /// Export recorded audit events to a sink.
    Export(commands::export::Args),
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn verify_clap_cli_invariants() {
        Cli::command().debug_assert();
    }
}
