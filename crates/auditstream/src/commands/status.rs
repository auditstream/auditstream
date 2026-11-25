//! `auditstream status` command implementation.
//!
//! Summarizes capture coverage across tables in scope and reports cheap catalog-only
//! history table statistics (`pg_class.reltuples`, `pg_total_relation_size`, and
//! `EXISTS (... LIMIT 1)` on `changes_default` — never `count(*)`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;

use anyhow::anyhow;
use auditstream_pg::introspect::{self, IntrospectOptions, TableInfo};
use auditstream_pg::partitions::{self, HistoryTableStats};
use auditstream_pg::verify::{self, InstalledTableCapture, VerifyOptions};
use clap::{Args as ClapArgs, ValueEnum};
use serde::Serialize;

use crate::commands::common::{
    build_configured_verify_specs, build_installed_verify_specs, connect_database,
    group_excludes_by_table, parse_exclude_refs, parse_table_refs, require_database_url,
    run_async as run_command_async, sanitize_terminal,
};
use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt, write_stdout};

/// Minimum future monthly partitions required to consider partition coverage healthy.
const MIN_MONTHS_COVERED_AHEAD: u32 = 2;

/// Expected number of `auditstream_*` triggers installed per audited table.
const EXPECTED_TRIGGERS_PER_TABLE: usize = 4;

/// Output format for `auditstream status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable text summary.
    #[default]
    Text,
    /// Machine-readable JSON summary.
    Json,
}

/// Arguments for `auditstream status`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Output format (`text` or `json`).
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub(crate) format: OutputFormat,
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    schema: String,
    tables: Vec<TableStatus>,
    history: HistoryStatus,
    problems_found: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct TableStatus {
    table: String,
    #[serde(skip)]
    schema: String,
    #[serde(skip)]
    name: String,
    captured: bool,
    configured: bool,
    trigger_state: &'static str,
    excluded_columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct HistoryStatus {
    exists: bool,
    partitions: Vec<String>,
    months_covered_ahead: u32,
    default_has_rows: bool,
    estimated_rows: i64,
    total_size_bytes: i64,
}

/// Runs the `status` command.
pub(crate) fn run(args: &Args, config: &Config) -> anyhow::Result<()> {
    run_command_async(run_async(args, config))
}

#[allow(clippy::too_many_lines)]
async fn run_async(args: &Args, config: &Config) -> anyhow::Result<()> {
    let db_url = require_database_url(config)?;

    let configured_tables = parse_table_refs(&config.capture.tables, true)?;
    let configured_excludes = parse_exclude_refs(&config.capture.exclude, true)?;

    let pool = connect_database(db_url).await?;

    let introspect_opts = IntrospectOptions::with_audit_schema(&config.schema);

    let all_user_schemas: BTreeSet<String> = introspect::user_schemas(&pool, &introspect_opts)
        .await
        .map_err(|err| anyhow!("failed to list database schemas: {err}"))
        .exit_code(Exit::Database)?
        .into_iter()
        .collect();

    let installed = verify::installed_captures(&pool, &config.schema)
        .await
        .map_err(|err| anyhow!("failed to inspect installed auditstream triggers: {err}"))
        .exit_code(Exit::Database)?;

    let history_stats = partitions::history_stats(&pool, &config.schema)
        .await
        .map_err(|err| anyhow!("failed to inspect history table catalog statistics: {err}"))
        .exit_code(Exit::Database)?;

    // Scope schemas: every schema named in capture.tables (or `public` without a selection),
    // plus any schema that already holds installed auditstream triggers.
    let mut scope_schemas: BTreeSet<String> = BTreeSet::new();
    if configured_tables.is_empty() {
        if all_user_schemas.contains("public") {
            scope_schemas.insert("public".to_owned());
        }
    } else {
        for (schema, _) in &configured_tables {
            if all_user_schemas.contains(schema) {
                scope_schemas.insert(schema.clone());
            }
        }
    }
    for cap in &installed {
        if all_user_schemas.contains(&cap.schema) {
            scope_schemas.insert(cap.schema.clone());
        }
    }

    let schema_refs: Vec<&str> = scope_schemas.iter().map(String::as_str).collect();
    let introspected = if schema_refs.is_empty() {
        Vec::new()
    } else {
        introspect::tables_with_options(&pool, &schema_refs, &introspect_opts)
            .await
            .map_err(|err| anyhow!("failed to introspect database tables: {err}"))
            .exit_code(Exit::Database)?
    };

    let introspected_map: BTreeMap<(String, String), &TableInfo> = introspected
        .iter()
        .map(|t| ((t.schema.clone(), t.name.clone()), t))
        .collect();

    let installed_map: BTreeMap<(String, String), &InstalledTableCapture> = installed
        .iter()
        .map(|c| ((c.schema.clone(), c.table.clone()), c))
        .collect();

    let configured_set: BTreeSet<(String, String)> = configured_tables.iter().cloned().collect();
    let configured_excludes_by_table = group_excludes_by_table(&configured_excludes);

    // Build expected specs for D7 drift detection so status knows whether to print a verify hint.
    let verify_specs = if configured_set.is_empty() {
        build_installed_verify_specs(&installed, &introspected_map)
    } else {
        build_configured_verify_specs(
            &configured_set,
            &configured_excludes_by_table,
            &introspected_map,
        )
    };
    let verify_report =
        verify::check_with_options(&pool, &VerifyOptions::new(&config.schema, verify_specs))
            .await
            .map_err(|err| anyhow!("failed to check auditstream health: {err}"))
            .exit_code(Exit::Database)?;
    pool.close().await;

    let partition_gap_problem = usize::from(
        history_stats.exists && history_stats.months_covered_ahead < MIN_MONTHS_COVERED_AHEAD,
    );
    let problems_found = verify_report.findings.len() + partition_gap_problem;

    // Union of introspected tables in scope, configured tables, and tables with installed triggers.
    let mut all_tables: BTreeSet<(String, String)> = BTreeSet::new();
    for key in introspected_map.keys() {
        all_tables.insert(key.clone());
    }
    for key in &configured_set {
        all_tables.insert(key.clone());
    }
    for key in installed_map.keys() {
        all_tables.insert(key.clone());
    }

    let tables: Vec<TableStatus> = all_tables
        .into_iter()
        .map(|key| {
            let configured = configured_set.contains(&key);
            let live_capture = installed_map.get(&key).copied();
            let (captured, trigger_state, excluded_columns) = if let Some(cap) = live_capture {
                let mut excl = cap.excluded_columns.clone();
                excl.sort();
                excl.dedup();
                (true, classify_trigger_state(cap), excl)
            } else {
                let excl = configured_excludes_by_table
                    .get(&key)
                    .map(|set| set.iter().cloned().collect())
                    .unwrap_or_default();
                (false, "none", excl)
            };
            TableStatus {
                table: format!("{}.{}", key.0, key.1),
                schema: key.0,
                name: key.1,
                captured,
                configured,
                trigger_state,
                excluded_columns,
            }
        })
        .collect();

    let history = build_history_status(&history_stats);
    let hint = (problems_found > 0)
        .then(|| format!("Run `auditstream verify` to inspect {problems_found} problem(s)."));

    let report = StatusReport {
        schema: config.schema.clone(),
        tables,
        history,
        problems_found,
        hint,
    };

    let rendered = match args.format {
        OutputFormat::Text => render_text(&report),
        OutputFormat::Json => {
            let mut json = serde_json::to_string_pretty(&report)
                .map_err(|err| anyhow!("failed to serialize status JSON: {err}"))
                .exit_code(Exit::Internal)?;
            json.push('\n');
            json
        }
    };

    let mut out = io::stdout().lock();
    write_stdout(&mut out, rendered.as_bytes())?;
    Ok(())
}

fn classify_trigger_state(cap: &InstalledTableCapture) -> &'static str {
    if cap.triggers.is_empty() {
        return "none";
    }
    if cap.triggers.iter().any(|(_, state)| state != "O") {
        return "disabled";
    }
    if cap.triggers.len() < EXPECTED_TRIGGERS_PER_TABLE {
        return "partial";
    }
    "enabled"
}

fn build_history_status(stats: &HistoryTableStats) -> HistoryStatus {
    HistoryStatus {
        exists: stats.exists,
        partitions: stats
            .monthly_partitions
            .iter()
            .map(|ym| ym.partition_name())
            .collect(),
        months_covered_ahead: stats.months_covered_ahead,
        default_has_rows: stats.default_has_rows,
        estimated_rows: stats.estimated_rows,
        total_size_bytes: stats.total_size_bytes,
    }
}

fn render_text(report: &StatusReport) -> String {
    let mut out = String::new();
    let captured_count = report.tables.iter().filter(|t| t.captured).count();
    let _ = writeln!(
        out,
        "Tables ({} total, {} captured):",
        report.tables.len(),
        captured_count
    );

    if report.tables.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for tbl in &report.tables {
            let state_label = if tbl.captured {
                "captured"
            } else {
                "uncaptured"
            };
            let excl_display = if tbl.excluded_columns.is_empty() {
                "none".to_owned()
            } else {
                tbl.excluded_columns
                    .iter()
                    .map(|col| sanitize_terminal(col))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let table_display = if tbl.schema.is_empty() && tbl.name.is_empty() {
                sanitize_terminal(&tbl.table)
            } else {
                format!(
                    "{}.{}",
                    sanitize_terminal(&tbl.schema),
                    sanitize_terminal(&tbl.name)
                )
            };
            let _ = writeln!(
                out,
                "  {}  {}  triggers={}  excluded={}",
                table_display, state_label, tbl.trigger_state, excl_display
            );
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "History table ({}.changes):",
        sanitize_terminal(&report.schema)
    );
    if report.history.exists {
        let part_summary = match (
            report.history.partitions.first(),
            report.history.partitions.last(),
        ) {
            (Some(first), Some(last)) if first == last => {
                format!("1 monthly ({})", sanitize_terminal(first))
            }
            (Some(first), Some(last)) => {
                format!(
                    "{} monthly ({}..{})",
                    report.history.partitions.len(),
                    sanitize_terminal(first),
                    sanitize_terminal(last)
                )
            }
            _ => "0 monthly".to_owned(),
        };
        let _ = writeln!(out, "  partitions: {part_summary}");
        let _ = writeln!(
            out,
            "  months_covered_ahead: {}",
            report.history.months_covered_ahead
        );
        let _ = writeln!(
            out,
            "  default_has_rows: {}",
            if report.history.default_has_rows {
                "yes"
            } else {
                "no"
            }
        );
        let _ = writeln!(out, "  estimated_rows: {}", report.history.estimated_rows);
        let _ = writeln!(
            out,
            "  total_size_bytes: {}",
            report.history.total_size_bytes
        );
    } else {
        let _ = writeln!(out, "  status: not installed");
    }

    if let Some(hint) = &report.hint {
        let _ = writeln!(out);
        let _ = writeln!(out, "Hint: {}", sanitize_terminal(hint));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{HistoryStatus, StatusReport, TableStatus, render_text};

    #[test]
    fn render_text_sanitizes_control_ansi_and_bidi_sequences() {
        let report = StatusReport {
            schema: "audit\x1b[31mstream\x1b[0m".to_owned(),
            tables: vec![TableStatus {
                table: "public\x1b]52;c;unterminated.\x1b]52;c;YWJj\x07inv\u{202E}oice".to_owned(),
                schema: "public\x1b]52;c;unterminated".to_owned(),
                name: "\x1b]52;c;YWJj\x07inv\u{202E}oice".to_owned(),
                captured: true,
                configured: true,
                trigger_state: "enabled",
                excluded_columns: vec!["sec\x08ret\u{009B}31m".to_owned()],
            }],
            history: HistoryStatus {
                exists: true,
                partitions: vec!["changes_\r2026_11".to_owned()],
                months_covered_ahead: 3,
                default_has_rows: false,
                estimated_rows: 0,
                total_size_bytes: 0,
            },
            problems_found: 1,
            hint: Some("check\u{2066}verify\u{2069}".to_owned()),
        };

        let rendered = render_text(&report);
        assert!(!rendered.contains('\x1b'), "ESC leaked: {rendered:?}");
        assert!(!rendered.contains('\x07'), "BEL leaked: {rendered:?}");
        assert!(!rendered.contains('\x08'), "BS leaked: {rendered:?}");
        assert!(!rendered.contains('\r'), "CR leaked: {rendered:?}");
        assert!(
            !rendered.contains('\u{009B}'),
            "8-bit CSI leaked: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{202E}'),
            "BiDi RLO leaked: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{2066}'),
            "BiDi LRI leaked: {rendered:?}"
        );
        assert!(rendered.contains("public.invoice"), "{rendered:?}");
        assert!(rendered.contains("excluded=secret"), "{rendered:?}");
        assert!(rendered.contains("auditstream.changes"), "{rendered:?}");
    }
}
