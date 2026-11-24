use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
use auditstream_testkit::TestDb;
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor, Row};
use time::OffsetDateTime;
use uuid::Uuid;

async fn run_statements(conn: &mut PgConnection, stmts: &[String]) -> Result<(), sqlx::Error> {
    for stmt in stmts {
        conn.execute(AssertSqlSafe(stmt.clone())).await?;
    }
    Ok(())
}

fn app_role_url(db: &TestDb, role: &str, password: &str) -> String {
    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    format!("{scheme}://{role}:{password}@{host_and_db}")
}

#[test]
fn generate_is_deterministic_contains_no_drops_and_matches_snapshot() {
    let opts = DdlOptions::new(DEFAULT_SCHEMA, vec!["app_user".to_owned()]);
    let first = ddl::generate(&opts);
    let second = ddl::generate(&opts);
    assert_eq!(first, second);

    for stmt in &first {
        let upper = stmt.to_ascii_uppercase();
        assert!(
            !upper.contains("DROP ROLE")
                && !upper.contains("DROP SCHEMA")
                && !upper.contains("DROP TABLE")
                && !upper.contains("DROP OWNED"),
            "generated DDL must never drop objects or roles: {stmt}"
        );
    }

    insta::assert_snapshot!("ddl_default_with_app_role", first.join(";\n\n"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn setup_runs_as_non_superuser_revokes_self_grant_and_fails_without_self_grant() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    // 1. Fresh non-superuser migrator (CREATEROLE NOSUPERUSER).
    let db = TestDb::non_superuser()
        .await
        .expect("create non-superuser TestDb");
    let admin_url = db.admin_url().to_owned();
    let migrator_role = db
        .role_name()
        .expect("non_superuser has role name")
        .to_owned();

    // Ensure auditstream_owner does not pre-exist from another role in the shared cluster,
    // and create a dedicated application role for this test database.
    let app_role = format!("app_{}", db.database_name());
    let app_pw = Uuid::new_v4().simple().to_string();
    {
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin connect");
        admin
            .execute(AssertSqlSafe(format!(
                "DROP ROLE IF EXISTS {}",
                quote_ident(OWNER_ROLE)
            )))
            .await
            .expect("ensure clean owner role");
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE {} LOGIN PASSWORD '{app_pw}'",
                quote_ident(&app_role)
            )))
            .await
            .expect("create app role");
        admin.close().await.expect("close admin");
    }

    let opts = DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]);
    let stmts = ddl::generate(&opts);

    {
        let mut conn = PgConnection::connect(db.url())
            .await
            .expect("migrator connect");
        run_statements(&mut conn, &stmts)
            .await
            .expect("non-superuser setup must succeed");

        // Migrating role must have its temporary self-grant revoked and cannot write to changes.
        let has_usage: bool = sqlx::query_scalar("SELECT pg_catalog.pg_has_role($1, $2, 'USAGE')")
            .bind(&migrator_role)
            .bind(OWNER_ROLE)
            .fetch_one(&mut conn)
            .await
            .expect("check usage");
        assert!(
            !has_usage,
            "migrator should no longer hold USAGE on {OWNER_ROLE} after cleanup"
        );

        let write_err = conn
            .execute("INSERT INTO auditstream.changes (op) VALUES ('FAKE')")
            .await
            .expect_err("migrator must not be able to write to auditstream.changes after setup");
        assert!(
            write_err.to_string().contains("permission denied"),
            "unexpected error: {write_err}"
        );

        // Verify expected columns on auditstream.changes via pg_catalog (migrator no longer has
        // table privileges after self-grant revocation, so information_schema hides the table).
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT a.attname::text \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'auditstream' AND c.relname = 'changes' \
               AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
        )
        .fetch_all(&mut conn)
        .await
        .expect("query columns");
        assert_eq!(
            columns,
            vec![
                "id",
                "event_id",
                "occurred_at",
                "xact_id",
                "source",
                "schema_name",
                "table_name",
                "table_oid",
                "op",
                "record_id",
                "before",
                "after",
                "changed",
                "actor",
                "context",
                "actor_source",
                "db_user",
                "schema_version",
            ]
        );

        // Verify DEFAULT partition + current UTC month + next 3 months exist and are owned by auditstream_owner.
        let now_utc = OffsetDateTime::now_utc();
        let mut expected_parts = vec!["changes_default".to_owned()];
        let mut year = now_utc.year();
        let mut month = u8::from(now_utc.month());
        for _ in 0..4 {
            expected_parts.push(format!("changes_{year:04}_{month:02}"));
            month += 1;
            if month > 12 {
                month = 1;
                year += 1;
            }
        }
        expected_parts.sort();

        let actual_parts: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.relname::text, pg_catalog.pg_get_userbyid(c.relowner)::text \
             FROM pg_catalog.pg_inherits i \
             JOIN pg_catalog.pg_class c ON c.oid = i.inhrelid \
             JOIN pg_catalog.pg_class p ON p.oid = i.inhparent \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.relnamespace \
             WHERE n.nspname = 'auditstream' AND p.relname = 'changes' \
             ORDER BY c.relname",
        )
        .fetch_all(&mut conn)
        .await
        .expect("query partitions");
        let actual_names: Vec<String> = actual_parts.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(actual_names, expected_parts);
        for (name, owner) in &actual_parts {
            assert_eq!(
                owner, OWNER_ROLE,
                "partition {name} must be owned by {OWNER_ROLE}"
            );
        }

        // Verify named indexes exist on auditstream.changes.
        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT indexname::text FROM pg_catalog.pg_indexes \
             WHERE schemaname = 'auditstream' AND tablename = 'changes' \
             ORDER BY indexname",
        )
        .fetch_all(&mut conn)
        .await
        .expect("query indexes");
        assert!(indexes.contains(&"changes_occurred_at_brin_idx".to_owned()));
        assert!(indexes.contains(&"changes_xact_id_idx".to_owned()));
        assert!(indexes.contains(&"changes_lookup_idx".to_owned()));

        conn.close().await.expect("close migrator");
    }

    // 2. Verify app_role permissions (SELECT + EXECUTE log_event only; INSERT/UPDATE/DELETE/TRUNCATE denied on changes AND on a partition).
    // Also keep app_conn open across a second idempotent setup run to verify session continuity!
    let app_url = app_role_url(&db, &app_role, &app_pw);
    let mut app_conn = PgConnection::connect(&app_url)
        .await
        .expect("app_role connect");

    // Create a minimal resolve_actor() stub as superuser so we can exercise log_event() end to end.
    {
        let (base, _) = admin_url.rsplit_once('/').expect("admin url slash");
        let admin_db_url = format!("{base}/{}", db.database_name());
        let mut db_admin = PgConnection::connect(&admin_db_url)
            .await
            .expect("admin connect target db");
        db_admin
            .execute(
                "CREATE OR REPLACE FUNCTION auditstream.resolve_actor(\
                    OUT actor jsonb, OUT context jsonb, OUT actor_source text\
                 ) LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp \
                 AS $$ SELECT '{\"type\":\"user\",\"id\":\"usr_1\"}'::jsonb, NULL::jsonb, 'comment'::text $$",
            )
            .await
            .expect("create resolve_actor stub");
        db_admin
            .execute("ALTER FUNCTION auditstream.resolve_actor() OWNER TO auditstream_owner")
            .await
            .expect("alter resolve_actor owner");
        db_admin.close().await.expect("close db_admin");
    }

    let event_id: String = sqlx::query_scalar(
        "SELECT auditstream.log_event('invoice.exported', '{\"id\":1}'::jsonb, '{\"ok\":true}'::jsonb)::text",
    )
    .fetch_one(&mut app_conn)
    .await
    .expect("app role executes log_event");

    // Verify row recorded session_user (app_role), source='app', op='invoice.exported'.
    let row = sqlx::query(
        "SELECT source, op, db_user, actor_source FROM auditstream.changes WHERE event_id::text = $1",
    )
    .bind(&event_id)
    .fetch_one(&mut app_conn)
    .await
    .expect("app role selects from changes");
    assert_eq!(row.get::<String, _>("source"), "app");
    assert_eq!(row.get::<String, _>("op"), "invoice.exported");
    assert_eq!(row.get::<String, _>("db_user"), app_role);
    assert_eq!(
        row.get::<Option<String>, _>("actor_source").as_deref(),
        Some("comment")
    );

    // Verify invalid action raises SQLSTATE 22023.
    let bad_action = sqlx::query("SELECT auditstream.log_event('INSERT', NULL, NULL)")
        .execute(&mut app_conn)
        .await
        .expect_err("reserved action INSERT must fail");
    assert_eq!(
        bad_action
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );

    // Verify INSERT, UPDATE, DELETE, TRUNCATE denied on changes AND on changes_default partition.
    for target in ["auditstream.changes", "auditstream.changes_default"] {
        for sql in [
            format!("INSERT INTO {target} (op) VALUES ('FAKE')"),
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

    // 3. Re-run setup a second time as migrator: must succeed idempotently, preserve existing rows,
    //    and keep the already-open app_conn session working!
    {
        let mut conn = PgConnection::connect(db.url())
            .await
            .expect("migrator reconnect");
        run_statements(&mut conn, &stmts)
            .await
            .expect("second setup run must succeed idempotently");
        conn.close().await.expect("close migrator 2");
    }

    let count_after: i64 = sqlx::query_scalar("SELECT count(*) FROM auditstream.changes")
        .fetch_one(&mut app_conn)
        .await
        .expect("existing app session still selects after second setup");
    assert_eq!(
        count_after, 1,
        "existing row must be preserved across re-run"
    );

    let second_id: String =
        sqlx::query_scalar("SELECT auditstream.log_event('invoice.reprinted', NULL, NULL)::text")
            .fetch_one(&mut app_conn)
            .await
            .expect("existing app session still calls log_event after second setup");
    assert_ne!(second_id, event_id);
    app_conn.close().await.expect("close app_conn");

    // 4. Prove setup FAILS on non-superuser when self-grant is removed!
    drop(db);
    {
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin cleanup before db2");
        admin
            .execute(AssertSqlSafe(format!(
                "DROP ROLE IF EXISTS {}",
                quote_ident(OWNER_ROLE)
            )))
            .await
            .expect("drop owner role before negative test");
        admin.close().await.expect("close admin");
    }

    let db2 = TestDb::non_superuser()
        .await
        .expect("create second non-superuser TestDb");
    let without_self_grant: Vec<String> = stmts
        .iter()
        .map(|s| {
            if s.contains("GRANT \"auditstream_owner\" TO") {
                "DO $$ BEGIN PERFORM pg_catalog.pg_advisory_xact_lock(7022634597926343794); \
                 IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'auditstream_owner') THEN \
                   EXECUTE 'CREATE ROLE \"auditstream_owner\" NOLOGIN'; \
                 END IF; END $$"
                    .to_owned()
            } else {
                s.clone()
            }
        })
        .collect();

    {
        let mut conn2 = PgConnection::connect(db2.url())
            .await
            .expect("migrator2 connect");
        let err = run_statements(&mut conn2, &without_self_grant)
            .await
            .expect_err("non-superuser setup without self-grant must fail");
        assert_eq!(
            err.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("42501"),
            "expected SQLSTATE 42501 without self-grant, got: {err}"
        );
        conn2.close().await.expect("close conn2");
    }

    // Clean up cluster-wide roles created by this test.
    drop(db2);
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup connect");
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(&app_role)
        )))
        .await;
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(OWNER_ROLE)
        )))
        .await;
    let _ = admin.close().await;
}
