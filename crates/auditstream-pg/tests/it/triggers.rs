use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use auditstream_core::transport::{DecodedActor, comment, resolve_precedence};
use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_testkit::TestDb;
use serde::Deserialize;
use serde_json::Value;
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor, Row};
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

fn conformance_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("spec/conformance")
}

#[test]
fn generated_sql_snapshots_match_three_fixture_schemas_and_have_no_large_regex_quantifiers() {
    // Fixture schema 1: standard invoice + line_item (single PK + excluded column)
    let standard = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![
            TableTriggerSpec::new(
                "public",
                "invoice",
                vec!["id".to_owned()],
                vec!["password_hash".to_owned()],
            ),
            TableTriggerSpec::new("public", "line_item", vec!["id".to_owned()], vec![]),
        ],
    ));

    // Fixture schema 2: table without primary key
    let no_pk = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![TableTriggerSpec::new(
            "public",
            "scratch_log",
            vec![],
            vec![],
        )],
    ));

    // Fixture schema 3: composite PK + partitioned user table
    let composite_and_partitioned = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![
            TableTriggerSpec::new(
                "public",
                "project",
                vec!["tenant_id".to_owned(), "project_id".to_owned()],
                vec![],
            ),
            TableTriggerSpec::new(
                "public",
                "ledger",
                vec!["id".to_owned(), "bucket".to_owned()],
                vec![],
            )
            .partitioned(),
        ],
    ));

    // Verify deterministic output and no regex repetition count above 255 anywhere.
    for stmts in [&standard, &no_pk, &composite_and_partitioned] {
        for stmt in stmts {
            assert!(
                !stmt.contains("{1,683}") && !stmt.contains("{1,700}") && !stmt.contains("{0,683}"),
                "generated SQL must never use regex repetition counts > 255: {stmt}"
            );
        }
    }

    insta::assert_snapshot!(
        "triggers_standard_invoice_line_item",
        standard.join(";\n\n")
    );
    insta::assert_snapshot!("triggers_no_primary_key", no_pk.join(";\n\n"));
    insta::assert_snapshot!(
        "triggers_composite_and_partitioned",
        composite_and_partitioned.join(";\n\n")
    );
}

#[test]
fn generated_capture_and_resolve_actor_use_hot_path_optimizations() {
    let stmts = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec![],
        )],
    ));

    let resolve_actor = stmts
        .iter()
        .find(|s| s.starts_with("CREATE OR REPLACE FUNCTION \"auditstream\".resolve_actor("))
        .expect("resolve_actor statement");
    let capture = stmts
        .iter()
        .find(|s| s.starts_with("CREATE OR REPLACE FUNCTION \"auditstream\".capture()"))
        .expect("capture statement");

    // resolve_actor() avoids POSIX regexes on the hot path and caches base64url length.
    assert!(
        !resolve_actor.contains("regexp_match") && !resolve_actor.contains("!~"),
        "resolve_actor() should use pure string builtins instead of POSIX regexes:\n{resolve_actor}"
    );
    assert!(
        resolve_actor.contains("pg_catalog.rtrim(") && resolve_actor.contains("pg_catalog.ltrim("),
        "resolve_actor() should trim trailing whitespace and validate base64url via ltrim:\n{resolve_actor}"
    );

    // capture() uses single-column PK fast path and avoids empty TG_ARGV JSON array subqueries.
    assert!(
        capture.contains("pg_catalog.jsonb_build_object(pk_single"),
        "capture() should build single-column PK record_id via jsonb_build_object:\n{capture}"
    );
    assert!(
        capture.contains("TG_ARGV[0] <> '[]'") && capture.contains("TG_ARGV[1] <> '[]'"),
        "capture() should short-circuit empty TG_ARGV JSON arrays:\n{capture}"
    );
    // UPDATE checks excluded-column no-op before calling resolve_actor().
    let update_branch = capture
        .split("IF TG_OP = 'UPDATE' THEN")
        .nth(1)
        .expect("UPDATE branch");
    let noop_pos = update_branch
        .find("IF v_after = v_before THEN")
        .expect("excluded-column no-op guard in UPDATE branch");
    let resolve_pos = update_branch
        .find("resolve_actor()")
        .expect("resolve_actor call in UPDATE branch");
    assert!(
        noop_pos < resolve_pos,
        "UPDATE branch must check excluded-column no-op before invoking resolve_actor()"
    );
}

#[derive(Debug, Deserialize)]
struct CaptureSuite {
    cases: Vec<CaptureCase>,
}

#[derive(Debug, Deserialize)]
struct CaptureCase {
    name: String,
    input: CaptureInput,
    expected: CaptureExpected,
}

#[derive(Debug, Deserialize)]
struct CaptureInput {
    schema_sql: String,
    excluded_columns: BTreeMap<String, Vec<String>>,
    steps: Vec<CaptureStep>,
}

#[derive(Debug, Deserialize)]
struct CaptureStep {
    sql: String,
    role: String,
    set_config: Option<String>,
    jwt_claims: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CaptureExpected {
    rows: Vec<ExpectedCaptureRow>,
    #[serde(default)]
    step_errors: Vec<ExpectedStepError>,
}

#[derive(Debug, Deserialize)]
struct ExpectedCaptureRow {
    table_name: String,
    op: String,
    record_id: Option<Value>,
    changed: Option<Vec<String>>,
    actor: Option<Value>,
    actor_source: Option<String>,
    before_keys: Option<Vec<String>>,
    after_keys: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ExpectedStepError {
    step_index: usize,
    sqlstate: String,
}

async fn introspect_table_specs(
    conn: &mut PgConnection,
    excluded_columns: &BTreeMap<String, Vec<String>>,
) -> Vec<TableTriggerSpec> {
    let tables: Vec<(String, String, i8)> = sqlx::query_as(
        "SELECT n.nspname::text, c.relname::text, c.relkind::\"char\" \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' \
           AND c.relkind IN ('r', 'p') \
           AND NOT c.relispartition \
         ORDER BY c.relname",
    )
    .fetch_all(&mut *conn)
    .await
    .expect("introspect public tables");

    let mut specs = Vec::with_capacity(tables.len());
    for (schema, table, relkind) in tables {
        let pk_cols: Vec<String> = sqlx::query_scalar(
            "SELECT a.attname::text \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class c ON c.oid = i.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
             JOIN pg_catalog.unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) \
               ON a.attnum = k.attnum \
             WHERE n.nspname = $1 AND c.relname = $2 AND i.indisprimary \
             ORDER BY k.ord",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_all(&mut *conn)
        .await
        .expect("introspect primary key");

        let excluded = excluded_columns.get(&table).cloned().unwrap_or_default();
        let mut spec = TableTriggerSpec::new(schema, table, pk_cols, excluded);
        if relkind == b'p'.cast_signed() {
            spec = spec.partitioned();
        }
        specs.push(spec);
    }
    specs
}

fn sorted_object_keys(raw: Option<&str>) -> Option<Vec<String>> {
    raw.map(|s| {
        let val: Value = serde_json::from_str(s).expect("valid jsonb");
        let mut keys: Vec<String> = val
            .as_object()
            .expect("row image is JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    })
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn every_capture_conformance_case_passes_and_rerunning_triggers_is_idempotent() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let capture_files = ["capture/kit-t1-t9.json", "capture/edge-tables.json"];
    let mut total_cases = 0usize;

    for rel_path in capture_files {
        let raw = fs::read_to_string(conformance_dir().join(rel_path))
            .expect("read capture conformance fixture");
        let suite: CaptureSuite = serde_json::from_str(&raw).expect("parse capture suite");

        for case in suite.cases {
            total_cases += 1;

            let db = TestDb::non_superuser()
                .await
                .expect("create non-superuser db");
            let admin_url = db.admin_url().to_owned();
            let app_role = format!("app_{}", db.database_name());
            let app_pw = Uuid::new_v4().simple().to_string();

            {
                let mut admin = PgConnection::connect(&admin_url)
                    .await
                    .expect("admin connect");
                // Ensure auditstream_owner is created fresh by this non-superuser migrator
                // (so PostgreSQL 16+ grants ADMIN to the migrator rather than an earlier test role).
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

            let ddl_stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]));
            let trigger_stmts: Vec<String>;
            let table_specs: Vec<TableTriggerSpec>;

            {
                let mut migrator = PgConnection::connect(db.url())
                    .await
                    .expect("migrator connect");

                // 1. Apply history schema DDL (D2).
                run_statements(&mut migrator, &ddl_stmts)
                    .await
                    .expect("apply D2 history DDL");

                // 2. Apply case schema SQL and grant DML on all user tables to app_role.
                for stmt in case
                    .input
                    .schema_sql
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    migrator
                        .execute(AssertSqlSafe(stmt.to_owned()))
                        .await
                        .unwrap_or_else(|e| panic!("case `{}` schema SQL failed: {e}", case.name));
                }

                table_specs =
                    introspect_table_specs(&mut migrator, &case.input.excluded_columns).await;
                for spec in &table_specs {
                    migrator
                        .execute(AssertSqlSafe(format!(
                            "GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON {}.{} TO {}",
                            quote_ident(&spec.schema),
                            quote_ident(&spec.table),
                            quote_ident(&app_role),
                        )))
                        .await
                        .expect("grant table DML to app_role");
                }

                // 3. Generate and apply D3 triggers, then re-run them idempotently!
                trigger_stmts =
                    triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, table_specs.clone()));
                run_statements(&mut migrator, &trigger_stmts)
                    .await
                    .unwrap_or_else(|e| panic!("case `{}` trigger install failed: {e}", case.name));
                run_statements(&mut migrator, &trigger_stmts)
                    .await
                    .unwrap_or_else(|e| panic!("case `{}` trigger re-run failed: {e}", case.name));

                // Verify SECURITY DEFINER, auditstream_owner ownership, safe search_path, and exactly 4 triggers per table.
                let fn_meta: Vec<(String, bool, String, Vec<String>)> = sqlx::query_as(
                    "SELECT p.proname::text, p.prosecdef, r.rolname::text, COALESCE(p.proconfig, '{}') \
                     FROM pg_catalog.pg_proc p \
                     JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                     JOIN pg_catalog.pg_roles r ON r.oid = p.proowner \
                     WHERE n.nspname = 'auditstream' AND p.proname IN ('resolve_actor', 'capture') \
                     ORDER BY p.proname",
                )
                .fetch_all(&mut migrator)
                .await
                .expect("query function metadata");
                assert_eq!(fn_meta.len(), 2, "case `{}` missing functions", case.name);
                for (name, secdef, owner, config) in fn_meta {
                    assert!(
                        secdef,
                        "case `{}` {name} must be SECURITY DEFINER",
                        case.name
                    );
                    assert_eq!(owner, OWNER_ROLE, "case `{}` {name} owner", case.name);
                    assert!(
                        config
                            .iter()
                            .any(|c| c == "search_path=pg_catalog, pg_temp"),
                        "case `{}` {name} proconfig missing safe search_path: {config:?}",
                        case.name
                    );
                }

                for spec in &table_specs {
                    let tg_names: Vec<String> = sqlx::query_scalar(
                        "SELECT t.tgname::text \
                         FROM pg_catalog.pg_trigger t \
                         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
                         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal \
                         ORDER BY t.tgname",
                    )
                    .bind(&spec.schema)
                    .bind(&spec.table)
                    .fetch_all(&mut migrator)
                    .await
                    .expect("query triggers");
                    assert_eq!(
                        tg_names,
                        vec![
                            "auditstream_delete",
                            "auditstream_insert",
                            "auditstream_truncate",
                            "auditstream_update",
                        ],
                        "case `{}` table `{}` must have exactly 4 triggers after idempotent re-run",
                        case.name,
                        spec.table
                    );
                }

                migrator.close().await.expect("close migrator");
            }

            // 4. Execute each step as admin or app_role.
            let app_url = app_role_url(&db, &app_role, &app_pw);
            let mut app_conn = PgConnection::connect(&app_url)
                .await
                .expect("connect app_role");
            let mut migrator_conn = PgConnection::connect(db.url())
                .await
                .expect("connect migrator for admin steps");

            for (step_idx, step) in case.input.steps.iter().enumerate() {
                let conn = if step.role == "app" {
                    &mut app_conn
                } else {
                    &mut migrator_conn
                };

                let expected_err = case
                    .expected
                    .step_errors
                    .iter()
                    .find(|e| e.step_index == step_idx);

                let res = if step.set_config.is_some() || step.jwt_claims.is_some() {
                    let mut tx = conn.begin().await.expect("begin tx");
                    if let Some(ref actor_json) = step.set_config {
                        sqlx::query("SELECT pg_catalog.set_config('auditstream.actor', $1, true)")
                            .bind(actor_json)
                            .execute(&mut *tx)
                            .await
                            .expect("set auditstream.actor");
                    }
                    if let Some(ref jwt) = step.jwt_claims {
                        sqlx::query("SELECT pg_catalog.set_config('request.jwt.claims', $1, true)")
                            .bind(jwt)
                            .execute(&mut *tx)
                            .await
                            .expect("set request.jwt.claims");
                    }
                    let exec_res = tx.execute(AssertSqlSafe(step.sql.clone())).await;
                    match exec_res {
                        Ok(_) => tx.commit().await,
                        Err(e) => {
                            let _ = tx.rollback().await;
                            Err(e)
                        }
                    }
                } else {
                    conn.execute(AssertSqlSafe(step.sql.clone()))
                        .await
                        .map(|_| ())
                };

                match (expected_err, res) {
                    (Some(exp), Err(err)) => {
                        let code = err
                            .as_database_error()
                            .and_then(sqlx::error::DatabaseError::code);
                        assert_eq!(
                            code.as_deref(),
                            Some(exp.sqlstate.as_str()),
                            "case `{}` step {step_idx} SQLSTATE mismatch: {err}",
                            case.name
                        );
                    }
                    (Some(exp), Ok(())) => {
                        panic!(
                            "case `{}` step {step_idx} expected SQLSTATE {} but succeeded",
                            case.name, exp.sqlstate
                        );
                    }
                    (None, Err(err)) => {
                        panic!(
                            "case `{}` step {step_idx} unexpectedly failed: {err}",
                            case.name
                        );
                    }
                    (None, Ok(())) => {}
                }
            }

            // 5. Read auditstream.changes as app_role (verifying SELECT works) and compare against expected rows.
            let actual_rows = sqlx::query(
                "SELECT table_name, op, record_id::text AS record_id, changed, \
                        actor::text AS actor, actor_source, \
                        before::text AS before, after::text AS after \
                 FROM auditstream.changes \
                 ORDER BY id ASC",
            )
            .fetch_all(&mut app_conn)
            .await
            .expect("select auditstream.changes as app_role");

            assert_eq!(
                actual_rows.len(),
                case.expected.rows.len(),
                "case `{}` row count mismatch",
                case.name
            );

            for (idx, (actual, expected)) in actual_rows
                .iter()
                .zip(case.expected.rows.iter())
                .enumerate()
            {
                let table_name: Option<String> = actual.get("table_name");
                let op: String = actual.get("op");
                let record_id: Option<String> = actual.get("record_id");
                let changed: Option<Vec<String>> = actual.get("changed");
                let actor: Option<String> = actual.get("actor");
                let actor_source: Option<String> = actual.get("actor_source");
                let before: Option<String> = actual.get("before");
                let after: Option<String> = actual.get("after");

                assert_eq!(
                    table_name.as_deref(),
                    Some(expected.table_name.as_str()),
                    "case `{}` row {idx} table_name",
                    case.name
                );
                assert_eq!(op, expected.op, "case `{}` row {idx} op", case.name);
                assert_eq!(
                    record_id
                        .as_deref()
                        .map(|s| serde_json::from_str::<Value>(s).unwrap()),
                    expected.record_id,
                    "case `{}` row {idx} record_id",
                    case.name
                );
                assert_eq!(
                    changed, expected.changed,
                    "case `{}` row {idx} changed",
                    case.name
                );
                assert_eq!(
                    actor
                        .as_deref()
                        .map(|s| serde_json::from_str::<Value>(s).unwrap()),
                    expected.actor,
                    "case `{}` row {idx} actor",
                    case.name
                );
                assert_eq!(
                    actor_source, expected.actor_source,
                    "case `{}` row {idx} actor_source",
                    case.name
                );
                assert_eq!(
                    sorted_object_keys(before.as_deref()),
                    expected.before_keys,
                    "case `{}` row {idx} before_keys",
                    case.name
                );
                assert_eq!(
                    sorted_object_keys(after.as_deref()),
                    expected.after_keys,
                    "case `{}` row {idx} after_keys",
                    case.name
                );
            }

            app_conn.close().await.expect("close app_conn");
            migrator_conn.close().await.expect("close migrator_conn");
            drop(db);

            // Clean up per-test app_role and cluster-wide owner role.
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
    }

    assert!(
        total_cases >= 12,
        "expected at least 12 capture conformance cases, ran {total_cases}"
    );
}

#[derive(Debug, Deserialize)]
struct DecodeSuite {
    cases: Vec<DecodeCase>,
}

#[derive(Debug, Deserialize)]
struct DecodeCase {
    name: String,
    input: DecodeInput,
    expected: ResolvedExpected,
}

#[derive(Debug, Deserialize)]
struct DecodeInput {
    sql: String,
}

#[derive(Debug, Deserialize)]
struct PrecedenceSuite {
    cases: Vec<PrecedenceCase>,
}

#[derive(Debug, Deserialize)]
struct PrecedenceCase {
    name: String,
    input: PrecedenceInput,
    expected: ResolvedExpected,
}

#[derive(Debug, Deserialize)]
struct PrecedenceInput {
    sql: String,
    set_config: Option<String>,
    jwt_claims: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResolvedExpected {
    actor: Option<Value>,
    context: Option<Value>,
    actor_source: Option<String>,
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn every_transport_conformance_comment_and_precedence_case_resolves_in_sql_matching_c3() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    let ddl_stmts = ddl::generate(&DdlOptions::default());
    run_statements(&mut conn, &ddl_stmts)
        .await
        .expect("apply DDL");
    let trigger_stmts = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![TableTriggerSpec::new(
            "public",
            "invoice",
            vec!["id".to_owned()],
            vec!["password_hash".to_owned()],
        )],
    ));
    conn.execute(
        "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, total numeric, password_hash text)",
    )
    .await
    .expect("create invoice table");
    run_statements(&mut conn, &trigger_stmts)
        .await
        .expect("apply trigger functions and invoice triggers");

    // 1. Every case in spec/conformance/transport/decode.json must resolve in PostgreSQL
    //    (executing the exact unmodified fixture SQL!) to the same actor, context, and
    //    actor_source as C3's comment::decode().
    let decode_raw =
        fs::read_to_string(conformance_dir().join("transport/decode.json")).expect("read decode");
    let decode_suite: DecodeSuite = serde_json::from_str(&decode_raw).expect("parse decode");

    for case in &decode_suite.cases {
        let mut tx = conn.begin().await.expect("begin decode tx");
        if case.input.sql.trim_start().starts_with("UPDATE") {
            tx.execute(
                "INSERT INTO public.invoice VALUES (1,'seed',10,'s1'),(2,'seed',20,'s2'),(3,'seed',30,'s3')",
            )
            .await
            .expect("seed invoice rows");
            tx.execute("DELETE FROM auditstream.changes")
                .await
                .expect("clear seed audit rows");
        }
        tx.execute(AssertSqlSafe(case.input.sql.clone()))
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "decode case `{}` SQL execution failed: {e}\nSQL: {}",
                    case.name, case.input.sql
                )
            });
        let row = sqlx::query(
            "SELECT actor::text, context::text, actor_source FROM auditstream.changes ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("decode case `{}` captured no history row: {e}", case.name));
        tx.rollback().await.expect("rollback decode tx");

        let sql_actor: Option<String> = row.get("actor");
        let sql_context: Option<String> = row.get("context");
        let sql_source: Option<String> = row.get("actor_source");

        let sql_actor_val = sql_actor
            .as_deref()
            .map(|s| serde_json::from_str::<Value>(s).expect("valid actor json"));
        let sql_context_val = sql_context
            .as_deref()
            .map(|s| serde_json::from_str::<Value>(s).expect("valid context json"));

        // Compare PostgreSQL result against spec fixture.
        assert_eq!(
            sql_actor_val, case.expected.actor,
            "decode case `{}` actor",
            case.name
        );
        assert_eq!(
            sql_context_val, case.expected.context,
            "decode case `{}` context",
            case.name
        );
        assert_eq!(
            sql_source, case.expected.actor_source,
            "decode case `{}` actor_source",
            case.name
        );

        // Compare PostgreSQL result directly against C3's Rust decoder!
        let c3 = comment::decode(&case.input.sql);
        let (c3_actor, c3_ctx, c3_src) = match c3 {
            DecodedActor::Absent => (None, None, None),
            DecodedActor::Invalid => (None, None, Some("invalid".to_owned())),
            DecodedActor::Valid { actor, context } => {
                (Some(actor), context, Some("comment".to_owned()))
            }
        };
        assert_eq!(
            sql_actor_val, c3_actor,
            "decode case `{}` SQL vs C3 actor",
            case.name
        );
        assert_eq!(
            sql_context_val, c3_ctx,
            "decode case `{}` SQL vs C3 context",
            case.name
        );
        assert_eq!(
            sql_source, c3_src,
            "decode case `{}` SQL vs C3 source",
            case.name
        );
    }

    // 2. Every case in spec/conformance/transport/precedence.json must resolve in PostgreSQL
    //    (executing the exact unmodified fixture SQL!) to the same actor, context, and
    //    actor_source as C3's resolve_precedence().
    let prec_raw = fs::read_to_string(conformance_dir().join("transport/precedence.json"))
        .expect("read precedence");
    let prec_suite: PrecedenceSuite = serde_json::from_str(&prec_raw).expect("parse precedence");

    for case in &prec_suite.cases {
        let mut tx = conn.begin().await.expect("begin tx");
        tx.execute("INSERT INTO public.invoice VALUES (1,'seed',10,'s1'),(2,'seed',20,'s2')")
            .await
            .expect("seed invoice rows");
        tx.execute("DELETE FROM auditstream.changes")
            .await
            .expect("clear seed audit rows");

        if let Some(ref sc) = case.input.set_config {
            sqlx::query("SELECT pg_catalog.set_config('auditstream.actor', $1, true)")
                .bind(sc)
                .execute(&mut *tx)
                .await
                .expect("set auditstream.actor");
        }
        if let Some(ref jwt) = case.input.jwt_claims {
            sqlx::query("SELECT pg_catalog.set_config('request.jwt.claims', $1, true)")
                .bind(jwt)
                .execute(&mut *tx)
                .await
                .expect("set request.jwt.claims");
        }

        tx.execute(AssertSqlSafe(case.input.sql.clone()))
            .await
            .unwrap_or_else(|e| panic!("precedence case `{}` SQL failed: {e}", case.name));
        let row = sqlx::query(
            "SELECT actor::text, context::text, actor_source FROM auditstream.changes ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("precedence case `{}` captured no history row: {e}", case.name));
        tx.rollback().await.expect("rollback tx");

        let sql_actor: Option<String> = row.get("actor");
        let sql_context: Option<String> = row.get("context");
        let sql_source: Option<String> = row.get("actor_source");

        let sql_actor_val = sql_actor
            .as_deref()
            .map(|s| serde_json::from_str::<Value>(s).expect("valid actor json"));
        let sql_context_val = sql_context
            .as_deref()
            .map(|s| serde_json::from_str::<Value>(s).expect("valid context json"));

        assert_eq!(
            sql_actor_val, case.expected.actor,
            "prec case `{}` actor",
            case.name
        );
        assert_eq!(
            sql_context_val, case.expected.context,
            "prec case `{}` context",
            case.name
        );
        assert_eq!(
            sql_source, case.expected.actor_source,
            "prec case `{}` actor_source",
            case.name
        );

        let c3 = resolve_precedence(
            &case.input.sql,
            case.input.set_config.as_deref(),
            case.input.jwt_claims.as_deref(),
        );
        let c3_actor = c3.actor;
        let c3_ctx = c3.context;
        let c3_src = c3.actor_source.map(|s| {
            serde_json::to_value(s)
                .expect("serialize actor_source")
                .as_str()
                .expect("string")
                .to_owned()
        });
        assert_eq!(
            sql_actor_val, c3_actor,
            "prec case `{}` SQL vs C3 actor",
            case.name
        );
        assert_eq!(
            sql_context_val, c3_ctx,
            "prec case `{}` SQL vs C3 context",
            case.name
        );
        assert_eq!(
            sql_source, c3_src,
            "prec case `{}` SQL vs C3 source",
            case.name
        );
    }

    conn.close().await.expect("close conn");
}

type CapturedRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hot_path_fast_paths_handle_single_pk_composite_no_pk_special_names_and_excluded_noops() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    let ddl_stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    run_statements(&mut conn, &ddl_stmts)
        .await
        .expect("apply ddl");

    conn.execute(
        "CREATE TABLE public.single_pk (id int PRIMARY KEY, status text, secret text);\n\
         CREATE TABLE public.quoted_cols (\"row id\" text PRIMARY KEY, \"note \"\"x\"\"\" text, \"secret col\" text);\n\
         CREATE TABLE public.comp_pk (tenant_id text, item_id int, val text, PRIMARY KEY (tenant_id, item_id));\n\
         CREATE TABLE public.no_pk (label text);",
    )
    .await
    .expect("create test tables");

    let trigger_stmts = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![
            TableTriggerSpec::new(
                "public",
                "single_pk",
                vec!["id".to_owned()],
                vec!["secret".to_owned()],
            ),
            TableTriggerSpec::new(
                "public",
                "quoted_cols",
                vec!["row id".to_owned()],
                vec!["secret col".to_owned()],
            ),
            TableTriggerSpec::new(
                "public",
                "comp_pk",
                vec!["tenant_id".to_owned(), "item_id".to_owned()],
                vec![],
            ),
            TableTriggerSpec::new("public", "no_pk", vec![], vec![]),
        ],
    ));
    run_statements(&mut conn, &trigger_stmts)
        .await
        .expect("apply triggers");

    // 1. Multi-row INSERT + UPDATE + DELETE on single-PK and quoted-column tables.
    conn.execute(
        "INSERT INTO public.single_pk VALUES (1, 'open', 's1'), (2, 'open', 's2') \
         /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzEifQ'*/",
    )
    .await
    .expect("insert single_pk");

    // Excluded-column-only UPDATE (even with a malformed comment) must write zero rows.
    conn.execute(
        "UPDATE public.single_pk SET secret = 's1_new' WHERE id = 1 /*auditstream='!!!bad'*/",
    )
    .await
    .expect("excluded-only update single_pk");

    // Real UPDATE with actor-less query (exercises no-'*/' short-circuit in resolve_actor).
    conn.execute("UPDATE public.single_pk SET status = 'paid' WHERE id = 1")
        .await
        .expect("actorless update single_pk");

    conn.execute("DELETE FROM public.single_pk WHERE id = 2")
        .await
        .expect("delete single_pk");

    // Special-character single PK and excluded column.
    conn.execute(
        "INSERT INTO public.quoted_cols VALUES ('k-1', 'hello', 'topsecret') \
         /*traceparent='00-abc',auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzIifQ'*/",
    )
    .await
    .expect("insert quoted_cols");
    conn.execute(
        "UPDATE public.quoted_cols SET \"secret col\" = 'rotated' WHERE \"row id\" = 'k-1'",
    )
    .await
    .expect("excluded-only update quoted_cols");
    conn.execute(
        "UPDATE public.quoted_cols SET \"note \"\"x\"\"\" = 'world' WHERE \"row id\" = 'k-1'",
    )
    .await
    .expect("real update quoted_cols");

    // Composite PK and PK-less tables.
    conn.execute("INSERT INTO public.comp_pk VALUES ('t1', 7, 'a')")
        .await
        .expect("insert comp_pk");
    conn.execute("UPDATE public.comp_pk SET val = 'b' WHERE tenant_id = 't1' AND item_id = 7")
        .await
        .expect("update comp_pk");
    conn.execute("DELETE FROM public.comp_pk WHERE tenant_id = 't1' AND item_id = 7")
        .await
        .expect("delete comp_pk");
    conn.execute("INSERT INTO public.no_pk VALUES ('x')")
        .await
        .expect("insert no_pk");
    conn.execute("UPDATE public.no_pk SET label = 'y'")
        .await
        .expect("update no_pk");
    conn.execute("DELETE FROM public.no_pk")
        .await
        .expect("delete no_pk");

    // Malformed / edge comment shapes: slash inside comment body & empty auditstream=''.
    conn.execute(
        "INSERT INTO public.no_pk VALUES ('slash') /*a/auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzEifQ'*/",
    )
    .await
    .expect("slash in comment body");
    conn.execute("INSERT INTO public.no_pk VALUES ('empty') /*auditstream=''*/")
        .await
        .expect("empty auditstream tag");

    let rows: Vec<CapturedRow> = sqlx::query_as(
        "SELECT table_name, op, record_id::text, before::text, after::text, actor_source \
             FROM auditstream.changes ORDER BY id ASC",
    )
    .fetch_all(&mut conn)
    .await
    .expect("fetch captured rows");

    // single_pk: 2 INSERT + 0 excluded-only UPDATE + 1 UPDATE + 1 DELETE = 4
    // quoted_cols: 1 INSERT + 0 excluded-only UPDATE + 1 UPDATE = 2
    // comp_pk: 1 INSERT + 1 UPDATE + 1 DELETE = 3
    // no_pk: 1 INSERT + 1 UPDATE + 1 DELETE + 2 edge comments = 5
    assert_eq!(rows.len(), 14, "unexpected captured row count: {rows:#?}");

    // single_pk assertions (secret excluded everywhere)
    assert_eq!(rows[0].0, "single_pk");
    assert_eq!(rows[0].1, "INSERT");
    assert_eq!(rows[0].2.as_deref(), Some(r#"{"id": 1}"#));
    assert!(!rows[0].4.as_deref().unwrap_or("").contains("secret"));
    assert_eq!(rows[0].5.as_deref(), Some("comment"));

    assert_eq!(rows[2].0, "single_pk");
    assert_eq!(rows[2].1, "UPDATE");
    assert_eq!(rows[2].2.as_deref(), Some(r#"{"id": 1}"#));
    assert_eq!(rows[2].5, None);

    assert_eq!(rows[3].0, "single_pk");
    assert_eq!(rows[3].1, "DELETE");
    assert_eq!(rows[3].2.as_deref(), Some(r#"{"id": 2}"#));

    // quoted_cols assertions
    assert_eq!(rows[4].0, "quoted_cols");
    assert_eq!(rows[4].1, "INSERT");
    assert_eq!(rows[4].2.as_deref(), Some(r#"{"row id": "k-1"}"#));
    assert!(!rows[4].4.as_deref().unwrap_or("").contains("secret col"));
    assert_eq!(rows[4].5.as_deref(), Some("comment"));

    assert_eq!(rows[5].0, "quoted_cols");
    assert_eq!(rows[5].1, "UPDATE");
    assert_eq!(rows[5].2.as_deref(), Some(r#"{"row id": "k-1"}"#));

    // comp_pk assertions
    for idx in [6, 7, 8] {
        assert_eq!(rows[idx].0, "comp_pk");
        let rec: Value =
            serde_json::from_str(rows[idx].2.as_deref().expect("comp_pk record_id")).expect("json");
        assert_eq!(rec, serde_json::json!({"tenant_id": "t1", "item_id": 7}));
    }

    // no_pk assertions
    for idx in [9, 10, 11] {
        assert_eq!(rows[idx].0, "no_pk");
        assert_eq!(rows[idx].2, None);
    }

    // Slash inside comment body is ignored (actor_source = NULL); empty tag is invalid.
    assert_eq!(rows[12].5, None);
    assert_eq!(rows[13].5.as_deref(), Some("invalid"));

    conn.close().await.expect("close conn");
}
