//! Integration tests for CLI flags, config precedence, logging, redaction and exit codes.

use std::fs;

use insta_cmd::assert_cmd_snapshot;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::support::{auditstream_cmd, auditstream_std_cmd};

// A zero batch size fails after the resolved config is logged and before connecting.
const SHOW_CONFIG: [&str; 4] = ["-vv", "export", "--batch-size", "0"];

#[test]
fn help_output_matches_snapshot() {
    let mut cmd = auditstream_std_cmd();
    cmd.arg("--help");
    assert_cmd_snapshot!(cmd);
}

#[test]
fn missing_explicit_config_exits_2_and_names_path() {
    let dir = tempdir().unwrap();
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["--config", "missing.toml", "status"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("missing.toml"));
}

#[test]
fn unknown_config_key_exits_2_and_names_key() {
    let dir = tempdir().unwrap();
    let cfg = dir.path().join("bad.toml");
    fs::write(&cfg, "unknown_option = true\n").unwrap();

    auditstream_cmd()
        .current_dir(dir.path())
        .args(["--config", cfg.to_str().unwrap(), "status"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("unknown_option"));
}

#[test]
fn wrong_config_type_exits_2_and_names_field() {
    let dir = tempdir().unwrap();
    let cfg = dir.path().join("bad-type.toml");
    fs::write(&cfg, "[export]\nbatch_size = \"not-a-number\"\n").unwrap();

    auditstream_cmd()
        .current_dir(dir.path())
        .args(["--config", cfg.to_str().unwrap(), "status"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("batch_size"));
}

#[test]
fn config_precedence_layers_end_to_end() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("auditstream.toml"),
        "database_url = \"postgres://file_user:pw@file-host/db\"\nschema = \"from_file\"\n",
    )
    .unwrap();

    // 1. Default file layer
    auditstream_cmd()
        .current_dir(dir.path())
        .args(SHOW_CONFIG)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("file-host"))
        .stderr(predicate::str::contains("from_file"));

    // 2. DATABASE_URL overrides file
    auditstream_cmd()
        .current_dir(dir.path())
        .env("DATABASE_URL", "postgres://dbenv:pw@dbenv-host/db")
        .args(SHOW_CONFIG)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("dbenv-host"));

    // 3. AUDITSTREAM_DATABASE_URL and AUDITSTREAM_SCHEMA override DATABASE_URL and file
    auditstream_cmd()
        .current_dir(dir.path())
        .env("DATABASE_URL", "postgres://dbenv:pw@dbenv-host/db")
        .env(
            "AUDITSTREAM_DATABASE_URL",
            "postgres://asenv:pw@asenv-host/db",
        )
        .env("AUDITSTREAM_SCHEMA", "from_env")
        .args(SHOW_CONFIG)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("asenv-host"))
        .stderr(predicate::str::contains("from_env"));

    // 4. CLI flags override environment variables
    auditstream_cmd()
        .current_dir(dir.path())
        .env(
            "AUDITSTREAM_DATABASE_URL",
            "postgres://asenv:pw@asenv-host/db",
        )
        .env("AUDITSTREAM_SCHEMA", "from_env")
        .args([
            "--database-url",
            "postgres://flag:pw@flag-host/db",
            "--schema",
            "from_flag",
        ])
        .args(SHOW_CONFIG)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("flag-host"))
        .stderr(predicate::str::contains("from_flag"));
}

#[test]
fn auditstream_config_env_selects_config_file() {
    let dir = tempdir().unwrap();
    let custom = dir.path().join("custom.toml");
    fs::write(&custom, "schema = \"via_env_config\"\n").unwrap();

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_CONFIG", custom.to_str().unwrap())
        .args(SHOW_CONFIG)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("via_env_config"));
}

#[test]
fn database_password_never_appears_in_help_logs_or_errors() {
    let dir = tempdir().unwrap();
    let secret = "super_secret_pg_password_987654";
    let url = format!("postgresql://app_user:{secret}@db.internal:5432/auditdb");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &url)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains(secret).not());

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &url)
        .args(SHOW_CONFIG)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains(
            "postgresql://app_user:***@db.internal:5432/auditdb",
        ));

    // Unrecognized secret query params (`sslpassword`, uppercase `PASSWORD`) and libpq KV
    // strings must never leak to stderr at default verbosity or `-vv` when connecting fails.
    for variant in [
        format!("postgresql://app_user@127.0.0.1:1/auditdb?sslpassword={secret}"),
        format!("postgresql://app_user@127.0.0.1:1/auditdb?PASSWORD={secret}"),
        format!("postgresql://app_user@127.0.0.1:1/auditdb?%70assword={secret}"),
        format!("host=127.0.0.1 port=1 password={secret} dbname=auditdb"),
    ] {
        for args in [&["status"][..], &["-vv", "status"][..]] {
            auditstream_cmd()
                .current_dir(dir.path())
                .env("AUDITSTREAM_DATABASE_URL", &variant)
                .args(args)
                .assert()
                .code(3)
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains(secret).not());
        }
    }
}
