//! Comprehensive PostgreSQL capture integration suite (D6).
//!
//! Ports handbook test-kit scenarios T1–T9 (matching `expected_capture.txt` with
//! `spec/transport.md` rules for malformed payloads) plus nested writes (writable CTE,
//! PL/pgSQL function, multi-statement transaction), 10,000-row bulk updates and
//! multi-row inserts/deletes, no-PK and composite-PK tables, partitioned tables
//! (pinning direct-partition bypass behavior reported by D7), and `ON CONFLICT DO UPDATE`
//! upserts across PostgreSQL 15–19.

use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_pg::verify::{self, FindingKind};
use auditstream_testkit::{ClusterLock, TestDb};
use serde_json::{Value, json};
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor, Row};
use uuid::Uuid;

const COMMENT_USER_42: &str = "/*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/";
const COMMENT_USER_42_AND_TRACEPARENT: &str = "/*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'*/";
const COMMENT_API_KEY_9: &str = "/*auditstream='eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ'*/";
const COMMENT_AI_AGENT: &str = "/*auditstream='eyJ0eXBlIjoiYWlfYWdlbnQiLCJpZCI6ImJpbGxpbmctYWdlbnQiLCJvbkJlaGFsZk9mIjp7InR5cGUiOiJ1c2VyIiwiaWQiOiJ1c3JfNDIifX0'*/";

/// Drops a unique per-test application role after the associated [`TestDb`] has been dropped.
struct AppRoleGuard {
    admin_url: String,
    role_name: String,
}

impl Drop for AppRoleGuard {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let role_name = self.role_name.clone();
        let handle = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
                    let _ = admin
                        .execute(AssertSqlSafe(format!(
                            "DROP ROLE IF EXISTS {}",
                            quote_ident(&role_name)
                        )))
                        .await;
                    let _ = admin.close().await;
                }
            });
        });
        let _ = handle.join();
    }
}

/// Isolated test database with D2 + D3 installed and a dedicated non-privileged app role.
///
/// Field declaration order ensures `app_conn` drops first, then `db` drops (force-dropping
/// the database and all objects owned by `auditstream_owner`), then `_app_role_guard` drops
/// the unique cluster role, and finally `_cluster_lock` releases the cluster advisory lock
/// so parallel tests that recreate `auditstream_owner` never race with live databases.
struct CaptureFixture {
    app_conn: PgConnection,
    db: TestDb,
    _app_role_guard: AppRoleGuard,
    _cluster_lock: ClusterLock,
}

impl CaptureFixture {
    /// Creates a fresh database with the test-kit `invoice` + `line_item` schema and triggers.
    async fn kit_default() -> Self {
        let schema_sql = [
            "CREATE TABLE public.invoice (\
                id int PRIMARY KEY, \
                status text, \
                total numeric, \
                password_hash text\
             )",
            "CREATE TABLE public.line_item (\
                id int PRIMARY KEY, \
                invoice_id int REFERENCES public.invoice(id) ON DELETE CASCADE, \
                qty int\
             )",
        ];
        let specs = vec![
            TableTriggerSpec::new(
                "public",
                "invoice",
                vec!["id".to_owned()],
                vec!["password_hash".to_owned()],
            ),
            TableTriggerSpec::new("public", "line_item", vec!["id".to_owned()], vec![]),
        ];
        Self::with_schema(&schema_sql, specs, &[]).await
    }

    async fn with_schema(
        schema_stmts: &[&str],
        specs: Vec<TableTriggerSpec>,
        extra_grants: &[&str],
    ) -> Self {
        // Hold the cluster advisory lock across the fixture lifetime so parallel tests
        // (such as `triggers` / `ddl` non-superuser role tests) cannot drop `auditstream_owner`
        // while this database still holds objects owned by it.
        let cluster_lock = TestDb::cluster_lock()
            .await
            .expect("acquire cluster lock for role setup");

        let db = TestDb::new().await.expect("create TestDb");
        let app_role = format!("app_{}", db.database_name());
        let app_pw = Uuid::new_v4().simple().to_string();
        let admin_url = db.admin_url().to_owned();

        {
            let mut admin = PgConnection::connect(&admin_url)
                .await
                .expect("admin connect");
            admin
                .execute(AssertSqlSafe(format!(
                    "CREATE ROLE {} LOGIN PASSWORD '{app_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE",
                    quote_ident(&app_role),
                )))
                .await
                .expect("create unique app role");
            admin.close().await.expect("close admin");

            let mut setup = PgConnection::connect(db.url())
                .await
                .expect("setup connect");
            for stmt in schema_stmts {
                setup
                    .execute(AssertSqlSafe((*stmt).to_owned()))
                    .await
                    .unwrap_or_else(|e| panic!("schema statement failed: {e}\nSQL: {stmt}"));
            }

            let ddl_stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]));
            {
                let mut tx = setup.begin().await.expect("begin DDL tx");
                for stmt in &ddl_stmts {
                    tx.execute(AssertSqlSafe(stmt.clone()))
                        .await
                        .unwrap_or_else(|e| panic!("D2 DDL failed: {e}\nSQL: {stmt}"));
                }
                tx.commit().await.expect("commit DDL tx");
            }

            let trigger_stmts =
                triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, specs.clone()));
            {
                let mut tx = setup.begin().await.expect("begin trigger tx");
                for stmt in &trigger_stmts {
                    tx.execute(AssertSqlSafe(stmt.clone()))
                        .await
                        .unwrap_or_else(|e| panic!("D3 trigger failed: {e}\nSQL: {stmt}"));
                }
                tx.commit().await.expect("commit trigger tx");
            }

            for spec in &specs {
                setup
                    .execute(AssertSqlSafe(format!(
                        "GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON {}.{} TO {}",
                        quote_ident(&spec.schema),
                        quote_ident(&spec.table),
                        quote_ident(&app_role),
                    )))
                    .await
                    .expect("grant DML on audited table to app role");
            }
            for grant_sql in extra_grants {
                let rendered = grant_sql.replace("{app_role}", &quote_ident(&app_role));
                setup
                    .execute(AssertSqlSafe(rendered))
                    .await
                    .expect("apply extra grant");
            }

            setup.close().await.expect("close setup");
        }

        let app_url = role_url(&db, &app_role, &app_pw);
        let app_conn = PgConnection::connect(&app_url)
            .await
            .expect("connect as application role");

        Self {
            app_conn,
            db,
            _app_role_guard: AppRoleGuard {
                admin_url,
                role_name: app_role,
            },
            _cluster_lock: cluster_lock,
        }
    }

    async fn exec_app(&mut self, sql: &str) {
        self.app_conn
            .execute(AssertSqlSafe(sql.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("app SQL failed: {e}\nSQL: {sql}"));
    }

    async fn max_change_id(&mut self) -> i64 {
        sqlx::query_scalar("SELECT coalesce(max(id), 0) FROM auditstream.changes")
            .fetch_one(&mut self.app_conn)
            .await
            .expect("read max change id")
    }

    /// Returns the exact test-kit projection (`docs/handbook/test-kit/sql/capture.sql`)
    /// for rows written after `after_id`, ordered by `id ASC`.
    async fn kit_projection_since(&mut self, after_id: i64) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT concat_ws(\
               '|', \
               table_name, \
               op, \
               record_id->>'id', \
               changed::text, \
               actor->>'type', \
               actor->>'id', \
               actor->'onBehalfOf'->>'id', \
               actor_source, \
               (after ? 'password_hash')::text\
             ) \
             FROM auditstream.changes \
            WHERE id > $1 \
              AND table_name IN ('invoice', 'line_item') \
            ORDER BY id ASC",
        )
        .bind(after_id)
        .fetch_all(&mut self.app_conn)
        .await
        .expect("query kit projection")
    }

    async fn seed_invoices_for_update_tests(&mut self) -> i64 {
        self.exec_app(&format!(
            "INSERT INTO invoice VALUES (1,'open',100,'s1'),(2,'open',50,'s2'),(3,'open',10,'s3') {COMMENT_USER_42}"
        ))
        .await;
        self.exec_app(&format!(
            "INSERT INTO line_item VALUES (10,1,1),(11,1,2) {COMMENT_USER_42}"
        ))
        .await;
        self.max_change_id().await
    }
}

fn role_url(db: &TestDb, role: &str, password: &str) -> String {
    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    format!("{scheme}://{role}:{password}@{host_and_db}")
}

/// Compares a slice of transition-table rows from one statement as an unordered multiset.
fn assert_statement_set(actual: &[String], expected: &[&str]) {
    let mut got: Vec<&str> = actual.iter().map(String::as_str).collect();
    let mut exp: Vec<&str> = expected.to_vec();
    got.sort_unstable();
    exp.sort_unstable();
    assert_eq!(got, exp);
}

#[tokio::test]
async fn t1_sqlcommenter_insert_with_traceparent_and_line_items() {
    let mut fx = CaptureFixture::kit_default().await;

    fx.exec_app(&format!(
        "INSERT INTO invoice VALUES (1,'open',100,'s1'),(2,'open',50,'s2'),(3,'open',10,'s3') {COMMENT_USER_42_AND_TRACEPARENT}"
    ))
    .await;
    let after_invoices = fx.max_change_id().await;

    fx.exec_app(&format!(
        "INSERT INTO line_item VALUES (10,1,1),(11,1,2) {COMMENT_USER_42}"
    ))
    .await;

    let all = fx.kit_projection_since(0).await;
    assert_eq!(all.len(), 5);
    assert_statement_set(
        &all[0..3],
        &[
            "invoice|INSERT|1|user|usr_42|comment|false",
            "invoice|INSERT|2|user|usr_42|comment|false",
            "invoice|INSERT|3|user|usr_42|comment|false",
        ],
    );
    assert_statement_set(
        &all[3..5],
        &[
            "line_item|INSERT|10|user|usr_42|comment|false",
            "line_item|INSERT|11|user|usr_42|comment|false",
        ],
    );

    // Ensure statement ordering: all invoice INSERT ids precede line_item INSERT ids.
    let line_item_min_id: i64 = sqlx::query_scalar(
        "SELECT min(id) FROM auditstream.changes WHERE table_name = 'line_item'",
    )
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("min line_item id");
    assert!(line_item_min_id > after_invoices);
}

#[tokio::test]
async fn t2_set_config_wins_over_comment() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    let mut tx = fx.app_conn.begin().await.expect("begin tx");
    sqlx::query(
        "SELECT pg_catalog.set_config('auditstream.actor', '{\"type\":\"system\",\"id\":\"nightly-job\"}', true)",
    )
    .execute(&mut *tx)
    .await
    .expect("set_config actor");
    tx.execute(AssertSqlSafe(format!(
        "UPDATE invoice SET status='paid' WHERE id=1 {COMMENT_API_KEY_9}"
    )))
    .await
    .expect("update with set_config and comment");
    tx.commit().await.expect("commit tx");

    let rows = fx.kit_projection_since(start_id).await;
    assert_eq!(
        rows,
        vec!["invoice|UPDATE|1|{status}|system|nightly-job|set_config|false"]
    );
}

#[tokio::test]
async fn t3_jwt_claims_fallback() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    let mut tx = fx.app_conn.begin().await.expect("begin tx");
    sqlx::query(
        "SELECT pg_catalog.set_config('request.jwt.claims', '{\"sub\":\"auth0|abc\",\"role\":\"authenticated\"}', true)",
    )
    .execute(&mut *tx)
    .await
    .expect("set_config jwt");
    tx.execute("UPDATE invoice SET total=55 WHERE id=2")
        .await
        .expect("update with jwt");
    tx.commit().await.expect("commit tx");

    let rows = fx.kit_projection_since(start_id).await;
    assert_eq!(
        rows,
        vec!["invoice|UPDATE|2|{total}|user|auth0|abc|jwt|false"]
    );
}

#[tokio::test]
async fn t4_spoof_comment_inside_string_literal_is_ignored() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    fx.exec_app(
        "UPDATE invoice SET status='/*auditstream=''eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ''*/' WHERE id=3",
    )
    .await;
    fx.exec_app(
        "UPDATE invoice SET status='x' WHERE status='/*auditstream=''eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ''*/'",
    )
    .await;

    let rows = fx.kit_projection_since(start_id).await;
    assert_eq!(
        rows,
        vec![
            "invoice|UPDATE|3|{status}|false",
            "invoice|UPDATE|3|{status}|false",
        ]
    );
}

#[tokio::test]
async fn t5_malformed_payloads_never_abort_write_and_record_invalid_source() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    // Spec/transport.md requires both invalid base64url chars (`!!!notbase64`) and
    // valid base64url of non-JSON (`bm90IGpzb24`) to record actor=NULL, actor_source='invalid'
    // without aborting the user's UPDATE.
    fx.exec_app("UPDATE invoice SET total=56 WHERE id=2 /*auditstream='!!!notbase64'*/")
        .await;
    fx.exec_app("UPDATE invoice SET total=57 WHERE id=2 /*auditstream='bm90IGpzb24'*/")
        .await;

    let total: String = sqlx::query_scalar("SELECT total::text FROM invoice WHERE id = 2")
        .fetch_one(&mut fx.app_conn)
        .await
        .expect("read final total");
    assert_eq!(total, "57");

    let rows = fx.kit_projection_since(start_id).await;
    assert_eq!(
        rows,
        vec![
            "invoice|UPDATE|2|{total}|invalid|false",
            "invoice|UPDATE|2|{total}|invalid|false",
        ]
    );
}

#[tokio::test]
async fn t6_agent_on_behalf_of_bulk_update_and_cascade_delete() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    fx.exec_app(&format!(
        "UPDATE invoice SET status='void' WHERE id IN (2,3) {COMMENT_AI_AGENT}"
    ))
    .await;
    let after_update = fx.max_change_id().await;

    fx.exec_app(&format!(
        "DELETE FROM invoice WHERE id=1 {COMMENT_AI_AGENT}"
    ))
    .await;

    let update_rows = fx.kit_projection_since(start_id).await;
    let mut up_slice = Vec::new();
    let mut del_slice = Vec::new();
    let detailed: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, concat_ws(\
           '|', table_name, op, record_id->>'id', changed::text, \
           actor->>'type', actor->>'id', actor->'onBehalfOf'->>'id', \
           actor_source, (after ? 'password_hash')::text\
         ) \
         FROM auditstream.changes \
        WHERE id > $1 AND table_name IN ('invoice', 'line_item') \
        ORDER BY id ASC",
    )
    .bind(start_id)
    .fetch_all(&mut fx.app_conn)
    .await
    .expect("query t6 rows");

    assert_eq!(update_rows.len(), 5);
    for (id, line) in detailed {
        if id <= after_update {
            up_slice.push(line);
        } else {
            del_slice.push(line);
        }
    }

    assert_statement_set(
        &up_slice,
        &[
            "invoice|UPDATE|2|{status}|ai_agent|billing-agent|usr_42|comment|false",
            "invoice|UPDATE|3|{status}|ai_agent|billing-agent|usr_42|comment|false",
        ],
    );
    assert_statement_set(
        &del_slice,
        &[
            "invoice|DELETE|1|ai_agent|billing-agent|usr_42|comment",
            "line_item|DELETE|10|ai_agent|billing-agent|usr_42|comment",
            "line_item|DELETE|11|ai_agent|billing-agent|usr_42|comment",
        ],
    );
}

#[tokio::test]
async fn t7_noop_and_excluded_column_only_updates_write_nothing_and_exclude_from_images() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    // No-op update and excluded-column-only update must write 0 history rows.
    fx.exec_app(&format!(
        "UPDATE invoice SET status=status WHERE id=2 {COMMENT_USER_42}"
    ))
    .await;
    fx.exec_app(&format!(
        "UPDATE invoice SET password_hash='rotated' WHERE id=2 {COMMENT_USER_42}"
    ))
    .await;
    assert_eq!(
        fx.kit_projection_since(start_id).await,
        Vec::<String>::new()
    );

    // A real update and delete must strip `password_hash` from both `before` and `after`.
    fx.exec_app(&format!(
        "UPDATE invoice SET status='changed', password_hash='secret2' WHERE id=2 {COMMENT_USER_42}"
    ))
    .await;
    fx.exec_app(&format!("DELETE FROM invoice WHERE id=2 {COMMENT_USER_42}"))
        .await;

    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) \
           FROM auditstream.changes \
          WHERE table_name = 'invoice' \
            AND (coalesce(before ? 'password_hash', false) OR coalesce(after ? 'password_hash', false))",
    )
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("check password_hash leakage");
    assert_eq!(leaked, 0);

    let changed_cols: Vec<String> = sqlx::query_scalar(
        "SELECT changed FROM auditstream.changes WHERE id > $1 AND op = 'UPDATE'",
    )
    .bind(start_id)
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("fetch changed array");
    assert_eq!(changed_cols, vec!["status"]);
}

#[tokio::test]
async fn updates_capture_changes_after_adding_a_json_column() {
    let mut fx = CaptureFixture::kit_default().await;
    fx.db
        .execute(
            "ALTER TABLE line_item \
             ADD COLUMN document json, \
             ADD COLUMN note xml, \
             ADD COLUMN coords point",
        )
        .await
        .expect("add non-equality columns after installing triggers");
    fx.exec_app(
        r#"INSERT INTO line_item (id, document, note, coords)
           VALUES (1, '{"n":1}', xmlparse(content '<a/>'), point '(1,2)')"#,
    )
    .await;
    // Semantically equivalent JSON formatting and unchanged xml/point are no-op updates
    // on a table without excluded columns and must write no history row.
    fx.exec_app(
        r#"UPDATE line_item
              SET document = '{ "n": 1 }',
                  note = note,
                  coords = coords
            WHERE id = 1"#,
    )
    .await;
    fx.exec_app(
        r#"UPDATE line_item
              SET document = '{"n":2}',
                  note = xmlparse(content '<b/>'),
                  coords = point '(3,4)'
            WHERE id = 1"#,
    )
    .await;

    let mut updates: Vec<(Vec<String>, String, String, String, String)> = sqlx::query_as(
        "SELECT changed, \
                before->'document'->>'n', after->'document'->>'n', \
                after->>'note', after->>'coords' \
         FROM auditstream.changes WHERE op = 'UPDATE'",
    )
    .fetch_all(fx.db.pool())
    .await
    .expect("read captured updates");
    for (changed, ..) in &mut updates {
        changed.sort();
    }
    assert_eq!(
        updates,
        [(
            vec!["coords".into(), "document".into(), "note".into()],
            "1".into(),
            "2".into(),
            "<b/>".into(),
            "(3,4)".into(),
        )]
    );
}

#[tokio::test]
async fn t8_truncate_records_table_and_actor() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    fx.exec_app(&format!("TRUNCATE line_item {COMMENT_USER_42}"))
        .await;

    let rows = fx.kit_projection_since(start_id).await;
    assert_eq!(rows, vec!["line_item|TRUNCATE|user|usr_42|comment"]);
}

#[tokio::test]
async fn t9_app_role_cannot_tamper_with_history_and_own_writes_are_captured() {
    let mut fx = CaptureFixture::kit_default().await;

    // Seed one history row via a normal app write first so UPDATE/DELETE target an existing row.
    fx.exec_app(&format!(
        "INSERT INTO invoice VALUES (1,'open',100,'s1') {COMMENT_USER_42}"
    ))
    .await;

    let tamper_stmts = [
        "INSERT INTO auditstream.changes(op) VALUES ('FAKE')",
        "UPDATE auditstream.changes SET actor = NULL",
        "DELETE FROM auditstream.changes",
        "TRUNCATE auditstream.changes",
    ];

    for stmt in tamper_stmts {
        let err = fx
            .app_conn
            .execute(AssertSqlSafe(stmt))
            .await
            .expect_err(&format!("expected permission denied for `{stmt}`"));
        let sqlstate = err
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code);
        assert_eq!(
            sqlstate.as_deref(),
            Some("42501"),
            "expected SQLSTATE 42501 for `{stmt}`, got: {err}"
        );
    }

    // Confirm subsequent app writes are still captured cleanly through SECURITY DEFINER.
    fx.exec_app(&format!(
        "UPDATE invoice SET status='paid' WHERE id=1 {COMMENT_API_KEY_9}"
    ))
    .await;

    let rows = fx.kit_projection_since(0).await;
    assert_eq!(
        rows,
        vec![
            "invoice|INSERT|1|user|usr_42|comment|false",
            "invoice|UPDATE|1|{status}|api_key|key_9|comment|false",
        ]
    );
}

#[tokio::test]
async fn nested_writes_capture_outer_actor_cte_plpgsql_and_multi_statement_tx() {
    let schema_sql = [
        "CREATE TABLE public.invoice (\
            id int PRIMARY KEY, \
            status text, \
            total numeric, \
            password_hash text\
         )",
        "CREATE TABLE public.line_item (\
            id int PRIMARY KEY, \
            invoice_id int REFERENCES public.invoice(id) ON DELETE CASCADE, \
            qty int\
         )",
        "CREATE FUNCTION public.create_invoice_with_item(\
            p_inv_id int, \
            p_item_id int\
         ) RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN \
           INSERT INTO public.invoice VALUES (p_inv_id, 'via_fn', 42, 'fn_secret'); \
           INSERT INTO public.line_item VALUES (p_item_id, p_inv_id, 3); \
         END $$",
    ];
    let specs = vec![
        TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["password_hash".to_owned()],
        ),
        TableTriggerSpec::new("public", "line_item", vec!["id".to_owned()], vec![]),
    ];
    let mut fx = CaptureFixture::with_schema(
        &schema_sql,
        specs,
        &["GRANT EXECUTE ON FUNCTION public.create_invoice_with_item(int, int) TO {app_role}"],
    )
    .await;

    // 1. Writable CTE spanning invoice and line_item.
    let cte_start = fx.max_change_id().await;
    fx.exec_app(&format!(
        "WITH ins AS (\
           INSERT INTO invoice VALUES (100, 'cte', 99, 'cte_secret') RETURNING id\
         ) \
         INSERT INTO line_item SELECT 1000, id, 5 FROM ins {COMMENT_USER_42}"
    ))
    .await;
    assert_statement_set(
        &fx.kit_projection_since(cte_start).await,
        &[
            "invoice|INSERT|100|user|usr_42|comment|false",
            "line_item|INSERT|1000|user|usr_42|comment|false",
        ],
    );

    // 2. PL/pgSQL function called from an outer statement carrying a trailing comment.
    let fn_start = fx.max_change_id().await;
    fx.exec_app(&format!(
        "SELECT public.create_invoice_with_item(200, 2000) {COMMENT_API_KEY_9}"
    ))
    .await;
    assert_statement_set(
        &fx.kit_projection_since(fn_start).await,
        &[
            "invoice|INSERT|200|api_key|key_9|comment|false",
            "line_item|INSERT|2000|api_key|key_9|comment|false",
        ],
    );

    // 3. Multi-statement transaction using transaction-local set_config.
    let tx_start = fx.max_change_id().await;
    let mut tx = fx.app_conn.begin().await.expect("begin tx");
    sqlx::query(
        "SELECT pg_catalog.set_config('auditstream.actor', '{\"type\":\"service\",\"id\":\"billing-worker\"}', true)",
    )
    .execute(&mut *tx)
    .await
    .expect("set_config service actor");
    tx.execute("INSERT INTO invoice VALUES (300, 'draft', 10, 'tx_secret')")
        .await
        .expect("tx insert invoice");
    tx.execute("INSERT INTO line_item VALUES (3000, 300, 1)")
        .await
        .expect("tx insert line_item");
    tx.execute("UPDATE invoice SET status = 'final' WHERE id = 300")
        .await
        .expect("tx update invoice");
    tx.commit().await.expect("commit tx");

    assert_eq!(
        fx.kit_projection_since(tx_start).await,
        vec![
            "invoice|INSERT|300|service|billing-worker|set_config|false",
            "line_item|INSERT|3000|service|billing-worker|set_config|false",
            "invoice|UPDATE|300|{status}|service|billing-worker|set_config|false",
        ]
    );
}

#[tokio::test]
async fn bulk_10000_row_update_and_multirow_insert_delete_capture_every_row_with_actor() {
    let mut fx = CaptureFixture::kit_default().await;

    // 1. Multi-row INSERT of 10,000 rows in one statement.
    let ins_start = fx.max_change_id().await;
    fx.exec_app(&format!(
        "INSERT INTO invoice (id, status, total, password_hash) \
         SELECT g, 'new', g, 'secret' FROM generate_series(1, 10000) AS g {COMMENT_USER_42}"
    ))
    .await;
    let (ins_count, ins_actor_matches): (i64, i64) = sqlx::query_as(
        "SELECT \
           count(*), \
           count(*) FILTER (WHERE op = 'INSERT' AND actor->>'id' = 'usr_42' AND actor_source = 'comment') \
         FROM auditstream.changes \
        WHERE id > $1 AND table_name = 'invoice'",
    )
    .bind(ins_start)
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("count bulk insert");
    assert_eq!(ins_count, 10_000);
    assert_eq!(ins_actor_matches, 10_000);

    // 2. 10,000-row UPDATE in one statement.
    let upd_start = fx.max_change_id().await;
    fx.exec_app(&format!(
        "UPDATE invoice SET status = 'bulk_updated' {COMMENT_API_KEY_9}"
    ))
    .await;
    let (upd_count, upd_actor_matches): (i64, i64) = sqlx::query_as(
        "SELECT \
           count(*), \
           count(*) FILTER (WHERE op = 'UPDATE' AND actor->>'id' = 'key_9' AND actor_source = 'comment') \
         FROM auditstream.changes \
        WHERE id > $1 AND table_name = 'invoice'",
    )
    .bind(upd_start)
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("count bulk update");
    assert_eq!(upd_count, 10_000);
    assert_eq!(upd_actor_matches, 10_000);

    // 3. Multi-row DELETE of 1,000 rows in one statement.
    let del_start = fx.max_change_id().await;
    fx.exec_app(&format!(
        "DELETE FROM invoice WHERE id <= 1000 {COMMENT_AI_AGENT}"
    ))
    .await;
    let (del_count, del_actor_matches): (i64, i64) = sqlx::query_as(
        "SELECT \
           count(*), \
           count(*) FILTER (WHERE op = 'DELETE' AND actor->>'id' = 'billing-agent' AND actor_source = 'comment') \
         FROM auditstream.changes \
        WHERE id > $1 AND table_name = 'invoice'",
    )
    .bind(del_start)
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("count bulk delete");
    assert_eq!(del_count, 1_000);
    assert_eq!(del_actor_matches, 1_000);
}

#[tokio::test]
async fn no_primary_key_and_composite_primary_key_tables_capture_defined_record_id() {
    let schema_sql = [
        "CREATE TABLE public.scratch_note (note text NOT NULL, seq int NOT NULL)",
        "CREATE TABLE public.membership (\
            tenant_id text NOT NULL, \
            user_id int NOT NULL, \
            role text NOT NULL, \
            PRIMARY KEY (tenant_id, user_id)\
         )",
    ];
    let specs = vec![
        TableTriggerSpec::new("public", "scratch_note", vec![], vec![]),
        TableTriggerSpec::new(
            "public",
            "membership",
            vec!["tenant_id".to_owned(), "user_id".to_owned()],
            vec![],
        ),
    ];
    let mut fx = CaptureFixture::with_schema(&schema_sql, specs, &[]).await;

    fx.exec_app(&format!(
        "INSERT INTO scratch_note VALUES ('hello', 1) {COMMENT_USER_42}"
    ))
    .await;
    fx.exec_app(&format!(
        "UPDATE scratch_note SET note = 'world' WHERE seq = 1 {COMMENT_USER_42}"
    ))
    .await;
    fx.exec_app(&format!(
        "DELETE FROM scratch_note WHERE seq = 1 {COMMENT_USER_42}"
    ))
    .await;

    fx.exec_app(&format!(
        "INSERT INTO membership VALUES ('acme', 7, 'member') {COMMENT_API_KEY_9}"
    ))
    .await;
    fx.exec_app(&format!(
        "UPDATE membership SET role = 'admin' WHERE tenant_id = 'acme' AND user_id = 7 {COMMENT_API_KEY_9}"
    ))
    .await;
    fx.exec_app(&format!(
        "DELETE FROM membership WHERE tenant_id = 'acme' AND user_id = 7 {COMMENT_API_KEY_9}"
    ))
    .await;

    let rows = sqlx::query(
        "SELECT table_name, op, record_id::text AS record_id, actor->>'id' AS actor_id \
           FROM auditstream.changes \
          ORDER BY id ASC",
    )
    .fetch_all(&mut fx.app_conn)
    .await
    .expect("read capture rows");

    assert_eq!(rows.len(), 6);
    for (idx, expected_op) in ["INSERT", "UPDATE", "DELETE"].iter().enumerate() {
        let table: String = rows[idx].get("table_name");
        let op: String = rows[idx].get("op");
        let record_id: Option<String> = rows[idx].get("record_id");
        assert_eq!(table, "scratch_note");
        assert_eq!(op, *expected_op);
        assert_eq!(record_id, None, "no-PK table must store NULL record_id");
    }

    for (offset, expected_op) in ["INSERT", "UPDATE", "DELETE"].iter().enumerate() {
        let row = &rows[3 + offset];
        let table: String = row.get("table_name");
        let op: String = row.get("op");
        let record_id: Option<String> = row.get("record_id");
        let parsed: Value =
            serde_json::from_str(record_id.as_deref().expect("composite PK record_id"))
                .expect("valid jsonb");
        assert_eq!(table, "membership");
        assert_eq!(op, *expected_op);
        assert_eq!(parsed, json!({"tenant_id": "acme", "user_id": 7}));
    }
}

#[tokio::test]
async fn partitioned_table_captures_parent_writes_and_pins_direct_partition_insert_gap() {
    let schema_sql = [
        "CREATE TABLE public.ledger (\
            id int NOT NULL, \
            bucket int NOT NULL, \
            amount int NOT NULL, \
            PRIMARY KEY (id, bucket)\
         ) PARTITION BY RANGE (bucket)",
        "CREATE TABLE public.ledger_p1 PARTITION OF public.ledger FOR VALUES FROM (0) TO (100)",
        "CREATE TABLE public.ledger_p2 PARTITION OF public.ledger FOR VALUES FROM (100) TO (200)",
    ];
    let spec = TableTriggerSpec::new(
        "public",
        "ledger",
        vec!["id".to_owned(), "bucket".to_owned()],
        vec![],
    )
    .partitioned();
    let mut fx = CaptureFixture::with_schema(
        &schema_sql,
        vec![spec.clone()],
        &["GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON public.ledger_p1, public.ledger_p2 TO {app_role}"],
    )
    .await;

    // 1. Writes through the parent table are captured under `ledger` (parent OID).
    fx.exec_app(&format!(
        "INSERT INTO public.ledger VALUES (1, 10, 500), (2, 150, 750) {COMMENT_USER_42}"
    ))
    .await;
    fx.exec_app(&format!(
        "UPDATE public.ledger SET amount = 600 WHERE id = 1 AND bucket = 10 {COMMENT_API_KEY_9}"
    ))
    .await;
    fx.exec_app(&format!(
        "DELETE FROM public.ledger WHERE id = 2 AND bucket = 150 {COMMENT_AI_AGENT}"
    ))
    .await;
    fx.exec_app(&format!("TRUNCATE public.ledger {COMMENT_USER_42}"))
        .await;

    let parent_oid: i64 = sqlx::query_scalar("SELECT 'public.ledger'::regclass::oid::bigint")
        .fetch_one(&mut fx.app_conn)
        .await
        .expect("parent oid");

    let parent_rows: Vec<(String, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT table_name, table_oid::bigint, op, actor->>'id' \
           FROM auditstream.changes \
          ORDER BY id ASC",
    )
    .fetch_all(&mut fx.app_conn)
    .await
    .expect("read parent rows");

    assert_eq!(parent_rows.len(), 5);
    for (table_name, table_oid, _, _) in &parent_rows {
        assert_eq!(table_name, "ledger");
        assert_eq!(*table_oid, parent_oid);
    }
    assert_eq!(parent_rows[0].2, "INSERT");
    assert_eq!(parent_rows[1].2, "INSERT");
    assert_eq!(parent_rows[2].2, "UPDATE");
    assert_eq!(parent_rows[2].3.as_deref(), Some("key_9"));
    assert_eq!(parent_rows[3].2, "DELETE");
    assert_eq!(parent_rows[3].3.as_deref(), Some("billing-agent"));
    assert_eq!(parent_rows[4].2, "TRUNCATE");

    // 2. Pin the documented PostgreSQL behavior: INSERT naming a child partition directly
    //    bypasses the parent's statement-level AFTER INSERT trigger.
    let before_direct = fx.max_change_id().await;
    fx.exec_app(&format!(
        "INSERT INTO public.ledger_p1 VALUES (99, 50, 123) {COMMENT_USER_42}"
    ))
    .await;
    let direct_captured: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auditstream.changes WHERE id > $1")
            .bind(before_direct)
            .fetch_one(&mut fx.app_conn)
            .await
            .expect("count direct partition inserts");
    assert_eq!(
        direct_captured, 0,
        "direct INSERT into a child partition bypasses parent statement-level trigger"
    );

    // 3. Confirm D7 (`verify::check`) reports this partitioned table so operators see the caveat.
    let report = verify::check(fx.db.pool(), &[spec])
        .await
        .expect("verify partitioned table");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::PartitionedTable
                && f.table.as_deref() == Some("public.ledger")),
        "expected PartitionedTable finding from verify::check, got: {:?}",
        report.findings
    );
    assert!(
        !report.has_errors(),
        "clean partitioned install should have warning only, no errors: {:?}",
        report.findings
    );
}

#[tokio::test]
async fn upsert_records_inserted_as_insert_and_changed_conflict_as_update_never_both() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    // Single INSERT ... ON CONFLICT DO UPDATE mixing:
    // - id = 1: conflicting row with changed status and total -> UPDATE
    // - id = 2: conflicting row with identical status and total -> skipped (no-op)
    // - id = 4: brand-new row -> INSERT
    fx.exec_app(&format!(
        "INSERT INTO invoice (id, status, total, password_hash) \
         VALUES (1, 'upserted', 150, 's1'), (2, 'open', 50, 's2'), (4, 'fresh', 30, 's4') \
         ON CONFLICT (id) DO UPDATE \
           SET status = EXCLUDED.status, total = EXCLUDED.total \
         {COMMENT_USER_42}"
    ))
    .await;

    let rows = fx.kit_projection_since(start_id).await;
    assert_eq!(
        rows.len(),
        2,
        "expected 1 INSERT + 1 UPDATE (no-op conflict skipped), got: {rows:?}"
    );
    assert_statement_set(
        &rows,
        &[
            "invoice|INSERT|4|user|usr_42|comment|false",
            "invoice|UPDATE|1|{total,status}|user|usr_42|comment|false",
        ],
    );

    // Double check: no single record_id produced both INSERT and UPDATE in the same upsert.
    let duplicate_ids: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (\
           SELECT record_id FROM auditstream.changes \
            WHERE id > $1 AND table_name = 'invoice' \
            GROUP BY record_id \
           HAVING count(*) > 1\
         ) dup",
    )
    .bind(start_id)
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("check no duplicate record_id per upsert");
    assert_eq!(duplicate_ids, 0);
}

#[tokio::test]
async fn owner_role_owns_history_and_trigger_functions_after_install() {
    let fx = CaptureFixture::kit_default().await;
    let owners: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, pg_catalog.pg_get_userbyid(c.relowner)::text \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'auditstream' AND c.relname = 'changes'",
    )
    .fetch_all(fx.db.pool())
    .await
    .expect("read changes owner");
    assert_eq!(owners, vec![("changes".to_owned(), OWNER_ROLE.to_owned())]);
}
