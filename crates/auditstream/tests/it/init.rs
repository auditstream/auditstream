//! Integration tests for `auditstream init` (F2).

use std::fs;
use std::path::PathBuf;

use auditstream_pg::migrations::drizzle::STATEMENT_BREAKPOINT;
use auditstream_testkit::TestDb;
use insta_cmd::assert_cmd_snapshot;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::support::{auditstream_cmd, auditstream_std_cmd};

async fn create_fixture_tables(db: &TestDb) {
    db.execute_batch(
        "CREATE TABLE public.invoice ( \
           id integer PRIMARY KEY, \
           status text NOT NULL, \
           password_hash text \
         ); \
         CREATE TABLE public.line_item ( \
           id integer PRIMARY KEY, \
           invoice_id integer NOT NULL, \
           qty integer NOT NULL \
         ); \
         CREATE TABLE public.customer ( \
           id integer PRIMARY KEY, \
           tier text NOT NULL \
         );",
    )
    .await
    .expect("create fixture tables");
}

async fn apply_migration_file(db: &TestDb, format: &str, path: &std::path::Path) {
    let sql = fs::read_to_string(path).expect("read migration sql");
    if format == "drizzle" {
        let chunks: Vec<String> = sql
            .split(STATEMENT_BREAKPOINT)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        db.execute_statements(&chunks)
            .await
            .expect("apply drizzle migration chunks");
    } else {
        db.execute_batch(&sql)
            .await
            .expect("apply prisma/sql migration");
    }
}

#[test]
fn init_help_matches_snapshot() {
    let mut cmd = auditstream_std_cmd();
    cmd.args(["init", "--help"]);
    assert_cmd_snapshot!(cmd);
}

#[tokio::test]
async fn no_table_selected_exits_2_and_lists_discovered_tables_on_stderr() {
    let db = TestDb::new().await.expect("create TestDb");
    create_fixture_tables(&db).await;
    let dir = tempdir().expect("tempdir");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["init", "--migrations", "sql"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("public.customer"))
        .stderr(predicate::str::contains("public.invoice"))
        .stderr(predicate::str::contains("public.line_item"));
}

#[tokio::test]
async fn unknown_table_and_unknown_excluded_column_exit_2_and_name_them() {
    let db = TestDb::new().await.expect("create TestDb");
    create_fixture_tables(&db).await;
    let dir = tempdir().expect("tempdir");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--migrations",
            "sql",
            "--table",
            "public.does_not_exist",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("public.does_not_exist"));

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--migrations",
            "sql",
            "--table",
            "public.invoice",
            "--exclude",
            "public.invoice.no_such_col",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("public.invoice.no_such_col"));
}

#[tokio::test]
async fn dry_run_prints_sql_to_stdout_and_writes_no_files() {
    let db = TestDb::new().await.expect("create TestDb");
    create_fixture_tables(&db).await;
    let dir = tempdir().expect("tempdir");

    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--migrations",
            "sql",
            "--table",
            "public.invoice",
            "--exclude",
            "public.invoice.password_hash",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CREATE SCHEMA IF NOT EXISTS"))
        .stdout(predicate::str::contains(r#"ON "public"."invoice""#))
        .stdout(predicate::str::contains("password_hash"));

    assert!(
        !dir.path().join("migrations").exists(),
        "dry-run must not create migration directory"
    );
    assert!(
        !dir.path().join("auditstream.toml").exists(),
        "dry-run must not create auditstream.toml"
    );
}

#[tokio::test]
async fn creates_and_updates_config_preserving_comments_and_other_keys_without_secrets() {
    let db = TestDb::new().await.expect("create TestDb");
    create_fixture_tables(&db).await;
    let dir = tempdir().expect("tempdir");

    // 1. Fresh run creates auditstream.toml with [capture] and [migrations], never database_url.
    let out = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--migrations",
            "sql",
            "--table",
            "public.invoice",
            "--exclude",
            "public.invoice.password_hash",
        ])
        .output()
        .expect("run initial init");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout_paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect();
    assert_eq!(stdout_paths.len(), 1);
    assert!(dir.path().join(&stdout_paths[0]).exists());

    let created_toml =
        fs::read_to_string(dir.path().join("auditstream.toml")).expect("read created config");
    assert!(created_toml.contains("[capture]"));
    assert!(created_toml.contains("public.invoice"));
    assert!(created_toml.contains("public.invoice.password_hash"));
    assert!(created_toml.contains("[migrations]"));
    assert!(created_toml.contains(r#"format = "sql""#));
    assert!(created_toml.contains(r#"dir = "migrations""#));
    assert!(
        !created_toml.contains("database_url"),
        "init must never write database_url into auditstream.toml"
    );

    // 2. Re-running with identical selection (using auditstream.toml) writes nothing and says "up to date".
    auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args(["init"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("up to date"));

    // 3. Seed comments, schema, and [export] into auditstream.toml, then change selection via flags.
    let annotated_toml = "\
# Team configuration comment
schema = \"auditstream\"

[capture]
# Existing capture comment
tables = [\"public.invoice\"]
exclude = [\"public.invoice.password_hash\"]

[migrations]
format = \"sql\"
dir = \"migrations\"

# Export settings must stay untouched
[export]
cursor_file = \".auditstream.cursor\"
batch_size = 500
";
    fs::write(dir.path().join("auditstream.toml"), annotated_toml).expect("write annotated toml");

    let out2 = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--table",
            "public.invoice",
            "--table",
            "public.line_item",
        ])
        .output()
        .expect("run updated init");
    assert!(
        out2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let updated_toml =
        fs::read_to_string(dir.path().join("auditstream.toml")).expect("read updated config");
    assert!(updated_toml.contains("# Team configuration comment"));
    assert!(updated_toml.contains("# Existing capture comment"));
    assert!(updated_toml.contains("# Export settings must stay untouched"));
    assert!(updated_toml.contains(r#"cursor_file = ".auditstream.cursor""#));
    assert!(updated_toml.contains("batch_size = 500"));
    assert!(updated_toml.contains("public.line_item"));
    assert!(!updated_toml.contains("database_url"));
}

#[tokio::test]
async fn per_format_generated_migrations_capture_writes_without_excluded_column() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    for (format, default_dir) in [
        ("prisma", "prisma/migrations"),
        ("drizzle", "drizzle"),
        ("sql", "migrations"),
    ] {
        let db = TestDb::new().await.expect("create TestDb");
        create_fixture_tables(&db).await;
        let dir = tempdir().expect("tempdir");

        let out = auditstream_cmd()
            .current_dir(dir.path())
            .env("AUDITSTREAM_DATABASE_URL", db.url())
            .args([
                "init",
                "--migrations",
                format,
                "--table",
                "public.invoice",
                "--exclude",
                "public.invoice.password_hash",
            ])
            .output()
            .expect("run init");
        assert!(
            out.status.success(),
            "{format}: stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| dir.path().join(l.trim()))
            .collect();
        assert!(!paths.is_empty(), "{format}: expected printed written path");
        assert!(
            paths[0].starts_with(dir.path().join(default_dir)),
            "{format}: expected path under {default_dir}, got {:?}",
            paths[0]
        );

        // Apply the generated SQL file directly on TestDb (no Prisma or Drizzle CLI).
        let sql_file = paths
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "sql"))
            .expect("generated sql path");
        apply_migration_file(&db, format, sql_file).await;

        db.execute(
            "INSERT INTO public.invoice (id, status, password_hash) VALUES (1, 'open', 'top_secret') \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzEifQ'*/",
        )
        .await
        .expect("insert invoice");

        let count = db
            .query_scalar_i64(
                "SELECT count(*) FROM auditstream.changes WHERE table_name = 'invoice'",
            )
            .await
            .expect("count captured invoice rows");
        assert_eq!(count, 1, "{format}: expected 1 captured row");

        let after_json = db
            .query_scalar_string(
                "SELECT after::text FROM auditstream.changes WHERE table_name = 'invoice' LIMIT 1",
            )
            .await
            .expect("fetch after json");
        assert!(
            after_json.contains("open"),
            "{format}: expected status in after image: {after_json}"
        );
        assert!(
            !after_json.contains("password_hash") && !after_json.contains("top_secret"),
            "{format}: excluded column must not appear in after image: {after_json}"
        );
    }
}

#[tokio::test]
async fn changed_selection_writes_new_migration_and_applying_both_captures_only_new_selection() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    create_fixture_tables(&db).await;
    let dir = tempdir().expect("tempdir");

    // Migration 1: invoice + line_item
    let out1 = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--migrations",
            "sql",
            "--table",
            "public.invoice",
            "--table",
            "public.line_item",
        ])
        .output()
        .expect("run init 1");
    assert!(
        out1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let mig1_rel = String::from_utf8_lossy(&out1.stdout).trim().to_owned();
    let mig1_path = dir.path().join(&mig1_rel);
    let mig1_bytes = fs::read(&mig1_path).expect("read mig1");

    // Migration 2: drop line_item, keep invoice, add customer
    let out2 = auditstream_cmd()
        .current_dir(dir.path())
        .env("AUDITSTREAM_DATABASE_URL", db.url())
        .args([
            "init",
            "--migrations",
            "sql",
            "--table",
            "public.invoice",
            "--table",
            "public.customer",
        ])
        .output()
        .expect("run init 2");
    assert!(
        out2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let mig2_rel = String::from_utf8_lossy(&out2.stdout).trim().to_owned();
    let mig2_path = dir.path().join(&mig2_rel);
    assert_ne!(mig1_path, mig2_path);
    assert_eq!(
        fs::read(&mig1_path).expect("re-read mig1"),
        mig1_bytes,
        "first migration file must stay byte-identical"
    );

    // Apply both migrations in order on live Postgres.
    apply_migration_file(&db, "sql", &mig1_path).await;
    apply_migration_file(&db, "sql", &mig2_path).await;

    // Write to all three tables: only invoice and customer must be captured (not removed line_item).
    db.execute("INSERT INTO public.invoice (id, status) VALUES (1, 'open')")
        .await
        .expect("insert invoice");
    db.execute("INSERT INTO public.line_item (id, invoice_id, qty) VALUES (10, 1, 2)")
        .await
        .expect("insert removed line_item");
    db.execute("INSERT INTO public.customer (id, tier) VALUES (5, 'pro')")
        .await
        .expect("insert customer");

    let captured_tables = db
        .query_strings("SELECT table_name FROM auditstream.changes ORDER BY table_name ASC")
        .await
        .expect("query captured tables");
    assert_eq!(
        captured_tables,
        vec!["customer".to_owned(), "invoice".to_owned()]
    );
}
