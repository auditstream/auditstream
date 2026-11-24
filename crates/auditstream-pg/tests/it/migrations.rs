use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use auditstream_pg::ddl::{DEFAULT_SCHEMA, DdlOptions, quote_ident};
use auditstream_pg::migrations::drizzle::STATEMENT_BREAKPOINT;
use auditstream_pg::migrations::{
    self, DrizzleLayout, MigrationFormat, MigrationOptions, WriteOutcome,
};
use auditstream_pg::triggers::TableTriggerSpec;
use auditstream_testkit::TestDb;
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

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
    for chunk in sql_text.split(STATEMENT_BREAKPOINT) {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        conn.execute(AssertSqlSafe(trimmed.to_owned()))
            .await
            .unwrap_or_else(|err| panic!("drizzle chunk failed:\n{trimmed}\nerror: {err}"));
    }
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
        let app_pw = "mig_app_pw";

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

        // Apply migration TWICE as its migrator does (single script for Prisma/SQL, chunk-by-chunk for Drizzle).
        for run_idx in 0..2 {
            match format {
                MigrationFormat::Prisma | MigrationFormat::Sql => {
                    admin
                        .execute(AssertSqlSafe(mig1_sql.clone()))
                        .await
                        .unwrap_or_else(|err| panic!("{label} run {run_idx} failed: {err}"));
                }
                MigrationFormat::Drizzle => {
                    apply_drizzle_chunks(&mut admin, &mig1_sql).await;
                }
            }
        }

        // Verify capture works after double application (and excluded password_hash is stripped).
        let app_url = app_role_url(&db, &app_role, app_pw);
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
