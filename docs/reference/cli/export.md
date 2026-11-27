# `auditstream export`

Write committed audit events as JSON Lines or RFC 4180 CSV. By default, each invocation reads all matching events and writes them to stdout. To append new events across runs, use the same cursor and output file each time.

```sh
auditstream export --format jsonl --output events.jsonl --cursor-file events.cursor
```

## Options

| Flag | Behavior |
| --- | --- |
| `--format jsonl\|csv` | Select the encoding. The default is `jsonl`. |
| `--output PATH` | Append to a file. The default is stdout. A nonempty CSV file keeps its existing header. |
| `--cursor-file PATH` | Save progress after each flushed batch. Omitting this option and `export.cursor_file` makes the export stateless. |
| `--since TIME` | Include events at or after this instant. |
| `--until TIME` | Include events strictly before this instant. Accepts the same inputs as `--since`, an RFC 3339 timestamp with an offset or `YYYY-MM-DD` at midnight UTC. |
| `--actor ID` | Match `actor.id` or `actor.onBehalfOf.id`. |
| `--tenant ID` | Match `actor.tenant` or `actor.onBehalfOf.tenant`. |
| `--source db\|app` | Select database changes or application events. |
| `--batch-size N` | Read up to `N` events per batch. The default is `1000` and the minimum is `1`. |
| `--from-start` | Replay all matching events and replace the checkpoint. Replaying into the same file appends another copy. Use a new output path for a fresh file. |
| `--allow-rls-filtered` | Allow exporting when Row-Level Security is active on `<schema>.changes` without an unobstructed `PERMISSIVE USING (true)` policy (for intentional per-tenant exports). Recorded in the cursor checkpoint so unfiltered and filtered cursors cannot be mixed without `--from-start`. |
| `--stats-format text\|json` | Format the stderr summary as plain text (`text`) or structured JSON telemetry (`json`, reporting `events`, `batches`, `duration_ms`, `cursor`, `snapshot_xmin`, `oldest_unexported_age_secs`, and `stalled`). Defaults to `json` when `--log-format json` / `AUDITSTREAM_LOG_FORMAT=json` is active, and `text` otherwise. Stdout always stays reserved for event data. |
| `-q`, `--quiet` | Suppress the stderr summary of events, batches, and the last cursor. |

Flags override the configured sink type, path, cursor file, or batch size individually. See the [configuration reference](../config.md).

## Checkpoints and delivery

Delivery is **at least once**. The reader advances through committed transaction IDs. An open transaction can postpone an event until a later run. Each batch is acknowledged after the sink flushes. Downstream consumers can deduplicate using `event_id`. See the [export specification](../../../spec/export.md).

The cursor file is JSON. `cursor` contains the transaction ID `xact_id` and row `id`. `anchor` records the latest committed boundary row (`xact_id`, `id`, `event_id`) so resuming verifies history continuity — confirming both that the anchor row still matches `event_id` and that no unexpected audit rows appeared in the drained gap between `anchor` and `cursor.xact_id` (allowing verified zero-loss HA forward promotions while rejecting backward `timeline_id` regressions, PITR rollbacks, async failover data loss, or purged partitions). `filters` records `since`, `until`, `actor`, `tenant`, and `source`, with `null` for unused filters. Omitted filters also mean no restriction. The `database` object records the server-side PostgreSQL identity (`system_identifier` when permitted by role privileges, `name`, `oid`, `history_table_oid`, and `timeline_id` when readable via `pg_walfile_name(pg_current_wal_lsn())` / `pg_control_checkpoint()` on primaries or `pg_stat_wal_receiver` on standbys), so resuming works across equivalent endpoints (such as `localhost` vs `127.0.0.1`, PgBouncer, or verified HA failover) while rejecting a different or recreated database. The checkpoint stores a secret-free `sink` fingerprint (`type` and destination `path`/`target`, never credentials), the audit `schema`, `output_len` in bytes for a regular file, any configured `export.redaction` policy, and `allow_rls_filtered: true` when `--allow-rls-filtered` is enabled. Stdout and devices have no recoverable file length.

If the saved cursor is ahead of PostgreSQL's current `pg_snapshot_xmin()` (`cursor.xact_id > snapshot_xmin`, or `cursor.xact_id == snapshot_xmin` with `cursor.id > 0`), or if `timeline_id` regressed, became unreadable after a failover (`saved timeline_id > 1`), or the boundary row anchor diverged or disappeared, export fails loudly with exit code `3` before modifying the output file or cursor (with a standby replication-lag hint when `pg_is_in_recovery()` is `true`, and a failover/PITR hint otherwise). When an open transaction pins `pg_snapshot_xmin()` while committed rows wait behind it, `--stats-format json` reports `"stalled": true` and `oldest_unexported_age_secs` (keeping stderr to a single JSON line under `--log-format text`, or preceded by a JSON `WARN` log line under `--log-format json`), while `--stats-format text` appends `stalled=true oldest_unexported_age_secs=<N.N>` and emits a stderr warning.

Use one cursor and output file per export. Rows excluded by filters still advance the cursor, so resuming requires the same filters, format, output path, `--allow-rls-filtered` mode, and effective `export.redaction` policy (use `--from-start` to replay under a new policy). On resume, the CLI trims any bytes beyond the last acknowledged length before appending. It refuses to extend a shortened file. Keep other writers off this output, since recovery discards anything appended after the checkpoint. For stdout, a downstream consumer may receive the last batch again after an interruption.

A sidecar named `<cursor-file>.lock` (mode `0600` on Unix; pre-existing lock files are tightened to `0600`) stays in place between runs. Newly created `--output` files (`.jsonl` or `.csv`) are also created with mode `0600` on Unix so exported row snapshots are not world-readable (pre-existing output files keep their permissions). On Unix, `--cursor-file`, `<cursor-file>.lock`, and `--output` reject symbolic links (`O_NOFOLLOW`) to prevent symlink overwrite, truncation, or append attacks. The CLI holds its OS lock while exporting and replaces the cursor atomically. Closing the process releases the lock.

For non-loopback PostgreSQL hosts, pass `sslmode=verify-full` in `AUDITSTREAM_DATABASE_URL` / `--database-url` so exported audit data is encrypted and server certificates are verified; `disable`, `allow`, `prefer`, `require`, and `verify-ca` emit a stderr warning.

## Export-time redaction

Configure `[[export.redaction.rules]]` in `auditstream.toml` to mask (`"[redacted]"`) or remove top-level keys inside `before` and `after` before any sink (`jsonl` or `csv`) writes a batch. See [Configuration reference: Export redaction](../config.md#export-redaction-exportredactionrules).

- **Secrets belong in `capture.exclude`:** Export redaction runs only when events leave PostgreSQL. Secrets, credentials, password hashes, and payment card numbers must be excluded via `capture.exclude` so they are never stored in `auditstream.changes`.
- **Top-level keys only & fail-closed:** Redaction matches top-level JSON object keys only (nested dot-paths such as `metadata.ssn` exit `2` at startup) and preserves untouched key order and arbitrary-precision PostgreSQL `numeric` values. If a matching rule encounters a non-object or malformed JSON payload (such as an `app` event whose `data` is a JSON array), the export aborts with exit code `5` before writing or advancing the cursor.
- **`record_id` is never redacted:** Rules only modify `before`, `after`, and (for `mode = "remove"`) `changed`. Tables with natural PII primary keys (for example `email` or `ssn`) or `app` events that put PII in `target` still export `record_id` in plaintext; prefer surrogate primary keys and opaque `target` identifiers when exported streams must exclude PII. See [Configuration reference: Export redaction](../config.md#export-redaction-exportredactionrules).

## Row-Level Security (RLS) guard

Before reading events or advancing any cursor, `auditstream export` checks `pg_catalog.row_security_active` on the parent `<schema>.changes` table.
When RLS is active for the connected role, export **fails closed** with exit code `3` (without creating or advancing the cursor or opening the output file) unless:

- at least one applicable `PERMISSIVE` policy covering `SELECT` (`polcmd IN ('*', 'r')`, including policies targeting `PUBLIC` or inherited roles) has `USING (true)`, **and**
- zero applicable `RESTRICTIVE` policies covering `SELECT` have a `USING` expression other than `'true'`.

To export all tenants safely under RLS — including on PostgreSQL 15 and managed PostgreSQL providers such as Amazon RDS where non-superuser `BYPASSRLS` is disallowed, and without relying on elevated `BYPASSRLS` admin roles on Neon or Supabase — create a dedicated `NOSUPERUSER NOBYPASSRLS` role (for example `auditstream_exporter`) with `USAGE` on the audit schema, `SELECT` on `<schema>.changes`, and a `PERMISSIVE` `SELECT` policy `USING (true)` that is not narrowed by any applicable `RESTRICTIVE` policy. See [Isolate tenants with Row-Level Security (RLS)](../../how-to/multi-tenant-rls.md).
Pass `--allow-rls-filtered` only when an intentionally RLS-filtered (for example per-tenant) export is desired.

## Tenant filtering notes

- **Partition pruning:** Pair `--tenant` with `--since` / `--until` on large backfills so PostgreSQL prunes monthly partitions.
- **No default tenant index:** Per [ADR-0008](../../decisions/0008-history-lookup-index.md), Auditstream does not create a default index on `actor.tenant`. For frequent tenant-scoped queries, see the online paired partial expression index recipe in [`auditstream history` — Opt-in tenant indexes](history.md#opt-in-tenant-indexes).
- **Security and completeness caveat:** `--tenant` matches self-asserted `actor.tenant` metadata only. It omits rows where `actor` is `NULL` (migrations, cron jobs, direct DBA writes), `actor_source = 'invalid'`, or `actor_source = 'jwt'` (which populates only `type` and `id`), and is not a substitute for PostgreSQL RLS or row-level tenant ownership.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Export completed, or the consumer closed its stdout pipe on a stateless export. |
| `2` | Invalid arguments, configuration (including invalid `export.redaction` rules), or incompatible checkpoint filters, format, schema, database, output path, `--allow-rls-filtered` mode, or `export.redaction` policy. |
| `3` | PostgreSQL connection or read failure, active Row-Level Security on `<schema>.changes` without an unobstructed `PERMISSIVE USING (true)` policy (unless `--allow-rls-filtered` is passed), a saved cursor ahead of `pg_snapshot_xmin()` (standby lag, failover/PITR, or restore), a backward `timeline_id` regression or unreadable post-failover timeline, or an unanchored/diverged/missing boundary row anchor. |
| `4` | Output or cursor I/O failure, including a held lock, corrupt cursor, shortened output file, or a broken stdout pipe on a stateful (`--cursor-file`) export. |
| `5` | Fail-closed redaction error on a non-object or malformed event payload, or unexpected internal failure. |

See the [exit code reference](../exit-codes.md) for the full table.
