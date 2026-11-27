//! Live PostgreSQL integration tests for `auditstream_pg::verify` (D7).

use auditstream_pg::ddl::{DEFAULT_SCHEMA, DdlOptions};
use auditstream_pg::migrations;
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_pg::verify::{self, FindingKind, Severity};
use auditstream_testkit::TestDb;
use sqlx::{AssertSqlSafe, Executor};

async fn install_fixture(db: &TestDb, tables: &[TableTriggerSpec]) {
    let _guard = TestDb::cluster_lock()
        .await
        .expect("acquire cluster lock for role setup");

    let statements =
        migrations::install_statements(&DdlOptions::new(DEFAULT_SCHEMA, vec![]), tables)
            .expect("compose D2+D3 install statements");

    let mut tx = db.pool().begin().await.expect("begin fixture tx");
    for stmt in statements {
        tx.execute(AssertSqlSafe(stmt))
            .await
            .expect("install capture DDL/triggers");
    }
    tx.commit().await.expect("commit fixture tx");
}

/// Installs the D2 history schema via [`migrations::install_statements`] and then attaches
/// unvalidated/raw triggers via [`triggers::generate`] inside a single transaction so `verify`
/// tests can simulate misconfigured or non-canonical live triggers.
async fn install_raw_triggers_fixture(db: &TestDb, tables: &[TableTriggerSpec]) {
    let _guard = TestDb::cluster_lock()
        .await
        .expect("acquire cluster lock for role setup");

    let ddl_stmts = migrations::install_statements(&DdlOptions::new(DEFAULT_SCHEMA, vec![]), &[])
        .expect("compose D2 schema statements");
    let trigger_stmts = triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, tables.to_vec()));

    let mut tx = db.pool().begin().await.expect("begin raw fixture tx");
    for stmt in ddl_stmts.into_iter().chain(trigger_stmts) {
        tx.execute(AssertSqlSafe(stmt))
            .await
            .expect("install raw capture DDL/triggers");
    }
    tx.commit().await.expect("commit raw fixture tx");
}

async fn create_invoice_table(db: &TestDb) {
    db.pool()
        .execute(
            "CREATE TABLE public.invoice ( \
               id int PRIMARY KEY, \
               status text NOT NULL, \
               password_hash text \
             )",
        )
        .await
        .expect("create public.invoice");
}

fn invoice_spec(excluded: Vec<&str>) -> TableTriggerSpec {
    TableTriggerSpec::new(
        "public",
        "invoice",
        vec!["id".to_owned()],
        excluded.into_iter().map(str::to_owned).collect(),
    )
}

#[tokio::test]
async fn fresh_install_reports_no_findings_and_runs_in_read_only_session() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify clean install");
    assert!(
        report.is_clean(),
        "expected no findings on fresh install, got: {:?}",
        report.findings
    );
    assert!(!report.has_errors());
    assert_eq!(
        report.default_partition.as_ref().map(|d| d.row_count),
        Some(0)
    );

    // Also verify read-only guarantee explicitly using a connection configured read-only.
    let mut ro_conn = db.pool().acquire().await.expect("acquire conn");
    let tx_read_only: String = sqlx::query_scalar("SHOW transaction_read_only")
        .fetch_one(&mut *ro_conn)
        .await
        .expect("show transaction_read_only");
    assert_eq!(tx_read_only, "off");
}

#[tokio::test]
async fn dropped_trigger_is_reported_as_missing_error() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    db.pool()
        .execute("DROP TRIGGER auditstream_delete ON public.invoice")
        .await
        .expect("drop trigger");

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify missing trigger");
    assert!(report.has_errors());
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.kind, FindingKind::Missing);
    assert_eq!(finding.severity, Severity::Error);
    assert_eq!(finding.table.as_deref(), Some("public.invoice"));
    assert!(finding.object.contains("auditstream_delete"));
    assert!(finding.detail.contains("auditstream_delete"));
}

#[tokio::test]
async fn missing_capture_function_and_history_table_are_reported() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec![])];
    install_fixture(&db, &specs).await;

    db.pool()
        .execute("DROP TABLE auditstream.changes CASCADE")
        .await
        .expect("drop history table");
    db.pool()
        .execute("DROP FUNCTION auditstream.capture() CASCADE")
        .await
        .expect("drop capture()");

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify missing schema objects");
    assert!(report.has_errors());

    assert!(report.findings.iter().any(|f| {
        f.kind == FindingKind::Missing
            && f.severity == Severity::Error
            && f.object == "auditstream.changes"
    }));
    assert!(report.findings.iter().any(|f| {
        f.kind == FindingKind::Missing
            && f.severity == Severity::Error
            && f.object == "auditstream.capture()"
    }));
}

#[tokio::test]
async fn disabled_and_replica_triggers_are_reported_as_disabled_errors() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    db.pool()
        .execute("ALTER TABLE public.invoice DISABLE TRIGGER auditstream_update")
        .await
        .expect("disable update trigger");
    db.pool()
        .execute("ALTER TABLE public.invoice ENABLE REPLICA TRIGGER auditstream_insert")
        .await
        .expect("set insert trigger to replica");

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify disabled triggers");
    assert!(report.has_errors());
    assert_eq!(report.findings.len(), 2);

    assert!(report.findings.iter().any(|f| {
        f.kind == FindingKind::Disabled
            && f.severity == Severity::Error
            && f.object == "public.invoice.auditstream_update"
            && f.detail.contains("'D'")
    }));
    assert!(report.findings.iter().any(|f| {
        f.kind == FindingKind::Disabled
            && f.severity == Severity::Error
            && f.object == "public.invoice.auditstream_insert"
            && f.detail.contains("'R'")
    }));
}

#[tokio::test]
async fn outdated_function_body_trigger_definition_and_excluded_columns_are_reported() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let installed_specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &installed_specs).await;

    // 1. Change excluded_columns in config without regenerating -> Outdated naming excluded columns.
    let changed_config = vec![invoice_spec(vec!["password_hash", "status"])];
    let excluded_report = verify::check(db.pool(), &changed_config)
        .await
        .expect("verify changed excluded_columns");
    assert!(excluded_report.has_errors());
    assert_eq!(excluded_report.findings.len(), 4);
    for finding in &excluded_report.findings {
        assert_eq!(finding.kind, FindingKind::Outdated);
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.detail.contains("excluded columns"),
            "unexpected detail: {}",
            finding.detail
        );
    }

    // 2. Replace capture() body with a modified function body -> Outdated.
    db.pool()
        .execute(
            "CREATE OR REPLACE FUNCTION auditstream.capture() \
             RETURNS trigger \
             LANGUAGE plpgsql \
             SECURITY DEFINER \
             SET search_path = pg_catalog, pg_temp \
             AS $$ BEGIN RETURN NULL; END $$",
        )
        .await
        .expect("modify capture() body");

    // 3. Restore the v0.1.0 whole-row comparison, which breaks UPDATE on json columns.
    db.pool()
        .execute(
            "CREATE OR REPLACE TRIGGER auditstream_update \
               AFTER UPDATE ON public.invoice \
               FOR EACH ROW \
               WHEN (OLD.* IS DISTINCT FROM NEW.*) \
               EXECUTE FUNCTION auditstream.capture('[\"id\"]', '[\"password_hash\"]')",
        )
        .await
        .expect("install legacy update trigger");

    let drift_report = verify::check(db.pool(), &installed_specs)
        .await
        .expect("verify body and trigger drift");
    assert!(drift_report.has_errors());
    assert!(drift_report.findings.iter().any(|f| {
        f.kind == FindingKind::Outdated
            && f.object == "auditstream.capture()"
            && f.detail.contains("function body")
    }));
    assert!(drift_report.findings.iter().any(|f| {
        f.kind == FindingKind::Outdated
            && f.object == "public.invoice.auditstream_update"
            && f.detail.contains("WHEN clause")
    }));
}

#[tokio::test]
async fn partitioned_table_warns_and_detects_disabled_child_partition_clone() {
    let db = TestDb::new().await.expect("create TestDb");
    db.pool()
        .execute(
            "CREATE TABLE public.orders ( \
               id int NOT NULL, \
               created_year int NOT NULL, \
               total numeric NOT NULL, \
               PRIMARY KEY (id, created_year) \
             ) PARTITION BY RANGE (created_year)",
        )
        .await
        .expect("create partitioned parent");
    db.pool()
        .execute(
            "CREATE TABLE public.orders_2026 PARTITION OF public.orders \
             FOR VALUES FROM (2026) TO (2027)",
        )
        .await
        .expect("create child partition");

    let specs = vec![
        TableTriggerSpec::new(
            "public",
            "orders",
            vec!["id".to_owned(), "created_year".to_owned()],
            vec![],
        )
        .partitioned(),
    ];
    install_fixture(&db, &specs).await;

    // An unrelated disabled user trigger cloned onto a child partition must NOT be flagged.
    db.pool()
        .execute(
            "CREATE FUNCTION public.touch_orders() RETURNS trigger LANGUAGE plpgsql \
             AS $$ BEGIN RETURN NEW; END $$",
        )
        .await
        .expect("create unrelated trigger fn");
    db.pool()
        .execute(
            "CREATE TRIGGER app_touch_orders BEFORE UPDATE ON public.orders \
             FOR EACH ROW EXECUTE FUNCTION public.touch_orders()",
        )
        .await
        .expect("create unrelated user trigger on partitioned parent");
    db.pool()
        .execute("ALTER TABLE public.orders_2026 DISABLE TRIGGER app_touch_orders")
        .await
        .expect("disable unrelated user trigger on child partition");

    // Clean partitioned install reports exactly one warning (no errors).
    let warn_report = verify::check(db.pool(), &specs)
        .await
        .expect("verify partitioned table");
    assert!(!warn_report.has_errors());
    assert_eq!(warn_report.findings.len(), 1);
    assert_eq!(warn_report.findings[0].kind, FindingKind::PartitionedTable);
    assert_eq!(warn_report.findings[0].severity, Severity::Warning);
    assert!(
        warn_report.findings[0]
            .detail
            .contains("INSERT, DELETE, and TRUNCATE")
    );

    // Disable the cloned row-level UPDATE trigger on the child partition only.
    db.pool()
        .execute("ALTER TABLE public.orders_2026 DISABLE TRIGGER auditstream_update")
        .await
        .expect("disable child partition update trigger clone");

    let clone_report = verify::check(db.pool(), &specs)
        .await
        .expect("verify disabled child partition clone");
    assert!(clone_report.has_errors());
    assert_eq!(clone_report.findings.len(), 2);
    // Stable ordering: Error before Warning.
    assert_eq!(clone_report.findings[0].severity, Severity::Error);
    assert_eq!(clone_report.findings[0].kind, FindingKind::Disabled);
    assert_eq!(
        clone_report.findings[0].object,
        "public.orders_2026.auditstream_update"
    );
    assert_eq!(clone_report.findings[1].severity, Severity::Warning);
    assert_eq!(clone_report.findings[1].kind, FindingKind::PartitionedTable);
}

#[tokio::test]
async fn rows_in_changes_default_are_reported_with_count_oldest_and_newest() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    // Insert two rows with historical timestamps outside the pre-created monthly partitions so
    // PostgreSQL routes them directly into auditstream.changes_default.
    db.pool()
        .execute(
            "INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
             VALUES \
               ('2020-01-15T10:00:00.000000Z'::timestamptz, 'app', 'legacy.one', 'postgres', 1), \
               ('2020-02-20T18:30:45.123456Z'::timestamptz, 'app', 'legacy.two', 'postgres', 1)",
        )
        .await
        .expect("seed default partition rows");

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify default partition rows");
    assert!(report.has_errors());
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.kind, FindingKind::DefaultPartitionRows);
    assert_eq!(finding.severity, Severity::Error);
    assert_eq!(finding.object, "auditstream.changes_default");
    assert!(finding.detail.contains("2 row(s)"));
    assert!(finding.detail.contains("2020-01-15T10:00:00.000000Z"));
    assert!(finding.detail.contains("2020-02-20T18:30:45.123456Z"));

    let status = report
        .default_partition
        .as_ref()
        .expect("default_partition stats present");
    assert_eq!(status.row_count, 2);
    assert_eq!(
        status.oldest_occurred_at.as_deref(),
        Some("2020-01-15T10:00:00.000000Z")
    );
    assert_eq!(
        status.newest_occurred_at.as_deref(),
        Some("2020-02-20T18:30:45.123456Z")
    );
}

#[tokio::test]
async fn installed_captures_discovers_top_level_tables_args_and_trigger_states() {
    let db = TestDb::new().await.expect("create TestDb");
    let empty = verify::installed_captures(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("installed_captures before install");
    assert_eq!(empty, Vec::new());

    create_invoice_table(&db).await;
    db.execute(
        "CREATE TABLE public.orders ( \
           id int NOT NULL, \
           created_year int NOT NULL, \
           total numeric NOT NULL, \
           PRIMARY KEY (id, created_year) \
         ) PARTITION BY RANGE (created_year)",
    )
    .await
    .expect("create partitioned orders");
    db.execute(
        "CREATE TABLE public.orders_2026 PARTITION OF public.orders \
         FOR VALUES FROM (2026) TO (2027)",
    )
    .await
    .expect("create child orders_2026");

    let specs = vec![
        invoice_spec(vec!["password_hash"]),
        TableTriggerSpec::new(
            "public",
            "orders",
            vec!["id".to_owned(), "created_year".to_owned()],
            vec![],
        )
        .partitioned(),
    ];
    install_fixture(&db, &specs).await;

    db.execute("ALTER TABLE public.invoice DISABLE TRIGGER auditstream_delete")
        .await
        .expect("disable invoice delete trigger");

    let installed = verify::installed_captures(db.pool(), DEFAULT_SCHEMA)
        .await
        .expect("installed_captures");
    assert_eq!(
        installed.len(),
        2,
        "child partition orders_2026 must not appear as a separate top-level entry"
    );

    let inv = &installed[0];
    assert_eq!(inv.schema, "public");
    assert_eq!(inv.table, "invoice");
    assert!(!inv.is_partitioned);
    assert_eq!(inv.primary_key, vec!["id".to_owned()]);
    assert_eq!(inv.excluded_columns, vec!["password_hash".to_owned()]);
    assert_eq!(
        inv.triggers,
        vec![
            ("auditstream_delete".to_owned(), "D".to_owned()),
            ("auditstream_insert".to_owned(), "O".to_owned()),
            ("auditstream_truncate".to_owned(), "O".to_owned()),
            ("auditstream_update".to_owned(), "O".to_owned()),
        ]
    );

    let ord = &installed[1];
    assert_eq!(ord.schema, "public");
    assert_eq!(ord.table, "orders");
    assert!(ord.is_partitioned);
    assert_eq!(
        ord.primary_key,
        vec!["id".to_owned(), "created_year".to_owned()]
    );
    assert_eq!(ord.excluded_columns, Vec::<String>::new());
    assert_eq!(ord.triggers.len(), 4);
    assert!(ord.triggers.iter().all(|(_, state)| state == "O"));
}

#[tokio::test]
async fn reports_error_when_primary_key_column_is_excluded() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let bad_specs = vec![invoice_spec(vec!["id"])];
    install_raw_triggers_fixture(&db, &bad_specs).await;

    // 1. Config-side exclusion of a PK column reports InvalidExclusion.
    let report = verify::check(db.pool(), &bad_specs)
        .await
        .expect("verify PK exclusion");
    assert!(report.has_errors());
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::InvalidExclusion
                && f.severity == Severity::Error
                && f.object == "public.invoice"
                && f.detail.contains("id")
        }),
        "expected InvalidExclusion finding for config PK exclusion, got: {:?}",
        report.findings
    );

    // 2. Live trigger excluding a PK column is also flagged even if config no longer excludes it.
    let clean_specs = vec![invoice_spec(vec![])];
    let live_report = verify::check(db.pool(), &clean_specs)
        .await
        .expect("verify live trigger PK exclusion");
    assert!(live_report.has_errors());
    assert!(
        live_report.findings.iter().any(|f| {
            f.kind == FindingKind::InvalidExclusion
                && f.severity == Severity::Error
                && f.object == "public.invoice.auditstream_update"
                && f.detail.contains("id")
        }),
        "expected InvalidExclusion finding for live trigger PK exclusion, got: {:?}",
        live_report.findings
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn reports_unsafe_privileges_and_invalid_ownership_across_history_objects() {
    use auditstream_pg::ddl::quote_ident;
    use sqlx::Connection;

    let db = TestDb::new().await.expect("create TestDb");
    let admin_url = db.admin_url().to_owned();
    create_invoice_table(&db).await;
    // Also exercise unsorted excluded_columns in triggers::generate + verify::check parity.
    let specs = vec![invoice_spec(vec!["password_hash", "status"])];
    let unsorted_specs = vec![invoice_spec(vec!["status", "password_hash"])];
    install_raw_triggers_fixture(&db, &unsorted_specs).await;

    // 1. Legitimate least-privilege app_role (USAGE + parent SELECT + EXECUTE log_event, zero
    //    child grants) must produce zero findings AND run `verify::check` itself cleanly!
    let app_role = format!("verify_app_{}", db.database_name());
    let app_pw = "verify_app_pw";
    db.execute(&format!(
        "CREATE ROLE {} LOGIN PASSWORD '{}'; \
         GRANT USAGE ON SCHEMA auditstream TO {}; \
         GRANT SELECT ON TABLE auditstream.changes TO {}; \
         GRANT EXECUTE ON FUNCTION auditstream.log_event(text, jsonb, jsonb) TO {};",
        quote_ident(&app_role),
        app_pw,
        quote_ident(&app_role),
        quote_ident(&app_role),
        quote_ident(&app_role),
    ))
    .await
    .expect("create least-privilege app_role");

    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    let app_url = format!("{scheme}://{app_role}:{app_pw}@{host_and_db}");
    let app_pool = auditstream_pg::connect(&app_url)
        .await
        .expect("connect app_pool");

    let clean_as_app = verify::check(&app_pool, &specs)
        .await
        .expect("verify::check must succeed as least-privilege app_role without child grants");
    assert!(
        clean_as_app.is_clean(),
        "legitimate app_role grants and unsorted triggers::generate excludes must be clean, got: {:?}",
        clean_as_app.findings
    );
    app_pool.close().await;

    // 2. Inject unsafe named-role ACLs (including column-level attacl and GRANT OPTION) across all
    //    7 history object types, plus wrong ownership across schema, changes (+ sequence),
    //    changes_default, capture(), resolve_actor(), and log_event().
    let app_q = quote_ident(&app_role);
    for sql in [
        format!("GRANT CREATE ON SCHEMA auditstream TO {app_q}"),
        format!("GRANT UPDATE (actor) ON TABLE auditstream.changes TO {app_q}"),
        format!("GRANT SELECT ON TABLE auditstream.changes TO {app_q} WITH GRANT OPTION"),
        format!("GRANT SELECT ON TABLE auditstream.changes_default TO {app_q}"),
        format!("GRANT USAGE ON SEQUENCE auditstream.changes_id_seq TO {app_q}"),
        format!("GRANT EXECUTE ON FUNCTION auditstream.capture() TO {app_q}"),
        format!("GRANT EXECUTE ON FUNCTION auditstream.resolve_actor() TO {app_q}"),
        format!(
            "GRANT EXECUTE ON FUNCTION auditstream.log_event(text, jsonb, jsonb) TO {app_q} WITH GRANT OPTION"
        ),
        "GRANT EXECUTE ON FUNCTION auditstream.log_event(text, jsonb, jsonb) TO PUBLIC".to_owned(),
        "ALTER SCHEMA auditstream OWNER TO CURRENT_USER".to_owned(),
        "ALTER TABLE auditstream.changes OWNER TO CURRENT_USER".to_owned(),
        "ALTER TABLE auditstream.changes_default OWNER TO CURRENT_USER".to_owned(),
        "ALTER FUNCTION auditstream.capture() OWNER TO CURRENT_USER".to_owned(),
        "ALTER FUNCTION auditstream.resolve_actor() OWNER TO CURRENT_USER".to_owned(),
        "ALTER FUNCTION auditstream.log_event(text, jsonb, jsonb) OWNER TO CURRENT_USER".to_owned(),
    ] {
        db.execute(&sql).await.expect(&sql);
    }

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify unsafe privileges and ownership");
    assert!(report.has_errors());

    for expected_obj in [
        "auditstream",
        "auditstream.changes",
        "auditstream.changes_default",
        "auditstream.changes_id_seq",
        "auditstream.capture()",
        "auditstream.resolve_actor()",
        "auditstream.log_event(text, jsonb, jsonb)",
    ] {
        assert!(
            report.findings.iter().any(|f| {
                f.kind == FindingKind::UnsafePrivileges
                    && f.severity == Severity::Error
                    && f.object == expected_obj
            }),
            "expected UnsafePrivileges finding for {expected_obj}, got: {:?}",
            report.findings
        );
        assert!(
            report.findings.iter().any(|f| {
                f.kind == FindingKind::InvalidOwnership
                    && f.severity == Severity::Error
                    && f.object == expected_obj
            }),
            "expected InvalidOwnership finding for {expected_obj}, got: {:?}",
            report.findings
        );
    }
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::UnsafePrivileges
                && f.object == "auditstream.changes"
                && f.detail.contains("UPDATE (actor)")
        }),
        "expected column-level attacl finding for UPDATE (actor), got: {:?}",
        report.findings
    );

    // 3. Re-applying DDL + triggers heals every ownership and ACL finding (including attacl).
    install_fixture(&db, &specs).await;
    let healed = verify::check(db.pool(), &specs)
        .await
        .expect("verify healed install");
    assert!(
        healed.is_clean(),
        "expected clean verify after re-applying DDL + triggers, got: {:?}",
        healed.findings
    );

    // 4. Dropping log_event is reported as Missing.
    db.execute("DROP FUNCTION auditstream.log_event(text, jsonb, jsonb)")
        .await
        .expect("drop log_event");
    let missing_log_event = verify::check(db.pool(), &specs)
        .await
        .expect("verify missing log_event");
    assert!(
        missing_log_event.findings.iter().any(|f| {
            f.kind == FindingKind::Missing
                && f.object == "auditstream.log_event(text, jsonb, jsonb)"
        }),
        "expected Missing finding for dropped log_event, got: {:?}",
        missing_log_event.findings
    );

    drop(db);
    let mut admin = sqlx::postgres::PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    let _ = admin
        .execute(AssertSqlSafe(format!("DROP ROLE IF EXISTS {app_q}")))
        .await;
    let _ = admin.close().await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn log_event_tampering_overloads_and_extra_proconfig_guc_are_reported() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    // 1. Drop and recreate log_event() without DEFAULT NULL, without SECURITY DEFINER, with STRICT,
    //    and with a tampered body (PG rejects removing defaults via CREATE OR REPLACE).
    db.execute_batch(
        "DROP FUNCTION auditstream.log_event(text, jsonb, jsonb); \
         CREATE FUNCTION auditstream.log_event( \
           action text, \
           target jsonb, \
           data jsonb \
         ) \
         RETURNS uuid \
         LANGUAGE plpgsql \
         STRICT \
         SET search_path = pg_catalog, pg_temp \
         AS $$ BEGIN RETURN gen_random_uuid(); END $$; \
         ALTER FUNCTION auditstream.log_event(text, jsonb, jsonb) OWNER TO auditstream_owner; \
         REVOKE ALL ON FUNCTION auditstream.log_event(text, jsonb, jsonb) FROM PUBLIC;",
    )
    .await
    .expect("tamper log_event()");

    // 2. Attach unexpected extra GUCs via ALTER FUNCTION ... SET alongside search_path on all 3,
    //    weaken resolve_actor() volatility to IMMUTABLE, install a shadowing sibling overload,
    //    and narrow auditstream_update to UPDATE OF id (tgattr bypass).
    db.execute_batch(
        "ALTER FUNCTION auditstream.resolve_actor() IMMUTABLE \
           SET auditstream.actor = '{\"type\":\"user\",\"id\":\"spoof\"}'; \
         ALTER FUNCTION auditstream.capture() SET work_mem = '8MB'; \
         ALTER FUNCTION auditstream.log_event(text, jsonb, jsonb) SET work_mem = '8MB'; \
         CREATE FUNCTION auditstream.log_event(action text, target text, data text) \
           RETURNS uuid LANGUAGE sql AS $$ SELECT gen_random_uuid() $$; \
         CREATE OR REPLACE TRIGGER auditstream_update \
           AFTER UPDATE OF id ON public.invoice \
           FOR EACH ROW \
           EXECUTE FUNCTION auditstream.capture('[\"id\"]', '[\"password_hash\"]');",
    )
    .await
    .expect("apply function, overload, and tgattr tampering");

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify tampered functions");
    assert!(report.has_errors());

    let log_event_outdated = report
        .findings
        .iter()
        .find(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.log_event(text, jsonb, jsonb)"
        })
        .expect("expected Outdated finding for tampered log_event()");
    for needle in [
        "function body",
        "SECURITY DEFINER",
        "STRICT",
        "default argument count",
        "work_mem=",
    ] {
        assert!(
            log_event_outdated.detail.contains(needle),
            "missing {needle:?} in detail: {}",
            log_event_outdated.detail
        );
    }

    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.resolve_actor()"
                && f.detail.contains("auditstream.actor=")
                && f.detail.contains("volatility")
        }),
        "expected Outdated finding for extra GUC + volatility on resolve_actor(), got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.capture()"
                && f.detail.contains("work_mem=")
        }),
        "expected Outdated finding for extra GUC on capture(), got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::TamperedHistory
                && f.object == "auditstream.log_event(action text, target text, data text)"
        }),
        "expected TamperedHistory finding for shadowing log_event overload, got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "public.invoice.auditstream_update"
                && f.detail.contains("UPDATE OF column filter")
        }),
        "expected Outdated finding for UPDATE OF tgattr filter, got: {:?}",
        report.findings
    );

    // 3. Remove the rogue overload and re-apply DDL + triggers -> completely clean.
    db.execute("DROP FUNCTION auditstream.log_event(text, text, text)")
        .await
        .expect("drop rogue overload");
    install_fixture(&db, &specs).await;
    let healed = verify::check(db.pool(), &specs)
        .await
        .expect("verify healed functions");
    assert!(
        healed.is_clean(),
        "expected clean report after healing functions, got: {:?}",
        healed.findings
    );
}

#[tokio::test]
async fn excluded_column_rename_or_drop_is_reported_as_invalid_exclusion() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    // 1. Rename excluded column on the live table without updating config or triggers.
    db.execute("ALTER TABLE public.invoice RENAME COLUMN password_hash TO secret_hash")
        .await
        .expect("rename password_hash");

    let stale_report = verify::check(db.pool(), &specs)
        .await
        .expect("verify renamed excluded column");
    assert!(stale_report.has_errors());
    assert!(
        stale_report.findings.iter().any(|f| {
            f.kind == FindingKind::InvalidExclusion
                && f.object == "public.invoice"
                && f.detail.contains("password_hash")
                && f.detail.contains("do not exist")
        }),
        "expected config-level InvalidExclusion for renamed column, got: {:?}",
        stale_report.findings
    );
    assert!(
        stale_report.findings.iter().any(|f| {
            f.kind == FindingKind::InvalidExclusion
                && f.object == "public.invoice.auditstream_update"
                && f.detail.contains("password_hash")
                && f.detail.contains("non-existent")
        }),
        "expected live-trigger InvalidExclusion for renamed column, got: {:?}",
        stale_report.findings
    );

    // 2. Even after updating config to `secret_hash`, stale live triggers still fail InvalidExclusion.
    let updated_specs = vec![invoice_spec(vec!["secret_hash"])];
    let live_only_report = verify::check(db.pool(), &updated_specs)
        .await
        .expect("verify live trigger drift after config update");
    assert!(live_only_report.has_errors());
    assert!(
        !live_only_report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::InvalidExclusion && f.object == "public.invoice"),
        "valid updated config must not report table-level InvalidExclusion: {:?}",
        live_only_report.findings
    );
    assert!(
        live_only_report.findings.iter().any(|f| {
            f.kind == FindingKind::InvalidExclusion
                && f.object == "public.invoice.auditstream_insert"
                && f.detail.contains("password_hash")
        }),
        "expected live-trigger InvalidExclusion for stale trigger exclusion, got: {:?}",
        live_only_report.findings
    );

    // 3. Re-installing triggers heals rename drift; dropping an excluded column fails again.
    install_fixture(&db, &updated_specs).await;
    let clean = verify::check(db.pool(), &updated_specs)
        .await
        .expect("verify healed rename");
    assert!(clean.is_clean(), "got: {:?}", clean.findings);

    db.execute("ALTER TABLE public.invoice DROP COLUMN secret_hash")
        .await
        .expect("drop secret_hash");
    let dropped_report = verify::check(db.pool(), &updated_specs)
        .await
        .expect("verify dropped excluded column");
    assert!(
        dropped_report.findings.iter().any(|f| {
            f.kind == FindingKind::InvalidExclusion
                && f.object == "public.invoice"
                && f.detail.contains("secret_hash")
        }),
        "expected InvalidExclusion for dropped column, got: {:?}",
        dropped_report.findings
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn detached_or_missing_default_partition_and_broken_history_indexes_are_reported() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    // 1. Detach changes_default (leaves a standalone table named `changes_default`).
    db.execute("ALTER TABLE auditstream.changes DETACH PARTITION auditstream.changes_default")
        .await
        .expect("detach changes_default");
    let detached = verify::check(db.pool(), &specs)
        .await
        .expect("verify detached default partition");
    assert!(detached.has_errors());
    assert!(detached.default_partition.is_none());
    assert!(
        detached.findings.iter().any(|f| {
            f.kind == FindingKind::Missing
                && f.object == "auditstream.changes_default"
                && f.detail.contains("unattached")
        }),
        "expected Missing finding for detached changes_default, got: {:?}",
        detached.findings
    );

    // 2. Drop standalone changes_default completely -> Missing.
    db.execute("DROP TABLE auditstream.changes_default")
        .await
        .expect("drop standalone changes_default");
    let dropped = verify::check(db.pool(), &specs)
        .await
        .expect("verify dropped default partition");
    assert!(
        dropped.findings.iter().any(|f| {
            f.kind == FindingKind::Missing
                && f.object == "auditstream.changes_default"
                && f.detail.contains("missing")
        }),
        "expected Missing finding for dropped changes_default, got: {:?}",
        dropped.findings
    );

    // 3. Attach a DEFAULT partition with a non-canonical name -> Outdated + still inspects rows.
    db.execute_batch(
        "CREATE TABLE auditstream.changes_fallback PARTITION OF auditstream.changes DEFAULT; \
         ALTER TABLE auditstream.changes_fallback OWNER TO auditstream_owner; \
         INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
         VALUES ('2020-01-15T10:00:00.000000Z'::timestamptz, 'app', 'fallback.row', 'postgres', 1);",
    )
    .await
    .expect("create renamed default partition with row");
    let renamed = verify::check(db.pool(), &specs)
        .await
        .expect("verify renamed default partition");
    assert!(
        renamed.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.changes_default"
                && f.detail.contains("changes_fallback")
        }),
        "expected Outdated finding for renamed DEFAULT partition, got: {:?}",
        renamed.findings
    );
    assert!(
        renamed.findings.iter().any(|f| {
            f.kind == FindingKind::DefaultPartitionRows
                && f.object == "auditstream.changes_fallback"
        }),
        "expected DefaultPartitionRows on renamed DEFAULT partition, got: {:?}",
        renamed.findings
    );
    assert_eq!(
        renamed.default_partition.as_ref().map(|d| d.row_count),
        Some(1)
    );
    db.execute("DROP TABLE auditstream.changes_fallback")
        .await
        .expect("drop renamed default partition");

    // 3b. Sub-partitioned changes_default (`relkind = 'p'`) reports Outdated relkind AND counts
    //     rows via constant leaf OIDs.
    db.execute_batch(
        "CREATE TABLE auditstream.changes_default PARTITION OF auditstream.changes DEFAULT \
           PARTITION BY RANGE (occurred_at); \
         CREATE TABLE auditstream.changes_default_leaf PARTITION OF auditstream.changes_default \
           DEFAULT; \
         ALTER TABLE auditstream.changes_default OWNER TO auditstream_owner; \
         ALTER TABLE auditstream.changes_default_leaf OWNER TO auditstream_owner; \
         INSERT INTO auditstream.changes (occurred_at, source, op, db_user, schema_version) \
         VALUES ('2020-03-10T12:00:00.000000Z'::timestamptz, 'app', 'sub.row', 'postgres', 1);",
    )
    .await
    .expect("create sub-partitioned changes_default");
    let sub_part = verify::check(db.pool(), &specs)
        .await
        .expect("verify sub-partitioned changes_default");
    assert!(
        sub_part.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.changes_default"
                && f.detail.contains("relkind 'p'")
        }),
        "expected Outdated relkind finding for sub-partitioned changes_default, got: {:?}",
        sub_part.findings
    );
    assert_eq!(
        sub_part.default_partition.as_ref().map(|d| d.row_count),
        Some(1)
    );
    assert!(
        sub_part.findings.iter().any(|f| {
            f.kind == FindingKind::DefaultPartitionRows && f.object == "auditstream.changes_default"
        }),
        "expected DefaultPartitionRows for sub-partitioned changes_default, got: {:?}",
        sub_part.findings
    );
    db.execute("DROP TABLE auditstream.changes_default CASCADE")
        .await
        .expect("drop sub-partitioned changes_default");

    // 4. Break all three required history indexes:
    //    - Drop changes_occurred_at_brin_idx -> Missing
    //    - Recreate changes_xact_id_idx with DESC ordering + INCLUDE column -> Outdated
    //    - Recreate changes_lookup_idx with wrong AM and missing predicate -> Outdated
    db.execute("DROP INDEX auditstream.changes_occurred_at_brin_idx")
        .await
        .expect("drop brin idx");
    db.execute_batch(
        "DROP INDEX auditstream.changes_xact_id_idx; \
         CREATE INDEX changes_xact_id_idx ON auditstream.changes (xact_id DESC, id) INCLUDE (op);",
    )
    .await
    .expect("alter xact_id idx");
    db.execute_batch(
        "DROP INDEX auditstream.changes_lookup_idx; \
         CREATE INDEX changes_lookup_idx ON auditstream.changes USING btree (record_id);",
    )
    .await
    .expect("alter lookup idx");

    let idx_report = verify::check(db.pool(), &specs)
        .await
        .expect("verify broken indexes");
    assert!(
        idx_report.findings.iter().any(|f| {
            f.kind == FindingKind::Missing && f.object == "auditstream.changes_occurred_at_brin_idx"
        }),
        "expected Missing brin index, got: {:?}",
        idx_report.findings
    );
    assert!(
        idx_report.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.changes_xact_id_idx"
                && f.detail.contains("INCLUDE")
                && f.detail.contains("sort/nulls")
        }),
        "expected Outdated xact_id index for INCLUDE + DESC, got: {:?}",
        idx_report.findings
    );
    assert!(
        idx_report.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.changes_lookup_idx"
                && f.detail.contains("access method")
                && f.detail.contains("predicate")
        }),
        "expected Outdated lookup index, got: {:?}",
        idx_report.findings
    );

    // 5. Partitioned index created ON ONLY (indisvalid = false) -> Outdated, and dropping
    //    `changes_pkey` -> Missing.
    db.execute_batch(
        "CREATE INDEX changes_occurred_at_brin_idx ON ONLY auditstream.changes USING brin (occurred_at); \
         ALTER TABLE auditstream.changes DROP CONSTRAINT changes_pkey;",
    )
    .await
    .expect("create invalid ON ONLY index and drop changes_pkey");
    let invalid_idx_report = verify::check(db.pool(), &specs)
        .await
        .expect("verify invalid ON ONLY index and missing PK");
    assert!(
        invalid_idx_report.findings.iter().any(|f| {
            f.kind == FindingKind::Outdated
                && f.object == "auditstream.changes_occurred_at_brin_idx"
                && f.detail.contains("invalid")
        }),
        "expected Outdated invalid index finding, got: {:?}",
        invalid_idx_report.findings
    );
    assert!(
        invalid_idx_report
            .findings
            .iter()
            .any(|f| { f.kind == FindingKind::Missing && f.object == "auditstream.changes_pkey" }),
        "expected Missing changes_pkey finding, got: {:?}",
        invalid_idx_report.findings
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn history_tampering_triggers_fk_rules_and_rls_on_parent_and_child_partitions_are_reported() {
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    install_fixture(&db, &specs).await;

    // 1. Stealth BEFORE INSERT triggers on both parent `changes` and child `changes_default`,
    //    plus an internal RI trigger via FOREIGN KEY ... ON DELETE CASCADE on `changes`.
    db.execute_batch(
        "CREATE FUNCTION public.drop_audit_rows() RETURNS trigger LANGUAGE plpgsql \
         AS $$ BEGIN RETURN NULL; END $$; \
         CREATE TRIGGER stealth_parent_drop BEFORE INSERT ON auditstream.changes \
         FOR EACH ROW EXECUTE FUNCTION public.drop_audit_rows(); \
         CREATE TRIGGER stealth_child_drop BEFORE INSERT ON auditstream.changes_default \
         FOR EACH ROW EXECUTE FUNCTION public.drop_audit_rows(); \
         CREATE TABLE public.kill_switch (op text PRIMARY KEY); \
         ALTER TABLE auditstream.changes \
           ADD CONSTRAINT fk_kill_switch FOREIGN KEY (op) REFERENCES public.kill_switch(op) ON DELETE CASCADE;",
    )
    .await
    .expect("install stealth triggers and FK on history");

    // 2. Stealth rewrite rules on both parent `changes` and child `changes_default`.
    db.execute_batch(
        "CREATE RULE stealth_parent_rule AS ON INSERT TO auditstream.changes DO INSTEAD NOTHING; \
         CREATE RULE stealth_child_rule AS ON DELETE TO auditstream.changes_default DO INSTEAD NOTHING;",
    )
    .await
    .expect("install stealth rewrite rules");

    // 3. Forced RLS on parent `changes` and child `changes_default` (plus unexpected child RLS
    //    policy on `changes_default` and zero PERMISSIVE SELECT policies on parent `changes`).
    db.execute_batch(
        "ALTER TABLE auditstream.changes ENABLE ROW LEVEL SECURITY; \
         ALTER TABLE auditstream.changes FORCE ROW LEVEL SECURITY; \
         ALTER TABLE auditstream.changes_default FORCE ROW LEVEL SECURITY; \
         CREATE POLICY stealth_child_pol ON auditstream.changes_default FOR ALL USING (false);",
    )
    .await
    .expect("enable unexpected RLS on history");

    let report = verify::check(db.pool(), &specs)
        .await
        .expect("verify tampered history");
    assert!(report.has_errors());

    let parent_trig_matches = report
        .findings
        .iter()
        .filter(|f| {
            f.kind == FindingKind::TamperedHistory && f.detail.contains("stealth_parent_drop")
        })
        .count();
    assert_eq!(
        parent_trig_matches, 1,
        "parent trigger must be reported once (not duplicated per child clone): {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::TamperedHistory
                && f.object
                    .starts_with("auditstream.changes.RI_ConstraintTrigger_")
        }),
        "expected TamperedHistory finding for FK internal RI trigger, got: {:?}",
        report.findings
    );

    for (kind, object, needle) in [
        (
            FindingKind::TamperedHistory,
            "auditstream.changes.stealth_parent_drop",
            "unexpected trigger",
        ),
        (
            FindingKind::TamperedHistory,
            "auditstream.changes_default.stealth_child_drop",
            "unexpected trigger",
        ),
        (
            FindingKind::TamperedHistory,
            "auditstream.changes.stealth_parent_rule",
            "unexpected rewrite rule",
        ),
        (
            FindingKind::TamperedHistory,
            "auditstream.changes_default.stealth_child_rule",
            "unexpected rewrite rule",
        ),
        (
            FindingKind::ForcedRls,
            "auditstream.changes",
            "FORCE ROW LEVEL SECURITY",
        ),
        (
            FindingKind::ForcedRls,
            "auditstream.changes_default",
            "FORCE ROW LEVEL SECURITY",
        ),
        (
            FindingKind::RlsWithoutSelectPolicy,
            "auditstream.changes",
            "zero PERMISSIVE SELECT policies",
        ),
        (
            FindingKind::TamperedHistory,
            "auditstream.changes_default",
            "1 RLS policy/policies",
        ),
    ] {
        assert!(
            report.findings.iter().any(|f| {
                f.kind == kind
                    && f.severity == Severity::Error
                    && f.object == object
                    && f.detail.contains(needle)
            }),
            "expected {kind:?} for {object} containing {needle:?}, got: {:?}",
            report.findings
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn owner_role_and_session_replication_role_security_checks_detect_cluster_tampering() {
    let _lock = TestDb::cluster_lock().await.expect("acquire cluster lock");
    let db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    let statements =
        migrations::install_statements(&DdlOptions::new(DEFAULT_SCHEMA, vec![]), &specs)
            .expect("compose install statements");
    db.execute_statements(&statements)
        .await
        .expect("install capture DDL/triggers");

    // Apply cluster-wide mutations inside an uncommitted transaction using two distinct short
    // role names (< 63 bytes to avoid NAMEDATALEN truncation and circular membership SQLSTATE 0LP01),
    // verify inside the same transaction, and roll back atomically.
    let member_role = format!("vt_mem_{}", db.database_name());
    let parent_role = format!("vt_par_{}", db.database_name());
    let member_q = auditstream_pg::ddl::quote_ident(&member_role);
    let parent_q = auditstream_pg::ddl::quote_ident(&parent_role);
    let mut tx = db.pool().begin().await.expect("begin tamper tx");
    for stmt in [
        format!("CREATE ROLE {member_q} NOLOGIN"),
        format!("CREATE ROLE {parent_q} NOLOGIN"),
        "ALTER ROLE auditstream_owner LOGIN SUPERUSER CREATEROLE CREATEDB REPLICATION BYPASSRLS"
            .to_owned(),
        format!("GRANT auditstream_owner TO {member_q}"),
        format!("GRANT {parent_q} TO auditstream_owner"),
        format!(
            "GRANT SET, ALTER SYSTEM ON PARAMETER session_replication_role TO {member_q} WITH GRANT OPTION"
        ),
        "GRANT SET ON PARAMETER session_replication_role TO PUBLIC".to_owned(),
        "SET LOCAL session_replication_role = 'replica'".to_owned(),
        format!("ALTER ROLE {member_q} SET session_replication_role = 'replica'"),
    ] {
        tx.execute(AssertSqlSafe(stmt))
            .await
            .expect("apply uncommitted cluster tamper");
    }

    let report = verify::check_in_transaction(
        &mut tx,
        &verify::VerifyOptions::new(DEFAULT_SCHEMA, specs.clone()),
    )
    .await
    .expect("verify inside uncommitted tamper tx");
    tx.rollback().await.expect("rollback cluster tamper");

    assert!(report.has_errors());
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::UnsafePrivileges
                && f.object == "auditstream_owner"
                && f.detail.contains("LOGIN")
                && f.detail.contains("SUPERUSER")
                && f.detail.contains("CREATEROLE")
                && f.detail.contains("BYPASSRLS")
        }),
        "expected unsafe role attribute finding, got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::UnsafePrivileges
                && f.object == "auditstream_owner"
                && f.detail.contains("unsafe role membership")
                && f.detail.contains(&member_role)
        }),
        "expected unsafe role membership finding, got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::UnsafePrivileges
                && f.object == "auditstream_owner"
                && f.detail.contains("unexpectedly a member of role")
                && f.detail.contains(&parent_role)
        }),
        "expected unexpected granted role finding, got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::UnsafePrivileges
                && f.object == "session_replication_role"
                && f.detail.contains("PUBLIC (SET)")
                && f.detail
                    .contains(&format!("{member_role} (ALTER SYSTEM WITH GRANT OPTION)"))
                && f.detail
                    .contains(&format!("{member_role} (SET WITH GRANT OPTION)"))
        }),
        "expected session_replication_role parameter ACL finding, got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::UnsafePrivileges
                && f.object == "session_replication_role"
                && f.detail
                    .contains("active session `session_replication_role` is `replica`")
        }),
        "expected active session_replication_role finding, got: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::UnsafePrivileges
                && f.object == "session_replication_role"
                && f.detail.contains("database/role default setting")
                && f.detail.contains(&member_role)
        }),
        "expected pg_db_role_setting session_replication_role finding, got: {:?}",
        report.findings
    );

    // Confirm rollback left cluster clean.
    let clean = verify::check(db.pool(), &specs)
        .await
        .expect("verify clean after rollback");
    assert!(
        clean.is_clean(),
        "expected clean after rollback, got: {:?}",
        clean.findings
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn non_superuser_install_passes_verify() {
    use auditstream_pg::ddl::quote_ident;
    use sqlx::Connection;

    let _lock = TestDb::cluster_lock().await.expect("acquire cluster lock");
    let db = TestDb::non_superuser()
        .await
        .expect("create non-superuser TestDb");
    let admin_url = db.admin_url().to_owned();
    let app_role = format!("ns_app_{}", db.database_name());
    let app_pw = uuid::Uuid::new_v4().simple().to_string();
    let app_q = quote_ident(&app_role);

    // 1. Drop pre-existing `auditstream_owner` and confirm missing owner role is reported before
    //    install, while also preparing a least-privilege app role for post-install verification.
    let (without_query, query) = match admin_url.split_once('?') {
        Some((before, q)) => (before, Some(q)),
        None => (admin_url.as_str(), None),
    };
    let (prefix, _) = without_query
        .rsplit_once('/')
        .expect("admin_url has db suffix");
    let super_db_url = match query {
        Some(q) => format!("{prefix}/{}?{q}", db.database_name()),
        None => format!("{prefix}/{}", db.database_name()),
    };
    let super_pool = auditstream_pg::connect(&super_db_url)
        .await
        .expect("connect superuser to test db");
    super_pool
        .execute("DROP ROLE IF EXISTS auditstream_owner")
        .await
        .expect("drop pre-existing auditstream_owner");
    super_pool
        .execute(AssertSqlSafe(format!(
            "CREATE ROLE {app_q} LOGIN PASSWORD '{app_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE"
        )))
        .await
        .expect("create app_role");

    let missing_report = verify::check(&super_pool, &[])
        .await
        .expect("verify missing auditstream_owner");
    assert!(
        missing_report.findings.iter().any(|f| {
            f.kind == FindingKind::Missing
                && f.severity == Severity::Error
                && f.object == "auditstream_owner"
        }),
        "expected Missing finding for absent auditstream_owner, got: {:?}",
        missing_report.findings
    );

    // 2. Non-superuser CREATEROLE migrator installs DDL + triggers (creating `auditstream_owner`
    //    with its mandatory PG 16+ ADMIN-only row in `pg_auth_members`) and grants read access to
    //    `app_role`.
    create_invoice_table(&db).await;
    let specs = vec![invoice_spec(vec!["password_hash"])];
    let statements = migrations::install_statements(
        &DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]),
        &specs,
    )
    .expect("compose install statements");
    db.execute_statements(&statements)
        .await
        .expect("non-superuser install");

    // 3. Both least-privilege `app_role` and superuser see a completely clean verify report on
    //    PG 15-19.
    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    let app_url = format!("{scheme}://{app_role}:{app_pw}@{host_and_db}");
    let app_pool = auditstream_pg::connect(&app_url)
        .await
        .expect("connect app_pool");
    let app_report = verify::check(&app_pool, &specs)
        .await
        .expect("verify non-superuser install as app_role");
    assert!(
        app_report.is_clean(),
        "expected clean verify as app_role on PG 15-19 non-superuser install, got: {:?}",
        app_report.findings
    );
    app_pool.close().await;

    let super_report = verify::check(&super_pool, &specs)
        .await
        .expect("verify non-superuser install as superuser");
    assert!(
        super_report.is_clean(),
        "expected clean verify as superuser on PG 15-19 non-superuser install, got: {:?}",
        super_report.findings
    );

    // 4. On PG 16+, verify that granting the CREATEROLE migrator to `app_role` via either
    //    INHERIT or ADMIN-only (indirect ADMIN escalation to `auditstream_owner`) is caught inside
    //    an uncommitted transaction.
    let pg_version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(&super_pool)
        .await
        .expect("read server_version_num");
    if pg_version >= 160_000 {
        let migrator_q = quote_ident(db.role_name().expect("non-superuser role name"));
        for grant_opts in [
            "WITH INHERIT TRUE, SET FALSE",
            "WITH ADMIN TRUE, INHERIT FALSE, SET FALSE",
        ] {
            let mut tx = super_pool.begin().await.expect("begin pg16 indirect tx");
            tx.execute(AssertSqlSafe(format!(
                "GRANT {migrator_q} TO {app_q} {grant_opts}"
            )))
            .await
            .expect("grant migrator to app_role");
            let indirect_report = verify::check_in_transaction(
                &mut tx,
                &verify::VerifyOptions::new(DEFAULT_SCHEMA, specs.clone()),
            )
            .await
            .expect("verify indirect ADMIN escalation");
            tx.rollback().await.expect("rollback indirect tx");
            assert!(
                indirect_report.findings.iter().any(|f| {
                    f.kind == FindingKind::UnsafePrivileges
                        && f.object == "auditstream_owner"
                        && f.detail.contains("indirect ADMIN")
                        && f.detail.contains(&app_role)
                }),
                "expected indirect ADMIN UnsafePrivileges finding for `{grant_opts}` on PG 16+, got: {:?}",
                indirect_report.findings
            );
        }
    }

    super_pool.close().await;
    drop(db);
    let mut admin = sqlx::postgres::PgConnection::connect(&admin_url)
        .await
        .expect("admin cleanup");
    let _ = admin
        .execute(AssertSqlSafe(format!("DROP ROLE IF EXISTS {app_q}")))
        .await;
    let _ = admin.execute("DROP ROLE IF EXISTS auditstream_owner").await;
    let _ = admin.close().await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rls_safety_checks_detect_forced_missing_select_hidden_default_and_bypassed_app_roles() {
    use auditstream_pg::ddl::quote_ident;
    use auditstream_pg::partitions::{self, PartitionError};

    let _lock = TestDb::cluster_lock().await.expect("acquire cluster lock");
    let mut db = TestDb::new().await.expect("create TestDb");
    create_invoice_table(&db).await;

    let suffix = db.database_name().to_owned();
    let app_role = format!("rls_app_{suffix}");
    let exporter_role = format!("rls_exp_{suffix}");
    let bypass_role = format!("rls_byp_{suffix}");
    let set_bypass_role = format!("rls_sbp_{suffix}");
    let super_role = format!("rls_sup_{suffix}");
    let member_role = format!("rls_mem_{suffix}");
    for role in [
        &app_role,
        &exporter_role,
        &bypass_role,
        &set_bypass_role,
        &super_role,
        &member_role,
    ] {
        db.track_role(role);
    }

    let app_pw = uuid::Uuid::new_v4().simple().to_string();
    let exp_pw = uuid::Uuid::new_v4().simple().to_string();
    let app_q = quote_ident(&app_role);
    let exp_q = quote_ident(&exporter_role);
    let byp_q = quote_ident(&bypass_role);
    let set_byp_q = quote_ident(&set_bypass_role);
    let sup_q = quote_ident(&super_role);
    let mem_q = quote_ident(&member_role);

    db.execute_batch(&format!(
        "CREATE ROLE {app_q} LOGIN PASSWORD '{app_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS; \
         CREATE ROLE {exp_q} LOGIN PASSWORD '{exp_pw}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS; \
         CREATE ROLE {byp_q} NOLOGIN NOSUPERUSER BYPASSRLS; \
         CREATE ROLE {set_byp_q} NOLOGIN NOSUPERUSER NOBYPASSRLS; \
         CREATE ROLE {sup_q} NOLOGIN SUPERUSER; \
         CREATE ROLE {mem_q} NOLOGIN NOSUPERUSER NOBYPASSRLS; \
         GRANT {byp_q} TO {set_byp_q};"
    ))
    .await
    .expect("create RLS test roles");

    let specs = vec![invoice_spec(vec!["password_hash"])];
    let stmts = migrations::install_statements(
        &DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]),
        &specs,
    )
    .expect("compose install statements");
    db.execute_statements(&stmts)
        .await
        .expect("install capture DDL and triggers");

    db.execute_batch(&format!(
        "GRANT USAGE ON SCHEMA public TO {app_q}; \
         GRANT SELECT, INSERT, UPDATE, DELETE ON public.invoice TO {app_q}; \
         GRANT USAGE ON SCHEMA auditstream TO {exp_q}; \
         GRANT SELECT ON auditstream.changes TO {exp_q};"
    ))
    .await
    .expect("grant invoice and exporter privileges");

    let base = db.url();
    let (scheme, rest) = base.split_once("://").expect("valid postgres URL");
    let (_, host_and_db) = rest.split_once('@').expect("URL has credentials");
    let app_pool =
        auditstream_pg::connect(&format!("{scheme}://{app_role}:{app_pw}@{host_and_db}"))
            .await
            .expect("connect app_pool");
    let exp_pool = auditstream_pg::connect(&format!(
        "{scheme}://{exporter_role}:{exp_pw}@{host_and_db}"
    ))
    .await
    .expect("connect exp_pool");

    // 1. FORCE RLS on parent `changes` breaks app_user DML writes with 42501 and triggers ForcedRls.
    db.execute_batch(
        "ALTER TABLE auditstream.changes ENABLE ROW LEVEL SECURITY; \
         ALTER TABLE auditstream.changes FORCE ROW LEVEL SECURITY;",
    )
    .await
    .expect("enable and force RLS on parent changes");

    let write_err = sqlx::query("INSERT INTO public.invoice (id, status) VALUES (1, 'open')")
        .execute(&app_pool)
        .await
        .expect_err("FORCE RLS on changes must abort app_user writes");
    assert_eq!(
        write_err
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501"),
        "expected SQLSTATE 42501 under FORCE RLS, got: {write_err}"
    );

    let forced_parent = verify::check(db.pool(), &specs)
        .await
        .expect("verify forced parent RLS");
    assert!(
        forced_parent.findings.iter().any(|f| {
            f.kind == FindingKind::ForcedRls
                && f.severity == Severity::Error
                && f.object == "auditstream.changes"
        }),
        "expected ForcedRls on parent changes, got: {:?}",
        forced_parent.findings
    );
    assert!(
        forced_parent.findings.iter().any(|f| {
            f.kind == FindingKind::RlsWithoutSelectPolicy
                && f.severity == Severity::Error
                && f.object == "auditstream.changes"
        }),
        "expected RlsWithoutSelectPolicy with 0 policies, got: {:?}",
        forced_parent.findings
    );

    // 2. Remove FORCE on parent (confirming live non-superuser DML capture now succeeds under
    //    non-forced RLS!), enable FORCE on `changes_default` AND a monthly partition -> exactly 2
    //    ForcedRls findings (one per child partition).
    let monthly_part: String = sqlx::query_scalar(
        "SELECT child.relname \
           FROM pg_catalog.pg_inherits i \
           JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent \
           JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace \
           JOIN pg_catalog.pg_class child ON child.oid = i.inhrelid \
          WHERE pn.nspname = 'auditstream' \
            AND parent.relname = 'changes' \
            AND child.relname ~ '^changes_[0-9]{4}_[0-9]{2}$' \
          ORDER BY child.relname ASC LIMIT 1",
    )
    .fetch_one(db.pool())
    .await
    .expect("find attached monthly partition");

    db.execute("ALTER TABLE auditstream.changes NO FORCE ROW LEVEL SECURITY")
        .await
        .expect("clear parent FORCE RLS");
    sqlx::query("INSERT INTO public.invoice (id, status) VALUES (1, 'open')")
        .execute(&app_pool)
        .await
        .expect("non-superuser app_user DML capture must succeed under non-forced ENABLE RLS");

    db.execute_batch(&format!(
        "ALTER TABLE auditstream.changes_default FORCE ROW LEVEL SECURITY; \
         ALTER TABLE auditstream.{} FORCE ROW LEVEL SECURITY;",
        quote_ident(&monthly_part)
    ))
    .await
    .expect("force RLS on child partitions");

    let forced_children = verify::check(db.pool(), &specs)
        .await
        .expect("verify forced child partitions");
    let forced_count = forced_children
        .findings
        .iter()
        .filter(|f| f.kind == FindingKind::ForcedRls)
        .count();
    assert_eq!(
        forced_count, 2,
        "expected exactly 2 child ForcedRls findings after clearing parent FORCE, got: {:?}",
        forced_children.findings
    );
    for child_obj in [
        "auditstream.changes_default".to_owned(),
        format!("auditstream.{monthly_part}"),
    ] {
        assert!(
            forced_children.findings.iter().any(|f| {
                f.kind == FindingKind::ForcedRls
                    && f.severity == Severity::Error
                    && f.object == child_obj
            }),
            "expected ForcedRls on {child_obj}, got: {:?}",
            forced_children.findings
        );
    }

    // Clear child FORCE flags and verify INSERT-only, WITH-CHECK-only, and RESTRICTIVE-only
    // policies still trigger RlsWithoutSelectPolicy on `auditstream.changes`.
    db.execute_batch(&format!(
        "ALTER TABLE auditstream.changes_default NO FORCE ROW LEVEL SECURITY; \
         ALTER TABLE auditstream.{} NO FORCE ROW LEVEL SECURITY;",
        quote_ident(&monthly_part)
    ))
    .await
    .expect("clear child FORCE RLS");

    for (label, policy_sql) in [
        (
            "INSERT-only",
            "CREATE POLICY p_ins ON auditstream.changes FOR INSERT TO PUBLIC WITH CHECK (true)",
        ),
        (
            "FOR ALL WITH CHECK only (NULL USING)",
            "CREATE POLICY p_wc ON auditstream.changes FOR ALL TO PUBLIC WITH CHECK (true)",
        ),
        (
            "RESTRICTIVE-only",
            "CREATE POLICY p_rest ON auditstream.changes AS RESTRICTIVE FOR SELECT TO PUBLIC USING (true)",
        ),
    ] {
        db.execute(policy_sql)
            .await
            .expect("create non-permissive-select policy");
        let rep = verify::check(db.pool(), &specs)
            .await
            .expect("verify RlsWithoutSelectPolicy");
        assert!(
            rep.findings.iter().any(|f| {
                f.kind == FindingKind::RlsWithoutSelectPolicy
                    && f.severity == Severity::Error
                    && f.object == "auditstream.changes"
            }),
            "expected RlsWithoutSelectPolicy on auditstream.changes for {label}, got: {:?}",
            rep.findings
        );
    }
    db.execute_batch(
        "DROP POLICY p_ins ON auditstream.changes; \
         DROP POLICY p_wc ON auditstream.changes; \
         DROP POLICY p_rest ON auditstream.changes;",
    )
    .await
    .expect("drop test policies");

    // 3. Multi-tenant RLS with tenant-scoped app_user + unobstructed PERMISSIVE USING (true)
    //    auditstream_exporter (no BYPASSRLS!), plus 2 stranded rows in `changes_default`.
    db.execute_batch(&format!(
        "CREATE POLICY tenant_read ON auditstream.changes FOR SELECT TO {app_q} \
           USING (actor->>'tenant' = pg_catalog.current_setting('app.tenant', true)); \
         CREATE POLICY exporter_read ON auditstream.changes FOR SELECT TO {exp_q} USING (true); \
         INSERT INTO auditstream.changes (occurred_at, source, op, actor, db_user, schema_version) VALUES \
           ('2020-01-10T00:00:00Z'::timestamptz, 'app', 'seed.a', '{{\"type\":\"user\",\"id\":\"u1\",\"tenant\":\"t1\"}}', 'postgres', 1), \
           ('2020-01-11T00:00:00Z'::timestamptz, 'app', 'seed.b', '{{\"type\":\"user\",\"id\":\"u2\",\"tenant\":\"t2\"}}', 'postgres', 1);"
    ))
    .await
    .expect("create multi-tenant RLS policies and seed changes_default");

    // Confirm live PG behavior: row_security_active is false on changes_default, true on parent changes!
    let (default_rsa, parent_rsa): (bool, bool) = sqlx::query_as(
        "SELECT \
           pg_catalog.row_security_active('auditstream.changes_default'::regclass), \
           pg_catalog.row_security_active('auditstream.changes'::regclass)",
    )
    .fetch_one(&app_pool)
    .await
    .expect("probe row_security_active");
    assert!(
        !default_rsa,
        "child partition row_security_active must be false"
    );
    assert!(
        parent_rsa,
        "parent changes row_security_active must be true for app_user"
    );

    // App user (tenant-filtered) sees RlsHidesDefaultPartition warning, skips default_partition
    // probe, and `partitions::coverage` fails closed with `PartitionError::RlsFiltered` instead
    // of silently returning `default_row_count = 0`.
    let app_rep = verify::check(&app_pool, &specs)
        .await
        .expect("verify as tenant-filtered app_user");
    assert!(
        !app_rep.has_errors(),
        "unexpected errors: {:?}",
        app_rep.findings
    );
    assert!(
        app_rep.default_partition.is_none(),
        "default_partition must be None when RLS hides rows from verifying role"
    );
    assert!(
        app_rep.findings.iter().any(|f| {
            f.kind == FindingKind::RlsHidesDefaultPartition
                && f.severity == Severity::Warning
                && f.object == "auditstream.changes_default"
                && f.detail.contains(&app_role)
        }),
        "expected RlsHidesDefaultPartition warning for app_user, got: {:?}",
        app_rep.findings
    );
    let app_stats = partitions::history_stats(&app_pool, DEFAULT_SCHEMA)
        .await
        .expect("history_stats as app_user");
    assert!(!app_stats.default_has_rows);
    let cov_err = partitions::coverage(&app_pool, DEFAULT_SCHEMA)
        .await
        .expect_err("partitions::coverage must fail closed under tenant-filtered RLS");
    assert!(
        matches!(cov_err, PartitionError::RlsFiltered(_)),
        "expected PartitionError::RlsFiltered, got: {cov_err:?}"
    );

    // Non-superuser NOBYPASSRLS exporter with exclusively unobstructed PERMISSIVE USING (true)
    // sees BOTH rows in `verify`, `history_stats`, and `coverage`!
    let exp_rep = verify::check(&exp_pool, &specs)
        .await
        .expect("verify as auditstream_exporter");
    assert!(
        !exp_rep
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::RlsHidesDefaultPartition),
        "unobstructed exporter must not emit RlsHidesDefaultPartition: {:?}",
        exp_rep.findings
    );
    assert_eq!(
        exp_rep
            .default_partition
            .as_ref()
            .expect("exporter must inspect default_partition")
            .row_count,
        2
    );
    assert!(
        exp_rep
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::DefaultPartitionRows),
        "exporter must catch stranded default rows under RLS: {:?}",
        exp_rep.findings
    );
    let exp_stats = partitions::history_stats(&exp_pool, DEFAULT_SCHEMA)
        .await
        .expect("history_stats as exporter");
    assert!(exp_stats.default_has_rows);
    let exp_cov = partitions::coverage(&exp_pool, DEFAULT_SCHEMA)
        .await
        .expect("coverage as exporter");
    assert_eq!(exp_cov.default_row_count, 2);

    // A hostile sibling `TO PUBLIC` PERMISSIVE policy (`1 / 0 = 1`) alongside `exporter_read`
    // (`USING (true)`) must NOT crash `verify`, `history_stats`, or `coverage` with division by zero!
    db.execute(
        "CREATE POLICY a_public_trap ON auditstream.changes FOR SELECT TO PUBLIC USING (1 / 0 = 1)",
    )
    .await
    .expect("install sibling hostile PUBLIC policy");
    let trap_rep = verify::check(&exp_pool, &specs)
        .await
        .expect("verify must not crash on hostile sibling PUBLIC policy");
    assert!(trap_rep.default_partition.is_none());
    assert!(
        trap_rep.findings.iter().any(|f| {
            f.kind == FindingKind::RlsHidesDefaultPartition
                && f.detail
                    .contains("non-trivial sibling PERMISSIVE SELECT policy")
        }),
        "hostile sibling PUBLIC policy must skip default probe with sibling-policy RlsHidesDefaultPartition detail: {:?}",
        trap_rep.findings
    );
    let trap_stats = partitions::history_stats(&exp_pool, DEFAULT_SCHEMA)
        .await
        .expect("history_stats must not crash on hostile sibling PUBLIC policy");
    assert!(!trap_stats.default_has_rows);
    assert!(matches!(
        partitions::coverage(&exp_pool, DEFAULT_SCHEMA).await,
        Err(PartitionError::RlsFiltered(_))
    ));
    db.execute("DROP POLICY a_public_trap ON auditstream.changes")
        .await
        .expect("drop hostile sibling PUBLIC policy");

    // Adding a PUBLIC RESTRICTIVE policy blocks even the exporter from inspecting changes_default reliably.
    db.execute(
        "CREATE POLICY rest_all ON auditstream.changes AS RESTRICTIVE FOR ALL TO PUBLIC \
         USING (actor->>'tenant' = 't1')",
    )
    .await
    .expect("add PUBLIC RESTRICTIVE FOR ALL policy");
    let exp_blocked = verify::check(&exp_pool, &specs)
        .await
        .expect("verify as exporter with PUBLIC RESTRICTIVE policy");
    assert!(exp_blocked.default_partition.is_none());
    assert!(
        exp_blocked
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::RlsHidesDefaultPartition
                && f.severity == Severity::Warning),
        "RESTRICTIVE FOR ALL policy must trigger RlsHidesDefaultPartition even with PERMISSIVE USING (true): {:?}",
        exp_blocked.findings
    );

    // Clean up restrictive policy & default rows -> clean multi-tenant RLS passes with 0 findings!
    db.execute_batch(
        "DROP POLICY rest_all ON auditstream.changes; \
         DELETE FROM auditstream.changes WHERE occurred_at < '2021-01-01'::timestamptz;",
    )
    .await
    .expect("clean restrictive policy and seeded default rows");
    let clean_exp = verify::check_with_options(
        &exp_pool,
        &verify::VerifyOptions::new(DEFAULT_SCHEMA, specs.clone())
            .with_app_roles(vec![app_role.clone()]),
    )
    .await
    .expect("clean verify as exporter with safe app_role");
    assert!(
        clean_exp.is_clean(),
        "valid multi-tenant RLS with NOSUPERUSER NOBYPASSRLS app_role must verify cleanly, got: {:?}",
        clean_exp.findings
    );

    // 4. RlsBypassedByAppRole checks SUPERUSER, BYPASSRLS, transitive SET-able BYPASSRLS
    //    membership, and auditstream_owner membership inside an uncommitted transaction.
    //    On PG 16+, also confirm `GRANT <bypass_role> TO <app_role> WITH SET FALSE` does NOT
    //    falsely trigger `RlsBypassedByAppRole` (since `BYPASSRLS` is never inherited).
    let pg_version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(db.pool())
        .await
        .expect("read server_version_num");
    let mut tx = db.pool().begin().await.expect("begin bypass tx");
    tx.execute(AssertSqlSafe(format!("GRANT auditstream_owner TO {mem_q}")))
        .await
        .expect("grant auditstream_owner to member_role");
    if pg_version >= 160_000 {
        tx.execute(AssertSqlSafe(format!(
            "GRANT {byp_q} TO {app_q} WITH INHERIT TRUE, SET FALSE"
        )))
        .await
        .expect("grant BYPASSRLS role WITH SET FALSE to app_role");
    }
    let bypass_rep = verify::check_in_transaction(
        &mut tx,
        &verify::VerifyOptions::new(DEFAULT_SCHEMA, specs).with_app_roles(vec![
            app_role.clone(),
            bypass_role.clone(),
            set_bypass_role.clone(),
            super_role.clone(),
            member_role.clone(),
            "nonexistent_ghost_role".to_owned(),
        ]),
    )
    .await
    .expect("verify RlsBypassedByAppRole");
    tx.rollback().await.expect("rollback bypass tx");

    let bypass_findings: Vec<&verify::Finding> = bypass_rep
        .findings
        .iter()
        .filter(|f| f.kind == FindingKind::RlsBypassedByAppRole)
        .collect();
    assert_eq!(
        bypass_findings.len(),
        4,
        "expected exactly 4 RlsBypassedByAppRole findings (excluding safe app_role and nonexistent_ghost_role), got: {bypass_findings:?}"
    );
    assert!(
        !bypass_findings.iter().any(|f| f.object == app_role),
        "safe app_role (even with PG 16+ SET FALSE BYPASSRLS grant) must not be flagged: {bypass_findings:?}"
    );
    for (role, exact_reasons) in [
        (&bypass_role, "(BYPASSRLS)"),
        (&set_bypass_role, "(member of BYPASSRLS role)"),
        (&super_role, "(SUPERUSER)"),
        (&member_role, "(member of auditstream_owner)"),
    ] {
        assert!(
            bypass_findings.iter().any(|f| {
                f.severity == Severity::Warning
                    && f.table.as_deref() == Some("auditstream.changes")
                    && f.object == *role
                    && f.detail.ends_with(exact_reasons)
            }),
            "expected RlsBypassedByAppRole for {role} ending with {exact_reasons:?}, got: {bypass_findings:?}"
        );
    }

    app_pool.close().await;
    exp_pool.close().await;
}
