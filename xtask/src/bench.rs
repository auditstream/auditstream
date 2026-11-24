//! Capture benchmark runner (`cargo xtask bench`).
//!
//! Creates a fresh temporary database from `auditstream_pg`'s D2 + D3 generators,
//! seeds tables before triggers exist, runs the full `pgbench` + bulk-insert matrix
//! and lookup-index variants, asserts zero failed transactions and exact captured row
//! counts, and emits Markdown and JSON reports.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use auditstream_pg::ddl::{self, DEFAULT_SCHEMA, DdlOptions};
use auditstream_pg::triggers::{self, TableTriggerSpec, TriggerOptions};
use serde::Serialize;

const BULK_ROWS: u64 = 10_000;
const COMMENT_TAG: &str = "/*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidmlhX2NvbW1lbnQifQ'*/";

#[derive(Debug, Clone)]
struct BenchConfig {
    database_url: String,
    mode: String,
    clients: u32,
    jobs: u32,
    duration_secs: u32,
    bulk_iterations: u32,
    markdown_out: Option<PathBuf>,
    json_out: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
struct IndexVariant {
    id: &'static str,
    label: &'static str,
    create_sql: Option<&'static str>,
}

const INDEX_VARIANTS: [IndexVariant; 4] = [
    IndexVariant {
        id: "none",
        label: "None (no lookup index)",
        create_sql: None,
    },
    IndexVariant {
        id: "current",
        label: "Current: `(table_name, record_id, occurred_at)`",
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
    IndexVariant {
        id: "record_id_hash",
        label: "Partial hash: `USING hash (record_id) WHERE record_id IS NOT NULL`",
        create_sql: Some(
            "CREATE INDEX changes_lookup_idx ON auditstream.changes USING hash (record_id) WHERE record_id IS NOT NULL",
        ),
    },
];

#[derive(Debug, Serialize)]
struct BenchReport {
    postgres_version: String,
    git_commit: String,
    pgbench_settings: String,
    bulk_iterations: u32,
    workloads: Vec<WorkloadRow>,
    index_variants: Vec<IndexVariantRow>,
}

#[derive(Debug, Serialize)]
struct WorkloadRow {
    name: String,
    metric_unit: String,
    value: f64,
    us_per_row: f64,
    cost_pct: Option<f64>,
    cost_us_per_row: Option<f64>,
    rows_written: u64,
    rows_captured: u64,
}

#[derive(Debug, Serialize)]
struct IndexVariantRow {
    variant: String,
    label: String,
    update_comment_tps: f64,
    update_comment_cost_pct: f64,
    update_comment_cost_us: f64,
    update_setcfg_tps: f64,
    update_setcfg_cost_pct: f64,
    update_setcfg_cost_us: f64,
    bulk_insert_10k_ms: f64,
    bulk_insert_10k_cost_pct: f64,
    bulk_insert_10k_cost_us: f64,
}

#[derive(Debug)]
struct PgbenchResult {
    tx: u64,
    failed: u64,
    tps: f64,
}

struct TempBenchDb {
    admin_url: String,
    bench_url: String,
    db_name: String,
}

impl TempBenchDb {
    fn create(admin_url: &str) -> Result<Self, String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let db_name = format!("auditstream_bench_{}_{}", std::process::id(), ts);
        psql_exec(admin_url, &format!("CREATE DATABASE \"{db_name}\""))?;
        let bench_url = replace_db_name(admin_url, &db_name)?;
        Ok(Self {
            admin_url: admin_url.to_owned(),
            bench_url,
            db_name,
        })
    }
}

impl Drop for TempBenchDb {
    fn drop(&mut self) {
        let drop_sql = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.db_name);
        if let Err(err) = psql_exec(&self.admin_url, &drop_sql) {
            eprintln!(
                "xtask bench: warning: failed to drop {}: {err}",
                self.db_name
            );
        }
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

    let cfg = parse_args(args)?;
    let repo_root = find_repo_root()?;
    let bench_dir = repo_root.join("bench").join("capture");

    let temp_db = TempBenchDb::create(&cfg.database_url)?;
    let pg_version = psql_scalar(&temp_db.bench_url, "SELECT version()")?;
    let git_commit = git_short_commit(&repo_root);
    let pgbench_settings = format!(
        "-M {} -c {} -j {} -T {}",
        cfg.mode, cfg.clients, cfg.jobs, cfg.duration_secs
    );

    eprintln!("==> Setting up benchmark schema in {}", temp_db.db_name);
    setup_benchmark_schema(&temp_db.bench_url)?;

    eprintln!("==> Running core workloads ({pgbench_settings})");
    let (workloads, baselines) = run_core_workloads(&temp_db.bench_url, &bench_dir, &cfg)?;

    eprintln!("==> Measuring lookup index variants on worst-case UPDATE and 10k bulk INSERT");
    let index_variants = run_index_variants(&temp_db.bench_url, &bench_dir, &cfg, baselines)?;

    let report = BenchReport {
        postgres_version: pg_version,
        git_commit,
        pgbench_settings,
        bulk_iterations: cfg.bulk_iterations,
        workloads,
        index_variants,
    };

    let markdown = render_markdown(&report);
    let json = serde_json::to_string_pretty(&report)
        .map_err(|err| format!("failed to serialize JSON report: {err}"))?;

    if let Some(md_path) = &cfg.markdown_out {
        fs::write(md_path, &markdown)
            .map_err(|err| format!("failed to write {}: {err}", md_path.display()))?;
    }
    if let Some(json_path) = &cfg.json_out {
        fs::write(json_path, format!("{json}\n"))
            .map_err(|err| format!("failed to write {}: {err}", json_path.display()))?;
    }

    print!("{markdown}");
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Baselines {
    update_tps: f64,
    update_us_per_row: f64,
    bulk_insert_ms: f64,
    bulk_insert_us_per_row: f64,
}

fn tps_row(
    name: &str,
    measured: &PgbenchResult,
    baseline: Option<(f64, f64)>,
    captured: bool,
) -> WorkloadRow {
    let us_per_row = 1_000_000.0 / measured.tps;
    WorkloadRow {
        name: name.to_owned(),
        metric_unit: "tps".to_owned(),
        value: measured.tps,
        us_per_row,
        cost_pct: baseline.map(|(base_tps, _)| tps_cost_pct(base_tps, measured.tps)),
        cost_us_per_row: baseline.map(|(_, base_us)| us_per_row - base_us),
        rows_written: measured.tx,
        rows_captured: if captured { measured.tx } else { 0 },
    }
}

fn bulk_row(
    name: &str,
    millis: f64,
    iterations: u32,
    baseline: Option<(f64, f64)>,
    captured: bool,
) -> WorkloadRow {
    let us_per_row = millis * 1000.0 / f64_from_u64(BULK_ROWS);
    let rows = BULK_ROWS * u64::from(iterations);
    WorkloadRow {
        name: name.to_owned(),
        metric_unit: "ms/stmt".to_owned(),
        value: millis,
        us_per_row,
        cost_pct: baseline.map(|(base_ms, _)| ms_cost_pct(base_ms, millis)),
        cost_us_per_row: baseline.map(|(_, base_us)| us_per_row - base_us),
        rows_written: rows,
        rows_captured: if captured { rows } else { 0 },
    }
}

fn run_core_workloads(
    db_url: &str,
    bench_dir: &Path,
    cfg: &BenchConfig,
) -> Result<(Vec<WorkloadRow>, Baselines), String> {
    let up_base = run_pgbench(db_url, &bench_dir.join("update_baseline.sql"), cfg)?;
    let up_base_micros = 1_000_000.0 / up_base.tps;

    let up_comment = run_captured_pgbench(
        db_url,
        &bench_dir.join("update_comment.sql"),
        cfg,
        "bench_up_cap",
        "comment",
    )?;
    let up_setcfg = run_captured_pgbench(
        db_url,
        &bench_dir.join("update_setcfg.sql"),
        cfg,
        "bench_up_cap",
        "set_config",
    )?;

    let ins_base = run_pgbench(db_url, &bench_dir.join("insert_baseline.sql"), cfg)?;
    let ins_base_micros = 1_000_000.0 / ins_base.tps;

    let ins_cap = run_captured_pgbench(
        db_url,
        &bench_dir.join("insert_captured.sql"),
        cfg,
        "bench_ins_cap",
        "comment",
    )?;

    let bulk_base_millis = run_bulk_insert_baseline(db_url, cfg.bulk_iterations)?;
    let bulk_base_micros = bulk_base_millis * 1000.0 / f64_from_u64(BULK_ROWS);
    let bulk_cap_millis = run_bulk_insert_captured(db_url, cfg.bulk_iterations)?;

    let workloads = vec![
        tps_row("Single-row UPDATE (baseline)", &up_base, None, false),
        tps_row(
            "Single-row UPDATE (comment)",
            &up_comment,
            Some((up_base.tps, up_base_micros)),
            true,
        ),
        tps_row(
            "Single-row UPDATE (set_config)",
            &up_setcfg,
            Some((up_base.tps, up_base_micros)),
            true,
        ),
        tps_row("Single-row INSERT (baseline)", &ins_base, None, false),
        tps_row(
            "Single-row INSERT (comment)",
            &ins_cap,
            Some((ins_base.tps, ins_base_micros)),
            true,
        ),
        bulk_row(
            "10,000-row INSERT (baseline)",
            bulk_base_millis,
            cfg.bulk_iterations,
            None,
            false,
        ),
        bulk_row(
            "10,000-row INSERT (comment)",
            bulk_cap_millis,
            cfg.bulk_iterations,
            Some((bulk_base_millis, bulk_base_micros)),
            true,
        ),
    ];

    Ok((
        workloads,
        Baselines {
            update_tps: up_base.tps,
            update_us_per_row: up_base_micros,
            bulk_insert_ms: bulk_base_millis,
            bulk_insert_us_per_row: bulk_base_micros,
        },
    ))
}

fn run_index_variants(
    db_url: &str,
    bench_dir: &Path,
    cfg: &BenchConfig,
    baselines: Baselines,
) -> Result<Vec<IndexVariantRow>, String> {
    let mut index_variants = Vec::with_capacity(INDEX_VARIANTS.len());
    for variant in INDEX_VARIANTS {
        eprintln!("  -> variant `{}`", variant.id);
        apply_index_variant(db_url, variant)?;

        let var_comment = run_captured_pgbench(
            db_url,
            &bench_dir.join("update_comment.sql"),
            cfg,
            "bench_up_cap",
            "comment",
        )?;
        let var_setcfg = run_captured_pgbench(
            db_url,
            &bench_dir.join("update_setcfg.sql"),
            cfg,
            "bench_up_cap",
            "set_config",
        )?;
        let var_bulk_millis = run_bulk_insert_captured(db_url, cfg.bulk_iterations)?;
        let var_bulk_micros = var_bulk_millis * 1000.0 / f64_from_u64(BULK_ROWS);

        index_variants.push(IndexVariantRow {
            variant: variant.id.to_owned(),
            label: variant.label.to_owned(),
            update_comment_tps: var_comment.tps,
            update_comment_cost_pct: tps_cost_pct(baselines.update_tps, var_comment.tps),
            update_comment_cost_us: (1_000_000.0 / var_comment.tps) - baselines.update_us_per_row,
            update_setcfg_tps: var_setcfg.tps,
            update_setcfg_cost_pct: tps_cost_pct(baselines.update_tps, var_setcfg.tps),
            update_setcfg_cost_us: (1_000_000.0 / var_setcfg.tps) - baselines.update_us_per_row,
            bulk_insert_10k_ms: var_bulk_millis,
            bulk_insert_10k_cost_pct: ms_cost_pct(baselines.bulk_insert_ms, var_bulk_millis),
            bulk_insert_10k_cost_us: var_bulk_micros - baselines.bulk_insert_us_per_row,
        });
    }
    Ok(index_variants)
}

fn setup_benchmark_schema(db_url: &str) -> Result<(), String> {
    // Seed tables BEFORE triggers exist so seeding is never audited.
    let seed_sql = "\
        CREATE TABLE public.bench_up_plain (id int PRIMARY KEY, v int NOT NULL DEFAULT 0);\n\
        CREATE TABLE public.bench_up_cap (id int PRIMARY KEY, v int NOT NULL DEFAULT 0);\n\
        CREATE TABLE public.bench_ins_plain (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        CREATE TABLE public.bench_ins_cap (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        CREATE TABLE public.bench_bulk_plain (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        CREATE TABLE public.bench_bulk_cap (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        INSERT INTO public.bench_up_plain (id, v) SELECT g, 0 FROM generate_series(1, 10000) AS g;\n\
        INSERT INTO public.bench_up_cap (id, v) SELECT g, 0 FROM generate_series(1, 10000) AS g;\n\
        ANALYZE public.bench_up_plain, public.bench_up_cap;\n";
    psql_exec(db_url, seed_sql)?;

    let mut ddl_stmts = ddl::generate(&DdlOptions::new(DEFAULT_SCHEMA, vec![]));
    let role_cleanup = ddl_stmts.pop();

    let tables = vec![
        TableTriggerSpec::new("public", "bench_up_cap", vec!["id".to_owned()], vec![]),
        TableTriggerSpec::new("public", "bench_ins_cap", vec!["id".to_owned()], vec![]),
        TableTriggerSpec::new("public", "bench_bulk_cap", vec!["id".to_owned()], vec![]),
    ];
    let trigger_stmts = triggers::generate(&TriggerOptions::new(DEFAULT_SCHEMA, tables));

    let mut combined = String::new();
    for stmt in ddl_stmts
        .into_iter()
        .chain(trigger_stmts)
        .chain(role_cleanup)
    {
        combined.push_str(&stmt);
        combined.push_str(";\n");
    }

    psql_exec(db_url, &combined)
}

fn apply_index_variant(db_url: &str, variant: IndexVariant) -> Result<(), String> {
    let mut sql = String::from(
        "TRUNCATE auditstream.changes;\n\
         DROP INDEX IF EXISTS auditstream.changes_lookup_idx;\n",
    );
    if let Some(create_sql) = variant.create_sql {
        sql.push_str(create_sql);
        sql.push_str(";\n");
    }
    sql.push_str("ANALYZE auditstream.changes;\n");
    psql_exec(db_url, &sql)?;
    settle_workload_state(db_url)
}

/// Vacuums benchmark tables and issues a best-effort `CHECKPOINT` between timed windows so
/// dead tuples, autovacuum bursts, and pending WAL flushes do not spill across measurements.
fn settle_workload_state(db_url: &str) -> Result<(), String> {
    psql_exec(
        db_url,
        "VACUUM ANALYZE public.bench_up_plain, public.bench_up_cap, public.bench_ins_plain, public.bench_ins_cap;\n",
    )?;
    // `CHECKPOINT` requires superuser or `pg_checkpoint`; skip silently on managed roles.
    let _ = psql_exec(db_url, "CHECKPOINT;\n");
    Ok(())
}

fn run_pgbench(db_url: &str, script: &Path, cfg: &BenchConfig) -> Result<PgbenchResult, String> {
    settle_workload_state(db_url)?;
    let output = Command::new("pgbench")
        .args([
            "-n",
            "-M",
            &cfg.mode,
            "-c",
            &cfg.clients.to_string(),
            "-j",
            &cfg.jobs.to_string(),
            "-T",
            &cfg.duration_secs.to_string(),
            "-f",
        ])
        .arg(script)
        .arg(db_url)
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

    let parsed = parse_pgbench_output(&combined)?;
    if parsed.failed != 0 {
        return Err(format!(
            "pgbench reported {} failed transaction(s) for {}:\n{combined}",
            parsed.failed,
            script.display()
        ));
    }
    if parsed.tx == 0 || parsed.tps <= 0.0 {
        return Err(format!(
            "pgbench processed 0 transactions for {}:\n{combined}",
            script.display()
        ));
    }
    Ok(parsed)
}

fn run_captured_pgbench(
    db_url: &str,
    script: &Path,
    cfg: &BenchConfig,
    table_name: &str,
    expected_actor_source: &str,
) -> Result<PgbenchResult, String> {
    let start_id = max_change_id(db_url)?;
    let res = run_pgbench(db_url, script, cfg)?;

    let captured: u64 = psql_scalar(
        db_url,
        &format!(
            "SELECT count(*) FROM auditstream.changes \
              WHERE id > {start_id} \
                AND table_name = '{table_name}' \
                AND actor_source = '{expected_actor_source}'"
        ),
    )?
    .parse()
    .map_err(|err| format!("invalid captured row count: {err}"))?;

    if captured != res.tx {
        return Err(format!(
            "captured row mismatch for {}: expected {} rows with actor_source='{expected_actor_source}', got {captured}",
            script.display(),
            res.tx
        ));
    }
    Ok(res)
}

fn run_bulk_insert_baseline(db_url: &str, iterations: u32) -> Result<f64, String> {
    let mut total_ms = 0.0;
    for _ in 0..iterations {
        let start = Instant::now();
        psql_exec(
            db_url,
            &format!(
                "INSERT INTO public.bench_bulk_plain (v) \
                 SELECT g FROM generate_series(1, {BULK_ROWS}) AS g;"
            ),
        )?;
        total_ms += start.elapsed().as_secs_f64() * 1000.0;
    }
    Ok(total_ms / f64::from(iterations))
}

fn run_bulk_insert_captured(db_url: &str, iterations: u32) -> Result<f64, String> {
    let start_id = max_change_id(db_url)?;
    let mut total_ms = 0.0;
    for _ in 0..iterations {
        let start = Instant::now();
        psql_exec(
            db_url,
            &format!(
                "INSERT INTO public.bench_bulk_cap (v) \
                 SELECT g FROM generate_series(1, {BULK_ROWS}) AS g {COMMENT_TAG};"
            ),
        )?;
        total_ms += start.elapsed().as_secs_f64() * 1000.0;
    }

    let expected = BULK_ROWS * u64::from(iterations);
    let captured: u64 = psql_scalar(
        db_url,
        &format!(
            "SELECT count(*) FROM auditstream.changes \
              WHERE id > {start_id} \
                AND table_name = 'bench_bulk_cap' \
                AND actor_source = 'comment'"
        ),
    )?
    .parse()
    .map_err(|err| format!("invalid bulk captured count: {err}"))?;

    if captured != expected {
        return Err(format!(
            "bulk 10k insert captured mismatch: expected {expected} rows, got {captured}"
        ));
    }

    Ok(total_ms / f64::from(iterations))
}

fn max_change_id(db_url: &str) -> Result<u64, String> {
    psql_scalar(
        db_url,
        "SELECT coalesce(max(id), 0) FROM auditstream.changes",
    )?
    .parse()
    .map_err(|err| format!("invalid max(id): {err}"))
}

fn psql_exec(db_url: &str, sql: &str) -> Result<(), String> {
    let mut child = Command::new("psql")
        .args([db_url, "-v", "ON_ERROR_STOP=1", "-q", "-X"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn psql: {err}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(sql.as_bytes())
            .map_err(|err| format!("failed to write SQL to psql stdin: {err}"))?;
    }

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
    Ok(())
}

fn psql_scalar(db_url: &str, sql: &str) -> Result<String, String> {
    let output = Command::new("psql")
        .args([db_url, "-v", "ON_ERROR_STOP=1", "-X", "-Atc", sql])
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

fn parse_pgbench_output(text: &str) -> Result<PgbenchResult, String> {
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

fn tps_cost_pct(baseline_tps: f64, measured_tps: f64) -> f64 {
    ((baseline_tps - measured_tps) / baseline_tps) * 100.0
}

fn ms_cost_pct(baseline_ms: f64, measured_ms: f64) -> f64 {
    ((measured_ms - baseline_ms) / baseline_ms) * 100.0
}

#[allow(clippy::cast_precision_loss)]
fn f64_from_u64(value: u64) -> f64 {
    value as f64
}

fn render_markdown(report: &BenchReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Auditstream Capture Benchmark Report\n");
    let _ = writeln!(out, "- **PostgreSQL:** `{}`", report.postgres_version);
    let _ = writeln!(out, "- **Commit:** `{}`", report.git_commit);
    let _ = writeln!(out, "- **pgbench:** `{}`", report.pgbench_settings);
    let _ = writeln!(
        out,
        "- **10,000-row bulk insert iterations:** `{}`\n",
        report.bulk_iterations
    );

    let _ = writeln!(out, "## Core Workloads (Default D2 Schema)\n");
    let _ = writeln!(
        out,
        "| Workload | Throughput / Latency | µs / row | Overhead (%) | Extra µs / row | Rows Written | Rows Captured |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
    for row in &report.workloads {
        let metric = format!("{:.1} {}", row.value, row.metric_unit);
        let pct = row
            .cost_pct
            .map_or_else(|| "baseline".to_owned(), |v| format!("{v:+.1}%"));
        let delta_us = row
            .cost_us_per_row
            .map_or_else(|| "—".to_owned(), |v| format!("{v:+.1} µs"));
        let _ = writeln!(
            out,
            "| {} | {} | {:.1} | {} | {} | {} | {} |",
            row.name, metric, row.us_per_row, pct, delta_us, row.rows_written, row.rows_captured
        );
    }

    let _ = writeln!(out, "\n## Lookup Index Variants\n");
    let _ = writeln!(
        out,
        "| Variant | UPDATE comment (tps / cost) | UPDATE `set_config` (tps / cost) | 10k INSERT (ms / cost) |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: |");
    for row in &report.index_variants {
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
    out
}

fn replace_db_name(url: &str, db_name: &str) -> Result<String, String> {
    let scheme_end = url
        .find("://")
        .ok_or_else(|| format!("DATABASE_URL `{url}` is missing a scheme"))?;
    let rest = &url[scheme_end + 3..];
    let (before_query, query_suffix) = match rest.find(['?', '#']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let slash = before_query.find('/').unwrap_or(before_query.len());
    let authority = &before_query[..slash];
    Ok(format!(
        "{}{authority}/{db_name}{query_suffix}",
        &url[..scheme_end + 3]
    ))
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

fn parse_args(args: &[String]) -> Result<BenchConfig, String> {
    let mut quick = false;
    let mut duration_secs: Option<u32> = None;
    let mut clients: u32 = 8;
    let mut jobs: u32 = 4;
    let mut mode = "prepared".to_owned();
    let mut bulk_iterations: Option<u32> = None;
    let mut database_url: Option<String> = None;
    let mut markdown_out: Option<PathBuf> = None;
    let mut json_out: Option<PathBuf> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--quick" => quick = true,
            "--duration" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--duration requires a seconds value".to_owned())?;
                duration_secs = Some(
                    raw.parse()
                        .map_err(|err| format!("invalid --duration `{raw}`: {err}"))?,
                );
            }
            "--clients" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--clients requires an integer".to_owned())?;
                clients = raw
                    .parse()
                    .map_err(|err| format!("invalid --clients `{raw}`: {err}"))?;
            }
            "--jobs" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--jobs requires an integer".to_owned())?;
                jobs = raw
                    .parse()
                    .map_err(|err| format!("invalid --jobs `{raw}`: {err}"))?;
            }
            "--mode" => {
                mode.clone_from(
                    iter.next()
                        .ok_or_else(|| "--mode requires simple|extended|prepared".to_owned())?,
                );
            }
            "--bulk-runs" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--bulk-runs requires an integer".to_owned())?;
                bulk_iterations = Some(
                    raw.parse()
                        .map_err(|err| format!("invalid --bulk-runs `{raw}`: {err}"))?,
                );
            }
            "--database-url" => {
                database_url = Some(
                    iter.next()
                        .ok_or_else(|| "--database-url requires a URL".to_owned())?
                        .clone(),
                );
            }
            "--markdown-out" => {
                markdown_out =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        "--markdown-out requires a file path".to_owned()
                    })?));
            }
            "--json-out" => {
                json_out =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        "--json-out requires a file path".to_owned()
                    })?));
            }
            other => return Err(format!("unknown bench flag `{other}`")),
        }
    }

    let db_url = database_url
        .or_else(|| env::var("DATABASE_URL").ok().filter(|v| !v.is_empty()))
        .ok_or_else(|| "DATABASE_URL (or --database-url) must be set".to_owned())?;

    let final_duration = duration_secs.unwrap_or(if quick { 5 } else { 60 });
    let final_bulk = bulk_iterations.unwrap_or(if quick { 3 } else { 5 });

    Ok(BenchConfig {
        database_url: db_url,
        mode,
        clients,
        jobs,
        duration_secs: final_duration,
        bulk_iterations: final_bulk,
        markdown_out,
        json_out,
    })
}

fn print_bench_help() {
    println!("cargo xtask bench [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --quick                 Run every workload for 5s (local verification)");
    println!("  --duration <secs>       pgbench duration per workload (default: 60, --quick: 5)");
    println!("  --clients <n>           pgbench clients (default: 8)");
    println!("  --jobs <n>              pgbench worker threads (default: 4)");
    println!(
        "  --mode <mode>           pgbench query mode: simple|extended|prepared (default: prepared)"
    );
    println!("  --bulk-runs <n>         10k bulk INSERT iterations (default: 5, --quick: 3)");
    println!("  --database-url <url>    Admin PostgreSQL URL (default: $DATABASE_URL)");
    println!("  --markdown-out <path>   Write Markdown report to file");
    println!("  --json-out <path>       Write JSON report to file");
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parsed.tx, 33150);
        assert_eq!(parsed.failed, 0);
        assert!((parsed.tps - 6630.4218).abs() < 0.001);
    }

    #[test]
    fn replace_db_name_preserves_authority_and_query_string() {
        assert_eq!(
            replace_db_name(
                "postgres://postgres:pg@127.0.0.1:5432/postgres?sslmode=disable",
                "auditstream_bench_1"
            )
            .expect("replace"),
            "postgres://postgres:pg@127.0.0.1:5432/auditstream_bench_1?sslmode=disable"
        );
    }
}
