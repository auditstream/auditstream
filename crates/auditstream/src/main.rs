//! CLI entry point for Auditstream.

mod cli;
mod commands;
mod config;
mod exit;
mod logging;

use std::env;
use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use auditstream_core as _;
use auditstream_export as _;
use auditstream_pg as _;
use clap::Parser;
use tokio as _;

use crate::cli::{Cli, LogFormat};
use crate::config::Config;
use crate::exit::{Exit, exit_code_for, format_error_line, write_stdout};

fn parse_log_format_value(raw: &str) -> Option<LogFormat> {
    match raw {
        "text" => Some(LogFormat::Text),
        "json" => Some(LogFormat::Json),
        _ => None,
    }
}

/// Best-effort detection of `--log-format` / `AUDITSTREAM_LOG_FORMAT` when `clap` fails before
/// returning a parsed [`Cli`], so stderr parse errors also respect JSON log formatting.
fn detect_requested_log_format(args: &[OsString], env_val: Option<&str>) -> LogFormat {
    let mut format = env_val
        .and_then(parse_log_format_value)
        .unwrap_or(LogFormat::Text);

    let mut iter = args.iter().skip(1).peekable();
    while let Some(arg) = iter.next() {
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }
        if text == "--log-format" {
            if let Some(parsed) = iter
                .peek()
                .and_then(|v| v.to_str())
                .and_then(parse_log_format_value)
            {
                let _ = iter.next();
                format = parsed;
            }
        } else if let Some(val) = text.strip_prefix("--log-format=")
            && let Some(parsed) = parse_log_format_value(val)
        {
            format = parsed;
        }
    }
    format
}

fn emit_fatal_error(log_format: LogFormat, err: &anyhow::Error, code: Exit) {
    match log_format {
        LogFormat::Text => eprintln!("{}", format_error_line(err)),
        LogFormat::Json => eprintln!("{}", logging::format_json_error(err, code.code())),
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let raw_args: Vec<OsString> = env::args_os().collect();
            let env_format = env::var("AUDITSTREAM_LOG_FORMAT").ok();
            let log_format = detect_requested_log_format(&raw_args, env_format.as_deref());
            if err.use_stderr() {
                match log_format {
                    LogFormat::Text => eprint!("{err}"),
                    LogFormat::Json => {
                        let wrapped = anyhow::Error::new(err);
                        emit_fatal_error(LogFormat::Json, &wrapped, Exit::Usage);
                    }
                }
                return Exit::Usage.to_exit_code();
            }
            let mut stdout = io::stdout().lock();
            return match write_stdout(&mut stdout, err.to_string().as_bytes()) {
                Ok(_) => Exit::Success.to_exit_code(),
                Err(io_err) => {
                    let code = exit_code_for(&io_err);
                    emit_fatal_error(log_format, &io_err, code);
                    code.to_exit_code()
                }
            };
        }
    };

    logging::init(cli.verbose, cli.quiet, cli.log_format);

    let result = run_cli(&cli);
    match result {
        Ok(()) => Exit::Success.to_exit_code(),
        Err(err) => {
            let code = exit_code_for(&err);
            emit_fatal_error(cli.log_format, &err, code);
            code.to_exit_code()
        }
    }
}

fn run_cli(cli: &Cli) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let config = Config::load(cli, &cwd)?;
    tracing::debug!(?config, "loaded configuration");
    commands::run(&cli.command, &config)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{LogFormat, detect_requested_log_format};

    fn os_args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn detect_requested_log_format_respects_precedence_and_double_dash() {
        assert_eq!(
            detect_requested_log_format(&os_args(&["auditstream", "export"]), None),
            LogFormat::Text
        );
        assert_eq!(
            detect_requested_log_format(&os_args(&["auditstream", "export"]), Some("json")),
            LogFormat::Json
        );
        assert_eq!(
            detect_requested_log_format(
                &os_args(&["auditstream", "--log-format", "json", "--bogus"]),
                None,
            ),
            LogFormat::Json
        );
        assert_eq!(
            detect_requested_log_format(
                &os_args(&["auditstream", "--log-format=text", "--bogus"]),
                Some("json"),
            ),
            LogFormat::Text
        );
        assert_eq!(
            detect_requested_log_format(
                &os_args(&["auditstream", "export", "--", "--log-format", "json"]),
                None,
            ),
            LogFormat::Text
        );
        assert_eq!(
            detect_requested_log_format(
                &os_args(&["auditstream", "--log-format", "--", "--log-format", "json"]),
                None,
            ),
            LogFormat::Text
        );
        assert_eq!(
            detect_requested_log_format(
                &os_args(&["auditstream", "--log-format", "--log-format=json", "export"]),
                None,
            ),
            LogFormat::Json
        );
        assert_eq!(
            detect_requested_log_format(
                &os_args(&["auditstream", "--log-format", "bogus", "export"]),
                Some("json"),
            ),
            LogFormat::Json
        );
    }
}
