//! Structured logging setup writing exclusively to stderr.

use std::fmt;

use serde_json::{Map, Number, Value};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

use crate::cli::LogFormat;
use crate::exit::format_error_line;

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

fn format_utc_rfc3339(now: OffsetDateTime) -> String {
    now.to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[derive(Default)]
struct JsonFieldVisitor {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl JsonFieldVisitor {
    fn insert(&mut self, field: &Field, value: Value) {
        if field.name() == "message" {
            self.message = Some(match value {
                Value::String(text) => text,
                other => other.to_string(),
            });
        } else {
            self.fields.insert(field.name().to_owned(), value);
        }
    }
}

impl Visit for JsonFieldVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        let json_val =
            Number::from_f64(value).map_or_else(|| Value::String(value.to_string()), Value::Number);
        self.insert(field, json_val);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, Value::Number(Number::from(value)));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, Value::Number(Number::from(value)));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        let json_val = i64::try_from(value).map_or_else(
            |_| Value::String(value.to_string()),
            |n| Value::Number(n.into()),
        );
        self.insert(field, json_val);
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        let json_val = u64::try_from(value).map_or_else(
            |_| Value::String(value.to_string()),
            |n| Value::Number(n.into()),
        );
        self.insert(field, json_val);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, Value::String(value.to_owned()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field, Value::String(format!("{value:?}")));
    }
}

struct JsonEventFormat;

impl<S, N> FormatEvent<S, N> for JsonEventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = JsonFieldVisitor::default();
        event.record(&mut visitor);

        let meta = event.metadata();
        let mut record = Map::with_capacity(5);
        record.insert(
            "timestamp".to_owned(),
            Value::String(format_utc_rfc3339(OffsetDateTime::now_utc())),
        );
        record.insert(
            "level".to_owned(),
            Value::String(meta.level().as_str().to_owned()),
        );
        record.insert("target".to_owned(), Value::String(meta.target().to_owned()));
        record.insert(
            "message".to_owned(),
            Value::String(visitor.message.unwrap_or_default()),
        );
        if !visitor.fields.is_empty() {
            record.insert("fields".to_owned(), Value::Object(visitor.fields));
        }

        let line = serde_json::to_string(&Value::Object(record)).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

fn format_json_record(level: &str, message: &str, exit_code: Option<u8>) -> String {
    let mut record = Map::with_capacity(if exit_code.is_some() { 4 } else { 3 });
    record.insert(
        "timestamp".to_owned(),
        Value::String(format_utc_rfc3339(OffsetDateTime::now_utc())),
    );
    record.insert("level".to_owned(), Value::String(level.to_owned()));
    record.insert("message".to_owned(), Value::String(message.to_owned()));
    if let Some(code) = exit_code {
        record.insert("exit_code".to_owned(), Value::Number(code.into()));
    }
    serde_json::to_string(&Value::Object(record)).unwrap_or_else(|_| match exit_code {
        Some(code) => format!(r#"{{"level":"{level}","exit_code":{code}}}"#),
        None => format!(r#"{{"level":"{level}"}}"#),
    })
}

/// Formats a fatal CLI error as a single-line JSON log record for `--log-format json`.
pub(crate) fn format_json_error(err: &anyhow::Error, exit_code: u8) -> String {
    let raw_line = format_error_line(err);
    let message = raw_line.strip_prefix("error: ").unwrap_or(&raw_line);
    let message = message.strip_prefix("error: ").unwrap_or(message);
    format_json_record("ERROR", message, Some(exit_code))
}

/// Writes an informational diagnostic note to stderr unless `quiet` is set, formatted as plain
/// text or a single-line JSON `INFO` record matching the active [`LogFormat`].
pub(crate) fn emit_note(log_format: LogFormat, quiet: bool, message: &str) {
    if quiet {
        return;
    }
    emit_diagnostic(log_format, message);
}

/// Writes an error-path diagnostic hint to stderr regardless of `--quiet`, formatted as plain text
/// or a single-line JSON `INFO` record matching the active [`LogFormat`].
pub(crate) fn emit_diagnostic(log_format: LogFormat, message: &str) {
    match log_format {
        LogFormat::Text => eprintln!("{message}"),
        LogFormat::Json => eprintln!("{}", format_json_record("INFO", message, None)),
    }
}

/// Initializes the global `tracing` subscriber writing only to stderr.
pub(crate) fn init(verbose: u8, quiet: bool, format: LogFormat) {
    let env_log = std::env::var("AUDITSTREAM_LOG").ok();
    let directive = resolve_filter(verbose, quiet, env_log.as_deref());
    let filter = EnvFilter::try_new(&directive)
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,{SQLX_OPTIONS_SILENCE}")));

    match format {
        LogFormat::Text => {
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter)
                .with_target(false)
                .try_init();
        }
        LogFormat::Json => {
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter)
                .event_format(JsonEventFormat)
                .try_init();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;
    use serde_json::Value;

    use super::{JsonEventFormat, format_json_error, format_json_record, resolve_filter};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    struct BufGuard(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock buf").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = BufGuard;

        fn make_writer(&'a self) -> Self::Writer {
            BufGuard(Arc::clone(&self.0))
        }
    }

    #[test]
    fn json_event_format_emits_structured_single_line_json_and_handles_edge_types() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::DEBUG)
            .event_format(JsonEventFormat)
            .finish();

        let io_err = io::Error::other("disk timeout");
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                snapshot_xmin = 500_u64,
                signed_delta = -7_i64,
                small_i128 = -42_i128,
                huge_i128 = i128::MAX,
                small_u128 = 99_u128,
                huge_u128 = u128::MAX,
                stalled = true,
                lag = 12.5_f64,
                non_finite = f64::NAN,
                note = "line1\nline2",
                debug_list = ?vec![1_u8, 2],
                err = &io_err as &dyn std::error::Error,
                "export stalled"
            );
            tracing::info!(message = 42_u64);
        });

        let text = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 JSON lines, got: {text:?}");
        let parsed: Value = serde_json::from_str(lines[0]).expect("valid JSON log");
        assert!(
            parsed["timestamp"]
                .as_str()
                .is_some_and(|s| s.ends_with('Z'))
        );
        assert_eq!(parsed["level"], "WARN");
        assert_eq!(parsed["message"], "export stalled");
        assert_eq!(parsed["fields"]["snapshot_xmin"], 500);
        assert_eq!(parsed["fields"]["signed_delta"], -7);
        assert_eq!(parsed["fields"]["small_i128"], -42);
        assert_eq!(parsed["fields"]["huge_i128"], i128::MAX.to_string());
        assert_eq!(parsed["fields"]["small_u128"], 99);
        assert_eq!(parsed["fields"]["huge_u128"], u128::MAX.to_string());
        assert_eq!(parsed["fields"]["stalled"], true);
        assert_eq!(parsed["fields"]["lag"], 12.5);
        assert_eq!(parsed["fields"]["non_finite"], "NaN");
        assert_eq!(parsed["fields"]["note"], "line1\nline2");
        assert_eq!(parsed["fields"]["debug_list"], "[1, 2]");
        assert_eq!(parsed["fields"]["err"], "disk timeout");

        let numeric_msg: Value = serde_json::from_str(lines[1]).expect("valid JSON log 2");
        assert_eq!(numeric_msg["message"], "42");

        let info_note: Value =
            serde_json::from_str(&format_json_record("INFO", "no events matched", None))
                .expect("valid JSON info record");
        assert_eq!(info_note["level"], "INFO");
        assert_eq!(info_note["message"], "no events matched");
        assert!(info_note.get("exit_code").is_none());
    }

    #[test]
    fn format_json_error_emits_single_line_json_with_exit_code() {
        let err = anyhow!("root\x1b[31mcause\x1b[0m").context("outer failure");
        let line = format_json_error(&err, 3);
        assert!(!line.contains('\n'));
        let parsed: Value = serde_json::from_str(&line).expect("valid JSON error");
        assert_eq!(parsed["level"], "ERROR");
        assert_eq!(parsed["message"], "outer failure: rootcause");
        assert_eq!(parsed["exit_code"], 3);
        assert!(
            parsed["timestamp"]
                .as_str()
                .is_some_and(|s| s.ends_with('Z'))
        );

        let parse_err = anyhow!("error: unexpected argument '--bogus'\n\nUsage: auditstream");
        let rendered = format_json_error(&parse_err, 2);
        let parsed_clap: Value = serde_json::from_str(&rendered).expect("valid JSON clap error");
        assert_eq!(parsed_clap["exit_code"], 2);
        assert_eq!(
            parsed_clap["message"],
            "unexpected argument '--bogus' Usage: auditstream"
        );
    }

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
