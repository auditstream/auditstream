//! Postgres introspection, DDL and trigger generation, migration writers and history reader.

pub mod ddl;
pub mod introspect;
pub mod migrations;
pub mod partitions;
pub mod reader;
pub mod triggers;
pub mod verify;

use std::borrow::Cow;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use sqlx::ConnectOptions as _;
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
#[non_exhaustive]
pub enum TlsPolicy {
    /// Emit a `tracing::warn!` diagnostic when connecting to a non-loopback TCP host without
    /// `sslmode=verify-full`, while preserving backwards compatibility.
    #[default]
    WarnInsecure,
    /// Refuse connections to non-loopback TCP hosts unless `sslmode=verify-full` is configured.
    RequireVerifyFull,
}

/// TLS options for [`connect_with_options`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConnectOptions {
    /// TLS enforcement policy applied before opening a connection to non-loopback TCP hosts.
    pub tls_policy: TlsPolicy,
}

impl ConnectOptions {
    /// Creates [`ConnectOptions`] with the specified [`TlsPolicy`].
    #[must_use]
    pub const fn with_tls_policy(tls_policy: TlsPolicy) -> Self {
        Self { tls_policy }
    }

    /// Creates [`ConnectOptions`] with strict `sslmode=verify-full` enforcement on non-loopback hosts.
    #[must_use]
    pub const fn require_verify_full() -> Self {
        Self::with_tls_policy(TlsPolicy::RequireVerifyFull)
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

/// Returns `true` when `host` refers solely to a local Unix domain socket path or loopback address.
#[must_use]
pub fn is_local_host(host: &str) -> bool {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return true;
    }
    trimmed.split(',').all(|segment| {
        let h = segment.trim();
        if h.is_empty() || h.starts_with('/') {
            return true;
        }
        let fqdn_stripped = h.strip_suffix('.').unwrap_or(h);
        if fqdn_stripped.eq_ignore_ascii_case("localhost") {
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

fn is_valid_uri_scheme(prefix: &str) -> bool {
    let mut bytes = prefix.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn percent_decode_form_key(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                decoded.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    decoded.push((hi << 4) | lo);
                    i += 3;
                } else {
                    decoded.push(bytes[i]);
                    i += 1;
                }
            }
            byte => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Strips unrecognized secret query parameters (`sslpassword` and non-lowercase `PASSWORD`
/// variants) before passing a PostgreSQL URI to `sqlx::PgConnectOptions::from_str`.
///
/// `sqlx-postgres` logs unrecognized query parameters at `tracing::warn!` with their raw values,
/// which would otherwise leak `sslpassword` or uppercase `PASSWORD` onto stderr at default
/// verbosity. Lowercase `password` is preserved so normal authentication continues to work.
fn sanitize_database_url_for_sqlx(database_url: &str) -> Cow<'_, str> {
    let Some(scheme_end) = database_url.find("://") else {
        return Cow::Borrowed(database_url);
    };
    if !is_valid_uri_scheme(&database_url[..scheme_end]) {
        return Cow::Borrowed(database_url);
    }
    let Some(q_rel) = database_url[scheme_end + 3..].find('?') else {
        return Cow::Borrowed(database_url);
    };
    let q_pos = scheme_end + 3 + q_rel;
    let after_q = &database_url[q_pos + 1..];
    let (query, fragment) = match after_q.find('#') {
        Some(hash_pos) => (&after_q[..hash_pos], &after_q[hash_pos..]),
        None => (after_q, ""),
    };

    let mut stripped = false;
    let mut kept = Vec::new();
    for pair in query.split('&') {
        if let Some(eq_pos) = pair.find('=') {
            let decoded_key = percent_decode_form_key(&pair[..eq_pos]);
            let is_unrecognized_secret = decoded_key.eq_ignore_ascii_case("sslpassword")
                || (decoded_key.eq_ignore_ascii_case("password") && decoded_key != "password");
            if is_unrecognized_secret {
                stripped = true;
                continue;
            }
        }
        kept.push(pair);
    }

    if !stripped {
        return Cow::Borrowed(database_url);
    }

    let mut out = String::with_capacity(database_url.len());
    out.push_str(&database_url[..q_pos]);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out.push_str(fragment);
    Cow::Owned(out)
}

/// Resolves the effective non-local TCP host target for TLS checks, returning `None` for Unix
/// domain sockets and loopback TCP addresses.
///
/// Accounts for `pg_opts.get_socket()` (which overrides `host`/`PGHOST`) and for `hostaddr` /
/// `PGHOSTADDR` (which overrides `host` as the actual TCP destination in `sqlx-postgres`).
fn effective_remote_tcp_host(pg_opts: &PgConnectOptions) -> Option<String> {
    if pg_opts.get_socket().is_some() {
        return None;
    }

    // `to_url_lossy` reflects both `?hostaddr=` and `PGHOSTADDR`. Never log this URL!
    let lossy = pg_opts.to_url_lossy();
    let hostaddr = lossy
        .query_pairs()
        .find_map(|(k, v)| (k == "hostaddr" && !v.is_empty()).then(|| v.into_owned()));

    if let Some(addr) = hostaddr {
        return (!is_local_host(&addr)).then_some(addr);
    }

    let host = pg_opts.get_host();
    (!is_local_host(host)).then(|| host.to_owned())
}

/// Validates TLS posture for parsed `pg_opts` and emits a `tracing::warn!` or returns
/// [`ConnectError::InsecureTls`] as configured by `policy`.
fn enforce_tls_policy(pg_opts: &PgConnectOptions, policy: TlsPolicy) -> Result<(), ConnectError> {
    let Some(remote_host) = effective_remote_tcp_host(pg_opts) else {
        return Ok(());
    };
    let ssl_mode = pg_opts.get_ssl_mode();

    match (policy, ssl_mode) {
        (TlsPolicy::RequireVerifyFull, mode) if !matches!(mode, PgSslMode::VerifyFull) => {
            Err(ConnectError::InsecureTls {
                host: remote_host,
                ssl_mode: ssl_mode_name(mode),
            })
        }
        (
            TlsPolicy::WarnInsecure,
            PgSslMode::Disable | PgSslMode::Allow | PgSslMode::Prefer | PgSslMode::Require,
        ) => {
            tracing::warn!(
                host = %remote_host,
                ssl_mode = ssl_mode_name(ssl_mode),
                "connecting to non-loopback PostgreSQL host without verified TLS certificates; set sslmode=verify-full in your connection URL for production"
            );
            Ok(())
        }
        (TlsPolicy::WarnInsecure, PgSslMode::VerifyCa) => {
            tracing::warn!(
                host = %remote_host,
                ssl_mode = "verify-ca",
                "connecting to non-loopback PostgreSQL host without TLS hostname verification; set sslmode=verify-full in your connection URL for production"
            );
            Ok(())
        }
        _ => Ok(()),
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
    let sanitized = sanitize_database_url_for_sqlx(database_url);
    let pg_opts = PgConnectOptions::from_str(&sanitized).map_err(|_| ConnectError::InvalidUrl)?;
    enforce_tls_policy(&pg_opts, options.tls_policy)?;

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
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sqlx::postgres::PgConnectOptions;

    use super::{
        ConnectError, ConnectOptions, TlsPolicy, connect_with_options, effective_remote_tcp_host,
        enforce_tls_policy, is_local_host, sanitize_database_url_for_sqlx,
    };

    struct WarnCounter(Arc<AtomicUsize>);

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[test]
    fn warn_insecure_emits_warning_for_all_unverified_remote_modes_and_stays_silent_for_safe_targets()
     {
        for url in [
            "postgresql://app:pw@db.example.com:5432/prod?sslmode=disable",
            "postgresql://app:pw@db.example.com:5432/prod?sslmode=allow",
            "postgresql://app:pw@db.example.com:5432/prod?sslmode=prefer",
            "postgresql://app:pw@db.example.com:5432/prod?sslmode=require",
            "postgresql://app:pw@db.example.com:5432/prod?sslmode=verify-ca",
            "postgresql://app:pw@localhost:5432/prod?hostaddr=203.0.113.10&sslmode=disable",
        ] {
            let count = Arc::new(AtomicUsize::new(0));
            let opts = PgConnectOptions::from_str(url).expect("parse url");
            tracing::subscriber::with_default(WarnCounter(Arc::clone(&count)), || {
                enforce_tls_policy(&opts, TlsPolicy::WarnInsecure).expect("warn mode allows");
            });
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "expected exactly one WARN for {url}"
            );
        }

        for safe_url in [
            "postgresql://app:pw@db.example.com:5432/prod?sslmode=verify-full",
            "postgresql://app:pw@localhost:5432/prod?sslmode=disable",
            "postgresql://app:pw@localhost.:5432/prod?sslmode=disable",
            "postgresql://app:pw@127.0.0.1:5432/prod?sslmode=disable",
        ] {
            let count = Arc::new(AtomicUsize::new(0));
            let opts = PgConnectOptions::from_str(safe_url).expect("parse safe url");
            tracing::subscriber::with_default(WarnCounter(Arc::clone(&count)), || {
                enforce_tls_policy(&opts, TlsPolicy::WarnInsecure).expect("safe target allows");
            });
            assert_eq!(
                count.load(Ordering::SeqCst),
                0,
                "expected zero warnings for {safe_url}"
            );
        }
    }

    #[test]
    fn is_local_host_distinguishes_loopback_and_unix_sockets_from_remote_hosts() {
        for local in [
            "",
            "/var/run/postgresql",
            "localhost",
            "LOCALHOST",
            "localhost.",
            "127.0.0.1",
            "127.0.0.42",
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:1",
            "localhost,127.0.0.1",
        ] {
            assert!(is_local_host(local), "expected local host for {local:?}");
        }

        for remote in [
            "db.example.com",
            "postgres",
            "@abstract-socket",
            "localhost..",
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

    #[test]
    fn sanitize_strips_unrecognized_secret_query_params_before_sqlx() {
        let raw = "postgresql://app@db.example.com:5432/prod?sslmode=verify-full&sslpassword=KEY_SECRET&PASSWORD=UPPER_SECRET&%53slPassword=PCT_SECRET&password=REAL_PW#frag";
        let sanitized = sanitize_database_url_for_sqlx(raw);
        assert!(
            !sanitized.contains("KEY_SECRET")
                && !sanitized.contains("UPPER_SECRET")
                && !sanitized.contains("PCT_SECRET"),
            "unrecognized secret params leaked into sanitized URL: {sanitized}"
        );
        assert!(
            sanitized.contains("password=REAL_PW"),
            "lowercase password param must be preserved for auth: {sanitized}"
        );
        assert!(sanitized.ends_with("#frag"), "fragment lost: {sanitized}");
    }

    #[test]
    fn tls_policy_handles_hostaddr_bypass_and_unix_socket_with_remote_host() {
        // Remote ?hostaddr= overrides localhost and must be treated as non-local!
        let hostaddr_opts = PgConnectOptions::from_str(
            "postgresql://app:pw@localhost/prod?hostaddr=203.0.113.10&sslmode=disable",
        )
        .expect("parse hostaddr URL");
        assert_eq!(
            effective_remote_tcp_host(&hostaddr_opts).as_deref(),
            Some("203.0.113.10")
        );
        let err = enforce_tls_policy(&hostaddr_opts, TlsPolicy::RequireVerifyFull)
            .expect_err("hostaddr bypass must be rejected");
        assert!(matches!(
            err,
            ConnectError::InsecureTls { ref host, ssl_mode: "disable" } if host == "203.0.113.10"
        ));

        // Explicit Unix socket overrides a remote host and must be treated as local!
        let uds_opts = PgConnectOptions::new()
            .host("db.example.com")
            .socket("/var/run/postgresql");
        assert_eq!(effective_remote_tcp_host(&uds_opts), None);
        enforce_tls_policy(&uds_opts, TlsPolicy::RequireVerifyFull)
            .expect("unix domain socket must never be rejected by TCP TLS policy");

        // Loopback host is allowed under RequireVerifyFull even with sslmode=disable.
        let local_opts =
            PgConnectOptions::from_str("postgresql://app:pw@127.0.0.1:5432/prod?sslmode=disable")
                .expect("parse local URL");
        assert_eq!(effective_remote_tcp_host(&local_opts), None);
        enforce_tls_policy(&local_opts, TlsPolicy::RequireVerifyFull)
            .expect("loopback host must pass RequireVerifyFull");
    }

    #[tokio::test]
    async fn require_verify_full_rejects_non_loopback_without_verify_full_and_never_echoes_password()
     {
        let secret = "super_secret_pw_123";
        for mode in [
            "",
            "?sslmode=disable",
            "?sslmode=allow",
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

        // Hostaddr bypass must also fail via connect_with_options without echoing password.
        let hostaddr_url = format!(
            "postgresql://app:{secret}@localhost:5432/prod?hostaddr=203.0.113.10&sslmode=disable"
        );
        let err = connect_with_options(&hostaddr_url, &ConnectOptions::require_verify_full())
            .await
            .expect_err("hostaddr non-loopback without verify-full must fail");
        assert!(matches!(
            err,
            ConnectError::InsecureTls { ref host, .. } if host == "203.0.113.10"
        ));
        assert!(!err.to_string().contains(secret));

        // Invalid URL and libpq KV errors must also never echo embedded passwords.
        for bad_url in [
            format!("postgresql://app:{secret}@db.example.com:notaport/prod"),
            format!("host=db.example.com password={secret} dbname=prod"),
        ] {
            let err = connect_with_options(&bad_url, &ConnectOptions::default())
                .await
                .expect_err("invalid URL must fail");
            assert!(matches!(err, ConnectError::InvalidUrl));
            assert!(!err.to_string().contains(secret));
        }
    }
}
