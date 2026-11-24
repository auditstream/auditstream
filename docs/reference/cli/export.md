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
| `--source db\|app` | Select database changes or application events. |
| `--batch-size N` | Read up to `N` events per batch. The default is `1000` and the minimum is `1`. |
| `--from-start` | Replay all matching events and replace the checkpoint. Replaying into the same file appends another copy. Use a new output path for a fresh file. |
| `-q`, `--quiet` | Suppress the stderr summary of events, batches, and the last cursor. |

Flags override the configured sink type, path, cursor file, or batch size individually. See the [configuration reference](../config.md).

## Checkpoints and delivery

Delivery is **at least once**. The reader advances through committed transaction IDs. An open transaction can postpone an event until a later run. Each batch is acknowledged after the sink flushes. Downstream consumers can deduplicate using `event_id`. See the [export specification](../../../spec/export.md).

The cursor file is JSON. `cursor` contains the transaction ID `xact_id` and row `id`. `filters` records `since`, `until`, `actor`, and `source`, with `null` for unused filters. The checkpoint also identifies the source database and `schema`, the `sink` type and destination, and `output_len` in bytes for a regular file. Stdout and devices have no recoverable file length.

Use one cursor and output file per export. Rows excluded by filters still advance the cursor, so resuming requires the same filters. On resume, the CLI trims any bytes beyond the last acknowledged length before appending. It refuses to extend a shortened file. Keep other writers off this output, since recovery discards anything appended after the checkpoint. For stdout, a downstream consumer may receive the last batch again after an interruption.

A sidecar named `<cursor-file>.lock` stays in place between runs. The CLI holds its OS lock while exporting and replaces the cursor atomically. Closing the process releases the lock.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Export completed, or the consumer closed its stdout pipe. |
| `2` | Invalid arguments, configuration, or incompatible checkpoint filters. |
| `3` | PostgreSQL connection or read failure. |
| `4` | Output or cursor I/O failure, including a held lock or incompatible output path. |

See the [exit code reference](../exit-codes.md) for the full table.
