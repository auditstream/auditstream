//! End-to-end CLI journeys driving the real `auditstream` binary against live PostgreSQL (F6).

use std::collections::BTreeSet;
use std::collections::hash_map::RandomState;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use auditstream_pg::ddl::quote_ident;
use auditstream_testkit::TestDb;
use insta_cmd::assert_cmd_snapshot;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::tempdir;

use crate::support::{apply_generated_migration, auditstream_cmd, auditstream_std_cmd};

const FIXTURE_SCHEMA_SQL: &str = include_str!("e2e/fixture_schema.sql");

/// Base64url-unpadded `{"type":"user","id":"usr_comment"}`.
const COMMENT_ACTOR_SQL: &str = "/*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2NvbW1lbnQifQ'*/";

fn ephemeral_secret() -> String {
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u8(1);
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u8(2);
    format!("{:016x}{:016x}", h1.finish(), h2.finish())
}

fn role_connection_url(db: &TestDb, role: &str, password: &str) -> String {
    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL contains credentials");
    format!("{scheme}://{role}:{password}@{host_and_db}")
}

fn load_event_validator() -> jsonschema::Validator {
    let schema: Value =
        serde_json::from_str(include_str!("../../../../spec/schemas/event.schema.json"))
            .expect("parse event.schema.json");
    jsonschema::validator_for(&schema).expect("compile event.schema.json validator")
}

fn parse_and_validate_jsonl(raw: &[u8]) -> Vec<Value> {
    let validator = load_event_validator();
    String::from_utf8_lossy(raw)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("JSON line must parse cleanly");
            validator
                .validate(&value)
                .unwrap_or_else(|err| panic!("event violates event.schema.json: {err}\n{value}"));
            value
        })
        .collect()
}

fn run_init(db_url: &str, dir: &Path, format: &str) -> Vec<PathBuf> {
    let out = auditstream_cmd()
        .current_dir(dir)
        .env("AUDITSTREAM_DATABASE_URL", db_url)
        .args([
            "init",
            "--migrations",
            format,
            "--table",
            "public.invoice",
            "--table",
            "public.line_item",
            "--exclude",
            "public.invoice.password_hash",
        ])
        .output()
        .expect("run auditstream init");
    assert!(
        out.status.success(),
        "{format}: init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|rel| dir.join(rel))
        .collect();
    paths.sort();
    paths
}

async fn export_until_jsonl_count(
    db_url: &str,
    dir: &Path,
    output_file: &str,
    args: &[&str],
    expected_count: usize,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = auditstream_cmd()
            .current_dir(dir)
            .env("AUDITSTREAM_DATABASE_URL", db_url)
            .arg("export")
            .args(args)
            .output()
            .expect("run auditstream export");
        assert!(
            out.status.success(),
            "export failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "file export must leave stdout empty");
        let bytes = fs::read(dir.join(output_file)).unwrap_or_default();
        let events = parse_and_validate_jsonl(&bytes);
        if events.len() >= expected_count {
            assert_eq!(events.len(), expected_count);
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected_count} events; got {}",
            events.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_snapshot_xmin_past_max_xact(db: &TestDb) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ready = db
            .query_scalar_i64(
                "SELECT COALESCE((\
                   MAX(xact_id) < pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())\
                 )::int::bigint, 1::bigint) FROM auditstream.changes",
            )
            .await
            .expect("check snapshot xmin");
        if ready == 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pg_snapshot_xmin to advance past seeded xact_id"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn init_apply_and_verify_succeeds_for_prisma_drizzle_and_sql() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    for format in ["prisma", "drizzle", "sql"] {
        let db = TestDb::new().await.expect("create TestDb");
        db.execute_batch(FIXTURE_SCHEMA_SQL)
            .await
            .expect("apply fixture app schema");

        let dir = tempdir().expect("tempdir");
        let generated = run_init(db.url(), dir.path(), format);
        let sql_files: Vec<&PathBuf> = generated
            .iter()
            .filter(|p| p.extension().is_some_and(|ext| ext == "sql"))
            .collect();
        assert!(!sql_files.is_empty(), "{format}: expected generated SQL");

        for sql_file in sql_files {
            apply_generated_migration(&db, format, sql_file).await;
        }

        auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["verify"])
            .assert()
            .success()
            .stdout(predicate::str::contains("ok: no findings"))
            .stderr(predicate::str::is_empty());

        auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["status"])
            .assert()
            .success()
            .stdout(predicate::str::contains("public.invoice"))
            .stdout(predicate::str::contains("public.line_item"))
            .stderr(predicate::str::is_empty());
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn app_role_capture_history_incremental_jsonl_and_strict_csv_journey() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    db.execute_batch(FIXTURE_SCHEMA_SQL)
        .await
        .expect("apply fixture schema");

    let dir = tempdir().expect("tempdir");
    let generated = run_init(db.url(), dir.path(), "sql");
    for path in generated
        .iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "sql"))
    {
        apply_generated_migration(&db, "sql", path).await;
    }

    // Create a least-privilege application role holding ONLY SELECT on auditstream.changes
    // (plus USAGE on schema and EXECUTE on log_event) and DML on application tables.
    let app_role = format!("e2e_app_{}", db.database_name());
    let app_role_q = quote_ident(&app_role);
    let app_pw = ephemeral_secret();
    db.execute_statements(&[
        format!("CREATE ROLE {app_role_q} LOGIN PASSWORD '{app_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE"),
        format!("GRANT USAGE ON SCHEMA public TO {app_role_q}"),
        format!("GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON public.invoice, public.line_item TO {app_role_q}"),
        format!("GRANT USAGE ON SCHEMA auditstream TO {app_role_q}"),
        format!("GRANT SELECT ON auditstream.changes TO {app_role_q}"),
        format!("GRANT EXECUTE ON FUNCTION auditstream.log_event(text, jsonb, jsonb) TO {app_role_q}"),
    ])
    .await
    .expect("create least-privilege app role");

    let app_url = role_connection_url(&db, &app_role, &app_pw);

    // 1. Write with trailing SQL comment actor as the application role.
    db.execute_statements(&[
        "BEGIN".to_owned(),
        format!("SET LOCAL ROLE {app_role_q}"),
        format!(
            "INSERT INTO public.invoice (id, status, amount, password_hash) \
             VALUES (1, 'draft', 100.2500, 'hidden_pw') {COMMENT_ACTOR_SQL}"
        ),
        "COMMIT".to_owned(),
    ])
    .await
    .expect("comment-tagged insert as app role");

    // 2. Write with transaction-local set_config actor as the application role.
    db.execute_statements(&[
        "BEGIN".to_owned(),
        format!("SET LOCAL ROLE {app_role_q}"),
        "SELECT set_config('auditstream.actor', '{\"type\":\"user\",\"id\":\"usr_setcfg\"}', true)"
            .to_owned(),
        "UPDATE public.invoice SET status = 'sent' WHERE id = 1".to_owned(),
        "COMMIT".to_owned(),
    ])
    .await
    .expect("set_config update as app role");

    // 3. Write with no actor as the application role.
    db.execute_statements(&[
        "BEGIN".to_owned(),
        format!("SET LOCAL ROLE {app_role_q}"),
        "INSERT INTO public.invoice (id, status, amount, password_hash) \
         VALUES (2, 'open', 55.5000, 'hidden_pw_2')"
            .to_owned(),
        "COMMIT".to_owned(),
    ])
    .await
    .expect("untagged insert as app role");

    // 4. `auditstream history --format json` runs AS the least-privilege app role
    // and reports actor_source as comment, set_config, and null.
    let hist_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &app_url)
        .args(["history", "--format", "json"])
        .output()
        .expect("run history as app role");
    assert!(
        hist_out.status.success(),
        "history as app role failed: {}",
        String::from_utf8_lossy(&hist_out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&hist_out.stderr), "");

    let hist_events = parse_and_validate_jsonl(&hist_out.stdout);
    assert_eq!(hist_events.len(), 3);
    // Ordered newest first: untagged INSERT (null), set_config UPDATE, comment INSERT.
    assert!(hist_events[0]["actor_source"].is_null());
    assert_eq!(hist_events[1]["actor_source"], "set_config");
    assert_eq!(hist_events[1]["actor"]["id"], "usr_setcfg");
    assert_eq!(hist_events[2]["actor_source"], "comment");
    assert_eq!(hist_events[2]["actor"]["id"], "usr_comment");
    assert!(
        hist_events[2]["after"].get("password_hash").is_none(),
        "excluded column must stay absent from captured images"
    );

    // 5. Incremental JSONL export with cursor file runs AS the least-privilege app role.
    let jsonl_args = [
        "--format",
        "jsonl",
        "--cursor-file",
        "export.cursor",
        "--output",
        "events.jsonl",
    ];
    let batch1 =
        export_until_jsonl_count(&app_url, dir.path(), "events.jsonl", &jsonl_args, 3).await;
    let ids1: BTreeSet<String> = batch1
        .iter()
        .map(|ev| ev["event_id"].as_str().expect("event_id").to_owned())
        .collect();
    assert_eq!(ids1.len(), 3);

    // Write 2 more events (one DB write + one app event via log_event) as app role.
    db.execute_statements(&[
        "BEGIN".to_owned(),
        format!("SET LOCAL ROLE {app_role_q}"),
        format!(
            "INSERT INTO public.line_item (id, invoice_id, qty) VALUES (10, 1, 3) {COMMENT_ACTOR_SQL}"
        ),
        format!(
            "SELECT auditstream.log_event('invoice.exported', '{{\"id\":1}}'::jsonb, '{{\"fmt\":\"pdf\"}}'::jsonb) {COMMENT_ACTOR_SQL}"
        ),
        "COMMIT".to_owned(),
    ])
    .await
    .expect("write additional events as app role");

    let batch2 =
        export_until_jsonl_count(&app_url, dir.path(), "events.jsonl", &jsonl_args, 5).await;
    assert_eq!(
        &batch2[..3],
        &batch1[..],
        "incremental run must preserve earlier events byte-for-byte"
    );
    let ids2: BTreeSet<String> = batch2
        .iter()
        .map(|ev| ev["event_id"].as_str().expect("event_id").to_owned())
        .collect();
    assert_eq!(ids2.len(), 5, "each event_id must appear exactly once");
    assert!(ids1.is_subset(&ids2));

    // 6. Strict RFC 4180 CSV export of the same rows AS the app role holds the exact same event_ids.
    let csv_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &app_url)
        .args(["export", "--format", "csv"])
        .output()
        .expect("run csv export as app role");
    assert!(
        csv_out.status.success(),
        "csv export failed: {}",
        String::from_utf8_lossy(&csv_out.stderr)
    );
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(csv_out.stdout.as_slice());
    assert_eq!(reader.headers().expect("csv headers").len(), 15);
    let csv_ids: BTreeSet<String> = reader
        .records()
        .map(|rec| rec.expect("valid strict CSV record")[0].to_owned())
        .collect();
    assert_eq!(csv_ids, ids2);

    // Clean up cluster-wide app role before dropping the database.
    db.execute_statements(&[
        format!("REVOKE ALL ON SCHEMA public FROM {app_role_q}"),
        format!("REVOKE ALL ON ALL TABLES IN SCHEMA public FROM {app_role_q}"),
        format!("REVOKE ALL ON SCHEMA auditstream FROM {app_role_q}"),
        format!("REVOKE ALL ON auditstream.changes FROM {app_role_q}"),
        format!(
            "REVOKE ALL ON FUNCTION auditstream.log_event(text, jsonb, jsonb) FROM {app_role_q}"
        ),
        format!("DROP ROLE {app_role_q}"),
    ])
    .await
    .expect("drop e2e app role");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn killed_export_over_5000_events_recovers_valid_file_with_every_event_at_least_once() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    db.execute_batch(FIXTURE_SCHEMA_SQL)
        .await
        .expect("apply fixture schema");

    let dir = tempdir().expect("tempdir");
    let generated = run_init(db.url(), dir.path(), "sql");
    for path in generated
        .iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "sql"))
    {
        apply_generated_migration(&db, "sql", path).await;
    }

    // Seed 5,000 events directly into auditstream.changes in one fast set-based INSERT.
    db.execute(
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, after, actor, actor_source, db_user, schema_version) \
         SELECT \
           '2026-06-15T12:00:00Z'::timestamptz + (g || ' milliseconds')::interval, \
           'db', 'public', 'invoice', 'INSERT', \
           jsonb_build_object('id', g), \
           jsonb_build_object('id', g, 'status', 'open'), \
           '{\"type\":\"system\",\"id\":\"seeder\"}'::jsonb, \
           'set_config', 'postgres', 1 \
         FROM generate_series(1, 5000) AS g",
    )
    .await
    .expect("seed 5000 events");

    let expected_ids: BTreeSet<String> = db
        .query_strings("SELECT event_id::text FROM auditstream.changes")
        .await
        .expect("read seeded event_ids")
        .into_iter()
        .collect();
    assert_eq!(expected_ids.len(), 5000);

    // Ensure snapshot xmin is already past the seed transaction so the spawned export writes immediately.
    wait_for_snapshot_xmin_past_max_xact(&db).await;

    let out_path = dir.path().join("bulk.jsonl");

    // Start export with --batch-size 1, wait until at least the first line hits disk, then kill it.
    let mut child = auditstream_std_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--format",
            "jsonl",
            "--batch-size",
            "1",
            "--cursor-file",
            "bulk.cursor",
            "--output",
            "bulk.jsonl",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bulk export");

    let wait_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(bytes) = fs::read(&out_path)
            && bytes.contains(&b'\n')
        {
            break;
        }
        if child.try_wait().expect("poll child").is_some() {
            break;
        }
        assert!(
            Instant::now() < wait_deadline,
            "timed out waiting for first exported line"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let _ = child.kill();
    let _ = child.wait();

    // Re-run export to completion; checkpoint recovery truncates any partial tail and resumes cleanly.
    let resume_deadline = Instant::now() + Duration::from_secs(30);
    let final_events = loop {
        let out = auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args([
                "export",
                "--format",
                "jsonl",
                "--batch-size",
                "500",
                "--cursor-file",
                "bulk.cursor",
                "--output",
                "bulk.jsonl",
            ])
            .output()
            .expect("resume export");
        assert!(
            out.status.success(),
            "resumed export failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bytes = fs::read(&out_path).expect("read bulk.jsonl");
        let events = parse_and_validate_jsonl(&bytes);
        let seen_ids: BTreeSet<String> = events
            .iter()
            .map(|ev| ev["event_id"].as_str().expect("event_id").to_owned())
            .collect();
        if seen_ids.len() >= 5000 {
            break events;
        }
        assert!(
            Instant::now() < resume_deadline,
            "resumed export saw only {} / 5000 unique event_ids",
            seen_ids.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let exported_ids: BTreeSet<String> = final_events
        .iter()
        .map(|ev| ev["event_id"].as_str().expect("event_id").to_owned())
        .collect();
    assert_eq!(
        exported_ids, expected_ids,
        "every seeded event_id must appear at least once after kill + resume"
    );
}

#[tokio::test]
async fn verify_drift_snapshots_for_disabled_dropped_and_default_partition_row() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    db.execute_batch(FIXTURE_SCHEMA_SQL)
        .await
        .expect("apply fixture schema");

    let dir = tempdir().expect("tempdir");
    let generated = run_init(db.url(), dir.path(), "sql");
    let sql_file = generated
        .iter()
        .find(|p| p.extension().is_some_and(|ext| ext == "sql"))
        .expect("generated sql migration");
    apply_generated_migration(&db, "sql", sql_file).await;

    // Store database_url in auditstream.toml so insta-cmd snapshot metadata has zero volatile env vars.
    let existing_toml =
        fs::read_to_string(dir.path().join("auditstream.toml")).expect("read auditstream.toml");
    fs::write(
        dir.path().join("auditstream.toml"),
        format!("database_url = {:?}\n{existing_toml}", db.url()),
    )
    .expect("prepend database_url");

    // 1. Disabled trigger -> exit 1 + insta-cmd snapshot
    db.execute("ALTER TABLE public.invoice DISABLE TRIGGER auditstream_update")
        .await
        .expect("disable trigger");
    let mut cmd_disabled = auditstream_std_cmd();
    cmd_disabled.current_dir(dir.path()).args(["verify"]);
    assert_cmd_snapshot!("e2e_verify_disabled_trigger", cmd_disabled);

    // 2. Dropped trigger -> exit 1 + insta-cmd snapshot
    db.execute("ALTER TABLE public.invoice ENABLE TRIGGER auditstream_update")
        .await
        .expect("re-enable trigger");
    db.execute("DROP TRIGGER auditstream_delete ON public.invoice")
        .await
        .expect("drop trigger");
    let mut cmd_dropped = auditstream_std_cmd();
    cmd_dropped.current_dir(dir.path()).args(["verify"]);
    assert_cmd_snapshot!("e2e_verify_dropped_trigger", cmd_dropped);

    // 3. Row in changes_default -> exit 1 + insta-cmd snapshot
    apply_generated_migration(&db, "sql", sql_file).await;
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
         VALUES ('2020-01-15T10:00:00Z'::timestamptz, 'app', 'legacy.event', 'postgres', 1)",
    )
    .await
    .expect("seed default partition row");
    let mut cmd_default = auditstream_std_cmd();
    cmd_default.current_dir(dir.path()).args(["verify"]);
    assert_cmd_snapshot!("e2e_verify_default_partition_row", cmd_default);
}

#[tokio::test]
async fn every_documented_exit_code_except_internal_is_produced() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    db.execute_batch(FIXTURE_SCHEMA_SQL)
        .await
        .expect("apply fixture schema");

    let dir = tempdir().expect("tempdir");
    let generated = run_init(db.url(), dir.path(), "sql");
    for path in generated
        .iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "sql"))
    {
        apply_generated_migration(&db, "sql", path).await;
    }

    // Exit 0: Success (`verify` on clean install)
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify"])
        .assert()
        .code(0);

    // Exit 1: Check failed (`verify` with disabled trigger)
    db.execute("ALTER TABLE public.invoice DISABLE TRIGGER auditstream_insert")
        .await
        .expect("disable trigger for exit 1");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["verify"])
        .assert()
        .code(1);

    // Exit 2: Usage or config error (`history --record-id` without `--table`)
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--record-id", "1"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());

    // Exit 3: Database error (unreachable PostgreSQL endpoint)
    let unreachable_pw = ephemeral_secret();
    auditstream_cmd()
        .current_dir(dir.path())
        .env(
            "AUDITSTREAM_DATABASE_URL",
            format!("postgresql://postgres:{unreachable_pw}@127.0.0.1:1/nope"),
        )
        .args(["status"])
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(&unreachable_pw).not());

    // Exit 4: Output or cursor I/O error (held export sidecar lock)
    let lock = File::create(dir.path().join("held.cursor.lock")).expect("create sidecar lock");
    lock.try_lock().expect("hold sidecar lock");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--cursor-file",
            "held.cursor",
            "--output",
            "held.jsonl",
        ])
        .assert()
        .code(4)
        .stdout(predicate::str::is_empty());
}
