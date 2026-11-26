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
                "DO $$ BEGIN \
                   DROP ROLE IF EXISTS {owner}; \
                 EXCEPTION WHEN dependent_objects_still_exist THEN \
                   IF pg_catalog.current_setting('server_version_num')::int >= 160000 THEN \
                     EXECUTE 'GRANT {owner} TO {mig} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE'; \
                   END IF; \
                 END $$",
                owner = quote_ident(OWNER_ROLE),
                mig = quote_ident(&migrator_role),
            )))
            .await
            .expect("reset or delegate owner role");
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
        // grants ALL ON TABLES, SEQUENCES, and FUNCTIONS to the application role and PUBLIC.
        for grant_sql in [
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON TABLES TO {}",
                quote_ident(&migrator_role),
                quote_ident(&app_role),
            ),
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON TABLES TO PUBLIC",
                quote_ident(&migrator_role),
            ),
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON SEQUENCES TO {}",
                quote_ident(&migrator_role),
                quote_ident(&app_role),
            ),
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON SEQUENCES TO PUBLIC",
                quote_ident(&migrator_role),
            ),
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON FUNCTIONS TO {}",
                quote_ident(&migrator_role),
                quote_ident(&app_role),
            ),
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT ALL ON FUNCTIONS TO PUBLIC",
                quote_ident(&migrator_role),
            ),
        ] {
            conn.execute(AssertSqlSafe(grant_sql))
                .await
                .expect("set default privileges");
        }

        conn.execute("CREATE TABLE public.invoice (id int PRIMARY KEY, status text)")
            .await
            .expect("create public.invoice");

        let mut tx = conn.begin().await.expect("begin setup tx");
        run_statements(&mut tx, &stmts)
            .await
            .expect("apply DDL under ALTER DEFAULT PRIVILEGES");
        let trig_stmts =
            auditstream_pg::triggers::generate(&auditstream_pg::triggers::TriggerOptions::new(
                DEFAULT_SCHEMA,
                vec![auditstream_pg::triggers::TableTriggerSpec::new(
                    "public",
                    "invoice",
                    vec!["id".to_owned()],
                    vec![],
                )],
            ));
        run_statements(&mut tx, &trig_stmts)
            .await
            .expect("apply triggers under ALTER DEFAULT PRIVILEGES");
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

    // app_role must NOT hold sequence privileges (prevents setval DoS / cursor corruption).
    let seq_err = app_conn
        .execute("SELECT pg_catalog.setval('auditstream.changes_id_seq', 1)")
        .await
        .expect_err("app_role must be denied setval on changes_id_seq");
    assert!(
        seq_err.to_string().contains("permission denied"),
        "expected permission denied on sequence, got: {seq_err}"
    );

    // app_role must NOT hold EXECUTE on SECURITY DEFINER capture() or resolve_actor(),
    // blocking forged audit rows via temp-table triggers.
    let resolve_err = app_conn
        .execute("SELECT * FROM auditstream.resolve_actor()")
        .await
        .expect_err("app_role must be denied EXECUTE on resolve_actor()");
    assert!(
        resolve_err.to_string().contains("permission denied"),
        "expected permission denied on resolve_actor(), got: {resolve_err}"
    );
    app_conn
        .execute("CREATE TEMP TABLE evil (id int, status text)")
        .await
        .expect("create temp table");
    let spoof_trig_err = app_conn
        .execute(
            "CREATE TRIGGER spoof AFTER INSERT ON evil \
             REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT \
             EXECUTE FUNCTION auditstream.capture('[\"id\"]', '[]', 'public', 'invoice')",
        )
        .await
        .expect_err("app_role must be denied CREATE TRIGGER calling auditstream.capture()");
    assert!(
        spoof_trig_err.to_string().contains("permission denied"),
        "expected permission denied on capture() trigger creation, got: {spoof_trig_err}"
    );

    // Defense in depth: even if a superuser attaches capture() with spoofed TG_ARGV[2..3]
    // pointing to an unrelated table, capture() rejects the write with SQLSTATE 42501.
    {
        let (base, _) = admin_url.rsplit_once('/').expect("admin url slash");
        let admin_db_url = format!("{base}/{}", db.database_name());
        let mut db_admin = PgConnection::connect(&admin_db_url)
            .await
            .expect("admin connect target db");
        db_admin
            .execute(
                "CREATE TABLE public.unrelated_evil (id int PRIMARY KEY, status text); \
                 CREATE TRIGGER spoof_insert AFTER INSERT ON public.unrelated_evil \
                 REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT \
                 EXECUTE FUNCTION auditstream.capture('[\"id\"]', '[]', 'public', 'invoice');",
            )
            .await
            .expect("create unrelated table with spoofed trigger");
        let spoof_insert_err = db_admin
            .execute("INSERT INTO public.unrelated_evil (id, status) VALUES (999, 'forged')")
            .await
            .expect_err("capture() must reject TG_RELID not belonging to named parent");
        assert_eq!(
            spoof_insert_err
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("42501"),
            "expected SQLSTATE 42501 on parent spoof, got: {spoof_insert_err}"
        );

        // Also inject leaked grants on a historical partition, SELECT WITH GRANT OPTION on changes,
        // column-level UPDATE on changes, sequence grants, and function grants to verify re-running
        // DDL heals all of them (including mixed-case/hyphenated app_roles).
        db_admin
            .execute(AssertSqlSafe(format!(
                "CREATE TABLE auditstream.changes_2025_01 PARTITION OF auditstream.changes \
                   FOR VALUES FROM ('2025-01-01 00:00:00+00') TO ('2025-02-01 00:00:00+00'); \
                 ALTER TABLE auditstream.changes_2025_01 OWNER TO auditstream_owner; \
                 GRANT ALL ON TABLE auditstream.changes, auditstream.changes_default, \
                   auditstream.changes_2025_01, {current_part} TO {} WITH GRANT OPTION; \
                 GRANT UPDATE (actor, after) ON TABLE auditstream.changes TO {}; \
                 GRANT ALL ON SEQUENCE auditstream.changes_id_seq TO {}; \
                 GRANT EXECUTE ON FUNCTION auditstream.capture() TO {};",
                quote_ident(&app_role),
                quote_ident(&app_role),
                quote_ident(&app_role),
                quote_ident(&app_role),
            )))
            .await
            .expect("inject legacy leaked grants");
        let special_role = format!("App-Role-{}", db.database_name());
        db_admin
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE {} NOLOGIN",
                quote_ident(&special_role)
            )))
            .await
            .expect("create mixed-case hyphenated role");
        db_admin.close().await.expect("close db_admin");

        let mut migrator = PgConnection::connect(db.url())
            .await
            .expect("migrator reconnect");
        let heal_stmts = ddl::generate(&DdlOptions::new(
            DEFAULT_SCHEMA,
            vec![special_role.clone(), app_role.clone()],
        ));
        let trig_stmts = auditstream_pg::triggers::generate(
            &auditstream_pg::triggers::TriggerOptions::new(DEFAULT_SCHEMA, vec![]),
        );
        let mut tx = migrator.begin().await.expect("begin healing tx");
        run_statements(&mut tx, &heal_stmts)
            .await
            .expect("re-apply DDL with hyphenated/mixed-case app_roles to heal leaked grants");
        run_statements(&mut tx, &trig_stmts)
            .await
            .expect("re-apply triggers to heal function grants");
        tx.commit().await.expect("commit healing tx");
        migrator.close().await.expect("close migrator");

        let mut cleanup_admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin drop special role");
        // Keep app_role; remove special_role from ACLs first so DROP ROLE succeeds cleanly.
        let (base, _) = admin_url.rsplit_once('/').expect("admin url slash");
        let mut special_db_admin = PgConnection::connect(&format!("{base}/{}", db.database_name()))
            .await
            .expect("db admin revoke special role");
        let mut tx = special_db_admin.begin().await.expect("begin re-heal");
        run_statements(&mut tx, &stmts)
            .await
            .expect("re-apply default app_roles");
        tx.commit().await.expect("commit re-heal");
        special_db_admin.close().await.expect("close db admin");
        let _ = cleanup_admin
            .execute(AssertSqlSafe(format!(
                "DROP ROLE IF EXISTS {}",
                quote_ident(&special_role)
            )))
            .await;
        cleanup_admin.close().await.expect("close cleanup_admin");
    }

    // Re-assert column-level UPDATE, changes_id_seq, and capture() are all denied after healing.
    let col_upd_err = app_conn
        .execute("UPDATE auditstream.changes SET actor = NULL")
        .await
        .expect_err("column-level UPDATE on changes must be revoked after healing");
    assert!(
        col_upd_err.to_string().contains("permission denied"),
        "expected permission denied for column UPDATE, got {col_upd_err}"
    );
    let healed_seq_err = app_conn
        .execute("SELECT setval('auditstream.changes_id_seq', 1)")
        .await
        .expect_err("changes_id_seq must be revoked after healing");
    assert!(
        healed_seq_err.to_string().contains("permission denied"),
        "expected permission denied on healed sequence, got {healed_seq_err}"
    );
    let healed_cap_err = app_conn
        .execute(
            "CREATE TRIGGER spoof_healed AFTER INSERT ON evil \
             REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT \
             EXECUTE FUNCTION auditstream.capture('[\"id\"]', '[]', 'public', 'invoice')",
        )
        .await
        .expect_err("capture() EXECUTE must be revoked after healing");
    assert!(
        healed_cap_err.to_string().contains("permission denied"),
        "expected permission denied on healed capture(), got {healed_cap_err}"
    );

    for target in [
        "auditstream.changes",
        "auditstream.changes_default",
        "auditstream.changes_2025_01",
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
    // Historical child partition must deny even SELECT directly (parent SELECT suffices).
    let child_select_err = app_conn
        .execute("SELECT count(*) FROM auditstream.changes_2025_01")
        .await
        .expect_err("healed child partition must deny direct SELECT");
    assert!(
        child_select_err.to_string().contains("permission denied"),
        "expected permission denied for direct child SELECT, got {child_select_err}"
    );
    // GRANT OPTION on changes must be stripped (app_role cannot re-grant SELECT to PUBLIC).
    let has_grant_option: bool = sqlx::query_scalar(
        "SELECT pg_catalog.has_table_privilege($1, 'auditstream.changes', 'SELECT WITH GRANT OPTION')",
    )
    .bind(&app_role)
    .fetch_one(&mut app_conn)
    .await
    .expect("check SELECT WITH GRANT OPTION");
    assert!(
        !has_grant_option,
        "app_role must not retain WITH GRANT OPTION on auditstream.changes"
    );
    let _ = app_conn
        .execute("GRANT SELECT ON TABLE auditstream.changes TO PUBLIC")
        .await;
    let public_has_select: bool = sqlx::query_scalar(
        "SELECT pg_catalog.has_table_privilege('public', 'auditstream.changes', 'SELECT')",
    )
    .fetch_one(&mut app_conn)
    .await
    .expect("check PUBLIC SELECT after regrant attempt");
    assert!(
        !public_has_select,
        "app_role must not be able to re-grant SELECT on auditstream.changes to PUBLIC"
    );
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
#[allow(clippy::too_many_lines)]
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
                "DO $$ BEGIN \
                   DROP ROLE IF EXISTS {owner}; \
                 EXCEPTION WHEN dependent_objects_still_exist THEN \
                   IF pg_catalog.current_setting('server_version_num')::int >= 160000 THEN \
                     EXECUTE 'GRANT {owner} TO {mig} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE'; \
                   END IF; \
                 END $$",
                owner = quote_ident(OWNER_ROLE),
                mig = quote_ident(&migrator_role),
            )))
            .await
            .expect("reset or delegate owner role");
        admin.close().await.expect("close admin");
    }

    let stmts = ddl::generate(&DdlOptions::default());
    let mut conn = PgConnection::connect(db.url())
        .await
        .expect("migrator connect");
    let pg_version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(&mut conn)
        .await
        .expect("read server_version_num");

    // 1. Autocommit execution must fail at the transaction guard BEFORE creating or granting auditstream_owner.
    let autocommit_err = run_statements(&mut conn, &stmts)
        .await
        .expect_err("autocommit execution must fail");
    assert!(
        autocommit_err.to_string().contains("single transaction"),
        "expected transaction-guard error, got: {autocommit_err}"
    );
    assert_no_elevated_owner_membership(&mut conn, &migrator_role, pg_version, "autocommit").await;

    // 2. A mid-transaction failure inside BEGIN/ROLLBACK must also leave zero elevated membership.
    {
        let mut tx = conn.begin().await.expect("begin failing tx");
        let mut failing_stmts = stmts.clone();
        failing_stmts.insert(failing_stmts.len() - 1, "SELECT 1 / 0".to_owned());
        let div_err = run_statements(&mut tx, &failing_stmts)
            .await
            .expect_err("injected error fails tx");
        assert_eq!(
            div_err
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("22012"),
            "expected division_by_zero (22012) after role setup ran, got: {div_err}"
        );
        tx.rollback().await.expect("rollback failed tx");
    }
    assert_no_elevated_owner_membership(&mut conn, &migrator_role, pg_version, "rollback").await;

    // 3. A NOINHERIT migrator role must also reconcile pre-existing migrator-owned schema/tables/
    //    functions cleanly on both PG 15 and PG 16+ and revoke all temporary grants.
    conn.execute("CREATE TABLE public.noinherit_item (id int PRIMARY KEY, note text)")
        .await
        .expect("create table owned by migrator");
    conn.execute("CREATE SCHEMA auditstream")
        .await
        .expect("pre-create migrator-owned auditstream schema");
    let create_changes = stmts
        .iter()
        .find(|s| s.starts_with("CREATE TABLE IF NOT EXISTS"))
        .expect("ddl must include CREATE TABLE IF NOT EXISTS");
    conn.execute(AssertSqlSafe(create_changes.clone()))
        .await
        .expect("pre-create migrator-owned changes table");
    conn.execute(
        "CREATE FUNCTION auditstream.log_event(action text, target jsonb DEFAULT NULL, data jsonb DEFAULT NULL) \
         RETURNS uuid LANGUAGE sql AS 'SELECT gen_random_uuid()'",
    )
    .await
    .expect("pre-create migrator-owned log_event");
    conn.execute(
        "CREATE FUNCTION auditstream.resolve_actor(OUT actor jsonb, OUT context jsonb, OUT actor_source text) \
         LANGUAGE plpgsql AS $$ BEGIN END $$",
    )
    .await
    .expect("pre-create migrator-owned resolve_actor");
    conn.execute(
        "CREATE FUNCTION auditstream.capture() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
    )
    .await
    .expect("pre-create migrator-owned capture");
    {
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin alter noinherit");
        admin
            .execute(AssertSqlSafe(format!(
                "ALTER ROLE {} NOINHERIT",
                quote_ident(&migrator_role)
            )))
            .await
            .expect("set NOINHERIT");
        admin.close().await.expect("close admin");
    }
    {
        let mut tx = conn.begin().await.expect("begin noinherit tx");
        run_statements(&mut tx, &stmts)
            .await
            .expect("DDL must succeed as NOINHERIT migrator reconciling pre-existing objects");
        let trig_stmts =
            auditstream_pg::triggers::generate(&auditstream_pg::triggers::TriggerOptions::new(
                DEFAULT_SCHEMA,
                vec![auditstream_pg::triggers::TableTriggerSpec::new(
                    "public",
                    "noinherit_item",
                    vec!["id".to_owned()],
                    vec![],
                )],
            ));
        run_statements(&mut tx, &trig_stmts).await.expect(
            "triggers must succeed as NOINHERIT migrator reconciling pre-existing functions",
        );
        tx.commit().await.expect("commit noinherit tx");
    }
    assert_no_elevated_owner_membership(&mut conn, &migrator_role, pg_version, "noinherit commit")
        .await;

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

async fn assert_no_elevated_owner_membership(
    conn: &mut PgConnection,
    migrator_role: &str,
    pg_version: i32,
    phase: &str,
) {
    let owner_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1)")
            .bind(OWNER_ROLE)
            .fetch_one(&mut *conn)
            .await
            .expect("check owner role existence");
    if !owner_exists {
        return;
    }

    let has_usage: bool = sqlx::query_scalar("SELECT pg_catalog.pg_has_role($1, $2, 'USAGE')")
        .bind(migrator_role)
        .bind(OWNER_ROLE)
        .fetch_one(&mut *conn)
        .await
        .expect("check USAGE");
    assert!(
        !has_usage,
        "{phase}: migrator must not retain USAGE on {OWNER_ROLE}"
    );
    if pg_version >= 160_000 {
        let has_set: bool = sqlx::query_scalar("SELECT pg_catalog.pg_has_role($1, $2, 'SET')")
            .bind(migrator_role)
            .bind(OWNER_ROLE)
            .fetch_one(&mut *conn)
            .await
            .expect("check SET");
        assert!(
            !has_set,
            "{phase}: migrator must not retain SET on {OWNER_ROLE}"
        );
        let has_self_zombie: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
               SELECT 1 FROM pg_catalog.pg_auth_members m \
               JOIN pg_catalog.pg_roles r ON r.oid = m.roleid \
               JOIN pg_catalog.pg_roles u ON u.oid = m.member \
              WHERE r.rolname = $2 AND u.rolname = $1 AND m.grantor = u.oid \
             )",
        )
        .bind(migrator_role)
        .bind(OWNER_ROLE)
        .fetch_one(&mut *conn)
        .await
        .expect("check self-granted zombie row");
        assert!(
            !has_self_zombie,
            "{phase}: PG 16+ cleanup must delete temporary self-granted row without leaving a zombie tuple"
        );
    } else {
        let has_member: bool =
            sqlx::query_scalar("SELECT pg_catalog.pg_has_role($1, $2, 'MEMBER')")
                .bind(migrator_role)
                .bind(OWNER_ROLE)
                .fetch_one(&mut *conn)
                .await
                .expect("check MEMBER");
        assert!(
            !has_member,
            "{phase}: PG 15 migrator must not retain MEMBER on {OWNER_ROLE}"
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pg16_group_admin_and_createrole_self_grant_preserve_memberships() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let db = TestDb::non_superuser()
        .await
        .expect("create non-superuser TestDb");
    let admin_url = db.admin_url().to_owned();
    let migrator_role = db.role_name().expect("migrator role").to_owned();

    let pg_version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(db.pool())
        .await
        .expect("read server_version_num");
    if pg_version < 160_000 {
        return;
    }

    // 1. Even with `createrole_self_grant = 'set, inherit'`, initial DDL setup must NOT persist
    //    permanent INHERIT/SET on `auditstream_owner` (tested when role is newly created).
    let ops_admin = format!("ops_admin_{}", db.database_name());
    let group_mig = format!("grp_mig_{}", db.database_name());
    let group_pw = Uuid::new_v4().simple().to_string();
    {
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin connect");
        admin
            .execute(AssertSqlSafe(format!(
                "ALTER ROLE {} SET createrole_self_grant = 'set, inherit'",
                quote_ident(&migrator_role)
            )))
            .await
            .expect("set createrole_self_grant");
        let _ = admin
            .execute(AssertSqlSafe(format!(
                "DO $$ BEGIN \
                   DROP ROLE IF EXISTS {owner}; \
                 EXCEPTION WHEN dependent_objects_still_exist THEN \
                   EXECUTE 'GRANT {owner} TO {mig} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE'; \
                 END $$",
                owner = quote_ident(OWNER_ROLE),
                mig = quote_ident(&migrator_role),
            )))
            .await;
        admin.close().await.expect("close admin");
    }

    let stmts = ddl::generate(&DdlOptions::default());
    {
        let mut conn = PgConnection::connect(db.url())
            .await
            .expect("migrator connect with createrole_self_grant");
        run_statements_tx(&mut conn, &stmts)
            .await
            .expect("DDL setup succeeds with createrole_self_grant = 'set, inherit'");
        assert_no_elevated_owner_membership(
            &mut conn,
            &migrator_role,
            pg_version,
            "createrole_self_grant",
        )
        .await;
        conn.close().await.expect("close migrator");
    }

    // 2. Group-role delegated ADMIN OPTION (`ops_admin` -> `group_mig` with pre-existing
    //    `INHERIT FALSE, SET TRUE`) must succeed without clobbering SET or deleting membership.
    let db_name = db.database_name().to_owned();
    {
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin setup group admin");
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE {} NOLOGIN",
                quote_ident(&ops_admin)
            )))
            .await
            .expect("create ops_admin");
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE {} LOGIN PASSWORD '{group_pw}'",
                quote_ident(&group_mig)
            )))
            .await
            .expect("create group_mig");
        admin
            .execute(AssertSqlSafe(format!(
                "GRANT {} TO {} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE",
                quote_ident(OWNER_ROLE),
                quote_ident(&ops_admin),
            )))
            .await
            .expect("grant owner ADMIN to ops_admin");
        admin
            .execute(AssertSqlSafe(format!(
                "GRANT {} TO {} WITH INHERIT TRUE, SET FALSE",
                quote_ident(&ops_admin),
                quote_ident(&group_mig),
            )))
            .await
            .expect("grant ops_admin to group_mig");
        admin
            .execute(AssertSqlSafe(format!(
                "GRANT {} TO {} WITH INHERIT FALSE, SET TRUE GRANTED BY {}",
                quote_ident(OWNER_ROLE),
                quote_ident(&group_mig),
                quote_ident(&ops_admin),
            )))
            .await
            .expect("pre-grant INHERIT FALSE, SET TRUE by ops_admin");
        admin
            .execute(AssertSqlSafe(format!(
                "GRANT CONNECT, CREATE ON DATABASE {} TO {}",
                quote_ident(&db_name),
                quote_ident(&group_mig),
            )))
            .await
            .expect("grant db connect");
        admin.close().await.expect("close admin");
    }

    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL credentials");
    let group_url = format!("{scheme}://{group_mig}:{group_pw}@{host_and_db}");
    let mut group_conn = PgConnection::connect(&group_url)
        .await
        .expect("connect as group_mig");
    run_statements_tx(&mut group_conn, &stmts)
        .await
        .expect("DDL refresh must succeed via group-inherited ADMIN OPTION without clobbering SET");

    let (has_usage, has_set): (bool, bool) = sqlx::query_as(
        "SELECT pg_catalog.pg_has_role($1, $2, 'USAGE'), pg_catalog.pg_has_role($1, $2, 'SET')",
    )
    .bind(&group_mig)
    .bind(OWNER_ROLE)
    .fetch_one(&mut group_conn)
    .await
    .expect("check group_mig membership after DDL");
    assert!(
        !has_usage && has_set,
        "group_mig must keep pre-existing (INHERIT FALSE, SET TRUE), got usage={has_usage}, set={has_set}"
    );
    group_conn.close().await.expect("close group_conn");

    drop(db);
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    for role in [&group_mig, &ops_admin] {
        let _ = admin
            .execute(AssertSqlSafe(format!(
                "DROP ROLE IF EXISTS {}",
                quote_ident(role)
            )))
            .await;
    }
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(OWNER_ROLE)
        )))
        .await;
    let _ = admin.close().await;
}
