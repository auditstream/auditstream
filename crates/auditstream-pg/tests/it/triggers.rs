use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use auditstream_core::transport::{DecodedActor, comment, resolve_precedence};
use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, OWNER_ROLE, quote_ident};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_pg::verify;
use auditstream_testkit::TestDb;
use serde::Deserialize;
use serde_json::Value;
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor, Row};
use uuid::Uuid;

async fn run_statements(conn: &mut PgConnection, stmts: &[String]) -> Result<(), sqlx::Error> {
    let mut tx = conn.begin().await?;
    for stmt in stmts {
        tx.execute(AssertSqlSafe(stmt.clone())).await?;
    }
    tx.commit().await
}

fn app_role_url(db: &TestDb, role: &str, password: &str) -> String {
    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    format!("{scheme}://{role}:{password}@{host_and_db}")
}

fn conformance_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
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

    // Both resolve_actor() and inlined capture() use DFA-cached COLLATE "C" textregexeq
    // (avoiding O(64*N) ltrim and POSIX regexp_match/!~), skip rtrim when ending with '/',
    // and use the single-'/' fast path before falling back to reverse().
    for (name, sql) in [("resolve_actor", resolve_actor), ("capture", capture)] {
        assert!(
            !sql.contains("regexp_match")
                && !sql.contains("!~")
                && !sql.contains("pg_catalog.ltrim("),
            "{name}() must not use regexp_match, !~, or ltrim:\n{sql}"
        );
        assert!(
            sql.contains("pg_catalog.textregexeq(")
                && sql.contains(r#"COLLATE pg_catalog."C""#)
                && sql.contains("'[^A-Za-z0-9_-]'"),
            "{name}() must validate base64url with COLLATE \"C\" textregexeq:\n{sql}"
        );
        assert!(
            sql.contains("v_last_ch <> '/'") && sql.contains("pg_catalog.rtrim("),
            "{name}() must guard rtrim with trailing-char fast check:\n{sql}"
        );
        assert!(
            sql.contains("v_p1 := pg_catalog.strpos(v_prefix, '/')"),
            "{name}() must include single-'/' fast path before reverse():\n{sql}"
        );
    }

    // capture() inlines actor resolution directly (no cross-function call to resolve_actor()).
    assert!(
        !capture.contains(".resolve_actor()"),
        "capture() must inline actor resolution instead of calling resolve_actor():\n{capture}"
    );
    assert!(
        capture.contains("<<resolve_actor>>"),
        "capture() must contain the labeled inlined <<resolve_actor>> block:\n{capture}"
    );

    // capture() uses single-column PK fast path, unified TG_ARGV decoding, and folds
    // changed[] into a single-SPI UPDATE INSERT ... VALUES (...).
    assert!(
        capture.contains("pg_catalog.jsonb_build_object(pk_single"),
        "capture() should build single-column PK record_id via jsonb_build_object:\n{capture}"
    );
    assert!(
        capture.contains("v_arg <> '[]'")
            && capture.contains("pg_catalog.left(v_arg, 1) = '['")
            && capture.contains("pg_catalog.left(v_arg, 1) = '{'"),
        "capture() should support unified JSON array, PG text[], and plain TG_ARGV formats:\n{capture}"
    );
    assert!(
        !capture.contains("v_changed :="),
        "capture() must fold changed[] into INSERT VALUES instead of separate v_changed SPI:\n{capture}"
    );
    assert!(
        capture.contains("ARRAY(\n        SELECT e.k"),
        "capture() must compute changed[] inline inside INSERT VALUES:\n{capture}"
    );

    // UPDATE checks excluded-column no-op before running the inlined <<resolve_actor>> block.
    let noop_pos = capture
        .find("IF v_after = v_before THEN")
        .expect("excluded-column no-op guard in capture()");
    let resolve_pos = capture
        .find("<<resolve_actor>>")
        .expect("inlined <<resolve_actor>> block in capture()");
    assert!(
        noop_pos < resolve_pos,
        "UPDATE no-op check must execute before inlined actor resolution"
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
struct EncodeSuite {
    cases: Vec<EncodeCase>,
}

#[derive(Debug, Deserialize)]
struct EncodeCase {
    name: String,
    input: EncodeInput,
    expected: EncodeExpected,
}

#[derive(Debug, Deserialize)]
struct EncodeInput {
    actor: Value,
}

#[derive(Debug, Deserialize)]
struct EncodeExpected {
    canonical_json: Option<String>,
    comment: Option<String>,
    error: Option<String>,
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

type ResolvedTuple = (Option<Value>, Option<Value>, Option<String>);

fn parse_resolved_row(row: &sqlx::postgres::PgRow) -> ResolvedTuple {
    let sql_actor: Option<String> = row.get("actor");
    let sql_context: Option<String> = row.get("context");
    let sql_source: Option<String> = row.get("actor_source");
    let actor_val = sql_actor
        .as_deref()
        .map(|s| serde_json::from_str::<Value>(s).expect("valid actor json"));
    let context_val = sql_context
        .as_deref()
        .map(|s| serde_json::from_str::<Value>(s).expect("valid context json"));
    (actor_val, context_val, sql_source)
}

async fn install_resolve_actor_observer(conn: &mut PgConnection, table_fq: &str) {
    conn.execute(
        "CREATE TEMP TABLE direct_resolved (\
           id bigserial PRIMARY KEY, \
           actor jsonb, \
           context jsonb, \
           actor_source text\
         )",
    )
    .await
    .expect("create direct_resolved temp table");
    conn.execute(
        "CREATE FUNCTION pg_temp.record_resolve_actor() RETURNS trigger LANGUAGE plpgsql AS $$\
         DECLARE r record; \
         BEGIN \
           SELECT actor, context, actor_source INTO r FROM auditstream.resolve_actor(); \
           INSERT INTO pg_temp.direct_resolved (actor, context, actor_source) \
           VALUES (r.actor, r.context, r.actor_source); \
           RETURN NULL; \
         END $$",
    )
    .await
    .expect("create pg_temp.record_resolve_actor");
    conn.execute(AssertSqlSafe(format!(
        "CREATE TRIGGER z_observe_resolve_actor \
         AFTER INSERT OR UPDATE ON {table_fq} \
         FOR EACH STATEMENT EXECUTE FUNCTION pg_temp.record_resolve_actor()"
    )))
    .await
    .expect("attach resolve_actor observer trigger");
}

async fn assert_dml_actor_parity(
    conn: &mut PgConnection,
    seed_sql: Option<&str>,
    gucs: &[(&str, &str)],
    dml_sql: &str,
    expected: &ResolvedTuple,
    label: &str,
) -> ResolvedTuple {
    let mut tx = conn.begin().await.expect("begin parity tx");
    if let Some(seed) = seed_sql {
        tx.execute(AssertSqlSafe(seed.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("{label}: seed SQL failed: {e}"));
        tx.execute("DELETE FROM auditstream.changes")
            .await
            .expect("clear seed audit rows");
        tx.execute("DELETE FROM pg_temp.direct_resolved")
            .await
            .expect("clear seed direct_resolved rows");
    }
    for (guc_name, guc_val) in gucs {
        sqlx::query("SELECT pg_catalog.set_config($1, $2, true)")
            .bind(guc_name)
            .bind(guc_val)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{label}: set_config({guc_name}) failed: {e}"));
    }
    tx.execute(AssertSqlSafe(dml_sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("{label}: SQL execution failed: {e}\nSQL: {dml_sql}"));
    let cap_row = sqlx::query(
        "SELECT actor::text, context::text, actor_source FROM auditstream.changes ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap_or_else(|e| panic!("{label}: captured no history row: {e}"));
    let dir_row = sqlx::query(
        "SELECT actor::text, context::text, actor_source FROM pg_temp.direct_resolved ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap_or_else(|e| panic!("{label}: direct resolve_actor missing: {e}"));
    tx.rollback().await.expect("rollback parity tx");

    let cap = parse_resolved_row(&cap_row);
    let dir = parse_resolved_row(&dir_row);
    assert_eq!(&cap, expected, "{label}: capture() mismatch");
    assert_eq!(&dir, expected, "{label}: resolve_actor() mismatch");
    cap
}

fn encode_base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        }
    }
    out
}

async fn assert_encode_conformance(conn: &mut PgConnection) {
    let encode_raw =
        fs::read_to_string(conformance_dir().join("transport/encode.json")).expect("read encode");
    let encode_suite: EncodeSuite = serde_json::from_str(&encode_raw).expect("parse encode");
    assert!(
        !encode_suite.cases.is_empty(),
        "transport/encode.json must contain conformance cases"
    );

    let seed = "INSERT INTO public.invoice VALUES (1,'seed',10,'s1')";
    let expected_invalid: ResolvedTuple = (None, None, Some("invalid".to_owned()));

    for case in &encode_suite.cases {
        if let (Some(comment_tag), Some(canonical_json)) =
            (&case.expected.comment, &case.expected.canonical_json)
        {
            let mut expected_actor = case.input.actor.clone();
            let expected_ctx = expected_actor
                .as_object_mut()
                .and_then(|obj| obj.remove("context"));

            let comment_sql =
                format!("UPDATE public.invoice SET status = 'enc' WHERE id = 1 {comment_tag}");
            let exp_comment: ResolvedTuple = (
                Some(expected_actor.clone()),
                expected_ctx.clone(),
                Some("comment".to_owned()),
            );
            assert_dml_actor_parity(
                conn,
                Some(seed),
                &[],
                &comment_sql,
                &exp_comment,
                &format!("encode `{}` (comment)", case.name),
            )
            .await;

            let exp_sc: ResolvedTuple = (
                Some(expected_actor),
                expected_ctx,
                Some("set_config".to_owned()),
            );
            assert_dml_actor_parity(
                conn,
                Some(seed),
                &[("auditstream.actor", canonical_json.as_str())],
                "UPDATE public.invoice SET status = 'enc' WHERE id = 1",
                &exp_sc,
                &format!("encode `{}` (set_config)", case.name),
            )
            .await;
        } else if case.expected.error.is_some() {
            // `serde_json::to_string` escapes U+0000 as ASCII `\u0000`, so every invalid case can
            // be sent through both set_config and base64url comment to verify rejection parity.
            let invalid_json =
                serde_json::to_string(&case.input.actor).expect("serialize invalid actor");
            let invalid_b64 = encode_base64url(invalid_json.as_bytes());
            let comment_sql = format!(
                "UPDATE public.invoice SET status = 'bad' WHERE id = 1 /*auditstream='{invalid_b64}'*/"
            );
            assert_dml_actor_parity(
                conn,
                Some(seed),
                &[],
                &comment_sql,
                &expected_invalid,
                &format!("encode error `{}` (comment)", case.name),
            )
            .await;
            assert_dml_actor_parity(
                conn,
                Some(seed),
                &[("auditstream.actor", invalid_json.as_str())],
                "UPDATE public.invoice SET status = 'bad' WHERE id = 1",
                &expected_invalid,
                &format!("encode error `{}` (set_config)", case.name),
            )
            .await;
        } else {
            panic!("unhandled encode conformance case `{}`", case.name);
        }
    }
}

async fn assert_decode_conformance(conn: &mut PgConnection) {
    let decode_raw =
        fs::read_to_string(conformance_dir().join("transport/decode.json")).expect("read decode");
    let decode_suite: DecodeSuite = serde_json::from_str(&decode_raw).expect("parse decode");
    assert!(
        !decode_suite.cases.is_empty(),
        "transport/decode.json must contain conformance cases"
    );

    let decode_seed = "INSERT INTO public.invoice VALUES (1,'seed',10,'s1'),(2,'seed',20,'s2'),(3,'seed',30,'s3')";
    for case in &decode_suite.cases {
        let seed = case
            .input
            .sql
            .trim_start()
            .starts_with("UPDATE")
            .then_some(decode_seed);
        let expected: ResolvedTuple = (
            case.expected.actor.clone(),
            case.expected.context.clone(),
            case.expected.actor_source.clone(),
        );
        let cap = assert_dml_actor_parity(
            conn,
            seed,
            &[],
            &case.input.sql,
            &expected,
            &format!("decode `{}`", case.name),
        )
        .await;

        let c3 = match comment::decode(&case.input.sql) {
            DecodedActor::Absent => (None, None, None),
            DecodedActor::Invalid => (None, None, Some("invalid".to_owned())),
            DecodedActor::Valid { actor, context } => {
                (Some(actor), context, Some("comment".to_owned()))
            }
        };
        assert_eq!(cap, c3, "decode `{}` SQL vs C3 mismatch", case.name);
    }
}

async fn assert_precedence_conformance(conn: &mut PgConnection) {
    let prec_raw = fs::read_to_string(conformance_dir().join("transport/precedence.json"))
        .expect("read precedence");
    let prec_suite: PrecedenceSuite = serde_json::from_str(&prec_raw).expect("parse precedence");
    assert!(
        !prec_suite.cases.is_empty(),
        "transport/precedence.json must contain conformance cases"
    );

    let prec_seed = "INSERT INTO public.invoice VALUES (1,'seed',10,'s1'),(2,'seed',20,'s2')";
    for case in &prec_suite.cases {
        let mut gucs = Vec::new();
        if let Some(ref sc) = case.input.set_config {
            gucs.push(("auditstream.actor", sc.as_str()));
        }
        if let Some(ref jwt) = case.input.jwt_claims {
            gucs.push(("request.jwt.claims", jwt.as_str()));
        }
        let expected: ResolvedTuple = (
            case.expected.actor.clone(),
            case.expected.context.clone(),
            case.expected.actor_source.clone(),
        );
        let cap = assert_dml_actor_parity(
            conn,
            Some(prec_seed),
            &gucs,
            &case.input.sql,
            &expected,
            &format!("prec `{}`", case.name),
        )
        .await;

        let c3 = resolve_precedence(
            &case.input.sql,
            case.input.set_config.as_deref(),
            case.input.jwt_claims.as_deref(),
        );
        let c3_src = c3.actor_source.map(|s| {
            serde_json::to_value(s)
                .expect("serialize actor_source")
                .as_str()
                .expect("string")
                .to_owned()
        });
        assert_eq!(
            cap,
            (c3.actor, c3.context, c3_src),
            "prec `{}` SQL vs C3 mismatch",
            case.name
        );
    }
}

#[tokio::test]
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

    // Observe `auditstream.resolve_actor()` directly alongside inlined `auditstream.capture()`
    // on the exact same unmodified SQL statements and transaction GUCs.
    install_resolve_actor_observer(&mut conn, "public.invoice").await;

    assert_decode_conformance(&mut conn).await;
    assert_precedence_conformance(&mut conn).await;
    assert_encode_conformance(&mut conn).await;

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

/// RAII drop guard for an extra temporary database created directly in a test, ensuring
/// `DROP DATABASE ... WITH (FORCE)` always runs before `TestDb::cluster_lock()` releases
/// even when an assertion panics.
struct TempDbDropGuard {
    admin_url: String,
    db_name: String,
}

impl Drop for TempDbDropGuard {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let db_name = self.db_name.clone();
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
                            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                            quote_ident(&db_name)
                        )))
                        .await;
                    let _ = admin.close().await;
                }
            });
        });
        let _ = handle.join();
    }
}

fn replace_url_database(base_url: &str, db_name: &str) -> String {
    let (without_query, query) = match base_url.split_once('?') {
        Some((head, q)) => (head, Some(q)),
        None => (base_url, None),
    };
    let (prefix, _) = without_query.rsplit_once('/').expect("db url has /db");
    match query {
        Some(q) => format!("{prefix}/{db_name}?{q}"),
        None => format!("{prefix}/{db_name}"),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn actor_resolution_parity_on_3vl_edge_cases_jwt_null_sub_utf8_and_nondeterministic_collation()
 {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    let ddl_stmts = ddl::generate(&DdlOptions::default());
    run_statements(&mut conn, &ddl_stmts)
        .await
        .expect("apply DDL");

    // Create a nondeterministic ICU collation and an audited table using it.
    conn.execute(
        "CREATE COLLATION public.nd_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false); \
         CREATE TABLE public.items (id int PRIMARY KEY, label text COLLATE public.nd_ci)",
    )
    .await
    .expect("create nondeterministic ICU table");

    let trigger_stmts = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![TableTriggerSpec::new(
            "public",
            "items",
            vec!["id".to_owned()],
            vec![],
        )],
    ));
    run_statements(&mut conn, &trigger_stmts)
        .await
        .expect("apply triggers");
    install_resolve_actor_observer(&mut conn, "public.items").await;

    let item_seed = "INSERT INTO public.items VALUES (1, 'seed')";
    let expected_invalid: ResolvedTuple = (None, None, Some("invalid".to_owned()));

    // 1. 3VL & strict type edge cases must yield `actor_source = 'invalid'` on BOTH `capture()`
    //    and `resolve_actor()`, via both `set_config` and trailing base64url comments.
    let invalid_3vl_payloads = [
        r#"{"id":"x"}"#,
        r#"{"type":null,"id":"x"}"#,
        r#"{"type":"user","id":123}"#,
        r#"{"type":123,"id":"x"}"#,
        r#"{"type":"user","id":null}"#,
        r#"{"type":"user","id":""}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":null}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"id":"u1"}}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"type":"user"}}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"type":null,"id":"u1"}}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"type":"user","id":123}}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"type":"user","id":""}}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"type":"user","id":"u1","onBehalfOf":null}}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"type":"user","id":"u1","tenant":null}}"#,
        r#"{"type":"ai_agent","id":"a1","onBehalfOf":{"type":"user","id":"u1","tenant":""}}"#,
        r#"{"type":"user","id":"u1","tenant":null}"#,
        r#"{"type":"user","id":"u1","context":null}"#,
        r#"{"type":"user","id":"u1","context":{"requestId":null}}"#,
        r#"{"type":"user","id":"u1","context":{"source":123}}"#,
        r#"{"type":"user","id":"u1","context":{"ip":""}}"#,
        r#"{"type":"user","id":"u1","context":{"userAgent":false}}"#,
    ];

    for payload in invalid_3vl_payloads {
        let encoded = encode_base64url(payload.as_bytes());
        let comment_sql = format!(
            "UPDATE public.items SET label = 'next' WHERE id = 1 /*auditstream='{encoded}'*/"
        );
        assert_dml_actor_parity(
            &mut conn,
            Some(item_seed),
            &[],
            &comment_sql,
            &expected_invalid,
            &format!("3VL `{payload}` (comment)"),
        )
        .await;
        assert_dml_actor_parity(
            &mut conn,
            Some(item_seed),
            &[("auditstream.actor", payload)],
            "UPDATE public.items SET label = 'next' WHERE id = 1",
            &expected_invalid,
            &format!("3VL `{payload}` (set_config)"),
        )
        .await;
    }

    // 2. JWT `{"sub":null}` and absent `sub` MUST resolve to NULL (not `'invalid'`), while
    //    non-string or empty `sub` MUST resolve to `'invalid'`, identically in both paths.
    let jwt_cases: [(&str, Option<&str>); 6] = [
        (r#"{"sub":null}"#, None),
        (r#"{"sub":null,"role":"anon"}"#, None),
        (r#"{"role":"anon"}"#, None),
        (r#"{"sub":123}"#, Some("invalid")),
        (r#"{"sub":true}"#, Some("invalid")),
        (r#"{"sub":""}"#, Some("invalid")),
    ];
    for (jwt_json, expected_src) in jwt_cases {
        let expected: ResolvedTuple = (None, None, expected_src.map(str::to_owned));
        assert_dml_actor_parity(
            &mut conn,
            Some(item_seed),
            &[("request.jwt.claims", jwt_json)],
            "UPDATE public.items SET label = 'jwt' WHERE id = 1",
            &expected,
            &format!("JWT `{jwt_json}`"),
        )
        .await;
    }

    // 3. Base64url COLLATE "C" regex rejects accented UTF-8, newlines, padding, '+', space,
    //    while accepting valid unpadded base64url (including '-' and '_') and running cleanly
    //    against nondeterministic ICU columns and single-'/' vs multi-'/' comments.
    let valid_b64 = encode_base64url(br#"{"type":"user","id":"usr?>~???"}"#);
    assert!(
        valid_b64.contains('-') && valid_b64.contains('_'),
        "expected both '-' and '_' in base64url sample, got: {valid_b64}"
    );
    // Replace '-' with '+' so base64 decoding would still produce valid JSON if '+' were not
    // explicitly rejected by the base64url COLLATE "C" regex.
    let plus_b64 = valid_b64.replace('-', "+");
    assert_ne!(plus_b64, valid_b64);

    let b64_invalid_samples = [
        "eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0é".to_owned(),
        "eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0ß".to_owned(),
        "eyJ0eXBlIjoidXNlciIs\nImlkIjoidXNyXzQyIn0".to_owned(),
        "eyJ0eXBlIjoidXNlciIs\rImlkIjoidXNyXzQyIn0".to_owned(),
        "eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0=".to_owned(),
        plus_b64.clone(),
        format!("{} {}", &valid_b64[..8], &valid_b64[8..]),
    ];
    for bad_b64 in &b64_invalid_samples {
        let sql = format!(
            "UPDATE public.items SET label = 'bad' WHERE id = 1 /*auditstream='{bad_b64}'*/"
        );
        assert_dml_actor_parity(
            &mut conn,
            Some(item_seed),
            &[],
            &sql,
            &expected_invalid,
            &format!("bad b64 `{bad_b64:?}`"),
        )
        .await;
    }

    // Valid comment on nondeterministic ICU table (also exercises trailing '/' no-rtrim path
    // and multi-'/' reverse() fallback path with a '/' inside a string literal).
    for sql in [
        format!("INSERT INTO public.items VALUES (10, 'Café') /*auditstream='{valid_b64}'*/"),
        format!(
            "UPDATE public.items SET label = 'path/with/slashes' WHERE id = 10 /*auditstream='{valid_b64}'*/;"
        ),
    ] {
        conn.execute(AssertSqlSafe(sql))
            .await
            .expect("valid icu write");
    }
    let expected_valid: ResolvedTuple = (
        Some(serde_json::json!({"type": "user", "id": "usr?>~???"})),
        None,
        Some("comment".to_owned()),
    );
    let cap_rows = sqlx::query(
        "SELECT actor::text, context::text, actor_source FROM auditstream.changes ORDER BY id ASC",
    )
    .fetch_all(&mut conn)
    .await
    .expect("read valid icu capture rows");
    let dir_rows = sqlx::query(
        "SELECT actor::text, context::text, actor_source FROM pg_temp.direct_resolved ORDER BY id ASC",
    )
    .fetch_all(&mut conn)
    .await
    .expect("read valid icu direct rows");
    assert_eq!(cap_rows.len(), 2);
    assert_eq!(dir_rows.len(), 2);
    for (cap, dir) in cap_rows.iter().zip(dir_rows.iter()) {
        assert_eq!(parse_resolved_row(cap), expected_valid);
        assert_eq!(parse_resolved_row(dir), expected_valid);
    }

    // 4. Verify a UTF-8 ICU database (`LOCALE_PROVIDER icu ICU_LOCALE 'en-US'`) where the
    //    default `text` collation is locale-sensitive rejects non-ASCII keys and invalid
    //    base64url chars identically in both `capture()` and `resolve_actor()`.
    let icu_db_name = format!("auditstream_icu_{}", Uuid::new_v4().simple());
    let admin_url = db.admin_url().to_owned();
    {
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("admin connect for icu db");
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE DATABASE {} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER icu ICU_LOCALE 'en-US'",
                quote_ident(&icu_db_name),
            )))
            .await
            .expect("create icu en-US database");
        admin.close().await.expect("close admin");
    }
    let _icu_guard = TempDbDropGuard {
        admin_url,
        db_name: icu_db_name.clone(),
    };

    let icu_url = replace_url_database(db.url(), &icu_db_name);
    let mut icu_conn = PgConnection::connect(&icu_url)
        .await
        .expect("connect icu db");
    run_statements(&mut icu_conn, &ddl_stmts)
        .await
        .expect("apply ddl on icu db");
    icu_conn
        .execute("CREATE TABLE public.t_icu (id int PRIMARY KEY, name text)")
        .await
        .expect("create t_icu");
    let icu_trig = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![TableTriggerSpec::new(
            "public",
            "t_icu",
            vec!["id".to_owned()],
            vec![],
        )],
    ));
    run_statements(&mut icu_conn, &icu_trig)
        .await
        .expect("apply triggers on icu db");
    install_resolve_actor_observer(&mut icu_conn, "public.t_icu").await;

    let icu_seed = "INSERT INTO public.t_icu VALUES (1, 'a')";
    let expected_absent: ResolvedTuple = (None, None, None);
    let icu_cases = [
        (
            format!("UPDATE public.t_icu SET name = 'ok' WHERE id = 1 /*auditstream='{valid_b64}'*/"),
            &expected_valid,
            "icu en-US valid b64",
        ),
        (
            format!("UPDATE public.t_icu SET name = 'plus' WHERE id = 1 /*auditstream='{plus_b64}'*/"),
            &expected_invalid,
            "icu en-US plus in b64",
        ),
        (
            format!(
                "UPDATE public.t_icu SET name = 'sp' WHERE id = 1 /*auditstream='{} {}'*/",
                &valid_b64[..8],
                &valid_b64[8..]
            ),
            &expected_invalid,
            "icu en-US space in b64",
        ),
        (
            "UPDATE public.t_icu SET name = 'acc' WHERE id = 1 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0é'*/".to_owned(),
            &expected_invalid,
            "icu en-US accented b64",
        ),
        // Accented sqlcommenter key tests `v_key_check COLLATE pg_catalog."C"` directly without
        // falling back to base64 decode: the invalid key rejects the comment pair list (-> NULL).
        (
            format!(
                "UPDATE public.t_icu SET name = 'key' WHERE id = 1 /*rôute='1',auditstream='{valid_b64}'*/"
            ),
            &expected_absent,
            "icu en-US accented sqlcommenter key",
        ),
    ];
    for (sql, expected, label) in icu_cases {
        assert_dml_actor_parity(&mut icu_conn, Some(icu_seed), &[], &sql, expected, label).await;
    }
    icu_conn.close().await.expect("close icu conn");

    conn.close().await.expect("close conn");
}

type UnifiedArgRow = (
    String,
    String,
    Option<String>,
    Option<Vec<String>>,
    Option<String>,
);

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn capture_supports_unified_tg_argv_formats_and_zero_lock_v01x_function_upgrade() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");

    let db = TestDb::new().await.expect("create TestDb");
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");

    let ddl_stmts = ddl::generate(&DdlOptions::default());
    run_statements(&mut conn, &ddl_stmts)
        .await
        .expect("apply DDL");

    conn.execute(
        "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, password_hash text); \
         CREATE TABLE public.plain_arg (id int PRIMARY KEY, status text, secret text); \
         CREATE TABLE public.plain_empty_arg (label text); \
         CREATE TABLE public.json_multi_ex (id int PRIMARY KEY, val text, s1 text, s2 text); \
         CREATE TABLE public.pg_array_arg (tenant_id text, item_id int, val text, s1 text, s2 text, PRIMARY KEY (tenant_id, item_id)); \
         CREATE TABLE public.pg_array_single (\"row id\" text PRIMARY KEY, note text);",
    )
    .await
    .expect("create test tables");

    let invoice_spec = TableTriggerSpec::new(
        "public",
        "invoice",
        vec!["id".to_owned()],
        vec!["password_hash".to_owned()],
    );
    let full_stmts = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![invoice_spec.clone()],
    ));
    run_statements(&mut conn, &full_stmts)
        .await
        .expect("install full triggers on invoice");

    // 1. Simulate a pre-upgrade v0.1.x database whose triggers on `public.invoice` are already
    //    up to date while `resolve_actor()` and `capture()` have outdated function bodies.
    conn.execute(
        "DO $$ BEGIN \
           SET LOCAL ROLE auditstream_owner; \
           CREATE OR REPLACE FUNCTION auditstream.resolve_actor(OUT actor jsonb, OUT context jsonb, OUT actor_source text) \
             LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp \
             AS $old$ BEGIN actor := NULL; context := NULL; actor_source := NULL; END $old$; \
           CREATE OR REPLACE FUNCTION auditstream.capture() \
             RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp \
             AS $old$ BEGIN RETURN NULL; END $old$; \
         END $$",
    )
    .await
    .expect("downgrade function bodies to simulate v0.1.x pre-upgrade");

    let before_report = verify::check(db.pool(), std::slice::from_ref(&invoice_spec))
        .await
        .expect("verify before function-only upgrade");
    for expected_outdated in ["auditstream.resolve_actor()", "auditstream.capture()"] {
        assert!(
            before_report
                .findings
                .iter()
                .any(|f| f.kind == verify::FindingKind::Outdated && f.object == expected_outdated),
            "expected Outdated finding for {expected_outdated} before upgrade, got: {:?}",
            before_report.findings
        );
    }
    assert!(
        !before_report
            .findings
            .iter()
            .any(|f| f.object.starts_with("public.invoice.")),
        "v0.1.x triggers on public.invoice should not need replacement: {:?}",
        before_report.findings
    );

    // Upgrading functions with TriggerOptions::new(DEFAULT_SCHEMA, vec![]) MUST emit
    // `CREATE OR REPLACE FUNCTION` for both functions, ZERO `CREATE OR REPLACE TRIGGER`
    // statements, and succeed immediately while a concurrent writer holds ROW EXCLUSIVE on
    // `public.invoice` under a tight lock_timeout.
    let fn_only_stmts = triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, vec![]));
    assert!(
        fn_only_stmts
            .iter()
            .any(|s| s.starts_with(r#"CREATE OR REPLACE FUNCTION "auditstream".resolve_actor("#)),
        "function-only upgrade must emit CREATE OR REPLACE FUNCTION resolve_actor()"
    );
    assert!(
        fn_only_stmts
            .iter()
            .any(|s| s.starts_with(r#"CREATE OR REPLACE FUNCTION "auditstream".capture()"#)),
        "function-only upgrade must emit CREATE OR REPLACE FUNCTION capture()"
    );
    assert!(
        !fn_only_stmts
            .iter()
            .any(|s| s.contains("CREATE OR REPLACE TRIGGER")),
        "function-only upgrade must emit zero CREATE OR REPLACE TRIGGER statements"
    );
    conn.execute("INSERT INTO public.invoice VALUES (1, 'open', 's1')")
        .await
        .expect("seed invoice");

    let mut holder = PgConnection::connect(db.url())
        .await
        .expect("connect lock holder");
    let mut holder_tx = holder.begin().await.expect("begin holder tx");
    holder_tx
        .execute("UPDATE public.invoice SET status = status WHERE false")
        .await
        .expect("acquire ROW EXCLUSIVE on invoice");

    let mut upgrader = PgConnection::connect(db.url())
        .await
        .expect("connect upgrader");
    let mut upgrade_tx = upgrader.begin().await.expect("begin upgrade tx");
    upgrade_tx
        .execute("SET LOCAL lock_timeout = '100ms'")
        .await
        .expect("set tight lock_timeout");
    for stmt in &fn_only_stmts {
        upgrade_tx
            .execute(AssertSqlSafe(stmt.clone()))
            .await
            .unwrap_or_else(|e| {
                panic!("zero-lock function upgrade failed under ROW EXCLUSIVE: {e}")
            });
    }
    upgrade_tx.commit().await.expect("commit function upgrade");
    holder_tx.rollback().await.expect("release holder lock");
    holder.close().await.expect("close holder");
    upgrader.close().await.expect("close upgrader");

    let report = verify::check(db.pool(), &[invoice_spec])
        .await
        .expect("verify after function-only upgrade");
    assert!(
        report.findings.is_empty(),
        "verify must report zero findings after v0.1.x function-only upgrade: {:?}",
        report.findings
    );

    // Exercise the preserved v0.1.x trigger on `public.invoice` with the upgraded `capture()`.
    conn.execute(
        "UPDATE public.invoice SET status = 'upgraded' WHERE id = 1 \
         /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzE2MiJ9'*/",
    )
    .await
    .expect("update invoice after zero-lock function upgrade");
    let upgraded_row: (String, Option<String>, Option<Vec<String>>, Option<String>) =
        sqlx::query_as(
            "SELECT op, record_id::text, changed, actor_source \
               FROM auditstream.changes \
              WHERE table_name = 'invoice' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&mut conn)
        .await
        .expect("read upgraded invoice capture row");
    assert_eq!(upgraded_row.0, "UPDATE");
    assert_eq!(upgraded_row.1.as_deref(), Some(r#"{"id": 1}"#));
    assert_eq!(upgraded_row.2, Some(vec!["status".to_owned()]));
    assert_eq!(upgraded_row.3.as_deref(), Some("comment"));

    // 2. Verify unified TG_ARGV decoder handles plain single-column args (`'id'`, `'secret'`, `''`),
    //    multi-excluded JSON arrays (`'["s1","s2"]'`), and PostgreSQL text[] args
    //    (`'{tenant_id,item_id}'`, `'{s1,s2}'`, `'{"row id"}'`, `'{}'`).
    for stmt in [
        "CREATE TRIGGER auditstream_insert AFTER INSERT ON public.plain_arg REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION auditstream.capture('id', 'secret')",
        "CREATE TRIGGER auditstream_update AFTER UPDATE ON public.plain_arg FOR EACH ROW EXECUTE FUNCTION auditstream.capture('id', 'secret')",
        "CREATE TRIGGER auditstream_delete AFTER DELETE ON public.plain_arg REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION auditstream.capture('id', 'secret')",
        "CREATE TRIGGER auditstream_insert AFTER INSERT ON public.plain_empty_arg REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION auditstream.capture('', '')",
        "CREATE TRIGGER auditstream_update AFTER UPDATE ON public.plain_empty_arg FOR EACH ROW EXECUTE FUNCTION auditstream.capture('', '')",
        "CREATE TRIGGER auditstream_insert AFTER INSERT ON public.json_multi_ex REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION auditstream.capture('[\"id\"]', '[\"s1\",\"s2\"]')",
        "CREATE TRIGGER auditstream_update AFTER UPDATE ON public.json_multi_ex FOR EACH ROW EXECUTE FUNCTION auditstream.capture('[\"id\"]', '[\"s1\",\"s2\"]')",
        "CREATE TRIGGER auditstream_insert AFTER INSERT ON public.pg_array_arg REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION auditstream.capture('{tenant_id,item_id}', '{s1,s2}')",
        "CREATE TRIGGER auditstream_update AFTER UPDATE ON public.pg_array_arg FOR EACH ROW EXECUTE FUNCTION auditstream.capture('{tenant_id,item_id}', '{s1,s2}')",
        "CREATE TRIGGER auditstream_delete AFTER DELETE ON public.pg_array_arg REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION auditstream.capture('{tenant_id,item_id}', '{s1,s2}')",
        "CREATE TRIGGER auditstream_insert AFTER INSERT ON public.pg_array_single REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION auditstream.capture('{\"row id\"}', '{}')",
        "CREATE TRIGGER auditstream_update AFTER UPDATE ON public.pg_array_single FOR EACH ROW EXECUTE FUNCTION auditstream.capture('{\"row id\"}', '{}')",
    ] {
        conn.execute(stmt)
            .await
            .expect("attach unified-arg trigger");
    }

    conn.execute("DELETE FROM auditstream.changes")
        .await
        .expect("clear changes");

    // Plain single-column args
    conn.execute("INSERT INTO public.plain_arg VALUES (7, 'a', 'top')")
        .await
        .expect("insert plain_arg");
    conn.execute("UPDATE public.plain_arg SET secret = 'rot' WHERE id = 7")
        .await
        .expect("excluded-only update plain_arg");
    conn.execute("UPDATE public.plain_arg SET status = 'b' WHERE id = 7")
        .await
        .expect("real update plain_arg");
    conn.execute("DELETE FROM public.plain_arg WHERE id = 7")
        .await
        .expect("delete plain_arg");

    // Empty plain args (`capture('', '')`) on a no-PK table must emit record_id = NULL.
    conn.execute("INSERT INTO public.plain_empty_arg VALUES ('x')")
        .await
        .expect("insert plain_empty_arg");
    conn.execute("UPDATE public.plain_empty_arg SET label = 'y'")
        .await
        .expect("update plain_empty_arg");

    // Multi-excluded JSON array (`ex_len > 1`) strips both excluded columns and skips no-op updates.
    conn.execute("INSERT INTO public.json_multi_ex VALUES (11, 'v1', 'a', 'b')")
        .await
        .expect("insert json_multi_ex");
    conn.execute("UPDATE public.json_multi_ex SET s1 = 'a2', s2 = 'b2' WHERE id = 11")
        .await
        .expect("excluded-only update json_multi_ex");
    conn.execute("UPDATE public.json_multi_ex SET val = 'v2' WHERE id = 11")
        .await
        .expect("real update json_multi_ex");

    // PG text[] multi-column PK + multi-column excluded
    conn.execute("INSERT INTO public.pg_array_arg VALUES ('t1', 3, 'v1', 'x1', 'x2')")
        .await
        .expect("insert pg_array_arg");
    conn.execute("UPDATE public.pg_array_arg SET s1 = 'y1', s2 = 'y2' WHERE tenant_id = 't1' AND item_id = 3")
        .await
        .expect("excluded-only update pg_array_arg");
    conn.execute(
        "UPDATE public.pg_array_arg SET val = 'v2' WHERE tenant_id = 't1' AND item_id = 3",
    )
    .await
    .expect("real update pg_array_arg");
    conn.execute("DELETE FROM public.pg_array_arg WHERE tenant_id = 't1' AND item_id = 3")
        .await
        .expect("delete pg_array_arg");

    // PG text[] 1-element quoted PK + '{}' empty excluded
    conn.execute("INSERT INTO public.pg_array_single VALUES ('k-9', 'n1')")
        .await
        .expect("insert pg_array_single");
    conn.execute("UPDATE public.pg_array_single SET note = 'n2' WHERE \"row id\" = 'k-9'")
        .await
        .expect("update pg_array_single");

    let rows: Vec<UnifiedArgRow> = sqlx::query_as(
        "SELECT table_name, op, record_id::text, changed, after::text \
               FROM auditstream.changes ORDER BY id ASC",
    )
    .fetch_all(&mut conn)
    .await
    .expect("read unified-arg rows");

    assert_eq!(rows.len(), 12, "unexpected unified-arg rows: {rows:#?}");
    assert_eq!(rows[0].0, "plain_arg");
    assert_eq!(rows[0].1, "INSERT");
    assert_eq!(rows[0].2.as_deref(), Some(r#"{"id": 7}"#));
    assert!(!rows[0].4.as_deref().unwrap_or("").contains("secret"));
    assert_eq!(rows[1].1, "UPDATE");
    assert_eq!(rows[1].3, Some(vec!["status".to_owned()]));
    assert_eq!(rows[2].1, "DELETE");

    assert_eq!(rows[3].0, "plain_empty_arg");
    assert_eq!(rows[3].1, "INSERT");
    assert_eq!(rows[3].2, None);
    assert_eq!(rows[4].0, "plain_empty_arg");
    assert_eq!(rows[4].1, "UPDATE");
    assert_eq!(rows[4].2, None);
    assert_eq!(rows[4].3, Some(vec!["label".to_owned()]));

    assert_eq!(rows[5].0, "json_multi_ex");
    assert_eq!(rows[5].1, "INSERT");
    assert_eq!(rows[5].2.as_deref(), Some(r#"{"id": 11}"#));
    let after5 = rows[5].4.as_deref().unwrap_or("");
    assert!(!after5.contains("\"s1\"") && !after5.contains("\"s2\""));
    assert_eq!(rows[6].0, "json_multi_ex");
    assert_eq!(rows[6].1, "UPDATE");
    assert_eq!(rows[6].3, Some(vec!["val".to_owned()]));
    let after6 = rows[6].4.as_deref().unwrap_or("");
    assert!(!after6.contains("\"s1\"") && !after6.contains("\"s2\""));

    assert_eq!(rows[7].0, "pg_array_arg");
    let rec: Value =
        serde_json::from_str(rows[7].2.as_deref().expect("pg_array_arg record_id")).expect("json");
    assert_eq!(rec, serde_json::json!({"tenant_id": "t1", "item_id": 3}));
    let after7 = rows[7].4.as_deref().unwrap_or("");
    assert!(!after7.contains("\"s1\"") && !after7.contains("\"s2\""));
    assert_eq!(rows[8].1, "UPDATE");
    assert_eq!(rows[8].3, Some(vec!["val".to_owned()]));
    assert_eq!(rows[9].1, "DELETE");

    assert_eq!(rows[10].0, "pg_array_single");
    assert_eq!(rows[10].2.as_deref(), Some(r#"{"row id": "k-9"}"#));
    assert_eq!(rows[11].1, "UPDATE");
    assert_eq!(rows[11].3, Some(vec!["note".to_owned()]));

    conn.close().await.expect("close conn");
}
