# Benchmark Suite (`bench/capture`)

Measures the per-write cost of Auditstream's engine-generated triggers (`D2` + `D3`) against uncaptured baselines on the same PostgreSQL instance, lookup-index variants, 10,000-row bulk `INSERT` and `UPDATE`, `auditstream export` throughput (plain and redacted JSONL), and `auditstream history` point-in-time record lookup latency (`p50` / `p99` ms).

Every workload runs an optional warmup phase (`--warmup-secs`) followed by `--samples <N>` timed runs (default `3`), reporting the **median** alongside `min`/`max`/`spread` statistics in the JSON output.

## Quick local run

Requires `psql` and `pgbench` on `PATH` and a running PostgreSQL server in `DATABASE_URL`:

```sh
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres cargo xtask bench --quick
```

## Full run (CLI defaults & nightly CI)

Default CLI parameters: `pgbench` `-M prepared -c 8 -j 4 -T 60` with `xtask` options `--samples 3 --warmup-secs 2 --bulk-runs 5 --export-rows 25000 --history-runs 50`:

```sh
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
  cargo xtask bench --markdown-out bench-report.md --json-out bench-report.json
```

The scheduled nightly workflow (`.github/workflows/nightly.yml`) runs `--duration 20 --samples 3 --warmup-secs 2 --bulk-runs 5 --export-rows 25000 --history-runs 50` (completing well inside its 60-minute timeout while collecting 3-sample medians across every workload).

## Baseline regression comparison

`bench/baselines/v0.1.0.json` stores a committed 3-sample reference baseline (captured on PostgreSQL 18.6, 8-core `linux/x86_64` with `-M prepared -c 4 -j 2 -T 3 --samples 3 --warmup-secs 1 --bulk-runs 3 --export-rows 25000 --history-runs 30`). For apples-to-apples regression gating on your own host or runner, capture a same-host baseline first and pass matching flags to `--compare` (exits non-zero when any tracked metric regresses beyond `--max-regression-pct`, default `15.0`):

```sh
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
  cargo xtask bench --quick --json-out /tmp/local-baseline.json

DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
  cargo xtask bench --quick --compare /tmp/local-baseline.json --max-regression-pct 15
```

## Workloads

1. Single-row `UPDATE` without capture (`update_baseline.sql`)
2. Single-row `UPDATE` with trailing SQL comment (`update_comment.sql`)
3. Single-row `UPDATE` with transaction-local `set_config` (`update_setcfg.sql`)
4. Single-row `INSERT` without capture (`insert_baseline.sql`)
5. Single-row `INSERT` with capture (`insert_captured.sql`)
6. 10,000-row single-statement bulk `INSERT` without capture vs. with capture
7. 10,000-row single-statement bulk `UPDATE` without capture vs. with capture
8. Lookup index variants (`none`, `current`, `table_occurred_at`, `record_id_hash`) across worst-case single-row updates and 10,000-row bulk inserts
9. `auditstream export` throughput (`events/s`) in plain JSONL and redacted JSONL modes (`--export-rows`)
10. `auditstream history --table public.invoice --record-id <id>` point-in-time lookup latency (`p50` / `p99` ms over `--history-runs` queries per sample)

Every captured run asserts `0` failed transactions and that the exact number of rows written to `auditstream.changes` equals the number of rows written to the source table. Export runs verify exact exported event counts and confirm redacted output contains `[redacted]` with zero leaked seed PII.
