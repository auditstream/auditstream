//! Report data structures, row builders, and Markdown rendering for `xtask bench`.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::compare::{ComparisonReport, render_comparison_markdown};
use super::stats::{EnvironmentMetadata, SampleStats, f64_from_u64, ms_cost_pct, tps_cost_pct};

pub(super) const REPORT_SCHEMA_VERSION: u32 = 2;

const fn default_schema_version() -> u32 {
    1
}

const fn default_samples() -> u32 {
    1
}

/// Complete serializable benchmark report written to `--json-out` and consumed by `--compare`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct BenchReport {
    #[serde(default = "default_schema_version")]
    pub(super) schema_version: u32,
    pub(super) postgres_version: String,
    pub(super) git_commit: String,
    #[serde(default)]
    pub(super) environment: Option<EnvironmentMetadata>,
    pub(super) pgbench_settings: String,
    #[serde(default = "default_samples")]
    pub(super) samples: u32,
    #[serde(default)]
    pub(super) warmup_secs: u32,
    pub(super) bulk_iterations: u32,
    #[serde(default)]
    pub(super) export_rows: u64,
    #[serde(default)]
    pub(super) history_runs: u32,
    pub(super) workloads: Vec<WorkloadRow>,
    pub(super) index_variants: Vec<IndexVariantRow>,
    #[serde(default)]
    pub(super) export_workloads: Vec<ExportWorkloadRow>,
    #[serde(default)]
    pub(super) history_workloads: Vec<HistoryWorkloadRow>,
}

/// Single capture or bulk workload row (reporting median across `--samples`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct WorkloadRow {
    pub(super) name: String,
    pub(super) metric_unit: String,
    pub(super) value: f64,
    pub(super) us_per_row: f64,
    pub(super) cost_pct: Option<f64>,
    pub(super) cost_us_per_row: Option<f64>,
    pub(super) rows_written: u64,
    pub(super) rows_captured: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stats: Option<SampleStats>,
}

/// Lookup-index comparison row across worst-case UPDATE and 10k bulk INSERT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct IndexVariantRow {
    pub(super) variant: String,
    pub(super) label: String,
    pub(super) update_comment_tps: f64,
    pub(super) update_comment_cost_pct: f64,
    pub(super) update_comment_cost_us: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) update_comment_stats: Option<SampleStats>,
    pub(super) update_setcfg_tps: f64,
    pub(super) update_setcfg_cost_pct: f64,
    pub(super) update_setcfg_cost_us: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) update_setcfg_stats: Option<SampleStats>,
    pub(super) bulk_insert_10k_ms: f64,
    pub(super) bulk_insert_10k_cost_pct: f64,
    pub(super) bulk_insert_10k_cost_us: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) bulk_insert_10k_stats: Option<SampleStats>,
}

/// `auditstream export` throughput benchmark row (plain JSONL vs redacted JSONL).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct ExportWorkloadRow {
    pub(super) name: String,
    pub(super) rows_exported: u64,
    pub(super) events_per_sec: f64,
    pub(super) elapsed_ms: f64,
    pub(super) overhead_pct: Option<f64>,
    pub(super) stats: SampleStats,
}

/// `auditstream history` point-in-time record lookup latency benchmark row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct HistoryWorkloadRow {
    pub(super) name: String,
    pub(super) queries_per_sample: u32,
    pub(super) p50_ms: f64,
    pub(super) p99_ms: f64,
    pub(super) p50_stats: SampleStats,
    pub(super) p99_stats: SampleStats,
}

pub(super) fn tps_row(
    name: &str,
    stats: SampleStats,
    total_tx: u64,
    baseline: Option<(f64, f64)>,
    captured: bool,
) -> WorkloadRow {
    let median_tps = stats.median;
    let us_per_row = 1_000_000.0 / median_tps;
    WorkloadRow {
        name: name.to_owned(),
        metric_unit: "tps".to_owned(),
        value: median_tps,
        us_per_row,
        cost_pct: baseline.map(|(base_tps, _)| tps_cost_pct(base_tps, median_tps)),
        cost_us_per_row: baseline.map(|(_, base_us)| us_per_row - base_us),
        rows_written: total_tx,
        rows_captured: if captured { total_tx } else { 0 },
        stats: Some(stats),
    }
}

pub(super) fn bulk_row(
    name: &str,
    stats: SampleStats,
    rows_per_stmt: u64,
    total_rows: u64,
    baseline: Option<(f64, f64)>,
    captured: bool,
) -> WorkloadRow {
    let median_ms = stats.median;
    let us_per_row = median_ms * 1000.0 / f64_from_u64(rows_per_stmt);
    WorkloadRow {
        name: name.to_owned(),
        metric_unit: "ms/stmt".to_owned(),
        value: median_ms,
        us_per_row,
        cost_pct: baseline.map(|(base_ms, _)| ms_cost_pct(base_ms, median_ms)),
        cost_us_per_row: baseline.map(|(_, base_us)| us_per_row - base_us),
        rows_written: total_rows,
        rows_captured: if captured { total_rows } else { 0 },
        stats: Some(stats),
    }
}

pub(super) fn export_row(
    name: &str,
    rows_exported: u64,
    rate_stats: SampleStats,
    baseline_rate: Option<f64>,
) -> ExportWorkloadRow {
    let events_per_sec = rate_stats.median;
    let elapsed_ms = (f64_from_u64(rows_exported) / events_per_sec) * 1000.0;
    ExportWorkloadRow {
        name: name.to_owned(),
        rows_exported,
        events_per_sec,
        elapsed_ms,
        overhead_pct: baseline_rate.map(|base| tps_cost_pct(base, events_per_sec)),
        stats: rate_stats,
    }
}

pub(super) fn history_row(
    name: &str,
    queries_per_sample: u32,
    p50_stats: SampleStats,
    p99_stats: SampleStats,
) -> HistoryWorkloadRow {
    HistoryWorkloadRow {
        name: name.to_owned(),
        queries_per_sample,
        p50_ms: p50_stats.median,
        p99_ms: p99_stats.median,
        p50_stats,
        p99_stats,
    }
}

pub(super) fn render_markdown(
    report: &BenchReport,
    comparison: Option<&ComparisonReport>,
) -> String {
    let mut out = String::new();
    render_header(&mut out, report);
    render_core_table(&mut out, &report.workloads);
    render_index_table(&mut out, &report.index_variants);
    if !report.export_workloads.is_empty() {
        render_export_table(&mut out, &report.export_workloads);
    }
    if !report.history_workloads.is_empty() {
        render_history_table(&mut out, &report.history_workloads);
    }
    if let Some(cmp) = comparison {
        out.push('\n');
        out.push_str(&render_comparison_markdown(cmp));
    }
    out
}

fn render_header(out: &mut String, report: &BenchReport) {
    let _ = writeln!(out, "# Auditstream Benchmark Report\n");
    let _ = writeln!(out, "- **PostgreSQL:** `{}`", report.postgres_version);
    let _ = writeln!(out, "- **Commit:** `{}`", report.git_commit);
    if let Some(env_meta) = &report.environment {
        let mem = env_meta
            .memory_mib
            .map_or_else(|| "unknown RAM".to_owned(), |m| format!("{m} MiB RAM"));
        let _ = writeln!(
            out,
            "- **Environment:** `{}/{}` · `{}` ({} cores, {})",
            env_meta.os, env_meta.arch, env_meta.cpu_model, env_meta.cpu_cores, mem
        );
    }
    let _ = writeln!(out, "- **pgbench:** `{}`", report.pgbench_settings);
    let _ = writeln!(
        out,
        "- **Sampling:** `{} sample(s)` (median reported), warmup `{}s`, bulk iterations `{}`",
        report.samples, report.warmup_secs, report.bulk_iterations
    );
    if report.export_rows > 0 || report.history_runs > 0 {
        let _ = writeln!(
            out,
            "- **Export / History:** `{}` seeded rows, `{}` history lookups/sample",
            report.export_rows, report.history_runs
        );
    }
    out.push('\n');
}

fn format_spread(stats: Option<&SampleStats>) -> String {
    match stats {
        Some(s) if s.samples.len() > 1 => format!("{:.1}–{:.1}", s.min, s.max),
        _ => "—".to_owned(),
    }
}

fn render_core_table(out: &mut String, workloads: &[WorkloadRow]) {
    let _ = writeln!(out, "## Core Capture Workloads (Default D2 Schema)\n");
    let _ = writeln!(
        out,
        "| Workload | Median | Min–Max | µs / row | Overhead (%) | Extra µs / row | Rows Written | Rows Captured |"
    );
    let _ = writeln!(
        out,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for row in workloads {
        let metric = format!("{:.1} {}", row.value, row.metric_unit);
        let spread = format_spread(row.stats.as_ref());
        let pct = row
            .cost_pct
            .map_or_else(|| "baseline".to_owned(), |v| format!("{v:+.1}%"));
        let delta_us = row
            .cost_us_per_row
            .map_or_else(|| "—".to_owned(), |v| format!("{v:+.1} µs"));
        let _ = writeln!(
            out,
            "| {} | {} | {} | {:.1} | {} | {} | {} | {} |",
            row.name,
            metric,
            spread,
            row.us_per_row,
            pct,
            delta_us,
            row.rows_written,
            row.rows_captured
        );
    }
}

fn render_index_table(out: &mut String, variants: &[IndexVariantRow]) {
    let _ = writeln!(out, "\n## Lookup Index Variants\n");
    let _ = writeln!(
        out,
        "| Variant | UPDATE comment (tps / cost) | UPDATE `set_config` (tps / cost) | 10k INSERT (ms / cost) |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: |");
    for row in variants {
        let _ = writeln!(
            out,
            "| `{}` — {} | {:.1} tps ({:+.1}%, {:+.1} µs/row) | {:.1} tps ({:+.1}%, {:+.1} µs/row) | {:.1} ms ({:+.1}%, {:+.1} µs/row) |",
            row.variant,
            row.label,
            row.update_comment_tps,
            row.update_comment_cost_pct,
            row.update_comment_cost_us,
            row.update_setcfg_tps,
            row.update_setcfg_cost_pct,
            row.update_setcfg_cost_us,
            row.bulk_insert_10k_ms,
            row.bulk_insert_10k_cost_pct,
            row.bulk_insert_10k_cost_us,
        );
    }
}

fn render_export_table(out: &mut String, exports: &[ExportWorkloadRow]) {
    let _ = writeln!(out, "\n## CLI Export Throughput (`auditstream export`)\n");
    let _ = writeln!(
        out,
        "| Workload | Median (events/s) | Min–Max (events/s) | Elapsed (ms) | Overhead (%) | Rows Exported |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: | ---: |");
    for row in exports {
        let pct = row
            .overhead_pct
            .map_or_else(|| "baseline".to_owned(), |v| format!("{v:+.1}%"));
        let _ = writeln!(
            out,
            "| {} | {:.1} | {} | {:.1} | {} | {} |",
            row.name,
            row.events_per_sec,
            format_spread(Some(&row.stats)),
            row.elapsed_ms,
            pct,
            row.rows_exported
        );
    }
}

fn render_history_table(out: &mut String, histories: &[HistoryWorkloadRow]) {
    let _ = writeln!(
        out,
        "\n## CLI History Lookup Latency (`auditstream history`)\n"
    );
    let _ = writeln!(
        out,
        "| Workload | p50 (ms) | p50 Min–Max (ms) | p99 (ms) | p99 Min–Max (ms) | Queries / Sample |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: | ---: |");
    for row in histories {
        let _ = writeln!(
            out,
            "| {} | {:.2} | {} | {:.2} | {} | {} |",
            row.name,
            row.p50_ms,
            format_spread(Some(&row.p50_stats)),
            row.p99_ms,
            format_spread(Some(&row.p99_stats)),
            row.queries_per_sample
        );
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::bench::compare::compare_reports;

    fn triple(mid: f64) -> SampleStats {
        SampleStats::from_samples(vec![mid, mid * 1.02, mid * 0.98]).expect("triple stats")
    }

    pub(crate) fn sample_report(scale: f64) -> BenchReport {
        let up_base = triple(10_000.0 * scale);
        let up_comment = triple(8_000.0 * scale);
        let bulk_base = triple(20.0 / scale);
        let bulk_cap = triple(50.0 / scale);
        let export_plain = triple(25_000.0 * scale);
        let export_redacted = triple(24_000.0 * scale);
        let hist_p50 = triple(8.0 / scale);
        let hist_p99 = triple(12.0 / scale);

        let base_tps = up_base.median;
        let base_micros = 1_000_000.0 / base_tps;
        let bulk_base_millis = bulk_base.median;
        let bulk_base_micros = bulk_base_millis * 1000.0 / 10_000.0;

        BenchReport {
            schema_version: REPORT_SCHEMA_VERSION,
            postgres_version: "PostgreSQL 18.6".to_owned(),
            git_commit: "abc1234".to_owned(),
            environment: Some(EnvironmentMetadata {
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                cpu_model: "Test CPU".to_owned(),
                cpu_cores: 8,
                memory_mib: Some(16_384),
            }),
            pgbench_settings: "-M prepared -c 4 -j 2 -T 5".to_owned(),
            samples: 3,
            warmup_secs: 2,
            bulk_iterations: 3,
            export_rows: 10_000,
            history_runs: 25,
            workloads: vec![
                tps_row("Single-row UPDATE (baseline)", up_base, 30_000, None, false),
                tps_row(
                    "Single-row UPDATE (comment)",
                    up_comment.clone(),
                    24_000,
                    Some((base_tps, base_micros)),
                    true,
                ),
                bulk_row(
                    "10,000-row INSERT (baseline)",
                    bulk_base,
                    10_000,
                    30_000,
                    None,
                    false,
                ),
                bulk_row(
                    "10,000-row UPDATE (comment)",
                    bulk_cap.clone(),
                    10_000,
                    30_000,
                    Some((bulk_base_millis, bulk_base_micros)),
                    true,
                ),
            ],
            index_variants: vec![IndexVariantRow {
                variant: "record_id_hash".to_owned(),
                label: "Default partial hash".to_owned(),
                update_comment_tps: up_comment.median,
                update_comment_cost_pct: tps_cost_pct(base_tps, up_comment.median),
                update_comment_cost_us: (1_000_000.0 / up_comment.median) - base_micros,
                update_comment_stats: Some(up_comment.clone()),
                update_setcfg_tps: up_comment.median * 0.9,
                update_setcfg_cost_pct: tps_cost_pct(base_tps, up_comment.median * 0.9),
                update_setcfg_cost_us: (1_000_000.0 / (up_comment.median * 0.9)) - base_micros,
                update_setcfg_stats: Some(up_comment),
                bulk_insert_10k_ms: bulk_cap.median,
                bulk_insert_10k_cost_pct: ms_cost_pct(bulk_base_millis, bulk_cap.median),
                bulk_insert_10k_cost_us: (bulk_cap.median * 1000.0 / 10_000.0) - bulk_base_micros,
                bulk_insert_10k_stats: Some(bulk_cap),
            }],
            export_workloads: vec![
                export_row(
                    "auditstream export (plain JSONL)",
                    10_000,
                    export_plain.clone(),
                    None,
                ),
                export_row(
                    "auditstream export (redacted JSONL)",
                    10_000,
                    export_redacted,
                    Some(export_plain.median),
                ),
            ],
            history_workloads: vec![history_row(
                "auditstream history (--table --record-id)",
                25,
                hist_p50,
                hist_p99,
            )],
        }
    }

    #[test]
    fn report_json_round_trips_computes_exact_metrics_and_renders_all_sections() {
        let base = sample_report(1.0);
        let cand = sample_report(1.0);
        let json = serde_json::to_string_pretty(&base).expect("serialize");
        let parsed: BenchReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, base);

        // Verify exact row formulas (not just serde round-trip equality).
        let up_cap = &base.workloads[1];
        assert!((up_cap.us_per_row - 125.0).abs() < 1e-9); // 1e6 / 8000
        assert!((up_cap.cost_pct.expect("cost_pct") - 20.0).abs() < 1e-9); // (10k - 8k) / 10k
        assert!((up_cap.cost_us_per_row.expect("cost_us") - 25.0).abs() < 1e-9); // 125 - 100

        let bulk_cap = &base.workloads[3];
        assert!((bulk_cap.us_per_row - 5.0).abs() < 1e-9); // 50ms * 1000 / 10_000
        assert!((bulk_cap.cost_pct.expect("bulk cost_pct") - 150.0).abs() < 1e-9); // (50 - 20) / 20
        assert!((bulk_cap.cost_us_per_row.expect("bulk cost_us") - 3.0).abs() < 1e-9); // 5 - 2

        let redacted_exp = &base.export_workloads[1];
        assert!((redacted_exp.elapsed_ms - (10_000.0 / 24_000.0 * 1000.0)).abs() < 1e-6);
        assert!((redacted_exp.overhead_pct.expect("overhead") - 4.0).abs() < 1e-9); // (25k - 24k) / 25k

        let cmp = compare_reports(&base, &cand, "v0.1.0.json", 15.0).expect("compare");
        let md = render_markdown(&cand, Some(&cmp));
        assert!(md.contains("## Core Capture Workloads"));
        assert!(md.contains("## Lookup Index Variants"));
        assert!(md.contains("## CLI Export Throughput"));
        assert!(md.contains("## CLI History Lookup Latency"));
        assert!(md.contains("## Baseline Regression Comparison"));
        assert!(md.contains("Test CPU"));
        assert!(md.contains("16384 MiB RAM"));

        // Verify single-sample spread ("—") and unknown memory rendering branches.
        let mut single = sample_report(1.0);
        single.environment.as_mut().expect("env").memory_mib = None;
        single.workloads[0].stats =
            Some(SampleStats::from_samples(vec![10_000.0]).expect("single"));
        let single_md = render_markdown(&single, None);
        assert!(single_md.contains("unknown RAM"));
        assert!(single_md.contains("| Single-row UPDATE (baseline) | 10000.0 tps | — |"));
    }

    #[test]
    fn legacy_v1_report_without_new_fields_deserializes_cleanly() {
        let legacy = r#"{
            "postgres_version": "PostgreSQL 19.0",
            "git_commit": "0000000",
            "pgbench_settings": "-M prepared -c 2 -j 1 -T 1",
            "bulk_iterations": 1,
            "workloads": [{
                "name": "Single-row UPDATE (comment)",
                "metric_unit": "tps",
                "value": 5000.0,
                "us_per_row": 200.0,
                "cost_pct": 10.0,
                "cost_us_per_row": 20.0,
                "rows_written": 5000,
                "rows_captured": 5000
            }],
            "index_variants": []
        }"#;
        let parsed: BenchReport = serde_json::from_str(legacy).expect("parse legacy v1");
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.samples, 1);
        assert!(parsed.environment.is_none());
        assert_eq!(parsed.export_workloads.len(), 0);
        assert_eq!(parsed.history_workloads.len(), 0);
    }
}
