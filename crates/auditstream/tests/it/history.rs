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

    // Invalid --table formats
    for bad_table in ["a.b.c", ".invoice", "public."] {
        auditstream_cmd()
            .current_dir(dir.path())
            .args(["history", "--table", bad_table])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("<schema>.<table>"));
    }

    // --diff cannot be combined with --format json
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--diff", "--format", "json"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--diff"));

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
fn redaction_validation_errors_exit_2_without_connecting() {
    let dir = tempdir().expect("tempdir");

    // --redact cannot be combined with --unredacted
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--redact", "--unredacted"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--unredacted"));

    // --omit-payload cannot be combined with --diff
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--diff", "--omit-payload"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--omit-payload"));

    // --redact fails closed when no [[export.redaction.rules]] are configured (with or without --omit-payload)
    for args in [
        &["history", "--redact"][..],
        &["history", "--redact", "--omit-payload"][..],
    ] {
        auditstream_cmd()
            .current_dir(dir.path())
            .args(args)
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("export.redaction.rules"));
    }

    // Invalid [[export.redaction.rules]] in auditstream.toml exits 2 without connecting
    let bad_cfg_dir = tempdir().expect("tempdir");
    std::fs::write(
        bad_cfg_dir.path().join("auditstream.toml"),
        "[[export.redaction.rules]]\ntable = \"public.invoice\"\ncolumns = [\"metadata.ssn\"]\n",
    )
    .expect("write invalid redaction config");
    auditstream_cmd()
        .current_dir(bad_cfg_dir.path())
        .args(["history"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("nested JSON path"));
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

    // 3. --table alone (qualified and unqualified defaulting to `public`)
    for table_arg in ["public.invoice", "invoice"] {
        let out = auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["history", "--table", table_arg, "--format", "json"])
            .output()
            .expect("run --table");
        assert!(out.status.success());
        let rows = parse_json_lines(&out.stdout);
        assert_eq!(rows.len(), 3, "table_arg={table_arg}");
        assert!(rows.iter().all(|r| r["table_name"] == "invoice"));
    }

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

    for (label, args) in [
        ("json", vec!["history", "--format", "json"]),
        ("table", vec!["history", "--format", "table"]),
        ("diff", vec!["history", "--diff"]),
    ] {
        let mut child = auditstream_std_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn auditstream history");

        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let mut reader = BufReader::new(stdout);
        let mut first_line = String::new();
        let bytes_read = reader.read_line(&mut first_line).expect("read first line");
        assert!(bytes_read > 0, "expected at least one line for {label}");
        match label {
            "json" => {
                let _: serde_json::Value =
                    serde_json::from_str(&first_line).expect("first JSON line parses");
            }
            "table" => {
                assert!(first_line.starts_with("OCCURRED_AT"));
            }
            "diff" => {
                assert!(first_line.contains("public.invoice"), "{first_line:?}");
            }
            _ => unreachable!(),
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
            "expected exit 0 on broken pipe ({label}), stderr={stderr_text:?}"
        );
        assert_eq!(
            stderr_text, "",
            "expected no panic or error on stderr ({label})"
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

#[tokio::test]
async fn history_diff_renders_update_insert_delete_and_sanitizes_terminal_output() {
    let db = TestDb::new().await.expect("create TestDb");
    install_history_schema(&db).await;

    db.execute(
        r#"INSERT INTO auditstream.changes
           (occurred_at, source, schema_name, table_name, op, record_id, before, after, changed, actor, actor_source, db_user, schema_version) VALUES
           ('2026-06-15T10:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', '{"id":1}'::jsonb,
            NULL, '{"id":1,"status":"draft","note":"ok"}'::jsonb, NULL,
            '{"type":"user","id":"usr_1"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T11:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'UPDATE', '{"id":1}'::jsonb,
            '{"id":1,"status":"draft","note":"ok"}'::jsonb,
            jsonb_build_object('id', 1, 'status', 'sent', 'note', E'\x1b[31mred\x1b[0m' || E'\x1b]52;c;YWJj\x07' || U&'\009B' || '2J' || U&'\202E' || 'clean'),
            ARRAY['note','status'],
            jsonb_build_object('type', 'user', 'id', E'\x1b[31madmin\x1b[0m'),
            'set_config', 'app_owner', 1),
           ('2026-06-15T12:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'DELETE', '{"id":1}'::jsonb,
            '{"id":1,"status":"sent"}'::jsonb, NULL, NULL,
            '{"type":"user","id":"usr_2"}'::jsonb, 'comment', 'app_owner', 1)"#,
    )
    .await
    .expect("seed diff rows");

    let dir = tempdir().expect("tempdir");
    let out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--table", "invoice", "--diff"])
        .output()
        .expect("run history --diff");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");

    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert!(
        !stdout.chars().any(|ch| (ch.is_control() && ch != '\n')
            || matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')),
        "diff output leaked control or BiDi char: {stdout:?}"
    );
    assert!(
        stdout.contains("~ status: \"draft\" -> \"sent\"\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("~ note: \"ok\" -> \"redclean\"\n"),
        "{stdout}"
    );
    assert!(stdout.contains("+ status: \"draft\"\n"), "{stdout}");
    assert!(stdout.contains("- status: \"sent\"\n"), "{stdout}");
    assert!(stdout.contains("user:admin (set_config)"), "{stdout}");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn history_applies_configured_redaction_supports_unredacted_and_omit_payload() {
    let db = TestDb::new().await.expect("create TestDb");
    install_history_schema(&db).await;

    db.execute(
        r#"INSERT INTO auditstream.changes
           (occurred_at, source, schema_name, table_name, op, record_id, before, after, changed, actor, actor_source, db_user, schema_version) VALUES
           ('2026-06-15T10:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', '{"id":1}'::jsonb,
            NULL,
            '{"id":1,"ssn":"111-11-1111","tax_id":"TAX-INIT","amount":10.50,"status":"draft"}'::jsonb,
            NULL,
            '{"type":"user","id":"usr_1"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T11:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'UPDATE', '{"id":1}'::jsonb,
            '{"id":1,"ssn":"111-11-1111","tax_id":"TAX-INIT","amount":10.50,"status":"draft"}'::jsonb,
            jsonb_build_object(
              'id', 1,
              'ssn', '222-22-2222',
              'tax_id', 'TAX-NEXT',
              'amount', 12345678901234567890.123456789::numeric,
              'status', E'\x1b[31msent\x1b[0m'
            ),
            ARRAY['ssn','tax_id','amount','status'],
            '{"type":"user","id":"usr_1"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T12:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'UPDATE', '{"id":2}'::jsonb,
            '{"id":2,"tax_id":"TAX-ONLY-OLD","status":"open"}'::jsonb,
            '{"id":2,"tax_id":"TAX-ONLY-NEW","status":"open"}'::jsonb,
            ARRAY['tax_id'],
            '{"type":"user","id":"usr_2"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T12:30:00Z'::timestamptz, 'db', 'public', 'invoice', 'UPDATE', '{"id":3}'::jsonb,
            '{"id":3,"ssn":"333-33-3333","tax_id":"TAX-NULL-OLD","status":"open"}'::jsonb,
            '{"id":3,"ssn":"444-44-4444","tax_id":"TAX-NULL-NEW","status":"open"}'::jsonb,
            NULL,
            '{"type":"user","id":"usr_2"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T12:45:00Z'::timestamptz, 'db', 'public', 'invoice', 'DELETE', '{"id":4}'::jsonb,
            '{"id":4,"ssn":"555-55-5555","tax_id":"TAX-DEL","status":"void"}'::jsonb,
            NULL,
            NULL,
            '{"type":"user","id":"usr_2"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T13:00:00Z'::timestamptz, 'app', NULL, NULL, 'invoice.exported', '{"id":1}'::jsonb,
            NULL,
            '{"recipient_email":"secret@example.com","format":"csv"}'::jsonb,
            NULL,
            '{"type":"user","id":"usr_3"}'::jsonb, 'set_config', 'app_owner', 1)"#,
    )
    .await
    .expect("seed redaction rows");

    let dir = tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auditstream.toml"),
        format!(
            "database_url = {:?}\n\n\
             [[export.redaction.rules]]\n\
             table = \"public.invoice\"\n\
             columns = [\"ssn\"]\n\
             mode = \"mask\"\n\n\
             [[export.redaction.rules]]\n\
             table = \"invoice\"\n\
             columns = [\"tax_id\"]\n\
             mode = \"remove\"\n\n\
             [[export.redaction.rules]]\n\
             action = \"*\"\n\
             columns = [\"recipient_email\"]\n\
             mode = \"mask\"\n",
            db.url()
        ),
    )
    .expect("write auditstream.toml");

    let redacted_values = [
        "111-11-1111",
        "222-22-2222",
        "333-33-3333",
        "444-44-4444",
        "555-55-5555",
        "TAX-INIT",
        "TAX-NEXT",
        "TAX-ONLY-OLD",
        "TAX-ONLY-NEW",
        "TAX-NULL-OLD",
        "TAX-NULL-NEW",
        "TAX-DEL",
        "secret@example.com",
    ];
    let validator = load_event_schema();

    // 1. Default --format json automatically applies configured redaction and emits stderr note.
    let json_out = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--format", "json"])
        .output()
        .expect("run redacted history json");
    assert!(
        json_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let json_stdout = String::from_utf8(json_out.stdout.clone()).expect("utf8 stdout");
    let json_stderr = String::from_utf8(json_out.stderr).expect("utf8 stderr");
    for (idx, value) in redacted_values.iter().enumerate() {
        assert!(
            !json_stdout.contains(value),
            "json stdout leaked redacted fixture #{idx}"
        );
    }
    assert!(
        !json_stdout.contains("tax_id"),
        "removed column name leaked in json stdout: {json_stdout}"
    );
    assert!(
        json_stdout.contains("12345678901234567890.123456789"),
        "expected untouched arbitrary-precision numeric preserved"
    );
    assert_eq!(
        json_stderr,
        "redaction active (3 configured rules; pass --unredacted to disable)\n"
    );

    let events = parse_json_lines(&json_out.stdout);
    assert_eq!(events.len(), 6);
    for (idx, ev) in events.iter().enumerate() {
        let errors: Vec<String> = validator
            .iter_errors(ev)
            .map(|e| format!("{} at {}", e, e.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "redacted event #{idx} violated event.schema.json: {errors:?}\nevent: {ev}"
        );
    }
    assert_eq!(events[0]["after"]["recipient_email"], "[redacted]");
    assert_eq!(events[0]["after"]["format"], "csv");
    assert_eq!(events[1]["op"], "DELETE");
    assert_eq!(events[1]["before"]["ssn"], "[redacted]");
    assert!(events[1]["before"].get("tax_id").is_none());
    assert!(events[2]["changed"].is_null());
    assert_eq!(events[2]["before"]["ssn"], "[redacted]");
    assert_eq!(events[2]["after"]["ssn"], "[redacted]");
    assert_eq!(events[3]["changed"], serde_json::json!([]));
    assert_eq!(events[4]["before"]["ssn"], "[redacted]");
    assert_eq!(events[4]["after"]["ssn"], "[redacted]");
    assert_eq!(
        events[4]["changed"],
        serde_json::json!(["ssn", "amount", "status"])
    );
    assert_eq!(events[5]["after"]["ssn"], "[redacted]");

    // 2. Explicit --redact succeeds with configured rules, --log-format json formats stderr note,
    //    and --quiet suppresses the stderr note.
    let json_log_out = auditstream_cmd()
        .current_dir(dir.path())
        .args([
            "--log-format",
            "json",
            "history",
            "--redact",
            "--format",
            "json",
        ])
        .output()
        .expect("run --redact with --log-format json");
    assert!(json_log_out.status.success());
    assert_eq!(json_log_out.stdout, json_out.stdout);
    let json_log_stderr = String::from_utf8(json_log_out.stderr).expect("utf8 stderr");
    let log_lines: Vec<&str> = json_log_stderr.lines().collect();
    assert_eq!(
        log_lines.len(),
        1,
        "expected single JSON stderr note: {json_log_stderr}"
    );
    let log_record: serde_json::Value =
        serde_json::from_str(log_lines[0]).expect("valid JSON stderr note");
    assert_eq!(log_record["level"], "INFO");
    assert!(
        log_record["message"]
            .as_str()
            .is_some_and(|m| m.contains("redaction active"))
    );

    let quiet_out = auditstream_cmd()
        .current_dir(dir.path())
        .args(["--quiet", "history", "--format", "json"])
        .output()
        .expect("run quiet redacted history");
    assert!(quiet_out.status.success());
    assert_eq!(String::from_utf8_lossy(&quiet_out.stderr), "");
    assert_eq!(quiet_out.stdout, json_out.stdout);

    // 3. --diff automatically redacts values, drops removed columns, handles DELETE and NULL changed,
    //    and preserves terminal escaping.
    let diff_out = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--diff"])
        .output()
        .expect("run redacted history --diff");
    assert!(diff_out.status.success());
    let diff_stdout = String::from_utf8(diff_out.stdout).expect("utf8 diff stdout");
    let diff_stderr = String::from_utf8(diff_out.stderr).expect("utf8 diff stderr");
    for (idx, value) in redacted_values.iter().enumerate() {
        assert!(
            !diff_stdout.contains(value),
            "diff stdout leaked redacted fixture #{idx}"
        );
    }
    assert!(
        !diff_stdout.contains("tax_id"),
        "diff leaked removed column"
    );
    assert!(
        !diff_stdout.chars().any(|ch| ch.is_control() && ch != '\n'),
        "diff output leaked control char: {diff_stdout:?}"
    );
    assert_eq!(
        diff_stdout
            .matches("~ ssn: \"[redacted]\" -> \"[redacted]\"\n")
            .count(),
        2,
        "expected both UPDATE with changed and UPDATE with NULL changed to show masked ssn diff: {diff_stdout}"
    );
    assert!(diff_stdout.contains("- ssn: \"[redacted]\"\n"));
    assert!(diff_stdout.contains("~ status: \"draft\" -> \"sent\"\n"));
    assert!(diff_stdout.contains("+ recipient_email: \"[redacted]\"\n"));
    assert!(diff_stdout.contains("  (no column changes)\n"));
    assert!(diff_stderr.contains("redaction active"));

    // 4. --format table drops removed columns from CHANGED and emits stderr note.
    let table_out = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--format", "table"])
        .output()
        .expect("run redacted history table");
    assert!(table_out.status.success());
    let table_stdout = String::from_utf8(table_out.stdout).expect("utf8 table stdout");
    assert!(
        !table_stdout.contains("tax_id"),
        "table leaked removed col: {table_stdout}"
    );
    assert!(table_stdout.contains("ssn,amount,status"), "{table_stdout}");
    let all_removed_row = table_stdout
        .lines()
        .find(|line| line.starts_with("2026-06-15T12:00:00.000000Z"))
        .expect("row where only removed column changed");
    assert!(all_removed_row.ends_with("  -"), "{all_removed_row}");
    assert!(String::from_utf8_lossy(&table_out.stderr).contains("redaction active"));

    // 5. Empty query result with redaction configured emits only "no events matched".
    auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--actor", "no_such_actor"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::eq("no events matched\n"));

    // 6. --unredacted break-glass restores raw values and suppresses redaction stderr note.
    let unredacted_json = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--format", "json", "--unredacted"])
        .output()
        .expect("run --unredacted json");
    assert!(unredacted_json.status.success());
    assert_eq!(String::from_utf8_lossy(&unredacted_json.stderr), "");
    let raw_json_stdout = String::from_utf8(unredacted_json.stdout).expect("utf8");
    for (idx, value) in redacted_values.iter().enumerate() {
        assert!(
            raw_json_stdout.contains(value),
            "expected --unredacted to include fixture #{idx}"
        );
    }

    let unredacted_diff = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--diff", "--unredacted"])
        .output()
        .expect("run --unredacted diff");
    assert!(unredacted_diff.status.success());
    assert_eq!(String::from_utf8_lossy(&unredacted_diff.stderr), "");
    let raw_diff_stdout = String::from_utf8(unredacted_diff.stdout).expect("utf8");
    assert!(raw_diff_stdout.contains("~ ssn: \"111-11-1111\" -> \"222-22-2222\"\n"));
    assert!(raw_diff_stdout.contains("~ tax_id: \"TAX-INIT\" -> \"TAX-NEXT\"\n"));
    assert!(raw_diff_stdout.contains("- ssn: \"555-55-5555\"\n"));

    // 7. --omit-payload (with and without explicit --redact) keeps metadata + changed column names
    //    (still respecting remove rules) while setting before/after to null.
    for omit_args in [
        &["history", "--format", "json", "--omit-payload"][..],
        &["history", "--format", "json", "--redact", "--omit-payload"][..],
    ] {
        let omit_json = auditstream_cmd()
            .current_dir(dir.path())
            .args(omit_args)
            .output()
            .expect("run --omit-payload json");
        assert!(omit_json.status.success());
        let omit_stdout = String::from_utf8(omit_json.stdout.clone()).expect("utf8");
        let omit_stderr = String::from_utf8(omit_json.stderr).expect("utf8");
        for (idx, value) in redacted_values.iter().enumerate() {
            assert!(
                !omit_stdout.contains(value),
                "omit-payload leaked redacted fixture #{idx}"
            );
        }
        assert!(
            !omit_stdout.contains("tax_id"),
            "omit-payload leaked removed col"
        );
        assert!(!omit_stdout.contains("12345678901234567890.123456789"));
        assert_eq!(
            omit_stderr,
            "redaction active (3 configured rules; payloads omitted via --omit-payload)\n"
        );
        let omitted_events = parse_json_lines(&omit_json.stdout);
        assert_eq!(omitted_events.len(), 6);
        for ev in &omitted_events {
            assert!(ev["before"].is_null());
            assert!(ev["after"].is_null());
            let errors: Vec<String> = validator
                .iter_errors(ev)
                .map(|e| format!("{} at {}", e, e.instance_path()))
                .collect();
            assert!(errors.is_empty(), "omit-payload event invalid: {errors:?}");
        }
        assert_eq!(omitted_events[4]["schema_name"], "public");
        assert_eq!(omitted_events[4]["table_name"], "invoice");
        assert_eq!(omitted_events[4]["record_id"], serde_json::json!({"id": 1}));
        assert_eq!(omitted_events[4]["actor"]["id"], "usr_1");
        assert_eq!(omitted_events[4]["actor_source"], "set_config");
        assert_eq!(
            omitted_events[4]["changed"],
            serde_json::json!(["ssn", "amount", "status"])
        );
    }

    // 8. --omit-payload --unredacted omits before/after while keeping removed cols in changed.
    let omit_unredacted = auditstream_cmd()
        .current_dir(dir.path())
        .args([
            "history",
            "--format",
            "json",
            "--omit-payload",
            "--unredacted",
        ])
        .output()
        .expect("run --omit-payload --unredacted");
    assert!(omit_unredacted.status.success());
    assert_eq!(String::from_utf8_lossy(&omit_unredacted.stderr), "");
    let omit_unredacted_events = parse_json_lines(&omit_unredacted.stdout);
    assert!(omit_unredacted_events[4]["before"].is_null());
    assert!(omit_unredacted_events[4]["after"].is_null());
    assert_eq!(
        omit_unredacted_events[4]["changed"],
        serde_json::json!(["ssn", "tax_id", "amount", "status"])
    );

    // 9. --omit-payload with no configured redaction rules omits payloads quietly.
    let plain_dir = tempdir().expect("tempdir");
    let omit_no_rules = auditstream_cmd()
        .current_dir(plain_dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["history", "--format", "json", "--omit-payload"])
        .output()
        .expect("run --omit-payload without rules");
    assert!(omit_no_rules.status.success());
    assert_eq!(String::from_utf8_lossy(&omit_no_rules.stderr), "");
    let omit_no_rules_stdout = String::from_utf8(omit_no_rules.stdout.clone()).expect("utf8");
    for (idx, value) in redacted_values.iter().enumerate() {
        assert!(
            !omit_no_rules_stdout.contains(value),
            "omit-payload without rules leaked redacted fixture #{idx}"
        );
    }
    let omit_no_rules_events = parse_json_lines(&omit_no_rules.stdout);
    assert_eq!(omit_no_rules_events.len(), 6);
    assert!(
        omit_no_rules_events
            .iter()
            .all(|ev| ev["before"].is_null() && ev["after"].is_null())
    );
    assert_eq!(
        omit_no_rules_events[4]["changed"],
        serde_json::json!(["ssn", "tax_id", "amount", "status"])
    );

    // 10. --omit-payload works cleanly with --format table and emits redaction stderr note.
    let omit_table = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--format", "table", "--omit-payload"])
        .output()
        .expect("run --omit-payload table");
    assert!(omit_table.status.success());
    let omit_table_stdout = String::from_utf8(omit_table.stdout).expect("utf8");
    assert!(omit_table_stdout.contains("ssn,amount,status"));
    assert!(!omit_table_stdout.contains("tax_id"));
    assert_eq!(
        String::from_utf8_lossy(&omit_table.stderr),
        "redaction active (3 configured rules; payloads omitted via --omit-payload)\n"
    );
}

#[tokio::test]
async fn history_redaction_fails_closed_on_non_object_payload() {
    let db = TestDb::new().await.expect("create TestDb");
    install_history_schema(&db).await;

    // Seed a valid row NEWER than the non-object row (so events[0] succeeds redaction and
    // events[1] fails under occurred_at DESC), proving fail-closed redaction never leaks
    // earlier valid rows to stdout.
    db.execute(
        r#"INSERT INTO auditstream.changes
           (occurred_at, source, schema_name, table_name, op, record_id, after, actor, actor_source, db_user, schema_version) VALUES
           ('2026-06-15T10:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', '{"id":1}'::jsonb,
            '{"id":1,"ssn":"999-99-9999"}'::jsonb, '{"type":"user","id":"usr_1"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T11:00:00Z'::timestamptz, 'app', NULL, NULL, 'batch.run', NULL,
            '["sensitive_array_element"]'::jsonb, '{"type":"service","id":"worker_1"}'::jsonb, 'set_config', 'app_owner', 1),
           ('2026-06-15T12:00:00Z'::timestamptz, 'db', 'public', 'invoice', 'INSERT', '{"id":2}'::jsonb,
            '{"id":2,"ssn":"888-88-8888","note":"newest_valid_row"}'::jsonb, '{"type":"user","id":"usr_2"}'::jsonb, 'set_config', 'app_owner', 1)"#,
    )
    .await
    .expect("seed non-object payload row");

    let dir = tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("auditstream.toml"),
        format!(
            "database_url = {:?}\n\n\
             [[export.redaction.rules]]\n\
             table = \"*\"\n\
             columns = [\"ssn\"]\n\
             mode = \"mask\"\n",
            db.url()
        ),
    )
    .expect("write auditstream.toml");

    // Fails closed with exit 5 and completely empty stdout across json, diff, and table.
    for args in [
        &["history", "--format", "json"][..],
        &["history", "--diff"][..],
        &["history", "--format", "table"][..],
    ] {
        auditstream_cmd()
            .current_dir(dir.path())
            .args(args)
            .assert()
            .code(5)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("expected a JSON object"))
            .stderr(predicate::str::contains("888-88-8888").not())
            .stderr(predicate::str::contains("999-99-9999").not())
            .stderr(predicate::str::contains("newest_valid_row").not())
            .stderr(predicate::str::contains("sensitive_array_element").not())
            .stderr(predicate::str::contains("redaction active").not());
    }

    // --log-format json emits a single ERROR record with exit_code 5.
    let json_err = auditstream_cmd()
        .current_dir(dir.path())
        .args(["--log-format", "json", "history", "--format", "json"])
        .output()
        .expect("run json-log fail-closed");
    assert_eq!(json_err.status.code(), Some(5));
    assert_eq!(json_err.stdout.as_slice(), b"");
    let stderr_text = String::from_utf8(json_err.stderr).expect("utf8");
    let lines: Vec<&str> = stderr_text.lines().collect();
    assert_eq!(lines.len(), 1, "{stderr_text}");
    let parsed: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON error");
    assert_eq!(parsed["level"], "ERROR");
    assert_eq!(parsed["exit_code"], 5);

    // --omit-payload clears the non-object payload safely without leaking array contents.
    let omit_out = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--format", "json", "--omit-payload"])
        .output()
        .expect("run --omit-payload on non-object payload");
    assert!(omit_out.status.success());
    let omit_stdout = String::from_utf8(omit_out.stdout.clone()).expect("utf8");
    assert!(!omit_stdout.contains("sensitive_array_element"));
    assert!(!omit_stdout.contains("888-88-8888"));
    assert!(!omit_stdout.contains("999-99-9999"));
    let omitted = parse_json_lines(&omit_out.stdout);
    assert_eq!(omitted.len(), 3);
    assert!(
        omitted
            .iter()
            .all(|ev| ev["before"].is_null() && ev["after"].is_null())
    );

    // --unredacted bypasses redaction cleanly.
    let unredacted_out = auditstream_cmd()
        .current_dir(dir.path())
        .args(["history", "--format", "json", "--unredacted"])
        .output()
        .expect("run --unredacted on non-object payload");
    assert!(unredacted_out.status.success());
    assert!(String::from_utf8_lossy(&unredacted_out.stdout).contains("sensitive_array_element"));
}
