# Capture Benchmark Suite (`bench/capture`)

Measures the per-write cost of Auditstream's engine-generated triggers (`D2` + `D3`) against an uncaptured baseline on the same PostgreSQL instance, plus a lookup-index variant matrix covering worst-case single-row updates and 10,000-row bulk inserts.

## Quick local run

Requires `psql` and `pgbench` on `PATH` and a running PostgreSQL server in `DATABASE_URL`:

```sh
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres cargo xtask bench --quick
```

## Full run (nightly defaults)

Default `pgbench` parameters: `-M prepared -c 8 -j 4 -T 60`:

```sh
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
  cargo xtask bench --markdown-out bench-report.md --json-out bench-report.json
```

## Workloads

1. Single-row `UPDATE` without capture (`update_baseline.sql`)
2. Single-row `UPDATE` with trailing SQL comment (`update_comment.sql`)
3. Single-row `UPDATE` with transaction-local `set_config` (`update_setcfg.sql`)
4. Single-row `INSERT` without capture (`insert_baseline.sql`)
5. Single-row `INSERT` with capture (`insert_captured.sql`)
6. 10,000-row bulk `INSERT` without capture vs with capture
7. Lookup index variants (`none`, `current`, `table_occurred_at`, `record_id_hash`) across worst-case single-row updates and 10,000-row bulk inserts.

Every captured run asserts `0` failed transactions and that the exact number of rows written to `auditstream.changes` equals the number of rows written to the source table.
