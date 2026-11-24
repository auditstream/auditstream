//! `auditstream history` command implementation.
//!
//! Queries recorded audit changes from `{schema}.changes` via [`auditstream_pg::reader::lookup`]
//! (newest first, ordered by `occurred_at DESC, id DESC`) and renders them as an aligned table
//! (omitting `before`/`after`) or as newline-delimited JSON matching `spec/schemas/event.schema.json`.

use std::fmt::Write as _;
use std::io;

use anyhow::anyhow;
use auditstream_core::{Actor, ActorSource, ActorType, Event};
use auditstream_pg::reader::{self, HistoryQuery, MAX_LOOKUP_LIMIT, RecordIdFilter};
use clap::{Args as ClapArgs, ValueEnum};
use serde_json::Value;
use serde_json::value::RawValue;
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::commands::common::{connect_database, parse_table_ref, require_database_url, run_async};
use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt, write_stdout};

/// Default maximum number of history rows returned by `auditstream history`.
const DEFAULT_LIMIT: i64 = 100;

/// Table header labels for `--format table`.
const TABLE_HEADERS: [&str; 7] = [
    "OCCURRED_AT",
    "OP",
    "TABLE",
    "RECORD_ID",
    "ACTOR",
    "ACTOR_SOURCE",
    "CHANGED",
];

/// Output format for `auditstream history`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable column table (omits `before` and `after`).
    #[default]
    Table,
    /// One compact JSON event per line matching `spec/schemas/event.schema.json`.
    Json,
}

/// Arguments for `auditstream history`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Filter by audited table (`<schema>.<table>`).
    #[arg(long, value_name = "SCHEMA.TABLE")]
    pub(crate) table: Option<String>,

    /// Filter by primary key value (requires `--table`; parsed as JSON first, else string).
    #[arg(long, value_name = "VALUE")]
    pub(crate) record_id: Option<String>,

    /// Filter by actor identifier (matches `actor.id` or `actor.onBehalfOf.id`).
    #[arg(long, value_name = "ID")]
    pub(crate) actor: Option<String>,

    /// Filter by tenant identifier (matches `actor.tenant` or `actor.onBehalfOf.tenant`).
    #[arg(long, value_name = "ID")]
    pub(crate) tenant: Option<String>,

    /// Inclusive lower bound on `occurred_at` (RFC 3339 timestamp or `YYYY-MM-DD` UTC).
    #[arg(long, value_name = "TIME")]
    pub(crate) since: Option<String>,

    /// Exclusive upper bound on `occurred_at` (RFC 3339 timestamp or `YYYY-MM-DD` UTC).
    #[arg(long, value_name = "TIME")]
    pub(crate) until: Option<String>,

    /// Maximum number of events to return (`1..=1000`).
    #[arg(
        long,
        default_value_t = DEFAULT_LIMIT,
        value_name = "N",
        value_parser = clap::value_parser!(i64).range(1..=MAX_LOOKUP_LIMIT)
    )]
    pub(crate) limit: i64,

    /// Output format (`table` or `json`).
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub(crate) format: OutputFormat,
}

/// Runs the `history` command.
pub(crate) fn run(args: &Args, config: &Config) -> anyhow::Result<()> {
    let query = build_query(args)?;
    let db_url = require_database_url(config)?;

    let events = run_async(async {
        let pool = connect_database(db_url).await?;

        let result = reader::lookup(&pool, &config.schema, &query)
            .await
            .map_err(|err| anyhow!("failed to query audit history: {err}"))
            .exit_code(Exit::Database);

        pool.close().await;
        result
    })?;

    if events.is_empty() {
        eprintln!("no events matched");
        return Ok(());
    }

    let mut out = io::stdout().lock();
    match args.format {
        OutputFormat::Table => {
            for line in render_table_lines(&events) {
                if !write_stdout(&mut out, line.as_bytes())? {
                    break;
                }
            }
        }
        OutputFormat::Json => {
            for event in &events {
                let mut line = serde_json::to_string(event)
                    .map_err(|err| anyhow!("failed to serialize event JSON: {err}"))
                    .exit_code(Exit::Internal)?;
                line.push('\n');
                if !write_stdout(&mut out, line.as_bytes())? {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn build_query(args: &Args) -> anyhow::Result<HistoryQuery> {
    if args.record_id.is_some() && args.table.is_none() {
        return Err(anyhow!("--record-id requires --table <schema>.<table>"))
            .exit_code(Exit::Usage);
    }

    let (schema_name, table) = match args.table.as_deref() {
        Some(raw) => {
            let (schema, tbl) = parse_table_ref(raw, "--table")?;
            (Some(schema), Some(tbl))
        }
        None => (None, None),
    };

    let record_id_filter = match args.record_id.as_deref() {
        Some(raw) => Some(parse_record_id_filter(raw)?),
        None => None,
    };

    let actor_or_on_behalf_of = match args.actor.as_deref() {
        Some("") => {
            return Err(anyhow!("invalid --actor: identifier must not be empty"))
                .exit_code(Exit::Usage);
        }
        Some(id) => Some(id.to_owned()),
        None => None,
    };

    let tenant_or_on_behalf_of = match args.tenant.as_deref() {
        Some("") => {
            return Err(anyhow!("invalid --tenant: identifier must not be empty"))
                .exit_code(Exit::Usage);
        }
        Some(id) => Some(id.to_owned()),
        None => None,
    };

    let from = match args.since.as_deref() {
        Some(raw) => Some(parse_time_flag("--since", raw)?),
        None => None,
    };

    let until = match args.until.as_deref() {
        Some(raw) => Some(parse_time_flag("--until", raw)?),
        None => None,
    };

    if let (Some(since_dt), Some(until_dt)) = (from, until)
        && until_dt <= since_dt
    {
        return Err(anyhow!("--until must be strictly after --since")).exit_code(Exit::Usage);
    }

    Ok(HistoryQuery {
        table,
        schema_name,
        record_id: None,
        record_id_filter,
        actor_id: None,
        actor_or_on_behalf_of,
        tenant: None,
        tenant_or_on_behalf_of,
        from,
        to: None,
        until,
        source: None,
        limit: args.limit,
    })
}

fn parse_record_id_filter(raw: &str) -> anyhow::Result<RecordIdFilter> {
    if raw.is_empty() {
        return Err(anyhow!("invalid --record-id: value must not be empty")).exit_code(Exit::Usage);
    }

    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => {
            if map.is_empty() {
                return Err(anyhow!(
                    "invalid --record-id: JSON object filter must contain at least one key"
                ))
                .exit_code(Exit::Usage);
            }
            let canonical = serde_json::to_string(&Value::Object(map))
                .map_err(|err| anyhow!("failed to serialize --record-id object: {err}"))
                .exit_code(Exit::Internal)?;
            Ok(RecordIdFilter::Contains(canonical))
        }
        Ok(val @ (Value::Number(_) | Value::String(_) | Value::Bool(_))) => {
            let canonical = serde_json::to_string(&val)
                .map_err(|err| anyhow!("failed to serialize --record-id scalar: {err}"))
                .exit_code(Exit::Internal)?;
            Ok(RecordIdFilter::Scalar(canonical))
        }
        Ok(Value::Array(_) | Value::Null) => Err(anyhow!(
            "invalid --record-id `{raw}`: expected a JSON scalar (string, number, boolean) or JSON object"
        ))
        .exit_code(Exit::Usage),
        Err(_) => {
            let quoted = serde_json::to_string(raw)
                .map_err(|err| anyhow!("failed to encode --record-id string: {err}"))
                .exit_code(Exit::Internal)?;
            Ok(RecordIdFilter::Scalar(quoted))
        }
    }
}

fn parse_time_flag(flag: &str, raw: &str) -> anyhow::Result<OffsetDateTime> {
    if let Ok(dt) = OffsetDateTime::parse(raw, &Rfc3339) {
        return Ok(dt.to_offset(UtcOffset::UTC));
    }

    if let Some(dt) = parse_utc_date_midnight(raw) {
        return Ok(dt);
    }

    Err(anyhow!(
        "invalid {flag} timestamp `{raw}`: expected RFC 3339 (e.g. 2026-06-15T00:00:00Z) or YYYY-MM-DD"
    ))
    .exit_code(Exit::Usage)
}

fn parse_utc_date_midnight(raw: &str) -> Option<OffsetDateTime> {
    let bytes = raw.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !bytes[0..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return None;
    }

    let year: i32 = raw[0..4].parse().ok()?;
    let month_num: u8 = raw[5..7].parse().ok()?;
    let day: u8 = raw[8..10].parse().ok()?;
    let month = Month::try_from(month_num).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    Some(PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc())
}

fn render_table_lines(events: &[Event]) -> Vec<String> {
    let rows: Vec<[String; 7]> = events.iter().map(event_to_table_row).collect();
    let mut widths = TABLE_HEADERS.map(|header| header.chars().count());
    for row in &rows {
        for (col_idx, cell) in row.iter().enumerate() {
            widths[col_idx] = widths[col_idx].max(cell.chars().count());
        }
    }

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format_table_line(&TABLE_HEADERS, &widths));
    for row in &rows {
        let refs = [
            row[0].as_str(),
            row[1].as_str(),
            row[2].as_str(),
            row[3].as_str(),
            row[4].as_str(),
            row[5].as_str(),
            row[6].as_str(),
        ];
        lines.push(format_table_line(&refs, &widths));
    }
    lines
}

fn format_table_line(cells: &[&str; 7], widths: &[usize; 7]) -> String {
    let mut line = String::new();
    for (idx, cell) in cells.iter().enumerate() {
        if idx > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if idx + 1 < cells.len() {
            let pad = widths[idx].saturating_sub(cell.chars().count());
            for _ in 0..pad {
                line.push(' ');
            }
        }
    }
    line.push('\n');
    line
}

fn event_to_table_row(event: &Event) -> [String; 7] {
    let occurred_at = format_utc_rfc3339(event.occurred_at);
    let op = sanitize_cell(event.op.as_str());
    let table = match (&event.schema_name, &event.table_name) {
        (Some(schema), Some(table)) => sanitize_cell(&format!("{schema}.{table}")),
        (None, Some(table)) => sanitize_cell(table),
        _ => "-".to_owned(),
    };
    let record_id = event
        .record_id
        .as_deref()
        .map_or_else(|| "-".to_owned(), |raw| sanitize_cell(raw.get()));
    let actor = format_actor(event.actor.as_deref());
    let actor_source = actor_source_str(event.actor_source).to_owned();
    let changed = match event.changed.as_deref() {
        Some(cols) if !cols.is_empty() => sanitize_cell(&cols.join(",")),
        _ => "-".to_owned(),
    };

    [
        occurred_at,
        op,
        table,
        record_id,
        actor,
        actor_source,
        changed,
    ]
}

fn format_utc_rfc3339(dt: OffsetDateTime) -> String {
    let utc = dt.to_offset(UtcOffset::UTC);
    let mut out = String::with_capacity(27);
    let _ = write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.microsecond(),
    );
    out
}

fn format_actor(raw: Option<&RawValue>) -> String {
    let Some(raw) = raw else {
        return "-".to_owned();
    };

    if let Ok(actor) = serde_json::from_str::<Actor>(raw.get()) {
        let base = format!("{}:{}", actor_type_str(actor.actor_type), actor.id);
        let full = match actor.on_behalf_of {
            Some(nested) => format!(
                "{base} for {}:{}",
                actor_type_str(nested.actor_type),
                nested.id
            ),
            None => base,
        };
        return sanitize_cell(&full);
    }

    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw.get())
        && let (Some(actor_type), Some(actor_id)) = (
            map.get("type").and_then(Value::as_str),
            map.get("id").and_then(Value::as_str),
        )
    {
        let base = format!("{actor_type}:{actor_id}");
        if let Some(Value::Object(nested)) = map.get("onBehalfOf")
            && let (Some(nt), Some(ni)) = (
                nested.get("type").and_then(Value::as_str),
                nested.get("id").and_then(Value::as_str),
            )
        {
            return sanitize_cell(&format!("{base} for {nt}:{ni}"));
        }
        return sanitize_cell(&base);
    }

    sanitize_cell(raw.get())
}

fn actor_type_str(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::User => "user",
        ActorType::ApiKey => "api_key",
        ActorType::AiAgent => "ai_agent",
        ActorType::Service => "service",
        ActorType::System => "system",
    }
}

fn actor_source_str(source: Option<ActorSource>) -> &'static str {
    match source {
        None => "-",
        Some(ActorSource::SetConfig) => "set_config",
        Some(ActorSource::Comment) => "comment",
        Some(ActorSource::Jwt) => "jwt",
        Some(ActorSource::Invalid) => "invalid",
    }
}

fn sanitize_cell(raw: &str) -> String {
    if !raw.contains(['\n', '\r', '\t']) {
        return raw.to_owned();
    }
    raw.chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use auditstream_pg::reader::RecordIdFilter;
    use serde_json::value::RawValue;

    use super::{
        Args, DEFAULT_LIMIT, OutputFormat, build_query, format_actor, format_utc_rfc3339,
        parse_record_id_filter, parse_time_flag,
    };
    use crate::exit::{Exit, exit_code_for};

    fn base_args() -> Args {
        Args {
            table: None,
            record_id: None,
            actor: None,
            tenant: None,
            since: None,
            until: None,
            limit: DEFAULT_LIMIT,
            format: OutputFormat::Table,
        }
    }

    #[test]
    fn record_id_parses_json_first_then_falls_back_to_string() {
        assert_eq!(
            parse_record_id_filter("1").unwrap(),
            RecordIdFilter::Scalar("1".to_owned())
        );
        assert_eq!(
            parse_record_id_filter("\"1\"").unwrap(),
            RecordIdFilter::Scalar("\"1\"".to_owned())
        );
        assert_eq!(
            parse_record_id_filter("inv_42").unwrap(),
            RecordIdFilter::Scalar("\"inv_42\"".to_owned())
        );
        assert_eq!(
            parse_record_id_filter("true").unwrap(),
            RecordIdFilter::Scalar("true".to_owned())
        );
        assert_eq!(
            parse_record_id_filter(r#"{"tenant_id":"t1"}"#).unwrap(),
            RecordIdFilter::Contains(r#"{"tenant_id":"t1"}"#.to_owned())
        );

        for invalid in ["", "null", "[1,2]", "{}"] {
            let err = parse_record_id_filter(invalid).unwrap_err();
            assert_eq!(exit_code_for(&err), Exit::Usage, "input={invalid:?}");
        }
    }

    #[test]
    fn time_flag_accepts_rfc3339_and_utc_date_only() {
        let utc = parse_time_flag("--since", "2026-06-15T12:30:45.123456Z").unwrap();
        assert_eq!(format_utc_rfc3339(utc), "2026-06-15T12:30:45.123456Z");

        let offset = parse_time_flag("--since", "2026-06-15T09:00:00-03:00").unwrap();
        assert_eq!(format_utc_rfc3339(offset), "2026-06-15T12:00:00.000000Z");

        let date_only = parse_time_flag("--since", "2026-06-15").unwrap();
        assert_eq!(format_utc_rfc3339(date_only), "2026-06-15T00:00:00.000000Z");

        for invalid in ["2026-02-30", "2026-6-5", "yesterday", ""] {
            let err = parse_time_flag("--since", invalid).unwrap_err();
            assert_eq!(exit_code_for(&err), Exit::Usage, "input={invalid:?}");
        }
    }

    #[test]
    fn build_query_enforces_table_and_time_range_invariants() {
        let mut no_table = base_args();
        no_table.record_id = Some("1".to_owned());
        assert_eq!(
            exit_code_for(&build_query(&no_table).unwrap_err()),
            Exit::Usage
        );

        let mut bad_table = base_args();
        bad_table.table = Some("invoice".to_owned());
        assert_eq!(
            exit_code_for(&build_query(&bad_table).unwrap_err()),
            Exit::Usage
        );

        let mut equal_bounds = base_args();
        equal_bounds.since = Some("2026-06-15".to_owned());
        equal_bounds.until = Some("2026-06-15T00:00:00Z".to_owned());
        assert_eq!(
            exit_code_for(&build_query(&equal_bounds).unwrap_err()),
            Exit::Usage
        );

        let mut reversed_bounds = base_args();
        reversed_bounds.since = Some("2026-06-16".to_owned());
        reversed_bounds.until = Some("2026-06-15".to_owned());
        assert_eq!(
            exit_code_for(&build_query(&reversed_bounds).unwrap_err()),
            Exit::Usage
        );

        let mut empty_tenant = base_args();
        empty_tenant.tenant = Some(String::new());
        assert_eq!(
            exit_code_for(&build_query(&empty_tenant).unwrap_err()),
            Exit::Usage
        );

        let mut with_tenant = base_args();
        with_tenant.tenant = Some("org_acme".to_owned());
        let query = build_query(&with_tenant).unwrap();
        assert_eq!(query.tenant, None);
        assert_eq!(query.tenant_or_on_behalf_of.as_deref(), Some("org_acme"));
    }

    #[test]
    fn format_actor_handles_direct_on_behalf_of_and_absent() {
        assert_eq!(format_actor(None), "-");

        let direct = RawValue::from_string(r#"{"type":"user","id":"usr_42"}"#.to_owned()).unwrap();
        assert_eq!(format_actor(Some(&direct)), "user:usr_42");

        let delegated = RawValue::from_string(
            r#"{"type":"ai_agent","id":"bot_7","onBehalfOf":{"type":"user","id":"usr_42"}}"#
                .to_owned(),
        )
        .unwrap();
        assert_eq!(
            format_actor(Some(&delegated)),
            "ai_agent:bot_7 for user:usr_42"
        );
    }
}
