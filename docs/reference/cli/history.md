# `auditstream history`

> **Public contract:** Flags, filter semantics, output columns, JSON shape, and exit codes are part of the engine public contract governed by [Releases: Version policy](../../handbook/releases.md#version-policy).

`auditstream history` queries recorded audit events from the history table.
Results are ordered newest first by `occurred_at` descending, then `id` descending.

## Synopsis

```sh
auditstream [GLOBAL OPTIONS] history [OPTIONS]
```

## Options

| Flag | Values | Default | Description |
| --- | --- | --- | --- |
| `--table` | `<schema>.<table>` | none | Filter by audited schema and table name |
| `--record-id` | `<value>` | none | Filter by primary key value (requires `--table`) |
| `--actor` | `<id>` | none | Filter by `actor.id` or `actor.onBehalfOf.id` |
| `--since` | `<time>` | none | Inclusive lower bound on `occurred_at` |
| `--until` | `<time>` | none | Exclusive upper bound on `occurred_at` |
| `--limit` | `1..=1000` | `100` | Maximum number of events returned |
| `--format` | `table`, `json` | `table` | Output format written to stdout |

## Filter semantics

All provided filters combine with logical `AND`.
Every filter executes in PostgreSQL against `{schema}.changes`.

- **`--table <schema>.<table>`:** Requires both schema and table parts separated by one dot.
- **`--record-id <value>`:** Requires `--table`. Parses `<value>` as JSON first, falling back to a JSON string.
  - A JSON scalar (number, string, or boolean) matches single-key `record_id` objects with exact JSONB type equality (`1` differs from `"1"`).
  - A non-empty JSON object matches composite or partial keys by JSONB containment (`record_id @> value::jsonb`).
  - JSON `null`, arrays, and empty objects fail with exit code `2`.
- **`--actor <id>`:** Matches rows where `actor.id` equals `<id>` or `actor.onBehalfOf.id` equals `<id>`.
- **`--since <time>` and `--until <time>`:** Accept RFC 3339 timestamps or `YYYY-MM-DD` calendar dates read as midnight UTC.
  - `--since` is inclusive (`occurred_at >= since`).
  - `--until` is exclusive (`occurred_at < until`).
  - `--until` must be strictly after `--since`, otherwise the command exits `2`.
  - Time predicates bound `occurred_at` directly so PostgreSQL can prune monthly partitions.

## Output

Stdout carries event data only.
When no rows match, stdout remains empty, `no events matched` prints to stderr, and the command exits `0`.
Closing stdout early (such as piping to `head -n 1`) stops cleanly and exits `0`.

### Table output (`--format table`)

Prints an aligned UTC table omitting `before` and `after` snapshots.
Columns appear in fixed order:

| Column | Content |
| --- | --- |
| `OCCURRED_AT` | UTC RFC 3339 timestamp with microsecond precision |
| `OP` | DML operation (`INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`) or app action |
| `TABLE` | `<schema>.<table>`, or `-` for application events |
| `RECORD_ID` | Compact JSON primary key object, or `-` when absent |
| `ACTOR` | `type:id` (plus ` for type:id` when `onBehalfOf` is set), or `-` |
| `ACTOR_SOURCE` | `set_config`, `comment`, `jwt`, `invalid`, or `-` |
| `CHANGED` | Comma-separated changed column names, or `-` |

### JSON output (`--format json`)

Prints one compact JSON event per line matching `spec/schemas/event.schema.json`.
JSON output includes `before` and `after` and preserves arbitrary-precision PostgreSQL `numeric` values verbatim.

## Exit codes

- `0`: Query succeeded (including when no events matched or stdout closed early).
- `2`: Invalid CLI flags, filter combinations, timestamps, or configuration.
- `3`: Database unreachable or query failed.
- `4`: Writing output failed.

See [Exit codes](../exit-codes.md) for the full table.
