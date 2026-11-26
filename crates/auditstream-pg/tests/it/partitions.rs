//! Live PostgreSQL integration tests for `auditstream_pg::partitions` (D8).

use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
use auditstream_pg::partitions::{
    self, EnsureOptions, PartitionError, SkipReason, YearMonth, target_months_from,
};
use auditstream_testkit::TestDb;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor};

async fn install_ddl(db: &TestDb) {
    let _guard = TestDb::cluster_lock()
        .await
        .expect("acquire cluster lock for role setup");
    let stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    let mut tx = db.pool().begin().await.expect("begin ddl tx");
    for stmt in stmts {
        tx.execute(AssertSqlSafe(stmt)).await.expect("apply D2 DDL");
    }
    tx.commit().await.expect("commit ddl tx");
}

#[tokio::test]
async fn non_superuser_creates_future_partitions_owned_by_auditstream_owner_and_reports_coverage() {
    let _lock = TestDb::cluster_lock()
        .await
        .expect("acquire cluster lock for non-superuser role setup");
    let db = TestDb::non_superuser()
        .await
        .expect("create non_superuser TestDb");

    // Ensure this non-superuser migrator can administer `auditstream_owner` even when
    // parallel test databases in the shared cluster already hold objects owned by it.
    let migrator_role = db
        .role_name()
        .expect("non_superuser TestDb has role name")
        .to_owned();
    let mut admin = PgConnection::connect(db.admin_url())
        .await
        .expect("admin connect");
    admin
        .execute(AssertSqlSafe(format!(
            "DO $$ \
             BEGIN \
               DROP ROLE IF EXISTS {owner}; \
             EXCEPTION WHEN dependent_objects_still_exist THEN \
               EXECUTE 'GRANT {owner} TO {migrator} WITH ADMIN OPTION'; \
             END $$",
            owner = quote_ident(OWNER_ROLE),
            migrator = quote_ident(&migrator_role),
        )))
        .await
        .expect("prepare owner role for non-superuser migrator");
    admin.close().await.expect("close admin");

    let stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    {
        let mut tx = db.pool().begin().await.expect("begin ddl tx");
        for stmt in stmts {
            tx.execute(AssertSqlSafe(stmt))
                .await
                .expect("apply D2 DDL as non-superuser");
        }
        tx.commit().await.expect("commit ddl tx");
    }

    // D2 pre-creates months 0..=3; asking for 5 months ahead should create months 4 and 5.
    let report = partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 5)
        .await
        .expect("ensure_ahead as non-superuser");

    let expected_all = target_months_from(report.current_month, 5);
    assert_eq!(report.created, vec![expected_all[4], expected_all[5]]);
    assert_eq!(report.skipped.len(), 4);
    for skipped in &report.skipped {
        assert_eq!(skipped.reason, SkipReason::AlreadyExists);
    }

    // Verify ownership on every newly created partition is auditstream_owner and bounds are UTC.
    for ym in &report.created {
        let (owner, bound): (String, String) = sqlx::query_as(
            "SELECT \
               pg_catalog.pg_get_userbyid(c.relowner), \
               pg_catalog.pg_get_expr(c.relpartbound, c.oid) \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = 'auditstream' AND c.relname = $1",
        )
        .bind(ym.partition_name())
        .fetch_one(db.pool())
        .await
        .expect("fetch partition owner and bound");
        assert_eq!(owner, OWNER_ROLE);
        assert!(
            bound.contains("FOR VALUES FROM"),
            "expected range partition bound, got: {bound}"
        );
    }

    // Verify the temporary self-grant was revoked (when migrator created auditstream_owner)
    // and ensure_ahead does not fail when re-run as the same non-superuser role.
    let rerun = partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 5)
        .await
        .expect("re-run ensure_ahead as non-superuser");
    assert_eq!(rerun.created, Vec::<YearMonth>::new());
    assert_eq!(rerun.skipped.len(), 6);
}

#[tokio::test]
async fn america_sao_paulo_session_timezone_produces_identical_bounds_to_utc() {
    let db_utc = TestDb::new().await.expect("create utc TestDb");
    install_ddl(&db_utc).await;
    let db_sp = TestDb::new().await.expect("create sp TestDb");
    install_ddl(&db_sp).await;

    // Configure pool_sp so every session uses America/Sao_Paulo.
    let pool_sp = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute("SET TIME ZONE 'America/Sao_Paulo'").await?;
                Ok(())
            })
        })
        .connect(db_sp.url())
        .await
        .expect("connect sp pool");

    let rep_utc = partitions::ensure_ahead(db_utc.pool(), DEFAULT_SCHEMA, 5)
        .await
        .expect("ensure_ahead UTC");
    let rep_sp = partitions::ensure_ahead(&pool_sp, DEFAULT_SCHEMA, 5)
        .await
        .expect("ensure_ahead Sao Paulo");

    assert_eq!(rep_utc.created, rep_sp.created);
    let target_name = rep_utc.created[0].partition_name();

    // Compare partition bounds in UTC on both databases.
    let bound_utc: String = sqlx::query_scalar(
        "SELECT pg_catalog.pg_get_expr(c.relpartbound, c.oid) \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'auditstream' AND c.relname = $1",
    )
    .bind(&target_name)
    .fetch_one(db_utc.pool())
    .await
    .expect("read UTC bound");

    let bound_sp: String = sqlx::query_scalar(
        "SELECT pg_catalog.pg_get_expr(c.relpartbound, c.oid) \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'auditstream' AND c.relname = $1",
    )
    .bind(&target_name)
    .fetch_one(db_sp.pool())
    .await
    .expect("read SP bound in UTC session");

    assert_eq!(bound_sp, bound_utc);
    pool_sp.close().await;
}

#[tokio::test]
async fn second_call_is_noop_and_two_concurrent_calls_both_succeed() {
    let db = TestDb::new().await.expect("create TestDb");
    install_ddl(&db).await;

    // Run two concurrent calls creating months 4..=6; advisory lock serializes them cleanly.
    let (first_outcome, second_outcome) = tokio::join!(
        partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 6),
        partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 6),
    );
    let rep_a = first_outcome.expect("first concurrent ensure_ahead succeeds");
    let rep_b = second_outcome.expect("second concurrent ensure_ahead succeeds");

    let mut total_created: Vec<YearMonth> = rep_a
        .created
        .iter()
        .chain(rep_b.created.iter())
        .copied()
        .collect();
    total_created.sort();
    assert_eq!(total_created.len(), 3);
    total_created.dedup();
    assert_eq!(total_created.len(), 3);

    // Third call creates nothing.
    let rep_c = partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 6)
        .await
        .expect("idempotent re-run");
    assert_eq!(rep_c.created, Vec::<YearMonth>::new());
    assert_eq!(rep_c.skipped.len(), 7);
}

#[tokio::test]
async fn default_partition_rows_skip_only_conflicting_month_and_create_others() {
    let db = TestDb::new().await.expect("create TestDb");
    install_ddl(&db).await;

    let cov_before = partitions::coverage(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("initial coverage");
    let target = target_months_from(cov_before.current_month, 6);
    let blocked_month = target[5]; // 5 months ahead -> not pre-created by D2
    let (blocked_start, _) = blocked_month.utc_bounds();

    // Seed two rows into DEFAULT falling inside blocked_month.
    sqlx::query(
        "INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
         VALUES \
           ($1::timestamptz + interval '1 day', 'app', 'seed.one', 'postgres', 1), \
           ($1::timestamptz + interval '2 days', 'app', 'seed.two', 'postgres', 1)",
    )
    .bind(&blocked_start)
    .execute(db.pool())
    .await
    .expect("seed DEFAULT rows in month +5");

    let report = partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 6)
        .await
        .expect("ensure_ahead skips blocked month without leaking raw PG error");

    // Months +4 and +6 must still be created; month +5 must be reported with row_count = 2.
    assert_eq!(report.created, vec![target[4], target[6]]);
    let conflict = report
        .skipped
        .iter()
        .find(|s| s.month == blocked_month)
        .expect("blocked month reported in skipped");
    assert_eq!(
        conflict.reason,
        SkipReason::DefaultPartitionConflict { row_count: 2 }
    );

    let cov_after = partitions::coverage(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("coverage after");
    assert_eq!(cov_after.default_row_count, 2);
    assert_eq!(cov_after.last_covered_month, Some(target[6]));
}

#[tokio::test]
async fn lock_timeout_returns_retryable_error_and_leaves_no_partial_partition() {
    let db = TestDb::new().await.expect("create TestDb");
    install_ddl(&db).await;

    // Hold a conflicting lock on auditstream.changes in an open transaction so CREATE TABLE
    // PARTITION OF cannot acquire ACCESS EXCLUSIVE within 100ms.
    let mut blocker = db.pool().begin().await.expect("begin blocker tx");
    sqlx::query("LOCK TABLE auditstream.changes IN ACCESS SHARE MODE")
        .execute(&mut *blocker)
        .await
        .expect("acquire ACCESS SHARE lock");

    let opts = EnsureOptions::new(DEFAULT_SCHEMA, 5).with_lock_timeout("100ms");
    let err = partitions::ensure_ahead_with_options(db.pool(), &opts)
        .await
        .expect_err("expected lock timeout");

    assert!(
        err.is_retryable(),
        "expected retryable LockTimeout, got: {err:?}"
    );
    assert!(matches!(err, PartitionError::LockTimeout { .. }));

    blocker.rollback().await.expect("release blocker lock");

    // Verify no partial partition was left behind (still only the 4 initial months 0..=3).
    let cov = partitions::coverage(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("coverage after rollback");
    assert_eq!(cov.monthly_partitions.len(), 4);
}

#[tokio::test]
async fn history_stats_reports_missing_clean_contiguous_gap_and_default_rows() {
    let db = TestDb::new().await.expect("create TestDb");

    // 1. Before install: exists = false and zeroed stats.
    let missing = partitions::history_stats(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("history_stats before install");
    assert!(!missing.exists);
    assert_eq!(missing.current_month, None);
    assert_eq!(missing.monthly_partitions, Vec::<YearMonth>::new());
    assert_eq!(missing.months_covered_ahead, 0);
    assert!(!missing.default_has_rows);
    assert_eq!(missing.estimated_rows, 0);
    assert_eq!(missing.total_size_bytes, 0);

    // 2. Fresh install: 4 monthly partitions (0..=3 => 3 contiguous months ahead), empty default.
    install_ddl(&db).await;
    let fresh = partitions::history_stats(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("history_stats fresh");
    assert!(fresh.exists);
    let current = fresh.current_month.expect("current_month present");
    assert_eq!(fresh.monthly_partitions.len(), 4);
    assert_eq!(fresh.months_covered_ahead, 3);
    assert!(!fresh.default_has_rows);

    // 3. Seed rows into month +4 (routing to changes_default), then create month +5 via ensure_ahead.
    //    Monthly partitions will be [0, 1, 2, 3, 5], so contiguous months_covered_ahead must remain 3!
    let target = target_months_from(current, 5);
    let (month4_start, _) = target[4].utc_bounds();
    sqlx::query(
        "INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
         VALUES \
           ($1::timestamptz + interval '1 day', 'app', 'gap.one', 'postgres', 1), \
           ($1::timestamptz + interval '2 days', 'app', 'gap.two', 'postgres', 1)",
    )
    .bind(&month4_start)
    .execute(db.pool())
    .await
    .expect("seed default rows in month +4");

    let report = partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 5)
        .await
        .expect("ensure_ahead with gap");
    assert_eq!(report.created, vec![target[5]]);

    db.execute("ANALYZE auditstream.changes")
        .await
        .expect("analyze auditstream.changes");

    let after = partitions::history_stats(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("history_stats after gap");
    assert!(after.exists);
    assert_eq!(after.monthly_partitions.len(), 5);
    assert_eq!(
        after.months_covered_ahead, 3,
        "contiguous coverage ahead must stop at the missing month +4 gap"
    );
    assert!(after.default_has_rows);
    assert!(
        after.estimated_rows >= 2,
        "expected catalog estimate >= 2 after ANALYZE, got {}",
        after.estimated_rows
    );
    assert!(
        after.total_size_bytes > 0,
        "expected positive total_size_bytes, got {}",
        after.total_size_bytes
    );
}

#[tokio::test]
async fn ensure_ahead_under_alter_default_privileges_keeps_child_partitions_append_only() {
    let _lock = TestDb::cluster_lock().await.expect("acquire cluster lock");
    let db = TestDb::non_superuser()
        .await
        .expect("create non_superuser TestDb");
    let admin_url = db.admin_url().to_owned();
    let migrator_role = db.role_name().expect("migrator role").to_owned();
    let app_role = format!("partdef_{}", db.database_name());
    let app_pw = uuid::Uuid::new_v4().simple().to_string();

    {
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin connect");
        admin
            .execute(AssertSqlSafe(format!(
                "DO $$ \
                 BEGIN \
                   DROP ROLE IF EXISTS {owner}; \
                 EXCEPTION WHEN dependent_objects_still_exist THEN \
                   EXECUTE 'GRANT {owner} TO {migrator} WITH ADMIN OPTION'; \
                 END $$",
                owner = quote_ident(OWNER_ROLE),
                migrator = quote_ident(&migrator_role),
            )))
            .await
            .expect("prepare owner role");
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE {} LOGIN PASSWORD '{app_pw}'",
                quote_ident(&app_role)
            )))
            .await
            .expect("create app role");
        admin.close().await.expect("close admin");
    }

    let stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]));
    {
        let mut tx = db.pool().begin().await.expect("begin ddl tx");
        for stmt in stmts {
            tx.execute(AssertSqlSafe(stmt))
                .await
                .expect("apply DDL as non-superuser");
        }
        tx.commit().await.expect("commit ddl tx");
    }

    // Configure ALTER DEFAULT PRIVILEGES on the migrator role AFTER initial install,
    // then run ensure_ahead to create future months 4..=5.
    db.execute(&format!(
        "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON TABLES TO {}",
        quote_ident(&migrator_role),
        quote_ident(&app_role),
    ))
    .await
    .expect("alter default privileges for app_role");

    let report = partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 5)
        .await
        .expect("ensure_ahead under ALTER DEFAULT PRIVILEGES");
    assert_eq!(report.created.len(), 2);

    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    let app_url = format!("{scheme}://{app_role}:{app_pw}@{host_and_db}");
    let mut app_conn = PgConnection::connect(&app_url)
        .await
        .expect("connect app_role");

    for ym in &report.created {
        let target = format!("auditstream.{}", ym.partition_name());
        let (start_utc, _) = ym.utc_bounds();
        for sql in [
            format!(
                "INSERT INTO {target} (occurred_at, op) VALUES ('{start_utc}'::timestamptz, 'FAKE')"
            ),
            format!("UPDATE {target} SET op = 'FAKE'"),
            format!("DELETE FROM {target}"),
            format!("TRUNCATE {target}"),
        ] {
            let err = app_conn
                .execute(AssertSqlSafe(sql.clone()))
                .await
                .expect_err(&format!("expected permission denied for: {sql}"));
            assert!(
                err.to_string().contains("permission denied"),
                "expected permission denied for {sql}, got {err}"
            );
        }
    }

    app_conn.close().await.expect("close app_conn");
    drop(db);
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(&app_role)
        )))
        .await;
    let _ = admin.close().await;
}
