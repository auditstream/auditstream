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
    //    permanent INHERIT/SET on `auditstream_owner`.
    let ops_admin = format!("ops_admin_{}", db.database_name());
    let group_mig = format!("grp_mig_{}", db.database_name());
    let set_only_mig = format!("set_mig_{}", db.database_name());
    let group_pw = Uuid::new_v4().simple().to_string();
    let owner_was_dropped: bool;
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
        admin
            .execute(AssertSqlSafe(format!(
                "DO $$ BEGIN \
                   DROP ROLE IF EXISTS {owner}; \
                 EXCEPTION WHEN dependent_objects_still_exist THEN \
                   EXECUTE 'GRANT {owner} TO {mig} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE'; \
                 END $$",
                owner = quote_ident(OWNER_ROLE),
                mig = quote_ident(&migrator_role),
            )))
            .await
            .expect("reset or delegate owner role");
        owner_was_dropped = !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1)",
        )
        .bind(OWNER_ROLE)
        .fetch_one(&mut admin)
        .await
        .expect("check owner role dropped");
        admin.close().await.expect("close admin");
    }
    assert!(
        owner_was_dropped,
        "{OWNER_ROLE} must be absent before testing createrole_self_grant initial CREATE ROLE"
    );

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
    //    `INHERIT TRUE, SET FALSE`) must succeed via `GRANTED BY ops_admin` and preserve
    //    `(INHERIT TRUE, SET FALSE)` without deleting the pre-existing membership.
    // 3. A DBA-provisioned non-admin `NOINHERIT` / `SET`-only migrator (no `ADMIN OPTION`, no
    //    `CREATEROLE`, `INHERIT FALSE, SET TRUE`) must also succeed on both DDL and triggers.
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
                "CREATE ROLE {} LOGIN NOINHERIT PASSWORD '{group_pw}'",
                quote_ident(&set_only_mig)
            )))
            .await
            .expect("create set_only_mig");
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
                "GRANT {} TO {} WITH INHERIT TRUE, SET FALSE GRANTED BY {}",
                quote_ident(OWNER_ROLE),
                quote_ident(&group_mig),
                quote_ident(&ops_admin),
            )))
            .await
            .expect("pre-grant INHERIT TRUE, SET FALSE by ops_admin");
        admin
            .execute(AssertSqlSafe(format!(
                "GRANT {} TO {} WITH INHERIT FALSE, SET TRUE",
                quote_ident(OWNER_ROLE),
                quote_ident(&set_only_mig),
            )))
            .await
            .expect("grant SET-only non-admin membership to set_only_mig");
        for role in [&group_mig, &set_only_mig] {
            admin
                .execute(AssertSqlSafe(format!(
                    "GRANT CONNECT, CREATE ON DATABASE {} TO {}",
                    quote_ident(&db_name),
                    quote_ident(role),
                )))
                .await
                .expect("grant db connect");
        }
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
        .expect("DDL refresh must succeed via group-inherited ADMIN OPTION");

    let (has_usage, has_set): (bool, bool) = sqlx::query_as(
        "SELECT pg_catalog.pg_has_role($1, $2, 'USAGE'), pg_catalog.pg_has_role($1, $2, 'SET')",
    )
    .bind(&group_mig)
    .bind(OWNER_ROLE)
    .fetch_one(&mut group_conn)
    .await
    .expect("check group_mig membership after DDL");
    assert!(
        has_usage && !has_set,
        "group_mig must keep pre-existing (INHERIT TRUE, SET FALSE), got usage={has_usage}, set={has_set}"
    );
    group_conn.close().await.expect("close group_conn");

    let set_only_url = format!("{scheme}://{set_only_mig}:{group_pw}@{host_and_db}");
    db.pool()
        .execute(AssertSqlSafe(format!(
            "GRANT CREATE ON SCHEMA public TO {}",
            quote_ident(&set_only_mig)
        )))
        .await
        .expect("grant CREATE on public schema to set_only_mig");
    let mut set_conn = PgConnection::connect(&set_only_url)
        .await
        .expect("connect as set_only_mig");
    set_conn
        .execute("CREATE TABLE public.set_only_item (id int PRIMARY KEY, note text)")
        .await
        .expect("create table as set_only_mig");
    let trig_stmts =
        auditstream_pg::triggers::generate(&auditstream_pg::triggers::TriggerOptions::new(
            DEFAULT_SCHEMA,
            vec![auditstream_pg::triggers::TableTriggerSpec::new(
                "public",
                "set_only_item",
                vec!["id".to_owned()],
                vec![],
            )],
        ));
    {
        let mut tx = set_conn.begin().await.expect("begin set_only tx");
        run_statements(&mut tx, &stmts)
            .await
            .expect("DDL must succeed for SET-only non-admin migrator on PG 16+");
        run_statements(&mut tx, &trig_stmts)
            .await
            .expect("triggers must succeed for SET-only non-admin migrator on PG 16+");
        tx.commit().await.expect("commit set_only tx");
    }
    set_conn.close().await.expect("close set_conn");

    drop(db);
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    for role in [&set_only_mig, &group_mig, &ops_admin] {
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

#[tokio::test]
async fn log_event_rejects_control_chars_whitespace_and_padded_dml_and_round_trips_through_reader()
{
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    let mut stmts = ddl::generate(&DdlOptions::default());
    stmts.extend(triggers::generate(&TriggerOptions::default()));
    run_statements_tx(&mut conn, &stmts)
        .await
        .expect("install DDL and triggers");

    // Every poison-pill action (whitespace-only, control chars, leading/trailing ASCII & Unicode
    // whitespace, padded/cased reserved DML, overlong) MUST be rejected with SQLSTATE 22023.
    let invalid_actions: &[(&str, &str)] = &[
        ("empty", ""),
        ("spaces only", "   "),
        ("newline only", "\n"),
        ("tab only", "\t"),
        ("cr only", "\r"),
        ("vt only", "\x0b"),
        ("ff only", "\x0c"),
        ("nbsp only", "\u{00A0}"),
        ("ideographic space only", "\u{3000}"),
        ("leading space", " invoice.exported"),
        ("trailing space", "invoice.exported "),
        ("leading and trailing space", " invoice.exported "),
        ("leading nbsp", "\u{00A0}invoice.exported"),
        ("trailing em space", "invoice.exported\u{2003}"),
        ("trailing ideographic space", "invoice.exported\u{3000}"),
        ("leading line separator", "\u{2028}invoice.exported"),
        ("trailing paragraph separator", "invoice.exported\u{2029}"),
        ("embedded newline", "invoice\nexported"),
        ("embedded cr", "invoice\rexported"),
        ("embedded tab", "invoice\texported"),
        ("embedded vt", "invoice\x0bexported"),
        ("embedded ff", "invoice\x0cexported"),
        ("embedded soh", "invoice\x01exported"),
        ("embedded us", "invoice\x1fexported"),
        ("embedded del", "invoice\x7fexported"),
        ("embedded nel", "invoice\u{0085}exported"),
        ("embedded csi", "invoice\u{009B}exported"),
        ("reserved INSERT", "INSERT"),
        ("reserved lowercase insert", "insert"),
        ("reserved mixed UpDaTe", "UpDaTe"),
        ("reserved DELETE", "DELETE"),
        ("reserved TRUNCATE", "TRUNCATE"),
        ("tab-padded INSERT", "\tINSERT"),
        ("newline-padded update", "\nupdate"),
        ("nbsp-padded delete", "\u{00A0}delete"),
        ("space-padded TRUNCATE", " TRUNCATE "),
    ];

    for (label, action) in invalid_actions {
        let err = sqlx::query("SELECT auditstream.log_event($1, NULL, NULL)")
            .bind(*action)
            .execute(&mut conn)
            .await
            .expect_err(&format!("{label} ({action:?}) must be rejected"));
        assert_eq!(
            err.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("22023"),
            "{label}: expected SQLSTATE 22023, got {err}"
        );
    }

    let overlong_65 = "é".repeat(65);
    let overlong_err = sqlx::query("SELECT auditstream.log_event($1, NULL, NULL)")
        .bind(&overlong_65)
        .execute(&mut conn)
        .await
        .expect_err("65-char action must be rejected");
    assert_eq!(
        overlong_err
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023"),
        "expected SQLSTATE 22023 for 65-char action, got {overlong_err}"
    );

    // Valid actions (including internal space and exact 64-char UTF-8) succeed and decode cleanly
    // through `reader::lookup` without SourceError::Decode.
    let valid_64 = "é".repeat(64);
    for action in ["invoice.exported", "invoice exported", valid_64.as_str()] {
        let _: String = sqlx::query_scalar(
            "SELECT auditstream.log_event($1, '{\"id\":1}'::jsonb, '{\"ok\":true}'::jsonb)::text",
        )
        .bind(action)
        .fetch_one(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("valid action {action:?} failed: {e}"));
    }
    conn.close().await.expect("close conn");

    let events = auditstream_pg::reader::lookup(
        db.pool(),
        DEFAULT_SCHEMA,
        &auditstream_pg::reader::HistoryQuery::default(),
    )
    .await
    .expect("reader::lookup must decode every accepted log_event row cleanly");
    assert_eq!(events.len(), 3);
    let ops: Vec<&str> = events.iter().map(|ev| ev.op.as_str()).collect();
    assert!(ops.contains(&"invoice.exported"));
    assert!(ops.contains(&"invoice exported"));
    assert!(ops.contains(&valid_64.as_str()));
}

#[tokio::test]
async fn adversarial_dollar_quote_schema_names_install_and_execute_without_breakout() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    // Combines `\`, `$$`, `$do$`, `$fn$`, `'`, and `"` within PostgreSQL's 63-byte NAMEDATALEN;
    // executing under `standard_conforming_strings = off` would break out of any `DO '...'` fallback.
    let schema = r#"a\$$$do$$fn$'" ;DROP TABLE z;--"#;
    assert!(schema.len() <= 63);

    let mut stmts = ddl::generate(&DdlOptions::new(schema, vec![]));
    stmts.extend(triggers::generate(&TriggerOptions::new(schema, vec![])));
    for stmt in &stmts {
        if stmt.starts_with("DO ") {
            assert!(
                stmt.starts_with("DO $"),
                "DO block must use collision-free dollar-quoting, got: {stmt}"
            );
        }
    }

    // Install and idempotently re-apply (under `standard_conforming_strings = off` on PG <= 18;
    // PG 19 removed non-standard string literals with SQLSTATE 0A000).
    let scs_off = crate::supports_non_standard_strings(&mut conn).await;
    for phase in ["install", "re-apply"] {
        let mut tx = conn.begin().await.expect("begin tx");
        if scs_off {
            tx.execute("SET LOCAL standard_conforming_strings = off")
                .await
                .expect("disable standard_conforming_strings");
        }
        run_statements(&mut tx, &stmts)
            .await
            .unwrap_or_else(|err| panic!("adversarial schema {phase} failed: {err}"));
        tx.commit().await.expect("commit tx");
    }

    let schema_q = quote_ident(schema);
    let event_id: String = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT {schema_q}.log_event('invoice.exported', '{{\"id\":7}}'::jsonb, NULL)::text"
    )))
    .fetch_one(&mut conn)
    .await
    .expect("log_event executes inside adversarial schema");
    assert_ne!(event_id, "");

    let recorded_op: String = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT op FROM {schema_q}.changes WHERE event_id::text = $1"
    )))
    .bind(&event_id)
    .fetch_one(&mut conn)
    .await
    .expect("read back row from adversarial schema");
    assert_eq!(recorded_op, "invoice.exported");

    let server_supports_lz4: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
           SELECT 1 FROM pg_catalog.pg_settings \
            WHERE name = 'default_toast_compression' AND 'lz4' = ANY(enumvals) \
         )",
    )
    .fetch_one(&mut conn)
    .await
    .expect("check LZ4 support");
    let adversarial_comps: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text, a.attcompression::text \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
           JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
          WHERE n.nspname = $1 \
            AND c.relkind IN ('r', 'p') \
            AND a.attname IN ('before', 'after') \
            AND a.attnum > 0 AND NOT a.attisdropped \
          ORDER BY c.relname, a.attname",
    )
    .bind(schema)
    .fetch_all(&mut conn)
    .await
    .expect("query attcompression in adversarial schema");
    assert!(
        adversarial_comps.len() >= 12,
        "expected parent + default + 4 monthly partitions x 2 cols, got {adversarial_comps:?}"
    );
    for (rel, col, comp) in &adversarial_comps {
        if server_supports_lz4 {
            assert_eq!(
                comp, "l",
                "adversarial schema {rel}.{col} must use LZ4 compression"
            );
        } else {
            assert_ne!(comp, "l");
        }
    }
    conn.close().await.expect("close conn");
}

#[tokio::test]
async fn standard_conforming_strings_off_prevents_quote_literal_breakout_and_preserves_trigger_args()
 {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    let evil_table = r"t\'); CREATE TABLE public.scs_pwned (id int); --";
    assert!(evil_table.len() <= 63);
    let evil_role = format!(r"r\'_{}", db.database_name());
    assert!(evil_role.len() <= 63);

    conn.execute(AssertSqlSafe(format!(
        "CREATE ROLE {} NOLOGIN; \
         CREATE TABLE public.{} (\"col\"\"id\" int PRIMARY KEY, \"secret\\new\" text, note text) \
         PARTITION BY RANGE (\"col\"\"id\"); \
         CREATE TABLE public.scs_child PARTITION OF public.{} FOR VALUES FROM (0) TO (100);",
        quote_ident(&evil_role),
        quote_ident(evil_table),
        quote_ident(evil_table),
    )))
    .await
    .expect("create adversarial role and partitioned table");

    let spec = auditstream_pg::triggers::TableTriggerSpec::new(
        "public",
        evil_table,
        vec![r#"col"id"#.to_owned()],
        vec![r"secret\new".to_owned()],
    )
    .partitioned();
    let mut stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![evil_role.clone()]));
    stmts.extend(triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![spec],
    )));

    // Apply ALL generated statements inside a transaction (with `standard_conforming_strings = off`
    // on PG <= 18; PG 19 removed non-standard string literals with SQLSTATE 0A000).
    let scs_off = crate::supports_non_standard_strings(&mut conn).await;
    let mut tx = conn.begin().await.expect("begin tx");
    if scs_off {
        tx.execute("SET LOCAL standard_conforming_strings = off")
            .await
            .expect("disable standard_conforming_strings");
    }
    run_statements(&mut tx, &stmts)
        .await
        .expect("install DDL & triggers safely");
    tx.commit().await.expect("commit tx");

    let pwned_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.scs_pwned') IS NOT NULL")
            .fetch_one(&mut conn)
            .await
            .expect("check pwned table absent");
    assert!(!pwned_exists, "injected table must not exist");

    // Insert into the partitioned table: PK `col"id` must parse cleanly and `secret\new` must stay excluded.
    conn.execute(AssertSqlSafe(format!(
        "INSERT INTO public.{} (\"col\"\"id\", \"secret\\new\", note) VALUES (1, 'topsecret', 'ok')",
        quote_ident(evil_table)
    )))
    .await
    .expect("insert captured cleanly");

    let (record_id, after): (String, String) = sqlx::query_as(
        "SELECT record_id::text, after::text FROM auditstream.changes WHERE op = 'INSERT' LIMIT 1",
    )
    .fetch_one(&mut conn)
    .await
    .expect("read captured row");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&record_id).expect("valid record_id json"),
        serde_json::json!({"col\"id": 1})
    );
    let after_val: serde_json::Value = serde_json::from_str(&after).expect("valid after json");
    assert!(
        after_val.get(r"secret\new").is_none(),
        "excluded column with backslash must be stripped, got: {after}"
    );
    assert_eq!(after_val.get("note").and_then(|v| v.as_str()), Some("ok"));

    conn.close().await.expect("close conn");
    let admin_url = db.admin_url().to_owned();
    drop(db);
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(&evil_role)
        )))
        .await
        .expect("drop evil role");
    admin.close().await.expect("close admin");
}

#[tokio::test]
async fn log_event_handles_nondeterministic_icu_collations_deterministically() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    let mut stmts = ddl::generate(&DdlOptions::default());
    stmts.extend(triggers::generate(&TriggerOptions::default()));
    run_statements_tx(&mut conn, &stmts)
        .await
        .expect("install DDL and triggers");

    // Create nondeterministic ICU collations (case-insensitive, accent-insensitive, and ignorable-space).
    if let Err(err) = conn
        .execute(
            "CREATE COLLATION public.nd_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false); \
             CREATE COLLATION public.nd_l1 (provider = icu, locale = 'und-u-ks-level1', deterministic = false); \
             CREATE COLLATION public.nd_ignorable (provider = icu, locale = 'und-u-ka-shifted', deterministic = false);",
        )
        .await
    {
        let code = err
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code);
        if code.as_deref() == Some("0A000") && std::env::var_os("AUDITSTREAM_TEST_PG").is_none() {
            // Skip only on local non-ICU PostgreSQL builds; never silently skip in CI.
            return;
        }
        panic!("CREATE COLLATION for ICU test failed: {err}");
    }

    // Confirm the created ICU collations genuinely collapse accents and spaces as expected.
    let (accent_eq, space_eq): (bool, bool) = sqlx::query_as(
        "SELECT ('ïnsert' = 'INSERT' COLLATE public.nd_l1), \
                (' invoice.exported ' = 'invoice.exported' COLLATE public.nd_ignorable)",
    )
    .fetch_one(&mut conn)
    .await
    .expect("verify nondeterministic ICU collation semantics");
    assert!(accent_eq, "nd_l1 must treat 'ïnsert' as equal to 'INSERT'");
    assert!(
        space_eq,
        "nd_ignorable must treat ' invoice.exported ' as equal to 'invoice.exported'"
    );

    // Valid actions carrying nondeterministic collations MUST succeed (never fail with 0A000).
    let _: String = sqlx::query_scalar(
        "SELECT auditstream.log_event('invoice.exported' COLLATE public.nd_ci, NULL, NULL)::text",
    )
    .fetch_one(&mut conn)
    .await
    .expect("valid action with nondeterministic CI collation must succeed");

    // Accent-insensitive collation must NOT falsely reject accented non-DML action `'ïnsert'`.
    let _: String = sqlx::query_scalar(
        "SELECT auditstream.log_event('ïnsert' COLLATE public.nd_l1, NULL, NULL)::text",
    )
    .fetch_one(&mut conn)
    .await
    .expect(
        "accented non-DML action must not be falsely rejected under accent-insensitive collation",
    );

    // Ignorable-space collation must NOT bypass leading/trailing whitespace rejection.
    let padded_err = sqlx::query(
        "SELECT auditstream.log_event(' invoice.exported ' COLLATE public.nd_ignorable, NULL, NULL)",
    )
    .execute(&mut conn)
    .await
    .expect_err("whitespace-padded action must still be rejected under ignorable-space collation");
    assert_eq!(
        padded_err
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );

    // Control characters and reserved DML are still rejected under nondeterministic collations.
    for sql in [
        "SELECT auditstream.log_event(E'invoice\\nexported' COLLATE public.nd_ci, NULL, NULL)",
        "SELECT auditstream.log_event('insert' COLLATE public.nd_ci, NULL, NULL)",
    ] {
        let err = sqlx::query(sql)
            .execute(&mut conn)
            .await
            .expect_err(&format!("{sql} must fail with 22023"));
        assert_eq!(
            err.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("22023")
        );
    }

    conn.close().await.expect("close conn");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn lz4_toast_compression_sets_current_and_future_skips_historical_and_is_lock_free_on_rerun()
{
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::non_superuser()
        .await
        .expect("create non-superuser TestDb");
    let admin_url = db.admin_url().to_owned();
    let migrator_role = db.role_name().expect("migrator role").to_owned();
    let (base_admin, _) = admin_url.rsplit_once('/').expect("admin url slash");
    let admin_db_url = format!("{base_admin}/{}", db.database_name());

    // Prepare clean owner role / delegation for the non-superuser migrator.
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
            .expect("prepare owner role");
        admin.close().await.expect("close admin");
    }

    let stmts = ddl::generate(&DdlOptions::default());
    let toast_stmt = stmts
        .iter()
        .find(|s| s.contains("default_toast_compression") && s.contains("SET COMPRESSION lz4"))
        .expect("ddl::generate must emit lock-free LZ4 TOAST compression statement")
        .clone();

    let mut conn = PgConnection::connect(db.url())
        .await
        .expect("migrator connect");
    let server_supports_lz4: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
           SELECT 1 FROM pg_catalog.pg_settings \
            WHERE name = 'default_toast_compression' AND 'lz4' = ANY(enumvals) \
         )",
    )
    .fetch_one(&mut conn)
    .await
    .expect("check server LZ4 support");

    // 1. Fresh install as non-superuser migrator.
    run_statements_tx(&mut conn, &stmts)
        .await
        .expect("non-superuser DDL setup must succeed");

    let mut admin = PgConnection::connect(&admin_db_url)
        .await
        .expect("admin db connect");

    let compression_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text, a.attcompression::text \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
           JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
          WHERE n.nspname = 'auditstream' \
            AND c.relkind IN ('r', 'p') \
            AND a.attname IN ('before', 'after') \
            AND a.attnum > 0 AND NOT a.attisdropped \
          ORDER BY c.relname, a.attname",
    )
    .fetch_all(&mut admin)
    .await
    .expect("query attcompression");
    assert!(
        compression_rows.len() >= 12,
        "expected parent + default + 4 monthly partitions x 2 cols, got {compression_rows:?}"
    );
    if server_supports_lz4 {
        for (rel, col, comp) in &compression_rows {
            assert_eq!(
                comp, "l",
                "expected attcompression='l' on {rel}.{col}, got {comp:?}"
            );
        }

        // Verify PostgreSQL actually compresses large JSONB payloads with LZ4.
        admin
            .execute(
                "INSERT INTO auditstream.changes (source, op, after, db_user, schema_version) \
                 VALUES ('app', 'lz4.probe', jsonb_build_object('pad', repeat('abcdefghij', 500)), 'postgres', 1)",
            )
            .await
            .expect("insert large payload");
        let actual_codec: Option<String> = sqlx::query_scalar(
            "SELECT pg_catalog.pg_column_compression(after) \
               FROM auditstream.changes WHERE op = 'lz4.probe'",
        )
        .fetch_one(&mut admin)
        .await
        .expect("check pg_column_compression");
        assert_eq!(actual_codec.as_deref(), Some("lz4"));

        // Verify partitions::ensure_ahead inherits attcompression = 'l' automatically.
        let report = auditstream_pg::partitions::ensure_ahead(db.pool(), DEFAULT_SCHEMA, 4)
            .await
            .expect("ensure_ahead month +4");
        assert_eq!(report.created.len(), 1);
        let month4_name = report.created[0].partition_name();
        let month4_comps: Vec<String> = sqlx::query_scalar(
            "SELECT a.attcompression::text \
               FROM pg_catalog.pg_class c \
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
              WHERE n.nspname = 'auditstream' AND c.relname = $1 \
                AND a.attname IN ('before', 'after') AND a.attnum > 0 AND NOT a.attisdropped",
        )
        .bind(&month4_name)
        .fetch_all(&mut admin)
        .await
        .expect("query month +4 compression");
        assert_eq!(month4_comps, vec!["l", "l"]);
    } else {
        for (rel, col, comp) in &compression_rows {
            assert_ne!(
                comp, "l",
                "non-LZ4 server must not set attcompression='l' on {rel}.{col}"
            );
        }
    }

    // 2. Historical partition (< current UTC month) must stay untouched and NEVER be locked,
    //    even while an upgrade migration sets LZ4 on parent, default, and current/future months.
    //    Also test child-only drift (parent already 'l' while `changes_default` has 'p') to verify
    //    parent `changes` is locked FIRST (`AccessExclusiveLock`) before altering any child.
    admin
        .execute(
            "CREATE TABLE auditstream.changes_2025_01 PARTITION OF auditstream.changes \
               FOR VALUES FROM ('2025-01-01 00:00:00+00') TO ('2025-02-01 00:00:00+00'); \
             ALTER TABLE ONLY auditstream.changes_2025_01 \
               ALTER COLUMN before SET COMPRESSION default, \
               ALTER COLUMN after SET COMPRESSION default; \
             ALTER TABLE auditstream.changes_2025_01 OWNER TO auditstream_owner; \
             REVOKE ALL ON TABLE auditstream.changes_2025_01 FROM PUBLIC CASCADE; \
             ALTER TABLE ONLY auditstream.changes_default \
               ALTER COLUMN after SET COMPRESSION pglz;",
        )
        .await
        .expect("seed historical partition and child-only compression drift");

    if server_supports_lz4 {
        let mut child_only_tx = conn.begin().await.expect("begin child-only heal tx");
        run_statements(&mut child_only_tx, &stmts)
            .await
            .expect("child-only compression drift heals cleanly");
        let child_heal_locks: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname || ': ' || l.mode FROM pg_catalog.pg_locks l \
               JOIN pg_catalog.pg_class c ON c.oid = l.relation \
              WHERE l.pid = pg_catalog.pg_backend_pid() \
                AND c.relnamespace = 'auditstream'::pg_catalog.regnamespace \
                AND l.mode = 'AccessExclusiveLock' \
              ORDER BY 1",
        )
        .fetch_all(&mut *child_only_tx)
        .await
        .expect("read child-only heal locks");
        assert!(
            child_heal_locks.contains(&"changes: AccessExclusiveLock".to_owned()),
            "parent `changes` must acquire AccessExclusiveLock before altering child partition to prevent child->parent deadlocks, got: {child_heal_locks:?}"
        );
        assert!(
            child_heal_locks.contains(&"changes_default: AccessExclusiveLock".to_owned()),
            "drifted child `changes_default` must be altered, got: {child_heal_locks:?}"
        );
        assert!(
            !child_heal_locks
                .iter()
                .any(|entry| entry.starts_with("changes_2025_01:")),
            "historical partition must never be locked, got: {child_heal_locks:?}"
        );
        child_only_tx
            .commit()
            .await
            .expect("commit child-only heal tx");
    }

    admin
        .execute(
            "ALTER TABLE ONLY auditstream.changes \
               ALTER COLUMN before SET COMPRESSION pglz; \
             ALTER TABLE ONLY auditstream.changes_default \
               ALTER COLUMN after SET COMPRESSION pglz;",
        )
        .await
        .expect("seed parent + child pre-LZ4 compression");

    let mut hist_blocker = admin.begin().await.expect("begin historical lock tx");
    hist_blocker
        .execute("LOCK TABLE auditstream.changes_2025_01 IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("hold ACCESS EXCLUSIVE on historical partition");

    {
        let mut upgrade_tx = conn.begin().await.expect("begin upgrade tx");
        upgrade_tx
            .execute("SET LOCAL lock_timeout = '500ms'")
            .await
            .expect("set lock_timeout");
        run_statements(&mut upgrade_tx, &stmts)
            .await
            .expect("upgrade must not lock historical partition changes_2025_01");
        upgrade_tx.commit().await.expect("commit upgrade tx");
    }
    hist_blocker
        .rollback()
        .await
        .expect("release historical lock");

    let hist_comps: Vec<String> = sqlx::query_scalar(
        "SELECT a.attcompression::text \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
           JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
          WHERE n.nspname = 'auditstream' AND c.relname = 'changes_2025_01' \
            AND a.attname IN ('before', 'after') AND a.attnum > 0 AND NOT a.attisdropped",
    )
    .fetch_all(&mut admin)
    .await
    .expect("query historical partition compression");
    assert_eq!(
        hist_comps,
        vec!["", ""],
        "historical partition attcompression must stay untouched"
    );

    if server_supports_lz4 {
        let upgraded_parent: Vec<String> = sqlx::query_scalar(
            "SELECT a.attcompression::text \
               FROM pg_catalog.pg_class c \
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
              WHERE n.nspname = 'auditstream' \
                AND c.relname IN ('changes', 'changes_default') \
                AND a.attname IN ('before', 'after') AND a.attnum > 0 AND NOT a.attisdropped",
        )
        .fetch_all(&mut admin)
        .await
        .expect("query upgraded parent/default compression");
        assert_eq!(upgraded_parent, vec!["l", "l", "l", "l"]);
    }

    // 3. Re-running DDL a second time must execute zero ALTER TABLE commands and hold zero
    //    blocking locks on auditstream.*.
    admin
        .execute(
            "CREATE FUNCTION reject_alter_on_rerun() RETURNS event_trigger LANGUAGE plpgsql \
             AS $$ BEGIN RAISE EXCEPTION 'unexpected % on rerun', tg_tag; END $$; \
             CREATE EVENT TRIGGER reject_alter_on_rerun ON ddl_command_start \
             WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION reject_alter_on_rerun()",
        )
        .await
        .expect("install ALTER TABLE event trigger");

    {
        let mut rerun_tx = conn.begin().await.expect("begin rerun tx");
        run_statements(&mut rerun_tx, &stmts)
            .await
            .expect("second DDL run must execute zero ALTER TABLE commands");
        let blocking: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname || ': ' || l.mode FROM pg_catalog.pg_locks l \
               JOIN pg_catalog.pg_class c ON c.oid = l.relation \
              WHERE l.pid = pg_catalog.pg_backend_pid() \
                AND c.relnamespace = 'auditstream'::pg_catalog.regnamespace \
                AND l.mode IN ('ShareLock', 'ShareRowExclusiveLock', 'ExclusiveLock', 'AccessExclusiveLock') \
              ORDER BY 1",
        )
        .fetch_all(&mut *rerun_tx)
        .await
        .expect("read rerun locks");
        assert!(
            blocking.is_empty(),
            "second DDL run must hold zero blocking locks, got: {blocking:?}"
        );
        rerun_tx.commit().await.expect("commit rerun tx");
    }

    // 4. Simulate a PostgreSQL server compiled without `--with-lz4`: resetting `before` to pglz
    //    and probing a missing codec in `enumvals` must return lock-free before any ALTER TABLE.
    admin
        .execute("DROP EVENT TRIGGER reject_alter_on_rerun")
        .await
        .expect("temporarily drop event trigger");
    admin
        .execute("ALTER TABLE ONLY auditstream.changes ALTER COLUMN before SET COMPRESSION pglz")
        .await
        .expect("reset before to pglz");
    admin
        .execute(
            "CREATE EVENT TRIGGER reject_alter_on_rerun ON ddl_command_start \
             WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION reject_alter_on_rerun()",
        )
        .await
        .expect("reinstall event trigger");

    let non_lz4_probe = toast_stmt.replacen("'lz4' = ANY", "'__no_lz4__' = ANY", 1);
    assert_ne!(
        non_lz4_probe, toast_stmt,
        "toast_compression statement must check `'lz4' = ANY(enumvals)`"
    );
    {
        let mut no_lz4_tx = conn.begin().await.expect("begin non-lz4 tx");
        no_lz4_tx
            .execute(AssertSqlSafe(non_lz4_probe))
            .await
            .expect("non-LZ4 server path must return before acquiring any ALTER TABLE lock");
        let held_locks: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname || ': ' || l.mode FROM pg_catalog.pg_locks l \
               JOIN pg_catalog.pg_class c ON c.oid = l.relation \
              WHERE l.pid = pg_catalog.pg_backend_pid() \
                AND c.relnamespace = 'auditstream'::pg_catalog.regnamespace \
              ORDER BY 1",
        )
        .fetch_all(&mut *no_lz4_tx)
        .await
        .expect("check non-lz4 locks");
        assert!(
            held_locks.is_empty(),
            "non-LZ4 capability guard must acquire zero locks on auditstream.*, got: {held_locks:?}"
        );
        no_lz4_tx.commit().await.expect("commit non-lz4 tx");
    }
    let before_comp: String = sqlx::query_scalar(
        "SELECT a.attcompression::text \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
           JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
          WHERE n.nspname = 'auditstream' AND c.relname = 'changes' AND a.attname = 'before'",
    )
    .fetch_one(&mut admin)
    .await
    .expect("read before compression");
    assert_eq!(before_comp, "p");

    admin.close().await.expect("close admin");
    conn.close().await.expect("close migrator");
    drop(db);
    let mut cleanup = PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    let _ = cleanup
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(OWNER_ROLE)
        )))
        .await;
    let _ = cleanup.close().await;
}
