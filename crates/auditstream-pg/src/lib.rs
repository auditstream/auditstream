//! Postgres introspection, DDL and trigger generation, migration writers and history reader.

pub mod ddl;
pub mod introspect;
pub mod migrations;
pub mod partitions;
pub mod reader;
pub mod triggers;
pub mod verify;

use std::time::Duration;

pub use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use thiserror::Error;
use tokio as _;

/// Crate identifier retained for workspace smoke checks.
pub const CRATE_NAME: &str = "auditstream-pg";

/// Default maximum pool size opened by [`connect`].
const DEFAULT_MAX_CONNECTIONS: u32 = 5;

/// Default connection acquisition timeout used by [`connect`].
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Error returned by [`connect`] when PostgreSQL cannot be reached or rejects the connection.
#[derive(Debug, Error)]
#[error("failed to connect to PostgreSQL: {0}")]
pub struct ConnectError(#[from] sqlx::Error);

/// Connects a [`PgPool`] to `database_url` using CLI-appropriate pool size and timeout defaults.
///
/// Re-exported so callers such as the `auditstream` CLI binary can connect to PostgreSQL and
/// pass `&PgPool` to `auditstream-pg` APIs without depending on `sqlx` directly.
///
/// # Errors
///
/// Returns [`ConnectError`] if `database_url` is invalid or PostgreSQL cannot be reached.
pub async fn connect(database_url: &str) -> Result<PgPool, ConnectError> {
    PgPoolOptions::new()
        .max_connections(DEFAULT_MAX_CONNECTIONS)
        .acquire_timeout(DEFAULT_CONNECT_TIMEOUT)
        .connect(database_url)
        .await
        .map_err(ConnectError)
}

/// Re-verify dependency wiring to `auditstream-core`.
pub fn core_crate_name() -> &'static str {
    auditstream_core::CRATE_NAME
}
