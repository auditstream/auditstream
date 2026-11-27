//! Statistical helpers, output parsers, and environment metadata collection for `xtask bench`.

use std::env;
use std::fs;
use std::thread;

use serde::{Deserialize, Serialize};

/// Summary statistics computed across one or more timed benchmark samples.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct SampleStats {
    pub(super) median: f64,
    pub(super) min: f64,
    pub(super) max: f64,
    pub(super) spread: f64,
    pub(super) samples: Vec<f64>,
}

impl SampleStats {
    /// Computes median, min, max, and spread (`max - min`) over `samples`.
    pub(super) fn from_samples(samples: Vec<f64>) -> Result<Self, String> {
        if samples.is_empty() {
            return Err("cannot compute statistics over 0 samples".to_owned());
        }
        if samples.iter().any(|v| !v.is_finite()) {
            return Err("benchmark samples contain non-finite value".to_owned());
        }

        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        let min = sorted[0];
        let max = sorted[sorted.len() - 1];
        let median = percentile_sorted(&sorted, 50.0);

        Ok(Self {
            median,
            min,
            max,
            spread: max - min,
            samples,
        })
    }
}

/// Computes an interpolated percentile (`0.0..=100.0`) over `values`.
pub(super) fn percentile(values: &[f64], pct: f64) -> Result<f64, String> {
    if values.is_empty() {
        return Err("cannot compute percentile over 0 values".to_owned());
    }
    if !(0.0..=100.0).contains(&pct) || !pct.is_finite() {
        return Err(format!("percentile must be in 0..=100, got {pct}"));
    }
    if values.iter().any(|v| !v.is_finite()) {
        return Err("latency samples contain non-finite value".to_owned());
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Ok(percentile_sorted(&sorted, pct))
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn percentile_sorted(sorted: &[f64], pct: f64) -> f64 {
    let last = sorted.len() - 1;
    if last == 0 {
        return sorted[0];
    }
    let pos = (pct / 100.0) * (last as f64);
    let lo = (pos.floor() as usize).min(last);
    let hi = (pos.ceil() as usize).min(last);
    let frac = pos - (lo as f64);
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Host hardware and OS metadata recorded alongside benchmark results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct EnvironmentMetadata {
    pub(super) os: String,
    pub(super) arch: String,
    pub(super) cpu_model: String,
    pub(super) cpu_cores: usize,
    pub(super) memory_mib: Option<u64>,
}

pub(super) fn collect_environment() -> EnvironmentMetadata {
    let cpu_model = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| parse_proc_cpuinfo_model(&s))
        .unwrap_or_else(|| "unknown".to_owned());
    let memory_mib = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_proc_meminfo_mib(&s));
    let cpu_cores = thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);

    EnvironmentMetadata {
        os: env::consts::OS.to_owned(),
        arch: env::consts::ARCH.to_owned(),
        cpu_model,
        cpu_cores,
        memory_mib,
    }
}

pub(super) fn parse_proc_cpuinfo_model(content: &str) -> Option<String> {
    for line in content.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let trimmed_key = key.trim();
            if trimmed_key == "model name" || trimmed_key == "Hardware" {
                let model = value.trim();
                if !model.is_empty() {
                    return Some(model.to_owned());
                }
            }
        }
    }
    None
}

pub(super) fn parse_proc_meminfo_mib(content: &str) -> Option<u64> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib / 1024);
        }
    }
    None
}

/// Parsed transaction count, failure count, and TPS from a `pgbench` invocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PgbenchResult {
    pub(super) tx: u64,
    pub(super) failed: u64,
    pub(super) tps: f64,
}

pub(super) fn parse_pgbench_output(text: &str) -> Result<PgbenchResult, String> {
    let mut tx: Option<u64> = None;
    let mut failed: u64 = 0;
    let mut tps: Option<f64> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("number of transactions actually processed:") {
            let token = rest.split_whitespace().next().unwrap_or("");
            tx = token.parse::<u64>().ok();
        } else if let Some(rest) = trimmed.strip_prefix("number of failed transactions:") {
            let token = rest.split_whitespace().next().unwrap_or("0");
            failed = token.parse::<u64>().unwrap_or(0);
        } else if let Some(rest) = trimmed.strip_prefix("tps =") {
            let token = rest.split_whitespace().next().unwrap_or("");
            if let Ok(parsed_tps) = token.parse::<f64>() {
                tps = Some(parsed_tps);
            }
        }
    }

    Ok(PgbenchResult {
        tx: tx.ok_or_else(|| {
            "could not parse `number of transactions actually processed`".to_owned()
        })?,
        failed,
        tps: tps.ok_or_else(|| "could not parse `tps =` from pgbench output".to_owned())?,
    })
}

#[derive(Debug, Deserialize)]
struct ExportTelemetrySummary {
    events: u64,
}

/// Extracts the `events` count from the structured JSON summary emitted by
/// `auditstream export --stats-format json` on stderr (ignoring any leading log/warning lines).
pub(super) fn parse_export_stats_events(stderr: &str) -> Result<u64, String> {
    for line in stderr.lines().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with('{')
            && let Ok(summary) = serde_json::from_str::<ExportTelemetrySummary>(trimmed)
        {
            return Ok(summary.events);
        }
    }
    Err(format!(
        "could not parse export JSON stats from stderr: {}",
        stderr.trim()
    ))
}

pub(super) fn tps_cost_pct(baseline_tps: f64, measured_tps: f64) -> f64 {
    ((baseline_tps - measured_tps) / baseline_tps) * 100.0
}

pub(super) fn ms_cost_pct(baseline_ms: f64, measured_ms: f64) -> f64 {
    ((measured_ms - baseline_ms) / baseline_ms) * 100.0
}

#[allow(clippy::cast_precision_loss)]
pub(super) fn f64_from_u64(value: u64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_stats_computes_median_min_max_and_spread_for_skewed_odd_and_even_counts() {
        // Skewed inputs where median != mean: mean(100, 10, 20) = 43.33, median = 20.0.
        let odd = SampleStats::from_samples(vec![100.0, 10.0, 20.0]).expect("odd stats");
        assert!((odd.median - 20.0).abs() < 1e-9);
        assert!((odd.min - 10.0).abs() < 1e-9);
        assert!((odd.max - 100.0).abs() < 1e-9);
        assert!((odd.spread - 90.0).abs() < 1e-9);
        assert_eq!(odd.samples, vec![100.0, 10.0, 20.0]);

        // Skewed even input: mean(100, 10, 30, 20) = 40.0, median = 25.0.
        let even = SampleStats::from_samples(vec![100.0, 10.0, 30.0, 20.0]).expect("even stats");
        assert!((even.median - 25.0).abs() < 1e-9);
        assert!((even.spread - 90.0).abs() < 1e-9);

        let single = SampleStats::from_samples(vec![42.5]).expect("single stats");
        assert!((single.median - 42.5).abs() < 1e-9);
        assert!((single.spread - 0.0).abs() < 1e-9);
    }

    #[test]
    fn sample_stats_and_percentile_reject_empty_non_finite_and_out_of_range() {
        assert!(SampleStats::from_samples(vec![]).is_err());
        assert!(SampleStats::from_samples(vec![1.0, f64::NAN]).is_err());
        assert!(SampleStats::from_samples(vec![f64::INFINITY]).is_err());

        assert!(percentile(&[], 50.0).is_err());
        assert!(percentile(&[1.0], -1.0).is_err());
        assert!(percentile(&[1.0], 101.0).is_err());
        assert!(percentile(&[1.0], f64::NAN).is_err());
        assert!(percentile(&[1.0, f64::NAN], 99.0).is_err());
    }

    #[test]
    fn percentile_sorts_unsorted_inputs_and_cost_pct_helpers_have_exact_signs() {
        let mut vals: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        vals.swap(0, 37);
        vals.swap(12, 88);
        let p50 = percentile(&vals, 50.0).expect("p50");
        let p99 = percentile(&vals, 99.0).expect("p99");
        assert!((p50 - 50.5).abs() < 1e-9);
        assert!((p99 - 99.01).abs() < 1e-9);

        assert!((tps_cost_pct(10_000.0, 8_000.0) - 20.0).abs() < 1e-9);
        assert!((ms_cost_pct(20.0, 50.0) - 150.0).abs() < 1e-9);
    }

    #[test]
    fn proc_parsers_and_environment_collection_handle_valid_and_missing_inputs() {
        let cpuinfo =
            "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD Ryzen 9 9950X3D\n";
        assert_eq!(
            parse_proc_cpuinfo_model(cpuinfo).as_deref(),
            Some("AMD Ryzen 9 9950X3D")
        );
        assert_eq!(
            parse_proc_cpuinfo_model("Hardware : ARM Neoverse N1\n").as_deref(),
            Some("ARM Neoverse N1")
        );
        assert_eq!(parse_proc_cpuinfo_model("model name :\n"), None);

        let meminfo = "MemTotal:       32768000 kB\nMemFree:        1000000 kB\n";
        assert_eq!(parse_proc_meminfo_mib(meminfo), Some(32_000));
        assert_eq!(parse_proc_meminfo_mib("MemFree: 1024 kB\n"), None);

        let env_meta = collect_environment();
        assert_ne!(env_meta.os, "");
        assert_ne!(env_meta.arch, "");
        assert!(env_meta.cpu_cores >= 1);
    }

    #[test]
    fn parse_pgbench_output_extracts_tx_failed_and_tps() {
        let sample = "\
pgbench (18.6, server 19.0)
transaction type: bench/capture/update_comment.sql
scaling factor: 1
query mode: prepared
number of clients: 8
number of threads: 4
duration: 5 s
number of transactions actually processed: 33150
number of failed transactions: 0 (0.000%)
latency average = 1.206 ms
tps = 6630.421800 (without initial connection time)
";
        let parsed = parse_pgbench_output(sample).expect("parse sample");
        assert_eq!(parsed.tx, 33_150);
        assert_eq!(parsed.failed, 0);
        assert!((parsed.tps - 6630.4218).abs() < 0.001);

        assert!(parse_pgbench_output("tps = 100.0").is_err());
        assert!(parse_pgbench_output("number of transactions actually processed: 10").is_err());
    }

    #[test]
    fn parse_export_stats_events_ignores_leading_warnings() {
        let stderr = "\
WARN sslmode is not verify-full
{\"events\":25000,\"batches\":25,\"duration_ms\":842,\"cursor\":{\"xact_id\":10,\"id\":25000},\"snapshot_xmin\":11,\"oldest_unexported_age_secs\":null,\"stalled\":false}
";
        assert_eq!(parse_export_stats_events(stderr).expect("parse"), 25_000);
        assert!(parse_export_stats_events("not json").is_err());
    }
}
