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
    let mut conn = db.pool().acquire().await.expect("acquire conn");
    for stmt in stmts {
        conn.execute(AssertSqlSafe(stmt))
            .await
            .expect("apply D2 DDL");
    }
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
        let mut conn = db.pool().acquire().await.expect("acquire conn");
        for stmt in stmts {
            conn.execute(AssertSqlSafe(stmt))
                .await
                .expect("apply D2 DDL as non-superuser");
        }
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
