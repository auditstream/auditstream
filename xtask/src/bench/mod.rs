//! Capture, export, and history benchmark runner (`cargo xtask bench`).
//!
//! Creates a fresh temporary database from `auditstream_pg`'s D2 + D3 generators, seeds tables
//! before triggers exist, runs warmed multi-sample (`--samples`) capture (`pgbench` + 10k bulk
//! `INSERT` / `UPDATE`), lookup-index variants, `auditstream export` (plain & redacted JSONL),
//! and `auditstream history` lookup latency workloads, emits Markdown/JSON reports with
//! median/min/max/spread statistics, and optionally compares against a committed baseline JSON.

mod compare;
mod config;
mod report;
mod stats;

use std::env;
use std::fs::{self, DirBuilder};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use compare::{ComparisonReport, compare_reports};
use config::{
    BULK_ROWS, BenchConfig, COMMENT_TAG, build_export_toml, build_seed_changes_sql,
    build_seed_tables_sql, build_setup_schema_sql, parse_args, print_bench_help,
    release_auditstream_bin, replace_db_name, split_uri_password,
};
use report::{
    BenchReport, ExportWorkloadRow, HistoryWorkloadRow, IndexVariantRow, REPORT_SCHEMA_VERSION,
    WorkloadRow, bulk_row, export_row, history_row, render_markdown, tps_row,
};
use stats::{
    PgbenchResult, SampleStats, collect_environment, f64_from_u64, ms_cost_pct,
    parse_export_stats_events, parse_pgbench_output, percentile, tps_cost_pct,
};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct IndexVariant {
    id: &'static str,
    label: &'static str,
    create_sql: Option<&'static str>,
}

const DEFAULT_INDEX_VARIANT: IndexVariant = IndexVariant {
    id: "record_id_hash",
    label: "Default partial hash (ADR-0008): `USING hash (record_id) WHERE record_id IS NOT NULL`",
    create_sql: Some(
        "CREATE INDEX changes_lookup_idx ON auditstream.changes USING hash (record_id) WHERE record_id IS NOT NULL",
    ),
};

const INDEX_VARIANTS: [IndexVariant; 4] = [
    IndexVariant {
        id: "none",
        label: "None (no lookup index)",
        create_sql: None,
    },
    IndexVariant {
        id: "current",
        label: "Legacy composite B-tree: `(table_name, record_id, occurred_at)`",
        create_sql: Some(
            "CREATE INDEX changes_lookup_idx ON auditstream.changes (table_name, record_id, occurred_at)",
        ),
    },
    IndexVariant {
        id: "table_occurred_at",
        label: "Narrow: `(table_name, occurred_at DESC)`",
        create_sql: Some(
            "CREATE INDEX changes_lookup_idx ON auditstream.changes (table_name, occurred_at DESC)",
        ),
    },
    DEFAULT_INDEX_VARIANT,
];

#[derive(Debug, Clone)]
struct CliOutput {
    stdout: String,
    stderr: String,
    elapsed_secs: f64,
}

trait BenchRunner {
    fn exec_sql(&self, db_url: &str, sql: &str) -> Result<f64, String>;
    fn query_scalar(&self, db_url: &str, sql: &str) -> Result<String, String>;
    fn run_pgbench_raw(
        &self,
        db_url: &str,
        script: &Path,
        cfg: &BenchConfig,
        duration_secs: u32,
    ) -> Result<String, String>;
    fn run_cli(
        &self,
        bin: &Path,
        db_url: &str,
        work_dir: &Path,
        args: &[&str],
    ) -> Result<CliOutput, String>;
}

struct ShellRunner;

const AMBIENT_PG_ENV_VARS: [&str; 11] = [
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGUSER",
    "PGPASSWORD",
    "PGDATABASE",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGPASSFILE",
    "PGSSLMODE",
    "PGOPTIONS",
];

/// Clears ambient libpq connection env vars and moves any embedded URI password into
/// `PGPASSWORD` so `psql` and `pgbench` never expose credentials in process argv.
fn apply_libpq_uri(cmd: &mut Command, db_url: &str) -> String {
    for var in AMBIENT_PG_ENV_VARS {
        cmd.env_remove(var);
    }
    let (sanitized_url, password) = split_uri_password(db_url);
    if let Some(pw) = password {
        cmd.env("PGPASSWORD", pw);
    }
    sanitized_url
}

impl BenchRunner for ShellRunner {
    fn exec_sql(&self, db_url: &str, sql: &str) -> Result<f64, String> {
        let start = Instant::now();
        let mut cmd = Command::new("psql");
        let sanitized_url = apply_libpq_uri(&mut cmd, db_url);
        let mut child = cmd
            .args([&sanitized_url, "-v", "ON_ERROR_STOP=1", "-q", "-X"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to spawn psql: {err}"))?;

        let write_result = child
            .stdin
            .take()
            .map_or(Ok(()), |mut stdin| stdin.write_all(sql.as_bytes()));

        let output = child
            .wait_with_output()
            .map_err(|err| format!("failed to wait on psql: {err}"))?;

        if !output.status.success() {
            return Err(format!(
                "psql failed (status {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if let Err(err) = write_result {
            return Err(format!("failed to write SQL to psql stdin: {err}"));
        }
        Ok(start.elapsed().as_secs_f64() * 1000.0)
    }

    fn query_scalar(&self, db_url: &str, sql: &str) -> Result<String, String> {
        let mut cmd = Command::new("psql");
        let sanitized_url = apply_libpq_uri(&mut cmd, db_url);
        let output = cmd
            .args([&sanitized_url, "-v", "ON_ERROR_STOP=1", "-X", "-Atc", sql])
            .output()
            .map_err(|err| format!("failed to run psql scalar: {err}"))?;

        if !output.status.success() {
            return Err(format!(
                "psql scalar failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn run_pgbench_raw(
        &self,
        db_url: &str,
        script: &Path,
        cfg: &BenchConfig,
        duration_secs: u32,
    ) -> Result<String, String> {
        let mut cmd = Command::new("pgbench");
        let sanitized_url = apply_libpq_uri(&mut cmd, db_url);
        let output = cmd
            .args([
                "-n",
                "-M",
                &cfg.mode,
                "-c",
                &cfg.clients.to_string(),
                "-j",
                &cfg.jobs.to_string(),
                "-T",
                &duration_secs.to_string(),
                "-f",
            ])
            .arg(script)
            .arg(&sanitized_url)
            .output()
            .map_err(|err| format!("failed to run pgbench ({}): {err}", script.display()))?;

        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            return Err(format!(
                "pgbench failed for {} (status {}):\n{combined}",
                script.display(),
                output.status
            ));
        }
        Ok(combined)
    }

    fn run_cli(
        &self,
        bin: &Path,
        db_url: &str,
        work_dir: &Path,
        args: &[&str],
    ) -> Result<CliOutput, String> {
        let start = Instant::now();
        let mut cmd = Command::new(bin);
        cmd.current_dir(work_dir);
        for var in AMBIENT_PG_ENV_VARS {
            cmd.env_remove(var);
        }
        for var in [
            "DATABASE_URL",
            "AUDITSTREAM_DATABASE_URL",
            "AUDITSTREAM_CONFIG",
            "AUDITSTREAM_SCHEMA",
            "AUDITSTREAM_LOG",
            "AUDITSTREAM_LOG_FORMAT",
        ] {
            cmd.env_remove(var);
        }
        if !db_url.is_empty() {
            cmd.env("AUDITSTREAM_DATABASE_URL", db_url);
        }
        let output = cmd
            .args(args)
            .output()
            .map_err(|err| format!("failed to run {}: {err}", bin.display()))?;
        let elapsed_secs = start.elapsed().as_secs_f64();

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(format!(
                "{} failed (status {}): {}",
                bin.display(),
                output.status,
                stderr.trim()
            ));
        }
        Ok(CliOutput {
            stdout,
            stderr,
            elapsed_secs,
        })
    }
}

struct TempBenchDb<'a, R: BenchRunner> {
    runner: &'a R,
    admin_url: String,
    bench_url: String,
    db_name: String,
}

impl<'a, R: BenchRunner> TempBenchDb<'a, R> {
    fn create(runner: &'a R, admin_url: &str) -> Result<Self, String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let db_name = format!("auditstream_bench_{}_{}_{}", std::process::id(), ts, seq);
        // Validate and derive `bench_url` BEFORE creating the database so invalid URLs never leak an orphan DB.
        let bench_url = replace_db_name(admin_url, &db_name)?;
        runner.exec_sql(admin_url, &format!("CREATE DATABASE \"{db_name}\""))?;
        Ok(Self {
            runner,
            admin_url: admin_url.to_owned(),
            bench_url,
            db_name,
        })
    }
}

impl<R: BenchRunner> Drop for TempBenchDb<'_, R> {
    fn drop(&mut self) {
        let drop_sql = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.db_name);
        if let Err(err) = self.runner.exec_sql(&self.admin_url, &drop_sql) {
            eprintln!(
                "xtask bench: warning: failed to drop {}: {err}",
                self.db_name
            );
        }
    }
}

struct TempWorkDir {
    path: PathBuf,
}

impl TempWorkDir {
    fn create() -> Result<Self, String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut builder = DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }

        for _ in 0..32 {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "auditstream_bench_work_{}_{}_{}",
                std::process::id(),
                ts,
                seq
            ));
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(format!(
                        "failed to create temp work dir {}: {err}",
                        path.display()
                    ));
                }
            }
        }
        Err("failed to allocate unique temp work directory after 32 attempts".to_owned())
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask bench: error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_bench_help();
        return Ok(());
    }

    let env_db_url = env::var("DATABASE_URL").ok();
    let cfg = parse_args(args, env_db_url)?;
    let repo_root = find_repo_root()?;
    let bench_dir = repo_root.join("bench").join("capture");
    let auditstream_bin = resolve_auditstream_bin(&cfg, &repo_root)?;
    let git_commit = git_short_commit(&repo_root);
    let runner = ShellRunner;

    let (_report, comparison, markdown) =
        execute_and_emit(&runner, &cfg, &bench_dir, &auditstream_bin, &git_commit)?;

    print!("{markdown}");
    finalize_regression_gate(comparison.as_ref())
}

fn execute_and_emit<R: BenchRunner>(
    runner: &R,
    cfg: &BenchConfig,
    bench_dir: &Path,
    auditstream_bin: &Path,
    git_commit: &str,
) -> Result<(BenchReport, Option<ComparisonReport>, String), String> {
    let baseline = match &cfg.compare_baseline {
        Some(path) => Some((path.display().to_string(), load_baseline_report(path)?)),
        None => None,
    };

    let report = execute_benchmark(runner, cfg, bench_dir, auditstream_bin, git_commit)?;

    let comparison = match &baseline {
        Some((label, base_report)) => Some(compare_reports(
            base_report,
            &report,
            label,
            cfg.max_regression_pct,
        )?),
        None => None,
    };

    let markdown = render_markdown(&report, comparison.as_ref());
    let json = serde_json::to_string_pretty(&report)
        .map_err(|err| format!("failed to serialize JSON report: {err}"))?;

    if let Some(md_path) = &cfg.markdown_out {
        write_file_creating_parents(md_path, markdown.as_bytes())?;
    }
    if let Some(json_path) = &cfg.json_out {
        write_file_creating_parents(json_path, format!("{json}\n").as_bytes())?;
    }

    Ok((report, comparison, markdown))
}

fn finalize_regression_gate(comparison: Option<&ComparisonReport>) -> Result<(), String> {
    if let Some(cmp) = comparison
        && cmp.has_regressions()
    {
        return Err(format!(
            "performance regression exceeded {:.1}% threshold on {} metric(s):\n  - {}",
            cmp.max_regression_pct,
            cmp.regressions.len(),
            cmp.regressions.join("\n  - ")
        ));
    }
    Ok(())
}

fn execute_benchmark<R: BenchRunner>(
    runner: &R,
    cfg: &BenchConfig,
    bench_dir: &Path,
    auditstream_bin: &Path,
    git_commit: &str,
) -> Result<BenchReport, String> {
    let temp_db = TempBenchDb::create(runner, &cfg.database_url)?;
    let pg_version = runner.query_scalar(&temp_db.bench_url, "SELECT version()")?;
    let pgbench_settings = format!(
        "-M {} -c {} -j {} -T {}",
        cfg.mode, cfg.clients, cfg.jobs, cfg.duration_secs
    );

    eprintln!("==> Setting up benchmark schema in {}", temp_db.db_name);
    setup_benchmark_schema(runner, &temp_db.bench_url)?;

    eprintln!(
        "==> Running core capture workloads ({pgbench_settings}, {} sample(s), warmup {}s)",
        cfg.samples, cfg.warmup_secs
    );
    let (workloads, baselines) = run_core_workloads(runner, &temp_db.bench_url, bench_dir, cfg)?;

    eprintln!("==> Measuring lookup index variants on worst-case UPDATE and 10k bulk INSERT");
    let index_variants = run_index_variants(runner, &temp_db.bench_url, bench_dir, cfg, baselines)?;

    eprintln!(
        "==> Seeding {} history rows for CLI export and history lookup benchmarks",
        cfg.export_rows
    );
    apply_index_variant(runner, &temp_db.bench_url, DEFAULT_INDEX_VARIANT)?;
    runner.exec_sql(&temp_db.bench_url, &build_seed_changes_sql(cfg.export_rows))?;

    let work_dir = TempWorkDir::create()?;

    eprintln!("==> Measuring auditstream export throughput (plain & redacted JSONL)");
    let export_workloads = run_export_workloads(
        runner,
        &temp_db.bench_url,
        auditstream_bin,
        &work_dir.path,
        cfg,
    )?;

    eprintln!(
        "==> Measuring auditstream history point-in-time lookup latency ({} queries/sample)",
        cfg.history_runs
    );
    let history_workloads = run_history_workloads(
        runner,
        &temp_db.bench_url,
        auditstream_bin,
        &work_dir.path,
        cfg,
    )?;

    Ok(BenchReport {
        schema_version: REPORT_SCHEMA_VERSION,
        postgres_version: pg_version,
        git_commit: git_commit.to_owned(),
        environment: Some(collect_environment()),
        pgbench_settings,
        samples: cfg.samples,
        warmup_secs: cfg.warmup_secs,
        bulk_iterations: cfg.bulk_iterations,
        export_rows: cfg.export_rows,
        history_runs: cfg.history_runs,
        workloads,
        index_variants,
        export_workloads,
        history_workloads,
    })
}

#[derive(Debug, Clone, Copy)]
struct Baselines {
    update_tps: f64,
    update_us_per_row: f64,
    bulk_insert_ms: f64,
    bulk_insert_us_per_row: f64,
}

#[derive(Debug, Clone, Copy)]
struct BulkSpec {
    sql: &'static str,
    captured_table: Option<&'static str>,
    captured_op: &'static str,
    label: &'static str,
}

const BULK_INSERT_BASE: BulkSpec = BulkSpec {
    sql: "INSERT INTO public.bench_bulk_plain (v) SELECT g FROM generate_series(1, 10000) AS g;",
    captured_table: None,
    captured_op: "INSERT",
    label: "10,000-row INSERT (baseline)",
};

const BULK_INSERT_CAP: BulkSpec = BulkSpec {
    sql: "INSERT INTO public.bench_bulk_cap (v) SELECT g FROM generate_series(1, 10000) AS g /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidmlhX2NvbW1lbnQifQ'*/;",
    captured_table: Some("bench_bulk_cap"),
    captured_op: "INSERT",
    label: "10,000-row INSERT (comment)",
};

const BULK_UPDATE_BASE: BulkSpec = BulkSpec {
    sql: "UPDATE public.bench_bulk_up_plain SET v = v + 1;",
    captured_table: None,
    captured_op: "UPDATE",
    label: "10,000-row UPDATE (baseline)",
};

const BULK_UPDATE_CAP: BulkSpec = BulkSpec {
    sql: "UPDATE public.bench_bulk_up_cap SET v = v + 1 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidmlhX2NvbW1lbnQifQ'*/;",
    captured_table: Some("bench_bulk_up_cap"),
    captured_op: "UPDATE",
    label: "10,000-row UPDATE (comment)",
};

fn run_core_workloads<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    bench_dir: &Path,
    cfg: &BenchConfig,
) -> Result<(Vec<WorkloadRow>, Baselines), String> {
    let (mut workloads, update_tps, update_us_per_row) =
        run_core_pgbench_workloads(runner, db_url, bench_dir, cfg)?;
    let (bulk_rows, bulk_insert_ms, bulk_insert_us_per_row) =
        run_core_bulk_workloads(runner, db_url, cfg)?;
    workloads.extend(bulk_rows);

    Ok((
        workloads,
        Baselines {
            update_tps,
            update_us_per_row,
            bulk_insert_ms,
            bulk_insert_us_per_row,
        },
    ))
}

fn run_core_pgbench_workloads<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    bench_dir: &Path,
    cfg: &BenchConfig,
) -> Result<(Vec<WorkloadRow>, f64, f64), String> {
    let (up_base_stats, up_base_tx) = run_pgbench_sampled(
        runner,
        db_url,
        &bench_dir.join("update_baseline.sql"),
        cfg,
        None,
    )?;
    let up_base_tps = up_base_stats.median;
    let up_base_micros = 1_000_000.0 / up_base_tps;

    let (up_comment_stats, up_comment_tx) = run_pgbench_sampled(
        runner,
        db_url,
        &bench_dir.join("update_comment.sql"),
        cfg,
        Some(("bench_up_cap", "comment")),
    )?;
    let (up_setcfg_stats, up_setcfg_tx) = run_pgbench_sampled(
        runner,
        db_url,
        &bench_dir.join("update_setcfg.sql"),
        cfg,
        Some(("bench_up_cap", "set_config")),
    )?;

    let (ins_base_stats, ins_base_tx) = run_pgbench_sampled(
        runner,
        db_url,
        &bench_dir.join("insert_baseline.sql"),
        cfg,
        None,
    )?;
    let ins_base_tps = ins_base_stats.median;
    let ins_base_micros = 1_000_000.0 / ins_base_tps;

    let (ins_cap_stats, ins_cap_tx) = run_pgbench_sampled(
        runner,
        db_url,
        &bench_dir.join("insert_captured.sql"),
        cfg,
        Some(("bench_ins_cap", "comment")),
    )?;

    let rows = vec![
        tps_row(
            "Single-row UPDATE (baseline)",
            up_base_stats,
            up_base_tx,
            None,
            false,
        ),
        tps_row(
            "Single-row UPDATE (comment)",
            up_comment_stats,
            up_comment_tx,
            Some((up_base_tps, up_base_micros)),
            true,
        ),
        tps_row(
            "Single-row UPDATE (set_config)",
            up_setcfg_stats,
            up_setcfg_tx,
            Some((up_base_tps, up_base_micros)),
            true,
        ),
        tps_row(
            "Single-row INSERT (baseline)",
            ins_base_stats,
            ins_base_tx,
            None,
            false,
        ),
        tps_row(
            "Single-row INSERT (comment)",
            ins_cap_stats,
            ins_cap_tx,
            Some((ins_base_tps, ins_base_micros)),
            true,
        ),
    ];

    Ok((rows, up_base_tps, up_base_micros))
}

fn run_core_bulk_workloads<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    cfg: &BenchConfig,
) -> Result<(Vec<WorkloadRow>, f64, f64), String> {
    let (ins_base, ins_base_rows) = run_bulk_sampled(runner, db_url, cfg, BULK_INSERT_BASE)?;
    let ins_base_millis = ins_base.median;
    let ins_base_micros = ins_base_millis * 1000.0 / f64_from_u64(BULK_ROWS);
    let (ins_cap, ins_cap_rows) = run_bulk_sampled(runner, db_url, cfg, BULK_INSERT_CAP)?;

    let (up_base, up_base_rows) = run_bulk_sampled(runner, db_url, cfg, BULK_UPDATE_BASE)?;
    let up_base_millis = up_base.median;
    let up_base_micros = up_base_millis * 1000.0 / f64_from_u64(BULK_ROWS);
    let (up_cap, up_cap_rows) = run_bulk_sampled(runner, db_url, cfg, BULK_UPDATE_CAP)?;

    let rows = vec![
        bulk_row(
            BULK_INSERT_BASE.label,
            ins_base,
            BULK_ROWS,
            ins_base_rows,
            None,
            false,
        ),
        bulk_row(
            BULK_INSERT_CAP.label,
            ins_cap,
            BULK_ROWS,
            ins_cap_rows,
            Some((ins_base_millis, ins_base_micros)),
            true,
        ),
        bulk_row(
            BULK_UPDATE_BASE.label,
            up_base,
            BULK_ROWS,
            up_base_rows,
            None,
            false,
        ),
        bulk_row(
            BULK_UPDATE_CAP.label,
            up_cap,
            BULK_ROWS,
            up_cap_rows,
            Some((up_base_millis, up_base_micros)),
            true,
        ),
    ];

    Ok((rows, ins_base_millis, ins_base_micros))
}

fn run_index_variants<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    bench_dir: &Path,
    cfg: &BenchConfig,
    baselines: Baselines,
) -> Result<Vec<IndexVariantRow>, String> {
    let mut index_variants = Vec::with_capacity(INDEX_VARIANTS.len());
    for variant in INDEX_VARIANTS {
        eprintln!("  -> variant `{}`", variant.id);
        apply_index_variant(runner, db_url, variant)?;

        let (var_comment, _) = run_pgbench_sampled(
            runner,
            db_url,
            &bench_dir.join("update_comment.sql"),
            cfg,
            Some(("bench_up_cap", "comment")),
        )?;
        let (var_setcfg, _) = run_pgbench_sampled(
            runner,
            db_url,
            &bench_dir.join("update_setcfg.sql"),
            cfg,
            Some(("bench_up_cap", "set_config")),
        )?;
        let (var_bulk, _) = run_bulk_sampled(runner, db_url, cfg, BULK_INSERT_CAP)?;
        let var_bulk_us = var_bulk.median * 1000.0 / f64_from_u64(BULK_ROWS);

        index_variants.push(IndexVariantRow {
            variant: variant.id.to_owned(),
            label: variant.label.to_owned(),
            update_comment_tps: var_comment.median,
            update_comment_cost_pct: tps_cost_pct(baselines.update_tps, var_comment.median),
            update_comment_cost_us: (1_000_000.0 / var_comment.median)
                - baselines.update_us_per_row,
            update_comment_stats: Some(var_comment),
            update_setcfg_tps: var_setcfg.median,
            update_setcfg_cost_pct: tps_cost_pct(baselines.update_tps, var_setcfg.median),
            update_setcfg_cost_us: (1_000_000.0 / var_setcfg.median) - baselines.update_us_per_row,
            update_setcfg_stats: Some(var_setcfg),
            bulk_insert_10k_ms: var_bulk.median,
            bulk_insert_10k_cost_pct: ms_cost_pct(baselines.bulk_insert_ms, var_bulk.median),
            bulk_insert_10k_cost_us: var_bulk_us - baselines.bulk_insert_us_per_row,
            bulk_insert_10k_stats: Some(var_bulk),
        });
    }
    Ok(index_variants)
}

struct ExportRun<'a> {
    db_url: &'a str,
    auditstream_bin: &'a Path,
    work_dir: &'a Path,
    config_path: &'a Path,
    output_path: &'a Path,
    expected_rows: u64,
    verify_redaction: bool,
}

fn run_export_workloads<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    auditstream_bin: &Path,
    work_dir: &Path,
    cfg: &BenchConfig,
) -> Result<Vec<ExportWorkloadRow>, String> {
    let plain_cfg = work_dir.join("plain.toml");
    let redacted_cfg = work_dir.join("redacted.toml");
    fs::write(&plain_cfg, build_export_toml(false))
        .map_err(|err| format!("failed to write {}: {err}", plain_cfg.display()))?;
    fs::write(&redacted_cfg, build_export_toml(true))
        .map_err(|err| format!("failed to write {}: {err}", redacted_cfg.display()))?;

    let plain_out = work_dir.join("plain.jsonl");
    let redacted_out = work_dir.join("redacted.jsonl");

    let plain_stats = run_export_mode(
        runner,
        &ExportRun {
            db_url,
            auditstream_bin,
            work_dir,
            config_path: &plain_cfg,
            output_path: &plain_out,
            expected_rows: cfg.export_rows,
            verify_redaction: false,
        },
        cfg,
    )?;
    let redacted_stats = run_export_mode(
        runner,
        &ExportRun {
            db_url,
            auditstream_bin,
            work_dir,
            config_path: &redacted_cfg,
            output_path: &redacted_out,
            expected_rows: cfg.export_rows,
            verify_redaction: true,
        },
        cfg,
    )?;

    let plain_rate = plain_stats.median;
    Ok(vec![
        export_row(
            "auditstream export (plain JSONL)",
            cfg.export_rows,
            plain_stats,
            None,
        ),
        export_row(
            "auditstream export (redacted JSONL)",
            cfg.export_rows,
            redacted_stats,
            Some(plain_rate),
        ),
    ])
}

fn run_export_mode<R: BenchRunner>(
    runner: &R,
    spec: &ExportRun<'_>,
    cfg: &BenchConfig,
) -> Result<SampleStats, String> {
    if cfg.warmup_secs > 0 {
        let _ = run_single_export(runner, spec)?;
    }

    let mut rates = Vec::with_capacity(usize::try_from(cfg.samples).unwrap_or(3));
    for _ in 0..cfg.samples {
        rates.push(run_single_export(runner, spec)?);
    }
    SampleStats::from_samples(rates)
}

fn run_single_export<R: BenchRunner>(runner: &R, spec: &ExportRun<'_>) -> Result<f64, String> {
    let _ = fs::remove_file(spec.output_path);
    let config_str = spec.config_path.to_string_lossy();
    let output_str = spec.output_path.to_string_lossy();
    let cli_out = runner.run_cli(
        spec.auditstream_bin,
        spec.db_url,
        spec.work_dir,
        &[
            "--config",
            &config_str,
            "--schema",
            "auditstream",
            "export",
            "--format",
            "jsonl",
            "--output",
            &output_str,
            "--stats-format",
            "json",
        ],
    )?;

    let exported = parse_export_stats_events(&cli_out.stderr)?;
    if exported != spec.expected_rows {
        return Err(format!(
            "auditstream export mismatch: expected {} events, got {exported}",
            spec.expected_rows
        ));
    }

    if spec.verify_redaction {
        let sample_content = fs::read_to_string(spec.output_path)
            .map_err(|err| format!("failed to verify redacted export file: {err}"))?;
        if !sample_content.contains("[redacted]")
            || sample_content.contains("123-45-6789")
            || sample_content.contains("@example.com")
        {
            return Err("redacted export verification failed: PII leak or missing mask".to_owned());
        }
    }
    let _ = fs::remove_file(spec.output_path);

    if cli_out.elapsed_secs <= 0.0 || !cli_out.elapsed_secs.is_finite() {
        return Err(format!(
            "invalid export elapsed duration: {}",
            cli_out.elapsed_secs
        ));
    }
    Ok(f64_from_u64(spec.expected_rows) / cli_out.elapsed_secs)
}

fn run_history_workloads<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    auditstream_bin: &Path,
    work_dir: &Path,
    cfg: &BenchConfig,
) -> Result<Vec<HistoryWorkloadRow>, String> {
    if cfg.warmup_secs > 0 {
        for i in 0..3_u64 {
            let id = (i % cfg.export_rows) + 1;
            let _ = run_single_history_query(runner, db_url, auditstream_bin, work_dir, id)?;
        }
    }

    let step = (cfg.export_rows / u64::from(cfg.history_runs)).max(1);
    let mut p50_samples = Vec::with_capacity(usize::try_from(cfg.samples).unwrap_or(3));
    let mut p99_samples = Vec::with_capacity(usize::try_from(cfg.samples).unwrap_or(3));

    for _ in 0..cfg.samples {
        let mut latencies_ms = Vec::with_capacity(usize::try_from(cfg.history_runs).unwrap_or(20));
        for q in 0..u64::from(cfg.history_runs) {
            let id = ((q * step) % cfg.export_rows) + 1;
            latencies_ms.push(run_single_history_query(
                runner,
                db_url,
                auditstream_bin,
                work_dir,
                id,
            )?);
        }
        p50_samples.push(percentile(&latencies_ms, 50.0)?);
        p99_samples.push(percentile(&latencies_ms, 99.0)?);
    }

    Ok(vec![history_row(
        "auditstream history (--table public.invoice --record-id)",
        cfg.history_runs,
        SampleStats::from_samples(p50_samples)?,
        SampleStats::from_samples(p99_samples)?,
    )])
}

fn run_single_history_query<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    auditstream_bin: &Path,
    work_dir: &Path,
    record_id: u64,
) -> Result<f64, String> {
    let id_str = record_id.to_string();
    let out = runner.run_cli(
        auditstream_bin,
        db_url,
        work_dir,
        &[
            "--schema",
            "auditstream",
            "history",
            "--table",
            "public.invoice",
            "--record-id",
            &id_str,
            "--format",
            "json",
            "--limit",
            "20",
        ],
    )?;
    if out.stdout.trim().is_empty() {
        return Err(format!(
            "auditstream history returned 0 events for seeded record_id={record_id}"
        ));
    }
    Ok(out.elapsed_secs * 1000.0)
}

fn setup_benchmark_schema<R: BenchRunner>(runner: &R, db_url: &str) -> Result<(), String> {
    runner.exec_sql(db_url, &build_seed_tables_sql())?;
    let install_sql = build_setup_schema_sql()?;
    let _ = runner.exec_sql(db_url, &install_sql)?;
    Ok(())
}

fn apply_index_variant<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    variant: IndexVariant,
) -> Result<(), String> {
    let mut sql = String::from(
        "TRUNCATE auditstream.changes;\n\
         DROP INDEX IF EXISTS auditstream.changes_lookup_idx;\n",
    );
    if let Some(create_sql) = variant.create_sql {
        sql.push_str(create_sql);
        sql.push_str(";\n");
    }
    sql.push_str("ANALYZE auditstream.changes;\n");
    let _ = runner.exec_sql(db_url, &sql)?;
    settle_workload_state(runner, db_url)
}

fn settle_workload_state<R: BenchRunner>(runner: &R, db_url: &str) -> Result<(), String> {
    let _ = runner.exec_sql(
        db_url,
        "VACUUM ANALYZE public.bench_up_plain, public.bench_up_cap, \
         public.bench_ins_plain, public.bench_ins_cap, \
         public.bench_bulk_up_plain, public.bench_bulk_up_cap;\n",
    )?;
    // `CHECKPOINT` requires superuser or `pg_checkpoint`; skip silently on managed roles.
    let _ = runner.exec_sql(db_url, "CHECKPOINT;\n");
    Ok(())
}

fn run_pgbench_sampled<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    script: &Path,
    cfg: &BenchConfig,
    capture_check: Option<(&'static str, &'static str)>,
) -> Result<(SampleStats, u64), String> {
    if cfg.warmup_secs > 0 {
        let _ = run_pgbench_once(runner, db_url, script, cfg, cfg.warmup_secs, capture_check)?;
    }

    let mut tps_values = Vec::with_capacity(usize::try_from(cfg.samples).unwrap_or(3));
    let mut total_tx = 0_u64;
    for _ in 0..cfg.samples {
        let res = run_pgbench_once(
            runner,
            db_url,
            script,
            cfg,
            cfg.duration_secs,
            capture_check,
        )?;
        tps_values.push(res.tps);
        total_tx += res.tx;
    }
    Ok((SampleStats::from_samples(tps_values)?, total_tx))
}

fn run_pgbench_once<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    script: &Path,
    cfg: &BenchConfig,
    duration_secs: u32,
    capture_check: Option<(&'static str, &'static str)>,
) -> Result<PgbenchResult, String> {
    settle_workload_state(runner, db_url)?;
    let start_id = if capture_check.is_some() {
        max_change_id(runner, db_url)?
    } else {
        0
    };

    let combined = runner.run_pgbench_raw(db_url, script, cfg, duration_secs)?;
    let parsed = parse_pgbench_output(&combined)?;
    if parsed.failed != 0 {
        return Err(format!(
            "pgbench reported {} failed transaction(s) for {}:\n{combined}",
            parsed.failed,
            script.display()
        ));
    }
    if parsed.tx == 0 || parsed.tps <= 0.0 || !parsed.tps.is_finite() {
        return Err(format!(
            "pgbench processed 0 transactions for {}:\n{combined}",
            script.display()
        ));
    }

    if let Some((table_name, actor_source)) = capture_check {
        let captured =
            count_captured_rows(runner, db_url, start_id, table_name, actor_source, None)?;
        if captured != parsed.tx {
            return Err(format!(
                "captured row mismatch for {}: expected {} rows with actor_source='{actor_source}', got {captured}",
                script.display(),
                parsed.tx
            ));
        }
    }
    Ok(parsed)
}

fn run_bulk_sampled<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    cfg: &BenchConfig,
    spec: BulkSpec,
) -> Result<(SampleStats, u64), String> {
    debug_assert!(spec.captured_table.is_none() || spec.sql.contains(COMMENT_TAG));

    if cfg.warmup_secs > 0 {
        let _ = run_bulk_sample_once(runner, db_url, 1, spec)?;
    }

    let mut sample_medians = Vec::with_capacity(usize::try_from(cfg.samples).unwrap_or(3));
    let mut total_rows = 0_u64;
    for _ in 0..cfg.samples {
        let (median_ms, rows) = run_bulk_sample_once(runner, db_url, cfg.bulk_iterations, spec)?;
        sample_medians.push(median_ms);
        total_rows += rows;
    }
    Ok((SampleStats::from_samples(sample_medians)?, total_rows))
}

fn run_bulk_sample_once<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    iterations: u32,
    spec: BulkSpec,
) -> Result<(f64, u64), String> {
    settle_workload_state(runner, db_url)?;
    let start_id = if spec.captured_table.is_some() {
        max_change_id(runner, db_url)?
    } else {
        0
    };

    let mut iter_ms = Vec::with_capacity(usize::try_from(iterations).unwrap_or(5));
    for _ in 0..iterations {
        iter_ms.push(runner.exec_sql(db_url, spec.sql)?);
    }

    let expected_rows = BULK_ROWS * u64::from(iterations);
    if let Some(table_name) = spec.captured_table {
        let captured = count_captured_rows(
            runner,
            db_url,
            start_id,
            table_name,
            "comment",
            Some(spec.captured_op),
        )?;
        if captured != expected_rows {
            return Err(format!(
                "{} captured mismatch: expected {expected_rows} rows, got {captured}",
                spec.label
            ));
        }
    }

    let stats = SampleStats::from_samples(iter_ms)?;
    Ok((stats.median, expected_rows))
}

fn count_captured_rows<R: BenchRunner>(
    runner: &R,
    db_url: &str,
    start_id: u64,
    table_name: &str,
    actor_source: &str,
    op: Option<&str>,
) -> Result<u64, String> {
    let op_clause = op.map_or(String::new(), |v| format!(" AND op = '{v}'"));
    runner
        .query_scalar(
            db_url,
            &format!(
                "SELECT count(*) FROM auditstream.changes \
                 WHERE id > {start_id} \
                   AND table_name = '{table_name}' \
                   AND actor_source = '{actor_source}'{op_clause}"
            ),
        )?
        .parse()
        .map_err(|err| format!("invalid captured row count: {err}"))
}

fn max_change_id<R: BenchRunner>(runner: &R, db_url: &str) -> Result<u64, String> {
    runner
        .query_scalar(
            db_url,
            "SELECT coalesce(max(id), 0) FROM auditstream.changes",
        )?
        .parse()
        .map_err(|err| format!("invalid max(id): {err}"))
}

fn load_baseline_report(path: &Path) -> Result<BenchReport, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read baseline {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("failed to parse baseline {}: {err}", path.display()))
}

fn write_file_creating_parents(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create directory {}: {err}", parent.display()))?;
    }
    fs::write(path, bytes).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn resolve_auditstream_bin(cfg: &BenchConfig, repo_root: &Path) -> Result<PathBuf, String> {
    if let Some(bin) = &cfg.auditstream_bin {
        if !bin.is_file() {
            return Err(format!(
                "--auditstream-bin `{}` does not exist",
                bin.display()
            ));
        }
        return fs::canonicalize(bin).map_err(|err| {
            format!(
                "failed to resolve --auditstream-bin `{}`: {err}",
                bin.display()
            )
        });
    }

    eprintln!("==> Building release auditstream CLI binary for export/history benchmarks");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "auditstream", "--locked"])
        .current_dir(repo_root)
        .status()
        .map_err(|err| format!("failed to run cargo build for auditstream: {err}"))?;
    if !status.success() {
        return Err(format!(
            "cargo build -p auditstream --release exited with {status}"
        ));
    }

    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let bin = release_auditstream_bin(repo_root, target_dir.as_deref());
    if !bin.is_file() {
        return Err(format!(
            "expected built auditstream binary at {}",
            bin.display()
        ));
    }
    Ok(bin)
}

fn git_short_commit(repo_root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "unknown".to_owned(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )
}

fn find_repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not resolve workspace root from CARGO_MANIFEST_DIR".to_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::bench::compare::CompareStatus;

    #[derive(Clone)]
    struct FakeChangeBatch {
        start_id: u64,
        end_id: u64,
        count: u64,
        table: &'static str,
        actor_source: &'static str,
        op: &'static str,
    }

    #[derive(Clone, Copy)]
    enum RedactFixture {
        Clean,
        LeakSsnWithMask,
        LeakEmailWithMask,
        MissingMaskNoPii,
    }

    struct FakeState {
        next_id: u64,
        seeded_rows: u64,
        tick: u64,
        changes: Vec<FakeChangeBatch>,
        calls: Vec<String>,
    }

    struct FakeRunner {
        scale: f64,
        pgbench_tx: u64,
        pgbench_failed: u64,
        mismatch_table: Option<&'static str>,
        redact_fixture: RedactFixture,
        export_events_override: Option<u64>,
        export_elapsed_override: Option<f64>,
        empty_history: bool,
        fail_drop: bool,
        state: RefCell<FakeState>,
    }

    impl FakeRunner {
        fn healthy(scale: f64) -> Self {
            Self {
                scale,
                pgbench_tx: 1_000,
                pgbench_failed: 0,
                mismatch_table: None,
                redact_fixture: RedactFixture::Clean,
                export_events_override: None,
                export_elapsed_override: None,
                empty_history: false,
                fail_drop: false,
                state: RefCell::new(FakeState {
                    next_id: 0,
                    seeded_rows: 0,
                    tick: 0,
                    changes: Vec::new(),
                    calls: Vec::new(),
                }),
            }
        }

        fn push_changes(
            &self,
            table: &'static str,
            actor_source: &'static str,
            op: &'static str,
            raw_count: u64,
        ) {
            let count = if self.mismatch_table == Some(table) {
                raw_count.saturating_sub(1)
            } else {
                raw_count
            };
            if count == 0 {
                return;
            }
            let mut st = self.state.borrow_mut();
            let start_id = st.next_id + 1;
            let end_id = st.next_id + count;
            st.next_id = end_id;
            st.changes.push(FakeChangeBatch {
                start_id,
                end_id,
                count,
                table,
                actor_source,
                op,
            });
        }

        fn next_jitter(&self) -> f64 {
            let mut st = self.state.borrow_mut();
            st.tick += 1;
            // Deterministic varied factors so samples have non-zero spread and p99 > p50.
            match st.tick % 5 {
                0 => 0.96,
                1 => 1.00,
                2 => 1.04,
                3 => 0.98,
                _ => 1.08,
            }
        }
    }

    fn extract_quoted_field<'a>(sql: &'a str, prefix: &str) -> Option<&'a str> {
        let start = sql.find(prefix)? + prefix.len();
        let end = sql[start..].find('\'')?;
        Some(&sql[start..start + end])
    }

    fn extract_u64_after(sql: &str, prefix: &str) -> Option<u64> {
        let start = sql.find(prefix)? + prefix.len();
        let token = sql[start..].split(|c: char| !c.is_ascii_digit()).next()?;
        token.parse().ok()
    }

    impl BenchRunner for FakeRunner {
        fn exec_sql(&self, _db_url: &str, sql: &str) -> Result<f64, String> {
            if self.fail_drop && sql.starts_with("DROP DATABASE") {
                return Err("simulated drop error".to_owned());
            }
            self.state
                .borrow_mut()
                .calls
                .push(format!("sql:{}", sql.lines().next().unwrap_or("")));

            if sql.contains("TRUNCATE auditstream.changes") {
                let mut st = self.state.borrow_mut();
                st.changes.clear();
                st.next_id = 0;
            }
            if let Some(n) = extract_u64_after(sql, "FROM generate_series(1, ")
                && sql.contains("INSERT INTO auditstream.changes")
            {
                self.state.borrow_mut().seeded_rows = n;
            }

            let jitter = self.next_jitter();
            let base_ms = if sql.starts_with("UPDATE public.bench_bulk_up_cap") {
                self.push_changes("bench_bulk_up_cap", "comment", "UPDATE", BULK_ROWS);
                600.0
            } else if sql.starts_with("UPDATE public.bench_bulk_up_plain") {
                12.0
            } else if sql.starts_with("INSERT INTO public.bench_bulk_cap") {
                self.push_changes("bench_bulk_cap", "comment", "INSERT", BULK_ROWS);
                55.0
            } else {
                20.0
            };
            Ok((base_ms * jitter) / self.scale)
        }

        fn query_scalar(&self, _db_url: &str, sql: &str) -> Result<String, String> {
            if sql.contains("version()") {
                return Ok("PostgreSQL 18.6 (fake)".to_owned());
            }
            if sql.contains("max(id)") {
                return Ok(self.state.borrow().next_id.to_string());
            }
            if sql.contains("SELECT count(*) FROM auditstream.changes") {
                let start_id = extract_u64_after(sql, "WHERE id > ").unwrap_or(0);
                let table = extract_quoted_field(sql, "table_name = '").unwrap_or("");
                let actor_source = extract_quoted_field(sql, "actor_source = '").unwrap_or("");
                let op = extract_quoted_field(sql, "AND op = '");
                let count: u64 = self
                    .state
                    .borrow()
                    .changes
                    .iter()
                    .filter(|b| {
                        b.start_id > start_id
                            && b.end_id > start_id
                            && b.table == table
                            && b.actor_source == actor_source
                            && op.is_none_or(|want| b.op == want)
                    })
                    .map(|b| b.count)
                    .sum();
                return Ok(count.to_string());
            }
            Ok("0".to_owned())
        }

        fn run_pgbench_raw(
            &self,
            _db_url: &str,
            script: &Path,
            _cfg: &BenchConfig,
            duration_secs: u32,
        ) -> Result<String, String> {
            let name = script.file_name().and_then(|s| s.to_str()).unwrap_or("");
            self.state
                .borrow_mut()
                .calls
                .push(format!("pgbench:{name}:T{duration_secs}"));

            if self.pgbench_failed == 0 && self.pgbench_tx > 0 {
                match name {
                    "update_comment.sql" => {
                        self.push_changes("bench_up_cap", "comment", "UPDATE", self.pgbench_tx);
                    }
                    "update_setcfg.sql" => {
                        self.push_changes("bench_up_cap", "set_config", "UPDATE", self.pgbench_tx);
                    }
                    "insert_captured.sql" => {
                        self.push_changes("bench_ins_cap", "comment", "INSERT", self.pgbench_tx);
                    }
                    _ => {}
                }
            }

            let jitter = self.next_jitter();
            let base_tps = if name.contains("baseline") {
                10_000.0
            } else if name.contains("setcfg") {
                7_500.0
            } else {
                8_500.0
            };
            Ok(format!(
                "number of transactions actually processed: {}\n\
                 number of failed transactions: {} (0.000%)\n\
                 tps = {:.3} (without initial connection time)\n",
                self.pgbench_tx,
                self.pgbench_failed,
                base_tps * jitter * self.scale
            ))
        }

        fn run_cli(
            &self,
            _bin: &Path,
            _db_url: &str,
            _work_dir: &Path,
            args: &[&str],
        ) -> Result<CliOutput, String> {
            let jitter = self.next_jitter();
            if args.contains(&"export") {
                let out_pos = args
                    .iter()
                    .position(|&a| a == "--output")
                    .expect("output arg");
                let out_path = Path::new(args[out_pos + 1]);
                let cfg_pos = args
                    .iter()
                    .position(|&a| a == "--config")
                    .expect("config arg");
                let is_redacted = args[cfg_pos + 1].contains("redacted");
                self.state.borrow_mut().calls.push(if is_redacted {
                    "cli:export:redacted".to_owned()
                } else {
                    "cli:export:plain".to_owned()
                });
                let body = if is_redacted {
                    match self.redact_fixture {
                        RedactFixture::Clean => {
                            "{\"after\":{\"ssn\":\"[redacted]\",\"email\":\"[redacted]\"}}\n"
                        }
                        RedactFixture::LeakSsnWithMask => {
                            "{\"after\":{\"ssn\":\"123-45-6789\",\"email\":\"[redacted]\"}}\n"
                        }
                        RedactFixture::LeakEmailWithMask => {
                            "{\"after\":{\"ssn\":\"[redacted]\",\"email\":\"u1@example.com\"}}\n"
                        }
                        RedactFixture::MissingMaskNoPii => "{\"after\":{\"status\":\"paid\"}}\n",
                    }
                } else {
                    "{\"after\":{\"ssn\":\"123-45-6789\",\"email\":\"u1@example.com\"}}\n"
                };
                fs::write(out_path, body).map_err(|e| e.to_string())?;
                let events = self
                    .export_events_override
                    .unwrap_or(self.state.borrow().seeded_rows);
                let elapsed_secs = self
                    .export_elapsed_override
                    .unwrap_or((0.02 * jitter) / self.scale);
                return Ok(CliOutput {
                    stdout: String::new(),
                    stderr: format!("{{\"events\":{events},\"batches\":1,\"duration_ms\":20}}\n"),
                    elapsed_secs,
                });
            }

            if args.contains(&"history") {
                self.state.borrow_mut().calls.push("cli:history".to_owned());
                return Ok(CliOutput {
                    stdout: if self.empty_history {
                        String::new()
                    } else {
                        "{\"event_id\":\"00000000-0000-0000-0000-000000000001\"}\n".to_owned()
                    },
                    stderr: String::new(),
                    elapsed_secs: (0.005 * jitter) / self.scale,
                });
            }
            Err(format!("unexpected cli args: {args:?}"))
        }
    }

    fn test_cfg(work_dir: &Path) -> BenchConfig {
        BenchConfig {
            database_url: "postgres://postgres@127.0.0.1:5432/postgres".to_owned(),
            mode: "prepared".to_owned(),
            clients: 2,
            jobs: 1,
            duration_secs: 2,
            warmup_secs: 1,
            samples: 3,
            bulk_iterations: 3,
            export_rows: 500,
            history_runs: 6,
            auditstream_bin: Some(PathBuf::from("/bin/true")),
            markdown_out: Some(work_dir.join("nested/report.md")),
            json_out: Some(work_dir.join("nested/report.json")),
            compare_baseline: None,
            max_regression_pct: 15.0,
        }
    }

    fn assert_warmup_call_counts(calls: &[String]) {
        assert_eq!(calls.iter().filter(|c| c.ends_with(":T1")).count(), 13);
        assert_eq!(calls.iter().filter(|c| c.ends_with(":T2")).count(), 39);
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("sql:UPDATE public.bench_bulk_up_cap"))
                .count(),
            10 // 1 warmup + 3 samples * 3 iterations
        );
        assert_eq!(
            calls.iter().filter(|c| *c == "cli:export:plain").count(),
            4 // 1 warmup + 3 samples
        );
        assert_eq!(
            calls.iter().filter(|c| *c == "cli:export:redacted").count(),
            4
        );
        assert_eq!(
            calls.iter().filter(|c| *c == "cli:history").count(),
            21 // 3 warmup + 3 samples * 6 queries
        );
    }

    #[test]
    fn execute_and_emit_runs_warmup_multi_iter_bulk_samples_and_compare_gate() {
        let dir = TempWorkDir::create().expect("temp dir");
        let cfg = test_cfg(&dir.path);
        let runner = FakeRunner::healthy(1.0);

        let (base_report, cmp, md) = execute_and_emit(
            &runner,
            &cfg,
            Path::new("bench/capture"),
            Path::new("/fake/auditstream"),
            "deadbee",
        )
        .expect("baseline run");
        assert!(cmp.is_none());
        assert_eq!(base_report.workloads.len(), 9);
        assert_eq!(base_report.index_variants.len(), 4);
        assert_eq!(base_report.export_workloads.len(), 2);
        assert_eq!(base_report.history_workloads.len(), 1);

        // Verify warmup actually executed AND was excluded from timed samples/row counts.
        assert_warmup_call_counts(&runner.state.borrow().calls);

        let up_comment = base_report
            .workloads
            .iter()
            .find(|w| w.name == "Single-row UPDATE (comment)")
            .expect("up_comment");
        assert_eq!(up_comment.rows_written, 3_000);
        assert_eq!(up_comment.rows_captured, 3_000);
        let up_stats = up_comment.stats.as_ref().expect("stats");
        assert_eq!(up_stats.samples.len(), 3);
        assert!(up_stats.spread > 0.0);

        // Verify multi-iteration bulk_iterations=3 scaling (3 samples * 3 iters * 10_000 = 90_000).
        let bulk_up = base_report
            .workloads
            .iter()
            .find(|w| w.name == "10,000-row UPDATE (comment)")
            .expect("bulk_up");
        assert_eq!(bulk_up.rows_written, 90_000);
        assert_eq!(bulk_up.rows_captured, 90_000);

        // Verify p99 > p50 under varied history latencies.
        let hist = &base_report.history_workloads[0];
        assert!(hist.p99_ms > hist.p50_ms);

        // Verify zero metric-name drift against committed bench/baselines/v0.1.0.json.
        let committed_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bench/baselines/v0.1.0.json");
        let committed = load_baseline_report(&committed_path).expect("load committed");
        let drift_cmp = compare_reports(&committed, &base_report, "v0.1.0.json", 10_000.0)
            .expect("drift compare");
        assert_eq!(drift_cmp.rows.len(), 25);
        assert!(
            drift_cmp
                .rows
                .iter()
                .all(|r| r.status != CompareStatus::New),
            "metric name drifted vs committed v0.1.0.json"
        );

        assert!(md.contains("10,000-row UPDATE (comment)"));
        assert!(dir.path.join("nested/report.json").is_file());
        assert!(dir.path.join("nested/report.md").is_file());

        // Compare passing run against saved baseline.
        let mut cmp_cfg = test_cfg(&dir.path);
        cmp_cfg.compare_baseline = Some(dir.path.join("nested/report.json"));
        cmp_cfg.markdown_out = Some(dir.path.join("cmp-pass.md"));
        cmp_cfg.json_out = Some(dir.path.join("cmp-pass.json"));
        let (_, cmp_pass, _) = execute_and_emit(
            &FakeRunner::healthy(1.0),
            &cmp_cfg,
            Path::new("bench/capture"),
            Path::new("/fake/auditstream"),
            "cafebab",
        )
        .expect("compare pass");
        finalize_regression_gate(cmp_pass.as_ref()).expect("no regression");

        // Compare regressed run (-30% speed): report files must still be written before Err.
        let mut reg_cfg = test_cfg(&dir.path);
        reg_cfg.compare_baseline = Some(dir.path.join("nested/report.json"));
        reg_cfg.markdown_out = Some(dir.path.join("cmp-fail.md"));
        reg_cfg.json_out = Some(dir.path.join("cmp-fail.json"));
        let (_, cmp_reg, _) = execute_and_emit(
            &FakeRunner::healthy(0.7),
            &reg_cfg,
            Path::new("bench/capture"),
            Path::new("/fake/auditstream"),
            "badc0de",
        )
        .expect("emit regressed report");
        let md_fail = fs::read_to_string(dir.path.join("cmp-fail.md")).expect("read fail md");
        assert!(md_fail.contains("REGRESSION"));
        let gate_err =
            finalize_regression_gate(cmp_reg.as_ref()).expect_err("regression must fail gate");
        assert!(gate_err.contains("performance regression exceeded"));
    }

    #[test]
    fn warmup_zero_skips_warmup_across_all_workloads_and_compare_fails_fast_before_db() {
        let dir = TempWorkDir::create().expect("temp dir");
        let mut cfg = test_cfg(&dir.path);
        cfg.warmup_secs = 0;
        cfg.samples = 2;
        cfg.bulk_iterations = 2;
        cfg.history_runs = 4;

        let runner = FakeRunner::healthy(1.0);
        let report = execute_benchmark(
            &runner,
            &cfg,
            Path::new("bench/capture"),
            Path::new("/fake/auditstream"),
            "abc1234",
        )
        .expect("warmup=0 run");
        let calls = runner.state.borrow().calls.clone();
        assert!(
            !calls.iter().any(|c| c.ends_with(":T0")),
            "warmup=0 must never invoke pgbench -T 0"
        );
        assert_eq!(
            calls.iter().filter(|c| c.starts_with("pgbench:")).count(),
            26 // 13 workloads * 2 samples, 0 warmup
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("sql:UPDATE public.bench_bulk_up_cap"))
                .count(),
            4 // 2 samples * 2 iterations, 0 warmup
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("cli:export:plain"))
                .count(),
            2
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("cli:export:redacted"))
                .count(),
            2
        );
        assert_eq!(calls.iter().filter(|c| *c == "cli:history").count(), 8);
        assert_eq!(report.workloads[0].rows_written, 2_000);

        // Fail-fast on missing or corrupt --compare baseline BEFORE touching DB.
        for bad_path in [dir.path.join("missing.json"), dir.path.join("corrupt.json")] {
            if bad_path.ends_with("corrupt.json") {
                fs::write(&bad_path, "{not json").expect("write corrupt");
            }
            let mut bad_cfg = test_cfg(&dir.path);
            bad_cfg.compare_baseline = Some(bad_path);
            let untouched = FakeRunner::healthy(1.0);
            assert!(
                execute_and_emit(
                    &untouched,
                    &bad_cfg,
                    Path::new("bench/capture"),
                    Path::new("/fake"),
                    "abc"
                )
                .is_err()
            );
            assert_eq!(untouched.state.borrow().calls.len(), 0);
        }
    }

    #[test]
    fn integrity_checks_catch_pgbench_bulk_export_redaction_and_history_failures() {
        let dir = TempWorkDir::create().expect("temp dir");
        let mut cfg = test_cfg(&dir.path);
        cfg.warmup_secs = 0;
        cfg.samples = 1;
        cfg.bulk_iterations = 2;

        // 1. pgbench failed tx & zero tx.
        let mut fail_tx = FakeRunner::healthy(1.0);
        fail_tx.pgbench_failed = 1;
        let err = execute_benchmark(
            &fail_tx,
            &cfg,
            Path::new("bench/capture"),
            Path::new("/f"),
            "a",
        )
        .expect_err("failed tx must fail");
        assert!(err.contains("failed transaction"), "{err}");

        let mut zero_tx = FakeRunner::healthy(1.0);
        zero_tx.pgbench_tx = 0;
        let err = execute_benchmark(
            &zero_tx,
            &cfg,
            Path::new("bench/capture"),
            Path::new("/f"),
            "a",
        )
        .expect_err("zero tx must fail");
        assert!(err.contains("processed 0 transactions"), "{err}");

        // 2. Capture mismatch scoped separately to pgbench, bulk INSERT, and bulk UPDATE.
        for (table, expected_sub) in [
            ("bench_up_cap", "captured row mismatch"),
            (
                "bench_bulk_cap",
                "10,000-row INSERT (comment) captured mismatch",
            ),
            (
                "bench_bulk_up_cap",
                "10,000-row UPDATE (comment) captured mismatch",
            ),
        ] {
            let mut r = FakeRunner::healthy(1.0);
            r.mismatch_table = Some(table);
            let err = execute_benchmark(&r, &cfg, Path::new("bench/capture"), Path::new("/f"), "a")
                .expect_err("mismatch must fail");
            assert!(err.contains(expected_sub), "table {table}: {err}");
        }

        // 3. Export event count mismatch, invalid elapsed, and 3 distinct redaction failure modes.
        let mut bad_count = FakeRunner::healthy(1.0);
        bad_count.export_events_override = Some(499);
        let err = execute_benchmark(
            &bad_count,
            &cfg,
            Path::new("bench/capture"),
            Path::new("/f"),
            "a",
        )
        .expect_err("export mismatch");
        assert!(err.contains("auditstream export mismatch"), "{err}");

        let mut bad_elapsed = FakeRunner::healthy(1.0);
        bad_elapsed.export_elapsed_override = Some(0.0);
        let err = execute_benchmark(
            &bad_elapsed,
            &cfg,
            Path::new("bench/capture"),
            Path::new("/f"),
            "a",
        )
        .expect_err("zero elapsed");
        assert!(err.contains("invalid export elapsed duration"), "{err}");

        for fixture in [
            RedactFixture::LeakSsnWithMask,
            RedactFixture::LeakEmailWithMask,
            RedactFixture::MissingMaskNoPii,
        ] {
            let mut r = FakeRunner::healthy(1.0);
            r.redact_fixture = fixture;
            let err = execute_benchmark(&r, &cfg, Path::new("bench/capture"), Path::new("/f"), "a")
                .expect_err("redaction check must fail");
            assert!(err.contains("redacted export verification failed"), "{err}");
        }

        // 4. Empty history stdout & TempBenchDb drop warning path.
        let mut empty_hist = FakeRunner::healthy(1.0);
        empty_hist.empty_history = true;
        empty_hist.fail_drop = true;
        let err = execute_benchmark(
            &empty_hist,
            &cfg,
            Path::new("bench/capture"),
            Path::new("/f"),
            "a",
        )
        .expect_err("empty history must fail");
        assert!(err.contains("returned 0 events"), "{err}");
    }

    #[test]
    fn cli_entry_helpers_and_shell_runner_cli_work_without_postgres() {
        assert_eq!(run(&["--help".to_owned()]), ExitCode::SUCCESS);
        assert_eq!(run(&["--not-a-flag".to_owned()]), ExitCode::FAILURE);

        let root = find_repo_root().expect("repo root");
        assert!(root.join("Cargo.toml").is_file());
        assert_ne!(git_short_commit(&root), "");

        let non_git = TempWorkDir::create().expect("temp dir");
        assert_eq!(git_short_commit(&non_git.path), "unknown");

        // TempBenchDb::create must reject invalid URI before running CREATE DATABASE.
        let untouched = FakeRunner::healthy(1.0);
        assert!(TempBenchDb::create(&untouched, "not-a-uri").is_err());
        assert_eq!(untouched.state.borrow().calls.len(), 0);

        // resolve_auditstream_bin canonicalizes non-canonical paths and rejects missing paths.
        let existing_file = non_git.path.join("fake-bin");
        fs::write(&existing_file, "").expect("touch fake bin");
        let sub_dir = non_git.path.join("sub");
        fs::create_dir(&sub_dir).expect("create sub dir");
        let non_canonical = sub_dir.join("..").join("fake-bin");
        let canonical_existing = fs::canonicalize(&existing_file).expect("canonical");
        assert_ne!(non_canonical, canonical_existing);
        let mut cfg = test_cfg(&non_git.path);
        cfg.auditstream_bin = Some(non_canonical);
        assert_eq!(
            resolve_auditstream_bin(&cfg, &root).expect("existing bin"),
            canonical_existing
        );
        cfg.auditstream_bin = Some(non_git.path.join("does-not-exist"));
        assert!(resolve_auditstream_bin(&cfg, &root).is_err());

        // apply_libpq_uri scrubs all ambient PG* env vars and moves URI password into PGPASSWORD.
        let mut cmd_no_pw = Command::new("psql");
        let no_pw_uri = "postgres://postgres@127.0.0.1:5432/postgres";
        assert_eq!(apply_libpq_uri(&mut cmd_no_pw, no_pw_uri), no_pw_uri);
        let no_pw_envs: Vec<(String, Option<String>)> = cmd_no_pw
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for var in AMBIENT_PG_ENV_VARS {
            assert!(
                no_pw_envs.contains(&(var.to_owned(), None)),
                "expected {var} to be removed"
            );
        }

        let mut cmd = Command::new("psql");
        let tok = "p%40ss";
        let uri = format!("postgres://postgres:{tok}@127.0.0.1:5432/postgres");
        let sanitized = apply_libpq_uri(&mut cmd, &uri);
        assert_eq!(sanitized, no_pw_uri);
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&("PGPASSWORD".to_owned(), Some("p@ss".to_owned()))));

        // Exercise ShellRunner::run_cli with cargo --version and a missing binary.
        let cargo_bin = PathBuf::from(env!("CARGO"));
        let ok = ShellRunner
            .run_cli(&cargo_bin, "", &non_git.path, &["--version"])
            .expect("cargo --version");
        assert!(ok.stdout.contains("cargo"));
        assert!(
            ShellRunner
                .run_cli(
                    &cargo_bin,
                    "",
                    &non_git.path,
                    &["--definitely-invalid-flag"]
                )
                .is_err()
        );
        assert!(
            ShellRunner
                .run_cli(&non_git.path.join("missing-binary"), "", &non_git.path, &[])
                .is_err()
        );
    }
}
