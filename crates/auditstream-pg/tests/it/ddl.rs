use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
use auditstream_pg::triggers::{self, TriggerOptions};
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

async fn run_statements_tx(conn: &mut PgConnection, stmts: &[String]) -> Result<(), sqlx::Error> {
    let mut tx = conn.begin().await?;
    run_statements(&mut tx, stmts).await?;
    tx.commit().await
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

#[test]
fn ddl_lookup_index_uses_adr0008_partial_hash() {
    let stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    let expected = "CREATE INDEX IF NOT EXISTS changes_lookup_idx ON \"auditstream\".changes USING hash (record_id) WHERE record_id IS NOT NULL";
    assert!(
        stmts.iter().any(|s| s.contains(expected)),
        "expected ADR-0008 partial hash lookup index statement:\n  {expected}\nfound:\n{}",
        stmts
            .iter()
            .filter(|s| s.contains("changes_lookup_idx"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[tokio::test]
async fn reapplying_ddl_skips_redundant_owner_changes() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("test database");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    let schema = r#"Audit "History's" $$v1$$"#;
    let mut statements = ddl::generate(&DdlOptions::new(schema, vec![]));
    statements.extend(triggers::generate(&TriggerOptions::new(schema, vec![])));
    run_statements_tx(&mut conn, &statements)
        .await
        .expect("setup");

    conn.execute(
        "CREATE FUNCTION reject_redundant_alter() RETURNS event_trigger LANGUAGE plpgsql
         AS $$ BEGIN RAISE EXCEPTION 'unexpected % during refresh', tg_tag; END $$;
         CREATE EVENT TRIGGER reject_redundant_alter ON ddl_command_start
         WHEN TAG IN ('ALTER TABLE', 'ALTER FUNCTION') EXECUTE FUNCTION reject_redundant_alter()",
    )
    .await
    .expect("watch for redundant ALTERs");

    run_statements_tx(&mut conn, &statements)
        .await
        .expect("matching owners should be left in place");
}

#[tokio::test]
async fn reapplying_ddl_does_not_block_history_writes() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("test database");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    let mut statements = ddl::generate(&DdlOptions::default());
    statements.extend(triggers::generate(&TriggerOptions::default()));
    run_statements_tx(&mut conn, &statements)
        .await
        .expect("setup");

    // A refresh migration holds its locks until it commits. Every audited write inserts into
    // the history table, so a lock that conflicts with ROW EXCLUSIVE stalls all of them and can
    // deadlock with writers that already hold a lock on an audited table.
    let mut refresh = conn.begin().await.expect("begin refresh");
    run_statements(&mut refresh, &statements)
        .await
        .expect("refresh");
    let blocking: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname || ': ' || l.mode FROM pg_catalog.pg_locks l \
           JOIN pg_catalog.pg_class c ON c.oid = l.relation \
          WHERE l.pid = pg_catalog.pg_backend_pid() \
            AND c.relnamespace = 'auditstream'::pg_catalog.regnamespace \
            AND l.mode IN ('ShareLock', 'ShareRowExclusiveLock', 'ExclusiveLock', 'AccessExclusiveLock') \
          ORDER BY 1",
    )
    .fetch_all(&mut *refresh)
    .await
    .expect("read refresh locks");
    assert!(
        blocking.is_empty(),
        "refresh blocks audited writes: {blocking:?}"
    );
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
        run_statements_tx(&mut conn, &stmts)
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

        // Verify named indexes exist on auditstream.changes and changes_lookup_idx uses ADR-0008 partial hash.
        let indexes: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT i.indexname::text, am.amname::text, i.indexdef::text \
             FROM pg_catalog.pg_indexes i \
             JOIN pg_catalog.pg_namespace n ON n.nspname = i.schemaname \
             JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = i.indexname \
             JOIN pg_catalog.pg_am am ON am.oid = c.relam \
             WHERE i.schemaname = 'auditstream' AND i.tablename = 'changes' \
             ORDER BY i.indexname",
        )
        .fetch_all(&mut conn)
        .await
        .expect("query indexes");
        let index_names: Vec<&str> = indexes.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(index_names.contains(&"changes_occurred_at_brin_idx"));
        assert!(index_names.contains(&"changes_xact_id_idx"));
        let (_, lookup_am, lookup_def) = indexes
            .iter()
            .find(|(name, _, _)| name == "changes_lookup_idx")
            .expect("changes_lookup_idx must exist");
        assert_eq!(
            lookup_am, "hash",
            "changes_lookup_idx must use hash access method per ADR-0008, got {lookup_def}"
        );
        assert!(
            lookup_def.contains("USING hash (record_id) WHERE (record_id IS NOT NULL)"),
            "unexpected changes_lookup_idx definition: {lookup_def}"
        );

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

    // Verify invalid action (including whitespace-padded reserved DML) and non-object target/data raise SQLSTATE 22023.
    for (label, sql) in [
        (
            "reserved action INSERT",
            "SELECT auditstream.log_event('INSERT', NULL, NULL)",
        ),
        (
            "padded reserved action",
            "SELECT auditstream.log_event(' INSERT ', NULL, NULL)",
        ),
        (
            "non-object target array",
            "SELECT auditstream.log_event('invoice.exported', '[1,2]'::jsonb, NULL)",
        ),
        (
            "non-object data string",
            "SELECT auditstream.log_event('invoice.exported', NULL, '\"bad\"'::jsonb)",
        ),
    ] {
        let bad = sqlx::query(sql)
            .execute(&mut app_conn)
            .await
            .expect_err(&format!("{label} must fail"));
        assert_eq!(
            bad.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("22023"),
            "{label}: expected SQLSTATE 22023, got {bad}"
        );
    }

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
        run_statements_tx(&mut conn, &stmts)
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
        let err = run_statements_tx(&mut conn2, &without_self_grant)
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

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn alter_default_privileges_cannot_grant_writes_on_changes_or_child_partitions() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let db = TestDb::non_superuser()
        .await
        .expect("create non-superuser TestDb");
    let admin_url = db.admin_url().to_owned();
    let migrator_role = db.role_name().expect("migrator role").to_owned();
    let app_role = format!("defpriv_{}", db.database_name());
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
            .expect("drop owner role");
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
        let mut conn = PgConnection::connect(db.url())
            .await
            .expect("migrator connect");
        // Simulate a common managed-Postgres setup where the migration role automatically
        // grants ALL ON TABLES to the application role and PUBLIC.
        conn.execute(AssertSqlSafe(format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON TABLES TO {}",
            quote_ident(&migrator_role),
            quote_ident(&app_role),
        )))
        .await
        .expect("set default privileges for app_role");
        conn.execute(AssertSqlSafe(format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON TABLES TO PUBLIC",
            quote_ident(&migrator_role),
        )))
        .await
        .expect("set default privileges for PUBLIC");

        let mut tx = conn.begin().await.expect("begin setup tx");
        run_statements(&mut tx, &stmts)
            .await
            .expect("apply DDL under ALTER DEFAULT PRIVILEGES");
        tx.commit().await.expect("commit setup tx");
        conn.close().await.expect("close migrator");
    }

    let now_utc = OffsetDateTime::now_utc();
    let current_part = format!(
        "auditstream.changes_{:04}_{:02}",
        now_utc.year(),
        u8::from(now_utc.month())
    );

    let app_url = app_role_url(&db, &app_role, &app_pw);
    let mut app_conn = PgConnection::connect(&app_url)
        .await
        .expect("app_role connect");

    for target in [
        "auditstream.changes",
        "auditstream.changes_default",
        current_part.as_str(),
    ] {
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

    // Also verify re-applying DDL heals leaked write grants on pre-existing partitions and changes
    // (simulating an upgrade from a v0.1.0 database installed under ALTER DEFAULT PRIVILEGES).
    {
        let (base, _) = admin_url.rsplit_once('/').expect("admin url slash");
        let admin_db_url = format!("{base}/{}", db.database_name());
        let mut db_admin = PgConnection::connect(&admin_db_url)
            .await
            .expect("admin connect target db");
        db_admin
            .execute(AssertSqlSafe(format!(
                "GRANT ALL ON TABLE auditstream.changes, auditstream.changes_default, {current_part} TO {}",
                quote_ident(&app_role),
            )))
            .await
            .expect("inject legacy leaked grants");
        db_admin.close().await.expect("close db_admin");

        let mut migrator = PgConnection::connect(db.url())
            .await
            .expect("migrator reconnect");
        run_statements_tx(&mut migrator, &stmts)
            .await
            .expect("re-apply DDL to heal leaked grants");
        migrator.close().await.expect("close migrator");
    }

    for target in [
        "auditstream.changes",
        "auditstream.changes_default",
        current_part.as_str(),
    ] {
        let err = app_conn
            .execute(AssertSqlSafe(format!("DELETE FROM {target}")))
            .await
            .expect_err(&format!("healed {target} must deny DELETE"));
        assert!(
            err.to_string().contains("permission denied"),
            "expected permission denied after healing {target}, got {err}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM auditstream.changes")
        .fetch_one(&mut app_conn)
        .await
        .expect("app_role retains SELECT on auditstream.changes after healing");
    assert_eq!(count, 0);

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
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(OWNER_ROLE)
        )))
        .await;
    let _ = admin.close().await;
}

#[tokio::test]
async fn autocommit_and_failed_tx_never_leave_migrator_holding_auditstream_owner() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let db = TestDb::non_superuser()
        .await
        .expect("create non-superuser TestDb");
    let admin_url = db.admin_url().to_owned();
    let migrator_role = db.role_name().expect("migrator role").to_owned();

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
            .expect("drop owner role");
        admin.close().await.expect("close admin");
    }

    let stmts = ddl::generate(&DdlOptions::default());
    let mut conn = PgConnection::connect(db.url())
        .await
        .expect("migrator connect");

    // 1. Autocommit execution with a mid-setup failure must fail BEFORE granting auditstream_owner.
    let mut autocommit_stmts = stmts.clone();
    autocommit_stmts.insert(2, "SELECT 1 / 0".to_owned());
    let _ = run_statements(&mut conn, &autocommit_stmts)
        .await
        .expect_err("autocommit execution must fail");

    let has_usage_after_autocommit: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $2) AND pg_catalog.pg_has_role($1, $2, 'USAGE')")
            .bind(&migrator_role)
            .bind(OWNER_ROLE)
            .fetch_one(&mut conn)
            .await
            .unwrap_or(false);
    assert!(
        !has_usage_after_autocommit,
        "autocommit failure must never leave migrator holding USAGE on {OWNER_ROLE}"
    );

    // 2. A mid-transaction failure inside BEGIN/ROLLBACK must also leave zero elevated membership.
    {
        let mut tx = conn.begin().await.expect("begin failing tx");
        let mut failing_stmts = stmts.clone();
        failing_stmts.insert(failing_stmts.len() - 1, "SELECT 1 / 0".to_owned());
        let _ = run_statements(&mut tx, &failing_stmts)
            .await
            .expect_err("injected error fails tx");
        tx.rollback().await.expect("rollback failed tx");
    }

    let has_usage_after_rollback: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $2) AND pg_catalog.pg_has_role($1, $2, 'USAGE')")
            .bind(&migrator_role)
            .bind(OWNER_ROLE)
            .fetch_one(&mut conn)
            .await
            .unwrap_or(false);
    assert!(
        !has_usage_after_rollback,
        "rolled-back transaction must never leave migrator holding USAGE on {OWNER_ROLE}"
    );

    conn.close().await.expect("close conn");
    drop(db);
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(OWNER_ROLE)
        )))
        .await;
    let _ = admin.close().await;
}
