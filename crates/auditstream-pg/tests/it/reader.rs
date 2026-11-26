use std::time::{Duration, Instant};

use auditstream_core::{ActorSource, Cursor, Event, EventSource, Op, Source, SourceError};
use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, quote_ident};
use auditstream_pg::reader::{
    BoundaryAnchor, ExportFilter, HistoryQuery, PgEventSource, RecordIdFilter, ResumeVerification,
    database_identity, fetch_export_lag, lookup, verify_resume_state,
};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_testkit::TestDb;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, PgPool};
use time::OffsetDateTime;
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

struct ReaderFixture {
    db: TestDb,
    app_role: String,
    reader_role: String,
    app_url: String,
    reader_url: String,
    app_pool: PgPool,
    reader_pool: PgPool,
    admin_url: String,
}

/// Sets up an isolated database with D2 history DDL, D3 triggers on `invoice`, an `app_role`
/// for writing to `public.invoice` / `auditstream.log_event`, and a strictly least-privilege
/// `reader_role` that holds ONLY `USAGE` on `auditstream` and `SELECT` on `auditstream.changes`.
async fn setup_reader_db() -> ReaderFixture {
    let db = TestDb::new().await.expect("create TestDb");
    let admin_url = db.admin_url().to_owned();
    let app_role = format!("app_{}", db.database_name());
    let reader_role = format!("rdr_{}", db.database_name());
    let app_pw = Uuid::new_v4().simple().to_string();
    let reader_pw = Uuid::new_v4().simple().to_string();

    {
        let mut conn = PgConnection::connect(db.url()).await.expect("connect db");
        conn.execute(AssertSqlSafe(format!(
            "CREATE ROLE {} LOGIN PASSWORD '{app_pw}'",
            quote_ident(&app_role)
        )))
        .await
        .expect("create app role");
        conn.execute(AssertSqlSafe(format!(
            "CREATE ROLE {} LOGIN PASSWORD '{reader_pw}'",
            quote_ident(&reader_role)
        )))
        .await
        .expect("create reader role");

        let ddl_stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![app_role.clone()]));
        run_statements(&mut conn, &ddl_stmts)
            .await
            .expect("apply D2 DDL");

        conn.execute(AssertSqlSafe(format!(
            "GRANT USAGE ON SCHEMA {DEFAULT_SCHEMA} TO {}",
            quote_ident(&reader_role)
        )))
        .await
        .expect("grant usage to reader role");
        conn.execute(AssertSqlSafe(format!(
            "GRANT SELECT ON {DEFAULT_SCHEMA}.changes TO {}",
            quote_ident(&reader_role)
        )))
        .await
        .expect("grant select on changes to reader role");

        conn.execute(
            "CREATE TABLE public.invoice (id int PRIMARY KEY, status text, total numeric)",
        )
        .await
        .expect("create invoice table");
        conn.execute(AssertSqlSafe(format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON public.invoice TO {}",
            quote_ident(&app_role)
        )))
        .await
        .expect("grant invoice DML");

        let trigger_stmts = triggers::generate(&TriggerOptions::new(
            DEFAULT_SCHEMA,
            vec![TableTriggerSpec::new(
                "public",
                "invoice",
                vec!["id".to_owned()],
                vec![],
            )],
        ));
        run_statements(&mut conn, &trigger_stmts)
            .await
            .expect("apply D3 triggers");

        conn.close().await.expect("close setup conn");
    }

    let app_url = app_role_url(&db, &app_role, &app_pw);
    let app_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_url)
        .await
        .expect("connect app_pool");

    let reader_url = app_role_url(&db, &reader_role, &reader_pw);
    let reader_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&reader_url)
        .await
        .expect("connect reader_pool");

    ReaderFixture {
        db,
        app_role,
        reader_role,
        app_url,
        reader_url,
        app_pool,
        reader_pool,
        admin_url,
    }
}

async fn cleanup_role(admin_url: &str, role: &str) {
    let mut admin = PgConnection::connect(admin_url)
        .await
        .expect("admin cleanup connect");
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "DROP ROLE IF EXISTS {}",
            quote_ident(role)
        )))
        .await;
    let _ = admin.close().await;
}

/// Drains `PgEventSource` until `expected_events` have been collected or a deadline expires.
/// Because `pg_snapshot_xmin()` is cluster-wide, concurrent tests on other databases in the
/// same PostgreSQL cluster may briefly hold the window back.
async fn drain_until(
    source: &mut PgEventSource,
    batch_size: usize,
    expected_events: usize,
    timeout: Duration,
) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while out.len() < expected_events && Instant::now() < deadline {
        match source.next_batch(batch_size).await.expect("next_batch") {
            Some(batch) => {
                if let Some(cursor) = batch.cursor {
                    source.ack(&cursor).await.expect("ack");
                }
                out.extend(batch.events);
            }
            None => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    out
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn deterministic_out_of_order_commits_lose_a_row_with_id_cursor_but_xid8_window_delivers_both()
 {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let ReaderFixture {
        db,
        app_role,
        reader_role,
        app_url,
        app_pool,
        reader_pool,
        admin_url,
        ..
    } = setup_reader_db().await;

    let mut conn_a = PgConnection::connect(&app_url).await.expect("conn A");
    let mut conn_b = PgConnection::connect(&app_url).await.expect("conn B");
    let mut source = PgEventSource::with_default_schema(reader_pool.clone(), Cursor::new(0, 0));

    // Step 1: Conn A opens a transaction and inserts invoice 1 (allocates id=1, lower xact_id)
    // and stays OPEN.
    let mut tx_a = conn_a.begin().await.expect("begin tx A");
    tx_a.execute(
        "INSERT INTO public.invoice VALUES (1, 'from_a', 100) \
         /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2EifQ'*/",
    )
    .await
    .expect("tx A insert");

    // Step 2: Conn B opens a transaction, inserts invoice 2 (allocates id=2, higher xact_id),
    // and COMMITS immediately while A is still open.
    let mut tx_b = conn_b.begin().await.expect("begin tx B");
    tx_b.execute(
        "INSERT INTO public.invoice VALUES (2, 'from_b', 200) \
         /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2IifQ'*/",
    )
    .await
    .expect("tx B insert");
    tx_b.commit().await.expect("commit tx B");

    // Step 3: Run a read while A is still open.
    // 3a. Naive id-based cursor sees id=2 (invoice 2) and advances last_id = 2.
    let naive_first: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, record_id::text FROM auditstream.changes WHERE id > 0 ORDER BY id ASC",
    )
    .fetch_all(&app_pool)
    .await
    .expect("naive id read 1");
    assert_eq!(naive_first.len(), 1);
    let naive_cursor_id = naive_first[0].0;
    assert!(naive_first[0].1.contains('2'));

    // 3b. Commit-safe xid8 window MUST return None while A is open (B stays unread!).
    let while_a_open = source
        .next_batch(10)
        .await
        .expect("next_batch while A open");
    assert!(
        while_a_open.is_none(),
        "B's row must stay unread while earlier transaction A is still open"
    );

    // Step 4: Now A commits.
    tx_a.commit().await.expect("commit tx A");

    // 4a. Prove the naive id cursor (`id > naive_cursor_id`) PERMANENTLY LOST invoice 1!
    let naive_second: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, record_id::text FROM auditstream.changes WHERE id > $1 ORDER BY id ASC",
    )
    .bind(naive_cursor_id)
    .fetch_all(&app_pool)
    .await
    .expect("naive id read 2");
    assert!(
        naive_second.is_empty(),
        "naive id cursor was expected to miss A's lower-id row"
    );

    // 4b. Prove the xid8 snapshot window delivers BOTH rows (A then B) in (xact_id, id) order!
    let delivered = drain_until(&mut source, 10, 2, Duration::from_secs(5)).await;
    assert_eq!(delivered.len(), 2);
    assert_eq!(
        delivered[0].record_id.as_deref().expect("rec 0").get(),
        r#"{"id":1}"#
    );
    assert_eq!(
        delivered[1].record_id.as_deref().expect("rec 1").get(),
        r#"{"id":2}"#
    );

    // No duplicates on subsequent read.
    assert!(
        source
            .next_batch(10)
            .await
            .expect("empty after drain")
            .is_none()
    );

    conn_a.close().await.expect("close A");
    conn_b.close().await.expect("close B");
    app_pool.close().await;
    reader_pool.close().await;
    drop(db);
    cleanup_role(&admin_url, &app_role).await;
    cleanup_role(&admin_url, &reader_role).await;
}

#[tokio::test]
async fn open_transaction_holds_window_until_closed_then_delivers_all_later_rows_once() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let ReaderFixture {
        db,
        app_role,
        reader_role,
        app_url,
        app_pool,
        reader_pool,
        admin_url,
        ..
    } = setup_reader_db().await;

    let mut holder_conn = PgConnection::connect(&app_url).await.expect("holder conn");
    let mut source = PgEventSource::with_default_schema(reader_pool.clone(), Cursor::new(0, 0));

    // Open a transaction and assign it an xid8 via pg_current_xact_id() so it anchors xmin.
    let mut tx_hold = holder_conn.begin().await.expect("begin tx_hold");
    let _xid: String = sqlx::query_scalar("SELECT pg_catalog.pg_current_xact_id()::text")
        .fetch_one(&mut *tx_hold)
        .await
        .expect("allocate xid for holder tx");

    // Commit three later rows while tx_hold remains open.
    for id in [101_i32, 102, 103] {
        app_pool
            .execute(AssertSqlSafe(format!(
                "INSERT INTO public.invoice VALUES ({id}, 'later', {id}) \
                 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2xhdGVyIn0'*/"
            )))
            .await
            .expect("commit later row");
    }

    // All three later rows must remain unread while tx_hold is open.
    assert!(
        source
            .next_batch(10)
            .await
            .expect("next_batch while held")
            .is_none(),
        "committed rows above an open transaction's xid8 must stay unread"
    );

    // End the holding transaction; all three rows must now arrive exactly once.
    tx_hold.rollback().await.expect("rollback tx_hold");

    let delivered = drain_until(&mut source, 10, 3, Duration::from_secs(5)).await;
    assert_eq!(delivered.len(), 3);
    assert_eq!(
        delivered[0].record_id.as_deref().expect("r0").get(),
        r#"{"id":101}"#
    );
    assert_eq!(
        delivered[1].record_id.as_deref().expect("r1").get(),
        r#"{"id":102}"#
    );
    assert_eq!(
        delivered[2].record_id.as_deref().expect("r2").get(),
        r#"{"id":103}"#
    );

    // Subsequent poll yields None (no duplicates).
    assert!(
        source
            .next_batch(10)
            .await
            .expect("next_batch after drain")
            .is_none()
    );

    holder_conn.close().await.expect("close holder");
    app_pool.close().await;
    reader_pool.close().await;
    drop(db);
    cleanup_role(&admin_url, &app_role).await;
    cleanup_role(&admin_url, &reader_role).await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pagination_ack_monotonicity_precision_filters_and_lookup_work_as_select_only_role() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let ReaderFixture {
        db,
        app_role,
        reader_role,
        app_pool,
        reader_pool,
        admin_url,
        ..
    } = setup_reader_db().await;

    let precise_numeric = "12345678901234567890.123456789";

    // Write 5 events with distinct timestamps, actors, and an exact high-precision numeric.
    let t_before = OffsetDateTime::now_utc() - time::Duration::seconds(1);

    app_pool
        .execute(AssertSqlSafe(format!(
            "INSERT INTO public.invoice VALUES (10, 'open', {precise_numeric}) \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2FsaWNlIn0'*/"
        )))
        .await
        .expect("insert 10");

    app_pool
        .execute(
            "INSERT INTO public.invoice VALUES (11, 'open', 10.00) \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2JvYiJ9'*/",
        )
        .await
        .expect("insert 11");

    app_pool
        .execute(
            "UPDATE public.invoice SET status = 'paid' WHERE id = 10 \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2FsaWNlIn0'*/",
        )
        .await
        .expect("update 10");

    app_pool
        .execute(
            "SELECT auditstream.log_event('invoice.exported', '{\"id\":10}'::jsonb, '{\"ok\":true}'::jsonb) \
             /*auditstream='eyJ0eXBlIjoic2VydmljZSIsImlkIjoiZXhwb3J0ZXIifQ'*/",
        )
        .await
        .expect("app event");

    app_pool
        .execute(
            "DELETE FROM public.invoice WHERE id = 11 \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2JvYiJ9'*/",
        )
        .await
        .expect("delete 11");

    let t_after = OffsetDateTime::now_utc() + time::Duration::seconds(1);

    // 1. Paginated drain (`max = 2`) using strictly SELECT-only `reader_pool`.
    let mut reader = PgEventSource::with_default_schema(reader_pool.clone(), Cursor::new(0, 0));
    let mut batches = 0usize;
    let mut all_events = Vec::new();
    let mut saved_after_first_batch: Option<Cursor> = None;

    let deadline = Instant::now() + Duration::from_secs(5);
    while all_events.len() < 5 && Instant::now() < deadline {
        if let Some(batch) = reader.next_batch(2).await.expect("next_batch(2)") {
            assert!(batch.events.len() <= 2);
            let cursor = batch.cursor.expect("batch has cursor");
            reader.ack(&cursor).await.expect("ack");
            if saved_after_first_batch.is_none() {
                saved_after_first_batch = Some(cursor);
            }
            batches += 1;
            all_events.extend(batch.events);
        } else {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    assert_eq!(all_events.len(), 5);
    assert!(
        batches >= 3,
        "5 events with max=2 must span at least 3 batches"
    );

    // Verify ack never moves backwards!
    let highest_acked = reader.acked_cursor();
    let earlier = saved_after_first_batch.expect("first batch cursor");
    assert!(highest_acked > earlier);
    reader.ack(&earlier).await.expect("ack earlier is no-op");
    assert_eq!(
        reader.acked_cursor(),
        highest_acked,
        "ack must never move cursor backwards"
    );

    // Verify restarting from `earlier` resumes strictly after the first 2 events (yielding remaining 3)!
    let mut resumed = PgEventSource::with_default_schema(reader_pool.clone(), earlier);
    let resumed_events = drain_until(&mut resumed, 10, 3, Duration::from_secs(5)).await;
    assert_eq!(resumed_events.len(), 3);
    assert_eq!(resumed_events[0].event_id, all_events[2].event_id);
    assert_eq!(resumed_events[1].event_id, all_events[3].event_id);
    assert_eq!(resumed_events[2].event_id, all_events[4].event_id);

    // Verify exact field mapping & 29-digit numeric precision preserved verbatim!
    let first = &all_events[0];
    assert_eq!(first.source, Source::Db);
    assert_eq!(first.schema_name.as_deref(), Some("public"));
    assert_eq!(first.table_name.as_deref(), Some("invoice"));
    assert_eq!(first.op, Op::Insert);
    assert_eq!(first.actor_source, Some(ActorSource::Comment));
    assert_eq!(first.db_user, app_role);
    assert_eq!(first.schema_version, 1);
    let after_raw = first.after.as_deref().expect("after").get();
    assert!(
        after_raw.contains(precise_numeric),
        "numeric precision lost in Event.after: {after_raw}"
    );

    // 2. Filtered export window: filtered-out rows still advance the cursor!
    let mut filtered_source = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        Cursor::new(0, 0),
        ExportFilter {
            from: Some(t_before),
            to: Some(t_after),
            actor_id: Some("usr_nobody".to_owned()),
            source: Some(Source::Db),
            ..ExportFilter::default()
        },
    );
    let filtered_batch = filtered_source
        .next_batch(10)
        .await
        .expect("filtered next_batch")
        .expect("filtered window must advance cursor even when 0 rows match");
    assert_eq!(filtered_batch.events, Vec::new());
    let advanced_cursor = filtered_batch.cursor.expect("advanced cursor");
    assert!(
        advanced_cursor.xact_id > 0,
        "cursor must advance past filtered-out rows"
    );
    filtered_source.ack(&advanced_cursor).await.expect("ack");
    assert_eq!(filtered_source.acked_cursor(), advanced_cursor);
    assert!(
        filtered_source
            .next_batch(10)
            .await
            .expect("second filtered call")
            .is_none()
    );

    // Filtered export window matching only Source::App returns only the app event.
    let mut app_only_source = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        Cursor::new(0, 0),
        ExportFilter {
            source: Some(Source::App),
            ..ExportFilter::default()
        },
    );
    let app_only = drain_until(&mut app_only_source, 10, 1, Duration::from_secs(5)).await;
    assert_eq!(app_only.len(), 1);
    assert_eq!(app_only[0].source, Source::App);
    assert_eq!(app_only[0].op.as_str(), "invoice.exported");

    // 3. Lookup API for F4 (newest first, table, record_id, actor_id, time range, limit),
    // also executed strictly as the SELECT-only `reader_pool`.
    let by_record = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            table: Some("invoice".to_owned()),
            record_id: Some(r#"{"id":10}"#.to_owned()),
            actor_id: Some("usr_alice".to_owned()),
            from: Some(t_before),
            to: Some(t_after),
            limit: 10,
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup invoice 10");
    assert_eq!(by_record.len(), 2);
    // Newest first: UPDATE then INSERT!
    assert_eq!(by_record[0].op, Op::Update);
    assert_eq!(by_record[1].op, Op::Insert);
    assert!(
        by_record[1]
            .after
            .as_deref()
            .expect("after")
            .get()
            .contains(precise_numeric)
    );

    // Limit = 1 returns only the newest matching row.
    let limited = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            table: Some("invoice".to_owned()),
            record_id: Some(r#"{"id":10}"#.to_owned()),
            limit: 1,
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup limit 1");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].op, Op::Update);

    // Future time range returns empty list.
    let future_only = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            from: Some(t_after + time::Duration::seconds(60)),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("future lookup");
    assert_eq!(future_only, Vec::new());

    // Confirm reader_role truly cannot write to auditstream.changes or public.invoice.
    assert!(
        reader_pool
            .execute("DELETE FROM auditstream.changes")
            .await
            .is_err(),
        "SELECT-only reader role must not be able to delete history"
    );
    assert!(
        reader_pool
            .execute("SELECT * FROM public.invoice")
            .await
            .is_err(),
        "SELECT-only reader role must have no privileges on application tables"
    );

    app_pool.close().await;
    reader_pool.close().await;
    drop(db);
    cleanup_role(&admin_url, &app_role).await;
    cleanup_role(&admin_url, &reader_role).await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn cli_filters_support_exclusive_until_on_behalf_of_actor_and_record_id_modes() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let ReaderFixture {
        db,
        app_role,
        reader_role,
        app_pool,
        reader_pool,
        admin_url,
        ..
    } = setup_reader_db().await;

    // Add a string-PK table and a composite-PK table to verify RecordIdFilter::Scalar vs Contains.
    db.execute(
        "CREATE TABLE public.doc (code text PRIMARY KEY, body text); \
         CREATE TABLE public.line_item (tenant_id text NOT NULL, seq_no int NOT NULL, PRIMARY KEY (tenant_id, seq_no));",
    )
    .await
    .expect("create doc and line_item");
    db.execute(&format!(
        "GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON public.doc, public.line_item TO {}",
        quote_ident(&app_role)
    ))
    .await
    .expect("grant doc and line_item DML");
    let extra_triggers = triggers::generate(&TriggerOptions::new(
        DEFAULT_SCHEMA,
        vec![
            TableTriggerSpec::new("public", "doc", vec!["code".to_owned()], vec![]),
            TableTriggerSpec::new(
                "public",
                "line_item",
                vec!["tenant_id".to_owned(), "seq_no".to_owned()],
                vec![],
            ),
        ],
    ));
    db.execute_statements(&extra_triggers)
        .await
        .expect("install extra triggers");

    // 1. Direct actor `usr_alice` on invoice id=10 (integer scalar).
    // {"type":"user","id":"usr_alice"}
    app_pool
        .execute(
            "INSERT INTO public.invoice VALUES (10, 'open', 100) \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2FsaWNlIn0'*/",
        )
        .await
        .expect("insert invoice 10");

    // Small delay so the next event has a strictly greater occurred_at for `--until` boundary testing.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 2. Support user acting onBehalfOf `usr_alice` on doc code='10' (string scalar).
    // {"type":"user","id":"usr_support","onBehalfOf":{"type":"user","id":"usr_alice"}}
    app_pool
        .execute(
            "INSERT INTO public.doc VALUES ('10', 'hello') \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX3N1cHBvcnQiLCJvbkJlaGFsZk9mIjp7InR5cGUiOiJ1c2VyIiwiaWQiOiJ1c3JfYWxpY2UifX0'*/",
        )
        .await
        .expect("insert doc '10'");

    // 3. Composite-PK rows by usr_bob.
    // {"type":"user","id":"usr_bob"}
    app_pool
        .execute(
            "INSERT INTO public.line_item VALUES ('t1', 1), ('t1', 2) \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyX2JvYiJ9'*/",
        )
        .await
        .expect("insert line_items");

    // Load all 4 events in chronological order to read exact `occurred_at` of event #2.
    let mut all_source = PgEventSource::with_default_schema(reader_pool.clone(), Cursor::new(0, 0));
    let all = drain_until(&mut all_source, 10, 4, Duration::from_secs(5)).await;
    assert_eq!(all.len(), 4);
    let second_ts = all[1].occurred_at;

    // `until` is strictly exclusive (<) whereas `to` is inclusive (<=).
    let with_until = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            until: Some(second_ts),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup until");
    assert_eq!(with_until.len(), 1);
    assert_eq!(with_until[0].table_name.as_deref(), Some("invoice"));

    let with_to = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            to: Some(second_ts),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup to");
    assert_eq!(with_to.len(), 2);

    // ExportFilter.until is also strictly exclusive.
    let mut export_until = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        Cursor::new(0, 0),
        ExportFilter {
            until: Some(second_ts),
            ..ExportFilter::default()
        },
    );
    let exported_until = drain_until(&mut export_until, 10, 1, Duration::from_secs(5)).await;
    assert_eq!(exported_until.len(), 1);
    assert_eq!(exported_until[0].table_name.as_deref(), Some("invoice"));
    assert!(
        export_until
            .next_batch(10)
            .await
            .expect("no more events after until")
            .is_none()
    );

    // `actor_or_on_behalf_of` matches both direct actor and `onBehalfOf.id` (lookup + export).
    let direct_only = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            actor_id: Some("usr_alice".to_owned()),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup actor_id");
    assert_eq!(direct_only.len(), 1);

    let direct_or_obo = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            actor_or_on_behalf_of: Some("usr_alice".to_owned()),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup actor_or_on_behalf_of");
    assert_eq!(direct_or_obo.len(), 2);

    let mut export_obo = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        Cursor::new(0, 0),
        ExportFilter {
            actor_or_on_behalf_of: Some("usr_alice".to_owned()),
            ..ExportFilter::default()
        },
    );
    let exported_obo = drain_until(&mut export_obo, 10, 2, Duration::from_secs(5)).await;
    assert_eq!(exported_obo.len(), 2);

    // RecordIdFilter::Scalar preserves JSONB type distinction (10 vs "10") and skips composite PKs.
    let scalar_num = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            record_id_filter: Some(RecordIdFilter::Scalar("10".to_owned())),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup scalar 10");
    assert_eq!(scalar_num.len(), 1);
    assert_eq!(scalar_num[0].table_name.as_deref(), Some("invoice"));

    let scalar_str = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            record_id_filter: Some(RecordIdFilter::Scalar(r#""10""#.to_owned())),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup scalar \"10\"");
    assert_eq!(scalar_str.len(), 1);
    assert_eq!(scalar_str[0].table_name.as_deref(), Some("doc"));

    // RecordIdFilter::Contains matches partial composite keys; Exact matches one full key.
    let composite_partial = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            record_id_filter: Some(RecordIdFilter::Contains(r#"{"tenant_id":"t1"}"#.to_owned())),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup contains composite");
    assert_eq!(composite_partial.len(), 2);

    let composite_exact = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            record_id_filter: Some(RecordIdFilter::Exact(
                r#"{"tenant_id":"t1","seq_no":2}"#.to_owned(),
            )),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup exact composite");
    assert_eq!(composite_exact.len(), 1);
    assert_eq!(
        composite_exact[0]
            .record_id
            .as_deref()
            .expect("record_id")
            .get(),
        r#"{"seq_no":2,"tenant_id":"t1"}"#
    );

    app_pool.close().await;
    reader_pool.close().await;
    drop(db);
    cleanup_role(&admin_url, &app_role).await;
    cleanup_role(&admin_url, &reader_role).await;
}

#[tokio::test]
async fn tenant_filters_match_direct_delegated_and_advance_cursor() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let ReaderFixture {
        db,
        app_role,
        reader_role,
        app_pool,
        reader_pool,
        admin_url,
        ..
    } = setup_reader_db().await;

    // 1. Direct tenant `org_acme`
    // {"type":"user","id":"u_1","tenant":"org_acme"}
    app_pool
        .execute(
            "INSERT INTO public.invoice VALUES (1, 'open', 100) \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidV8xIiwidGVuYW50Ijoib3JnX2FjbWUifQ'*/",
        )
        .await
        .expect("insert direct acme");

    // 2. Delegated tenant via onBehalfOf `org_acme`
    // {"type":"ai_agent","id":"bot_1","onBehalfOf":{"type":"user","id":"u_1","tenant":"org_acme"}}
    app_pool
        .execute(
            "INSERT INTO public.invoice VALUES (2, 'open', 200) \
             /*auditstream='eyJ0eXBlIjoiYWlfYWdlbnQiLCJpZCI6ImJvdF8xIiwib25CZWhhbGZPZiI6eyJ0eXBlIjoidXNlciIsImlkIjoidV8xIiwidGVuYW50Ijoib3JnX2FjbWUifX0'*/",
        )
        .await
        .expect("insert delegated acme");

    // 3. Different tenant `org_beta`
    // {"type":"user","id":"u_2","tenant":"org_beta"}
    app_pool
        .execute(
            "INSERT INTO public.invoice VALUES (3, 'open', 300) \
             /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidV8yIiwidGVuYW50Ijoib3JnX2JldGEifQ'*/",
        )
        .await
        .expect("insert beta");

    // Direct-only `tenant` matches only row #1.
    let direct_only = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            tenant: Some("org_acme".to_owned()),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup direct tenant");
    assert_eq!(direct_only.len(), 1);

    // `tenant_or_on_behalf_of` matches rows #1 and #2 in lookup and export.
    let direct_or_obo = lookup(
        &reader_pool,
        DEFAULT_SCHEMA,
        &HistoryQuery {
            tenant_or_on_behalf_of: Some("org_acme".to_owned()),
            ..HistoryQuery::default()
        },
    )
    .await
    .expect("lookup tenant_or_on_behalf_of");
    assert_eq!(direct_or_obo.len(), 2);

    let mut export_acme = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        Cursor::new(0, 0),
        ExportFilter {
            tenant_or_on_behalf_of: Some("org_acme".to_owned()),
            ..ExportFilter::default()
        },
    );
    let exported = drain_until(&mut export_acme, 10, 2, Duration::from_secs(5)).await;
    assert_eq!(exported.len(), 2);

    // Filtered-out tenant advances cursor past rows that don't match.
    let mut export_none = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        Cursor::new(0, 0),
        ExportFilter {
            tenant_or_on_behalf_of: Some("org_unknown".to_owned()),
            ..ExportFilter::default()
        },
    );
    let batch = export_none
        .next_batch(10)
        .await
        .expect("next_batch")
        .expect("filtered tenant window must advance cursor");
    assert_eq!(batch.events, Vec::new());
    assert!(batch.cursor.expect("cursor").xact_id > 0);

    app_pool.close().await;
    reader_pool.close().await;
    drop(db);
    cleanup_role(&admin_url, &app_role).await;
    cleanup_role(&admin_url, &reader_role).await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn cursor_ahead_of_xmin_fails_loudly_and_anchor_timeline_and_lag_work_as_reader_role() {
    let _lock = TestDb::cluster_lock().await.expect("cluster lock");
    let ReaderFixture {
        db,
        app_role,
        reader_role,
        app_url,
        reader_url,
        app_pool,
        reader_pool,
        admin_url,
    } = setup_reader_db().await;

    // 1. Least-privilege reader_role resolves timeline_id via pg_walfile_name(pg_current_wal_lsn()).
    let ident = database_identity(&reader_pool, DEFAULT_SCHEMA)
        .await
        .expect("database_identity as reader_role");
    let current_tl = ident
        .timeline_id
        .expect("timeline_id must be readable by least-privilege reader_role");
    assert!(
        current_tl >= 1,
        "expected timeline_id >= 1, got {current_tl}"
    );

    // 2. Cursor strictly ahead of snapshot_xmin (and xact_id == xmin with id > 0) must fail loudly.
    let mut future_source =
        PgEventSource::with_default_schema(reader_pool.clone(), Cursor::new(u64::MAX - 1, 0));
    let err = future_source
        .next_batch(10)
        .await
        .expect_err("cursor.xact_id > xmin must fail loudly");
    assert!(matches!(err, SourceError::InvalidCursor(_)), "{err:?}");
    assert!(err.to_string().contains("snapshot_xmin"), "{err}");

    // Seed 3 rows and paginate with batch_size=1 on a 1-connection pool to verify pinned window
    // reading + clean connection release after window drain.
    for id in [1_i32, 2, 3] {
        app_pool
            .execute(AssertSqlSafe(format!(
                "INSERT INTO public.invoice VALUES ({id}, 'open', {id})"
            )))
            .await
            .expect("insert invoice");
    }

    let single_conn_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&reader_url)
        .await
        .expect("connect single-connection reader pool");

    let mut source =
        PgEventSource::with_default_schema(single_conn_pool.clone(), Cursor::new(0, 0));
    let delivered = drain_until(&mut source, 1, 3, Duration::from_secs(5)).await;
    assert_eq!(delivered.len(), 3);
    let cursor = source.acked_cursor();
    let anchor = source
        .acked_anchor()
        .expect("acked_anchor must be recorded after ack");
    assert_eq!(anchor.event_id, delivered[2].event_id);
    assert!(anchor.xact_id < cursor.xact_id);

    // After draining the window, the pinned connection must have been returned to the 1-connection pool.
    let lag_on_single = fetch_export_lag(&single_conn_pool, DEFAULT_SCHEMA, cursor)
        .await
        .expect("pinned connection must be released back to 1-connection pool");
    assert!(!lag_on_single.stalled);

    let snap_xmin = source
        .last_snapshot_xmin()
        .expect("last_snapshot_xmin recorded");
    assert_eq!(cursor, Cursor::new(snap_xmin, 0));
    assert!(
        source
            .next_batch(10)
            .await
            .expect("caught-up cursor returns Ok(None)")
            .is_none()
    );
    single_conn_pool.close().await;

    // 3. Deterministic caught-up vs future cursor check under a pinned snapshot_xmin.
    let mut pin_conn = PgConnection::connect(&app_url).await.expect("pin conn");
    let mut tx_pin = pin_conn.begin().await.expect("begin tx_pin");
    let pinned_xmin: u64 =
        sqlx::query_scalar::<_, String>("SELECT pg_catalog.pg_current_xact_id()::text")
            .fetch_one(&mut *tx_pin)
            .await
            .expect("allocate pinned xid")
            .parse()
            .expect("parse pinned xid");

    // Wait for any older concurrent test transactions on other DBs in the cluster to finish so
    // cluster-wide pg_snapshot_xmin() converges to our held `pinned_xmin`.
    let pin_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let cur_xmin: u64 = sqlx::query_scalar::<_, String>(
            "SELECT pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())::text",
        )
        .fetch_one(&reader_pool)
        .await
        .expect("query snapshot_xmin")
        .parse()
        .expect("parse snapshot_xmin");
        if cur_xmin == pinned_xmin {
            break;
        }
        assert!(
            Instant::now() < pin_deadline,
            "pg_snapshot_xmin ({cur_xmin}) did not converge to pinned_xmin ({pinned_xmin})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let mut caught_up_exact =
        PgEventSource::with_default_schema(reader_pool.clone(), Cursor::new(pinned_xmin, 0));
    assert!(
        caught_up_exact
            .next_batch(10)
            .await
            .expect("cursor == (xmin, 0) is caught up")
            .is_none()
    );

    let mut bad_same_xact =
        PgEventSource::with_default_schema(reader_pool.clone(), Cursor::new(pinned_xmin, 1));
    let same_xact_err = bad_same_xact
        .next_batch(10)
        .await
        .expect_err("cursor == (xmin, 1) must fail loudly");
    assert!(matches!(same_xact_err, SourceError::InvalidCursor(_)));

    let mut bad_future_xact = PgEventSource::with_default_schema(
        reader_pool.clone(),
        Cursor::new(pinned_xmin.saturating_add(1), 0),
    );
    let future_xact_err = bad_future_xact
        .next_batch(10)
        .await
        .expect_err("cursor.xact_id > xmin must fail loudly");
    assert!(matches!(future_xact_err, SourceError::InvalidCursor(_)));

    tx_pin.rollback().await.expect("rollback tx_pin");
    pin_conn.close().await.expect("close pin conn");

    // 4. verify_resume_state allows verified zero-loss HA failover across forward timeline_id
    //    increment and rejects backward timeline_id regressions, invalid timeline 0, and
    //    unreadable post-failover timelines.
    verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: Some(anchor),
            saved_timeline_id: Some(current_tl),
            current_timeline_id: Some(current_tl.saturating_add(1)),
        },
    )
    .await
    .expect("matching boundary anchor must allow verified zero-loss timeline increment");

    let regressed_tl = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: Some(anchor),
            saved_timeline_id: Some(current_tl.saturating_add(1)),
            current_timeline_id: Some(current_tl),
        },
    )
    .await
    .expect_err("backward timeline_id regression must fail even with matching anchor");
    assert!(matches!(regressed_tl, SourceError::InvalidCursor(_)));
    assert!(
        regressed_tl.to_string().contains("regressed"),
        "{regressed_tl}"
    );

    let zero_tl = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: Some(anchor),
            saved_timeline_id: Some(0),
            current_timeline_id: Some(current_tl),
        },
    )
    .await
    .expect_err("timeline_id 0 must be rejected");
    assert!(matches!(zero_tl, SourceError::InvalidCursor(_)));

    let post_failover_unknown = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: Some(anchor),
            saved_timeline_id: Some(2),
            current_timeline_id: None,
        },
    )
    .await
    .expect_err("unreadable current timeline after saved timeline > 1 must fail closed");
    assert!(matches!(
        post_failover_unknown,
        SourceError::InvalidCursor(_)
    ));

    // Initial timeline (1) with intact boundary anchor still allows reading from an unprivileged
    // standby whose timeline_id is None, whereas without an anchor it fails closed.
    verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: Some(anchor),
            saved_timeline_id: Some(1),
            current_timeline_id: None,
        },
    )
    .await
    .expect("saved timeline 1 with matching boundary anchor allows None current timeline");

    let unanchored_unknown = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: None,
            saved_timeline_id: Some(1),
            current_timeline_id: None,
        },
    )
    .await
    .expect_err("saved timeline without anchor and None current timeline must fail closed");
    assert!(matches!(unanchored_unknown, SourceError::InvalidCursor(_)));

    // 5. verify_resume_state rejects malformed cursor/anchor bounds, diverged event_id (simulated
    //    PITR), missing anchor, and unanchored forward timeline change on non-zero cursor.
    let neg_cursor = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor: Cursor::new(cursor.xact_id, -1),
            anchor: Some(anchor),
            saved_timeline_id: Some(current_tl),
            current_timeline_id: Some(current_tl),
        },
    )
    .await
    .expect_err("negative cursor.id must fail loudly");
    assert!(matches!(neg_cursor, SourceError::InvalidCursor(_)));

    let anchor_ahead = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor: Cursor::new(anchor.xact_id, 0),
            anchor: Some(anchor),
            saved_timeline_id: Some(current_tl),
            current_timeline_id: Some(current_tl),
        },
    )
    .await
    .expect_err("anchor at or ahead of cursor must fail loudly");
    assert!(matches!(anchor_ahead, SourceError::InvalidCursor(_)));

    let diverged = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: Some(BoundaryAnchor {
                event_id: Uuid::new_v4(),
                ..anchor
            }),
            saved_timeline_id: Some(current_tl),
            current_timeline_id: Some(current_tl.saturating_add(1)),
        },
    )
    .await
    .expect_err("diverged anchor event_id must fail loudly");
    assert!(matches!(diverged, SourceError::InvalidCursor(_)));
    assert!(diverged.to_string().contains("diverged"), "{diverged}");

    let missing = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: Some(BoundaryAnchor {
                id: anchor.id.saturating_sub(1).max(1) + 10_000,
                ..anchor
            }),
            saved_timeline_id: Some(current_tl),
            current_timeline_id: Some(current_tl),
        },
    )
    .await
    .expect_err("missing anchor row must fail loudly");
    assert!(matches!(missing, SourceError::InvalidCursor(_)));
    assert!(missing.to_string().contains("missing"), "{missing}");

    let unanchored_tl = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor,
            anchor: None,
            saved_timeline_id: Some(current_tl),
            current_timeline_id: Some(current_tl.saturating_add(1)),
        },
    )
    .await
    .expect_err("unanchored timeline change on non-zero cursor must fail loudly");
    assert!(matches!(unanchored_tl, SourceError::InvalidCursor(_)));
    assert!(
        unanchored_tl.to_string().contains("no boundary row anchor"),
        "{unanchored_tl}"
    );

    // 6. fetch_export_lag reports clean state when caught up, then stalled=true when an open
    //    transaction pins snapshot_xmin while a newer transaction commits rows behind it.
    let clean_lag = fetch_export_lag(&reader_pool, DEFAULT_SCHEMA, cursor)
        .await
        .expect("fetch clean export lag");
    assert!(!clean_lag.stalled);
    assert!(clean_lag.oldest_unexported_age_secs.is_none());
    assert!(clean_lag.snapshot_xmin >= cursor.xact_id);

    let mut holder_conn = PgConnection::connect(&app_url).await.expect("holder conn");
    let mut tx_hold = holder_conn.begin().await.expect("begin tx_hold");
    let held_xid: u64 =
        sqlx::query_scalar::<_, String>("SELECT pg_catalog.pg_current_xact_id()::text")
            .fetch_one(&mut *tx_hold)
            .await
            .expect("allocate holder xid")
            .parse()
            .expect("parse holder xid");

    let hold_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let cur_xmin: u64 = sqlx::query_scalar::<_, String>(
            "SELECT pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())::text",
        )
        .fetch_one(&reader_pool)
        .await
        .expect("query snapshot_xmin")
        .parse()
        .expect("parse snapshot_xmin");
        if cur_xmin == held_xid {
            break;
        }
        assert!(
            Instant::now() < hold_deadline,
            "pg_snapshot_xmin ({cur_xmin}) did not converge to held_xid ({held_xid})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    app_pool
        .execute("INSERT INTO public.invoice VALUES (99, 'stalled', 99)")
        .await
        .expect("commit row behind holder tx");

    let stalled_lag = fetch_export_lag(&reader_pool, DEFAULT_SCHEMA, cursor)
        .await
        .expect("fetch stalled export lag");
    assert!(
        stalled_lag.stalled,
        "expected stalled=true: {stalled_lag:?}"
    );
    assert_eq!(stalled_lag.snapshot_xmin, held_xid);
    assert!(
        stalled_lag
            .oldest_unexported_age_secs
            .is_some_and(|age| age >= 0.0),
        "expected non-negative oldest_unexported_age_secs: {stalled_lag:?}"
    );

    tx_hold.rollback().await.expect("rollback tx_hold");
    holder_conn.close().await.expect("close holder");

    // 7. Filtered-out drained window still records latest unfiltered BoundaryAnchor, AND a
    //    filtered window that matches earlier rows followed by unfiltered tail rows anchors the
    //    tail unfiltered row in a single deterministic snapshot window.
    let mut filtered_source = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        cursor,
        ExportFilter {
            actor_or_on_behalf_of: Some("nobody_matches".to_owned()),
            ..ExportFilter::default()
        },
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(batch) = filtered_source
            .next_batch(10)
            .await
            .expect("filtered batch")
        {
            assert_eq!(batch.events, Vec::new());
            let next_c = batch.cursor.expect("filtered cursor");
            filtered_source.ack(&next_c).await.expect("ack filtered");
            break;
        }
        assert!(Instant::now() < deadline, "filtered window timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let filtered_anchor = filtered_source
        .acked_anchor()
        .expect("filtered drained window must still record latest unfiltered anchor");
    assert!(filtered_anchor.xact_id > anchor.xact_id);

    let after_empty_filter_cursor = filtered_source.acked_cursor();
    app_pool
        .execute("SELECT auditstream.log_event('app.matched', '{\"n\":1}'::jsonb, NULL)")
        .await
        .expect("log matched app event");
    app_pool
        .execute("INSERT INTO public.invoice VALUES (100, 'tail_unmatched', 100)")
        .await
        .expect("insert trailing db event");
    let (tail_xact_raw, tail_event_id): (String, String) = sqlx::query_as(
        "SELECT xact_id::text, event_id::text FROM auditstream.changes \
         ORDER BY xact_id DESC, id DESC LIMIT 1",
    )
    .fetch_one(&reader_pool)
    .await
    .expect("read tail row");
    let tail_xact_id: u64 = tail_xact_raw.parse().expect("parse tail xact_id");

    // Wait until cluster-wide pg_snapshot_xmin() moves strictly past `tail_xact_id` so both the
    // matched app event and the trailing unmatched db event fall inside one snapshot window.
    let tail_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let cur_xmin: u64 = sqlx::query_scalar::<_, String>(
            "SELECT pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())::text",
        )
        .fetch_one(&reader_pool)
        .await
        .expect("query snapshot_xmin")
        .parse()
        .expect("parse snapshot_xmin");
        if cur_xmin > tail_xact_id {
            break;
        }
        assert!(
            Instant::now() < tail_deadline,
            "pg_snapshot_xmin ({cur_xmin}) did not advance past tail_xact_id ({tail_xact_id})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let mut partial_filtered = PgEventSource::new(
        reader_pool.clone(),
        DEFAULT_SCHEMA,
        after_empty_filter_cursor,
        ExportFilter {
            source: Some(Source::App),
            ..ExportFilter::default()
        },
    );
    let batch = partial_filtered
        .next_batch(10)
        .await
        .expect("partial filtered batch")
        .expect("expected single batch containing matched app event");
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].source, Source::App);
    let batch_cursor = batch.cursor.expect("batch cursor");
    partial_filtered
        .ack(&batch_cursor)
        .await
        .expect("ack partial filtered batch");
    let partial_anchor = partial_filtered
        .acked_anchor()
        .expect("partial filtered window must anchor trailing unfiltered row");
    assert_eq!(partial_anchor.event_id.to_string(), tail_event_id);

    // 8. Mid-window `next_batch(0)` releases its pinned connection cleanly, and `ack` rejects
    //    cursors ahead of `current_position()`.
    let pool_one = PgPoolOptions::new()
        .max_connections(1)
        .connect(&reader_url)
        .await
        .expect("connect 1-conn pool");
    let mut mid_window = PgEventSource::with_default_schema(pool_one.clone(), Cursor::new(0, 0));
    let first_page = mid_window
        .next_batch(1)
        .await
        .expect("first page")
        .expect("at least one row");
    assert_eq!(first_page.events.len(), 1);
    assert!(
        mid_window
            .next_batch(0)
            .await
            .expect("next_batch(0) returns None")
            .is_none()
    );
    // Pool connection must now be released so another query on `pool_one` succeeds immediately.
    let _ = fetch_export_lag(&pool_one, DEFAULT_SCHEMA, Cursor::new(0, 0))
        .await
        .expect("next_batch(0) must release pinned connection");
    let bad_ack = mid_window
        .ack(&Cursor::new(u64::MAX - 1, 0))
        .await
        .expect_err("ack ahead of position must fail");
    assert!(matches!(bad_ack, SourceError::InvalidCursor(_)));
    pool_one.close().await;

    // 9. verify_resume_state rejects unexpected audit rows inside the drained gap
    //    `(anchor, (cursor.xact_id, 0))` (catches PITR/async-failover writes when unaudited
    //    transactions had advanced `cursor.xact_id` far ahead of `anchor.xact_id`).
    let gap_divergence = verify_resume_state(
        &reader_pool,
        DEFAULT_SCHEMA,
        ResumeVerification {
            cursor: partial_filtered.acked_cursor(),
            anchor: Some(anchor),
            saved_timeline_id: Some(current_tl),
            current_timeline_id: Some(current_tl.saturating_add(1)),
        },
    )
    .await
    .expect_err("unexpected rows in (anchor, cursor) drained gap must fail loudly");
    assert!(matches!(gap_divergence, SourceError::InvalidCursor(_)));
    assert!(
        gap_divergence.to_string().contains("unexpected audit"),
        "{gap_divergence}"
    );

    // 10. Per-database privilege fallback in database_identity: revoking `pg_current_wal_lsn()`
    //     and `pg_control_checkpoint()` from PUBLIC inside this isolated TestDb exercises the
    //     role-scoped `pg_control_checkpoint()` fallback and the `timeline_id: None` /
    //     `system_identifier: None` paths cleanly.
    db.pool()
        .execute("REVOKE EXECUTE ON FUNCTION pg_catalog.pg_current_wal_lsn() FROM PUBLIC")
        .await
        .expect("revoke pg_current_wal_lsn from PUBLIC in isolated TestDb");
    db.pool()
        .execute("REVOKE EXECUTE ON FUNCTION pg_catalog.pg_control_checkpoint() FROM PUBLIC")
        .await
        .expect("revoke pg_control_checkpoint from PUBLIC in isolated TestDb");
    db.pool()
        .execute(AssertSqlSafe(format!(
            "GRANT EXECUTE ON FUNCTION pg_catalog.pg_control_checkpoint() TO {}",
            quote_ident(&reader_role)
        )))
        .await
        .expect("grant pg_control_checkpoint to reader_role");
    let checkpoint_ident = database_identity(&reader_pool, DEFAULT_SCHEMA)
        .await
        .expect("database_identity via pg_control_checkpoint fallback");
    assert_eq!(checkpoint_ident.timeline_id, Some(current_tl));

    db.pool()
        .execute(AssertSqlSafe(format!(
            "REVOKE EXECUTE ON FUNCTION pg_catalog.pg_control_checkpoint() FROM {}",
            quote_ident(&reader_role)
        )))
        .await
        .expect("revoke pg_control_checkpoint from reader_role");
    db.pool()
        .execute("REVOKE EXECUTE ON FUNCTION pg_catalog.pg_control_system() FROM PUBLIC")
        .await
        .expect("revoke pg_control_system from PUBLIC in isolated TestDb");
    let restricted_ident = database_identity(&reader_pool, DEFAULT_SCHEMA)
        .await
        .expect("database_identity when WAL/control functions are revoked");
    assert_eq!(restricted_ident.timeline_id, None);
    assert_eq!(restricted_ident.system_identifier, None);

    app_pool.close().await;
    reader_pool.close().await;
    drop(db);
    cleanup_role(&admin_url, &app_role).await;
    cleanup_role(&admin_url, &reader_role).await;
}
