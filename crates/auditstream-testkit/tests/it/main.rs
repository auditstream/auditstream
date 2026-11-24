//! Integration tests for `auditstream-testkit`.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auditstream_testkit::{TestDb, quote_ident};
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Connection, Executor, Row};
use tokio::task::JoinSet;
use uuid::Uuid;

async fn database_exists(admin_url: &str, db_name: &str) -> Result<bool, sqlx::Error> {
    let mut admin = PgConnection::connect(admin_url).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(db_name)
            .fetch_one(&mut admin)
            .await?;
    admin.close().await?;
    Ok(exists)
}

async fn role_exists(admin_url: &str, role_name: &str) -> Result<bool, sqlx::Error> {
    let mut admin = PgConnection::connect(admin_url).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(role_name)
            .fetch_one(&mut admin)
            .await?;
    admin.close().await?;
    Ok(exists)
}

#[tokio::test]
async fn new_creates_named_database_and_drops_on_drop() -> Result<(), Box<dyn std::error::Error>> {
    let db = TestDb::new().await?;
    let db_name = db.database_name().to_owned();
    let admin_url = db.admin_url().to_owned();

    assert!(
        db_name.starts_with("auditstream_test_"),
        "expected auditstream_test_<uuid>, got {db_name}"
    );
    assert!(db.role_name().is_none());

    let current_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(current_db, db_name);

    // URL connects directly to the same isolated database.
    let mut direct = PgConnection::connect(db.url()).await?;
    let direct_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut direct)
        .await?;
    assert_eq!(direct_db, db_name);
    // Keep `direct` open while dropping `db` to verify DROP DATABASE ... WITH (FORCE).

    drop(db);
    drop(direct);

    assert!(
        !database_exists(&admin_url, &db_name).await?,
        "database `{db_name}` should be dropped when TestDb is dropped"
    );
    Ok(())
}

#[tokio::test]
async fn drops_database_after_panic() -> Result<(), Box<dyn std::error::Error>> {
    let shared: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let shared_for_task = Arc::clone(&shared);

    let handle = tokio::spawn(async move {
        let db = TestDb::new().await.expect("create TestDb inside task");
        {
            let mut guard = shared_for_task.lock().expect("record db metadata");
            *guard = Some((db.admin_url().to_owned(), db.database_name().to_owned()));
        }
        panic!("intentional panic to verify Drop cleans up database");
    });

    let join_err = handle.await.expect_err("task should have panicked");
    assert!(join_err.is_panic(), "expected panic join error");

    let (admin_url, db_name) = shared
        .lock()
        .expect("read recorded db metadata")
        .clone()
        .expect("task recorded metadata before panicking");

    assert!(
        !database_exists(&admin_url, &db_name).await?,
        "database `{db_name}` should be dropped even after panic"
    );
    Ok(())
}

#[tokio::test]
async fn non_superuser_role_and_usage_grant_workflow() -> Result<(), Box<dyn std::error::Error>> {
    let db = TestDb::non_superuser().await?;
    let admin_url = db.admin_url().to_owned();
    let db_name = db.database_name().to_owned();
    let role_name = db
        .role_name()
        .expect("non_superuser creates a dedicated role")
        .to_owned();

    // Verify URL connects as the fresh LOGIN CREATEROLE NOSUPERUSER role.
    let mut conn = PgConnection::connect(db.url()).await?;
    let row = sqlx::query(
        "SELECT current_user::text AS current_role, r.rolsuper, r.rolcreaterole, r.rolcanlogin, \
                pg_get_userbyid(d.datdba)::text AS db_owner \
         FROM pg_roles r \
         JOIN pg_database d ON d.datname = current_database() \
         WHERE r.rolname = current_user",
    )
    .fetch_one(&mut conn)
    .await?;

    let current_role: String = row.try_get("current_role")?;
    let rolsuper: bool = row.try_get("rolsuper")?;
    let rolcreaterole: bool = row.try_get("rolcreaterole")?;
    let rolcanlogin: bool = row.try_get("rolcanlogin")?;
    let db_owner: String = row.try_get("db_owner")?;

    assert_eq!(current_role, role_name);
    assert!(!rolsuper, "role must not be superuser");
    assert!(rolcreaterole, "role must have CREATEROLE");
    assert!(rolcanlogin, "role must have LOGIN");
    assert_eq!(db_owner, role_name, "database must be owned by the role");

    // Managed Postgres (and PostgreSQL 15..19): creating a role does not grant
    // USAGE on it until the creator grants the role to itself.
    let child_role = format!("auditstream_owner_test_{}", Uuid::new_v4().simple());
    conn.execute(AssertSqlSafe(format!(
        "CREATE ROLE {} NOLOGIN",
        quote_ident(&child_role)
    )))
    .await?;

    let usage_before: bool = sqlx::query_scalar("SELECT pg_has_role(current_user, $1, 'USAGE')")
        .bind(&child_role)
        .fetch_one(&mut conn)
        .await?;
    assert!(
        !usage_before,
        "USAGE on newly created role `{child_role}` must be false before self-grant"
    );

    conn.execute(AssertSqlSafe(format!(
        "GRANT {} TO {}",
        quote_ident(&child_role),
        quote_ident(&role_name)
    )))
    .await?;

    let usage_after: bool = sqlx::query_scalar("SELECT pg_has_role(current_user, $1, 'USAGE')")
        .bind(&child_role)
        .fetch_one(&mut conn)
        .await?;
    assert!(
        usage_after,
        "USAGE on newly created role `{child_role}` must be true after self-grant"
    );

    // Clean up the child role before TestDb drops the migrator role.
    conn.execute(AssertSqlSafe(format!(
        "DROP ROLE {}",
        quote_ident(&child_role)
    )))
    .await?;
    conn.close().await?;

    drop(db);

    assert!(!database_exists(&admin_url, &db_name).await?);
    assert!(!role_exists(&admin_url, &role_name).await?);
    Ok(())
}

#[tokio::test]
async fn cluster_lock_serializes_eight_concurrent_fixed_role_creations()
-> Result<(), Box<dyn std::error::Error>> {
    const FIXED_ROLE: &str = "auditstream_testkit_fixed_role";

    let db = TestDb::new().await?;
    let admin_url = db.admin_url().to_owned();

    let mut tasks = JoinSet::new();
    for worker in 0..8_u32 {
        let worker_admin_url = admin_url.clone();
        tasks.spawn(async move {
            let lock = TestDb::cluster_lock().await?;
            let mut conn = PgConnection::connect(&worker_admin_url).await?;
            let create_sql = format!("CREATE ROLE {} NOLOGIN", quote_ident(FIXED_ROLE));
            let drop_sql = format!("DROP ROLE {}", quote_ident(FIXED_ROLE));

            conn.execute(AssertSqlSafe(create_sql)).await?;
            // Hold the lock briefly so any concurrency bug collides reliably.
            tokio::time::sleep(Duration::from_millis(15)).await;
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)")
                    .bind(FIXED_ROLE)
                    .fetch_one(&mut conn)
                    .await?;
            assert!(
                exists,
                "worker {worker} expected role `{FIXED_ROLE}` to exist"
            );
            conn.execute(AssertSqlSafe(drop_sql)).await?;
            conn.close().await?;

            if worker % 2 == 0 {
                lock.unlock().await?;
            } else {
                drop(lock);
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
    }

    while let Some(result) = tasks.join_next().await {
        result?.map_err(|err| format!("worker failed: {err}"))?;
    }

    assert!(!role_exists(&admin_url, FIXED_ROLE).await?);
    Ok(())
}

#[tokio::test]
async fn concurrent_databases_are_isolated() -> Result<(), Box<dyn std::error::Error>> {
    let (db_a, db_b) = tokio::try_join!(TestDb::new(), TestDb::new())?;
    assert_ne!(db_a.database_name(), db_b.database_name());

    let create_table = "CREATE TABLE shared_items (id integer PRIMARY KEY, label text NOT NULL)";
    tokio::try_join!(
        db_a.pool().execute(create_table),
        db_b.pool().execute(create_table),
    )?;

    tokio::try_join!(
        sqlx::query("INSERT INTO shared_items (id, label) VALUES (1, 'from-a')")
            .execute(db_a.pool()),
        sqlx::query("INSERT INTO shared_items (id, label) VALUES (1, 'from-b')")
            .execute(db_b.pool()),
    )?;

    let (label_a, label_b): (String, String) = tokio::try_join!(
        sqlx::query_scalar("SELECT label FROM shared_items WHERE id = 1").fetch_one(db_a.pool()),
        sqlx::query_scalar("SELECT label FROM shared_items WHERE id = 1").fetch_one(db_b.pool()),
    )?;

    assert_eq!(label_a, "from-a");
    assert_eq!(label_b, "from-b");
    Ok(())
}

#[tokio::test]
async fn server_major_matches_auditstream_test_pg() -> Result<(), Box<dyn std::error::Error>> {
    let db = TestDb::new().await?;
    let raw_version_num: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(db.pool())
        .await?;
    let version_num: u32 = raw_version_num.parse()?;
    let server_major = u16::try_from(version_num / 10_000)?;

    assert!(
        server_major >= 15,
        "expected PostgreSQL 15+, got {server_major} ({raw_version_num})"
    );

    let has_database_url = env::var("DATABASE_URL").is_ok_and(|v| !v.trim().is_empty());
    let expected_pg = env::var("AUDITSTREAM_TEST_PG")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());

    if let Some(expected_raw) = expected_pg {
        let expected_major: u16 = expected_raw.parse()?;
        assert_eq!(
            server_major, expected_major,
            "server major ({server_major}) did not match AUDITSTREAM_TEST_PG ({expected_major})"
        );
    } else if !has_database_url {
        assert_eq!(server_major, auditstream_testkit::DEFAULT_PG_MAJOR);
    }

    Ok(())
}
