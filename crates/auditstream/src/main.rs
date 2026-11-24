//! CLI entry point for Auditstream.

mod cli;
mod commands;
mod config;
mod exit;
mod logging;

use std::env;
use std::io;
use std::process::ExitCode;

use auditstream_core as _;
use auditstream_export as _;
use auditstream_pg as _;
use clap::Parser;
use tokio as _;

use crate::cli::Cli;
use crate::config::Config;
use crate::exit::{Exit, exit_code_for, format_error_line, write_stdout};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.use_stderr() {
                eprint!("{err}");
                return Exit::Usage.to_exit_code();
            }
            let mut stdout = io::stdout().lock();
            return match write_stdout(&mut stdout, err.to_string().as_bytes()) {
                Ok(_) => Exit::Success.to_exit_code(),
                Err(io_err) => {
                    eprintln!("{}", format_error_line(&io_err));
                    exit_code_for(&io_err).to_exit_code()
                }
            };
        }
    };

    logging::init(cli.verbose, cli.quiet);

    let result = run_cli(&cli);
    match result {
        Ok(()) => Exit::Success.to_exit_code(),
        Err(err) => {
            eprintln!("{}", format_error_line(&err));
            exit_code_for(&err).to_exit_code()
        }
    }
}

fn run_cli(cli: &Cli) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let config = Config::load(cli, &cwd)?;
    tracing::debug!(?config, "loaded configuration");
    commands::run(&cli.command, &config)
}
