//! Comprehensive PostgreSQL capture integration suite (D6).
//!
//! Ports handbook test-kit scenarios T1–T9 (matching `expected_capture.txt` with
//! `spec/transport.md` rules for malformed payloads) plus nested writes (writable CTE,
//! PL/pgSQL function, multi-statement transaction), 10,000-row bulk updates and
//! multi-row inserts/deletes, no-PK and composite-PK tables, partitioned tables
//! (pinning direct-partition bypass behavior reported by D7), `ON CONFLICT DO UPDATE`
//! upserts, and DDL schema evolution (`DROP COLUMN`, `RENAME COLUMN`, `ALTER COLUMN TYPE`)
//! across PostgreSQL 15–19.

use auditstream_core::{
    Actor, ActorType, Context, decode, encode, encode_with_context, tag, tag_with_context,
};
use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
use auditstream_pg::introspect;
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_pg::verify::{self, FindingKind, Severity};
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

    async fn exec_ddl(&self, stmts: &[&str]) {
        for sql in stmts {
            self.db
                .execute(sql)
                .await
                .unwrap_or_else(|e| panic!("DDL failed: {e}\nSQL: {sql}"));
        }
    }

    async fn run_dml(&mut self, stmts: &[&str]) {
        for entry in stmts {
            let (sql, comment) = if let Some(s) = entry.strip_prefix("U: ") {
                (s, COMMENT_USER_42)
            } else if let Some(s) = entry.strip_prefix("K: ") {
                (s, COMMENT_API_KEY_9)
            } else if let Some(s) = entry.strip_prefix("A: ") {
                (s, COMMENT_AI_AGENT)
            } else {
                panic!("run_dml entry must start with 'U: ', 'K: ', or 'A: ': {entry}");
            };
            self.exec_app(&format!("{sql} {comment}")).await;
        }
    }

    async fn changes_since(&mut self, after_id: i64) -> Vec<CapturedChange> {
        let rows: Vec<RawChangeRow> = sqlx::query_as(
            "SELECT schema_name, table_name, op, record_id::text AS record_id, \
                    before::text AS before, after::text AS after, changed, \
                    actor->>'id' AS actor_id, actor_source \
               FROM auditstream.changes WHERE id > $1 ORDER BY id ASC",
        )
        .bind(after_id)
        .fetch_all(&mut self.app_conn)
        .await
        .expect("query captured changes");

        rows.into_iter()
            .map(|mut row| {
                assert_eq!(row.schema_name, "public", "unexpected schema_name");
                if let Some(cols) = &mut row.changed {
                    cols.sort();
                }
                CapturedChange {
                    table: row.table_name,
                    op: row.op,
                    record_id: row.record_id.as_deref().map(parse_json),
                    before: row.before.as_deref().map(parse_json),
                    after: row.after.as_deref().map(parse_json),
                    changed: row.changed,
                    actor_id: row.actor_id,
                    actor_source: row.actor_source,
                }
            })
            .collect()
    }

    async fn reinstall_triggers(&self, specs: &[TableTriggerSpec]) {
        let stmts = triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, specs.to_vec()));
        let mut tx = self.db.pool().begin().await.expect("begin trigger tx");
        for stmt in stmts {
            tx.execute(AssertSqlSafe(stmt))
                .await
                .expect("D3 trigger reinstall");
        }
        tx.commit().await.expect("commit trigger tx");
    }

    async fn assert_verify_clean(&self, specs: &[TableTriggerSpec]) {
        let report = verify::check(self.db.pool(), specs)
            .await
            .expect("verify::check");
        assert!(
            report.is_clean(),
            "expected clean verify, got: {:?}",
            report.findings
        );
    }

    /// Asserts captured rows after `after_id` against compact
    /// `table|op|record_id|before|after|changed|actor` specs, parsing every JSON field via
    /// workspace `serde_json` (`arbitrary_precision` enabled) so exact numeric lexemes, JSON
    /// types, and full `before`/`after` key sets are checked.
    async fn assert_changes_since(&mut self, after_id: i64, specs: &[&str]) {
        let actual = self.changes_since(after_id).await;
        let expected: Vec<CapturedChange> = specs
            .iter()
            .map(|line| {
                let p: Vec<&str> = line.split('|').collect();
                assert_eq!(p.len(), 7, "bad change spec: {line}");
                CapturedChange {
                    table: p[0].to_owned(),
                    op: p[1].to_owned(),
                    record_id: Some(parse_json(p[2])),
                    before: (p[3] != "-").then(|| parse_json(p[3])),
                    after: (p[4] != "-").then(|| parse_json(p[4])),
                    changed: (p[5] != "-").then(|| p[5].split(',').map(str::to_owned).collect()),
                    actor_id: Some(p[6].to_owned()),
                    actor_source: Some("comment".to_owned()),
                }
            })
            .collect();
        assert_eq!(actual, expected);
    }

    async fn assert_invalid_exclusion(
        &self,
        specs: &[TableTriggerSpec],
        fq_table: &str,
        col: &str,
    ) {
        let report = verify::check(self.db.pool(), specs)
            .await
            .expect("verify::check");
        assert!(
            report.has_errors() && report.findings.len() == 5,
            "got: {:?}",
            report.findings
        );
        let expected_objects = std::iter::once(fq_table.to_owned()).chain(
            AUDIT_TRIGGERS
                .into_iter()
                .map(|t| format!("{fq_table}.{t}")),
        );
        for obj in expected_objects {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.kind == FindingKind::InvalidExclusion
                        && f.severity == Severity::Error
                        && f.table.as_deref() == Some(fq_table)
                        && f.object == obj
                        && f.detail.contains(col)),
                "expected InvalidExclusion on {obj} for {col}, got: {:?}",
                report.findings
            );
        }
    }

    async fn assert_outdated_pk(&self, specs: &[TableTriggerSpec], expected: &[(&str, &str)]) {
        let report = verify::check(self.db.pool(), specs)
            .await
            .expect("verify::check");
        assert!(
            report.has_errors() && report.findings.len() == expected.len() * 4,
            "got: {:?}",
            report.findings
        );
        for &(fq_table, pk_col) in expected {
            for trig in AUDIT_TRIGGERS {
                let obj = format!("{fq_table}.{trig}");
                assert!(
                    report
                        .findings
                        .iter()
                        .any(|f| f.kind == FindingKind::Outdated
                            && f.severity == Severity::Error
                            && f.table.as_deref() == Some(fq_table)
                            && f.object == obj
                            && f.detail.contains("arguments are")
                            && f.detail.contains(pk_col)),
                    "expected Outdated on {obj} mentioning {pk_col}, got: {:?}",
                    report.findings
                );
            }
        }
    }
}

const AUDIT_TRIGGERS: [&str; 4] = [
    "auditstream_insert",
    "auditstream_update",
    "auditstream_delete",
    "auditstream_truncate",
];

#[derive(sqlx::FromRow)]
struct RawChangeRow {
    schema_name: String,
    table_name: String,
    op: String,
    record_id: Option<String>,
    before: Option<String>,
    after: Option<String>,
    changed: Option<Vec<String>>,
    actor_id: Option<String>,
    actor_source: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct CapturedChange {
    table: String,
    op: String,
    record_id: Option<Value>,
    before: Option<Value>,
    after: Option<Value>,
    changed: Option<Vec<String>>,
    actor_id: Option<String>,
    actor_source: Option<String>,
}

fn parse_json(raw: &str) -> Value {
    serde_json::from_str(raw).expect("valid jsonb text")
}

fn public_spec(table: &str, pk: &[&str], ex: &[&str]) -> TableTriggerSpec {
    TableTriggerSpec::new(
        "public",
        table,
        pk.iter().map(|s| (*s).to_owned()).collect(),
        ex.iter().map(|s| (*s).to_owned()).collect(),
    )
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
async fn partitioned_table_in_private_schema_captures_dml_without_public_usage() {
    let schema_sql = [
        "CREATE SCHEMA billing",
        "REVOKE ALL ON SCHEMA billing FROM PUBLIC",
        "CREATE TABLE billing.ledger ( \
           id int NOT NULL, \
           bucket int NOT NULL, \
           amount numeric NOT NULL, \
           PRIMARY KEY (id, bucket) \
         ) PARTITION BY LIST (bucket)",
        "CREATE TABLE billing.ledger_0 PARTITION OF billing.ledger FOR VALUES IN (0)",
    ];
    let spec = TableTriggerSpec::new(
        "billing",
        "ledger",
        vec!["id".to_owned(), "bucket".to_owned()],
        vec![],
    )
    .partitioned();
    let mut fx = CaptureFixture::with_schema(
        &schema_sql,
        vec![spec.clone()],
        &["GRANT USAGE ON SCHEMA billing TO {app_role}"],
    )
    .await;

    // Writes by both app_role and superuser must succeed even though auditstream_owner has NO
    // USAGE privilege on schema `billing`.
    fx.exec_app(&format!(
        "INSERT INTO billing.ledger VALUES (1, 0, 100) {COMMENT_USER_42}"
    ))
    .await;
    fx.exec_app(&format!(
        "UPDATE billing.ledger SET amount = 250 WHERE id = 1 AND bucket = 0 {COMMENT_API_KEY_9}"
    ))
    .await;
    fx.db
        .execute("DELETE FROM billing.ledger WHERE id = 1 AND bucket = 0")
        .await
        .expect("superuser DELETE on private-schema partitioned table succeeds");
    fx.exec_app(&format!("TRUNCATE billing.ledger {COMMENT_AI_AGENT}"))
        .await;

    let ops: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT schema_name, table_name, op FROM auditstream.changes ORDER BY id ASC",
    )
    .fetch_all(&mut fx.app_conn)
    .await
    .expect("read private-schema partitioned changes");
    assert_eq!(
        ops,
        vec![
            ("billing".into(), "ledger".into(), "INSERT".into()),
            ("billing".into(), "ledger".into(), "UPDATE".into()),
            ("billing".into(), "ledger".into(), "DELETE".into()),
            ("billing".into(), "ledger".into(), "TRUNCATE".into()),
        ]
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

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn comment_transport_hardening_resolves_line_comments_embedded_tags_and_long_sqlcommenter() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    let real_actor = Actor::new(ActorType::User, "usr_real");
    let fake_encoded = encode(&Actor::new(ActorType::User, "usr_spoof")).expect("encode spoof");
    let ctx = Context {
        request_id: Some("req_153".to_owned()),
        source: Some("api".to_owned()),
        ..Context::default()
    };

    // 1. Queries tagged via `auditstream_core::tag_with_context()` with trailing `--` comments,
    //    embedded `,auditstream='...'` values, malformed comments, and long sqlcommenter blocks
    //    MUST all execute cleanly on PostgreSQL and resolve `usr_real` (`actor_source = 'comment'`).
    let real_encoded = encode_with_context(&real_actor, Some(&ctx)).expect("encode real + ctx");
    // Exact 4,096-char merged comment overhead: `/*auditstream='<E>',z_trace=''*/;`
    let merged_z_overhead =
        4 + "auditstream='".len() + real_encoded.len() + "',z_trace='".len() + 1 + 1;
    let exact_z_len = 4096 - merged_z_overhead;
    let long_z_exact = "z".repeat(exact_z_len);
    let long_z_over = "z".repeat(exact_z_len + 1);
    let long_z_utf8_exact = "日".repeat(exact_z_len);
    let long_z_utf8_over = "日".repeat(exact_z_len + 1);
    let evicted_z_pad = "z".repeat(4100);

    // Explicitly verify exact-boundary merge vs >4096 standalone append before executing in PG.
    let exact_ascii_sql =
        format!("UPDATE invoice SET total = 215 WHERE id = 1 /*z_trace='{long_z_exact}'*/;");
    let exact_ascii_tagged =
        tag_with_context(&exact_ascii_sql, &real_actor, Some(&ctx)).expect("merge exact ascii");
    assert!(
        exact_ascii_tagged.contains(&format!("/*auditstream='{real_encoded}',z_trace='")),
        "exact 4,096-char ASCII sqlcommenter must merge"
    );
    let exact_utf8_sql =
        format!("UPDATE invoice SET total = 217 WHERE id = 1 /*z_trace='{long_z_utf8_exact}'*/;");
    let exact_utf8_tagged =
        tag_with_context(&exact_utf8_sql, &real_actor, Some(&ctx)).expect("merge exact utf8");
    assert!(
        exact_utf8_tagged.contains(&format!("/*auditstream='{real_encoded}',z_trace='")),
        "exact 4,096-char multi-byte UTF-8 sqlcommenter must merge"
    );
    let over_ascii_sql =
        format!("UPDATE invoice SET total = 216 WHERE id = 1 /*z_trace='{long_z_over}'*/;");
    let over_ascii_tagged =
        tag_with_context(&over_ascii_sql, &real_actor, Some(&ctx)).expect("append over ascii");
    assert_eq!(
        over_ascii_tagged,
        format!("{over_ascii_sql} /*auditstream='{real_encoded}'*/")
    );

    let tagged_cases = [
        // Trailing `--` single-line comments (need `\n` before `/*auditstream='...'*/`).
        "UPDATE invoice SET total = 201 WHERE id = 1 -- user note".to_owned(),
        "UPDATE invoice SET total = 202 WHERE id = 1 --".to_owned(),
        format!(
            "UPDATE invoice SET total = 203 WHERE id = 1 -- /*auditstream='{fake_encoded}'*/"
        ),
        format!(
            "UPDATE invoice SET total = 204 WHERE id = 1 /*auditstream='{fake_encoded}'*/ -- trailing"
        ),
        "UPDATE invoice SET status = E'a'\n'b\\' ' || 'x', total = 205 WHERE id = 1 -- after quotecontinue"
            .to_owned(),
        "UPDATE invoice SET status = $tag$-- inside$tag$, total = 206 WHERE id = 1 -- after dollar quote"
            .to_owned(),
        // Non-trailing `--` inside quotes/comments (normal space before `/*auditstream='...'*/`).
        "UPDATE invoice SET status = 'foo -- bar', total = 207 WHERE id = 1".to_owned(),
        "UPDATE invoice SET status = 'ok', total = 208 WHERE id = 1 /* -- in block */".to_owned(),
        // Embedded `,auditstream='...'`, malformed/freeform comments, and invalid/duplicate tags.
        "UPDATE invoice SET total = 209 WHERE id = 1 /*route='foo,auditstream=',other='bar'*/"
            .to_owned(),
        "UPDATE invoice SET total = 210 WHERE id = 1 /*a_route='foo,auditstream='*/;".to_owned(),
        format!(
            "UPDATE invoice SET total = 211 WHERE id = 1 /* free text ,auditstream='{fake_encoded}' */"
        ),
        format!(
            "UPDATE invoice SET total = 212 WHERE id = 1 /*auditstream='{fake_encoded}',garbage*/"
        ),
        "UPDATE invoice SET total = 213 WHERE id = 1 /*auditstream='!!!notbase64'*/".to_owned(),
        format!(
            "UPDATE invoice SET total = 214 WHERE id = 1 /*auditstream='{fake_encoded}',auditstream='{fake_encoded}'*/"
        ),
        // Long existing sqlcommenter keys at exact 4,096 (merged) and 4,097 (standalone append).
        exact_ascii_sql,
        over_ascii_sql,
        exact_utf8_sql,
        format!("UPDATE invoice SET total = 218 WHERE id = 1 /*z_trace='{long_z_utf8_over}'*/;"),
        format!(
            "UPDATE invoice SET total = 219 WHERE id = 1 /*auditstream='{fake_encoded}',z_pad='{evicted_z_pad}'*/"
        ),
        // Star-adjacent block comments (`RETURNING */*...*/`) must append standalone, not merge/skip.
        "UPDATE invoice SET total = 220 WHERE id = 1 RETURNING */*traceparent='00-abc'*/;"
            .to_owned(),
        format!(
            "UPDATE invoice SET total = 221 WHERE id = 1 RETURNING */*auditstream='{fake_encoded}'*/;"
        ),
    ];

    for (idx, raw_sql) in tagged_cases.iter().enumerate() {
        let tagged = tag_with_context(raw_sql, &real_actor, Some(&ctx))
            .unwrap_or_else(|e| panic!("case {idx} tag failed: {e}"));
        assert_eq!(
            decode(&tagged).into_actor().expect("decode tagged"),
            Some(real_actor.clone()),
            "case {idx} Rust decode mismatch for {tagged}"
        );
        fx.exec_app(&tagged).await;
    }

    let parse_opt_json = |raw: Option<String>| -> Option<Value> {
        raw.as_deref()
            .map(|s| serde_json::from_str::<Value>(s).expect("valid jsonb text"))
    };

    let captured: Vec<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT actor::text, context::text, actor_source \
           FROM auditstream.changes \
          WHERE id > $1 AND table_name = 'invoice' \
          ORDER BY id ASC",
    )
    .bind(start_id)
    .fetch_all(&mut fx.app_conn)
    .await
    .expect("read hardened comment rows");

    assert_eq!(captured.len(), tagged_cases.len());
    for (idx, (actor, context, source)) in captured.into_iter().enumerate() {
        assert_eq!(
            parse_opt_json(actor),
            Some(json!({"type": "user", "id": "usr_real"})),
            "case {idx} actor"
        );
        assert_eq!(
            parse_opt_json(context),
            Some(json!({"requestId": "req_153", "source": "api"})),
            "case {idx} context"
        );
        assert_eq!(source.as_deref(), Some("comment"), "case {idx} source");
    }

    // 2. Untagged queries with embedded `,auditstream='...'` or free-form comments fall through
    //    to JWT cleanly in PostgreSQL `resolve_actor()`, while duplicate `auditstream` keys yield
    //    `actor_source = 'invalid'` without falling through.
    let jwt_start = fx.max_change_id().await;
    let mut tx = fx.app_conn.begin().await.expect("begin jwt tx");
    sqlx::query(
        "SELECT pg_catalog.set_config('request.jwt.claims', '{\"sub\":\"jwt_user_153\"}', true)",
    )
    .execute(&mut *tx)
    .await
    .expect("set jwt claims");

    tx.execute(
        "UPDATE invoice SET total = 301 WHERE id = 1 /*route='foo,auditstream=',other='bar'*/",
    )
    .await
    .expect("embedded auditstream falls through to jwt");
    tx.execute(AssertSqlSafe(format!(
        "UPDATE invoice SET total = 302 WHERE id = 1 /* free text ,auditstream='{fake_encoded}' */"
    )))
    .await
    .expect("freeform comment falls through to jwt");
    tx.execute(AssertSqlSafe(format!(
        "UPDATE invoice SET total = 303 WHERE id = 1 /*auditstream='{fake_encoded}',auditstream='{fake_encoded}'*/"
    )))
    .await
    .expect("duplicate auditstream yields invalid");
    tx.execute(AssertSqlSafe(format!(
        "UPDATE invoice SET total = 304 WHERE id = 1 RETURNING id AS /*auditstream='{fake_encoded}'*/ v;"
    )))
    .await
    .expect("mid-query comment before identifier v falls through to jwt");
    tx.execute(AssertSqlSafe(format!(
        "UPDATE invoice SET total = 305 WHERE id = 1 /* outer /* inner */*auditstream='{fake_encoded}'*/"
    )))
    .await
    .expect("nested block comment star-adjacent spoof falls through to jwt");
    // Exact 4,096-char boundary nested-comment spoof (`/` of inner `*/` is char 4,096 from end
    // -> `open_pos = 1`, while `*` of inner `*/` is char 4,097 from end).
    let boundary_after_slash = format!("*auditstream='{fake_encoded}'*/");
    let boundary_spaces = " ".repeat(4096 - 1 - boundary_after_slash.len());
    tx.execute(AssertSqlSafe(format!(
        "UPDATE invoice SET total = 306 WHERE id = 1 /* /* */{boundary_after_slash}{boundary_spaces}"
    )))
    .await
    .expect("exact 4096-boundary nested spoof falls through to jwt");
    tx.commit().await.expect("commit jwt tx");

    let jwt_rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT actor::text, actor_source \
           FROM auditstream.changes \
          WHERE id > $1 AND table_name = 'invoice' \
          ORDER BY id ASC",
    )
    .bind(jwt_start)
    .fetch_all(&mut fx.app_conn)
    .await
    .expect("read jwt precedence rows");

    let parsed_jwt_rows: Vec<(Option<Value>, Option<String>)> = jwt_rows
        .into_iter()
        .map(|(a, src)| (parse_opt_json(a), src))
        .collect();
    assert_eq!(
        parsed_jwt_rows,
        vec![
            (
                Some(json!({"type": "user", "id": "jwt_user_153"})),
                Some("jwt".to_owned()),
            ),
            (
                Some(json!({"type": "user", "id": "jwt_user_153"})),
                Some("jwt".to_owned()),
            ),
            (None, Some("invalid".to_owned())),
            (
                Some(json!({"type": "user", "id": "jwt_user_153"})),
                Some("jwt".to_owned()),
            ),
            (
                Some(json!({"type": "user", "id": "jwt_user_153"})),
                Some("jwt".to_owned()),
            ),
            (
                Some(json!({"type": "user", "id": "jwt_user_153"})),
                Some("jwt".to_owned()),
            ),
        ]
    );

    // 3. Valid Drizzle-escaped tag, earlier `,auditstream=` value before a real `auditstream` key,
    //    and trailing ASCII SQL whitespace (form feed 0x0C on PG 15+, plus vertical tab 0x0B on
    //    PG 17+ where `scan.l` added `\v` to SQL whitespace) all resolve cleanly in PostgreSQL.
    let pg_version_num: i32 =
        sqlx::query_scalar("SELECT pg_catalog.current_setting('server_version_num')::int")
            .fetch_one(&mut fx.app_conn)
            .await
            .expect("read server_version_num");
    let mut extra_valid_queries = vec![
        format!(
            "UPDATE invoice SET total = 307 WHERE id = 1 /*route='O\\'Reilly',auditstream='{fake_encoded}'*/"
        ),
        format!(
            "UPDATE invoice SET total = 308 WHERE id = 1 /*a_route='foo,auditstream=',auditstream='{fake_encoded}'*/"
        ),
        format!(
            "UPDATE invoice SET total = 309 WHERE id = 1 /*auditstream='{fake_encoded}'*/ \t\r\n\x0c;"
        ),
    ];
    if pg_version_num >= 170_000 {
        extra_valid_queries.push(format!(
            "UPDATE invoice SET total = 310 WHERE id = 1 /*auditstream='{fake_encoded}'*/ \t\r\n\x0c\x0b;"
        ));
    }
    let extra_start = fx.max_change_id().await;
    for sql in &extra_valid_queries {
        assert_eq!(tag(sql, &real_actor).expect("keep valid tag"), *sql);
        fx.exec_app(sql).await;
    }
    let extra_rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT actor::text, actor_source \
           FROM auditstream.changes \
          WHERE id > $1 AND table_name = 'invoice' \
          ORDER BY id ASC",
    )
    .bind(extra_start)
    .fetch_all(&mut fx.app_conn)
    .await
    .expect("read extra valid rows");
    assert_eq!(extra_rows.len(), extra_valid_queries.len());
    for (idx, (actor_txt, src)) in extra_rows.into_iter().enumerate() {
        assert_eq!(
            parse_opt_json(actor_txt),
            Some(json!({"type": "user", "id": "usr_spoof"})),
            "extra case {idx} actor"
        );
        assert_eq!(src.as_deref(), Some("comment"), "extra case {idx} source");
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn merge_and_subtransaction_rollback_capture_only_committed_dml_with_actor() {
    let mut fx = CaptureFixture::kit_default().await;
    let start_id = fx.seed_invoices_for_update_tests().await;

    // 1. MERGE executing INSERT, real UPDATE, excluded-only no-op UPDATE, and DELETE in one
    //    statement with a trailing SQL comment actor.
    fx.exec_app(&format!(
        "MERGE INTO invoice AS t \
         USING (VALUES \
           (1, 'merged', 111::numeric, 's1', 'upd'), \
           (2, 'open',    50::numeric, 's2_rot', 'upd'), \
           (3, 'gone',    10::numeric, 's3', 'del'), \
           (4, 'fresh',   40::numeric, 's4', 'ins') \
         ) AS s(id, status, total, password_hash, act) \
         ON t.id = s.id \
         WHEN MATCHED AND s.act = 'del' THEN DELETE \
         WHEN MATCHED AND s.act = 'upd' THEN UPDATE SET status = s.status, total = s.total, password_hash = s.password_hash \
         WHEN NOT MATCHED THEN INSERT (id, status, total, password_hash) VALUES (s.id, s.status, s.total, s.password_hash) \
         {COMMENT_USER_42}"
    ))
    .await;

    let merge_comment_rows = fx.kit_projection_since(start_id).await;
    assert_eq!(
        merge_comment_rows.len(),
        3,
        "MERGE must capture 1 INSERT + 1 real UPDATE + 1 DELETE and skip excluded-only UPDATE: {merge_comment_rows:?}"
    );
    assert_statement_set(
        &merge_comment_rows,
        &[
            "invoice|INSERT|4|user|usr_42|comment|false",
            "invoice|UPDATE|1|{total,status}|user|usr_42|comment|false",
            "invoice|DELETE|3|user|usr_42|comment",
        ],
    );

    // 2. MERGE with transaction-local set_config actor.
    let merge_sc_start = fx.max_change_id().await;
    let mut tx = fx.app_conn.begin().await.expect("begin merge sc tx");
    sqlx::query(
        "SELECT pg_catalog.set_config('auditstream.actor', '{\"type\":\"service\",\"id\":\"merge-worker\"}', true)",
    )
    .execute(&mut *tx)
    .await
    .expect("set_config merge-worker");
    tx.execute(
        "MERGE INTO invoice AS t \
         USING (VALUES (4, 'settled', 45::numeric), (5, 'new5', 55::numeric)) AS s(id, status, total) \
         ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET status = s.status, total = s.total \
         WHEN NOT MATCHED THEN INSERT (id, status, total, password_hash) VALUES (s.id, s.status, s.total, 's5')",
    )
    .await
    .expect("merge with set_config");
    tx.commit().await.expect("commit merge sc tx");

    assert_statement_set(
        &fx.kit_projection_since(merge_sc_start).await,
        &[
            "invoice|UPDATE|4|{total,status}|service|merge-worker|set_config|false",
            "invoice|INSERT|5|service|merge-worker|set_config|false",
        ],
    );

    // 3. PL/pgSQL BEGIN..EXCEPTION subtransaction rollback: DML inside the aborted inner block
    //    must leave zero rows in `auditstream.changes`, while outer and recovery DML capture
    //    cleanly with the exact outer actor (tested for both trailing comment and set_config).
    let subtx_comment_start = fx.max_change_id().await;
    fx.exec_app(&format!(
        "DO $$ \
         DECLARE dummy int; \
         BEGIN \
           INSERT INTO public.invoice VALUES (60, 'before_subtx', 1, 's60'); \
           BEGIN \
             INSERT INTO public.invoice VALUES (61, 'rolled_back', 2, 's61'); \
             UPDATE public.invoice SET status = 'rolled_back_upd' WHERE id = 60; \
             DELETE FROM public.invoice WHERE id = 5; \
             dummy := 1 / 0; \
           EXCEPTION WHEN division_by_zero THEN \
             UPDATE public.invoice SET status = 'recovered' WHERE id = 60; \
           END; \
         END $$ {COMMENT_AI_AGENT}"
    ))
    .await;

    assert_eq!(
        fx.kit_projection_since(subtx_comment_start).await,
        vec![
            "invoice|INSERT|60|ai_agent|billing-agent|usr_42|comment|false",
            "invoice|UPDATE|60|{status}|ai_agent|billing-agent|usr_42|comment|false",
        ]
    );

    // Verify rolled-back writes truly did not take effect on `invoice` either.
    let subtx_ids: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM public.invoice WHERE id IN (5, 60, 61) ORDER BY id ASC")
            .fetch_all(&mut fx.app_conn)
            .await
            .expect("check surviving rows after subtx rollback");
    assert_eq!(subtx_ids, vec![5, 60]);

    // 4. Subtransaction rollback with `set_config` changed inside the aborted block: rolling back
    //    the subtransaction must restore the outer `auditstream.actor` GUC too.
    let subtx_sc_start = fx.max_change_id().await;
    let mut tx = fx.app_conn.begin().await.expect("begin subtx sc");
    sqlx::query(
        "SELECT pg_catalog.set_config('auditstream.actor', '{\"type\":\"user\",\"id\":\"outer_usr\"}', true)",
    )
    .execute(&mut *tx)
    .await
    .expect("set outer actor");
    tx.execute(
        "DO $$ \
         DECLARE dummy int; \
         BEGIN \
           BEGIN \
             PERFORM pg_catalog.set_config('auditstream.actor', '{bad_json', true); \
             UPDATE public.invoice SET status = 'aborted' WHERE id = 60; \
             dummy := 1 / 0; \
           EXCEPTION WHEN division_by_zero THEN \
             UPDATE public.invoice SET status = 'after_subtx_sc' WHERE id = 60; \
           END; \
         END $$",
    )
    .await
    .expect("run subtx sc block");
    tx.commit().await.expect("commit subtx sc");

    assert_eq!(
        fx.kit_projection_since(subtx_sc_start).await,
        vec!["invoice|UPDATE|60|{status}|user|outer_usr|set_config|false"]
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn ddl_drop_column_captures_insert_update_delete_for_normal_and_excluded_columns() {
    let schema = [
        "CREATE TABLE public.invoice (id int PRIMARY KEY, status text NOT NULL, memo text, \
         total numeric NOT NULL, password_hash text, secret_token text)",
        "CREATE TABLE public.tag (id int PRIMARY KEY, label text NOT NULL, legacy_code text)",
    ];
    let inv_spec = public_spec("invoice", &["id"], &["password_hash", "secret_token"]);
    let tag_spec = public_spec("tag", &["id"], &[]);
    let mut fx =
        CaptureFixture::with_schema(&schema, vec![inv_spec.clone(), tag_spec.clone()], &[]).await;

    // Warm up cached trigger plans on `fx.app_conn` before DDL (4 changes per table).
    fx.run_dml(&[
        "U: INSERT INTO public.invoice VALUES (1,'open','note-1',100,'s1','t1'),(2,'open','note-2',50,'s2','t2')",
        "U: UPDATE public.invoice SET status = 'review' WHERE id = 1",
        "U: DELETE FROM public.invoice WHERE id = 2",
        "U: INSERT INTO public.tag VALUES (1,'alpha','L1'),(2,'beta','L2')",
        "U: UPDATE public.tag SET label = 'alpha-1' WHERE id = 1",
        "U: DELETE FROM public.tag WHERE id = 2",
    ])
    .await;
    let after_warmup = fx.max_change_id().await;
    assert_eq!(after_warmup, 8, "warmup must capture 8 rows");

    // 1. DROP normal columns on exclusion (`invoice.memo`) and fast-path (`tag.legacy_code`) tables.
    fx.exec_ddl(&[
        "ALTER TABLE public.invoice DROP COLUMN memo",
        "ALTER TABLE public.tag DROP COLUMN legacy_code",
    ])
    .await;
    fx.assert_verify_clean(&[inv_spec.clone(), tag_spec.clone()])
        .await;
    fx.run_dml(&[
        "U: INSERT INTO public.tag (id,label) VALUES (3,'gamma')",
        "K: UPDATE public.tag SET label = 'alpha-2' WHERE id = 1",
        "K: UPDATE public.tag SET label = 'alpha-2' WHERE id = 1",
        "A: DELETE FROM public.tag WHERE id = 3",
        "U: INSERT INTO public.invoice (id,status,total,password_hash,secret_token) VALUES (3,'fresh',25,'s3','t3')",
        "K: UPDATE public.invoice SET status = 'paid' WHERE id = 1",
        "K: UPDATE public.invoice SET password_hash = 's1-rot' WHERE id = 1",
        "A: DELETE FROM public.invoice WHERE id = 3",
    ])
    .await;
    fx.assert_changes_since(
        after_warmup,
        &[
            r#"tag|INSERT|{"id":3}|-|{"id":3,"label":"gamma"}|-|usr_42"#,
            r#"tag|UPDATE|{"id":1}|{"id":1,"label":"alpha-1"}|{"id":1,"label":"alpha-2"}|label|key_9"#,
            r#"tag|DELETE|{"id":3}|{"id":3,"label":"gamma"}|-|-|billing-agent"#,
            r#"invoice|INSERT|{"id":3}|-|{"id":3,"status":"fresh","total":25}|-|usr_42"#,
            r#"invoice|UPDATE|{"id":1}|{"id":1,"status":"review","total":100}|{"id":1,"status":"paid","total":100}|status|key_9"#,
            r#"invoice|DELETE|{"id":3}|{"id":3,"status":"fresh","total":25}|-|-|billing-agent"#,
        ],
    )
    .await;

    // 2. DROP one of two excluded columns (`ex_len > 1`): zero trigger error, `secret_token` stays excluded.
    let after_normal = fx.max_change_id().await;
    fx.exec_ddl(&["ALTER TABLE public.invoice DROP COLUMN password_hash"])
        .await;
    fx.run_dml(&[
        "U: INSERT INTO public.invoice (id,status,total,secret_token) VALUES (4,'open',75,'t4')",
        "K: UPDATE public.invoice SET total = 80 WHERE id = 1",
        "K: UPDATE public.invoice SET total = 80, secret_token = 't1-rot' WHERE id = 1",
        "A: DELETE FROM public.invoice WHERE id = 4",
    ])
    .await;
    fx.assert_changes_since(
        after_normal,
        &[
            r#"invoice|INSERT|{"id":4}|-|{"id":4,"status":"open","total":75}|-|usr_42"#,
            r#"invoice|UPDATE|{"id":1}|{"id":1,"status":"paid","total":100}|{"id":1,"status":"paid","total":80}|total|key_9"#,
            r#"invoice|DELETE|{"id":4}|{"id":4,"status":"open","total":75}|-|-|billing-agent"#,
        ],
    )
    .await;
    fx.assert_invalid_exclusion(
        &[inv_spec, tag_spec.clone()],
        "public.invoice",
        "password_hash",
    )
    .await;

    // 3. Heal to `ex_len = 1` (`secret_token`), then DROP the sole excluded column and heal to `[]`.
    let single_ex = public_spec("invoice", &["id"], &["secret_token"]);
    fx.reinstall_triggers(&[single_ex.clone(), tag_spec.clone()])
        .await;
    let after_single = fx.max_change_id().await;
    fx.exec_ddl(&["ALTER TABLE public.invoice DROP COLUMN secret_token"])
        .await;
    fx.run_dml(&[
        "U: INSERT INTO public.invoice (id,status,total) VALUES (5,'solo',90)",
        "K: UPDATE public.invoice SET total = 95 WHERE id = 1",
        "K: UPDATE public.invoice SET total = 95 WHERE id = 1",
        "A: DELETE FROM public.invoice WHERE id = 5",
    ])
    .await;
    fx.assert_changes_since(
        after_single,
        &[
            r#"invoice|INSERT|{"id":5}|-|{"id":5,"status":"solo","total":90}|-|usr_42"#,
            r#"invoice|UPDATE|{"id":1}|{"id":1,"status":"paid","total":80}|{"id":1,"status":"paid","total":95}|total|key_9"#,
            r#"invoice|DELETE|{"id":5}|{"id":5,"status":"solo","total":90}|-|-|billing-agent"#,
        ],
    )
    .await;
    fx.assert_invalid_exclusion(
        &[single_ex, tag_spec.clone()],
        "public.invoice",
        "secret_token",
    )
    .await;

    let healed = vec![public_spec("invoice", &["id"], &[]), tag_spec];
    fx.reinstall_triggers(&healed).await;
    fx.assert_verify_clean(&healed).await;
    let after_heal = fx.max_change_id().await;
    fx.run_dml(&["U: INSERT INTO public.invoice (id,status,total) VALUES (6,'fast',60)"])
        .await;
    fx.assert_changes_since(
        after_heal,
        &[r#"invoice|INSERT|{"id":6}|-|{"id":6,"status":"fast","total":60}|-|usr_42"#],
    )
    .await;

    // Pre-DDL history snapshots remain immutable.
    let memo: Option<String> = sqlx::query_scalar(
        "SELECT after->>'memo' FROM auditstream.changes \
          WHERE table_name = 'invoice' AND op = 'INSERT' AND record_id = '{\"id\": 1}'::jsonb",
    )
    .fetch_one(&mut fx.app_conn)
    .await
    .expect("read historical pre-DDL snapshot");
    assert_eq!(memo.as_deref(), Some("note-1"));
}

#[tokio::test]
async fn ddl_rename_normal_and_excluded_columns_pins_runtime_capture_and_drift_contract() {
    let schema = [
        "CREATE TABLE public.invoice (id int PRIMARY KEY, status text NOT NULL, \
         total numeric NOT NULL, password_hash text, secret_token text)",
    ];
    let spec = public_spec("invoice", &["id"], &["password_hash", "secret_token"]);
    let mut fx = CaptureFixture::with_schema(&schema, vec![spec.clone()], &[]).await;

    fx.run_dml(&[
        "U: INSERT INTO public.invoice VALUES (1,'open',100,'s1','t1'),(2,'open',50,'s2','t2')",
        "U: UPDATE public.invoice SET status = 'review' WHERE id = 1",
        "U: DELETE FROM public.invoice WHERE id = 2",
    ])
    .await;
    let after_warmup = fx.max_change_id().await;
    assert_eq!(after_warmup, 4, "warmup must capture 4 rows");

    // (a) Non-PK, non-excluded rename (`status` -> `state`): reflected immediately, verify stays clean.
    fx.exec_ddl(&["ALTER TABLE public.invoice RENAME COLUMN status TO state"])
        .await;
    fx.assert_verify_clean(std::slice::from_ref(&spec)).await;
    fx.run_dml(&[
        "U: INSERT INTO public.invoice (id,state,total,password_hash,secret_token) VALUES (3,'fresh',30,'s3','t3')",
        "K: UPDATE public.invoice SET state = 'paid' WHERE id = 1",
        "K: UPDATE public.invoice SET state = 'paid', password_hash = 's1-rot' WHERE id = 1",
        "A: DELETE FROM public.invoice WHERE id = 3",
    ])
    .await;
    fx.assert_changes_since(
        after_warmup,
        &[
            r#"invoice|INSERT|{"id":3}|-|{"id":3,"state":"fresh","total":30}|-|usr_42"#,
            r#"invoice|UPDATE|{"id":1}|{"id":1,"state":"review","total":100}|{"id":1,"state":"paid","total":100}|state|key_9"#,
            r#"invoice|DELETE|{"id":3}|{"id":3,"state":"fresh","total":30}|-|-|billing-agent"#,
        ],
    )
    .await;

    // (b) Excluded-column rename (`password_hash` -> `secret_hash`) WITHOUT regenerating triggers.
    //     Pinned drift contract: writes never abort and `secret_token` stays excluded, but stale
    //     `TG_ARGV[1]` still names `password_hash`, so `secret_hash` IS captured in `before`/`after`
    //     and `changed` (including secret-only updates) until triggers regenerate.
    let after_normal = fx.max_change_id().await;
    fx.exec_ddl(&["ALTER TABLE public.invoice RENAME COLUMN password_hash TO secret_hash"])
        .await;
    fx.run_dml(&[
        "U: INSERT INTO public.invoice (id,state,total,secret_hash,secret_token) VALUES (4,'open',40,'top-secret-4','t4')",
        "K: UPDATE public.invoice SET secret_hash = 'rotated-4' WHERE id = 4",
        "K: UPDATE public.invoice SET secret_token = 't4-rot' WHERE id = 4",
        "A: DELETE FROM public.invoice WHERE id = 4",
    ])
    .await;
    fx.assert_changes_since(
        after_normal,
        &[
            r#"invoice|INSERT|{"id":4}|-|{"id":4,"state":"open","total":40,"secret_hash":"top-secret-4"}|-|usr_42"#,
            r#"invoice|UPDATE|{"id":4}|{"id":4,"state":"open","total":40,"secret_hash":"top-secret-4"}|{"id":4,"state":"open","total":40,"secret_hash":"rotated-4"}|secret_hash|key_9"#,
            r#"invoice|DELETE|{"id":4}|{"id":4,"state":"open","total":40,"secret_hash":"rotated-4"}|-|-|billing-agent"#,
        ],
    )
    .await;
    fx.assert_invalid_exclusion(&[spec], "public.invoice", "password_hash")
        .await;

    // Regenerating triggers with `secret_hash` restores redaction on INSERT, UPDATE, and DELETE.
    let updated = [public_spec(
        "invoice",
        &["id"],
        &["secret_hash", "secret_token"],
    )];
    fx.reinstall_triggers(&updated).await;
    fx.assert_verify_clean(&updated).await;
    let after_heal = fx.max_change_id().await;
    fx.run_dml(&[
        "U: INSERT INTO public.invoice (id,state,total,secret_hash,secret_token) VALUES (5,'open',55,'h5','t5')",
        "K: UPDATE public.invoice SET secret_hash = 'h5-rot' WHERE id = 5",
        "K: UPDATE public.invoice SET state = 'closed', secret_hash = 'h5-rot2' WHERE id = 5",
        "A: DELETE FROM public.invoice WHERE id = 5",
    ])
    .await;
    fx.assert_changes_since(
        after_heal,
        &[
            r#"invoice|INSERT|{"id":5}|-|{"id":5,"state":"open","total":55}|-|usr_42"#,
            r#"invoice|UPDATE|{"id":5}|{"id":5,"state":"open","total":55}|{"id":5,"state":"closed","total":55}|state|key_9"#,
            r#"invoice|DELETE|{"id":5}|{"id":5,"state":"closed","total":55}|-|-|billing-agent"#,
        ],
    )
    .await;
}

#[tokio::test]
async fn ddl_rename_primary_key_column_pins_null_record_id_drift_and_remediation() {
    let schema = [
        "CREATE TABLE public.invoice (id int PRIMARY KEY, status text NOT NULL)",
        "CREATE TABLE public.membership (tenant_id text NOT NULL, id int NOT NULL, role text NOT NULL, PRIMARY KEY (tenant_id, id))",
    ];
    let specs = vec![
        public_spec("invoice", &["id"], &[]),
        public_spec("membership", &["tenant_id", "id"], &[]),
    ];
    let mut fx = CaptureFixture::with_schema(&schema, specs, &[]).await;

    fx.run_dml(&[
        "U: INSERT INTO public.invoice VALUES (1,'open'),(2,'open')",
        "U: UPDATE public.invoice SET status = 'review' WHERE id = 1",
        "U: DELETE FROM public.invoice WHERE id = 2",
        "U: INSERT INTO public.membership VALUES ('acme',1,'member'),('acme',2,'guest')",
        "U: UPDATE public.membership SET role = 'admin' WHERE tenant_id = 'acme' AND id = 1",
        "U: DELETE FROM public.membership WHERE tenant_id = 'acme' AND id = 2",
    ])
    .await;
    let after_warmup = fx.max_change_id().await;
    assert_eq!(after_warmup, 8, "warmup must capture 8 rows");

    // (c) Primary-key column rename WITHOUT regenerating triggers.
    //     Pinned drift contract: writes never abort and `before`/`after`/`changed` use the renamed
    //     column, while stale `TG_ARGV[0]` produces non-NULL JSONB objects with JSON `null` for the
    //     renamed key (`{"id": null}` on single PK, `{"tenant_id": "acme", "id": null}` on composite PK).
    fx.exec_ddl(&[
        "ALTER TABLE public.invoice RENAME COLUMN id TO invoice_id",
        "ALTER TABLE public.membership RENAME COLUMN id TO item_id",
    ])
    .await;
    fx.run_dml(&[
        "U: INSERT INTO public.invoice (invoice_id,status) VALUES (10,'open')",
        "K: UPDATE public.invoice SET status = 'paid' WHERE invoice_id = 1",
        "K: UPDATE public.invoice SET status = 'paid' WHERE invoice_id = 1",
        "K: UPDATE public.invoice SET invoice_id = 101 WHERE invoice_id = 1",
        "A: DELETE FROM public.invoice WHERE invoice_id = 10",
        "U: INSERT INTO public.membership (tenant_id,item_id,role) VALUES ('acme',20,'viewer')",
        "K: UPDATE public.membership SET role = 'owner' WHERE tenant_id = 'acme' AND item_id = 1",
        "K: UPDATE public.membership SET role = 'owner' WHERE tenant_id = 'acme' AND item_id = 1",
        "K: UPDATE public.membership SET item_id = 101 WHERE tenant_id = 'acme' AND item_id = 1",
        "A: DELETE FROM public.membership WHERE tenant_id = 'acme' AND item_id = 20",
    ])
    .await;
    fx.assert_changes_since(
        after_warmup,
        &[
            r#"invoice|INSERT|{"id":null}|-|{"invoice_id":10,"status":"open"}|-|usr_42"#,
            r#"invoice|UPDATE|{"id":null}|{"invoice_id":1,"status":"review"}|{"invoice_id":1,"status":"paid"}|status|key_9"#,
            r#"invoice|UPDATE|{"id":null}|{"invoice_id":1,"status":"paid"}|{"invoice_id":101,"status":"paid"}|invoice_id|key_9"#,
            r#"invoice|DELETE|{"id":null}|{"invoice_id":10,"status":"open"}|-|-|billing-agent"#,
            r#"membership|INSERT|{"tenant_id":"acme","id":null}|-|{"tenant_id":"acme","item_id":20,"role":"viewer"}|-|usr_42"#,
            r#"membership|UPDATE|{"tenant_id":"acme","id":null}|{"tenant_id":"acme","item_id":1,"role":"admin"}|{"tenant_id":"acme","item_id":1,"role":"owner"}|role|key_9"#,
            r#"membership|UPDATE|{"tenant_id":"acme","id":null}|{"tenant_id":"acme","item_id":1,"role":"owner"}|{"tenant_id":"acme","item_id":101,"role":"owner"}|item_id|key_9"#,
            r#"membership|DELETE|{"tenant_id":"acme","id":null}|{"tenant_id":"acme","item_id":20,"role":"viewer"}|-|-|billing-agent"#,
        ],
    )
    .await;

    // Live introspection detects renamed PKs in key order; `verify::check` flags all 8 triggers Outdated.
    let updated_specs: Vec<TableTriggerSpec> = introspect::tables(fx.db.pool(), &["public"])
        .await
        .expect("introspect renamed PK tables")
        .into_iter()
        .map(|t| TableTriggerSpec::new(t.schema, t.name, t.primary_key, vec![]))
        .collect();
    assert_eq!(
        updated_specs,
        vec![
            public_spec("invoice", &["invoice_id"], &[]),
            public_spec("membership", &["tenant_id", "item_id"], &[]),
        ]
    );
    fx.assert_outdated_pk(
        &updated_specs,
        &[
            ("public.invoice", "invoice_id"),
            ("public.membership", "item_id"),
        ],
    )
    .await;

    fx.reinstall_triggers(&updated_specs).await;
    fx.assert_verify_clean(&updated_specs).await;
    let after_heal = fx.max_change_id().await;
    fx.run_dml(&[
        "U: INSERT INTO public.invoice (invoice_id,status) VALUES (11,'healed')",
        "K: UPDATE public.invoice SET status = 'done' WHERE invoice_id = 11",
        "A: DELETE FROM public.invoice WHERE invoice_id = 11",
        "U: INSERT INTO public.membership (tenant_id,item_id,role) VALUES ('acme',21,'healed')",
        "K: UPDATE public.membership SET role = 'done' WHERE tenant_id = 'acme' AND item_id = 21",
        "A: DELETE FROM public.membership WHERE tenant_id = 'acme' AND item_id = 21",
    ])
    .await;
    fx.assert_changes_since(
        after_heal,
        &[
            r#"invoice|INSERT|{"invoice_id":11}|-|{"invoice_id":11,"status":"healed"}|-|usr_42"#,
            r#"invoice|UPDATE|{"invoice_id":11}|{"invoice_id":11,"status":"healed"}|{"invoice_id":11,"status":"done"}|status|key_9"#,
            r#"invoice|DELETE|{"invoice_id":11}|{"invoice_id":11,"status":"done"}|-|-|billing-agent"#,
            r#"membership|INSERT|{"tenant_id":"acme","item_id":21}|-|{"tenant_id":"acme","item_id":21,"role":"healed"}|-|usr_42"#,
            r#"membership|UPDATE|{"tenant_id":"acme","item_id":21}|{"tenant_id":"acme","item_id":21,"role":"healed"}|{"tenant_id":"acme","item_id":21,"role":"done"}|role|key_9"#,
            r#"membership|DELETE|{"tenant_id":"acme","item_id":21}|{"tenant_id":"acme","item_id":21,"role":"done"}|-|-|billing-agent"#,
        ],
    )
    .await;
}

#[tokio::test]
async fn ddl_alter_column_type_captures_dml_with_changed_array_and_json_precision() {
    let schema = [
        "CREATE TABLE public.metric (id int PRIMARY KEY, count int NOT NULL, label text NOT NULL, \
         amount numeric(10, 2) NOT NULL, code int NOT NULL, secret text)",
        "CREATE TABLE public.counter (id int PRIMARY KEY, total int NOT NULL, name text NOT NULL)",
    ];
    let specs = vec![
        public_spec("metric", &["id"], &["secret"]),
        public_spec("counter", &["id"], &[]),
    ];
    let mut fx = CaptureFixture::with_schema(&schema, specs.clone(), &[]).await;

    // Warm up exclusion (`metric`) and fast-path (`counter`) plans; 6 UPDATEs exercise `gplan` invalidation.
    fx.run_dml(&[
        "U: INSERT INTO public.metric VALUES (1,10,'initial',100.25,10,'sec-1'),(2,20,'temp',200.50,20,'sec-2')",
        "U: UPDATE public.metric SET count = 11 WHERE id = 1",
        "U: DELETE FROM public.metric WHERE id = 2",
        "U: INSERT INTO public.counter VALUES (1,1,'c1'),(2,2,'c2')",
        "U: DELETE FROM public.counter WHERE id = 2",
    ])
    .await;
    for n in 2..=7 {
        fx.exec_app(&format!(
            "UPDATE public.counter SET total = {n} WHERE id = 1 {COMMENT_USER_42}"
        ))
        .await;
    }
    let after_warmup = fx.max_change_id().await;
    assert_eq!(after_warmup, 13, "warmup must capture 13 rows");

    // Alter PK (`int -> bigint`), normal int (`int -> bigint`), text (`text -> varchar(64)`),
    // fixed decimal (`numeric(10,2) -> numeric(18,6)`), and type category (`int -> text`).
    fx.exec_ddl(&[
        "ALTER TABLE public.metric ALTER COLUMN id TYPE bigint, ALTER COLUMN count TYPE bigint, \
         ALTER COLUMN label TYPE varchar(64), ALTER COLUMN amount TYPE numeric(18, 6), \
         ALTER COLUMN code TYPE text USING code::text",
        "ALTER TABLE public.counter ALTER COLUMN id TYPE bigint, \
         ALTER COLUMN total TYPE bigint, ALTER COLUMN name TYPE varchar(64)",
    ])
    .await;
    fx.assert_verify_clean(&specs).await;

    // Values beyond `i32::MAX` and `2^53 - 1` (JS `Number.MAX_SAFE_INTEGER`) plus an 18-digit
    // decimal that IEEE-754 `f64` would round; isolated last-digit diffs (+1 / +1e-6, sub-ULP in
    // `f64`) verify `v_after = v_before` exact numeric comparison.
    let label_64 = "a".repeat(64);
    let ins_metric = format!(
        "U: INSERT INTO public.metric (id,count,label,amount,code,secret) \
         VALUES (9007199254740993,9007199254740992,'{label_64}',123456789012.345678,'42-new','sec-new')"
    );
    fx.run_dml(&[
        "U: INSERT INTO public.counter VALUES (9007199254740993,9007199254740992,'c-big')",
        "K: UPDATE public.counter SET total = 9007199254740993, name = 'c-upd' WHERE id = 9007199254740993",
        "K: UPDATE public.counter SET total = 9007199254740993, name = 'c-upd' WHERE id = 9007199254740993",
        "A: DELETE FROM public.counter WHERE id = 1",
        &ins_metric,
        "K: UPDATE public.metric SET count = 9007199254740993 WHERE id = 9007199254740993",
        "K: UPDATE public.metric SET amount = 123456789012.345679 WHERE id = 9007199254740993",
        "K: UPDATE public.metric SET id = 9007199254740994, label = 'narrowed-ok', code = '42-v2' WHERE id = 9007199254740993",
        "K: UPDATE public.metric SET count = 9223372036854775807, code = '10-text' WHERE id = 1",
        "K: UPDATE public.metric SET label = 'narrowed-ok', secret = 'sec-rot' WHERE id = 9007199254740994",
        "A: DELETE FROM public.metric WHERE id = 9007199254740994",
    ])
    .await;

    let m0 = format!(
        r#"{{"id":9007199254740993,"count":9007199254740992,"label":"{label_64}","amount":123456789012.345678,"code":"42-new"}}"#
    );
    let m1 = format!(
        r#"{{"id":9007199254740993,"count":9007199254740993,"label":"{label_64}","amount":123456789012.345678,"code":"42-new"}}"#
    );
    let m2 = format!(
        r#"{{"id":9007199254740993,"count":9007199254740993,"label":"{label_64}","amount":123456789012.345679,"code":"42-new"}}"#
    );
    let m3 = r#"{"id":9007199254740994,"count":9007199254740993,"label":"narrowed-ok","amount":123456789012.345679,"code":"42-v2"}"#;
    let e_ins = format!(r#"metric|INSERT|{{"id":9007199254740993}}|-|{m0}|-|usr_42"#);
    let e_u1 = format!(r#"metric|UPDATE|{{"id":9007199254740993}}|{m0}|{m1}|count|key_9"#);
    let e_u2 = format!(r#"metric|UPDATE|{{"id":9007199254740993}}|{m1}|{m2}|amount|key_9"#);
    let e_u3 = format!(r#"metric|UPDATE|{{"id":9007199254740994}}|{m2}|{m3}|code,id,label|key_9"#);
    let e_del = format!(r#"metric|DELETE|{{"id":9007199254740994}}|{m3}|-|-|billing-agent"#);
    fx.assert_changes_since(
        after_warmup,
        &[
            r#"counter|INSERT|{"id":9007199254740993}|-|{"id":9007199254740993,"total":9007199254740992,"name":"c-big"}|-|usr_42"#,
            r#"counter|UPDATE|{"id":9007199254740993}|{"id":9007199254740993,"total":9007199254740992,"name":"c-big"}|{"id":9007199254740993,"total":9007199254740993,"name":"c-upd"}|name,total|key_9"#,
            r#"counter|DELETE|{"id":1}|{"id":1,"total":7,"name":"c1"}|-|-|billing-agent"#,
            &e_ins,
            &e_u1,
            &e_u2,
            &e_u3,
            r#"metric|UPDATE|{"id":1}|{"id":1,"count":11,"label":"initial","amount":100.250000,"code":"10"}|{"id":1,"count":9223372036854775807,"label":"initial","amount":100.250000,"code":"10-text"}|code,count|key_9"#,
            &e_del,
        ],
    )
    .await;
}
