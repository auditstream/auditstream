# Export Incrementally to JSON Lines or CSV

Use `auditstream export` with a cursor file to stream committed events incrementally without skipping out-of-order commits.

## 1. Export incrementally to JSON Lines

Run `auditstream export` with `--cursor-file` and `--output`:

```sh
auditstream export \
  --format jsonl \
  --cursor-file events.cursor \
  --output events.jsonl
```

Stdout stays empty when `--output` points to a file.
Stderr prints a one-line summary (`events=... batches=... cursor=(...)`) unless `-q` is passed.

Run the exact same command again after new database writes:

```sh
auditstream export \
  --format jsonl \
  --cursor-file events.cursor \
  --output events.jsonl
```

Only events committed after the saved `xid8` cursor are appended to `events.jsonl`.

## 2. Export to RFC 4180 CSV

Pass `--format csv` with its own cursor and output files:

```sh
auditstream export \
  --format csv \
  --cursor-file events-csv.cursor \
  --output events.csv
```

When appending to a non-empty CSV file, `export` keeps the existing header row.

## 3. Filter exported events

Narrow exports with `--since`, `--until`, `--actor`, `--tenant`, or `--source db|app`:

```sh
auditstream export \
  --format jsonl \
  --since 2026-06-01 \
  --source db \
  --cursor-file db-events.cursor \
  --output db-events.jsonl
```

Keep filters identical on every resume against the same cursor file.
Changing filters on an existing cursor file exits with code `2`.
Use `--from-start` to replay from the beginning and replace the stored cursor.

## 4. Configure defaults in `auditstream.toml`

Save recurring export settings in `auditstream.toml`:

```toml
[export]
cursor_file = "events.cursor"
batch_size = 1000

[export.sink]
type = "jsonl"
path = "events.jsonl"
```

## Delivery and deduplication

Delivery is **at least once**.
On resume after an interrupted write, `export` trims any partial tail past the last acknowledged byte length before appending.
Downstream consumers should deduplicate by `event_id`.
See [`auditstream export`](../reference/cli/export.md) for the cursor JSON schema and exit codes.
