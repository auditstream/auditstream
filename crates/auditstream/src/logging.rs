//! Structured logging setup writing exclusively to stderr.

use tracing_subscriber::EnvFilter;

/// Computes the effective tracing filter directive.
///
/// Precedence: `AUDITSTREAM_LOG` environment variable > `-q` > `-v` count > `warn`.
pub(crate) fn resolve_filter(verbose: u8, quiet: bool, env_log: Option<&str>) -> String {
    if let Some(directive) = env_log.map(str::trim).filter(|val| !val.is_empty()) {
        return directive.to_owned();
    }

    if quiet {
        return "error".to_owned();
    }

    match verbose {
        0 => "warn".to_owned(),
        1 => "info".to_owned(),
        _ => "debug".to_owned(),
    }
}

/// Initializes the global `tracing` subscriber writing only to stderr.
pub(crate) fn init(verbose: u8, quiet: bool) {
    let env_log = std::env::var("AUDITSTREAM_LOG").ok();
    let directive = resolve_filter(verbose, quiet, env_log.as_deref());
    let filter = EnvFilter::try_new(&directive).unwrap_or_else(|_| EnvFilter::new("warn"));

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
        assert_eq!(resolve_filter(0, false, None), "warn");
        assert_eq!(resolve_filter(1, false, None), "info");
        assert_eq!(resolve_filter(2, false, None), "debug");
        assert_eq!(resolve_filter(3, false, None), "debug");
        assert_eq!(resolve_filter(0, true, None), "error");
        assert_eq!(resolve_filter(2, true, None), "error");
        assert_eq!(resolve_filter(0, false, Some("trace")), "trace");
        assert_eq!(
            resolve_filter(2, true, Some("auditstream=debug")),
            "auditstream=debug"
        );
    }
}
