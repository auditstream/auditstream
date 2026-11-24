//! Process exit codes, error tagging and stdout pipe handling.

use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;

/// Documented CLI exit codes (public contract: `docs/reference/exit-codes.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Exit {
    /// Command succeeded.
    Success = 0,
    /// Verification or status check failed (`verify` found problems).
    #[allow(dead_code)]
    CheckFailed = 1,
    /// CLI usage or configuration error (matches clap's exit code).
    Usage = 2,
    /// Database connection, query or migration error.
    #[allow(dead_code)]
    Database = 3,
    /// Output or cursor file I/O error.
    Io = 4,
    /// Internal or unimplemented error.
    Internal = 5,
}

impl Exit {
    /// Numeric POSIX exit status for this exit code.
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    /// Converts this code into `std::process::ExitCode`.
    pub(crate) fn to_exit_code(self) -> ExitCode {
        ExitCode::from(self.code())
    }
}

/// Wrapper carrying an explicit [`Exit`] code on an `anyhow::Error`.
#[derive(Debug)]
struct CodedError {
    code: Exit,
    source: anyhow::Error,
}

impl fmt::Display for CodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for CodedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

/// Extension trait for attaching an [`Exit`] code to results and errors.
pub(crate) trait ExitCodeExt<T> {
    /// Tags an error with a documented CLI [`Exit`] code.
    fn exit_code(self, code: Exit) -> anyhow::Result<T>;
}

impl<T, E> ExitCodeExt<T> for Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn exit_code(self, code: Exit) -> anyhow::Result<T> {
        self.map_err(|err| tag_error(err.into(), code))
    }
}

/// Tags an [`anyhow::Error`] with an explicit [`Exit`] code.
pub(crate) fn tag_error(source: anyhow::Error, code: Exit) -> anyhow::Error {
    anyhow::Error::new(CodedError { code, source })
}

/// Maps an [`anyhow::Error`] to its documented [`Exit`] code.
pub(crate) fn exit_code_for(err: &anyhow::Error) -> Exit {
    if let Some(clap_err) = err.downcast_ref::<clap::Error>() {
        return if clap_err.use_stderr() {
            Exit::Usage
        } else {
            Exit::Success
        };
    }

    for cause in err.chain() {
        if let Some(coded) = cause.downcast_ref::<CodedError>() {
            return coded.code;
        }
    }

    Exit::Internal
}

/// Formats an error and its cause chain into a single `error:` line.
pub(crate) fn format_error_line(err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in err.chain() {
        let cleaned = cause
            .to_string()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !cleaned.is_empty() && parts.last().is_none_or(|prev| prev != &cleaned) {
            parts.push(cleaned);
        }
    }
    format!("error: {}", parts.join(": "))
}

/// Writes bytes to stdout, turning `BrokenPipe` into a quiet success (`Ok(false)`).
pub(crate) fn write_stdout<W: Write>(writer: &mut W, bytes: &[u8]) -> anyhow::Result<bool> {
    match writer.write_all(bytes).and_then(|()| writer.flush()) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err).exit_code(Exit::Io),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use anyhow::anyhow;
    use clap::Parser;

    use super::{Exit, ExitCodeExt, exit_code_for, format_error_line, tag_error, write_stdout};
    use crate::cli::Cli;

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn exit_codes_match_public_contract() {
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::CheckFailed.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::Database.code(), 3);
        assert_eq!(Exit::Io.code(), 4);
        assert_eq!(Exit::Internal.code(), 5);
    }

    #[test]
    fn exit_code_for_maps_clap_tagged_and_untagged_errors() {
        let clap_err = Cli::try_parse_from(["auditstream", "--no-such-flag"]).unwrap_err();
        assert_eq!(exit_code_for(&anyhow::Error::new(clap_err)), Exit::Usage);

        let help_err = Cli::try_parse_from(["auditstream", "--help"]).unwrap_err();
        assert_eq!(exit_code_for(&anyhow::Error::new(help_err)), Exit::Success);

        let db_err = tag_error(anyhow!("connection refused"), Exit::Database);
        assert_eq!(exit_code_for(&db_err), Exit::Database);

        let io_err: anyhow::Result<()> = Err(anyhow!("disk full")).exit_code(Exit::Io);
        assert_eq!(exit_code_for(&io_err.unwrap_err()), Exit::Io);

        let check_err = tag_error(anyhow!("drift detected"), Exit::CheckFailed);
        assert_eq!(exit_code_for(&check_err), Exit::CheckFailed);

        let plain_err = anyhow!("unexpected bug");
        assert_eq!(exit_code_for(&plain_err), Exit::Internal);
    }

    #[test]
    fn format_error_line_joins_causes_into_one_line() {
        let root = io::Error::new(io::ErrorKind::NotFound, "no such file");
        let err = anyhow::Error::new(root).context("failed to read config");
        let line = format_error_line(&err);
        assert_eq!(line, "error: failed to read config: no such file");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn write_stdout_turns_broken_pipe_into_quiet_ok() {
        let mut writer = BrokenPipeWriter;
        let wrote = write_stdout(&mut writer, b"hello\n").unwrap();
        assert!(!wrote);
    }
}
