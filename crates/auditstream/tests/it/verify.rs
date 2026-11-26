//! Integration tests for `auditstream verify` (F3).

use std::fs;

use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_testkit::TestDb;
use insta_cmd::assert_cmd_snapshot;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::support::{auditstream_cmd, auditstream_std_cmd};

async fn install_capture(db: &TestDb, specs: &[TableTriggerSpec]) {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let mut ddl_stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    let cleanup = ddl_stmts.pop();
    let trigger_stmts = triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, specs.to_vec()));
    let all: Vec<String> = ddl_stmts
        .into_iter()
        .chain(trigger_stmts)
        .chain(cleanup)
        .collect();
    db.execute_statements(&all).await.expect("install D2+D3");
}

async fn create_invoice_table(db: &TestDb) {
    db.execute(
        "CREATE TABLE public.invoice ( \
           id int PRIMARY KEY, \
           status text NOT NULL, \
           password_hash text \
         )",
    )
    .await
    .expect("create public.invoice");
}

fn write_invoice_config(dir: &std::path::Path) {
    fs::write(
        dir.join("auditstream.toml"),
        "[capture]\ntables = [\"public.invoice\"]\nexclude = [\"public.invoice.password_hash\"]\n",
    )
    .expect("write auditstream.toml");
}

#[test]
fn verify_help_matches_snapshot() {
    let mut cmd = auditstream_std_cmd();
    cmd.args(["verify", "--help"]);
    assert_cmd_snapshot!(cmd);
}

#[tokio::test]
async fn clean_install_exits_0_and_matches_text_and_json_snapshots() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
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
    write_invoice_config(dir.path());

    let text_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify"])
        .output()
        .expect("run verify text");
    assert!(
        text_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&text_out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&text_out.stderr), "");
    insta::assert_snapshot!(
        "verify_clean_text",
        String::from_utf8_lossy(&text_out.stdout).into_owned()
    );

    let json_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json");
    assert!(
        json_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    insta::assert_snapshot!(
        "verify_clean_json",
        String::from_utf8_lossy(&json_out.stdout).into_owned()
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn disabled_dropped_outdated_and_default_row_each_exit_1_with_finding() {
    // 1. Disabled trigger
    {
        let db = TestDb::new().await.expect("create TestDb");
        create_invoice_table(&db).await;
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
        db.execute("ALTER TABLE public.invoice DISABLE TRIGGER auditstream_update")
            .await
            .expect("disable trigger");

        let dir = tempdir().expect("tempdir");
        write_invoice_config(dir.path());

        let text_out = auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["verify"])
            .output()
            .expect("verify disabled text");
        assert_eq!(text_out.status.code(), Some(1));
        insta::assert_snapshot!(
            "verify_disabled_text",
            String::from_utf8_lossy(&text_out.stdout).into_owned()
        );

        let json_out = auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["verify", "--format", "json"])
            .output()
            .expect("verify disabled json");
        assert_eq!(json_out.status.code(), Some(1));
        insta::assert_snapshot!(
            "verify_disabled_json",
            String::from_utf8_lossy(&json_out.stdout).into_owned()
        );
    }

    // 2. Dropped trigger
    {
        let db = TestDb::new().await.expect("create TestDb");
        create_invoice_table(&db).await;
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
        db.execute("DROP TRIGGER auditstream_delete ON public.invoice")
            .await
            .expect("drop trigger");

        let dir = tempdir().expect("tempdir");
        write_invoice_config(dir.path());

        auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["verify"])
            .assert()
            .code(1)
            .stdout(predicate::str::contains("public.invoice"))
            .stdout(predicate::str::contains("missing"))
            .stdout(predicate::str::contains("auditstream_delete"));
    }

    // 3. Outdated function body
    {
        let db = TestDb::new().await.expect("create TestDb");
        create_invoice_table(&db).await;
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
        db.execute(
            "CREATE OR REPLACE FUNCTION auditstream.capture() \
             RETURNS trigger \
             LANGUAGE plpgsql \
             SECURITY DEFINER \
             SET search_path = pg_catalog, pg_temp \
             AS $$ BEGIN RETURN NULL; END $$",
        )
        .await
        .expect("modify capture()");

        let dir = tempdir().expect("tempdir");
        write_invoice_config(dir.path());

        auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["verify"])
            .assert()
            .code(1)
            .stdout(predicate::str::contains("auditstream.capture()"))
            .stdout(predicate::str::contains("outdated"));
    }

    // 4. Row in changes_default
    {
        let db = TestDb::new().await.expect("create TestDb");
        create_invoice_table(&db).await;
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
        db.execute(
            "INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
             VALUES ('2020-01-15T10:00:00.000000Z'::timestamptz, 'app', 'legacy.one', 'postgres', 1)",
        )
        .await
        .expect("seed default row");

        let dir = tempdir().expect("tempdir");
        write_invoice_config(dir.path());

        auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["verify"])
            .assert()
            .code(1)
            .stdout(predicate::str::contains("auditstream.changes_default"))
            .stdout(predicate::str::contains("default_partition_rows"));
    }
}

#[tokio::test]
async fn fewer_than_two_months_ahead_fails_and_verify_fix_creates_partitions_and_passes() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
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

    // Drop the two farthest future monthly partitions so only 1 month ahead remains (< 2).
    let parts = db
        .query_strings(
            "SELECT child.relname \
               FROM pg_catalog.pg_inherits i \
               JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent \
               JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace \
               JOIN pg_catalog.pg_class child ON child.oid = i.inhrelid \
              WHERE pn.nspname = 'auditstream' \
                AND parent.relname = 'changes' \
                AND child.relname <> 'changes_default' \
              ORDER BY child.relname DESC \
              LIMIT 2",
        )
        .await
        .expect("list farthest 2 partitions");
    assert_eq!(parts.len(), 2);
    for name in &parts {
        db.execute(&format!("DROP TABLE auditstream.\"{name}\""))
            .await
            .expect("drop future partition");
    }

    let dir = tempdir().expect("tempdir");
    write_invoice_config(dir.path());

    // Without --fix: exits 1 reporting insufficient_partitions.
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("insufficient_partitions"))
        .stdout(predicate::str::contains("auditstream verify --fix"));

    // With --fix: creates the 2 missing partitions (covering 3 months ahead), reports them, and exits 0!
    let fix_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--fix", "--format", "json"])
        .output()
        .expect("verify --fix");
    assert!(
        fix_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&fix_out.stderr)
    );
    let stderr_str = String::from_utf8_lossy(&fix_out.stderr);
    assert!(
        stderr_str.contains("created 2 partition(s)"),
        "expected created partition summary on stderr, got: {stderr_str}"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&fix_out.stdout).expect("parse verify --fix json");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["months_covered_ahead"], 3);
    assert_eq!(
        parsed["created_partitions"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(parsed["findings"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn verify_without_selection_checks_installed_triggers_only_and_notes_on_stderr() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
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

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok: no findings"))
        .stderr(predicate::str::contains("installed triggers only"));
}

#[tokio::test]
async fn verify_reports_invalid_exclusion_when_config_excludes_primary_key() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
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
        "[capture]\ntables = [\"public.invoice\"]\nexclude = [\"public.invoice.id\"]\n",
    )
    .expect("write auditstream.toml");

    let out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json");
    assert_eq!(out.status.code(), Some(1));
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("parse verify json");
    assert_eq!(parsed["ok"], false);
    assert!(
        parsed["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|f| f["kind"] == "invalid_exclusion" && f["severity"] == "error"),
        "expected invalid_exclusion error finding, got: {parsed}"
    );
}

#[test]
fn verify_unreachable_database_exits_3() {
    let dir = tempdir().expect("tempdir");
    auditstream_cmd()
        .current_dir(dir.path())
        .env(
            "AUDITSTREAM_DATABASE_URL",
            "postgresql://postgres:secret@127.0.0.1:1/nope",
        )
        .args(["verify"])
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("secret").not());
}
