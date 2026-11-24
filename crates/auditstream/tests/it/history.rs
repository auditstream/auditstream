//! Integration tests for `auditstream history` (F4).

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::Stdio;

use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions};
use auditstream_testkit::TestDb;
use insta_cmd::assert_cmd_snapshot;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::support::{auditstream_cmd, auditstream_std_cmd};

fn load_event_schema() -> jsonschema::Validator {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
    let path = manifest_dir.join("../../spec/schemas/event.schema.json");
    let raw = std::fs::read_to_string(&path).expect("read spec/schemas/event.schema.json");
    let schema: serde_json::Value = serde_json::from_str(&raw).expect("parse event schema JSON");
    jsonschema::validator_for(&schema).expect("compile event.schema.json validator")
}

async fn install_history_schema(db: &TestDb) {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    db.execute_statements(&stmts)
        .await
        .expect("install D2 history schema");
}

async fn seed_fixed_history(db: &TestDb) {
    // Seed events with deterministic timestamps in ascending id order.
    let inserts = [
        // #1: 2026-06-14T23:59:59Z — integer record_id 1, direct user:usr_1
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, after, actor, actor_source, db_user, schema_version) \
         VALUES (\
           '2026-06-14T23:59:59Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', \
           '{\"id\":1}'::jsonb, \
           '{\"id\":1,\"amount\":10.50,\"status\":\"draft\",\"secret_note\":\"hidden_in_table\"}'::jsonb, \
           '{\"type\":\"user\",\"id\":\"usr_1\"}'::jsonb, \
           'set_config', 'app_owner', 1\
         )",
        // #2: 2026-06-15T00:00:00Z — integer record_id 1, ai_agent:bot_7 onBehalfOf user:usr_1,
        // high-precision numeric in `after`, changed=["amount","status"]
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, before, after, changed, actor, actor_source, db_user, schema_version) \
         VALUES (\
           '2026-06-15T00:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'UPDATE', \
           '{\"id\":1}'::jsonb, \
           '{\"id\":1,\"amount\":10.50,\"status\":\"draft\"}'::jsonb, \
           '{\"id\":1,\"amount\":12345678901234567890.123456789,\"status\":\"sent\"}'::jsonb, \
           ARRAY['amount','status'], \
           '{\"type\":\"ai_agent\",\"id\":\"bot_7\",\"onBehalfOf\":{\"type\":\"user\",\"id\":\"usr_1\"}}'::jsonb, \
           'comment', 'app_owner', 1\
         )",
        // #3: 2026-06-15T12:00:00Z — string record_id "1" (distinguished from int 1)
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, after, actor, actor_source, db_user, schema_version) \
         VALUES (\
           '2026-06-15T12:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', \
           '{\"id\":\"1\"}'::jsonb, \
           '{\"id\":\"1\",\"status\":\"string_key\"}'::jsonb, \
           '{\"type\":\"api_key\",\"id\":\"key_9\"}'::jsonb, \
           'set_config', 'app_owner', 1\
         )",
        // #4: 2026-06-15T12:00:00Z (same occurred_at as #3, higher id -> sorts before #3!)
        // composite record_id, absent actor
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, after, actor, actor_source, db_user, schema_version) \
         VALUES (\
           '2026-06-15T12:00:00Z'::timestamptz, 'db', 'public', 'line_item', 'INSERT', \
           '{\"tenant_id\":\"acme\",\"seq\":2}'::jsonb, \
           '{\"tenant_id\":\"acme\",\"seq\":2,\"sku\":\"widget\"}'::jsonb, \
           NULL, NULL, 'app_owner', 1\
         )",
        // #5: 2026-06-15T18:30:00Z — second composite key row for tenant_id=acme
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, after, actor, actor_source, db_user, schema_version) \
         VALUES (\
           '2026-06-15T18:30:00Z'::timestamptz, 'db', 'public', 'line_item', 'INSERT', \
           '{\"tenant_id\":\"acme\",\"seq\":3}'::jsonb, \
           '{\"tenant_id\":\"acme\",\"seq\":3,\"sku\":\"bolt\"}'::jsonb, \
           '{\"type\":\"service\",\"id\":\"sync_1\"}'::jsonb, \
           'jwt', 'app_owner', 1\
         )",
        // #6: 2026-06-16T00:00:00Z — app event (no table/record_id), exact exclusive --until boundary
        "INSERT INTO auditstream.changes \
         (occurred_at, source, op, after, actor, actor_source, db_user, schema_version) \
         VALUES (\
           '2026-06-16T00:00:00Z'::timestamptz, 'app', 'invoice.exported', \
           '{\"format\":\"csv\"}'::jsonb, \
           '{\"type\":\"user\",\"id\":\"usr_2\"}'::jsonb, \
           'set_config', 'app_owner', 1\
         )",
    ];

    for sql in inserts {
        db.execute(sql).await.expect("seed auditstream.changes row");
    }
}

fn parse_json_lines(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8(stdout.to_vec()).expect("stdout is valid UTF-8");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("each line is valid JSON"))
        .collect()
}

#[test]
fn history_help_matches_snapshot() {
    let mut cmd = auditstream_std_cmd();
    cmd.args(["history", "--help"]);
    assert_cmd_snapshot!(cmd);
}

#[test]
fn validation_errors_exit_2_without_connecting() {
    let dir = tempdir().expect("tempdir");

    // --record-id without --table
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--record-id", "1"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--table"));

    // Invalid --table format
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--table", "invoice"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("<schema>.<table>"));

    // --until equal to --since
    auditstream_cmd()
        .current_dir(dir.path())
        .args([
            "history",
            "--since",
            "2026-06-15",
            "--until",
            "2026-06-15T00:00:00Z",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--until"));

    // --until before --since
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--since", "2026-06-16", "--until", "2026-06-15"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--until"));

    // Empty --tenant
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--tenant", ""])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--tenant"));

    // Invalid timestamp
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--since", "not-a-timestamp"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--since"));

    // Out-of-range --limit
    for bad_limit in ["0", "1001"] {
        auditstream_cmd()
            .current_dir(dir.path())
            .args(["history", "--limit", bad_limit])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty());
    }

    // Invalid JSON --record-id kinds (null, array, empty object)
    for bad_rid in ["null", "[1,2]", "{}"] {
        auditstream_cmd()
            .current_dir(dir.path())
            .args([
                "history",
                "--table",
                "public.invoice",
                "--record-id",
                bad_rid,
            ])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty());
    }

    // Missing database URL
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("database URL"));
}

#[test]
fn unreachable_database_exits_3_and_redacts_password() {
    let dir = tempdir().expect("tempdir");
    let secret = "history_secret_pw_54321";
    auditstream_cmd()
        .current_dir(dir.path())
        .env(
            "AUDITSTREAM_DATABASE_URL",
            format!("postgresql://postgres:{secret}@127.0.0.1:1/nope"),
        )
        .args(["history"])
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains(
            "postgresql://postgres:***@127.0.0.1:1/nope",
        ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn history_filters_ordering_table_snapshot_and_json_schema() {
    let db = TestDb::new().await.expect("create TestDb");
    install_history_schema(&db).await;
    seed_fixed_history(&db).await;

    let dir = tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auditstream.toml"),
        format!("database_url = {:?}\n", db.url()),
    )
    .expect("write auditstream.toml");

    // 1. Default table format matches deterministic insta-cmd snapshot and omits before/after
    let mut table_cmd = auditstream_std_cmd();
    table_cmd.current_dir(dir.path()).args(["history"]);
    assert_cmd_snapshot!("history_table_all", table_cmd);

    let table_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--format", "table"])
        .output()
        .expect("run history table");
    assert!(table_out.status.success());
    assert_eq!(String::from_utf8_lossy(&table_out.stderr), "");
    let table_text = String::from_utf8(table_out.stdout).expect("utf8");
    assert!(!table_text.contains("hidden_in_table"));
    assert!(!table_text.contains("12345678901234567890.123456789"));

    // 2. JSON format emits one event per line, every line validates against event.schema.json,
    // preserves before/after and arbitrary-precision numeric, ordered occurred_at DESC, id DESC.
    let json_out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--format", "json"])
        .output()
        .expect("run history json");
    assert!(
        json_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&json_out.stderr), "");
    let raw_json_text = String::from_utf8(json_out.stdout.clone()).expect("utf8");
    assert!(
        raw_json_text.contains("12345678901234567890.123456789"),
        "expected arbitrary-precision numeric preserved in JSON output"
    );

    let validator = load_event_schema();
    let events = parse_json_lines(&json_out.stdout);
    assert_eq!(events.len(), 6);
    for (idx, ev) in events.iter().enumerate() {
        let errors: Vec<String> = validator
            .iter_errors(ev)
            .map(|e| format!("{} at {}", e, e.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "event #{idx} violated event.schema.json: {errors:?}\nevent: {ev}"
        );
    }
    // Check ordering: occurred_at DESC, then id DESC (#4 public.line_item precedes #3 public.invoice at 12:00:00Z)
    let ordered_ops_tables: Vec<(&str, Option<&str>)> = events
        .iter()
        .map(|ev| (ev["op"].as_str().unwrap(), ev["table_name"].as_str()))
        .collect();
    assert_eq!(
        ordered_ops_tables,
        vec![
            ("invoice.exported", None),
            ("INSERT", Some("line_item")),
            ("INSERT", Some("line_item")),
            ("INSERT", Some("invoice")),
            ("UPDATE", Some("invoice")),
            ("INSERT", Some("invoice")),
        ]
    );
    // Verify before/after present on the UPDATE event (#2 -> index 4)
    assert_eq!(events[4]["before"]["status"], "draft");
    assert_eq!(events[4]["after"]["status"], "sent");

    // 3. --table alone
    let out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--table", "public.invoice", "--format", "json"])
        .output()
        .expect("run --table");
    assert!(out.status.success());
    let rows = parse_json_lines(&out.stdout);
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r["table_name"] == "invoice"));

    // 4. --record-id scalar int 1 vs string "1"
    let out_int = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "history",
            "--table",
            "public.invoice",
            "--record-id",
            "1",
            "--format",
            "json",
        ])
        .output()
        .expect("run --record-id 1");
    assert!(out_int.status.success());
    let rows_int = parse_json_lines(&out_int.stdout);
    assert_eq!(rows_int.len(), 2);
    assert_eq!(rows_int[0]["op"], "UPDATE");
    assert_eq!(rows_int[1]["op"], "INSERT");
    assert!(rows_int.iter().all(|r| r["record_id"]["id"] == 1));

    let out_str = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "history",
            "--table",
            "public.invoice",
            "--record-id",
            "\"1\"",
            "--format",
            "json",
        ])
        .output()
        .expect("run --record-id \"1\"");
    assert!(out_str.status.success());
    let rows_str = parse_json_lines(&out_str.stdout);
    assert_eq!(rows_str.len(), 1);
    assert_eq!(rows_str[0]["record_id"]["id"], "1");

    // 5. --record-id JSON object containment (@>) partial and full composite key
    let out_partial = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "history",
            "--table",
            "public.line_item",
            "--record-id",
            r#"{"tenant_id":"acme"}"#,
            "--format",
            "json",
        ])
        .output()
        .expect("run partial composite --record-id");
    assert!(out_partial.status.success());
    let rows_partial = parse_json_lines(&out_partial.stdout);
    assert_eq!(rows_partial.len(), 2);
    assert_eq!(rows_partial[0]["record_id"]["seq"], 3);
    assert_eq!(rows_partial[1]["record_id"]["seq"], 2);

    let out_exact = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "history",
            "--table",
            "public.line_item",
            "--record-id",
            r#"{"tenant_id":"acme","seq":2}"#,
            "--format",
            "json",
        ])
        .output()
        .expect("run exact composite --record-id");
    assert!(out_exact.status.success());
    let rows_exact = parse_json_lines(&out_exact.stdout);
    assert_eq!(rows_exact.len(), 1);
    assert_eq!(rows_exact[0]["record_id"]["seq"], 2);

    // 6. --actor matches both actor.id and actor.onBehalfOf.id
    let out_actor = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--actor", "usr_1", "--format", "json"])
        .output()
        .expect("run --actor usr_1");
    assert!(out_actor.status.success());
    let rows_actor = parse_json_lines(&out_actor.stdout);
    assert_eq!(rows_actor.len(), 2);
    assert_eq!(rows_actor[0]["actor"]["id"], "bot_7");
    assert_eq!(rows_actor[0]["actor"]["onBehalfOf"]["id"], "usr_1");
    assert_eq!(rows_actor[1]["actor"]["id"], "usr_1");

    // 7. --since (inclusive date UTC) and --until (exclusive date UTC)
    let out_range = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "history",
            "--since",
            "2026-06-15",
            "--until",
            "2026-06-16",
            "--format",
            "json",
        ])
        .output()
        .expect("run --since/--until");
    assert!(out_range.status.success());
    let rows_range = parse_json_lines(&out_range.stdout);
    assert_eq!(rows_range.len(), 4);
    assert_eq!(rows_range[0]["occurred_at"], "2026-06-15T18:30:00.000000Z");
    assert_eq!(rows_range[3]["occurred_at"], "2026-06-15T00:00:00.000000Z");

    // 8. --since with non-UTC RFC 3339 offset (09:00-03:00 == 12:00Z)
    let out_offset = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "history",
            "--since",
            "2026-06-15T09:00:00-03:00",
            "--format",
            "json",
        ])
        .output()
        .expect("run --since offset");
    assert!(out_offset.status.success());
    let rows_offset = parse_json_lines(&out_offset.stdout);
    assert_eq!(rows_offset.len(), 4);
    assert_eq!(rows_offset[3]["occurred_at"], "2026-06-15T12:00:00.000000Z");

    // 9. --limit
    let out_limit = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--limit", "2", "--format", "json"])
        .output()
        .expect("run --limit");
    assert!(out_limit.status.success());
    let rows_limit = parse_json_lines(&out_limit.stdout);
    assert_eq!(rows_limit.len(), 2);
    assert_eq!(rows_limit[0]["op"], "invoice.exported");
    assert_eq!(rows_limit[1]["record_id"]["seq"], 3);

    // 10. All filters combined with AND
    let out_combined = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "history",
            "--table",
            "public.invoice",
            "--record-id",
            "1",
            "--actor",
            "usr_1",
            "--since",
            "2026-06-15",
            "--until",
            "2026-06-16",
            "--limit",
            "10",
            "--format",
            "json",
        ])
        .output()
        .expect("run combined filters");
    assert!(out_combined.status.success());
    let rows_combined = parse_json_lines(&out_combined.stdout);
    assert_eq!(rows_combined.len(), 1);
    assert_eq!(rows_combined[0]["op"], "UPDATE");
    assert_eq!(rows_combined[0]["actor"]["id"], "bot_7");

    // 11. No matches -> stdout empty, stderr "no events matched", exit 0
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--actor", "no_such_actor"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::eq("no events matched\n"));
}

#[tokio::test]
async fn history_broken_pipe_exits_0_without_panic() {
    let db = TestDb::new().await.expect("create TestDb");
    install_history_schema(&db).await;

    // Seed enough payload (> 180 KiB) to overflow the OS pipe buffer so closing stdout
    // after one line reliably triggers EPIPE in the child process.
    let pad = "x".repeat(3000);
    let sql = format!(
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, after, actor, actor_source, db_user, schema_version) \
         SELECT \
           '2026-06-15T12:00:00Z'::timestamptz + (g || ' seconds')::interval, \
           'db', 'public', 'invoice', 'INSERT', \
           jsonb_build_object('id', g), \
           jsonb_build_object('id', g, 'blob', '{pad}'), \
           '{{\"type\":\"user\",\"id\":\"usr_pipe\"}}'::jsonb, \
           'set_config', 'app_owner', 1 \
         FROM generate_series(1, 65) AS g"
    );
    db.execute(&sql).await.expect("seed pipe test rows");

    let dir = tempdir().expect("tempdir");

    for format in ["json", "table"] {
        let mut child = auditstream_std_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["history", "--format", format])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn auditstream history");

        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let mut reader = BufReader::new(stdout);
        let mut first_line = String::new();
        let bytes_read = reader.read_line(&mut first_line).expect("read first line");
        assert!(bytes_read > 0, "expected at least one line for {format}");
        if format == "json" {
            let _: serde_json::Value =
                serde_json::from_str(&first_line).expect("first JSON line parses");
        } else {
            assert!(first_line.starts_with("OCCURRED_AT"));
        }

        // Close the read end of the pipe immediately so subsequent writes hit BrokenPipe.
        drop(reader);

        let mut stderr_text = String::new();
        stderr
            .read_to_string(&mut stderr_text)
            .expect("read child stderr");
        let status = child.wait().expect("wait for child");

        assert_eq!(
            status.code(),
            Some(0),
            "expected exit 0 on broken pipe ({format}), stderr={stderr_text:?}"
        );
        assert_eq!(
            stderr_text, "",
            "expected no panic or error on stderr ({format})"
        );
    }
}

#[tokio::test]
async fn history_tenant_filter_matches_direct_and_on_behalf_of() {
    let db = TestDb::new().await.expect("create TestDb");
    install_history_schema(&db).await;

    db.execute(
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, actor, actor_source, db_user, schema_version) VALUES \
         ('2026-06-15T10:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', '{\"id\":1}'::jsonb, \
          '{\"type\":\"user\",\"id\":\"u_1\",\"tenant\":\"org_acme\"}'::jsonb, 'set_config', 'app_owner', 1), \
         ('2026-06-15T11:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'UPDATE', '{\"id\":1}'::jsonb, \
          '{\"type\":\"ai_agent\",\"id\":\"bot_1\",\"onBehalfOf\":{\"type\":\"user\",\"id\":\"u_1\",\"tenant\":\"org_acme\"}}'::jsonb, 'comment', 'app_owner', 1), \
         ('2026-06-15T12:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', '{\"id\":2}'::jsonb, \
          '{\"type\":\"user\",\"id\":\"u_2\",\"tenant\":\"org_beta\"}'::jsonb, 'set_config', 'app_owner', 1), \
         ('2026-06-15T13:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', '{\"id\":3}'::jsonb, \
          NULL, NULL, 'app_owner', 1)",
    )
    .await
    .expect("seed tenant rows");

    let dir = tempdir().expect("tempdir");
    let out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--tenant", "org_acme", "--format", "json"])
        .output()
        .expect("run history --tenant");
    assert!(out.status.success());
    let rows = parse_json_lines(&out.stdout);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["actor"]["id"], "bot_1");
    assert_eq!(rows[0]["actor"]["onBehalfOf"]["tenant"], "org_acme");
    assert_eq!(rows[1]["actor"]["id"], "u_1");
    assert_eq!(rows[1]["actor"]["tenant"], "org_acme");
}
