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

    // 5. Empty AUDITSTREAM_SCHEMA env var falls through to auditstream.toml, while explicit
    //    `--schema ""` fails validation with exit code 2.
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_SCHEMA", "")
        .args(SHOW_CONFIG)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("from_file"));

    auditstream_cmd()
        .current_dir(dir.path())
        .args(["--schema", "", "status"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "audit schema name must not be empty",
        ));
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

    // Unrecognized secret query params (`sslpassword`, `ssl-password`, `oauth_client_secret`,
    // uppercase `PASSWORD`, percent-encoded keys, leading spaces) and libpq KV strings must never
    // leak to stderr at default verbosity, `-vv`, or with `AUDITSTREAM_LOG=debug`.
    // Use a local stub listener returning an immediate PostgreSQL auth ErrorResponse so `sqlx`
    // parses the URL and fails in milliseconds instead of waiting for `acquire_timeout`.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local tcp stub");
    let port = listener.local_addr().expect("stub addr").port();
    let stub = std::thread::spawn(move || {
        use std::io::{Read, Write};
        // Minimal PostgreSQL ErrorResponse (`E`) with SQLSTATE 28P01 (invalid_password).
        let payload = b"SERROR\0C28P01\0Mdenied\0\0";
        let len = u32::try_from(payload.len() + 4)
            .expect("packet len")
            .to_be_bytes();
        let mut packet = Vec::with_capacity(1 + 4 + payload.len());
        packet.push(b'E');
        packet.extend_from_slice(&len);
        packet.extend_from_slice(payload);
        for stream in listener.incoming().flatten() {
            let mut s = stream;
            let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(200)));
            let mut buf = [0_u8; 256];
            let _ = s.read(&mut buf);
            let _ = s.write_all(&packet);
            let _ = s.flush();
        }
    });
    let _keep_stub_running = stub;

    for variant in [
        format!(
            "postgresql://app_user@127.0.0.1:{port}/auditdb?sslmode=disable&sslpassword={secret}"
        ),
        format!(
            "postgresql://app_user@127.0.0.1:{port}/auditdb?sslmode=disable&ssl-password={secret}"
        ),
        format!(
            "postgresql://app_user@127.0.0.1:{port}/auditdb?sslmode=disable&oauth_client_secret={secret}"
        ),
        format!("postgresql://app_user@127.0.0.1:{port}/auditdb?sslmode=disable&PASSWORD={secret}"),
        format!(
            "postgresql://app_user@127.0.0.1:{port}/auditdb?sslmode=disable&%70assword={secret}"
        ),
        format!(
            " postgresql://app_user@127.0.0.1:{port}/auditdb?sslmode=disable&ssl\tpassword={secret}"
        ),
        format!("host=127.0.0.1 port={port} password={secret} dbname=auditdb"),
    ] {
        for (args, env_log) in [
            (&["status"][..], None),
            (&["-vv", "status"][..], None),
            (&["status"][..], Some("debug")),
        ] {
            let mut cmd = auditstream_cmd();
            cmd.current_dir(dir.path())
                .env("AUDITSTREAM_DATABASE_URL", &variant)
                .args(args);
            if let Some(level) = env_log {
                cmd.env("AUDITSTREAM_LOG", level);
            }
            cmd.assert()
                .code(3)
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains(secret).not());
        }
    }
}

#[test]
fn log_format_json_emits_structured_stderr_records_and_redacts_secrets() {
    let dir = tempdir().unwrap();
    let secret = "json_log_secret_pw_42";
    let url = format!("postgresql://app_user:{secret}@db.internal:5432/auditdb");

    for (extra_args, env_format) in [
        (&["--log-format", "json"][..], None),
        (&[][..], Some("json")),
    ] {
        let mut cmd = auditstream_cmd();
        cmd.current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", &url)
            .args(extra_args)
            .args(SHOW_CONFIG);
        if let Some(fmt) = env_format {
            cmd.env("AUDITSTREAM_LOG_FORMAT", fmt);
        }
        let output = cmd.output().expect("run json log cli");
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout.as_slice(), b"");

        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
        let lines: Vec<&str> = stderr.lines().collect();
        assert!(
            lines.len() >= 2,
            "expected debug log + error record as JSONL, got: {stderr}"
        );
        for line in &lines {
            assert!(!line.contains(secret), "secret leaked in JSON log: {line}");
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("every stderr line must be valid JSON");
            assert!(parsed["timestamp"].as_str().is_some());
            assert!(parsed["level"].as_str().is_some());
            assert!(parsed["message"].as_str().is_some());
        }
        let last: serde_json::Value =
            serde_json::from_str(lines.last().expect("last line")).expect("parse error record");
        assert_eq!(last["level"], "ERROR");
        assert_eq!(last["exit_code"], 2);
    }

    // Clap parse errors also emit single-line JSON when `--log-format json` or
    // `AUDITSTREAM_LOG_FORMAT=json` is active.
    for (extra_args, env_format) in [
        (
            &["--log-format", "json", "export", "--bogus-flag"][..],
            None,
        ),
        (&["export", "--bogus-flag"][..], Some("json")),
    ] {
        let mut cmd = auditstream_cmd();
        cmd.current_dir(dir.path()).args(extra_args);
        if let Some(fmt) = env_format {
            cmd.env("AUDITSTREAM_LOG_FORMAT", fmt);
        }
        let output = cmd.output().expect("run json clap error");
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout.as_slice(), b"");
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
        let lines: Vec<&str> = stderr.lines().collect();
        assert_eq!(lines.len(), 1, "expected 1 JSON clap error line: {stderr}");
        let parsed: serde_json::Value =
            serde_json::from_str(lines[0]).expect("clap error must be valid JSON");
        assert_eq!(parsed["level"], "ERROR");
        assert_eq!(parsed["exit_code"], 2);
        let msg = parsed["message"].as_str().expect("message string");
        assert!(msg.contains("--bogus-flag"), "{msg}");
        assert!(!msg.starts_with("error: "), "no double error prefix: {msg}");
    }
}
