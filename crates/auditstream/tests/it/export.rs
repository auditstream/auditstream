//! Public CLI contracts for resumable exports (F5).

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions};
use auditstream_testkit::TestDb;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::tempdir;

use crate::support::auditstream_cmd;

#[test]
fn export_requires_a_database_after_parsing_filters() {
    let dir = tempdir().expect("tempdir");
    auditstream_cmd()
        .current_dir(dir.path())
        .args([
            "export",
            "--from-start",
            "--since",
            "2026-01-01T00:00:00Z",
            "--until",
            "2027-01-01T00:00:00Z",
            "--actor",
            "user-1",
            "--tenant",
            "org_acme",
            "--source",
            "db",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("database"));
}

#[test]
fn export_help_matches_snapshot() {
    let mut cmd = crate::support::auditstream_std_cmd();
    cmd.args(["export", "--help"]);
    insta_cmd::assert_cmd_snapshot!(cmd);
}

#[test]
fn invalid_export_options_exit_usage_without_writing() {
    let dir = tempdir().expect("tempdir");
    for args in [
        vec!["--format", "yaml"],
        vec!["--stats-format", "yaml"],
        vec!["--batch-size", "0"],
        vec!["--since", "yesterday"],
        vec!["--until", "tomorrow"],
        vec!["--source", "invalid"],
        vec!["--since", "2026-01-02", "--until", "2026-01-02"],
        vec![
            "--since",
            "2026-01-03T00:00:00Z",
            "--until",
            "2026-01-02T00:00:00Z",
        ],
        vec!["--actor", ""],
        vec!["--tenant", ""],
    ] {
        auditstream_cmd()
            .current_dir(dir.path())
            .arg("export")
            .args(args)
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty());
    }
}

async fn export_db() -> TestDb {
    let db = TestDb::new().await.expect("create TestDb");
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    db.execute_statements(&ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![])))
        .await
        .expect("install history schema");
    db
}

async fn seed_events(db: &TestDb) {
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op, actor, actor_source, after) VALUES \
         ('2020-01-01T00:00:00Z', 'app', 'invoice.sent', '{\"type\":\"user\",\"id\":\"direct\"}', 'set_config', '{\"note\":\"comma, quote \\\" and newline\\n\"}'), \
         ('2020-01-02T00:00:00Z', 'app', 'invoice.paid', '{\"type\":\"service\",\"id\":\"worker\",\"onBehalfOf\":{\"type\":\"user\",\"id\":\"delegated\"}}', 'comment', '{}'), \
         ('2020-01-03T00:00:00Z', 'db', 'INSERT', NULL, NULL, '{\"id\":3}')",
    )
    .await
    .expect("seed audit events");
}

fn run_export(db: &TestDb, dir: &Path, args: &[&str]) -> Output {
    let output = auditstream_cmd()
        .current_dir(dir)
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .output()
        .expect("run export");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn json_events(raw: &[u8]) -> Vec<Value> {
    let schema: Value =
        serde_json::from_str(include_str!("../../../../spec/schemas/event.schema.json"))
            .expect("event schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("compile event schema");
    String::from_utf8_lossy(raw)
        .lines()
        .map(|line| {
            let event = serde_json::from_str(line).expect("complete JSON event");
            validator
                .validate(&event)
                .expect("event matches schema 0.1.0");
            event
        })
        .collect()
}

async fn export_file_until(db: &TestDb, dir: &Path, args: &[&str], count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = run_export(db, dir, args);
        assert!(
            output.stdout.is_empty(),
            "file export must reserve stdout for data"
        );
        let events = json_events(&fs::read(dir.join("events.jsonl")).expect("read output"));
        if events.len() >= count {
            assert_eq!(events.len(), count);
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "snapshot window did not release expected events"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn cursor_resumes_and_recovers_partial_output_without_duplicate_events() {
    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    let args = [
        "--cursor-file",
        "cursor.json",
        "--output",
        "events.jsonl",
        "--batch-size",
        "1",
    ];
    let first = export_file_until(&db, dir.path(), &args, 3).await;
    let checkpoint = fs::read(dir.path().join("cursor.json")).expect("cursor JSON");
    let _: Value = serde_json::from_slice(&checkpoint).expect("cursor file parses");
    run_export(&db, dir.path(), &args);
    assert_eq!(
        json_events(&fs::read(dir.path().join("events.jsonl")).unwrap()),
        first
    );

    OpenOptions::new()
        .append(true)
        .open(dir.path().join("events.jsonl"))
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    db.execute("INSERT INTO auditstream.changes (source, op) VALUES ('app', 'invoice.updated')")
        .await
        .expect("new audit event");
    let events = export_file_until(&db, dir.path(), &args, 4).await;
    assert_eq!(&events[..3], first);
    let ids: BTreeSet<_> = events
        .iter()
        .map(|e| e["event_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4, "resuming must write every event exactly once");

    OpenOptions::new()
        .append(true)
        .open(dir.path().join("events.jsonl"))
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    let mut from_start = args.to_vec();
    from_start.push("--from-start");
    run_export(&db, dir.path(), &from_start);
    let replay = export_file_until(&db, dir.path(), &args, 8).await;
    assert_eq!(&replay[4..], events);
}

#[tokio::test]
async fn filters_match_delegated_actor_and_reject_cursor_reuse_with_other_filters() {
    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op, actor) VALUES \
         ('2020-01-01T23:59:59Z', 'app', 'before.since', '{\"type\":\"user\",\"id\":\"delegated\"}'), \
         ('2020-01-03T00:00:00Z', 'app', 'at.until', '{\"type\":\"user\",\"id\":\"delegated\"}'), \
         ('2020-01-02T12:00:00Z', 'db', 'INSERT', '{\"type\":\"user\",\"id\":\"delegated\"}')",
    )
    .await
    .expect("seed excluded actor events at filter boundaries");
    let args = [
        "--cursor-file",
        "cursor.json",
        "--output",
        "events.jsonl",
        "--since",
        "2020-01-02T00:00:00Z",
        "--until",
        "2020-01-03T00:00:00Z",
        "--actor",
        "delegated",
        "--source",
        "app",
    ];
    let events = export_file_until(&db, dir.path(), &args, 1).await;
    assert_eq!(events[0]["op"], "invoice.paid");
    let saved_cursor = fs::read(dir.path().join("cursor.json")).unwrap();
    let saved_output = fs::read(dir.path().join("events.jsonl")).unwrap();
    let mut changed = args;
    changed[9] = "direct";
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(changed)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("actor"));
    assert_eq!(
        fs::read(dir.path().join("cursor.json")).unwrap(),
        saved_cursor
    );
    assert_eq!(
        fs::read(dir.path().join("events.jsonl")).unwrap(),
        saved_output
    );

    // The excluded db row still advances the saved snapshot window without output.
    db.execute("INSERT INTO auditstream.changes (source, op) VALUES ('db', 'DELETE')")
        .await
        .expect("new excluded audit event");
    let deadline = Instant::now() + Duration::from_secs(15);
    while fs::read(dir.path().join("cursor.json")).unwrap() == saved_cursor {
        run_export(&db, dir.path(), &args);
        assert!(
            Instant::now() < deadline,
            "filtered-out rows must advance the cursor"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        fs::read(dir.path().join("events.jsonl")).unwrap(),
        saved_output
    );
}

#[tokio::test]
async fn stdout_is_stateless_and_quiet_hides_only_the_summary() {
    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    let deadline = Instant::now() + Duration::from_secs(15);
    let output = loop {
        let output = run_export(&db, dir.path(), &["--format", "jsonl"]);
        if json_events(&output.stdout).len() == 3 {
            break output;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let summary = String::from_utf8_lossy(&output.stderr);
    assert_eq!(summary.lines().count(), 1);
    assert!(
        summary.starts_with("events=3 batches=1 cursor=("),
        "{summary}"
    );
    let quiet = run_export(&db, dir.path(), &["-q", "--format", "jsonl"]);
    assert_eq!(quiet.stderr.as_slice(), b"");
    assert_eq!(json_events(&quiet.stdout), json_events(&output.stdout));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

    let csv_output = run_export(&db, dir.path(), &["--format", "csv"]);
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(csv_output.stdout.as_slice());
    assert_eq!(reader.headers().unwrap().len(), 15);
    let records: Vec<_> = reader
        .records()
        .collect::<Result<_, _>>()
        .expect("strict CSV");
    let json_ids: BTreeSet<_> = json_events(&output.stdout)
        .iter()
        .map(|e| e["event_id"].as_str().unwrap().to_owned())
        .collect();
    let csv_ids: BTreeSet<_> = records.iter().map(|r| r[0].to_owned()).collect();
    assert_eq!(csv_ids, json_ids);
    let after: Value = serde_json::from_str(&records[0][8]).expect("CSV JSON value");
    assert_eq!(after["note"], "comma, quote \" and newline\n");
    fs::write(
        dir.path().join("auditstream.toml"),
        "[export.sink]\ntype = \"csv\"\npath = \"events.csv\"\n",
    )
    .expect("configure CSV sink");
    run_export(&db, dir.path(), &[]);
    run_export(&db, dir.path(), &[]);
    let mut appended = csv::Reader::from_path(dir.path().join("events.csv")).unwrap();
    let rows: Vec<_> = appended.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows.len(), 6, "appends must keep a single CSV header");
}

#[tokio::test]
async fn held_sidecar_lock_exits_io_without_modifying_files() {
    let db = export_db().await;
    let dir = tempdir().expect("tempdir");
    let lock = File::create(dir.path().join("cursor.json.lock")).expect("lock file");
    lock.try_lock().expect("hold export lock");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--cursor-file",
            "cursor.json",
            "--output",
            "events.jsonl",
        ])
        .assert()
        .code(4)
        .stdout(predicate::str::is_empty());
    assert!(!dir.path().join("cursor.json").exists());
    assert!(!dir.path().join("events.jsonl").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn full_destination_exits_io_without_advancing_cursor() {
    let db = export_db().await;
    let dir = tempdir().expect("tempdir");
    let args = ["--cursor-file", "cursor.json", "--output", "/dev/full"];
    run_export(&db, dir.path(), &args);
    let cursor = fs::read(dir.path().join("cursor.json")).expect("initial checkpoint");
    seed_events(&db).await;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .arg("export")
            .args(args)
            .output()
            .expect("export to full destination");
        if output.status.code() == Some(4) {
            break;
        }
        assert!(
            output.status.success(),
            "unexpected error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(fs::read(dir.path().join("cursor.json")).unwrap(), cursor);
}

#[tokio::test]
async fn foreign_database_and_failed_replay_preserve_cursor_and_output() {
    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    let args = ["--cursor-file", "cursor.json", "--output", "events.jsonl"];
    export_file_until(&db, dir.path(), &args, 3).await;
    let saved_cursor = fs::read(dir.path().join("cursor.json")).unwrap();
    let saved_output = fs::read(dir.path().join("events.jsonl")).unwrap();
    let other_db = export_db().await;
    seed_events(&other_db).await;
    for (url, extra, code, error) in [
        (other_db.url(), &[][..], 2, "database"),
        (
            db.url(),
            &["--schema", "missing_schema", "--from-start"][..],
            3,
            "failed to query history batch",
        ),
    ] {
        auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", url)
            .arg("export")
            .args(args)
            .args(extra)
            .assert()
            .code(code)
            .stderr(predicate::str::contains(error));
        assert_eq!(
            fs::read(dir.path().join("cursor.json")).unwrap(),
            saved_cursor
        );
        assert_eq!(
            fs::read(dir.path().join("events.jsonl")).unwrap(),
            saved_output
        );
    }
}

#[tokio::test]
async fn output_colliding_with_cursor_or_lock_exits_usage() {
    let db = export_db().await;
    let dir = tempdir().expect("tempdir");
    for output in ["cursor.json", "cursor.json.lock"] {
        auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args(["export", "--cursor-file", "cursor.json", "--output", output])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("--output"));
        assert!(!dir.path().join("cursor.json").exists());
    }
}

#[tokio::test]
async fn failed_database_query_does_not_create_output_or_cursor_and_rejects_dir_output() {
    let db = export_db().await;
    let dir = tempdir().expect("tempdir");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--schema",
            "missing_schema",
            "--cursor-file",
            "cursor.json",
            "--output",
            "fresh.jsonl",
        ])
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("failed to query history batch"));
    assert!(
        !dir.path().join("fresh.jsonl").exists(),
        "failed DB query must not create a stray empty output file"
    );
    assert!(
        !dir.path().join("cursor.json").exists(),
        "failed DB query must not create a cursor file"
    );

    let out_dir = dir.path().join("out_dir");
    fs::create_dir(&out_dir).expect("create directory");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["export", "--output", "out_dir"])
        .assert()
        .code(4)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("directory"));
}

#[tokio::test]
async fn resume_rejects_mismatched_output_format_corrupt_cursor_and_shortened_output() {
    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    let args = ["--cursor-file", "cursor.json", "--output", "events.jsonl"];
    export_file_until(&db, dir.path(), &args, 3).await;
    let saved_cursor = fs::read(dir.path().join("cursor.json")).unwrap();
    let saved_output = fs::read(dir.path().join("events.jsonl")).unwrap();

    // Mismatched --output must exit 2 (Usage) without touching existing files or creating the new one.
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--cursor-file",
            "cursor.json",
            "--output",
            "other.jsonl",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--output"))
        .stderr(predicate::str::contains("--from-start"));
    assert!(!dir.path().join("other.jsonl").exists());
    assert_eq!(
        fs::read(dir.path().join("cursor.json")).unwrap(),
        saved_cursor
    );
    assert_eq!(
        fs::read(dir.path().join("events.jsonl")).unwrap(),
        saved_output
    );

    // Mismatched --format must exit 2 (Usage).
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--cursor-file",
            "cursor.json",
            "--output",
            "events.jsonl",
            "--format",
            "csv",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--format"))
        .stderr(predicate::str::contains("--from-start"));
    assert_eq!(
        fs::read(dir.path().join("cursor.json")).unwrap(),
        saved_cursor
    );
    assert_eq!(
        fs::read(dir.path().join("events.jsonl")).unwrap(),
        saved_output
    );

    // Output shortened below acknowledged output_len must exit 4 (Io).
    fs::write(
        dir.path().join("events.jsonl"),
        &saved_output[..saved_output.len() - 1],
    )
    .expect("truncate output");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .assert()
        .code(4)
        .stderr(predicate::str::contains("shorter"));
    assert_eq!(
        fs::read(dir.path().join("cursor.json")).unwrap(),
        saved_cursor
    );
    fs::write(dir.path().join("events.jsonl"), &saved_output).expect("restore output");

    // Corrupt cursor JSON must exit 4 (Io) without modifying output.
    fs::write(dir.path().join("cursor.json"), b"{corrupt-cursor").expect("corrupt cursor");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .assert()
        .code(4);
    assert_eq!(
        fs::read(dir.path().join("events.jsonl")).unwrap(),
        saved_output
    );
}

#[tokio::test]
async fn csv_cursor_resumes_and_recovers_partial_tail_with_single_header() {
    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    let args = [
        "--format",
        "csv",
        "--cursor-file",
        "cursor.json",
        "--output",
        "events.csv",
        "--batch-size",
        "1",
    ];

    let read_csv = |path: &Path| -> Vec<csv::StringRecord> {
        let mut reader = csv::ReaderBuilder::new()
            .flexible(false)
            .from_path(path)
            .expect("open CSV");
        assert_eq!(reader.headers().expect("CSV headers").len(), 15);
        reader
            .records()
            .collect::<Result<Vec<_>, _>>()
            .expect("strict CSV rows")
    };

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        run_export(&db, dir.path(), &args);
        if fs::metadata(dir.path().join("events.csv")).is_ok_and(|m| m.len() > 0)
            && read_csv(&dir.path().join("events.csv")).len() == 3
        {
            break;
        }
        assert!(Instant::now() < deadline, "CSV export timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    OpenOptions::new()
        .append(true)
        .open(dir.path().join("events.csv"))
        .unwrap()
        .write_all(b"partial-row,\"unterminated")
        .unwrap();
    db.execute("INSERT INTO auditstream.changes (source, op) VALUES ('app', 'invoice.refunded')")
        .await
        .expect("insert 4th event");

    let deadline = Instant::now() + Duration::from_secs(15);
    let records = loop {
        run_export(&db, dir.path(), &args);
        let rows = read_csv(&dir.path().join("events.csv"));
        if rows.len() == 4 {
            break rows;
        }
        assert!(Instant::now() < deadline, "CSV resume timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let ids: BTreeSet<_> = records.iter().map(|r| r[0].to_owned()).collect();
    assert_eq!(
        ids.len(),
        4,
        "CSV recovery must yield 4 unique events under a single header"
    );
}

#[tokio::test]
async fn server_side_database_identity_handles_endpoint_aliases_and_rejects_recreated_table() {
    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    let args = ["--cursor-file", "cursor.json", "--output", "events.jsonl"];
    export_file_until(&db, dir.path(), &args, 3).await;

    let checkpoint: Value =
        serde_json::from_slice(&fs::read(dir.path().join("cursor.json")).unwrap())
            .expect("parse cursor JSON");
    let db_meta = &checkpoint["database"];
    assert!(
        db_meta.get("host").is_none() && db_meta.get("port").is_none(),
        "checkpoint database identity must not depend on client host/port: {db_meta}"
    );
    assert!(db_meta["oid"].as_u64().is_some());
    let first_table_oid = db_meta["history_table_oid"]
        .as_u64()
        .expect("history_table_oid must be recorded");

    // Resuming through an equivalent connection endpoint alias must succeed.
    let alias_url = if db.url().contains("@127.0.0.1:") {
        db.url().replace("@127.0.0.1:", "@localhost:")
    } else if db.url().contains("@localhost:") {
        db.url().replace("@localhost:", "@127.0.0.1:")
    } else if db.url().contains('?') {
        format!("{}&application_name=auditstream_alias", db.url())
    } else {
        format!("{}?application_name=auditstream_alias", db.url())
    };
    let alias_status = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", &alias_url)
        .arg("export")
        .args(args)
        .output()
        .expect("run export through endpoint alias");
    assert!(
        alias_status.status.success(),
        "endpoint alias resume failed: {}",
        String::from_utf8_lossy(&alias_status.stderr)
    );

    // Dropping and recreating the history table on the same database URL must invalidate the cursor.
    {
        let _lock = TestDb::cluster_lock().await.expect("cluster lock");
        db.execute("DROP SCHEMA auditstream CASCADE")
            .await
            .expect("drop audit schema");
        db.execute_statements(&ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![])))
            .await
            .expect("reinstall history schema");
    }
    db.execute("INSERT INTO auditstream.changes (source, op) VALUES ('app', 'post.recreate')")
        .await
        .expect("insert post-recreate event");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("database"))
        .stderr(predicate::str::contains("--from-start"));

    // Explicit --from-start replaces the saved history_table_oid cleanly.
    let mut from_start = args.to_vec();
    from_start.push("--from-start");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        run_export(&db, dir.path(), &from_start);
        let updated: Value =
            serde_json::from_slice(&fs::read(dir.path().join("cursor.json")).unwrap())
                .expect("parse updated cursor");
        if updated["database"]["history_table_oid"].as_u64() != Some(first_table_oid)
            && json_events(&fs::read(dir.path().join("events.jsonl")).unwrap()).len() >= 4
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recreated-table replay timed out"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn date_only_filters_use_utc_midnight_bounds() {
    let db = export_db().await;
    seed_events(&db).await;
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op) VALUES \
         ('2020-01-02T23:59:59Z', 'app', 'invoice.late')",
    )
    .await
    .expect("seed late-day event");

    let dir = tempdir().expect("tempdir");
    let args = [
        "--output",
        "events.jsonl",
        "--since",
        "2020-01-02",
        "--until",
        "2020-01-03",
    ];
    let events = export_file_until(&db, dir.path(), &args, 2).await;
    let ops: BTreeSet<_> = events
        .iter()
        .map(|e| e["op"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ops,
        BTreeSet::from(["invoice.paid".to_owned(), "invoice.late".to_owned()])
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn export_redaction_applies_to_jsonl_and_csv_detects_drift_and_fails_closed() {
    let db = export_db().await;
    db.execute(
        "INSERT INTO auditstream.changes \
         (occurred_at, source, schema_name, table_name, op, record_id, before, after, changed) VALUES \
         ('2020-01-01T00:00:00Z', 'db', 'public', 'invoice', 'UPDATE', \
          '{\"id\":1}'::jsonb, \
          '{\"id\":1,\"ssn\":\"111-11-1111\",\"secret_token\":\"tok_old\",\"total\":12345678901234567890.123456789}'::jsonb, \
          '{\"id\":1,\"ssn\":\"222-22-2222\",\"secret_token\":\"tok_new\",\"total\":12345678901234567890.123456789}'::jsonb, \
          ARRAY['ssn','secret_token','total']), \
         ('2020-01-02T00:00:00Z', 'app', NULL, NULL, 'invoice.exported', \
          '{\"id\":1}'::jsonb, NULL, \
          '{\"recipient_email\":\"alice@example.com\",\"format\":\"csv\"}'::jsonb, NULL)",
    )
    .await
    .expect("seed db and app events");

    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("auditstream.toml");
    fs::write(
        &config_path,
        r#"
[[export.redaction.rules]]
table = "public.invoice"
columns = ["ssn"]
mode = "mask"

[[export.redaction.rules]]
table = "invoice"
columns = ["secret_token"]
mode = "remove"

[[export.redaction.rules]]
action = "invoice.exported"
columns = ["recipient_email"]
mode = "mask"
"#,
    )
    .expect("write auditstream.toml");

    // 1. JSONL export redacts both db and app events and preserves numeric precision.
    let jsonl_args = ["--cursor-file", "cursor.json", "--output", "events.jsonl"];
    let events = export_file_until(&db, dir.path(), &jsonl_args, 2).await;
    let raw_jsonl = fs::read_to_string(dir.path().join("events.jsonl")).expect("read jsonl");
    assert!(!raw_jsonl.contains("111-11-1111"));
    assert!(!raw_jsonl.contains("222-22-2222"));
    assert!(!raw_jsonl.contains("tok_old"));
    assert!(!raw_jsonl.contains("tok_new"));
    assert!(!raw_jsonl.contains("alice@example.com"));
    assert!(raw_jsonl.contains("12345678901234567890.123456789"));

    let db_ev = events
        .iter()
        .find(|e| e["source"] == "db")
        .expect("db event");
    assert_eq!(db_ev["before"]["ssn"], "[redacted]");
    assert_eq!(db_ev["after"]["ssn"], "[redacted]");
    assert!(db_ev["after"].get("secret_token").is_none());
    assert_eq!(db_ev["changed"], serde_json::json!(["ssn", "total"]));

    let app_ev = events
        .iter()
        .find(|e| e["source"] == "app")
        .expect("app event");
    assert_eq!(app_ev["after"]["recipient_email"], "[redacted]");

    // 2. CSV export applies the exact same redaction rules.
    let csv_out = run_export(&db, dir.path(), &["--format", "csv"]);
    let csv_text = String::from_utf8(csv_out.stdout).expect("utf8 csv");
    assert!(!csv_text.contains("222-22-2222"));
    assert!(!csv_text.contains("tok_new"));
    assert!(!csv_text.contains("alice@example.com"));
    assert!(csv_text.contains("[redacted]"));

    // 3. Changing redaction policy invalidates the saved checkpoint with exit code 2.
    fs::write(
        &config_path,
        r#"
[[export.redaction.rules]]
table = "public.invoice"
columns = ["ssn"]
mode = "remove"
"#,
    )
    .expect("change redaction policy");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(jsonl_args)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("export.redaction"))
        .stderr(predicate::str::contains("--from-start"));

    // 4. Fail-closed: non-object app event array aborts non-zero without emitting unredacted PII
    // or advancing the cursor.
    let saved_cursor = fs::read(dir.path().join("cursor.json")).expect("saved cursor");
    let saved_output = fs::read(dir.path().join("events.jsonl")).expect("saved output");
    fs::write(
        &config_path,
        r#"
[[export.redaction.rules]]
table = "public.invoice"
columns = ["ssn"]
mode = "mask"

[[export.redaction.rules]]
table = "invoice"
columns = ["secret_token"]
mode = "remove"

[[export.redaction.rules]]
action = "invoice.exported"
columns = ["recipient_email"]
mode = "mask"
"#,
    )
    .expect("restore original policy");
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op, after) VALUES \
         ('2020-01-03T00:00:00Z', 'app', 'invoice.exported', \
          '[{\"recipient_email\":\"leak@example.com\"}]'::jsonb)",
    )
    .await
    .expect("seed non-object app payload");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let out = auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .arg("export")
            .args(jsonl_args)
            .output()
            .expect("run fail-closed export");
        if !out.status.success() {
            assert_eq!(out.status.code(), Some(5));
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("expected a JSON object"),
                "stderr: {stderr}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "fail-closed test timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        fs::read(dir.path().join("cursor.json")).expect("cursor unchanged"),
        saved_cursor
    );
    assert_eq!(
        fs::read(dir.path().join("events.jsonl")).expect("output unchanged"),
        saved_output
    );
}

#[tokio::test]
async fn init_preserves_export_redaction_rules_in_auditstream_toml() {
    let db = TestDb::new().await.expect("create TestDb");
    db.execute("CREATE TABLE public.invoice (id bigint PRIMARY KEY, total numeric)")
        .await
        .expect("create table");

    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("auditstream.toml");
    fs::write(
        &config_path,
        r#"# keep comments and export redaction intact
[export]
batch_size = 500

[[export.redaction.rules]]
table = "public.invoice"
columns = ["ssn"]
mode = "mask"

[[export.redaction.rules]]
action = "invoice.exported"
columns = ["recipient_email"]
mode = "remove"
"#,
    )
    .expect("seed auditstream.toml");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--table",
            "public.invoice",
            "--migrations",
            "sql",
            "--out",
            "migrations",
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&config_path).expect("read updated auditstream.toml");
    assert!(updated.contains("# keep comments and export redaction intact"));
    assert!(updated.contains("[[export.redaction.rules]]"));
    assert!(updated.contains("recipient_email"));
    assert!(updated.contains("tables = [\"public.invoice\"]"));
}

#[tokio::test]
async fn export_tenant_filter_matches_direct_delegated_advances_cursor_and_checks_resume() {
    let db = export_db().await;
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op, actor, actor_source) VALUES \
         ('2020-01-01T00:00:00Z', 'app', 'acme.direct', \
          '{\"type\":\"user\",\"id\":\"u_1\",\"tenant\":\"org_acme\"}', 'set_config'), \
         ('2020-01-02T00:00:00Z', 'app', 'acme.delegated', \
          '{\"type\":\"ai_agent\",\"id\":\"bot_1\",\"onBehalfOf\":{\"type\":\"user\",\"id\":\"u_2\",\"tenant\":\"org_acme\"}}', 'comment'), \
         ('2020-01-03T00:00:00Z', 'app', 'other.tenant', \
          '{\"type\":\"user\",\"id\":\"u_3\",\"tenant\":\"org_beta\"}', 'set_config'), \
         ('2020-01-04T00:00:00Z', 'db', 'INSERT', NULL, NULL)",
    )
    .await
    .expect("seed tenant events");

    let dir = tempdir().expect("tempdir");
    let args = [
        "--tenant",
        "org_acme",
        "--cursor-file",
        "cursor.json",
        "--output",
        "events.jsonl",
    ];
    let events = export_file_until(&db, dir.path(), &args, 2).await;
    let ops: BTreeSet<_> = events
        .iter()
        .map(|e| e["op"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ops,
        BTreeSet::from(["acme.direct".to_owned(), "acme.delegated".to_owned()])
    );

    // Filtered-out rows for another tenant advance the saved cursor past them without emitting output.
    let before_cursor: Value =
        serde_json::from_slice(&fs::read(dir.path().join("cursor.json")).unwrap())
            .expect("parse cursor before");
    let before_xact = before_cursor["cursor"]["xact_id"].as_u64().unwrap();
    db.execute(
        "INSERT INTO auditstream.changes (occurred_at, source, op, actor, actor_source) VALUES \
         ('2020-01-05T00:00:00Z', 'app', 'beta.only', \
          '{\"type\":\"user\",\"id\":\"u_4\",\"tenant\":\"org_beta\"}', 'set_config')",
    )
    .await
    .expect("seed non-matching tenant event");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        run_export(&db, dir.path(), &args);
        let updated: Value =
            serde_json::from_slice(&fs::read(dir.path().join("cursor.json")).unwrap())
                .expect("parse cursor after");
        if updated["cursor"]["xact_id"].as_u64().unwrap() > before_xact {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "filtered cursor advance timed out"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        json_events(&fs::read(dir.path().join("events.jsonl")).unwrap()).len(),
        2,
        "non-matching tenant event must not be written to output"
    );

    // Resuming with a different --tenant must fail with exit code 2 naming --tenant and --from-start.
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--tenant",
            "org_beta",
            "--cursor-file",
            "cursor.json",
            "--output",
            "events.jsonl",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--tenant"))
        .stderr(predicate::str::contains("--from-start"));
}

#[cfg(unix)]
#[tokio::test]
async fn export_refuses_symlinks_and_creates_0600_output_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let db = export_db().await;
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");

    let victim = dir.path().join("victim.txt");
    fs::write(&victim, "DO_NOT_TOUCH\n").expect("write victim");

    // 1. Stateless `--output` pointing to a symlink must fail with Exit::Io (4).
    let out_link = dir.path().join("out-link.jsonl");
    symlink(&victim, &out_link).expect("symlink output");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["export", "--output", "out-link.jsonl"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("symlink"));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "DO_NOT_TOUCH\n");

    // 2. Stateful `--cursor-file` pointing to a symlink must fail with Exit::Io (4).
    let cursor_link = dir.path().join("cursor-link.json");
    symlink(&victim, &cursor_link).expect("symlink cursor");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "export",
            "--cursor-file",
            "cursor-link.json",
            "--output",
            "events.jsonl",
        ])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("symlink"));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "DO_NOT_TOUCH\n");

    // 3. Normal export creates output file with mode 0600.
    let args = ["--cursor-file", "cursor.json", "--output", "events.jsonl"];
    export_file_until(&db, dir.path(), &args, 3).await;
    let out_mode = fs::metadata(dir.path().join("events.jsonl"))
        .expect("stat output")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(out_mode, 0o600, "events.jsonl must have mode 0600");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn export_stats_json_stall_detection_failover_anchor_and_future_cursor_guard() {
    // Setup DB first — `export_db()` acquires and releases `TestDb::cluster_lock()` internally.
    let db = export_db().await;
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    seed_events(&db).await;
    let dir = tempdir().expect("tempdir");
    let args = [
        "--cursor-file",
        "cursor.json",
        "--output",
        "events.jsonl",
        "--stats-format",
        "json",
    ];

    // 1. Export with `--stats-format json` writes valid JSON telemetry to stderr and leaves stdout empty.
    let first = export_file_until(&db, dir.path(), &args, 3).await;
    assert_eq!(first.len(), 3);

    let caught_up = run_export(&db, dir.path(), &args);
    assert_eq!(caught_up.stdout.as_slice(), b"");
    let stderr_str = String::from_utf8_lossy(&caught_up.stderr);
    let lines: Vec<&str> = stderr_str.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected 1 JSON telemetry line: {stderr_str}"
    );
    let stats: Value = serde_json::from_str(lines[0]).expect("parse export stats json");
    assert_eq!(stats["events"], 0);
    assert_eq!(stats["batches"], 0);
    assert!(stats["duration_ms"].as_u64().is_some());
    assert!(stats["cursor"]["xact_id"].as_u64().unwrap() > 0);
    assert!(
        stats["snapshot_xmin"].as_u64().unwrap() >= stats["cursor"]["xact_id"].as_u64().unwrap()
    );
    assert!(stats["oldest_unexported_age_secs"].is_null());
    assert_eq!(stats["stalled"], false);

    // Omitting `--stats-format` under `--log-format json` defaults export telemetry to JSON too.
    let json_log_export = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "--log-format",
            "json",
            "export",
            "--cursor-file",
            "cursor.json",
            "--output",
            "events.jsonl",
        ])
        .output()
        .expect("run export with --log-format json");
    assert!(json_log_export.status.success());
    assert_eq!(json_log_export.stdout.as_slice(), b"");
    let json_log_stderr = String::from_utf8_lossy(&json_log_export.stderr);
    let inherited_stats: Value = serde_json::from_str(json_log_stderr.trim())
        .expect("--log-format json must default export stats summary to JSON");
    assert_eq!(inherited_stats["events"], 0);
    assert_eq!(inherited_stats["stalled"], false);

    // `-q` suppresses JSON telemetry on stderr.
    let quiet = run_export(
        &db,
        dir.path(),
        &[
            "--cursor-file",
            "cursor.json",
            "--output",
            "events.jsonl",
            "--stats-format",
            "json",
            "-q",
        ],
    );
    assert_eq!(quiet.stderr.as_slice(), b"");

    // Verify cursor.json persisted `database.timeline_id`, `anchor`, and secret-free `sink`.
    let cursor_path = dir.path().join("cursor.json");
    let output_path = dir.path().join("events.jsonl");
    let mut cp: Value =
        serde_json::from_slice(&fs::read(&cursor_path).expect("read cursor")).expect("parse");
    let original_tl = cp["database"]["timeline_id"]
        .as_u64()
        .expect("timeline_id persisted");
    assert!(original_tl >= 1);
    assert!(cp["anchor"]["xact_id"].as_u64().unwrap() > 0);
    assert!(cp["anchor"]["id"].as_i64().unwrap() > 0);
    assert!(cp["anchor"]["event_id"].as_str().is_some());
    assert_eq!(cp["sink"]["type"], "jsonl");

    // 2. Open transaction pinning `pg_snapshot_xmin()` while a newer row commits surfaces `stalled: true`.
    let (_held_xid, tx_hold) = db.hold_snapshot_xmin().await.expect("hold snapshot_xmin");
    db.execute("INSERT INTO auditstream.changes (source, op) VALUES ('app', 'invoice.stalled')")
        .await
        .expect("commit row behind open tx");

    let stalled_out = run_export(&db, dir.path(), &args);
    let stalled_stderr = String::from_utf8_lossy(&stalled_out.stderr);
    let stalled_stats: Value =
        serde_json::from_str(stalled_stderr.trim()).expect("pure JSON telemetry on stderr");
    assert_eq!(stalled_stats["events"], 0);
    assert_eq!(stalled_stats["stalled"], true);
    assert!(
        stalled_stats["oldest_unexported_age_secs"]
            .as_f64()
            .is_some_and(|age| age >= 0.0),
        "{stalled_stats}"
    );

    // Stalled default text stats append `stalled=true oldest_unexported_age_secs=` and log a WARN.
    let stalled_text_out = run_export(
        &db,
        dir.path(),
        &["--cursor-file", "cursor.json", "--output", "events.jsonl"],
    );
    let stalled_text = String::from_utf8_lossy(&stalled_text_out.stderr);
    assert!(
        stalled_text.contains("stalled=true oldest_unexported_age_secs="),
        "{stalled_text}"
    );
    assert!(stalled_text.contains("WARN"), "{stalled_text}");

    // Stalled `--log-format json` emits structured JSON WARN followed by JSON ExportStats.
    let stalled_json_log = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "--log-format",
            "json",
            "export",
            "--cursor-file",
            "cursor.json",
            "--output",
            "events.jsonl",
        ])
        .output()
        .expect("run stalled export with --log-format json");
    assert!(stalled_json_log.status.success());
    assert_eq!(stalled_json_log.stdout.as_slice(), b"");
    let stalled_json_stderr = String::from_utf8_lossy(&stalled_json_log.stderr);
    let stalled_lines: Vec<&str> = stalled_json_stderr.lines().collect();
    assert_eq!(
        stalled_lines.len(),
        2,
        "expected JSON WARN + JSON stats lines: {stalled_json_stderr}"
    );
    let warn_record: Value = serde_json::from_str(stalled_lines[0]).expect("parse json warn");
    assert_eq!(warn_record["level"], "WARN");
    let stats_record: Value = serde_json::from_str(stalled_lines[1]).expect("parse json stats");
    assert_eq!(stats_record["stalled"], true);

    tx_hold.rollback().await.expect("rollback holder tx");

    // Drain the previously stalled row.
    let after_stall = export_file_until(&db, dir.path(), &args, 4).await;
    assert_eq!(after_stall.len(), 4);

    // 3. Upgrading a legacy checkpoint with `timeline_id` omitted persists the current `timeline_id`,
    //    while a corrupt `timeline_id: 0` fails loudly with exit code 3.
    cp = serde_json::from_slice(&fs::read(&cursor_path).expect("read cursor")).expect("parse");
    let saved_good_cp = cp.clone();
    cp["database"]
        .as_object_mut()
        .expect("database object")
        .remove("timeline_id");
    fs::write(&cursor_path, serde_json::to_vec(&cp).unwrap()).expect("write legacy cursor");
    run_export(&db, dir.path(), &args);
    let updated_cp: Value =
        serde_json::from_slice(&fs::read(&cursor_path).expect("read cursor")).expect("parse");
    assert_eq!(
        updated_cp["database"]["timeline_id"].as_u64(),
        Some(original_tl),
        "upgraded legacy checkpoint must persist the current timeline_id"
    );

    let mut zero_tl_cp = saved_good_cp.clone();
    zero_tl_cp["database"]["timeline_id"] = Value::from(0);
    fs::write(&cursor_path, serde_json::to_vec(&zero_tl_cp).unwrap()).expect("write zero tl");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("timeline_id"));

    // 4. Simulated backward timeline regression (`saved_timeline_id > current_timeline_id`,
    //    e.g. reconnecting to a fenced old primary) fails with exit 3 even with an intact anchor
    //    and leaves both cursor.json and any unacknowledged tail bytes in events.jsonl untouched.
    let unacked_tail = b"{\"unacked_tail\":true}\n";
    let original_output = fs::read(&output_path).expect("read events.jsonl");
    let mut output_with_tail = original_output.clone();
    output_with_tail.extend_from_slice(unacked_tail);
    fs::write(&output_path, &output_with_tail).expect("append unacked tail");

    let mut regressed_cp = saved_good_cp.clone();
    regressed_cp["database"]["timeline_id"] = Value::from(original_tl.saturating_add(1));
    let regressed_bytes = serde_json::to_vec(&regressed_cp).unwrap();
    fs::write(&cursor_path, &regressed_bytes).expect("write regressed timeline cursor");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("regressed"));
    assert_eq!(
        fs::read(&cursor_path).expect("cursor untouched"),
        regressed_bytes,
        "exit 3 must not overwrite cursor.json"
    );
    assert_eq!(
        fs::read(&output_path).expect("output untouched"),
        output_with_tail,
        "exit 3 resume verification failure must not truncate events.jsonl"
    );

    // 5. Simulated PITR divergence (anchor.event_id changed) fails loudly with exit code 3
    //    without truncating events.jsonl.
    let mut diverged_cp = saved_good_cp.clone();
    diverged_cp["anchor"]["event_id"] = Value::from("00000000-0000-4000-8000-ffffffffffff");
    fs::write(&cursor_path, serde_json::to_vec(&diverged_cp).unwrap()).expect("write diverged");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("boundary row anchor"));
    assert_eq!(
        fs::read(&output_path).expect("output untouched after PITR error"),
        output_with_tail
    );

    // 6. Simulated future cursor (`cursor.xact_id > snapshot_xmin`) fails loudly with exit code 3
    //    without truncating events.jsonl.
    let mut future_cp = saved_good_cp;
    future_cp["cursor"]["xact_id"] = Value::from(u64::MAX - 1);
    fs::write(&cursor_path, serde_json::to_vec(&future_cp).unwrap()).expect("write future cursor");
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .arg("export")
        .args(args)
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("snapshot_xmin"));
    assert_eq!(
        fs::read(&output_path).expect("output untouched after future cursor error"),
        output_with_tail
    );
}
