//! Integration tests for `auditstream status` (F3).

use std::fs;

use auditstream_pg::ddl::{DEFAULT_SCHEMA, DdlOptions};
use auditstream_pg::migrations;
use auditstream_pg::triggers::TableTriggerSpec;
use auditstream_testkit::TestDb;
use insta_cmd::assert_cmd_snapshot;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::support::{auditstream_cmd, auditstream_std_cmd};

async fn install_capture(db: &TestDb, specs: &[TableTriggerSpec]) {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let all = migrations::install_statements(&DdlOptions::new(DEFAULT_SCHEMA, vec![]), specs)
        .expect("compose D2+D3 install statements");
    db.execute_statements(&all).await.expect("install D2+D3");
}

async fn seed_tables(db: &TestDb) {
    db.execute_batch(
        "CREATE TABLE public.invoice ( \
           id int PRIMARY KEY, \
           status text NOT NULL, \
           password_hash text \
         ); \
         CREATE TABLE public.notes ( \
           id int PRIMARY KEY, \
           body text NOT NULL \
         );",
    )
    .await
    .expect("create fixture tables");
}

#[test]
fn status_help_matches_snapshot() {
    let mut cmd = auditstream_std_cmd();
    cmd.args(["status", "--help"]);
    assert_cmd_snapshot!(cmd);
}

fn scrub_volatile(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Scrub `changes_YYYY_MM` partition names
        if raw[i..].starts_with("changes_")
            && i + 15 <= bytes.len()
            && bytes[i + 8..i + 12].iter().all(u8::is_ascii_digit)
            && bytes[i + 12] == b'_'
            && bytes[i + 13..i + 15].iter().all(u8::is_ascii_digit)
        {
            out.push_str("changes_<YYYY>_<MM>");
            i += 15;
            continue;
        }
        // Scrub `total_size_bytes: <digits>` and `"total_size_bytes": <digits>`
        if let Some(prefix) = ["total_size_bytes: ", "\"total_size_bytes\": "]
            .into_iter()
            .find(|p| raw[i..].starts_with(p))
        {
            out.push_str(prefix);
            i += prefix.len();
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            out.push_str(if prefix.starts_with('"') {
                "\"<BYTES>\""
            } else {
                "<BYTES>"
            });
            continue;
        }
        let ch = raw[i..].chars().next().expect("valid utf-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[tokio::test]
async fn clean_install_status_text_and_json_match_snapshots() {
    let db = TestDb::new().await.expect("create TestDb");
    seed_tables(&db).await;
    install_capture(
        &db,
        &[TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["password_hash".to_owned()],
        )],
    )
    .await;

    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\ntables = [\"public.invoice\"]\nexclude = [\"public.invoice.password_hash\"]\n",
    )
    .expect("write config");

    let text_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["status"])
        .output()
        .expect("run status text");
    assert!(
        text_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&text_out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&text_out.stderr), "");
    let text_stdout = String::from_utf8_lossy(&text_out.stdout).into_owned();
    insta::assert_snapshot!("status_clean_text", scrub_volatile(&text_stdout));

    let json_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["status", "--format", "json"])
        .output()
        .expect("run status json");
    assert!(
        json_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let json_stdout = String::from_utf8_lossy(&json_out.stdout).into_owned();
    let parsed: serde_json::Value =
        serde_json::from_str(&json_stdout).expect("status json parses cleanly");
    assert_eq!(parsed["problems_found"], 0);
    assert_eq!(parsed["history"]["exists"], true);
    assert_eq!(parsed["history"]["months_covered_ahead"], 3);
    assert_eq!(parsed["history"]["default_has_rows"], false);
    insta::assert_snapshot!("status_clean_json", scrub_volatile(&json_stdout));

    // When `capture.tables` is omitted, configured `capture.exclude` must still be checked by
    // `status` (matching `verify`), surfacing a PK exclusion as a problem and hinting `verify`.
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\nexclude = [\"public.invoice.id\"]\n",
    )
    .expect("write PK exclude config without capture.tables");
    let bad_status = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["status", "--format", "json"])
        .output()
        .expect("run status json with PK exclude");
    assert!(bad_status.status.success());
    let bad_parsed: serde_json::Value =
        serde_json::from_slice(&bad_status.stdout).expect("parse bad status json");
    assert!(
        bad_parsed["problems_found"].as_u64().unwrap_or(0) >= 1,
        "status must report problems_found >= 1 when capture.exclude excludes a PK without capture.tables, got: {bad_parsed}"
    );
}

#[tokio::test]
async fn status_exits_0_and_hints_verify_when_problems_exist() {
    let db = TestDb::new().await.expect("create TestDb");
    seed_tables(&db).await;
    install_capture(
        &db,
        &[TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["password_hash".to_owned()],
        )],
    )
    .await;

    // Disable a trigger AND insert a row into changes_default.
    db.execute("ALTER TABLE public.invoice DISABLE TRIGGER auditstream_update")
        .await
        .expect("disable trigger");
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
         VALUES ('2020-01-15T10:00:00Z'::timestamptz, 'app', 'legacy.event', 'postgres', 1)",
    )
    .await
    .expect("seed default row");

    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\ntables = [\"public.invoice\"]\nexclude = [\"public.invoice.password_hash\"]\n",
    )
    .expect("write config");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("triggers=disabled"))
        .stdout(predicate::str::contains("default_has_rows: yes"))
        .stdout(predicate::str::contains("auditstream verify"));
}

#[test]
fn unreachable_database_exits_3() {
    let dir = tempdir().expect("tempdir");
    auditstream_cmd()
        .current_dir(dir.path())
        .env(
            "AUDITSTREAM_DATABASE_URL",
            "postgresql://postgres:secret@127.0.0.1:1/nope",
        )
        .args(["status"])
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("secret").not());
}
