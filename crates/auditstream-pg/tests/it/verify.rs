//! Live PostgreSQL integration tests for `auditstream_pg::verify` (D7).

use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_pg::verify::{self, FindingKind, Severity};
use auditstream_testkit::TestDb;
use sqlx::{AssertSqlSafe, Executor};

async fn install_fixture(db: &TestDb, tables: &[TableTriggerSpec]) {
    let _guard = TestDb::cluster_lock()
        .await
        .expect("acquire cluster lock for role setup");

    let mut ddl_stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    let cleanup = ddl_stmts.pop();
    let trigger_stmts = triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, tables.to_vec()));

    let mut tx = db.pool().begin().await.expect("begin fixture tx");
    for stmt in ddl_stmts.into_iter().chain(trigger_stmts).chain(cleanup) {
        tx.execute(AssertSqlSafe(stmt))
            .await
            .expect("install capture DDL/triggers");
    }
    tx.commit().await.expect("commit fixture tx");
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
    install_fixture(&db, &bad_specs).await;

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
    install_fixture(&db, &unsorted_specs).await;

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
