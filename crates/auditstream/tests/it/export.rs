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
        vec!["--batch-size", "0"],
        vec!["--since", "yesterday"],
        vec!["--until", "tomorrow"],
        vec!["--source", "invalid"],
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
