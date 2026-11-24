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
| `-q`, `--quiet` | Suppress the stderr summary of events, batches, and the last cursor. |

Flags override the configured sink type, path, cursor file, or batch size individually. See the [configuration reference](../config.md).

## Checkpoints and delivery

Delivery is **at least once**. The reader advances through committed transaction IDs. An open transaction can postpone an event until a later run. Each batch is acknowledged after the sink flushes. Downstream consumers can deduplicate using `event_id`. See the [export specification](../../../spec/export.md).

The cursor file is JSON. `cursor` contains the transaction ID `xact_id` and row `id`. `filters` records `since`, `until`, `actor`, `tenant`, and `source`, with `null` for unused filters. Omitted filters also mean no restriction. The `database` object records the server-side PostgreSQL identity (`system_identifier` when permitted by role privileges, `name`, `oid`, and `history_table_oid`), so resuming works across equivalent endpoints (such as `localhost` vs `127.0.0.1`, PgBouncer, or failover) while rejecting a different or recreated database. The checkpoint also records the audit `schema`, the `sink` type and destination, `output_len` in bytes for a regular file, and any configured `export.redaction` policy. Stdout and devices have no recoverable file length.

Use one cursor and output file per export. Rows excluded by filters still advance the cursor, so resuming requires the same filters, format, output path, and effective `export.redaction` policy (use `--from-start` to replay under a new policy). On resume, the CLI trims any bytes beyond the last acknowledged length before appending. It refuses to extend a shortened file. Keep other writers off this output, since recovery discards anything appended after the checkpoint. For stdout, a downstream consumer may receive the last batch again after an interruption.

A sidecar named `<cursor-file>.lock` (mode `0600` on Unix) stays in place between runs. The CLI holds its OS lock while exporting and replaces the cursor atomically. Closing the process releases the lock.

## Export-time redaction

Configure `[[export.redaction.rules]]` in `auditstream.toml` to mask (`"[redacted]"`) or remove top-level keys inside `before` and `after` before any sink (`jsonl` or `csv`) writes a batch. See [Configuration reference: Export redaction](../config.md#export-redaction-exportredactionrules).

- **Secrets belong in `capture.exclude`:** Export redaction runs only when events leave PostgreSQL. Secrets, credentials, password hashes, and payment card numbers must be excluded via `capture.exclude` so they are never stored in `auditstream.changes`.
- **Top-level keys only & fail-closed:** Redaction matches top-level JSON object keys only (nested dot-paths such as `metadata.ssn` exit `2` at startup) and preserves untouched key order and arbitrary-precision PostgreSQL `numeric` values. If a matching rule encounters a non-object or malformed JSON payload (such as an `app` event whose `data` is a JSON array), the export aborts with exit code `5` before writing or advancing the cursor.

## Tenant filtering notes

- **Partition pruning:** Pair `--tenant` with `--since` / `--until` on large backfills so PostgreSQL prunes monthly partitions.
- **No default tenant index:** Per [ADR-0008](../../decisions/0008-history-lookup-index.md), Auditstream does not create a default index on `actor.tenant`. For frequent tenant-scoped queries, see the online paired partial expression index recipe in [`auditstream history` — Opt-in tenant indexes](history.md#opt-in-tenant-indexes).
- **Security and completeness caveat:** `--tenant` matches self-asserted `actor.tenant` metadata only. It omits rows where `actor` is `NULL` (migrations, cron jobs, direct DBA writes), `actor_source = 'invalid'`, or `actor_source = 'jwt'` (which populates only `type` and `id`), and is not a substitute for PostgreSQL RLS or row-level tenant ownership.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Export completed, or the consumer closed its stdout pipe on a stateless export. |
| `2` | Invalid arguments, configuration (including invalid `export.redaction` rules), or incompatible checkpoint filters, format, schema, database, output path, or `export.redaction` policy. |
| `3` | PostgreSQL connection or read failure. |
| `4` | Output or cursor I/O failure, including a held lock, corrupt cursor, shortened output file, or a broken stdout pipe on a stateful (`--cursor-file`) export. |
| `5` | Fail-closed redaction error on a non-object or malformed event payload, or unexpected internal failure. |

See the [exit code reference](../exit-codes.md) for the full table.
