//! Shared test helpers for `auditstream` CLI integration tests.

use std::env;
use std::process::Command;

/// Builds a fresh `auditstream` [`Command`] with all `AUDITSTREAM_*` variables and `DATABASE_URL` removed.
pub(crate) fn auditstream_std_cmd() -> Command {
    let bin = env::var_os("CARGO_BIN_EXE_auditstream")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_auditstream").into());
    let mut cmd = Command::new(bin);
    for (key, _) in env::vars() {
        if key.starts_with("AUDITSTREAM_") || key == "DATABASE_URL" {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// Builds an [`assert_cmd::Command`] wrapping [`auditstream_std_cmd`].
pub(crate) fn auditstream_cmd() -> assert_cmd::Command {
    assert_cmd::Command::from_std(auditstream_std_cmd())
}
