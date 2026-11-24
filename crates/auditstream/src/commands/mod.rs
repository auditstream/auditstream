//! CLI subcommand dispatch and stub modules.

pub(crate) mod common;
pub(crate) mod export;
pub(crate) mod history;
pub(crate) mod init;
pub(crate) mod status;
pub(crate) mod verify;

use crate::cli::Command;
use crate::config::Config;

/// Dispatches a parsed subcommand to its module implementation.
pub(crate) fn run(command: &Command, config: &Config) -> anyhow::Result<()> {
    match command {
        Command::Init(args) => init::run(args, config),
        Command::Status(args) => status::run(args, config),
        Command::Verify(args) => verify::run(args, config),
        Command::History(args) => history::run(args, config),
        Command::Export(args) => export::run(args, config),
    }
}
