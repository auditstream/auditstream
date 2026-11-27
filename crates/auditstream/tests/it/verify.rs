//! Integration tests for `auditstream verify` (F3).

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
#[allow(clippy::too_many_lines)]
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

    // Also caught when `capture.tables` is omitted and `capture.exclude` targets an installed table's PK.
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\nexclude = [\"public.invoice.id\"]\n",
    )
    .expect("write exclude-only auditstream.toml");
    let out_installed = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json without capture.tables");
    assert_eq!(out_installed.status.code(), Some(1));
    let parsed_installed: serde_json::Value =
        serde_json::from_slice(&out_installed.stdout).expect("parse verify json");
    assert!(
        parsed_installed["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|f| f["kind"] == "invalid_exclusion" && f["severity"] == "error"),
        "expected invalid_exclusion without capture.tables, got: {parsed_installed}"
    );

    // Without `capture.tables`, configured `capture.exclude` replaces (not unions) live excludes.
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\nexclude = [\"public.invoice.status\"]\n",
    )
    .expect("write swapped exclude config without capture.tables");
    let out_swapped = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json on swapped exclude");
    assert_eq!(out_swapped.status.code(), Some(1));
    let parsed_swapped: serde_json::Value =
        serde_json::from_slice(&out_swapped.stdout).expect("parse verify swapped json");
    assert!(
        parsed_swapped["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|f| {
                f["kind"] == "outdated"
                    && f["detail"]
                        .as_str()
                        .is_some_and(|d| d.contains(r#"expected ["status"]"#))
            }),
        "expected outdated finding with exact configured exclude [\"status\"], got: {parsed_swapped}"
    );

    // Leaked child-partition privileges and PUBLIC execute on capture() report `unsafe_privileges`.
    fs::remove_file(dir.path().join("auditstream.toml")).expect("remove config");
    db.execute("GRANT DELETE ON TABLE auditstream.changes_default TO PUBLIC")
        .await
        .expect("inject unsafe partition grant");
    let out_acl = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json on unsafe ACL");
    assert_eq!(out_acl.status.code(), Some(1));
    let parsed_acl: serde_json::Value =
        serde_json::from_slice(&out_acl.stdout).expect("parse verify acl json");
    assert!(
        parsed_acl["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|f| f["kind"] == "unsafe_privileges"
                && f["object"] == "auditstream.changes_default"),
        "expected unsafe_privileges finding on changes_default, got: {parsed_acl}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn partitioned_table_warning_exits_0_by_default_and_1_with_fail_on_warning() {
    let db = TestDb::new().await.expect("create TestDb");
    db.execute(
        "CREATE TABLE public.orders ( \
           id int NOT NULL, \
           created_year int NOT NULL, \
           total numeric NOT NULL, \
           PRIMARY KEY (id, created_year) \
         ) PARTITION BY RANGE (created_year)",
    )
    .await
    .expect("create partitioned orders");
    db.execute(
        "CREATE TABLE public.orders_2026 PARTITION OF public.orders \
         FOR VALUES FROM (2026) TO (2027)",
    )
    .await
    .expect("create orders_2026");

    install_capture(
        &db,
        &[TableTriggerSpec::new(
            "public",
            "orders",
            vec!["id".to_owned(), "created_year".to_owned()],
            vec![],
        )
        .partitioned()],
    )
    .await;

    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\ntables = [\"public.orders\"]\n",
    )
    .expect("write auditstream.toml");

    // 1. Default `--fail-on error` prints the warning and exits 0.
    let text_ok = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify"])
        .output()
        .expect("run verify default on partitioned table");
    assert!(
        text_ok.status.success(),
        "warning-only verify must exit 0 by default; stderr: {}",
        String::from_utf8_lossy(&text_ok.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&text_ok.stderr), "");
    let text_stdout = String::from_utf8_lossy(&text_ok.stdout);
    assert!(text_stdout.contains("[warning]"), "stdout: {text_stdout}");
    assert!(
        text_stdout.contains("partitioned_table"),
        "stdout: {text_stdout}"
    );

    // 2. Default JSON reports `"ok": true` while preserving the warning finding.
    let json_ok = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json on partitioned table");
    assert!(json_ok.status.success());
    let parsed_ok: serde_json::Value =
        serde_json::from_slice(&json_ok.stdout).expect("parse json_ok");
    assert_eq!(parsed_ok["ok"], true);
    let findings_ok = parsed_ok["findings"].as_array().expect("findings array");
    assert_eq!(findings_ok.len(), 1);
    assert_eq!(findings_ok[0]["severity"], "warning");
    assert_eq!(findings_ok[0]["kind"], "partitioned_table");

    // 3. Strict `--fail-on warning` exits 1 and sets `"ok": false`.
    let strict_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--fail-on", "warning", "--format", "json"])
        .output()
        .expect("run verify --fail-on warning");
    assert_eq!(strict_out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&strict_out.stderr)
            .contains("verification failed with 1 finding(s)"),
        "stderr: {}",
        String::from_utf8_lossy(&strict_out.stderr)
    );
    let parsed_strict: serde_json::Value =
        serde_json::from_slice(&strict_out.stdout).expect("parse json_strict");
    assert_eq!(parsed_strict["ok"], false);

    // 4. Mixed error + warning: errors sort first; `--fail-on error` reports 1 failing finding
    //    while `--fail-on warning` reports 2 failing findings.
    db.execute("ALTER TABLE public.orders DISABLE TRIGGER auditstream_insert")
        .await
        .expect("disable orders insert trigger");
    let mixed_err = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--fail-on", "error", "--format", "json"])
        .output()
        .expect("run verify mixed --fail-on error");
    assert_eq!(mixed_err.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&mixed_err.stderr)
            .contains("verification failed with 1 finding(s)"),
        "stderr: {}",
        String::from_utf8_lossy(&mixed_err.stderr)
    );
    let parsed_mixed: serde_json::Value =
        serde_json::from_slice(&mixed_err.stdout).expect("parse mixed json");
    let mixed_findings = parsed_mixed["findings"].as_array().expect("findings array");
    assert_eq!(mixed_findings.len(), 2);
    assert_eq!(mixed_findings[0]["severity"], "error");
    assert_eq!(mixed_findings[1]["severity"], "warning");

    let mixed_warn = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--fail-on", "warning"])
        .output()
        .expect("run verify mixed --fail-on warning");
    assert_eq!(mixed_warn.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&mixed_warn.stderr)
            .contains("verification failed with 2 finding(s)"),
        "stderr: {}",
        String::from_utf8_lossy(&mixed_warn.stderr)
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn verify_cli_reports_tampered_history_renamed_exclude_and_missing_default_and_index() {
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

    db.execute("ALTER TABLE public.invoice RENAME COLUMN password_hash TO secret_hash")
        .await
        .expect("rename excluded column");
    db.execute("ALTER FUNCTION auditstream.log_event(text, jsonb, jsonb) SET work_mem = '8MB'")
        .await
        .expect("tamper log_event proconfig");
    db.execute("ALTER TABLE auditstream.changes DETACH PARTITION auditstream.changes_default")
        .await
        .expect("detach changes_default");
    db.execute("DROP INDEX auditstream.changes_lookup_idx")
        .await
        .expect("drop changes_lookup_idx");
    db.execute("ALTER TABLE auditstream.changes ENABLE ROW LEVEL SECURITY")
        .await
        .expect("enable unexpected RLS");

    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\ntables = [\"public.invoice\"]\nexclude = [\"public.invoice.password_hash\", \"public.typo_table.secret\"]\n",
    )
    .expect("write auditstream.toml with renamed and unmatched excludes");

    let out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json");
    assert_eq!(out.status.code(), Some(1));
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("parse verify json");
    assert_eq!(parsed["ok"], false);
    let findings = parsed["findings"].as_array().expect("findings array");

    for (kind, object) in [
        ("invalid_exclusion", "public.invoice"),
        ("invalid_exclusion", "public.typo_table"),
        ("outdated", "auditstream.log_event(text, jsonb, jsonb)"),
        ("missing", "auditstream.changes_default"),
        ("missing", "auditstream.changes_lookup_idx"),
        ("rls_without_select_policy", "auditstream.changes"),
    ] {
        assert!(
            findings
                .iter()
                .any(|f| f["kind"] == kind && f["object"] == object && f["severity"] == "error"),
            "expected ({kind}, {object}) error finding, got: {parsed}"
        );
    }

    // Without `capture.tables`, renamed live trigger exclusions and unmatched `capture.exclude`
    // entries are still flagged as `invalid_exclusion`.
    fs::write(
        dir.path().join("auditstream.toml"),
        "[capture]\nexclude = [\"public.typo_table.secret\"]\n",
    )
    .expect("write exclude-only auditstream.toml");
    let out_omitted = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json without capture.tables");
    assert_eq!(out_omitted.status.code(), Some(1));
    let parsed_omitted: serde_json::Value =
        serde_json::from_slice(&out_omitted.stdout).expect("parse omitted json");
    let omitted_findings = parsed_omitted["findings"]
        .as_array()
        .expect("findings array");
    assert!(
        omitted_findings
            .iter()
            .any(|f| f["kind"] == "invalid_exclusion" && f["object"] == "public.typo_table"),
        "expected unmatched exclude invalid_exclusion without capture.tables, got: {parsed_omitted}"
    );
    assert!(
        omitted_findings
            .iter()
            .any(|f| f["kind"] == "invalid_exclusion"
                && f["object"] == "public.invoice.auditstream_update"),
        "expected live-trigger renamed exclude invalid_exclusion without capture.tables, got: {parsed_omitted}"
    );
}

#[tokio::test]
async fn verify_cli_reports_hostile_rls_policy_with_attached_default_partition_without_crashing() {
    use auditstream_pg::ddl::quote_ident;

    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;

    let app_role = format!("rls_app_{}", db.database_name());
    let app_pw = uuid::Uuid::new_v4().simple().to_string();
    let app_q = quote_ident(&app_role);
    db.execute(&format!(
        "CREATE ROLE {app_q} LOGIN PASSWORD '{app_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE"
    ))
    .await
    .expect("create non-superuser app_role");

    {
        let _lock = TestDb::cluster_lock().await.expect("cluster lock");
        let all = migrations::install_statements(
            &DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]),
            &[TableTriggerSpec::new(
                "public",
                "invoice",
                vec!["id".to_owned()],
                vec!["password_hash".to_owned()],
            )],
        )
        .expect("compose D2+D3 install statements");
        db.execute_statements(&all).await.expect("install D2+D3");
    }

    // Seed a row in `changes_default` (so PostgreSQL would evaluate the SELECT RLS policy if any
    // query touched `auditstream.changes`), then install a hostile `1 / 0 = 1` RLS policy while
    // keeping `changes_default` attached. Running `auditstream verify` as non-superuser `app_role`
    // must exit 1 with `forced_rls` and `rls_hides_default_partition`, never exit 3 (`division by zero`).
    db.execute_batch(
        "INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
           VALUES ('2020-01-15T10:00:00.000000Z'::timestamptz, 'app', 'rls.seed', 'postgres', 1); \
         ALTER TABLE auditstream.changes ENABLE ROW LEVEL SECURITY; \
         ALTER TABLE auditstream.changes FORCE ROW LEVEL SECURITY; \
         CREATE POLICY rls_trap ON auditstream.changes FOR SELECT USING (1 / 0 = 1);",
    )
    .await
    .expect("seed default row and install hostile RLS policy");

    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    let app_url = format!("{scheme}://{app_role}:{app_pw}@{host_and_db}");

    let dir = tempdir().expect("tempdir");
    write_invoice_config(dir.path());

    let out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &app_url)
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify json with hostile RLS as app_role");
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected CheckFailed (1), not Database (3); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("parse verify json");
    assert_eq!(parsed["ok"], false);
    let findings = parsed["findings"].as_array().expect("findings array");
    assert!(
        findings.iter().any(|f| {
            f["kind"] == "forced_rls"
                && f["severity"] == "error"
                && f["object"] == "auditstream.changes"
        }),
        "expected forced_rls error on auditstream.changes, got: {parsed}"
    );
    assert!(
        findings.iter().any(|f| {
            f["kind"] == "rls_hides_default_partition"
                && f["severity"] == "warning"
                && f["object"] == "auditstream.changes_default"
        }),
        "expected rls_hides_default_partition warning on auditstream.changes_default, got: {parsed}"
    );

    db.execute_batch(&format!(
        "DROP SCHEMA auditstream CASCADE; DROP ROLE IF EXISTS {app_q};"
    ))
    .await
    .expect("clean up app_role");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn verify_cli_rls_checks_and_app_roles_via_config_and_flag() {
    use auditstream_pg::ddl::quote_ident;

    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let mut db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;

    let suffix = db.database_name().to_owned();
    let app_role = format!("vcli_app_{suffix}");
    let exp_role = format!("vcli_exp_{suffix}");
    let bypass_role = format!("vcli_byp_{suffix}");
    db.track_role(&app_role);
    db.track_role(&exp_role);
    db.track_role(&bypass_role);

    let app_pw = uuid::Uuid::new_v4().simple().to_string();
    let exp_pw = uuid::Uuid::new_v4().simple().to_string();
    let app_q = quote_ident(&app_role);
    let exp_q = quote_ident(&exp_role);
    let byp_q = quote_ident(&bypass_role);

    db.execute_batch(&format!(
        "CREATE ROLE {app_q} LOGIN PASSWORD '{app_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS; \
         CREATE ROLE {exp_q} LOGIN PASSWORD '{exp_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS; \
         CREATE ROLE {byp_q} NOLOGIN NOSUPERUSER BYPASSRLS;"
    ))
    .await
    .expect("create CLI verify RLS roles");

    let all = migrations::install_statements(
        &DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]),
        &[TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["password_hash".to_owned()],
        )],
    )
    .expect("compose install statements");
    db.execute_statements(&all).await.expect("install capture");

    db.execute_batch(&format!(
        "GRANT USAGE ON SCHEMA auditstream TO {exp_q}; \
         GRANT SELECT ON auditstream.changes TO {exp_q}; \
         ALTER TABLE auditstream.changes ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY tenant_app ON auditstream.changes FOR SELECT TO {app_q} \
           USING (actor->>'tenant' = pg_catalog.current_setting('app.tenant', true)); \
         CREATE POLICY exporter_all ON auditstream.changes FOR SELECT TO {exp_q} USING (true);"
    ))
    .await
    .expect("enable multi-tenant RLS");

    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    let app_url = format!("{scheme}://{app_role}:{app_pw}@{host_and_db}");
    let exp_url = format!("{scheme}://{exp_role}:{exp_pw}@{host_and_db}");

    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("auditstream.toml"),
        format!("[capture]\ntables = [\"public.invoice\"]\nexclude = [\"public.invoice.password_hash\"]\napp_roles = [\"{bypass_role}\"]\n"),
    )
    .expect("write auditstream.toml with capture.app_roles");

    // 1. As app_user: warnings (rls_hides_default_partition + rls_bypassed_by_app_role from config)
    //    exit 0 under default --fail-on error and exit 1 under --fail-on warning.
    let app_err_mode = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &app_url)
        .args(["verify", "--format", "json"])
        .output()
        .expect("run verify as app_user");
    assert_eq!(app_err_mode.status.code(), Some(0));
    let parsed: serde_json::Value =
        serde_json::from_slice(&app_err_mode.stdout).expect("parse json");
    assert_eq!(parsed["ok"], true);
    let findings = parsed["findings"].as_array().expect("findings array");
    assert_eq!(
        findings.len(),
        2,
        "expected exactly 2 warnings, got: {parsed}"
    );
    assert!(findings.iter().any(|f| {
        f["kind"] == "rls_hides_default_partition"
            && f["severity"] == "warning"
            && f["object"] == "auditstream.changes_default"
    }));
    assert!(findings.iter().any(|f| {
        f["kind"] == "rls_bypassed_by_app_role"
            && f["severity"] == "warning"
            && f["object"] == bypass_role
    }));

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &app_url)
        .args(["verify", "--fail-on", "warning"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("rls_hides_default_partition"))
        .stdout(predicate::str::contains("rls_bypassed_by_app_role"));

    // 2. As auditstream_exporter with `--app-role <safe_app_role>` overriding config:
    //    passes cleanly with zero findings even under `--fail-on warning`!
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &exp_url)
        .args(["verify", "--fail-on", "warning", "--app-role", &app_role])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("ok: no findings"));

    // 3. Non-vacuous `--app-role` positive check: omit `app_roles` from config and pass the
    //    bypassing role via `--app-role` -> fails under `--fail-on warning` with exactly 1 finding!
    write_invoice_config(dir.path());
    let flag_bypass = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &exp_url)
        .args([
            "verify",
            "--format",
            "json",
            "--fail-on",
            "warning",
            "--app-role",
            &bypass_role,
        ])
        .output()
        .expect("run verify with --app-role <bypass_role>");
    assert_eq!(flag_bypass.status.code(), Some(1));
    let flag_parsed: serde_json::Value =
        serde_json::from_slice(&flag_bypass.stdout).expect("parse flag json");
    assert_eq!(flag_parsed["ok"], false);
    let flag_findings = flag_parsed["findings"].as_array().expect("findings array");
    assert_eq!(
        flag_findings.len(),
        1,
        "expected exactly 1 finding from --app-role override, got: {flag_parsed}"
    );
    assert_eq!(flag_findings[0]["kind"], "rls_bypassed_by_app_role");
    assert_eq!(flag_findings[0]["severity"], "warning");
    assert_eq!(flag_findings[0]["object"], bypass_role);
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
