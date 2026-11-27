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
| `--table` | `<[schema.]table>` | none | Filter by audited table (defaults unqualified names to `public`) |
| `--record-id` | `<value>` | none | Filter by primary key value (requires `--table`) |
| `--actor` | `<id>` | none | Filter by `actor.id` or `actor.onBehalfOf.id` |
| `--tenant` | `<id>` | none | Filter by `actor.tenant` or `actor.onBehalfOf.tenant` |
| `--since` | `<time>` | none | Inclusive lower bound on `occurred_at` |
| `--until` | `<time>` | none | Exclusive upper bound on `occurred_at` |
| `--limit` | `1..=1000` | `100` | Maximum number of events returned |
| `--format` | `table`, `json` | `table` | Output format written to stdout |
| `--diff` | — | off | Show field-level `before -> after` diffs (`--format table` only) |
| `--redact` | — | auto when configured | Require configured `[[export.redaction.rules]]` to be applied (exits `2` if none are configured; conflicts with `--unredacted`) |
| `--unredacted` | — | off | Break-glass opt-out disabling configured `[[export.redaction.rules]]` (conflicts with `--redact`) |
| `--omit-payload` | — | off | Omit `before` and `after` values (`null` in JSON), keeping event metadata and `changed` column names (cannot be combined with `--diff`) |

## Filter semantics

All provided filters combine with logical `AND`.
Every filter executes in PostgreSQL against `{schema}.changes`.

- **`--table <[schema.]table>`:** Accepts `<table>` (resolved to `public.<table>`) or `<schema>.<table>`. Rejects empty segments or extra dots with exit code `2`.
- **`--record-id <value>`:** Requires `--table`. Parses `<value>` as JSON first, falling back to a JSON string.
  - A JSON scalar (number, string, or boolean) matches single-key `record_id` objects with exact JSONB type equality (`1` differs from `"1"`).
  - A non-empty JSON object matches composite or partial keys by JSONB containment (`record_id @> value::jsonb`).
  - JSON `null`, arrays, and empty objects fail with exit code `2`.
- **`--actor <id>`:** Matches rows where `actor.id` equals `<id>` or `actor.onBehalfOf.id` equals `<id>`.
- **`--tenant <id>`:** Matches rows where `actor.tenant` equals `<id>` or `actor.onBehalfOf.tenant` equals `<id>`. Rejects an empty string with exit code `2`.
  - **Time bounds recommended:** Pair `--tenant` with `--since` and `--until` so PostgreSQL prunes monthly partitions before scanning.
  - **Security and completeness caveat:** `--tenant` filters self-asserted `actor.tenant` metadata attached by the application. It **does not** match rows where `actor` is `NULL` (such as migrations, cron jobs, or direct DBA writes), where `actor_source = 'invalid'`, or where `actor_source = 'jwt'` (which carries only `type` and `id`). Do not treat `--tenant` as a substitute for PostgreSQL Row-Level Security (RLS) or row-level tenant ownership.
- **`--since <time>` and `--until <time>`:** Accept RFC 3339 timestamps or `YYYY-MM-DD` calendar dates read as midnight UTC.
  - `--since` is inclusive (`occurred_at >= since`).
  - `--until` is exclusive (`occurred_at < until`).
  - `--until` must be strictly after `--since`, otherwise the command exits `2`.
  - Time predicates bound `occurred_at` directly so PostgreSQL can prune monthly partitions.

## Opt-in tenant indexes

Per [ADR-0008](../../decisions/0008-history-lookup-index.md), Auditstream does **not** create a default index on `actor.tenant` so single-tenant deployments pay no write amplification. If you run frequent `--tenant` queries or high-QPS Activity Feeds, create paired partial expression indexes online so PostgreSQL can combine both `OR` branches via `BitmapOr` without blocking writes:

```sql
-- 1. Create invalid parent index definitions without locking child partitions.
CREATE INDEX changes_actor_tenant_idx
  ON ONLY auditstream.changes ((actor->>'tenant'))
  WHERE actor->>'tenant' IS NOT NULL;

CREATE INDEX changes_obo_tenant_idx
  ON ONLY auditstream.changes ((actor->'onBehalfOf'->>'tenant'))
  WHERE actor->'onBehalfOf'->>'tenant' IS NOT NULL;

-- 2. For each existing monthly partition (and changes_default), build concurrently and attach:
CREATE INDEX CONCURRENTLY changes_2026_11_actor_tenant_idx
  ON auditstream.changes_2026_11 ((actor->>'tenant'))
  WHERE actor->>'tenant' IS NOT NULL;
ALTER INDEX auditstream.changes_actor_tenant_idx
  ATTACH PARTITION auditstream.changes_2026_11_actor_tenant_idx;

CREATE INDEX CONCURRENTLY changes_2026_11_obo_tenant_idx
  ON auditstream.changes_2026_11 ((actor->'onBehalfOf'->>'tenant'))
  WHERE actor->'onBehalfOf'->>'tenant' IS NOT NULL;
ALTER INDEX auditstream.changes_obo_tenant_idx
  ATTACH PARTITION auditstream.changes_2026_11_obo_tenant_idx;
```

Once attached to the parent, future monthly partitions created by Auditstream inherit both indexes automatically.

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

### Diff output (`--diff`)

Prints each event header followed by field-level changes:

- `UPDATE`: `~ <column>: <before> -> <after>` (plus `+` / `-` for keys added or removed), preserving arbitrary-precision PostgreSQL `numeric` values.
- `INSERT`: `+ <column>: <value>` for each column in `after`.
- `DELETE`: `- <column>: <value>` for each column in `before`.
- `TRUNCATE` (or events without row images): `(no row image)`.

Color output is enabled automatically only when stdout is a TTY, `NO_COLOR` is unset or empty, and `TERM` is not `dumb`.
Passing `--diff` together with `--format json` exits with code `2` (JSON output already includes `before` and `after`).

All human-readable terminal output (`--format table` and `--diff`) strips ANSI CSI/OSC/DCS/ESC sequences, C0/C1 control characters, and Unicode BiDi overrides (`U+202A`–`U+202E`, `U+2066`–`U+2069`), and maps `\n`, `\r`, `\t`, and Unicode line/paragraph separators (`U+2028`, `U+2029`) to spaces.

### JSON output (`--format json`)

Prints one compact JSON event per line matching `spec/schemas/event.schema.json`.
JSON output includes `before` and `after` (unless `--omit-payload` is set, in which case both are `null`) and preserves arbitrary-precision PostgreSQL `numeric` values verbatim.

## Redaction and payload omission

When `[[export.redaction.rules]]` are configured in `auditstream.toml`, `auditstream history` applies them automatically across `--format table`, `--diff`, and `--format json` before writing anything to stdout:

- **`mode = "mask"` (default):** Replaces matching top-level keys in `before` and `after` with `"[redacted]"` while keeping the column name in `changed` and preserving untouched key order and arbitrary-precision PostgreSQL `numeric` values.
- **`mode = "remove"`:** Drops matching top-level keys from `before`, `after`, and `changed` (including the `CHANGED` column in `--format table` and diff lines in `--diff`).
- **Stderr indicator:** Whenever configured redaction rules are active and at least one event is returned, `auditstream history` writes a concise note to stderr (formatted as plain text or a single-line JSON `INFO` record under `--log-format json`, and suppressed by `-q`/`--quiet`). Stdout remains reserved strictly for event data.
- **`--redact`:** Enforces that at least one `[[export.redaction.rules]]` rule is loaded; exits `2` without querying PostgreSQL if no rules are configured. Useful in scripts or coding-agent wrappers that must fail closed if `auditstream.toml` is missing.
- **`--unredacted`:** Explicit break-glass flag for local debugging that disables configured redaction rules and suppresses the redaction stderr note. Conflicts with `--redact` (exit `2`).
- **`--omit-payload`:** Sets `before` and `after` to `null` (preserving `spec/schemas/event.schema.json` compliance, event metadata, and `changed` column names) for compact, payload-free incident timelines and agent inspection. Cannot be combined with `--diff` (exit `2`). When redaction rules are configured and `--unredacted` is not set, `mode = "remove"` rules still filter `changed`.
- **Terminal escaping preserved:** `--format table` and `--diff` continue applying terminal escaping (stripping ANSI/C0/C1/BiDi controls and normalizing line breaks) to all displayed fields and keys when redaction is active.
- **Fail-closed on non-object payloads:** If a matching redaction rule encounters a non-object or malformed JSON payload (such as an `app` event whose `data` is a JSON array), `auditstream history` aborts with exit code `5` before emitting the redaction note or writing any event to stdout (use `--omit-payload` for payload-free metadata or `--unredacted` for break-glass inspection).

### Redaction scope (v0.1)

- **Top-level `before`/`after` keys only:** In v0.1, `[[export.redaction.rules]]` matches top-level JSON object keys inside `before` and `after` only.
- **Secrets belong in `capture.exclude`:** Secrets, credentials, password hashes, payment card numbers (PAN/CVV), and API tokens should be excluded via `capture.exclude` so triggers never write them into `auditstream.changes`.
- **Nested JSONB and `record_id`:** Nested JSONB path redaction and `record_id` redaction / HMAC pseudonymization are planned for v0.3. In v0.1, `record_id` is never redacted; prefer surrogate primary keys and opaque `log_event()` `target` identifiers when inspected output must exclude PII. See [Configuration reference: Export redaction](../config.md#export-redaction-exportredactionrules).

## Exit codes

- `0`: Query succeeded (including when no events matched or stdout closed early).
- `2`: Invalid CLI flags, filter combinations, timestamps, configuration (including invalid `export.redaction` rules), or `--redact` passed with no configured rules.
- `3`: Database unreachable or query failed.
- `4`: Writing output failed.
- `5`: Fail-closed redaction error on a non-object or malformed event payload, or unexpected internal failure.

See [Exit codes](../exit-codes.md) for the full table.
