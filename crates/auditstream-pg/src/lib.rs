//! Postgres introspection, DDL and trigger generation, migration writers and history reader.

pub mod ddl;
pub mod introspect;
pub mod migrations;
pub mod partitions;
pub mod reader;
pub mod triggers;
pub mod verify;

use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

pub use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use thiserror::Error;
use tokio as _;

/// Crate identifier retained for workspace smoke checks.
pub const CRATE_NAME: &str = "auditstream-pg";

/// Default maximum pool size opened by [`connect`].
const DEFAULT_MAX_CONNECTIONS: u32 = 5;

/// Default connection acquisition timeout used by [`connect`].
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// TLS enforcement posture applied to non-loopback PostgreSQL hosts by [`connect_with_options`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Emit a `tracing::warn!` diagnostic when connecting to a non-loopback TCP host with
    /// `sslmode` set to `disable`, `allow`, or `prefer`, while preserving backwards compatibility.
    #[default]
    WarnInsecure,
    /// Refuse connections to non-loopback TCP hosts unless `sslmode=verify-full` is configured.
    RequireVerifyFull,
}

/// Connection pool and TLS options for [`connect_with_options`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectOptions {
    /// TLS enforcement policy applied before opening a connection to non-loopback TCP hosts.
    pub tls_policy: TlsPolicy,
}

impl ConnectOptions {
    /// Creates [`ConnectOptions`] with strict `sslmode=verify-full` enforcement on non-loopback hosts.
    #[must_use]
    pub const fn require_verify_full() -> Self {
        Self {
            tls_policy: TlsPolicy::RequireVerifyFull,
        }
    }
}

/// Error returned by [`connect`] and [`connect_with_options`] when PostgreSQL cannot be reached
/// or fails connection validation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// The supplied connection string could not be parsed (raw credentials are never echoed).
    #[error("failed to connect to PostgreSQL: invalid connection URL or parameters")]
    InvalidUrl,

    /// Strict TLS policy rejected a non-loopback connection without `sslmode=verify-full`.
    #[error(
        "failed to connect to PostgreSQL: refusing insecure connection to non-loopback host `{host}` with sslmode={ssl_mode}; configure sslmode=verify-full"
    )]
    InsecureTls {
        /// Target PostgreSQL host name or address.
        host: String,
        /// Effective `sslmode` parsed from the connection string.
        ssl_mode: &'static str,
    },

    /// Opening or authenticating the PostgreSQL pool failed.
    #[error("failed to connect to PostgreSQL: {0}")]
    Database(#[from] sqlx::Error),
}

/// Returns `true` when `host` refers solely to a local Unix socket or loopback address.
#[must_use]
pub fn is_local_host(host: &str) -> bool {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return true;
    }
    trimmed.split(',').all(|segment| {
        let h = segment.trim();
        if h.is_empty() || h.starts_with('/') || h.starts_with('@') {
            return true;
        }
        if h.eq_ignore_ascii_case("localhost") {
            return true;
        }
        let unbracketed = h
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(h);
        unbracketed
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    })
}

const fn ssl_mode_name(mode: PgSslMode) -> &'static str {
    match mode {
        PgSslMode::Disable => "disable",
        PgSslMode::Allow => "allow",
        PgSslMode::Prefer => "prefer",
        PgSslMode::Require => "require",
        PgSslMode::VerifyCa => "verify-ca",
        PgSslMode::VerifyFull => "verify-full",
    }
}

/// Connects a [`PgPool`] to `database_url` using CLI-appropriate pool size, timeout, and default
/// TLS warning posture ([`TlsPolicy::WarnInsecure`]).
///
/// Re-exported so callers such as the `auditstream` CLI binary can connect to PostgreSQL and
/// pass `&PgPool` to `auditstream-pg` APIs without depending on `sqlx` directly.
///
/// # Errors
///
/// Returns [`ConnectError`] if `database_url` is invalid or PostgreSQL cannot be reached.
pub async fn connect(database_url: &str) -> Result<PgPool, ConnectError> {
    connect_with_options(database_url, &ConnectOptions::default()).await
}

/// Connects a [`PgPool`] to `database_url` applying explicit [`ConnectOptions`].
///
/// # Errors
///
/// Returns [`ConnectError::InvalidUrl`] if `database_url` cannot be parsed,
/// [`ConnectError::InsecureTls`] if [`TlsPolicy::RequireVerifyFull`] rejects a non-loopback host,
/// or [`ConnectError::Database`] if PostgreSQL cannot be reached.
pub async fn connect_with_options(
    database_url: &str,
    options: &ConnectOptions,
) -> Result<PgPool, ConnectError> {
    let pg_opts = PgConnectOptions::from_str(database_url).map_err(|_| ConnectError::InvalidUrl)?;
    let host = pg_opts.get_host();
    let ssl_mode = pg_opts.get_ssl_mode();

    if !is_local_host(host) {
        match (options.tls_policy, ssl_mode) {
            (TlsPolicy::RequireVerifyFull, mode) if !matches!(mode, PgSslMode::VerifyFull) => {
                return Err(ConnectError::InsecureTls {
                    host: host.to_owned(),
                    ssl_mode: ssl_mode_name(mode),
                });
            }
            (
                TlsPolicy::WarnInsecure,
                PgSslMode::Disable | PgSslMode::Allow | PgSslMode::Prefer,
            ) => {
                tracing::warn!(
                    host,
                    ssl_mode = ssl_mode_name(ssl_mode),
                    "connecting to non-loopback PostgreSQL host without verified TLS; set sslmode=verify-full in your connection URL for production"
                );
            }
            _ => {}
        }
    }

    PgPoolOptions::new()
        .max_connections(DEFAULT_MAX_CONNECTIONS)
        .acquire_timeout(DEFAULT_CONNECT_TIMEOUT)
        .connect_with(pg_opts)
        .await
        .map_err(ConnectError::Database)
}

/// Re-verify dependency wiring to `auditstream-core`.
pub fn core_crate_name() -> &'static str {
    auditstream_core::CRATE_NAME
}

#[cfg(test)]
mod tests {
    use super::{ConnectError, ConnectOptions, connect_with_options, is_local_host};

    #[test]
    fn is_local_host_distinguishes_loopback_and_unix_sockets_from_remote_hosts() {
        for local in [
            "",
            "/var/run/postgresql",
            "@abstract-socket",
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.0.0.42",
            "::1",
            "[::1]",
            "localhost,127.0.0.1",
        ] {
            assert!(is_local_host(local), "expected local host for {local:?}");
        }

        for remote in [
            "db.example.com",
            "postgres",
            "10.0.0.5",
            "192.168.1.10",
            "2001:db8::1",
            "localhost,db.example.com",
        ] {
            assert!(
                !is_local_host(remote),
                "expected remote host for {remote:?}"
            );
        }
    }

    #[tokio::test]
    async fn require_verify_full_rejects_non_loopback_without_verify_full_and_never_echoes_password()
     {
        let secret = "super_secret_pw_123";
        for mode in [
            "",
            "?sslmode=disable",
            "?sslmode=prefer",
            "?sslmode=require",
            "?sslmode=verify-ca",
        ] {
            let url = format!("postgresql://app:{secret}@db.example.com:5432/prod{mode}");
            let err = connect_with_options(&url, &ConnectOptions::require_verify_full())
                .await
                .expect_err("non-loopback without verify-full must fail");
            assert!(
                matches!(err, ConnectError::InsecureTls { ref host, .. } if host == "db.example.com"),
                "unexpected error for {mode:?}: {err:?}"
            );
            assert!(
                !err.to_string().contains(secret),
                "error leaked secret: {err}"
            );
        }

        // Invalid URL errors must also never echo embedded passwords.
        let bad_url = format!("postgresql://app:{secret}@db.example.com:notaport/prod");
        let err = connect_with_options(&bad_url, &ConnectOptions::default())
            .await
            .expect_err("invalid port must fail");
        assert!(matches!(err, ConnectError::InvalidUrl));
        assert!(!err.to_string().contains(secret));
    }
}
