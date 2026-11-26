//! `auditstream verify` command implementation.
//!
//! Runs D7's read-only drift detection and partition coverage checks (`>= 2` months covered ahead),
//! formats findings as one line per finding (text) or structured JSON, optionally creates 3 months
//! of future partitions via `--fix` (D8 `ensure_ahead`), and exits `1` when findings meet or exceed
//! the configured `--fail-on` severity threshold (default: `error`).

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
    run_async as run_command_async, sanitize_terminal, unmatched_configured_excludes,
};
use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt, write_stdout};
use crate::logging;

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

/// Minimum finding severity that causes `auditstream verify` to fail with exit code `1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum FailOn {
    /// Fail only when at least one `error` finding is present (warnings exit `0`).
    #[default]
    Error,
    /// Fail when any `error` or `warning` finding is present.
    Warning,
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

    /// Minimum finding severity that causes verification to fail (`error` or `warning`).
    #[arg(long, value_enum, default_value_t = FailOn::Error)]
    pub(crate) fail_on: FailOn,
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

async fn ensure_partitions_if_requested(
    pool: &auditstream_pg::PgPool,
    config: &Config,
    fix: bool,
) -> anyhow::Result<Vec<String>> {
    if !fix {
        return Ok(Vec::new());
    }
    let ensure = partitions::ensure_ahead(pool, &config.schema, DEFAULT_MONTHS_AHEAD)
        .await
        .map_err(|err| anyhow!("failed to ensure future partitions: {err}"))
        .exit_code(Exit::Database)?;

    let created_partitions: Vec<String> = ensure
        .created
        .iter()
        .map(|ym| ym.partition_name())
        .collect();

    if created_partitions.is_empty() {
        logging::emit_note(
            config.log_format,
            config.quiet,
            &format!("fix: partitions already covered {DEFAULT_MONTHS_AHEAD} months ahead"),
        );
    } else {
        logging::emit_note(
            config.log_format,
            config.quiet,
            &format!(
                "fix: created {} partition(s): {}",
                created_partitions.len(),
                created_partitions.join(", ")
            ),
        );
    }
    Ok(created_partitions)
}

#[allow(clippy::too_many_lines)]
async fn run_async(args: &Args, config: &Config) -> anyhow::Result<()> {
    let db_url = require_database_url(config)?;

    let configured_tables = parse_table_refs(&config.capture.tables, true)?;
    let configured_excludes = parse_exclude_refs(&config.capture.exclude, true)?;

    let pool = connect_database(db_url).await?;

    // Optional --fix step runs D8's ensure_ahead(3) before reading verification state.
    let created_partitions = ensure_partitions_if_requested(&pool, config, args.fix).await?;

    let introspect_opts = IntrospectOptions::with_audit_schema(&config.schema);
    let all_user_schemas: BTreeSet<String> = introspect::user_schemas(&pool, &introspect_opts)
        .await
        .map_err(|err| anyhow!("failed to list database schemas: {err}"))
        .exit_code(Exit::Database)?
        .into_iter()
        .collect();

    let excludes_by_table = group_excludes_by_table(&configured_excludes);

    let specs = if configured_tables.is_empty() {
        logging::emit_note(
            config.log_format,
            config.quiet,
            "note: no capture.tables configured; verifying installed triggers only",
        );
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

        build_installed_verify_specs(&installed, &excludes_by_table, &introspected_map)
    } else {
        let configured_set: BTreeSet<(String, String)> = configured_tables.into_iter().collect();

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

    let unmatched_excludes = unmatched_configured_excludes(&excludes_by_table, &specs);
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
    append_cli_findings(
        &mut findings,
        &config.schema,
        history_stats.exists,
        history_stats.months_covered_ahead,
        &unmatched_excludes,
    );

    let failing_count = failing_findings_count(&findings, args.fail_on);
    let ok = failing_count == 0;
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
            "verification failed with {failing_count} finding(s)"
        ))
        .exit_code(Exit::CheckFailed);
    }

    Ok(())
}

fn append_cli_findings(
    findings: &mut Vec<VerifyFinding>,
    schema: &str,
    history_exists: bool,
    months_covered_ahead: u32,
    unmatched_excludes: &[(String, String, Vec<String>)],
) {
    for (tbl_schema, tbl_name, cols) in unmatched_excludes {
        let fq_table = format!("{tbl_schema}.{tbl_name}");
        findings.push(VerifyFinding {
            severity: "error",
            kind: "invalid_exclusion",
            table: Some(fq_table.clone()),
            object: fq_table.clone(),
            detail: format!(
                "excluded column(s) {} in capture.exclude target table `{fq_table}`, which is not in the verified table set",
                cols.join(", ")
            ),
            hint: "Update capture.exclude to remove primary key, non-existent, or unverified table columns and re-run `auditstream init`."
                .to_owned(),
        });
    }

    if history_exists && months_covered_ahead < MIN_MONTHS_COVERED_AHEAD {
        findings.push(VerifyFinding {
            severity: "error",
            kind: "insufficient_partitions",
            table: None,
            object: format!("{schema}.changes"),
            detail: format!(
                "only {months_covered_ahead} month(s) covered ahead (minimum required: {MIN_MONTHS_COVERED_AHEAD})"
            ),
            hint: "Run `auditstream verify --fix` to create 3 months of future partitions."
                .to_owned(),
        });
    }

    findings.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then_with(|| a.table.cmp(&b.table))
            .then_with(|| a.object.cmp(&b.object))
            .then_with(|| kind_rank(a.kind).cmp(&kind_rank(b.kind)))
            .then_with(|| a.kind.cmp(b.kind))
            .then_with(|| a.detail.cmp(&b.detail))
    });
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    }
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "missing" => 0,
        "disabled" => 1,
        "outdated" => 2,
        "partitioned_table" => 3,
        "default_partition_rows" => 4,
        "invalid_exclusion" => 5,
        "unsafe_privileges" => 6,
        "invalid_ownership" => 7,
        "tampered_history" => 8,
        "insufficient_partitions" => 9,
        _ => 10,
    }
}

fn failing_findings_count(findings: &[VerifyFinding], fail_on: FailOn) -> usize {
    match fail_on {
        FailOn::Error => findings.iter().filter(|f| f.severity == "error").count(),
        FailOn::Warning => findings.len(),
    }
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
        FindingKind::InvalidExclusion => (
            "invalid_exclusion",
            "Update capture.exclude to remove primary key or non-existent columns and re-run `auditstream init`."
                .to_owned(),
        ),
        FindingKind::UnsafePrivileges => (
            "unsafe_privileges",
            "Re-run `auditstream init` and apply the generated migration (or revoke the extra privileges, role attributes/memberships, or parameter grants)."
                .to_owned(),
        ),
        FindingKind::InvalidOwnership => (
            "invalid_ownership",
            "Re-run `auditstream init` and apply the generated migration so `auditstream_owner` owns all Auditstream objects."
                .to_owned(),
        ),
        FindingKind::TamperedHistory => (
            "tampered_history",
            if finding.table.is_none() {
                "Inspect and drop unexpected helper function overloads from the audit schema."
                    .to_owned()
            } else {
                "Inspect and remove unexpected triggers, rewrite rules, or Row-Level Security from the history table and its partitions."
                    .to_owned()
            },
        ),
        _ => (
            "unknown",
            "Inspect the finding details and re-run `auditstream init`.".to_owned(),
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
        let scope = sanitize_terminal(finding.table.as_deref().unwrap_or(&finding.object));
        let detail = sanitize_terminal(&finding.detail);
        let hint = sanitize_terminal(&finding.hint);
        let _ = writeln!(
            out,
            "[{}] {}  {}  {} — hint: {}",
            finding.severity, scope, finding.kind, detail, hint
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use auditstream_pg::verify::{Finding, FindingKind, Severity};

    use super::{
        FailOn, VerifyFinding, VerifyJsonReport, append_cli_findings, convert_finding,
        failing_findings_count, render_text,
    };

    #[test]
    fn append_cli_findings_adds_unmatched_excludes_and_sorts_errors_before_warnings() {
        let mut findings = vec![
            VerifyFinding {
                severity: "warning",
                kind: "partitioned_table",
                table: Some("public.orders".to_owned()),
                object: "public.orders".to_owned(),
                detail: "partitioned".to_owned(),
                hint: "route through parent".to_owned(),
            },
            VerifyFinding {
                severity: "error",
                kind: "invalid_ownership",
                table: Some("auditstream.changes".to_owned()),
                object: "auditstream.changes".to_owned(),
                detail: "wrong owner".to_owned(),
                hint: "fix owner".to_owned(),
            },
            VerifyFinding {
                severity: "error",
                kind: "unsafe_privileges",
                table: Some("auditstream.changes".to_owned()),
                object: "auditstream.changes".to_owned(),
                detail: "extra grant".to_owned(),
                hint: "revoke".to_owned(),
            },
        ];
        let unmatched = vec![(
            "public".to_owned(),
            "typo_table".to_owned(),
            vec!["secret".to_owned()],
        )];
        append_cli_findings(&mut findings, "auditstream", true, 1, &unmatched);
        assert_eq!(findings.len(), 5);
        assert_eq!(findings[0].severity, "error");
        assert_eq!(findings[0].kind, "insufficient_partitions");
        // Preserves `FindingKind` declaration order: `unsafe_privileges` before `invalid_ownership`.
        assert_eq!(findings[1].kind, "unsafe_privileges");
        assert_eq!(findings[2].kind, "invalid_ownership");
        assert_eq!(findings[3].kind, "invalid_exclusion");
        assert_eq!(findings[3].object, "public.typo_table");
        assert_eq!(findings[4].severity, "warning");
        assert_eq!(findings[4].kind, "partitioned_table");
    }

    #[test]
    fn failing_findings_count_respects_fail_on_threshold() {
        let warning = VerifyFinding {
            severity: "warning",
            kind: "partitioned_table",
            table: Some("public.orders".to_owned()),
            object: "public.orders".to_owned(),
            detail: "partitioned".to_owned(),
            hint: "route through parent".to_owned(),
        };
        let error = VerifyFinding {
            severity: "error",
            kind: "disabled",
            table: Some("public.invoice".to_owned()),
            object: "public.invoice.auditstream_update".to_owned(),
            detail: "disabled".to_owned(),
            hint: "re-enable".to_owned(),
        };

        assert_eq!(failing_findings_count(&[], FailOn::Error), 0);
        assert_eq!(failing_findings_count(&[], FailOn::Warning), 0);

        let warn_only = [warning.clone()];
        assert_eq!(failing_findings_count(&warn_only, FailOn::Error), 0);
        assert_eq!(failing_findings_count(&warn_only, FailOn::Warning), 1);

        let mixed = [error, warning];
        assert_eq!(failing_findings_count(&mixed, FailOn::Error), 1);
        assert_eq!(failing_findings_count(&mixed, FailOn::Warning), 2);
    }

    #[test]
    fn convert_finding_maps_tampered_history_and_invalid_exclusion() {
        let tampered = convert_finding(&Finding {
            kind: FindingKind::TamperedHistory,
            severity: Severity::Error,
            table: Some("auditstream.changes".to_owned()),
            object: "auditstream.changes.evil_trig".to_owned(),
            detail: "unexpected trigger".to_owned(),
        });
        assert_eq!(tampered.kind, "tampered_history");
        assert_eq!(tampered.severity, "error");
        assert!(tampered.hint.contains("Row-Level Security"));

        let overload = convert_finding(&Finding {
            kind: FindingKind::TamperedHistory,
            severity: Severity::Error,
            table: None,
            object: "auditstream.log_event(action text, target text, data text)".to_owned(),
            detail: "unexpected function overload".to_owned(),
        });
        assert_eq!(overload.kind, "tampered_history");
        assert!(overload.hint.contains("overload"));

        let invalid_ex = convert_finding(&Finding {
            kind: FindingKind::InvalidExclusion,
            severity: Severity::Error,
            table: Some("public.invoice".to_owned()),
            object: "public.invoice".to_owned(),
            detail: "missing column".to_owned(),
        });
        assert_eq!(invalid_ex.kind, "invalid_exclusion");
        assert!(invalid_ex.hint.contains("non-existent"));
    }

    #[test]
    fn render_text_sanitizes_control_ansi_and_bidi_sequences() {
        let report = VerifyJsonReport {
            ok: false,
            schema: "auditstream".to_owned(),
            months_covered_ahead: 3,
            created_partitions: Vec::new(),
            findings: vec![VerifyFinding {
                severity: "error",
                kind: "disabled",
                table: Some("public.\x1b[31minv\u{202E}oice\x1b[0m".to_owned()),
                object: "public.inv".to_owned(),
                detail: "bad\x1b]52;c;YWJj\x07\x08detail\u{009B}2J".to_owned(),
                hint: "fix\rnow\u{2066}please\u{2069}".to_owned(),
            }],
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
        assert!(rendered.contains("baddetail"), "{rendered:?}");
        assert!(rendered.contains("fix nowplease"), "{rendered:?}");
    }
}
