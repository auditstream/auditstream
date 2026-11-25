//! Shared test helpers for `auditstream` CLI integration tests.

use std::path::Path;
use std::process::Command;
use std::{env, fs};

use auditstream_pg::migrations::drizzle::STATEMENT_BREAKPOINT;
use auditstream_testkit::TestDb;

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

/// Applies the file as a transactional migration runner would.
pub(crate) async fn apply_generated_migration(db: &TestDb, format: &str, path: &Path) {
    let sql = fs::read_to_string(path).expect("read generated migration");
    if format == "drizzle" {
        let chunks = std::iter::once("BEGIN")
            .chain(sql.split(STATEMENT_BREAKPOINT))
            .chain(std::iter::once("COMMIT"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        db.execute_statements(&chunks)
            .await
            .expect("apply Drizzle migration");
    } else {
        db.execute_batch(&sql)
            .await
            .expect("apply Prisma/SQL migration");
    }
}
