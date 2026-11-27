//! Baseline regression comparison engine and Markdown diff renderer for `xtask bench --compare`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::report::BenchReport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MetricDirection {
    HigherIsBetter,
    LowerIsBetter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompareStatus {
    Improved,
    Pass,
    Regression,
    New,
}

impl CompareStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Improved => "IMPROVED",
            Self::Pass => "PASS",
            Self::Regression => "REGRESSION",
            Self::New => "NEW",
        }
    }
}

/// One metric row in the baseline vs. candidate comparison diff.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ComparisonRow {
    pub(super) metric: String,
    pub(super) unit: String,
    pub(super) baseline: Option<f64>,
    pub(super) candidate: f64,
    pub(super) delta_pct: Option<f64>,
    pub(super) regression_pct: Option<f64>,
    pub(super) status: CompareStatus,
}

/// Complete comparison outcome against a committed baseline report.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ComparisonReport {
    pub(super) baseline_label: String,
    pub(super) baseline_commit: String,
    pub(super) max_regression_pct: f64,
    pub(super) warnings: Vec<String>,
    pub(super) rows: Vec<ComparisonRow>,
    pub(super) regressions: Vec<String>,
}

impl ComparisonReport {
    pub(super) fn has_regressions(&self) -> bool {
        !self.regressions.is_empty()
    }
}

/// Compares `candidate` against `baseline`, flagging any tracked metric that regresses
/// strictly beyond `max_regression_pct`.
pub(super) fn compare_reports(
    baseline: &BenchReport,
    candidate: &BenchReport,
    baseline_label: &str,
    max_regression_pct: f64,
) -> Result<ComparisonReport, String> {
    if !max_regression_pct.is_finite() || max_regression_pct < 0.0 {
        return Err(format!(
            "max_regression_pct must be finite and >= 0, got {max_regression_pct}"
        ));
    }

    let warnings = collect_drift_warnings(baseline, candidate);
    let mut rows = Vec::new();
    let mut regressions = Vec::new();

    compare_workloads(
        baseline,
        candidate,
        max_regression_pct,
        &mut rows,
        &mut regressions,
    )?;
    compare_exports(
        baseline,
        candidate,
        max_regression_pct,
        &mut rows,
        &mut regressions,
    )?;
    compare_histories(
        baseline,
        candidate,
        max_regression_pct,
        &mut rows,
        &mut regressions,
    )?;
    compare_index_variants(
        baseline,
        candidate,
        max_regression_pct,
        &mut rows,
        &mut regressions,
    )?;

    Ok(ComparisonReport {
        baseline_label: baseline_label.to_owned(),
        baseline_commit: baseline.git_commit.clone(),
        max_regression_pct,
        warnings,
        rows,
        regressions,
    })
}

fn collect_drift_warnings(baseline: &BenchReport, candidate: &BenchReport) -> Vec<String> {
    let mut warnings = Vec::new();
    if baseline.pgbench_settings != candidate.pgbench_settings {
        warnings.push(format!(
            "pgbench settings differ (baseline `{}` vs. candidate `{}`)",
            baseline.pgbench_settings, candidate.pgbench_settings
        ));
    }
    if baseline.export_rows != candidate.export_rows {
        warnings.push(format!(
            "export_rows differ (baseline `{}` vs. candidate `{}`)",
            baseline.export_rows, candidate.export_rows
        ));
    }
    if baseline.postgres_version != candidate.postgres_version {
        warnings.push(format!(
            "PostgreSQL version differs (baseline `{}` vs. candidate `{}`)",
            baseline.postgres_version, candidate.postgres_version
        ));
    }
    if baseline.environment != candidate.environment {
        warnings
            .push("host hardware/OS metadata differs between baseline and candidate".to_owned());
    }
    warnings
}

fn compare_workloads(
    baseline: &BenchReport,
    candidate: &BenchReport,
    max_regression_pct: f64,
    rows: &mut Vec<ComparisonRow>,
    regressions: &mut Vec<String>,
) -> Result<(), String> {
    let base_map: BTreeMap<&str, (&str, f64)> = baseline
        .workloads
        .iter()
        .map(|w| (w.name.as_str(), (w.metric_unit.as_str(), w.value)))
        .collect();

    for cand in &candidate.workloads {
        let base_val = match base_map.get(cand.name.as_str()).copied() {
            Some((base_unit, _)) if base_unit != cand.metric_unit => {
                return Err(format!(
                    "workload `{}` unit mismatch: baseline `{base_unit}` vs. candidate `{}`",
                    cand.name, cand.metric_unit
                ));
            }
            Some((_, val)) => Some(val),
            None => None,
        };
        let direction = if cand.metric_unit == "tps" {
            MetricDirection::HigherIsBetter
        } else {
            MetricDirection::LowerIsBetter
        };
        let row = evaluate_metric(
            &cand.name,
            &cand.metric_unit,
            base_val,
            cand.value,
            direction,
            max_regression_pct,
        )?;
        record_row(row, rows, regressions);
    }
    Ok(())
}

fn compare_exports(
    baseline: &BenchReport,
    candidate: &BenchReport,
    max_regression_pct: f64,
    rows: &mut Vec<ComparisonRow>,
    regressions: &mut Vec<String>,
) -> Result<(), String> {
    let base_map: BTreeMap<&str, f64> = baseline
        .export_workloads
        .iter()
        .map(|w| (w.name.as_str(), w.events_per_sec))
        .collect();

    for cand in &candidate.export_workloads {
        let row = evaluate_metric(
            &cand.name,
            "events/s",
            base_map.get(cand.name.as_str()).copied(),
            cand.events_per_sec,
            MetricDirection::HigherIsBetter,
            max_regression_pct,
        )?;
        record_row(row, rows, regressions);
    }
    Ok(())
}

fn compare_histories(
    baseline: &BenchReport,
    candidate: &BenchReport,
    max_regression_pct: f64,
    rows: &mut Vec<ComparisonRow>,
    regressions: &mut Vec<String>,
) -> Result<(), String> {
    let base_map: BTreeMap<&str, (f64, f64)> = baseline
        .history_workloads
        .iter()
        .map(|w| (w.name.as_str(), (w.p50_ms, w.p99_ms)))
        .collect();

    for cand in &candidate.history_workloads {
        let base_pair = base_map.get(cand.name.as_str()).copied();
        let p50_row = evaluate_metric(
            &format!("{} (p50)", cand.name),
            "ms",
            base_pair.map(|(p50, _)| p50),
            cand.p50_ms,
            MetricDirection::LowerIsBetter,
            max_regression_pct,
        )?;
        record_row(p50_row, rows, regressions);

        let p99_row = evaluate_metric(
            &format!("{} (p99)", cand.name),
            "ms",
            base_pair.map(|(_, p99)| p99),
            cand.p99_ms,
            MetricDirection::LowerIsBetter,
            max_regression_pct,
        )?;
        record_row(p99_row, rows, regressions);
    }
    Ok(())
}

fn compare_index_variants(
    baseline: &BenchReport,
    candidate: &BenchReport,
    max_regression_pct: f64,
    rows: &mut Vec<ComparisonRow>,
    regressions: &mut Vec<String>,
) -> Result<(), String> {
    let base_map: BTreeMap<&str, (f64, f64, f64)> = baseline
        .index_variants
        .iter()
        .map(|v| {
            (
                v.variant.as_str(),
                (
                    v.update_comment_tps,
                    v.update_setcfg_tps,
                    v.bulk_insert_10k_ms,
                ),
            )
        })
        .collect();

    for cand in &candidate.index_variants {
        let base_vals = base_map.get(cand.variant.as_str()).copied();
        let comment_row = evaluate_metric(
            &format!("index `{}` UPDATE comment", cand.variant),
            "tps",
            base_vals.map(|(c, _, _)| c),
            cand.update_comment_tps,
            MetricDirection::HigherIsBetter,
            max_regression_pct,
        )?;
        record_row(comment_row, rows, regressions);

        let setcfg_row = evaluate_metric(
            &format!("index `{}` UPDATE set_config", cand.variant),
            "tps",
            base_vals.map(|(_, s, _)| s),
            cand.update_setcfg_tps,
            MetricDirection::HigherIsBetter,
            max_regression_pct,
        )?;
        record_row(setcfg_row, rows, regressions);

        let bulk_row = evaluate_metric(
            &format!("index `{}` 10k INSERT", cand.variant),
            "ms/stmt",
            base_vals.map(|(_, _, b)| b),
            cand.bulk_insert_10k_ms,
            MetricDirection::LowerIsBetter,
            max_regression_pct,
        )?;
        record_row(bulk_row, rows, regressions);
    }
    Ok(())
}

fn evaluate_metric(
    metric: &str,
    unit: &str,
    baseline: Option<f64>,
    candidate: f64,
    direction: MetricDirection,
    max_regression_pct: f64,
) -> Result<ComparisonRow, String> {
    if !candidate.is_finite() || candidate <= 0.0 {
        return Err(format!(
            "candidate metric `{metric}` must be finite and > 0, got {candidate}"
        ));
    }

    let Some(base) = baseline else {
        return Ok(ComparisonRow {
            metric: metric.to_owned(),
            unit: unit.to_owned(),
            baseline: None,
            candidate,
            delta_pct: None,
            regression_pct: None,
            status: CompareStatus::New,
        });
    };

    if !base.is_finite() || base <= 0.0 {
        return Err(format!(
            "baseline metric `{metric}` must be finite and > 0, got {base}"
        ));
    }

    let delta_pct = ((candidate - base) / base) * 100.0;
    let regression_pct = match direction {
        MetricDirection::HigherIsBetter => -delta_pct,
        MetricDirection::LowerIsBetter => delta_pct,
    };

    let status = if regression_pct > max_regression_pct {
        CompareStatus::Regression
    } else if regression_pct < -0.5 {
        CompareStatus::Improved
    } else {
        CompareStatus::Pass
    };

    Ok(ComparisonRow {
        metric: metric.to_owned(),
        unit: unit.to_owned(),
        baseline: Some(base),
        candidate,
        delta_pct: Some(delta_pct),
        regression_pct: Some(regression_pct),
        status,
    })
}

fn record_row(row: ComparisonRow, rows: &mut Vec<ComparisonRow>, regressions: &mut Vec<String>) {
    if row.status == CompareStatus::Regression {
        let reg = row.regression_pct.unwrap_or(0.0);
        let base = row.baseline.unwrap_or(0.0);
        regressions.push(format!(
            "{} ({:.2} -> {:.2} {}, regressed by {:.1}%)",
            row.metric, base, row.candidate, row.unit, reg
        ));
    }
    rows.push(row);
}

pub(super) fn render_comparison_markdown(cmp: &ComparisonReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## Baseline Regression Comparison\n");
    let _ = writeln!(
        out,
        "- **Baseline:** `{}` (commit `{}`)",
        cmp.baseline_label, cmp.baseline_commit
    );
    let _ = writeln!(
        out,
        "- **Threshold:** max regression `{:.1}%` · **Result:** `{}`\n",
        cmp.max_regression_pct,
        if cmp.has_regressions() {
            format!("FAIL ({} regression(s))", cmp.regressions.len())
        } else {
            "PASS".to_owned()
        }
    );
    for warning in &cmp.warnings {
        let _ = writeln!(out, "> **Note:** {warning}");
    }
    if !cmp.warnings.is_empty() {
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "| Metric | Unit | Baseline | Candidate | Delta (%) | Status |"
    );
    let _ = writeln!(out, "| --- | --- | ---: | ---: | ---: | --- |");
    for row in &cmp.rows {
        let base = row
            .baseline
            .map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"));
        let delta = row
            .delta_pct
            .map_or_else(|| "—".to_owned(), |v| format!("{v:+.1}%"));
        let _ = writeln!(
            out,
            "| {} | {} | {} | {:.2} | {} | {} |",
            row.metric,
            row.unit,
            base,
            row.candidate,
            delta,
            row.status.label()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::report::tests::sample_report;

    #[test]
    fn identical_reports_pass_with_zero_regressions_and_no_drift_warnings() {
        let base = sample_report(1.0);
        let cand = sample_report(1.0);
        let cmp = compare_reports(&base, &cand, "v0.1.0.json", 15.0).expect("compare");
        assert!(!cmp.has_regressions());
        assert_eq!(cmp.warnings.len(), 0);
        assert_eq!(cmp.rows.len(), 11);
        assert!(cmp.rows.iter().all(|r| r.status == CompareStatus::Pass));
    }

    #[test]
    fn threshold_zone_exact_boundary_and_improvement_noise_band_behave_strictly() {
        // HigherIsBetter: -10% passes, -15.0% exact boundary passes, -15.1% regresses.
        let pass_within = evaluate_metric(
            "tps",
            "tps",
            Some(1000.0),
            900.0,
            MetricDirection::HigherIsBetter,
            15.0,
        )
        .expect("within");
        assert_eq!(pass_within.status, CompareStatus::Pass);

        let pass_exact = evaluate_metric(
            "tps",
            "tps",
            Some(1000.0),
            850.0,
            MetricDirection::HigherIsBetter,
            15.0,
        )
        .expect("exact boundary");
        assert_eq!(pass_exact.status, CompareStatus::Pass);

        let fail_over = evaluate_metric(
            "tps",
            "tps",
            Some(1000.0),
            849.0,
            MetricDirection::HigherIsBetter,
            15.0,
        )
        .expect("just over");
        assert_eq!(fail_over.status, CompareStatus::Regression);

        // LowerIsBetter: +10% passes, +15.0% exact boundary passes, +15.1% regresses.
        let ms_within = evaluate_metric(
            "ms",
            "ms/stmt",
            Some(100.0),
            110.0,
            MetricDirection::LowerIsBetter,
            15.0,
        )
        .expect("ms within");
        assert_eq!(ms_within.status, CompareStatus::Pass);

        let ms_exact = evaluate_metric(
            "ms",
            "ms/stmt",
            Some(100.0),
            115.0,
            MetricDirection::LowerIsBetter,
            15.0,
        )
        .expect("ms exact");
        assert_eq!(ms_exact.status, CompareStatus::Pass);

        let ms_over = evaluate_metric(
            "ms",
            "ms/stmt",
            Some(100.0),
            115.1,
            MetricDirection::LowerIsBetter,
            15.0,
        )
        .expect("ms over");
        assert_eq!(ms_over.status, CompareStatus::Regression);

        // Improvement noise band: -0.3% stays Pass, -1.0% becomes Improved.
        let noise = evaluate_metric(
            "ms",
            "ms/stmt",
            Some(100.0),
            99.7,
            MetricDirection::LowerIsBetter,
            15.0,
        )
        .expect("noise band");
        assert_eq!(noise.status, CompareStatus::Pass);

        let improved = evaluate_metric(
            "ms",
            "ms/stmt",
            Some(100.0),
            99.0,
            MetricDirection::LowerIsBetter,
            15.0,
        )
        .expect("improved");
        assert_eq!(improved.status, CompareStatus::Improved);
    }

    #[test]
    fn faster_candidate_is_marked_improved() {
        let base = sample_report(1.0);
        let cand = sample_report(1.25);
        let cmp = compare_reports(&base, &cand, "v0.1.0.json", 15.0).expect("compare");
        assert!(!cmp.has_regressions());
        assert_eq!(cmp.rows.len(), 11);
        assert!(cmp.rows.iter().all(|r| r.status == CompareStatus::Improved));
    }

    #[test]
    fn slower_candidate_beyond_threshold_flags_all_regressed_directions_and_drift_warnings() {
        let base = sample_report(1.0);
        let mut cand = sample_report(0.75); // -25% TPS / events/s, +33.3% latency
        cand.pgbench_settings = "-M simple -c 2 -j 1 -T 1".to_owned();
        cand.export_rows = 2_000;
        cand.postgres_version = "PostgreSQL 19.0".to_owned();
        cand.environment = None;

        let cmp = compare_reports(&base, &cand, "v0.1.0.json", 15.0).expect("compare");
        assert!(cmp.has_regressions());
        assert_eq!(cmp.rows.len(), 11);
        assert_eq!(cmp.regressions.len(), cmp.rows.len());
        assert_eq!(cmp.warnings.len(), 4);

        let md = render_comparison_markdown(&cmp);
        assert!(md.contains("FAIL ("));
        assert!(md.contains("REGRESSION"));
        assert!(md.contains("> **Note:** pgbench settings differ"));
        assert!(md.contains("> **Note:** export_rows differ"));
        assert!(md.contains("> **Note:** PostgreSQL version differs"));
        assert!(md.contains("> **Note:** host hardware/OS metadata differs"));
        assert!(md.contains("Single-row UPDATE (comment)"));
        assert!(md.contains("10,000-row UPDATE (comment)"));
        assert!(md.contains("auditstream export (plain JSONL)"));
        assert!(md.contains("auditstream history (--table --record-id) (p99)"));
        assert!(md.contains("index `record_id_hash` UPDATE comment"));
        assert!(md.contains("index `record_id_hash` 10k INSERT"));
    }

    #[test]
    fn missing_baseline_metrics_are_marked_new_without_failing() {
        let mut base = sample_report(1.0);
        base.export_workloads.clear();
        base.history_workloads.clear();
        let cand = sample_report(1.0);

        let cmp = compare_reports(&base, &cand, "legacy.json", 15.0).expect("compare");
        assert!(!cmp.has_regressions());
        let new_count = cmp
            .rows
            .iter()
            .filter(|r| r.status == CompareStatus::New)
            .count();
        assert_eq!(new_count, 4); // 2 export + 2 history (p50, p99)
    }

    #[test]
    fn rejects_invalid_threshold_unit_mismatch_and_non_positive_metrics() {
        let mut base = sample_report(1.0);
        let cand = sample_report(1.0);
        assert!(compare_reports(&base, &cand, "b.json", -1.0).is_err());
        assert!(compare_reports(&base, &cand, "b.json", f64::NAN).is_err());

        let mut unit_mismatch = sample_report(1.0);
        unit_mismatch.workloads[0].metric_unit = "ms/stmt".to_owned();
        let err = compare_reports(&base, &unit_mismatch, "b.json", 15.0)
            .expect_err("unit mismatch must fail");
        assert!(err.contains("unit mismatch"), "{err}");

        base.workloads[0].value = 0.0;
        assert!(compare_reports(&base, &cand, "b.json", 15.0).is_err());

        let base_ok = sample_report(1.0);
        let mut cand_bad = sample_report(1.0);
        cand_bad.workloads[0].value = -5.0;
        assert!(compare_reports(&base_ok, &cand_bad, "b.json", 15.0).is_err());
    }

    #[test]
    fn committed_v0_1_0_baseline_parses_and_self_compares_cleanly() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bench/baselines/v0.1.0.json");
        let raw = std::fs::read_to_string(&path).expect("read committed baseline");
        let report: BenchReport = serde_json::from_str(&raw).expect("parse committed baseline");
        assert_eq!(report.schema_version, 2);
        assert!(report.environment.is_some());
        assert!(report.samples >= 3);
        assert_eq!(report.workloads.len(), 9);
        assert!(
            report
                .workloads
                .iter()
                .any(|w| w.name == "10,000-row UPDATE (comment)")
        );
        assert_eq!(report.index_variants.len(), 4);
        assert_eq!(report.export_workloads.len(), 2);
        assert_eq!(report.history_workloads.len(), 1);

        let cmp = compare_reports(&report, &report, "bench/baselines/v0.1.0.json", 15.0)
            .expect("self compare");
        assert!(!cmp.has_regressions());
        assert_eq!(cmp.rows.len(), 25);
        assert!(cmp.rows.iter().all(|r| r.status == CompareStatus::Pass));
    }
}
