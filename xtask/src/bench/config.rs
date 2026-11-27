//! CLI argument parsing, SQL/config builders, and path helpers for `xtask bench`.

use std::env;
use std::path::{Path, PathBuf};

use auditstream_pg::ddl::{DEFAULT_SCHEMA, DdlOptions};
use auditstream_pg::migrations;
use auditstream_pg::triggers::TableTriggerSpec;

pub(super) const BULK_ROWS: u64 = 10_000;
pub(super) const COMMENT_TAG: &str =
    "/*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidmlhX2NvbW1lbnQifQ'*/";
pub(super) const DEFAULT_MAX_REGRESSION_PCT: f64 = 15.0;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct BenchConfig {
    pub(super) database_url: String,
    pub(super) mode: String,
    pub(super) clients: u32,
    pub(super) jobs: u32,
    pub(super) duration_secs: u32,
    pub(super) warmup_secs: u32,
    pub(super) samples: u32,
    pub(super) bulk_iterations: u32,
    pub(super) export_rows: u64,
    pub(super) history_runs: u32,
    pub(super) auditstream_bin: Option<PathBuf>,
    pub(super) markdown_out: Option<PathBuf>,
    pub(super) json_out: Option<PathBuf>,
    pub(super) compare_baseline: Option<PathBuf>,
    pub(super) max_regression_pct: f64,
}

#[derive(Default)]
struct RawArgs {
    quick: bool,
    duration_secs: Option<u32>,
    warmup_secs: Option<u32>,
    samples: Option<u32>,
    clients: Option<u32>,
    jobs: Option<u32>,
    mode: Option<String>,
    bulk_iterations: Option<u32>,
    export_rows: Option<u64>,
    history_runs: Option<u32>,
    auditstream_bin: Option<PathBuf>,
    database_url: Option<String>,
    markdown_out: Option<PathBuf>,
    json_out: Option<PathBuf>,
    compare_baseline: Option<PathBuf>,
    max_regression_pct: Option<f64>,
}

pub(super) fn parse_args(
    args: &[String],
    env_db_url: Option<String>,
) -> Result<BenchConfig, String> {
    let raw = scan_args(args)?;
    finalize_config(raw, env_db_url)
}

fn scan_args(args: &[String]) -> Result<RawArgs, String> {
    let mut raw = RawArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--quick" => raw.quick = true,
            "--duration" => raw.duration_secs = Some(parse_u32_flag(&mut iter, "--duration")?),
            "--warmup-secs" => raw.warmup_secs = Some(parse_u32_flag(&mut iter, "--warmup-secs")?),
            "--samples" => raw.samples = Some(parse_u32_flag(&mut iter, "--samples")?),
            "--clients" => raw.clients = Some(parse_u32_flag(&mut iter, "--clients")?),
            "--jobs" => raw.jobs = Some(parse_u32_flag(&mut iter, "--jobs")?),
            "--bulk-runs" => {
                raw.bulk_iterations = Some(parse_u32_flag(&mut iter, "--bulk-runs")?);
            }
            "--export-rows" => raw.export_rows = Some(parse_u64_flag(&mut iter, "--export-rows")?),
            "--history-runs" => {
                raw.history_runs = Some(parse_u32_flag(&mut iter, "--history-runs")?);
            }
            "--mode" => raw.mode = Some(take_str(&mut iter, "--mode")?.to_owned()),
            "--database-url" => {
                raw.database_url = Some(take_str(&mut iter, "--database-url")?.to_owned());
            }
            "--auditstream-bin" => {
                raw.auditstream_bin =
                    Some(PathBuf::from(take_str(&mut iter, "--auditstream-bin")?));
            }
            "--markdown-out" => {
                raw.markdown_out = Some(PathBuf::from(take_str(&mut iter, "--markdown-out")?));
            }
            "--json-out" => raw.json_out = Some(PathBuf::from(take_str(&mut iter, "--json-out")?)),
            "--compare" => {
                raw.compare_baseline = Some(PathBuf::from(take_str(&mut iter, "--compare")?));
            }
            "--max-regression-pct" => {
                let s = take_str(&mut iter, "--max-regression-pct")?;
                let pct: f64 = s
                    .parse()
                    .map_err(|err| format!("invalid --max-regression-pct `{s}`: {err}"))?;
                raw.max_regression_pct = Some(pct);
            }
            other => return Err(format!("unknown bench flag `{other}`")),
        }
    }
    Ok(raw)
}

fn finalize_config(raw: RawArgs, env_db_url: Option<String>) -> Result<BenchConfig, String> {
    let database_url = raw
        .database_url
        .filter(|v| !v.is_empty())
        .or_else(|| env_db_url.filter(|v| !v.is_empty()))
        .ok_or_else(|| "DATABASE_URL (or --database-url) must be set".to_owned())?;

    let mode = raw.mode.unwrap_or_else(|| "prepared".to_owned());
    validate_mode(&mode)?;

    let (def_dur, def_bulk, def_export, def_hist, def_warmup) = if raw.quick {
        (5, 3, 5_000, 20, 1)
    } else {
        (60, 5, 25_000, 50, 2)
    };

    let duration_secs = raw.duration_secs.unwrap_or(def_dur);
    let clients = raw.clients.unwrap_or(8);
    let jobs = raw.jobs.unwrap_or_else(|| 4.min(clients));
    let samples = raw.samples.unwrap_or(3);
    let bulk_iterations = raw.bulk_iterations.unwrap_or(def_bulk);
    let export_rows = raw.export_rows.unwrap_or(def_export);
    let history_runs = raw.history_runs.unwrap_or(def_hist);
    let warmup_secs = raw
        .warmup_secs
        .unwrap_or_else(|| def_warmup.min(duration_secs));
    let max_regression_pct = raw.max_regression_pct.unwrap_or(DEFAULT_MAX_REGRESSION_PCT);

    let cfg = BenchConfig {
        database_url,
        mode,
        clients,
        jobs,
        duration_secs,
        warmup_secs,
        samples,
        bulk_iterations,
        export_rows,
        history_runs,
        auditstream_bin: raw.auditstream_bin,
        markdown_out: raw.markdown_out,
        json_out: raw.json_out,
        compare_baseline: raw.compare_baseline,
        max_regression_pct,
    };
    validate_numeric_bounds(&cfg)?;
    Ok(cfg)
}

fn validate_mode(mode: &str) -> Result<(), String> {
    match mode {
        "simple" | "extended" | "prepared" => Ok(()),
        other => Err(format!(
            "invalid --mode `{other}` (expected simple, extended, or prepared)"
        )),
    }
}

fn validate_numeric_bounds(cfg: &BenchConfig) -> Result<(), String> {
    validate_positive_u32("--duration", cfg.duration_secs)?;
    validate_positive_u32("--clients", cfg.clients)?;
    validate_positive_u32("--jobs", cfg.jobs)?;
    if cfg.jobs > cfg.clients {
        return Err(format!(
            "--jobs ({}) cannot exceed --clients ({})",
            cfg.jobs, cfg.clients
        ));
    }
    validate_positive_u32("--samples", cfg.samples)?;
    validate_positive_u32("--bulk-runs", cfg.bulk_iterations)?;
    if cfg.export_rows == 0 {
        return Err("--export-rows must be at least 1".to_owned());
    }
    validate_positive_u32("--history-runs", cfg.history_runs)?;
    if !cfg.max_regression_pct.is_finite() || cfg.max_regression_pct < 0.0 {
        return Err(format!(
            "--max-regression-pct must be finite and >= 0, got {}",
            cfg.max_regression_pct
        ));
    }
    Ok(())
}

fn take_str<'a>(iter: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a str, String> {
    iter.next()
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_u32_flag(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<u32, String> {
    let raw = take_str(iter, flag)?;
    raw.parse()
        .map_err(|err| format!("invalid {flag} `{raw}`: {err}"))
}

fn parse_u64_flag(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<u64, String> {
    let raw = take_str(iter, flag)?;
    raw.parse()
        .map_err(|err| format!("invalid {flag} `{raw}`: {err}"))
}

fn validate_positive_u32(flag: &str, value: u32) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{flag} must be at least 1"));
    }
    Ok(())
}

pub(super) fn build_setup_schema_sql() -> Result<String, String> {
    let tables = vec![
        TableTriggerSpec::new("public", "bench_up_cap", vec!["id".to_owned()], vec![]),
        TableTriggerSpec::new("public", "bench_ins_cap", vec!["id".to_owned()], vec![]),
        TableTriggerSpec::new("public", "bench_bulk_cap", vec!["id".to_owned()], vec![]),
        TableTriggerSpec::new("public", "bench_bulk_up_cap", vec!["id".to_owned()], vec![]),
    ];
    migrations::render_install_sql(&DdlOptions::new(DEFAULT_SCHEMA, vec![]), &tables)
        .map_err(|err| format!("failed to render benchmark schema SQL: {err}"))
}

pub(super) fn build_seed_tables_sql() -> String {
    format!(
        "\
        CREATE TABLE public.bench_up_plain (id int PRIMARY KEY, v int NOT NULL DEFAULT 0);\n\
        CREATE TABLE public.bench_up_cap (id int PRIMARY KEY, v int NOT NULL DEFAULT 0);\n\
        CREATE TABLE public.bench_ins_plain (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        CREATE TABLE public.bench_ins_cap (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        CREATE TABLE public.bench_bulk_plain (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        CREATE TABLE public.bench_bulk_cap (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);\n\
        CREATE TABLE public.bench_bulk_up_plain (id int PRIMARY KEY, v int NOT NULL DEFAULT 0);\n\
        CREATE TABLE public.bench_bulk_up_cap (id int PRIMARY KEY, v int NOT NULL DEFAULT 0);\n\
        INSERT INTO public.bench_up_plain (id, v) SELECT g, 0 FROM generate_series(1, {BULK_ROWS}) AS g;\n\
        INSERT INTO public.bench_up_cap (id, v) SELECT g, 0 FROM generate_series(1, {BULK_ROWS}) AS g;\n\
        INSERT INTO public.bench_bulk_up_plain (id, v) SELECT g, 0 FROM generate_series(1, {BULK_ROWS}) AS g;\n\
        INSERT INTO public.bench_bulk_up_cap (id, v) SELECT g, 0 FROM generate_series(1, {BULK_ROWS}) AS g;\n\
        ANALYZE public.bench_up_plain, public.bench_up_cap, public.bench_bulk_up_plain, public.bench_bulk_up_cap;\n"
    )
}

/// Generates deterministic SQL seeding `rows` history events inside the current UTC monthly
/// partition for the `auditstream export` and `auditstream history` benchmarks.
pub(super) fn build_seed_changes_sql(rows: u64) -> String {
    format!(
        "\
        TRUNCATE auditstream.changes;\n\
        INSERT INTO auditstream.changes (\n\
          occurred_at, source, schema_name, table_name, op,\n\
          record_id, before, after, changed, actor, context, actor_source\n\
        )\n\
        SELECT\n\
          (date_trunc('month', clock_timestamp() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC')\n\
            + ((g % 86400) * interval '1 second'),\n\
          'db', 'public', 'invoice', 'UPDATE',\n\
          jsonb_build_object('id', g),\n\
          jsonb_build_object('id', g, 'status', 'open', 'ssn', '123-45-6789', 'email', 'u' || g || '@example.com', 'total', 12345.67),\n\
          jsonb_build_object('id', g, 'status', 'paid', 'ssn', '123-45-6789', 'email', 'u' || g || '@example.com', 'total', 12345.67),\n\
          ARRAY['status'],\n\
          '{{\"type\":\"user\",\"id\":\"usr_bench\",\"tenant\":\"org_1\"}}'::jsonb,\n\
          '{{\"requestId\":\"req_bench\",\"source\":\"api\"}}'::jsonb,\n\
          'comment'\n\
        FROM generate_series(1, {rows}) AS g;\n\
        ANALYZE auditstream.changes;\n"
    )
}

pub(super) fn build_export_toml(redacted: bool) -> String {
    let mut cfg = String::from("schema = \"auditstream\"\n\n[export]\nbatch_size = 1000\n");
    if redacted {
        cfg.push_str(
            "\n[[export.redaction.rules]]\n\
             table = \"public.invoice\"\n\
             columns = [\"ssn\", \"email\"]\n\
             mode = \"mask\"\n",
        );
    }
    cfg
}

pub(super) fn replace_db_name(url: &str, db_name: &str) -> Result<String, String> {
    let scheme_end = url.find("://").ok_or_else(|| {
        "DATABASE_URL must use a URI scheme such as postgres:// or postgresql://".to_owned()
    })?;
    let rest = &url[scheme_end + 3..];
    let (before_query, query_suffix) = match rest.find(['?', '#']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    if query_contains_dbname(query_suffix) {
        return Err(
            "DATABASE_URL query string must not contain `dbname` (it would override the temporary benchmark database)"
                .to_owned(),
        );
    }
    let slash = before_query.find('/').unwrap_or(before_query.len());
    let authority = &before_query[..slash];
    Ok(format!(
        "{}{authority}/{db_name}{query_suffix}",
        &url[..scheme_end + 3]
    ))
}

fn query_contains_dbname(query_suffix: &str) -> bool {
    let Some(after_q) = query_suffix.strip_prefix('?') else {
        return false;
    };
    let query = after_q.split('#').next().unwrap_or("");
    query.split('&').any(|pair| {
        let key = pair.split('=').next().unwrap_or("");
        key.eq_ignore_ascii_case("dbname")
    })
}

/// Splits an embedded URI password (from `user:password@` or query `?password=...`) out of a
/// PostgreSQL connection URL so `psql` and `pgbench` receive it via `PGPASSWORD` instead of argv.
pub(super) fn split_uri_password(url: &str) -> (String, Option<String>) {
    let Some(scheme_end) = url.find("://") else {
        return (url.to_owned(), None);
    };
    let scheme = &url[..scheme_end + 3];
    let rest = &url[scheme_end + 3..];
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let after_auth = &rest[auth_end..];

    let (clean_auth, userinfo_pw) = strip_userinfo_password(authority);
    let (clean_after, query_pw) = strip_query_password(after_auth);
    let password = query_pw.or(userinfo_pw);
    (format!("{scheme}{clean_auth}{clean_after}"), password)
}

fn strip_userinfo_password(authority: &str) -> (String, Option<String>) {
    let Some(at_pos) = authority.rfind('@') else {
        return (authority.to_owned(), None);
    };
    let userinfo = &authority[..at_pos];
    let Some(colon_pos) = userinfo.find(':') else {
        return (authority.to_owned(), None);
    };
    let user = &userinfo[..colon_pos];
    let raw_pw = &userinfo[colon_pos + 1..];
    let host = &authority[at_pos + 1..];
    (format!("{user}@{host}"), Some(percent_decode(raw_pw)))
}

fn strip_query_password(after_auth: &str) -> (String, Option<String>) {
    let Some(q_pos) = after_auth.find('?') else {
        return (after_auth.to_owned(), None);
    };
    let path = &after_auth[..q_pos];
    let after_q = &after_auth[q_pos + 1..];
    let (query, frag) = match after_q.find('#') {
        Some(idx) => (&after_q[..idx], &after_q[idx..]),
        None => (after_q, ""),
    };

    let mut kept = Vec::new();
    let mut found_pw: Option<String> = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k.eq_ignore_ascii_case("password")
        {
            found_pw = Some(percent_decode(v));
            continue;
        }
        if !pair.is_empty() {
            kept.push(pair);
        }
    }

    if found_pw.is_none() {
        return (after_auth.to_owned(), None);
    }
    let clean_query = if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    };
    (format!("{path}{clean_query}{frag}"), found_pw)
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(hi) = hex_nibble(bytes[i + 1])
            && let Some(lo) = hex_nibble(bytes[i + 2])
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(super) fn release_auditstream_bin(
    repo_root: &Path,
    cargo_target_dir: Option<&Path>,
) -> PathBuf {
    let target_dir =
        cargo_target_dir.map_or_else(|| repo_root.join("target"), |dir| repo_root.join(dir));
    target_dir
        .join("release")
        .join(format!("auditstream{}", env::consts::EXE_SUFFIX))
}

pub(super) fn print_bench_help() {
    println!("cargo xtask bench [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --quick                     Run shorter local verification defaults");
    println!("  --duration <secs>           pgbench duration per sample (default: 60, --quick: 5)");
    println!(
        "  --warmup-secs <secs>        Warmup duration before timing, 0 to skip (default: min(2, duration), --quick: 1)"
    );
    println!("  --samples <n>               Timed samples per workload for median (default: 3)");
    println!("  --clients <n>               pgbench clients (default: 8)");
    println!("  --jobs <n>                  pgbench worker threads (default: min(4, clients))");
    println!(
        "  --mode <mode>               pgbench query mode: simple|extended|prepared (default: prepared)"
    );
    println!(
        "  --bulk-runs <n>             10k bulk INSERT/UPDATE iterations per sample (default: 5, --quick: 3)"
    );
    println!(
        "  --export-rows <n>           Seeded rows for export/history benchmarks (default: 25000, --quick: 5000)"
    );
    println!(
        "  --history-runs <n>          Point-in-time history queries per sample (default: 50, --quick: 20)"
    );
    println!(
        "  --auditstream-bin <path>    Path to auditstream CLI binary (default: builds release binary)"
    );
    println!("  --database-url <url>        Admin PostgreSQL URL (default: $DATABASE_URL)");
    println!("  --markdown-out <path>       Write Markdown report to file");
    println!("  --json-out <path>           Write JSON report to file");
    println!("  --compare <path>            Compare against a baseline JSON report");
    println!(
        "  --max-regression-pct <pct>  Fail if any tracked metric regresses above PCT (default: 15.0)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parse_args_defaults_and_quick_and_explicit_overrides() {
        let env_url = Some("postgres://postgres@127.0.0.1:5432/postgres".to_owned());

        let defaults = parse_args(&[], env_url.clone()).expect("defaults");
        assert_eq!(defaults.duration_secs, 60);
        assert_eq!(defaults.warmup_secs, 2);
        assert_eq!(defaults.samples, 3);
        assert_eq!(defaults.clients, 8);
        assert_eq!(defaults.jobs, 4);
        assert_eq!(defaults.bulk_iterations, 5);
        assert_eq!(defaults.export_rows, 25_000);
        assert_eq!(defaults.history_runs, 50);
        assert!((defaults.max_regression_pct - 15.0).abs() < 1e-9);

        let quick = parse_args(&vec_s(&["--quick"]), env_url.clone()).expect("quick");
        assert_eq!(quick.duration_secs, 5);
        assert_eq!(quick.warmup_secs, 1);
        assert_eq!(quick.bulk_iterations, 3);
        assert_eq!(quick.export_rows, 5_000);
        assert_eq!(quick.history_runs, 20);

        let custom = parse_args(
            &vec_s(&[
                "--database-url",
                "postgres://custom@127.0.0.1:5432/postgres",
                "--duration",
                "1",
                "--clients",
                "2",
                "--jobs",
                "1",
                "--bulk-runs",
                "1",
                "--samples",
                "1",
                "--warmup-secs",
                "0",
                "--export-rows",
                "1000",
                "--history-runs",
                "10",
                "--mode",
                "simple",
                "--auditstream-bin",
                "/tmp/auditstream",
                "--json-out",
                "/tmp/out.json",
                "--markdown-out",
                "/tmp/out.md",
                "--compare",
                "/tmp/base.json",
                "--max-regression-pct",
                "50",
            ]),
            env_url,
        )
        .expect("custom");
        assert_eq!(
            custom.database_url,
            "postgres://custom@127.0.0.1:5432/postgres"
        );
        assert_eq!(custom.duration_secs, 1);
        assert_eq!(custom.warmup_secs, 0);
        assert_eq!(custom.samples, 1);
        assert_eq!(custom.clients, 2);
        assert_eq!(custom.jobs, 1);
        assert_eq!(custom.mode, "simple");
        assert_eq!(custom.export_rows, 1_000);
        assert_eq!(custom.history_runs, 10);
        assert_eq!(
            custom.auditstream_bin,
            Some(PathBuf::from("/tmp/auditstream"))
        );
        assert_eq!(
            custom.compare_baseline,
            Some(PathBuf::from("/tmp/base.json"))
        );
        assert!((custom.max_regression_pct - 50.0).abs() < 1e-9);
    }

    #[test]
    fn parse_args_clamps_default_jobs_and_rejects_invalid_flags() {
        let env_url = Some("postgres://127.0.0.1/postgres".to_owned());
        let two_clients = parse_args(&vec_s(&["--clients", "2"]), env_url.clone()).expect("2c");
        assert_eq!(two_clients.jobs, 2);

        for bad in [
            vec_s(&["--unknown"]),
            vec_s(&["--samples", "0"]),
            vec_s(&["--duration", "0"]),
            vec_s(&["--clients", "0"]),
            vec_s(&["--jobs", "0"]),
            vec_s(&["--clients", "2", "--jobs", "4"]),
            vec_s(&["--bulk-runs", "0"]),
            vec_s(&["--export-rows", "0"]),
            vec_s(&["--export-rows", "abc"]),
            vec_s(&["--history-runs", "0"]),
            vec_s(&["--mode", "invalid"]),
            vec_s(&["--max-regression-pct", "-1"]),
            vec_s(&["--max-regression-pct", "NaN"]),
            vec_s(&["--max-regression-pct", "abc"]),
            vec_s(&["--samples"]),
            vec_s(&["--samples", "abc"]),
        ] {
            assert!(
                parse_args(&bad, env_url.clone()).is_err(),
                "should reject {bad:?}"
            );
        }
        assert!(parse_args(&[], None).is_err());
    }

    #[test]
    fn replace_db_name_handles_queries_rejects_dbname_and_redacts_secrets() {
        let user = "postgres";
        let tok = "dummy_val";
        let with_auth = format!("postgres://{user}:{tok}@127.0.0.1:5432/postgres?sslmode=disable");
        let expected_auth =
            format!("postgres://{user}:{tok}@127.0.0.1:5432/auditstream_bench_1?sslmode=disable");
        assert_eq!(
            replace_db_name(&with_auth, "auditstream_bench_1").expect("replace"),
            expected_auth
        );
        assert_eq!(
            replace_db_name(
                "postgres://postgres@127.0.0.1:5432/postgres#frag",
                "auditstream_bench_1"
            )
            .expect("replace with fragment"),
            "postgres://postgres@127.0.0.1:5432/auditstream_bench_1#frag"
        );

        let kv_secret = "redacted_token_fixture";
        let kv_conninfo = format!("host=127.0.0.1 password={kv_secret} dbname=postgres");
        let err = replace_db_name(&kv_conninfo, "db").expect_err("non-URI must fail");
        assert!(
            !err.contains(kv_secret),
            "error must not leak password: {err}"
        );

        for override_url in [
            "postgres://postgres@127.0.0.1:5432/?dbname=postgres",
            "postgres://postgres@127.0.0.1:5432/x?sslmode=disable&DBNAME=postgres#frag",
        ] {
            let err =
                replace_db_name(override_url, "bench_db").expect_err("dbname query must fail");
            assert!(err.contains("dbname"), "unexpected error: {err}");
        }
    }

    #[test]
    fn release_auditstream_bin_resolves_default_relative_and_absolute_target_dirs() {
        let exe = format!("auditstream{}", env::consts::EXE_SUFFIX);
        assert_eq!(
            release_auditstream_bin(Path::new("/repo"), None),
            PathBuf::from(format!("/repo/target/release/{exe}"))
        );
        assert_eq!(
            release_auditstream_bin(Path::new("/repo"), Some(Path::new("custom-target"))),
            PathBuf::from(format!("/repo/custom-target/release/{exe}"))
        );
        assert_eq!(
            release_auditstream_bin(Path::new("/repo"), Some(Path::new("/cache"))),
            PathBuf::from(format!("/cache/release/{exe}"))
        );
    }

    #[test]
    fn split_uri_password_extracts_userinfo_and_query_passwords_without_touching_host() {
        let encoded_tok = "p%40ss%3A1%25";
        let uri_userinfo =
            format!("postgres://postgres:{encoded_tok}@127.0.0.1:5432/db?sslmode=disable");
        assert_eq!(
            split_uri_password(&uri_userinfo),
            (
                "postgres://postgres@127.0.0.1:5432/db?sslmode=disable".to_owned(),
                Some("p@ss:1%".to_owned())
            )
        );

        let ipv6_tok = "Ab9f";
        let uri_ipv6 = format!("postgres://user:{ipv6_tok}@[::1]:5432/db");
        assert_eq!(
            split_uri_password(&uri_ipv6),
            (
                "postgres://user@[::1]:5432/db".to_owned(),
                Some("Ab9f".to_owned())
            )
        );

        let query_tok = "q%40tok";
        let uri_query = format!(
            "postgresql:///?host=/var/run/postgresql&user=postgres&password={query_tok}#frag"
        );
        assert_eq!(
            split_uri_password(&uri_query),
            (
                "postgresql:///?host=/var/run/postgresql&user=postgres#frag".to_owned(),
                Some("q@tok".to_owned())
            )
        );

        let uri_only_pw_query =
            format!("postgres://postgres@127.0.0.1:5432/db?PASSWORD={query_tok}");
        assert_eq!(
            split_uri_password(&uri_only_pw_query),
            (
                "postgres://postgres@127.0.0.1:5432/db".to_owned(),
                Some("q@tok".to_owned())
            )
        );

        for no_pw in [
            "postgres://postgres@127.0.0.1:5432/db",
            "postgres://127.0.0.1:5432/db?application_name=a@b",
            "postgres://[::1]:5432/db",
            "not-a-uri",
        ] {
            assert_eq!(split_uri_password(no_pw), (no_pw.to_owned(), None));
        }
    }

    #[test]
    fn sql_and_toml_builders_produce_canonical_definitions() {
        let sql = build_setup_schema_sql().expect("build benchmark setup SQL");
        assert!(sql.starts_with("BEGIN;\n\n"));
        assert!(sql.ends_with("\n\nCOMMIT;\n"));
        assert!(sql.contains("auditstream._migration_tx"));
        for table in [
            "bench_up_cap",
            "bench_ins_cap",
            "bench_bulk_cap",
            "bench_bulk_up_cap",
        ] {
            assert!(sql.contains(&format!("ON \"public\".\"{table}\"")));
        }
        assert!(!sql.contains("auditstream._triggers_granted_self"));

        let seed_tables = build_seed_tables_sql();
        assert!(seed_tables.contains("bench_bulk_up_plain"));
        assert!(seed_tables.contains("bench_bulk_up_cap"));

        let seed_changes = build_seed_changes_sql(500);
        assert!(seed_changes.contains("AT TIME ZONE 'UTC'"));
        assert!(seed_changes.contains("generate_series(1, 500)"));

        let plain = build_export_toml(false);
        assert!(!plain.contains("redaction"));
        let redacted = build_export_toml(true);
        assert!(redacted.contains("[[export.redaction.rules]]"));
        assert!(redacted.contains("columns = [\"ssn\", \"email\"]"));
    }
}
