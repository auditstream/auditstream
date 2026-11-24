use std::time::{Duration, Instant};

use auditstream_core::{ActorSource, Cursor, Event, EventSource, Op, Source};
use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions, quote_ident};
use auditstream_pg::reader::{ExportFilter, HistoryQuery, PgEventSource, lookup};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use auditstream_testkit::TestDb;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, PgPool};
use time::OffsetDateTime;

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

/// Sets up an isolated database with D2 history DDL, D3 triggers on `invoice`, an `app_role`
/// for writing to `public.invoice` / `auditstream.log_event`, and a strictly least-privilege
/// `reader_role` that holds ONLY `USAGE` on `auditstream` and `SELECT` on `auditstream.changes`.
async fn setup_reader_db() -> (TestDb, String, String, PgPool, PgPool, String) {
    let db = TestDb::new().await.expect("create TestDb");
    let admin_url = db.admin_url().to_owned();
    let app_role = format!("app_{}", db.database_name());
    let reader_role = format!("rdr_{}", db.database_name());
    let app_pw = "reader_app_pw";
    let reader_pw = "reader_only_pw";

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

    let app_url = app_role_url(&db, &app_role, app_pw);
    let app_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_url)
        .await
        .expect("connect app_pool");

    let reader_url = app_role_url(&db, &reader_role, reader_pw);
    let reader_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&reader_url)
        .await
        .expect("connect reader_pool");

    (db, app_role, reader_role, app_pool, reader_pool, admin_url)
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
    let (db, app_role, reader_role, app_pool, reader_pool, admin_url) = setup_reader_db().await;
    let app_url = app_role_url(&db, &app_role, "reader_app_pw");

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
    let (db, app_role, reader_role, app_pool, reader_pool, admin_url) = setup_reader_db().await;
    let app_url = app_role_url(&db, &app_role, "reader_app_pw");

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
    let (db, app_role, reader_role, app_pool, reader_pool, admin_url) = setup_reader_db().await;

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
