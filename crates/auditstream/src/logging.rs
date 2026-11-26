//! Structured logging setup writing exclusively to stderr.

use tracing_subscriber::EnvFilter;

/// Suppresses `sqlx_postgres::options` below `error` so unrecognized URL query parameters
/// (`sslpassword`, uppercase `PASSWORD`) or malformed `~/.pgpass` lines can never leak secrets
/// to stderr at default or verbose CLI log levels.
const SQLX_OPTIONS_SILENCE: &str = "sqlx_postgres::options=error";

/// Computes the effective tracing filter directive.
///
/// Precedence: `AUDITSTREAM_LOG` environment variable > `-q` > `-v` count > `warn`.
pub(crate) fn resolve_filter(verbose: u8, quiet: bool, env_log: Option<&str>) -> String {
    if let Some(directive) = env_log.map(str::trim).filter(|val| !val.is_empty()) {
        let overrides_sqlx_options = directive.split(',').any(|part| {
            let part = part.trim();
            part == "sqlx_postgres::options" || part.starts_with("sqlx_postgres::options=")
        });
        if overrides_sqlx_options {
            return directive.to_owned();
        }
        return format!("{SQLX_OPTIONS_SILENCE},{directive}");
    }

    if quiet {
        return "error".to_owned();
    }

    match verbose {
        0 => format!("warn,{SQLX_OPTIONS_SILENCE}"),
        1 => format!("info,{SQLX_OPTIONS_SILENCE}"),
        _ => format!("debug,{SQLX_OPTIONS_SILENCE}"),
    }
}

/// Initializes the global `tracing` subscriber writing only to stderr.
pub(crate) fn init(verbose: u8, quiet: bool) {
    let env_log = std::env::var("AUDITSTREAM_LOG").ok();
    let directive = resolve_filter(verbose, quiet, env_log.as_deref());
    let filter = EnvFilter::try_new(&directive)
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,{SQLX_OPTIONS_SILENCE}")));

    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::resolve_filter;

    #[test]
    fn filter_levels_and_env_override() {
        assert_eq!(
            resolve_filter(0, false, None),
            "warn,sqlx_postgres::options=error"
        );
        assert_eq!(
            resolve_filter(1, false, None),
            "info,sqlx_postgres::options=error"
        );
        assert_eq!(
            resolve_filter(2, false, None),
            "debug,sqlx_postgres::options=error"
        );
        assert_eq!(
            resolve_filter(3, false, None),
            "debug,sqlx_postgres::options=error"
        );
        assert_eq!(resolve_filter(0, true, None), "error");
        assert_eq!(resolve_filter(2, true, None), "error");
        assert_eq!(
            resolve_filter(0, false, Some("trace")),
            "sqlx_postgres::options=error,trace"
        );
        assert_eq!(
            resolve_filter(2, true, Some("auditstream=debug")),
            "sqlx_postgres::options=error,auditstream=debug"
        );
        assert_eq!(
            resolve_filter(0, false, Some("debug,sqlx_postgres::options=warn")),
            "debug,sqlx_postgres::options=warn"
        );
        assert_eq!(
            resolve_filter(0, false, Some("debug,sqlx_postgres::options::parse=error")),
            "sqlx_postgres::options=error,debug,sqlx_postgres::options::parse=error"
        );
        assert_eq!(
            resolve_filter(0, false, Some("debug,sqlx_postgres::options::pgpass=error")),
            "sqlx_postgres::options=error,debug,sqlx_postgres::options::pgpass=error"
        );
        assert_eq!(
            resolve_filter(
                0,
                false,
                Some("debug,sqlx_postgres::options[connect]=error")
            ),
            "sqlx_postgres::options=error,debug,sqlx_postgres::options[connect]=error"
        );
        assert_eq!(
            resolve_filter(0, false, Some("debug,sqlx_postgres::options[{key}]=error")),
            "sqlx_postgres::options=error,debug,sqlx_postgres::options[{key}]=error"
        );
        assert!(
            super::EnvFilter::try_new(resolve_filter(
                0,
                false,
                Some("debug,sqlx_postgres::options::parse=error")
            ))
            .is_ok()
        );
    }
}
