//! `auditstream verify` command implementation.
//!
//! Runs D7's read-only drift detection and partition coverage checks (`>= 2` months covered ahead),
//! formats findings as one line per finding (text) or structured JSON, optionally creates 3 months
//! of future partitions via `--fix` (D8 `ensure_ahead`), and exits `1` when any finding is present.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;

use anyhow::anyhow;
use auditstream_pg::introspect::{self, IntrospectOptions, TableInfo};
use auditstream_pg::partitions::{self, DEFAULT_MONTHS_AHEAD};
use auditstream_pg::verify::{self, Finding, FindingKind, Severity, VerifyOptions};
use clap::{Args as ClapArgs, ValueEnum};
use serde::Serialize;

use crate::commands::common::{
    build_configured_verify_specs, build_installed_verify_specs, connect_database,
    group_excludes_by_table, parse_exclude_refs, parse_table_refs, require_database_url,
    run_async as run_command_async,
};
use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt, write_stdout};

/// Minimum future monthly partitions required ahead of the current UTC month.
const MIN_MONTHS_COVERED_AHEAD: u32 = 2;

/// Output format for `auditstream verify`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable one-line-per-finding output.
    #[default]
    Text,
    /// Machine-readable JSON report.
    Json,
}

/// Arguments for `auditstream verify`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Output format (`text` or `json`).
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub(crate) format: OutputFormat,

    /// Create missing monthly partitions 3 months ahead before verifying.
    #[arg(long)]
    pub(crate) fix: bool,
}

#[derive(Debug, Clone, Serialize)]
struct VerifyJsonReport {
    ok: bool,
    schema: String,
    months_covered_ahead: u32,
    created_partitions: Vec<String>,
    findings: Vec<VerifyFinding>,
}

#[derive(Debug, Clone, Serialize)]
struct VerifyFinding {
    severity: &'static str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    table: Option<String>,
    object: String,
    detail: String,
    hint: String,
}

/// Runs the `verify` command.
pub(crate) fn run(args: &Args, config: &Config) -> anyhow::Result<()> {
    run_command_async(run_async(args, config))
}

#[allow(clippy::too_many_lines)]
async fn run_async(args: &Args, config: &Config) -> anyhow::Result<()> {
    let db_url = require_database_url(config)?;

    let configured_tables = parse_table_refs(&config.capture.tables, true)?;
    let configured_excludes = parse_exclude_refs(&config.capture.exclude, true)?;

    let pool = connect_database(db_url).await?;

    // Optional --fix step runs D8's ensure_ahead(3) before reading verification state.
    let mut created_partitions = Vec::new();
    if args.fix {
        let ensure = partitions::ensure_ahead(&pool, &config.schema, DEFAULT_MONTHS_AHEAD)
            .await
            .map_err(|err| anyhow!("failed to ensure future partitions: {err}"))
            .exit_code(Exit::Database)?;

        created_partitions = ensure
            .created
            .iter()
            .map(|ym| ym.partition_name())
            .collect();

        if created_partitions.is_empty() {
            eprintln!("fix: partitions already covered {DEFAULT_MONTHS_AHEAD} months ahead");
        } else {
            eprintln!(
                "fix: created {} partition(s): {}",
                created_partitions.len(),
                created_partitions.join(", ")
            );
        }
    }

    let introspect_opts = IntrospectOptions::with_audit_schema(&config.schema);
    let all_user_schemas: BTreeSet<String> = introspect::user_schemas(&pool, &introspect_opts)
        .await
        .map_err(|err| anyhow!("failed to list database schemas: {err}"))
        .exit_code(Exit::Database)?
        .into_iter()
        .collect();

    let specs = if configured_tables.is_empty() {
        eprintln!("note: no capture.tables configured; verifying installed triggers only");
        let installed = verify::installed_captures(&pool, &config.schema)
            .await
            .map_err(|err| anyhow!("failed to inspect installed auditstream triggers: {err}"))
            .exit_code(Exit::Database)?;

        let installed_schemas: Vec<&str> = installed
            .iter()
            .map(|c| c.schema.as_str())
            .filter(|s| all_user_schemas.contains(*s))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let introspected = if installed_schemas.is_empty() {
            Vec::new()
        } else {
            introspect::tables_with_options(&pool, &installed_schemas, &introspect_opts)
                .await
                .map_err(|err| anyhow!("failed to introspect database tables: {err}"))
                .exit_code(Exit::Database)?
        };
        let introspected_map: BTreeMap<(String, String), &TableInfo> = introspected
            .iter()
            .map(|t| ((t.schema.clone(), t.name.clone()), t))
            .collect();

        build_installed_verify_specs(&installed, &introspected_map)
    } else {
        let configured_set: BTreeSet<(String, String)> = configured_tables.into_iter().collect();
        let excludes_by_table = group_excludes_by_table(&configured_excludes);

        let target_schemas: Vec<&str> = configured_set
            .iter()
            .map(|(s, _)| s.as_str())
            .filter(|s| all_user_schemas.contains(*s))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let introspected = if target_schemas.is_empty() {
            Vec::new()
        } else {
            introspect::tables_with_options(&pool, &target_schemas, &introspect_opts)
                .await
                .map_err(|err| anyhow!("failed to introspect database tables: {err}"))
                .exit_code(Exit::Database)?
        };
        let introspected_map: BTreeMap<(String, String), &TableInfo> = introspected
            .iter()
            .map(|t| ((t.schema.clone(), t.name.clone()), t))
            .collect();

        build_configured_verify_specs(&configured_set, &excludes_by_table, &introspected_map)
    };

    let d7_report = verify::check_with_options(&pool, &VerifyOptions::new(&config.schema, specs))
        .await
        .map_err(|err| anyhow!("failed to run auditstream verification: {err}"))
        .exit_code(Exit::Database)?;

    let history_stats = partitions::history_stats(&pool, &config.schema)
        .await
        .map_err(|err| anyhow!("failed to check partition coverage: {err}"))
        .exit_code(Exit::Database)?;
    pool.close().await;

    let mut findings: Vec<VerifyFinding> = d7_report.findings.iter().map(convert_finding).collect();

    if history_stats.exists && history_stats.months_covered_ahead < MIN_MONTHS_COVERED_AHEAD {
        findings.push(VerifyFinding {
            severity: "error",
            kind: "insufficient_partitions",
            table: None,
            object: format!("{}.changes", config.schema),
            detail: format!(
                "only {} month(s) covered ahead (minimum required: {MIN_MONTHS_COVERED_AHEAD})",
                history_stats.months_covered_ahead
            ),
            hint: "Run `auditstream verify --fix` to create 3 months of future partitions."
                .to_owned(),
        });
    }

    let ok = findings.is_empty();
    let json_report = VerifyJsonReport {
        ok,
        schema: config.schema.clone(),
        months_covered_ahead: history_stats.months_covered_ahead,
        created_partitions,
        findings,
    };

    let rendered = match args.format {
        OutputFormat::Text => render_text(&json_report),
        OutputFormat::Json => {
            let mut json = serde_json::to_string_pretty(&json_report)
                .map_err(|err| anyhow!("failed to serialize verify JSON: {err}"))
                .exit_code(Exit::Internal)?;
            json.push('\n');
            json
        }
    };

    let mut out = io::stdout().lock();
    write_stdout(&mut out, rendered.as_bytes())?;

    if !ok {
        return Err(anyhow!(
            "verification failed with {} finding(s)",
            json_report.findings.len()
        ))
        .exit_code(Exit::CheckFailed);
    }

    Ok(())
}

fn convert_finding(finding: &Finding) -> VerifyFinding {
    let severity = match finding.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let (kind, hint) = match finding.kind {
        FindingKind::Missing => (
            "missing",
            "Run `auditstream init` and apply the generated migration.".to_owned(),
        ),
        FindingKind::Disabled => (
            "disabled",
            format!(
                "Re-enable `{}` or re-apply the Auditstream migration.",
                finding.object
            ),
        ),
        FindingKind::Outdated => (
            "outdated",
            "Run `auditstream init` and apply the updated migration.".to_owned(),
        ),
        FindingKind::PartitionedTable => (
            "partitioned_table",
            "Route INSERT, DELETE, and TRUNCATE writes through the parent table instead of child partitions directly."
                .to_owned(),
        ),
        FindingKind::DefaultPartitionRows => (
            "default_partition_rows",
            "Move rows out of changes_default into monthly partitions, then run `auditstream verify --fix`."
                .to_owned(),
        ),
    };

    VerifyFinding {
        severity,
        kind,
        table: finding.table.clone(),
        object: finding.object.clone(),
        detail: finding.detail.clone(),
        hint,
    }
}

fn render_text(report: &VerifyJsonReport) -> String {
    let mut out = String::new();
    if report.findings.is_empty() {
        let _ = writeln!(out, "ok: no findings");
        return out;
    }

    for finding in &report.findings {
        let scope = finding.table.as_deref().unwrap_or(&finding.object);
        let _ = writeln!(
            out,
            "[{}] {}  {}  {} — hint: {}",
            finding.severity, scope, finding.kind, finding.detail, finding.hint
        );
    }
    out
}
