use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use auditstream_pg::ddl::{DEFAULT_SCHEMA, DdlOptions, quote_ident};
use auditstream_pg::migrations::drizzle::STATEMENT_BREAKPOINT;
use auditstream_pg::migrations::{
    self, DrizzleLayout, MigrationFormat, MigrationOptions, PreviewOutcome, WriteOutcome,
};
use auditstream_pg::triggers::TableTriggerSpec;
use auditstream_pg::verify;
use auditstream_testkit::TestDb;
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use uuid::Uuid;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(prefix: &str) -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("auditstream_d4_{prefix}_{pid}_{seq}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixed_time() -> OffsetDateTime {
    PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 24).expect("date"),
        Time::from_hms(12, 30, 0).expect("time"),
    )
    .assume_utc()
}

fn fixture_tables() -> Vec<TableTriggerSpec> {
    vec![
        TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["password_hash".to_owned()],
        ),
        TableTriggerSpec::new("public", "line_item", vec!["id".to_owned()], Vec::new()),
    ]
}

fn app_role_url(db: &TestDb, role: &str, password: &str) -> String {
    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    format!("{scheme}://{role}:{password}@{host_and_db}")
}

async fn apply_drizzle_chunks(conn: &mut PgConnection, sql_text: &str) {
    let mut tx = conn.begin().await.expect("begin Drizzle migration");
    execute_drizzle_chunks(&mut tx, sql_text)
        .await
        .expect("apply Drizzle migration");
    tx.commit().await.expect("commit Drizzle migration");
}

async fn execute_drizzle_chunks(conn: &mut PgConnection, sql: &str) -> Result<(), sqlx::Error> {
    for chunk in sql.split(STATEMENT_BREAKPOINT) {
        conn.execute(AssertSqlSafe(chunk)).await?;
    }
    Ok(())
}

#[tokio::test]
async fn autocommit_migrations_fail_before_ddl_even_with_configured_lock_timeout() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let dir = ScratchDir::new("autocommit_migrations");
    let drizzle_options = MigrationOptions::new(
        MigrationFormat::Drizzle,
        dir.path(),
        fixed_time(),
        DdlOptions::default(),
        vec![],
    );
    let PreviewOutcome::Sql {
        sql: drizzle_sql, ..
    } = migrations::preview(&drizzle_options).expect("preview drizzle")
    else {
        panic!("expected new drizzle migration");
    };
    let prisma_options = MigrationOptions::new(
        MigrationFormat::Prisma,
        dir.path(),
        fixed_time(),
        DdlOptions::default(),
        vec![],
    );
    let PreviewOutcome::Sql {
        sql: prisma_sql, ..
    } = migrations::preview(&prisma_options).expect("preview prisma")
    else {
        panic!("expected new prisma migration");
    };

    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    conn.execute("SET lock_timeout = 0")
        .await
        .expect("clear timeout");
    let err = execute_drizzle_chunks(&mut conn, &drizzle_sql)
        .await
        .expect_err("reject autocommit Drizzle migration when lock_timeout = 0");
    assert!(err.to_string().contains("single transaction"), "{err}");

    // Even with a nonzero role/database lock_timeout, autocommit runners (such as Neon HTTP or
    // `psql -f` on a Prisma file) must still be rejected before DDL so role self-grant cleanup
    // and DDL atomicity cannot be broken.
    conn.execute(AssertSqlSafe(format!(
        "ALTER DATABASE {} SET lock_timeout = '2s'",
        quote_ident(db.database_name())
    )))
    .await
    .expect("configure migration database");
    let mut configured = PgConnection::connect(db.url()).await.expect("reconnect");

    let drizzle_err = execute_drizzle_chunks(&mut configured, &drizzle_sql)
        .await
        .expect_err("reject autocommit Drizzle migration even with nonzero lock_timeout");
    assert!(
        drizzle_err.to_string().contains("single transaction"),
        "{drizzle_err}"
    );

    // Simulate `psql -f` executing the first two Prisma statements as separate autocommit queries.
    let prisma_stmts: Vec<&str> = prisma_sql.split(";\n\n").collect();
    assert!(
        prisma_stmts.len() >= 3,
        "expected Prisma statements separated by blank lines"
    );
    configured
        .execute(AssertSqlSafe(prisma_stmts[0]))
        .await
        .expect("first Prisma statement sets transaction marker");
    let prisma_err = configured
        .execute(AssertSqlSafe(prisma_stmts[1]))
        .await
        .expect_err("second Prisma statement must reject autocommit execution");
    assert!(
        prisma_err.to_string().contains("single transaction"),
        "{prisma_err}"
    );

    let schema_exists: bool =
        sqlx::query_scalar("SELECT to_regnamespace('auditstream') IS NOT NULL")
            .fetch_one(&mut configured)
            .await
            .expect("check schema");
    assert!(
        !schema_exists,
        "rejected autocommit migrations must leave the schema untouched"
    );

    apply_drizzle_chunks(&mut configured, &drizzle_sql).await;
    let installed: bool = sqlx::query_scalar("SELECT to_regnamespace('auditstream') IS NOT NULL")
        .fetch_one(&mut configured)
        .await
        .expect("check schema after transactional run");
    assert!(installed);
}

#[tokio::test]
async fn transactional_drizzle_run_keeps_its_lock_timeout_for_later_migrations() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let dir = ScratchDir::new("drizzle_later_migrations");
    let options = MigrationOptions::new(
        MigrationFormat::Drizzle,
        dir.path(),
        fixed_time(),
        DdlOptions::default(),
        vec![],
    );
    let PreviewOutcome::Sql { sql, .. } = migrations::preview(&options).expect("preview") else {
        panic!("expected new migration");
    };

    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    conn.execute("SET lock_timeout = 0")
        .await
        .expect("clear timeout");
    // Transactional Drizzle migrators apply every pending migration in one transaction.
    let mut run = conn.begin().await.expect("begin Drizzle run");
    execute_drizzle_chunks(&mut run, &sql)
        .await
        .expect("apply Auditstream migration");
    let next_migration_timeout: String =
        sqlx::query_scalar("SELECT pg_catalog.current_setting('lock_timeout')")
            .fetch_one(&mut *run)
            .await
            .expect("read timeout");
    assert_eq!(next_migration_timeout, "0");
    run.commit().await.expect("commit Drizzle run");
}

/// Prisma's schema engine sends each migration file as one script, then records the outcome
/// in `_prisma_migrations` on the same connection.
#[tokio::test]
async fn failed_prisma_migration_leaves_the_connection_able_to_record_the_failure() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let dir = ScratchDir::new("prisma_failure");
    let options = MigrationOptions::new(
        MigrationFormat::Prisma,
        dir.path(),
        fixed_time(),
        DdlOptions::default(),
        fixture_tables(),
    );
    let PreviewOutcome::Sql { sql, .. } = migrations::preview(&options).expect("preview") else {
        panic!("expected new migration");
    };

    let mut holder = PgConnection::connect(db.url()).await.expect("connect");
    holder
        .execute(
            "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, password_hash text); \
             CREATE TABLE public.line_item (id int PRIMARY KEY, invoice_id int, qty int);",
        )
        .await
        .expect("create tables");
    let mut busy = holder.begin().await.expect("begin");
    busy.execute("LOCK TABLE public.invoice IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("hold a conflicting lock");

    let mut prisma = PgConnection::connect(db.url()).await.expect("connect");
    prisma
        .execute("SET lock_timeout = '100ms'")
        .await
        .expect("set timeout");
    let err = prisma
        .execute(AssertSqlSafe(sql))
        .await
        .expect_err("busy table times out");
    assert_eq!(
        err.as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("55P03"),
        "expected SQLSTATE 55P03 (lock_not_available), got: {err}"
    );
    let schema_exists: bool =
        sqlx::query_scalar("SELECT to_regnamespace('auditstream') IS NOT NULL")
            .fetch_one(&mut prisma)
            .await
            .expect("Prisma records the failure on this connection next");
    assert!(
        !schema_exists,
        "failed Prisma implicit transaction must roll back earlier DDL"
    );
    busy.rollback().await.expect("release lock");
}

async fn wait_until_a_lock_on_invoice_is_awaited(db: &TestDb) {
    for _ in 0..500 {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_locks \
              WHERE NOT granted AND relation = 'public.invoice'::pg_catalog.regclass)",
        )
        .fetch_one(db.pool())
        .await
        .expect("read locks");
        if waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the refresh never waited for public.invoice");
}

#[tokio::test]
async fn refresh_in_a_later_month_does_not_deadlock_with_an_in_flight_write() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let dir = ScratchDir::new("refresh_deadlock");
    let options = MigrationOptions::new(
        MigrationFormat::Sql,
        dir.path(),
        fixed_time(),
        DdlOptions::default(),
        fixture_tables(),
    );
    let PreviewOutcome::Sql { sql, .. } = migrations::preview(&options).expect("preview") else {
        panic!("expected new migration");
    };

    let mut app = PgConnection::connect(db.url()).await.expect("connect");
    app.execute(
        "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, password_hash text); \
         CREATE TABLE public.line_item (id int PRIMARY KEY, invoice_id int, qty int);",
    )
    .await
    .expect("create tables");
    app.execute(AssertSqlSafe(sql.clone()))
        .await
        .expect("install");
    app.execute("INSERT INTO public.invoice (id, status) VALUES (1, 'open')")
        .await
        .expect("insert");
    // A refresh applied in a later month must create the partition three months ahead.
    app.execute(
        "DO $$ BEGIN EXECUTE pg_catalog.format('DROP TABLE auditstream.%I', 'changes_' || \
         pg_catalog.to_char(pg_catalog.date_trunc('month', pg_catalog.now() AT TIME ZONE 'UTC') \
         + interval '3 months', 'YYYY_MM')); END $$",
    )
    .await
    .expect("drop the newest partition");

    // A long statement on an audited table holds ROW EXCLUSIVE before its trigger writes history.
    let mut writer = app.begin().await.expect("begin writer");
    writer
        .execute("UPDATE public.invoice SET status = status WHERE false")
        .await
        .expect("lock invoice");
    let url = db.url().to_owned();
    let refresh = tokio::spawn(async move {
        let mut conn = PgConnection::connect(&url).await.expect("connect");
        conn.execute(AssertSqlSafe(sql)).await.map(|_| ())
    });
    wait_until_a_lock_on_invoice_is_awaited(&db).await;

    writer
        .execute("UPDATE public.invoice SET status = 'paid' WHERE id = 1")
        .await
        .expect("the in-flight write must not deadlock with the refresh");
    writer.commit().await.expect("commit writer");
    refresh
        .await
        .expect("join refresh")
        .expect("the refresh finishes after the writer commits");
}

/// `plain_sql` snapshot at tag `auditstream-v0.1.0`: the released engine's output for
/// [`fixture_tables`] with the `app_user` application role.
const V0_1_0_PLAIN_SQL: &str = include_str!("fixtures/v0.1.0/plain_sql.sql");

async fn assert_v0_1_0_verify_drift(pool: &sqlx::PgPool) {
    let report = verify::check(pool, &fixture_tables())
        .await
        .expect("verify before refresh");
    for object in [
        "auditstream.resolve_actor()",
        "auditstream.capture()",
        "auditstream.log_event(text, jsonb, jsonb)",
        "public.invoice.auditstream_update",
    ] {
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == verify::FindingKind::Outdated && f.object == object),
            "expected outdated {object} finding before refresh, got: {:?}",
            report.findings
        );
    }
}

#[tokio::test]
async fn upgrades_a_v0_1_0_installation_in_place() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let admin_url = db.admin_url().to_owned();
    let app_role = format!("upgrade_{}", db.database_name());
    let legacy = V0_1_0_PLAIN_SQL.replace("app_user", &app_role);
    let dir = ScratchDir::new("v0_1_0_upgrade");
    fs::write(dir.path().join("20261124123000_auditstream.sql"), &legacy)
        .expect("save v0.1.0 migration");

    let mut admin = PgConnection::connect(db.url()).await.expect("connect");
    admin
        .execute(AssertSqlSafe(format!(
            "CREATE ROLE {} NOLOGIN",
            quote_ident(&app_role)
        )))
        .await
        .expect("create role");
    admin
        .execute(
            "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, password_hash text); \
             CREATE TABLE public.line_item (id int PRIMARY KEY, invoice_id int, qty int);",
        )
        .await
        .expect("create tables");
    admin
        .execute(AssertSqlSafe(legacy))
        .await
        .expect("install v0.1.0");
    admin
        .execute(
            "INSERT INTO public.invoice (id, status) VALUES (1, 'open'); \
             ALTER TABLE public.invoice ADD COLUMN document json;",
        )
        .await
        .expect("record history and add a json column");
    let legacy_update = admin
        .execute("UPDATE public.invoice SET document = '{}' WHERE id = 1")
        .await
        .expect_err("the v0.1.0 WHEN clause cannot compare json");
    assert!(
        legacy_update.to_string().contains("equality operator"),
        "{legacy_update}"
    );
    assert_v0_1_0_verify_drift(db.pool()).await;

    let options = MigrationOptions::new(
        MigrationFormat::Sql,
        dir.path(),
        fixed_time() + time::Duration::days(1),
        DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]),
        fixture_tables(),
    );
    let WriteOutcome::Written {
        files,
        dropped_tables,
    } = migrations::write(&options).expect("write refresh")
    else {
        panic!("a v0.1.0 manifest must produce a refresh migration");
    };
    assert!(dropped_tables.is_empty(), "{dropped_tables:?}");
    let refresh = fs::read_to_string(&files[0]).expect("read refresh");
    admin
        .execute(AssertSqlSafe(refresh))
        .await
        .expect("apply refresh");

    let report = verify::check(db.pool(), &fixture_tables())
        .await
        .expect("verify");
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    admin
        .execute("UPDATE public.invoice SET document = '{}' WHERE id = 1")
        .await
        .expect("json UPDATE after the refresh");
    let ops: Vec<String> = sqlx::query_scalar("SELECT op FROM auditstream.changes ORDER BY id")
        .fetch_all(&mut admin)
        .await
        .expect("read history");
    assert_eq!(ops, ["INSERT", "UPDATE"]);
    assert!(matches!(
        migrations::write(&options).expect("re-run"),
        WriteOutcome::UpToDate { .. }
    ));

    admin.close().await.expect("close admin");
    drop(db);
    let mut cleanup = PgConnection::connect(&admin_url)
        .await
        .expect("cleanup connect");
    cleanup
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(&app_role)
        )))
        .await
        .expect("drop role");
}

#[test]
fn snapshot_prisma_migration() {
    let now = fixed_time();
    let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
    let tables = fixture_tables();

    let prisma_dir = ScratchDir::new("snap_prisma");
    let lock_path = prisma_dir.path().join("migration_lock.toml");
    let lock_content = "provider = \"postgresql\"\n";
    fs::write(&lock_path, lock_content).expect("write lock");

    let prisma_res = migrations::write(&MigrationOptions::new(
        MigrationFormat::Prisma,
        prisma_dir.path(),
        now,
        ddl,
        tables,
    ))
    .expect("write prisma");
    let WriteOutcome::Written {
        files: prisma_files,
        ..
    } = prisma_res
    else {
        panic!("expected Written");
    };
    assert_eq!(prisma_files.len(), 1);
    assert!(
        prisma_files[0].ends_with("20261124123000_auditstream/migration.sql"),
        "unexpected prisma path: {:?}",
        prisma_files[0]
    );
    assert_eq!(
        fs::read_to_string(&lock_path).expect("lock untouched"),
        lock_content
    );
    let prisma_sql = fs::read_to_string(&prisma_files[0]).expect("read prisma sql");
    insta::assert_snapshot!("prisma", prisma_sql);
}

#[test]
fn snapshot_drizzle_045_migration() {
    let now = fixed_time();
    let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
    let tables = fixture_tables();

    let drizzle45_dir = ScratchDir::new("snap_drizzle045");
    let meta_dir = drizzle45_dir.path().join("meta");
    fs::create_dir_all(&meta_dir).expect("create meta");
    fs::write(
        meta_dir.join("_journal.json"),
        r#"{
  "version": "7",
  "dialect": "postgresql",
  "entries": [
    {
      "idx": 0,
      "version": "7",
      "when": 1763900000000,
      "tag": "0000_init_tables",
      "breakpoints": true
    }
  ]
}
"#,
    )
    .expect("seed journal");

    let d45_res = migrations::write(&MigrationOptions::new(
        MigrationFormat::Drizzle,
        drizzle45_dir.path(),
        now,
        ddl,
        tables,
    ))
    .expect("write drizzle 0.45");
    let WriteOutcome::Written {
        files: d45_files, ..
    } = d45_res
    else {
        panic!("expected Written");
    };
    assert_eq!(d45_files.len(), 2);
    assert!(d45_files[0].ends_with("0001_auditstream.sql"));
    assert!(d45_files[1].ends_with("meta/_journal.json"));
    let d45_sql = fs::read_to_string(&d45_files[0]).expect("read drizzle 0.45 sql");
    let d45_journal = fs::read_to_string(&d45_files[1]).expect("read journal");
    insta::assert_snapshot!("drizzle_045_sql", d45_sql);
    insta::assert_snapshot!("drizzle_045_journal", d45_journal);
}

#[test]
fn snapshot_drizzle_1x_migration() {
    let now = fixed_time();
    let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
    let tables = fixture_tables();

    let drizzle1_dir = ScratchDir::new("snap_drizzle1x");
    let d1_res = migrations::write(&MigrationOptions::new(
        MigrationFormat::Drizzle,
        drizzle1_dir.path(),
        now,
        ddl,
        tables,
    ))
    .expect("write drizzle 1.x");
    let WriteOutcome::Written {
        files: d1_files, ..
    } = d1_res
    else {
        panic!("expected Written");
    };
    assert_eq!(d1_files.len(), 1);
    assert!(d1_files[0].ends_with("20261124123000_auditstream/migration.sql"));
    assert!(!drizzle1_dir.path().join("meta/_journal.json").exists());
    let d1_sql = fs::read_to_string(&d1_files[0]).expect("read drizzle 1.x sql");
    insta::assert_snapshot!("drizzle_1x_sql", d1_sql);
}

#[test]
fn snapshot_plain_sql_migration() {
    let now = fixed_time();
    let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
    let tables = fixture_tables();

    let sql_dir = ScratchDir::new("snap_sql");
    let sql_res = migrations::write(&MigrationOptions::new(
        MigrationFormat::Sql,
        sql_dir.path(),
        now,
        ddl,
        tables,
    ))
    .expect("write plain sql");
    let WriteOutcome::Written {
        files: sql_files, ..
    } = sql_res
    else {
        panic!("expected Written");
    };
    assert_eq!(sql_files.len(), 1);
    assert!(sql_files[0].ends_with("20261124123000_auditstream.sql"));
    let plain_sql = fs::read_to_string(&sql_files[0]).expect("read plain sql");
    insta::assert_snapshot!("plain_sql", plain_sql);
}

#[test]
fn rerunning_same_selection_is_up_to_date_and_changed_selection_adds_and_drops_without_editing_old_files()
 {
    let now = fixed_time();
    let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
    let initial_tables = fixture_tables();

    for (label, format, layout) in [
        ("prisma", MigrationFormat::Prisma, None),
        (
            "drizzle045",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V045),
        ),
        (
            "drizzle1x",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V1),
        ),
        ("sql", MigrationFormat::Sql, None),
    ] {
        let dir = ScratchDir::new(label);
        let mut opts =
            MigrationOptions::new(format, dir.path(), now, ddl.clone(), initial_tables.clone());
        if let Some(l) = layout {
            opts = opts.with_drizzle_layout(l);
        }

        let WriteOutcome::Written {
            files: first_files, ..
        } = migrations::write(&opts).expect("first write")
        else {
            panic!("{label}: expected first write");
        };
        let first_sql_bytes = fs::read(&first_files[0]).expect("read first sql");

        // Second run with identical input MUST write nothing and return UpToDate.
        let second = migrations::write(&opts).expect("second write");
        assert!(
            matches!(second, WriteOutcome::UpToDate { ref existing } if existing == &first_files[0]),
            "{label}: expected UpToDate pointing to first file, got {second:?}"
        );
        assert_eq!(
            fs::read(&first_files[0]).expect("re-read first sql"),
            first_sql_bytes,
            "{label}: existing file must remain byte-identical"
        );

        // Changed selection: drop `line_item`, keep `invoice`, add `customer`.
        let updated_tables = vec![
            initial_tables[0].clone(),
            TableTriggerSpec::new("public", "customer", vec!["id".to_owned()], Vec::new()),
        ];
        let mut changed_opts =
            MigrationOptions::new(format, dir.path(), now, ddl.clone(), updated_tables);
        if let Some(l) = layout {
            changed_opts = changed_opts.with_drizzle_layout(l);
        }

        let WriteOutcome::Written {
            files: second_files,
            dropped_tables,
        } = migrations::write(&changed_opts).expect("changed write")
        else {
            panic!("{label}: expected Written for changed selection");
        };
        assert_ne!(second_files[0], first_files[0]);
        assert_eq!(
            dropped_tables,
            vec![("public".to_owned(), "line_item".to_owned())]
        );
        // Earlier migration file stayed byte-identical!
        assert_eq!(
            fs::read(&first_files[0]).expect("first file unchanged"),
            first_sql_bytes
        );

        let second_sql = fs::read_to_string(&second_files[0]).expect("read second sql");
        assert!(
            second_sql
                .contains(r#"DROP TRIGGER IF EXISTS auditstream_insert ON "public"."line_item""#),
            "{label}: expected DROP TRIGGER for removed table"
        );
        assert!(
            second_sql.contains(r#"ON "public"."customer""#),
            "{label}: expected trigger creation for added table"
        );
    }
}

#[test]
fn reordered_or_duplicate_excluded_columns_stay_up_to_date() {
    let now = fixed_time();
    let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);

    for (label, format, layout) in [
        ("prisma", MigrationFormat::Prisma, None),
        (
            "drizzle045",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V045),
        ),
        (
            "drizzle1x",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V1),
        ),
        ("sql", MigrationFormat::Sql, None),
    ] {
        let dir = ScratchDir::new(&format!("canon_excl_{label}"));
        let initial = vec![TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec![
                "z_secret".to_owned(),
                "a_token".to_owned(),
                "z_secret".to_owned(),
            ],
        )];
        let mut opts1 = MigrationOptions::new(format, dir.path(), now, ddl.clone(), initial);
        if let Some(l) = layout {
            opts1 = opts1.with_drizzle_layout(l);
        }
        let WriteOutcome::Written { files, .. } = migrations::write(&opts1).expect("first write")
        else {
            panic!("{label}: expected Written");
        };
        let sql = fs::read_to_string(&files[0]).expect("read migration");
        assert!(
            sql.contains(r#"["a_token","z_secret"]"#),
            "{label}: expected sorted, deduplicated excluded_columns in trigger args"
        );

        let reordered = vec![TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["a_token".to_owned(), "z_secret".to_owned()],
        )];
        let mut opts2 = MigrationOptions::new(format, dir.path(), now, ddl.clone(), reordered);
        if let Some(l) = layout {
            opts2 = opts2.with_drizzle_layout(l);
        }
        let outcome = migrations::write(&opts2).expect("second write");
        assert!(
            matches!(outcome, WriteOutcome::UpToDate { ref existing } if existing == &files[0]),
            "{label}: reordered/duplicate excluded_columns must be UpToDate, got {outcome:?}"
        );
    }
}

#[test]
fn preview_renders_identical_sql_without_modifying_filesystem_and_detects_up_to_date_and_drops() {
    let now = fixed_time();
    let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
    let initial_tables = fixture_tables();

    for (label, format, layout) in [
        ("prisma", MigrationFormat::Prisma, None),
        (
            "drizzle045",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V045),
        ),
        (
            "drizzle1x",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V1),
        ),
        ("sql", MigrationFormat::Sql, None),
    ] {
        let scratch = ScratchDir::new(&format!("preview_{label}"));
        let target_dir = scratch.path().join("migrations_not_yet_created");
        assert!(!target_dir.exists());

        let mut opts = MigrationOptions::new(
            format,
            &target_dir,
            now,
            ddl.clone(),
            initial_tables.clone(),
        );
        if let Some(l) = layout {
            opts = opts.with_drizzle_layout(l);
        }

        // 1. Preview on non-existent directory produces SQL and creates NOTHING on disk.
        let PreviewOutcome::Sql {
            sql: preview_sql,
            dropped_tables,
        } = migrations::preview(&opts).expect("initial preview")
        else {
            panic!("{label}: expected PreviewOutcome::Sql");
        };
        assert_eq!(dropped_tables, Vec::<(String, String)>::new());
        assert!(
            !target_dir.exists(),
            "{label}: preview must not create target directory"
        );

        // 2. Real write emits byte-identical SQL.
        let WriteOutcome::Written { files, .. } = migrations::write(&opts).expect("write") else {
            panic!("{label}: expected Written");
        };
        let written_sql = fs::read_to_string(&files[0]).expect("read written sql");
        assert_eq!(
            preview_sql, written_sql,
            "{label}: preview SQL must match write SQL"
        );

        // 3. Re-running preview on identical selection returns UpToDate.
        let up_to_date = migrations::preview(&opts).expect("second preview");
        assert!(
            matches!(up_to_date, PreviewOutcome::UpToDate { ref existing } if existing == &files[0]),
            "{label}: expected PreviewOutcome::UpToDate, got {up_to_date:?}"
        );

        // 4. Previewing a changed selection reports dropped_tables without writing any new files.
        let entries_before: Vec<PathBuf> = fs::read_dir(&target_dir)
            .expect("read dir before")
            .map(|e| e.expect("dir entry").path())
            .collect();
        let updated_tables = vec![
            initial_tables[0].clone(),
            TableTriggerSpec::new("public", "customer", vec!["id".to_owned()], Vec::new()),
        ];
        let mut changed_opts =
            MigrationOptions::new(format, &target_dir, now, ddl.clone(), updated_tables);
        if let Some(l) = layout {
            changed_opts = changed_opts.with_drizzle_layout(l);
        }

        let PreviewOutcome::Sql {
            sql: changed_sql,
            dropped_tables: changed_drops,
        } = migrations::preview(&changed_opts).expect("changed preview")
        else {
            panic!("{label}: expected PreviewOutcome::Sql for changed selection");
        };
        assert_eq!(
            changed_drops,
            vec![("public".to_owned(), "line_item".to_owned())]
        );
        assert!(
            changed_sql
                .contains(r#"DROP TRIGGER IF EXISTS auditstream_insert ON "public"."line_item""#)
        );
        assert!(changed_sql.contains(r#"ON "public"."customer""#));

        let entries_after: Vec<PathBuf> = fs::read_dir(&target_dir)
            .expect("read dir after")
            .map(|e| e.expect("dir entry").path())
            .collect();
        assert_eq!(
            entries_after, entries_before,
            "{label}: preview must leave target directory untouched"
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn live_postgres_applies_each_format_twice_and_handles_removed_table_selection_change() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let now = fixed_time();

    for (label, format, layout) in [
        ("prisma", MigrationFormat::Prisma, None),
        (
            "drizzle045",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V045),
        ),
        (
            "drizzle1x",
            MigrationFormat::Drizzle,
            Some(DrizzleLayout::V1),
        ),
        ("sql", MigrationFormat::Sql, None),
    ] {
        let db = TestDb::new().await.expect("create TestDb");
        let admin_url = db.admin_url().to_owned();
        let app_role = format!("mig_{label}_{}", db.database_name());
        let app_pw = Uuid::new_v4().simple().to_string();

        let dir = ScratchDir::new(&format!("live_{label}"));
        let ddl = DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]);
        let tables = fixture_tables();

        let mut opts = MigrationOptions::new(format, dir.path(), now, ddl.clone(), tables.clone());
        if let Some(l) = layout {
            opts = opts.with_drizzle_layout(l);
        }
        let WriteOutcome::Written { files, .. } = migrations::write(&opts).expect("write mig 1")
        else {
            panic!("expected Written");
        };
        let mig1_sql = fs::read_to_string(&files[0]).expect("read mig 1");

        // Setup app role & tables on fresh DB.
        let mut admin = PgConnection::connect(db.url()).await.expect("connect db");
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE {} LOGIN PASSWORD '{app_pw}'",
                quote_ident(&app_role)
            )))
            .await
            .expect("create role");
        admin
            .execute(
                "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, password_hash text); \
                 CREATE TABLE public.line_item (id int PRIMARY KEY, invoice_id int, qty int);",
            )
            .await
            .expect("create tables");
        admin
            .execute(AssertSqlSafe(format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON public.invoice, public.line_item TO {}",
                quote_ident(&app_role)
            )))
            .await
            .expect("grant dml");

        admin
            .execute(
                "CREATE FUNCTION observe_lock_timeout() RETURNS event_trigger LANGUAGE plpgsql AS $$
                 BEGIN
                   PERFORM set_config('test.observed_timeout', current_setting('lock_timeout'), false);
                 END $$;
                 CREATE EVENT TRIGGER observe_lock_timeout ON ddl_command_start
                 WHEN TAG IN ('CREATE SCHEMA') EXECUTE FUNCTION observe_lock_timeout();",
            )
            .await
            .expect("record the timeout in effect during migration DDL");

        for (session_timeout, expected_during_ddl) in [("0", "5s"), ("2s", "2s")] {
            sqlx::query("SELECT set_config('lock_timeout', $1, false)")
                .bind(session_timeout)
                .execute(&mut admin)
                .await
                .expect("set migration connection timeout");
            match format {
                MigrationFormat::Prisma | MigrationFormat::Sql => {
                    admin
                        .execute(AssertSqlSafe(mig1_sql.clone()))
                        .await
                        .unwrap_or_else(|err| panic!("{label} migration failed: {err}"));
                }
                MigrationFormat::Drizzle => {
                    apply_drizzle_chunks(&mut admin, &mig1_sql).await;
                }
            }
            let (during, after): (String, String) = sqlx::query_as(
                "SELECT current_setting('test.observed_timeout'), current_setting('lock_timeout')",
            )
            .fetch_one(&mut admin)
            .await
            .expect("read observed and session lock timeouts");
            assert_eq!(during, expected_during_ddl, "{label}: DDL timeout");
            assert_eq!(after, session_timeout, "{label}: session timeout");
        }

        // Verify capture works after double application (and excluded password_hash is stripped).
        let app_url = app_role_url(&db, &app_role, &app_pw);
        let mut app_conn = PgConnection::connect(&app_url).await.expect("connect app");
        app_conn
            .execute(
                "INSERT INTO public.invoice VALUES (1, 'open', 'secret_pw') \
                 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzEifQ'*/",
            )
            .await
            .expect("insert invoice");
        app_conn
            .execute(
                "INSERT INTO public.line_item VALUES (10, 1, 2) \
                 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzEifQ'*/",
            )
            .await
            .expect("insert line_item");

        let count_before_drop: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auditstream.changes WHERE table_name IN ('invoice', 'line_item')",
        )
        .fetch_one(&mut admin)
        .await
        .expect("count history");
        assert_eq!(count_before_drop, 2, "{label}: expected 2 captured rows");

        let has_pw: bool = sqlx::query_scalar(
            "SELECT coalesce(after ? 'password_hash', false) FROM auditstream.changes WHERE table_name = 'invoice' LIMIT 1",
        )
        .fetch_one(&mut admin)
        .await
        .expect("check password_hash excluded");
        assert!(!has_pw, "{label}: password_hash must be excluded");

        // Now write a second migration removing `line_item` and apply it!
        let mut opts2 = MigrationOptions::new(
            format,
            dir.path(),
            now + time::Duration::seconds(10),
            ddl,
            vec![tables[0].clone()],
        );
        if let Some(l) = layout {
            opts2 = opts2.with_drizzle_layout(l);
        }
        let WriteOutcome::Written {
            files: files2,
            dropped_tables,
        } = migrations::write(&opts2).expect("write mig 2")
        else {
            panic!("expected Written for mig 2");
        };
        assert_eq!(
            dropped_tables,
            vec![("public".to_owned(), "line_item".to_owned())]
        );
        let mig2_sql = fs::read_to_string(&files2[0]).expect("read mig 2");
        match format {
            MigrationFormat::Prisma | MigrationFormat::Sql => {
                admin
                    .execute(AssertSqlSafe(mig2_sql))
                    .await
                    .expect("apply mig 2");
            }
            MigrationFormat::Drizzle => {
                apply_drizzle_chunks(&mut admin, &mig2_sql).await;
            }
        }

        // Write to `line_item` (must NOT be captured) and `invoice` (MUST still be captured).
        app_conn
            .execute("INSERT INTO public.line_item VALUES (11, 1, 5)")
            .await
            .expect("insert uncaptured line_item");
        app_conn
            .execute(
                "UPDATE public.invoice SET status = 'paid' WHERE id = 1 \
                 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzIifQ'*/",
            )
            .await
            .expect("update invoice");

        let line_item_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auditstream.changes WHERE table_name = 'line_item'",
        )
        .fetch_one(&mut admin)
        .await
        .expect("count line_item history");
        assert_eq!(
            line_item_rows, 1,
            "{label}: old line_item history preserved, new write not captured"
        );

        let invoice_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auditstream.changes WHERE table_name = 'invoice'",
        )
        .fetch_one(&mut admin)
        .await
        .expect("count invoice history");
        assert_eq!(invoice_rows, 2, "{label}: invoice still captured");

        app_conn.close().await.expect("close app");
        admin.close().await.expect("close admin");
        drop(db);

        let mut cleanup = PgConnection::connect(&admin_url)
            .await
            .expect("connect admin cleanup");
        let _ = cleanup
            .execute(AssertSqlSafe(format!(
                "DROP ROLE IF EXISTS {}",
                quote_ident(&app_role)
            )))
            .await;
        let _ = cleanup.close().await;
    }
}

#[test]
fn rejects_primary_key_column_in_excluded_columns() {
    let dir = ScratchDir::new("pk_exclude");
    let opts = MigrationOptions::new(
        MigrationFormat::Sql,
        dir.path(),
        fixed_time(),
        DdlOptions::default(),
        vec![TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["id".to_owned()],
        )],
    );
    let err =
        migrations::preview(&opts).expect_err("preview must reject excluding a primary key column");
    assert!(
        err.to_string().contains("primary key"),
        "unexpected error: {err}"
    );
    let err2 =
        migrations::write(&opts).expect_err("write must reject excluding a primary key column");
    assert!(
        err2.to_string().contains("primary key"),
        "unexpected error: {err2}"
    );
}

#[test]
fn drizzle_journal_rejects_path_traversal_and_invalid_tags() {
    let outside_dir = ScratchDir::new("drizzle_outside_secret");
    fs::write(
        outside_dir.path().join("outside.sql"),
        "-- auditstream:manifest {\"app_roles\":[],\"schema\":\"auditstream\",\"schema_version\":1,\"tables\":[]}\n",
    )
    .expect("seed outside sql");

    let bad_tags = [
        "../outside",
        "..\\outside",
        "..",
        ".",
        "",
        "sub/0000_init",
        "sub\\0000_init",
        "/etc/passwd",
        "C:evil",
        "0000..evil",
        "0000\nevil",
        "0000\0evil",
        "CON",
        "con ",
        "PRN",
        "AUX",
        "nul.bak",
        "COM1",
        "LPT9",
    ];

    for (idx, tag) in bad_tags.iter().enumerate() {
        // Place each migration dir directly inside `outside_dir` so `"../outside"` genuinely
        // resolves to `outside_dir/outside.sql` if traversal were ever allowed.
        let mig_dir = outside_dir.path().join(format!("mig_{idx}"));
        let meta = mig_dir.join("meta");
        fs::create_dir_all(&meta).expect("create meta");
        let journal = serde_json::json!({
            "version": "7",
            "dialect": "postgresql",
            "entries": [
                { "idx": 0, "version": "7", "when": 1_763_900_000_000_i64, "tag": tag, "breakpoints": true },
                { "idx": 1, "version": "7", "when": 1_763_900_000_001_i64, "tag": "0001_valid", "breakpoints": true }
            ]
        });
        fs::write(
            meta.join("_journal.json"),
            serde_json::to_string_pretty(&journal).expect("serialize journal"),
        )
        .expect("write journal");

        let opts = MigrationOptions::new(
            MigrationFormat::Drizzle,
            &mig_dir,
            fixed_time(),
            DdlOptions::default(),
            fixture_tables(),
        )
        .with_drizzle_layout(DrizzleLayout::V045);

        let prev_err = migrations::preview(&opts)
            .expect_err(&format!("preview must reject Drizzle tag {tag:?}"));
        assert!(
            prev_err.to_string().contains("invalid Drizzle journal tag"),
            "unexpected preview error for {tag:?}: {prev_err}"
        );
        let write_err =
            migrations::write(&opts).expect_err(&format!("write must reject Drizzle tag {tag:?}"));
        assert!(
            write_err
                .to_string()
                .contains("invalid Drizzle journal tag"),
            "unexpected write error for {tag:?}: {write_err}"
        );
    }
}

#[test]
fn drizzle_journal_rejects_malformed_entries_and_avoids_overwriting_existing_files() {
    for (label, journal_json, expected_substr) in [
        (
            "non-object entry",
            r#"{"version":"7","dialect":"postgresql","entries":[null]}"#,
            "non-object entry",
        ),
        (
            "missing tag",
            r#"{"version":"7","dialect":"postgresql","entries":[{"idx":0,"when":1}]}"#,
            "missing a string `tag`",
        ),
        (
            "non-string tag",
            r#"{"version":"7","dialect":"postgresql","entries":[{"idx":0,"tag":42}]}"#,
            "missing a string `tag`",
        ),
        (
            "idx u64::MAX overflow",
            r#"{"version":"7","dialect":"postgresql","entries":[{"idx":18446744073709551615,"when":1,"tag":"0000_init"}]}"#,
            "idx overflow",
        ),
        (
            "idx u64::MAX + 1 invalid",
            r#"{"version":"7","dialect":"postgresql","entries":[{"idx":18446744073709551616,"when":1,"tag":"0000_init"}]}"#,
            "invalid `idx`",
        ),
        (
            "idx negative invalid",
            r#"{"version":"7","dialect":"postgresql","entries":[{"idx":-1,"when":1,"tag":"0000_init"}]}"#,
            "invalid `idx`",
        ),
        (
            "when i64::MAX overflow",
            r#"{"version":"7","dialect":"postgresql","entries":[{"idx":0,"when":9223372036854775807,"tag":"0000_init"}]}"#,
            "when overflow",
        ),
        (
            "when i64::MAX + 1 invalid",
            r#"{"version":"7","dialect":"postgresql","entries":[{"idx":0,"when":9223372036854775808,"tag":"0000_init"}]}"#,
            "invalid `when`",
        ),
    ] {
        let dir = ScratchDir::new(&format!(
            "drizzle_malformed_{}",
            label.replace([' ', ':', '+'], "_")
        ));
        let meta = dir.path().join("meta");
        fs::create_dir_all(&meta).expect("create meta");
        fs::write(meta.join("_journal.json"), journal_json).expect("write malformed journal");
        let opts = MigrationOptions::new(
            MigrationFormat::Drizzle,
            dir.path(),
            fixed_time(),
            DdlOptions::default(),
            fixture_tables(),
        )
        .with_drizzle_layout(DrizzleLayout::V045);
        let err = migrations::write(&opts).expect_err(&format!("{label} must fail on write"));
        assert!(
            err.to_string().contains(expected_substr),
            "{label}: expected {expected_substr:?} in {err}"
        );
    }

    // Existing `{next_idx:04}_auditstream.sql` on disk must never be silently overwritten.
    let collision_dir = ScratchDir::new("drizzle_no_overwrite");
    let meta = collision_dir.path().join("meta");
    fs::create_dir_all(&meta).expect("create meta");
    fs::write(
        meta.join("_journal.json"),
        r#"{"version":"7","dialect":"postgresql","entries":[{"idx":0,"when":1,"tag":"0000_init"}]}"#,
    )
    .expect("write journal");
    let pre_existing = collision_dir.path().join("0001_auditstream.sql");
    fs::write(&pre_existing, "-- hand-authored sql\n").expect("seed pre-existing sql");
    let opts = MigrationOptions::new(
        MigrationFormat::Drizzle,
        collision_dir.path(),
        fixed_time(),
        DdlOptions::default(),
        fixture_tables(),
    )
    .with_drizzle_layout(DrizzleLayout::V045);
    let WriteOutcome::Written { files, .. } =
        migrations::write(&opts).expect("write avoids collision")
    else {
        panic!("expected Written");
    };
    assert!(
        files[0].ends_with("0002_auditstream.sql"),
        "expected next free index 0002, got {:?}",
        files[0]
    );
    assert_eq!(
        fs::read_to_string(&pre_existing).expect("pre-existing untouched"),
        "-- hand-authored sql\n"
    );
}

#[test]
fn drizzle_rejects_statement_breakpoint_inside_identifiers() {
    let bp_dir = ScratchDir::new("drizzle_breakpoint_injection");
    let evil_ident = "--> statement-breakpointDROP TABLE u;--> statement-breakpoint--";
    let bp_opts = MigrationOptions::new(
        MigrationFormat::Drizzle,
        bp_dir.path(),
        fixed_time(),
        DdlOptions::new(DEFAULT_SCHEMA, vec![evil_ident.to_owned()]),
        fixture_tables(),
    );
    assert!(
        migrations::preview(&bp_opts)
            .expect_err("drizzle preview must reject statement-breakpoint in identifiers")
            .to_string()
            .contains("statement-breakpoint")
    );
    assert!(
        migrations::write(&bp_opts)
            .expect_err("drizzle write must reject statement-breakpoint in identifiers")
            .to_string()
            .contains("statement-breakpoint")
    );
}

#[cfg(unix)]
#[test]
fn migrations_refuse_to_follow_symlinks_across_drizzle_sql_and_prisma() {
    use std::os::unix::fs::symlink;

    let make_opts = |format, dir: &Path| {
        MigrationOptions::new(
            format,
            dir,
            fixed_time(),
            DdlOptions::default(),
            fixture_tables(),
        )
    };
    let outside = ScratchDir::new("mig_symlink_outside");
    let outside_sql = outside.path().join("outside.sql");
    fs::write(
        &outside_sql,
        "-- auditstream:manifest {\"app_roles\":[],\"schema\":\"auditstream\",\"schema_version\":1,\"tables\":[{\"excluded_columns\":[],\"is_partitioned\":false,\"primary_key\":[\"id\"],\"schema\":\"public\",\"table\":\"victim\"}]}\n",
    )
    .expect("write outside sql");

    // 1. Drizzle 0.45: symlinked tag SQL file and dangling journal symlink are rejected.
    let drizzle_dir = ScratchDir::new("mig_symlink_drizzle");
    fs::create_dir_all(drizzle_dir.path().join("meta")).expect("create meta");
    fs::write(
        drizzle_dir.path().join("meta/_journal.json"),
        r#"{"version":"7","dialect":"postgresql","entries":[{"idx":0,"when":1,"tag":"0000_link"}]}"#,
    )
    .expect("write journal");
    symlink(&outside_sql, drizzle_dir.path().join("0000_link.sql")).expect("symlink drizzle sql");
    let drizzle_opts = make_opts(MigrationFormat::Drizzle, drizzle_dir.path())
        .with_drizzle_layout(DrizzleLayout::V045);
    assert!(
        migrations::preview(&drizzle_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    assert!(
        migrations::write(&drizzle_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );

    let journal_dir = ScratchDir::new("mig_symlink_drizzle_journal");
    fs::create_dir_all(journal_dir.path().join("meta")).expect("create meta");
    let dangling_journal = outside.path().join("should_not_create_journal.json");
    symlink(
        &dangling_journal,
        journal_dir.path().join("meta/_journal.json"),
    )
    .expect("symlink journal");
    let journal_opts = make_opts(MigrationFormat::Drizzle, journal_dir.path());
    assert!(
        migrations::write(&journal_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    assert!(!dangling_journal.exists());

    // 2. Plain SQL: dangling symlink at timestamped filename is rejected (not followed).
    let sql_dir = ScratchDir::new("mig_symlink_sql");
    let dangling_target = outside.path().join("should_not_be_created.sql");
    symlink(
        &dangling_target,
        sql_dir.path().join("20261124123000_auditstream.sql"),
    )
    .expect("symlink sql file");
    let sql_opts = make_opts(MigrationFormat::Sql, sql_dir.path());
    assert!(
        migrations::write(&sql_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    assert!(!dangling_target.exists());

    // 3. Prisma / folder layout: symlinked migration directory is rejected.
    let prisma_dir = ScratchDir::new("mig_symlink_prisma");
    let outside_folder = outside.path().join("fake_prisma_mig");
    fs::create_dir_all(&outside_folder).expect("create outside folder");
    fs::copy(&outside_sql, outside_folder.join("migration.sql")).expect("copy migration.sql");
    symlink(
        &outside_folder,
        prisma_dir.path().join("20261124120000_linked"),
    )
    .expect("symlink prisma folder");
    let prisma_opts = make_opts(MigrationFormat::Prisma, prisma_dir.path());
    assert!(
        migrations::preview(&prisma_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
}

#[cfg(unix)]
#[test]
fn migrations_refuse_symlinked_drizzle_meta_dir_and_prisma_migration_sql() {
    use std::os::unix::fs::symlink;

    let outside = ScratchDir::new("mig_symlink_nested_outside");
    let outside_meta = outside.path().join("outside_meta");
    fs::create_dir_all(&outside_meta).expect("create outside meta");
    fs::write(
        outside_meta.join("_journal.json"),
        r#"{"version":"7","dialect":"postgresql","entries":[]}"#,
    )
    .expect("write outside journal");
    let outside_sql = outside.path().join("outside.sql");
    fs::write(
        &outside_sql,
        "-- auditstream:manifest {\"app_roles\":[],\"schema\":\"auditstream\",\"schema_version\":1,\"tables\":[]}\n",
    )
    .expect("write outside sql");

    // Symlinked `meta` directory must auto-detect V045 and fail via `reject_symlink(&meta_dir)`.
    let meta_link_dir = ScratchDir::new("mig_symlink_meta_dir");
    symlink(&outside_meta, meta_link_dir.path().join("meta")).expect("symlink meta dir");
    let meta_opts = MigrationOptions::new(
        MigrationFormat::Drizzle,
        meta_link_dir.path(),
        fixed_time(),
        DdlOptions::default(),
        fixture_tables(),
    );
    assert!(
        migrations::preview(&meta_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    assert!(
        migrations::write(&meta_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );

    // Symlinked `migration.sql` inside a real Prisma folder must fail via `reject_symlink(&migration_sql)`.
    let prisma_dir = ScratchDir::new("mig_symlink_prisma_sql");
    let real_folder = prisma_dir.path().join("20261124120000_real");
    fs::create_dir_all(&real_folder).expect("create real folder");
    symlink(&outside_sql, real_folder.join("migration.sql")).expect("symlink migration.sql");
    let prisma_opts = MigrationOptions::new(
        MigrationFormat::Prisma,
        prisma_dir.path(),
        fixed_time(),
        DdlOptions::default(),
        fixture_tables(),
    );
    assert!(
        migrations::preview(&prisma_opts)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
}

#[test]
fn migrations_and_install_helpers_reject_invalid_audit_schema_names() {
    let dir = ScratchDir::new("bad_schema");
    let overlong = "a".repeat(64);
    for bad in [
        "",
        "audit$$stream",
        "audit$fn$stream",
        "audit\"stream",
        "audit; DROP TABLE x;--",
        "pg_audit",
        "information_schema",
        "INFORMATION_SCHEMA",
        overlong.as_str(),
    ] {
        let ddl = DdlOptions::new(bad, vec![]);
        assert!(
            migrations::install_statements(&ddl, &[]).is_err(),
            "install_statements must reject {bad:?}"
        );
        assert!(
            migrations::render_install_sql(&ddl, &[]).is_err(),
            "render_install_sql must reject {bad:?}"
        );
        let opts =
            MigrationOptions::new(MigrationFormat::Sql, dir.path(), fixed_time(), ddl, vec![]);
        assert!(
            migrations::preview(&opts).is_err(),
            "preview must reject {bad:?}"
        );
        assert!(
            migrations::write(&opts).is_err(),
            "write must reject {bad:?}"
        );
    }
}

#[tokio::test]
async fn install_statements_and_render_install_sql_install_clean_schema_and_reject_autocommit() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let ddl = DdlOptions::default();
    let tables = fixture_tables();
    let stmts = migrations::install_statements(&ddl, &tables).expect("compose install statements");
    let script = migrations::render_install_sql(&ddl, &tables).expect("render install sql");

    // 1. Statement-by-statement autocommit must be rejected before creating the audit schema.
    let db_stmt = TestDb::new().await.expect("create TestDb for statements");
    db_stmt
        .execute_batch(
            "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, password_hash text); \
             CREATE TABLE public.line_item (id int PRIMARY KEY, invoice_id int, qty int);",
        )
        .await
        .expect("create fixture tables");

    let mut conn = PgConnection::connect(db_stmt.url()).await.expect("connect");
    conn.execute(AssertSqlSafe(stmts[0].clone()))
        .await
        .expect("first statement sets transaction marker");
    let err = conn
        .execute(AssertSqlSafe(stmts[1].clone()))
        .await
        .expect_err("second statement must reject autocommit execution");
    assert!(err.to_string().contains("single transaction"), "{err}");
    let schema_exists: bool =
        sqlx::query_scalar("SELECT to_regnamespace('auditstream') IS NOT NULL")
            .fetch_one(&mut conn)
            .await
            .expect("check schema");
    assert!(!schema_exists);

    // 2. Transactional execution of `install_statements` yields a clean `verify::check` report.
    db_stmt
        .execute_statements(&stmts)
        .await
        .expect("apply install_statements transactionally");
    let report = verify::check(db_stmt.pool(), &tables)
        .await
        .expect("verify install_statements");
    assert!(
        report.findings.is_empty(),
        "unexpected findings after install_statements: {:?}",
        report.findings
    );

    // 3. `render_install_sql` runs directly as a single SQL script (matching `psql`) and is idempotent.
    let db_script = TestDb::new().await.expect("create TestDb for script");
    db_script
        .execute_batch(
            "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, password_hash text); \
             CREATE TABLE public.line_item (id int PRIMARY KEY, invoice_id int, qty int);",
        )
        .await
        .expect("create fixture tables");
    db_script
        .execute_batch(&script)
        .await
        .expect("apply render_install_sql");
    db_script
        .execute_batch(&script)
        .await
        .expect("re-apply render_install_sql idempotently");
    let script_report = verify::check(db_script.pool(), &tables)
        .await
        .expect("verify render_install_sql");
    assert!(
        script_report.findings.is_empty(),
        "unexpected findings after render_install_sql: {:?}",
        script_report.findings
    );

    db_script
        .execute(
            "INSERT INTO public.invoice (id, status, password_hash) VALUES (1, 'open', 'secret') \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzEifQ'*/",
        )
        .await
        .expect("insert audited invoice");
    let captured: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM auditstream.changes \
          WHERE table_name = 'invoice' AND actor_source = 'comment'",
    )
    .fetch_one(db_script.pool())
    .await
    .expect("count captured invoice rows");
    assert_eq!(captured, 1);
}
