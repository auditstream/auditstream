//! Shared PostgreSQL test harness for Auditstream integration suites.
//!
//! Each test gets its own isolated database (`auditstream_test_<uuid>`) that is
//! automatically dropped with `DROP DATABASE ... WITH (FORCE)` when [`TestDb`]
//! is dropped, including during a panic unwind.
//!
//! # Server selection
//!
//! - When `DATABASE_URL` is set, [`TestDb`] connects to that server and runs
//!   administrative DDL against its `postgres` maintenance database.
//! - Otherwise, [`TestDb`] starts or reuses one named `postgres:<major>` Docker
//!   container per major version (`AUDITSTREAM_TEST_PG`, defaulting to
//!   [`DEFAULT_PG_MAJOR`]).

mod server;

use std::path::PathBuf;
use std::time::Duration;

use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor};
use uuid::Uuid;

use crate::server::{database_url_for, resolve_admin_url};

/// Default PostgreSQL major version used when `DATABASE_URL` and
/// `AUDITSTREAM_TEST_PG` are both unset.
pub const DEFAULT_PG_MAJOR: u16 = 19;

/// Default connection pool cap per [`TestDb`] so parallel test processes stay
/// comfortably within PostgreSQL's `max_connections` budget.
const DEFAULT_POOL_MAX_CONNECTIONS: u32 = 5;

/// Fixed advisory lock key ("auditstr" in ASCII) taken in the `postgres`
/// maintenance database by [`TestDb::cluster_lock`].
const CLUSTER_LOCK_KEY: i64 = 0x6175_6469_7473_7472;

/// Errors returned by the PostgreSQL test harness.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `DATABASE_URL` could not be parsed as a valid URL.
    #[error("invalid DATABASE_URL `{url}`: {source}")]
    InvalidDatabaseUrl {
        /// The raw URL string from the environment.
        url: String,
        /// Underlying URL parse error.
        #[source]
        source: url::ParseError,
    },

    /// `AUDITSTREAM_TEST_PG` was set to a value that is not a positive major version.
    #[error("invalid AUDITSTREAM_TEST_PG `{0}`: expected a PostgreSQL major version integer")]
    InvalidPgMajor(String),

    /// Credentials could not be applied to the derived database URL.
    #[error("failed to set PostgreSQL credentials for role `{0}` on database URL")]
    UrlCredentials(String),

    /// Acquiring the cross-process container lockfile failed.
    #[error("failed to lock container coordination file `{path}`: {source}")]
    LockFile {
        /// Lockfile path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A blocking helper task failed to join.
    #[error("blocking helper task failed: {0}")]
    Join(#[source] tokio::task::JoinError),

    /// Starting or inspecting the reusable testcontainers PostgreSQL container failed.
    #[error("failed to start or reuse PostgreSQL container: {0}")]
    Container(String),

    /// The PostgreSQL container did not accept queries before the startup timeout.
    #[error("PostgreSQL container was not ready after {timeout:?}: {last_error}")]
    ContainerReadyTimeout {
        /// Maximum time waited.
        timeout: Duration,
        /// Last connection or query error observed.
        last_error: String,
    },

    /// A PostgreSQL connection or query error from `sqlx`.
    #[error("PostgreSQL error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// An isolated PostgreSQL test database that drops itself on [`Drop`].
///
/// Construct with [`TestDb::new`] for superuser/default access or
/// [`TestDb::non_superuser`] for a managed-Postgres-style
/// `LOGIN CREATEROLE NOSUPERUSER` role.
#[derive(Debug)]
pub struct TestDb {
    url: String,
    admin_url: String,
    database_name: String,
    role_name: Option<String>,
    pool: PgPool,
}

impl TestDb {
    /// Creates a fresh database named `auditstream_test_<uuid>` and connects a
    /// [`PgPool`] to it.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if server resolution fails, database creation
    /// fails, or the connection pool cannot connect.
    pub async fn new() -> Result<Self, Error> {
        let admin_url = resolve_admin_url().await?;
        let db_name = format!("auditstream_test_{}", Uuid::new_v4().simple());

        let mut admin = PgConnection::connect(admin_url.as_str()).await?;
        let create_db_sql = format!("CREATE DATABASE {}", quote_ident(&db_name));
        admin.execute(AssertSqlSafe(create_db_sql)).await?;
        admin.close().await?;

        let url = database_url_for(&admin_url, &db_name, None)?;
        let pool = PgPoolOptions::new()
            .max_connections(DEFAULT_POOL_MAX_CONNECTIONS)
            .connect(&url)
            .await?;

        Ok(Self {
            url,
            admin_url: admin_url.into(),
            database_name: db_name,
            role_name: None,
            pool,
        })
    }

    /// Creates a fresh `LOGIN CREATEROLE NOSUPERUSER` role and a fresh database
    /// owned by that role, returning a [`TestDb`] whose URL and [`PgPool`]
    /// connect as that role.
    ///
    /// Dropping the returned [`TestDb`] drops the database first and then the
    /// created role.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if server resolution, role or database creation, or
    /// connecting as the new role fails.
    pub async fn non_superuser() -> Result<Self, Error> {
        let admin_url = resolve_admin_url().await?;
        let id = Uuid::new_v4().simple().to_string();
        let db_name = format!("auditstream_test_{id}");
        let role_name = format!("auditstream_role_{id}");
        let password = format!("pw_{}", Uuid::new_v4().simple());

        let mut admin = PgConnection::connect(admin_url.as_str()).await?;
        let create_role_sql = format!(
            "CREATE ROLE {} LOGIN PASSWORD {} CREATEROLE NOSUPERUSER NOCREATEDB",
            quote_ident(&role_name),
            quote_literal(&password),
        );
        admin.execute(AssertSqlSafe(create_role_sql)).await?;

        let create_db_sql = format!(
            "CREATE DATABASE {} OWNER {}",
            quote_ident(&db_name),
            quote_ident(&role_name),
        );
        if let Err(err) = admin.execute(AssertSqlSafe(create_db_sql)).await {
            let cleanup_role = format!("DROP ROLE IF EXISTS {}", quote_ident(&role_name));
            let _ = admin.execute(AssertSqlSafe(cleanup_role)).await;
            let _ = admin.close().await;
            return Err(err.into());
        }
        admin.close().await?;

        let url = database_url_for(&admin_url, &db_name, Some((&role_name, &password)))?;
        let pool = match PgPoolOptions::new()
            .max_connections(DEFAULT_POOL_MAX_CONNECTIONS)
            .connect(&url)
            .await
        {
            Ok(pool) => pool,
            Err(err) => {
                drop_database_and_role(admin_url.as_str(), &db_name, Some(&role_name)).await;
                return Err(err.into());
            }
        };

        Ok(Self {
            url,
            admin_url: admin_url.into(),
            database_name: db_name,
            role_name: Some(role_name),
            pool,
        })
    }

    /// Acquires a cluster-wide advisory lock in the `postgres` maintenance
    /// database to serialize tests that create or drop fixed-name roles.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if connecting to the `postgres` maintenance
    /// database or acquiring the advisory lock fails.
    pub async fn cluster_lock() -> Result<ClusterLock, Error> {
        let admin_url = resolve_admin_url().await?;
        let mut conn = PgConnection::connect(admin_url.as_str()).await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(CLUSTER_LOCK_KEY)
            .execute(&mut conn)
            .await?;

        Ok(ClusterLock { conn: Some(conn) })
    }

    /// Connection URL for this isolated test database.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Connection URL for the server's `postgres` maintenance database.
    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    /// Connection pool attached to this isolated test database.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Name of this isolated test database (`auditstream_test_<uuid>`).
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// Name of the dedicated non-superuser role, if created via
    /// [`TestDb::non_superuser`].
    pub fn role_name(&self) -> Option<&str> {
        self.role_name.as_deref()
    }

    /// Executes a single SQL statement against this test database.
    ///
    /// Provided so downstream integration tests (such as `crates/auditstream`) can prepare
    /// fixtures without adding a direct `sqlx` dependency.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if statement execution fails.
    pub async fn execute(&self, sql: &str) -> Result<(), Error> {
        self.pool.execute(AssertSqlSafe(sql)).await?;
        Ok(())
    }

    /// Executes a multi-statement SQL script (for example a Prisma or plain-SQL migration file)
    /// against this test database using the PostgreSQL simple query protocol.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if any statement in the script fails.
    pub async fn execute_batch(&self, sql: &str) -> Result<(), Error> {
        sqlx::raw_sql(AssertSqlSafe(sql))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Executes an ordered sequence of single SQL statements inside one transaction
    /// (preserving transaction-local settings such as `auditstream._migration_tx` and
    /// `auditstream._granted_self` across statements).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if starting a transaction, executing any statement, or committing fails.
    pub async fn execute_statements(&self, stmts: &[String]) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        for stmt in stmts {
            tx.execute(AssertSqlSafe(stmt.as_str())).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Executes a scalar SQL query returning a single `BIGINT` (`i64`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if the query fails or does not return one row.
    pub async fn query_scalar_i64(&self, sql: &str) -> Result<i64, Error> {
        let val: i64 = sqlx::query_scalar(AssertSqlSafe(sql))
            .fetch_one(&self.pool)
            .await?;
        Ok(val)
    }

    /// Executes a scalar SQL query returning a single `TEXT` (`String`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if the query fails or does not return one row.
    pub async fn query_scalar_string(&self, sql: &str) -> Result<String, Error> {
        let val: String = sqlx::query_scalar(AssertSqlSafe(sql))
            .fetch_one(&self.pool)
            .await?;
        Ok(val)
    }

    /// Executes a single-column `TEXT` SQL query returning all rows in query order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if the query fails.
    pub async fn query_strings(&self, sql: &str) -> Result<Vec<String>, Error> {
        let rows: Vec<String> = sqlx::query_scalar(AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Opens a dedicated connection with an explicit transaction and allocates a real `xid8` via
    /// `pg_current_xact_id()` so PostgreSQL's cluster-wide `pg_snapshot_xmin()` stays pinned at or
    /// below that `xid8` until [`HeldTransaction::rollback`] is called or the guard is dropped.
    ///
    /// Uses a standalone [`PgConnection`] (outside `self.pool`) so dropping the guard without
    /// calling [`HeldTransaction::rollback`] closes the TCP session immediately and PostgreSQL
    /// aborts the transaction without leaving any pooled connection idle in transaction.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if connecting, opening the transaction, or allocating an `xid8`
    /// fails.
    pub async fn hold_snapshot_xmin(&self) -> Result<(u64, HeldTransaction), Error> {
        let mut conn = PgConnection::connect(&self.url).await?;
        conn.execute("BEGIN").await?;
        let raw_xid: String = sqlx::query_scalar("SELECT pg_catalog.pg_current_xact_id()::text")
            .fetch_one(&mut conn)
            .await?;
        let xid = raw_xid.parse::<u64>().map_err(|err| {
            Error::Sqlx(sqlx::Error::Decode(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid pg_current_xact_id `{raw_xid}`: {err}"),
            ))))
        })?;
        Ok((xid, HeldTransaction { conn: Some(conn) }))
    }
}

/// Guard holding a dedicated PostgreSQL connection in an open transaction that pins
/// `pg_snapshot_xmin()`.
///
/// The transaction is rolled back either explicitly via [`HeldTransaction::rollback`] or
/// automatically by PostgreSQL when the guard is dropped and its dedicated connection closes.
#[derive(Debug)]
pub struct HeldTransaction {
    conn: Option<PgConnection>,
}

impl HeldTransaction {
    /// Explicitly rolls back the open transaction and closes its dedicated connection.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlx`] if issuing `ROLLBACK` or closing the connection fails.
    pub async fn rollback(mut self) -> Result<(), Error> {
        if let Some(mut conn) = self.conn.take() {
            conn.execute("ROLLBACK").await?;
            conn.close().await?;
        }
        Ok(())
    }
}

impl Drop for HeldTransaction {
    fn drop(&mut self) {
        // Dropping the standalone PgConnection closes the TCP socket immediately,
        // causing PostgreSQL to abort the open transaction even during a panic unwind
        // without leaving any pooled connection idle-in-transaction.
        let _ = self.conn.take();
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let db_name = self.database_name.clone();
        let role_name = self.role_name.clone();

        // Run async cleanup on a dedicated OS thread with its own single-thread
        // Tokio runtime so Drop works reliably inside both `current_thread` and
        // `multi_thread` Tokio tests and never double-panics during unwinding.
        // We intentionally do not await `self.pool.close()` here: on a
        // `current_thread` runtime, blocking in `join()` prevents the outer
        // runtime from polling pool cleanup tasks. `DROP DATABASE ... WITH
        // (FORCE)` terminates all connections to the database directly on the
        // PostgreSQL server, including pooled connections.
        let handle = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    eprintln!("auditstream-testkit: failed to build cleanup runtime: {err}");
                    return;
                }
            };

            runtime.block_on(drop_database_and_role(
                &admin_url,
                &db_name,
                role_name.as_deref(),
            ));
        });

        if let Err(err) = handle.join() {
            eprintln!("auditstream-testkit: cleanup thread panicked: {err:?}");
        }
    }
}

async fn drop_database_and_role(admin_url: &str, db_name: &str, role_name: Option<&str>) {
    let mut admin = match PgConnection::connect(admin_url).await {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("auditstream-testkit: failed to connect for cleanup of `{db_name}`: {err}");
            return;
        }
    };

    let drop_db_sql = format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
        quote_ident(db_name)
    );
    if let Err(err) = admin.execute(AssertSqlSafe(drop_db_sql)).await {
        eprintln!("auditstream-testkit: failed to drop database `{db_name}`: {err}");
    }

    if let Some(role) = role_name {
        let drop_role_sql = format!("DROP ROLE IF EXISTS {}", quote_ident(role));
        if let Err(err) = admin.execute(AssertSqlSafe(drop_role_sql)).await {
            eprintln!("auditstream-testkit: failed to drop role `{role}`: {err}");
        }
    }

    let _ = admin.close().await;
}

/// Guard holding a session-level PostgreSQL advisory lock in the `postgres`
/// maintenance database.
///
/// The lock is released either explicitly via [`ClusterLock::unlock`] or
/// automatically when the guard is dropped (closing its dedicated session).
#[derive(Debug)]
pub struct ClusterLock {
    conn: Option<PgConnection>,
}

impl ClusterLock {
    /// Explicitly unlocks the advisory lock and closes its maintenance
    /// connection.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if unlocking or closing the connection fails.
    pub async fn unlock(mut self) -> Result<(), Error> {
        if let Some(mut conn) = self.conn.take() {
            sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(CLUSTER_LOCK_KEY)
                .execute(&mut conn)
                .await?;
            conn.close().await?;
        }
        Ok(())
    }
}

impl Drop for ClusterLock {
    fn drop(&mut self) {
        // Dropping the standalone PgConnection closes the TCP socket
        // immediately, causing PostgreSQL to end the backend session and
        // release the session-level advisory lock even during a panic unwind.
        let _ = self.conn.take();
    }
}

/// Quotes a PostgreSQL identifier with double quotes, escaping embedded `"`
/// characters by doubling them.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_doubles_embedded_double_quotes() {
        assert_eq!(quote_ident("simple"), "\"simple\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn quote_literal_doubles_embedded_single_quotes() {
        assert_eq!(quote_literal("secret"), "'secret'");
        assert_eq!(quote_literal("a'b"), "'a''b'");
    }
}
