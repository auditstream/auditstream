# Configuration Reference

> **Public contract:** The `auditstream.toml` schema, keys, defaults and precedence order are part of the engine public contract governed by [Releases: Version policy](../handbook/releases.md#version-policy) and mirrored in `crates/auditstream/src/config.rs`.

## File discovery

1. If `--config <PATH>` or `AUDITSTREAM_CONFIG` is set, that file is loaded; if it does not exist, the CLI exits with code `2` and names the path.
2. Otherwise, `./auditstream.toml` is loaded if it exists in the current working directory.
3. Unknown keys or wrong value types always exit with code `2` and name the offending key.

## Precedence

Values resolve in strict order (first non-empty source wins; empty environment variables fall through to lower-priority sources, while an explicit empty `--schema ""` or `schema = ""` fails validation with exit code `2`):

1. Command-line flag
2. Environment variable (global options only, so subcommand flags never collide)
3. Configuration file (`auditstream.toml`)
4. Built-in default

## Schema

| Key | Type | Default | Flag | Environment |
| --- | --- | --- | --- | --- |
| `database_url` | string | none | `--database-url` | `AUDITSTREAM_DATABASE_URL`, then `DATABASE_URL` (use `sslmode=verify-full` for non-loopback hosts) |
| `schema` | string (`[A-Za-z_][A-Za-z0-9_]*`, max 63 bytes, not `pg_*` or `information_schema`) | `auditstream` | `--schema` | `AUDITSTREAM_SCHEMA` |
| `capture.tables` | list of `schema.table` | `[]` | `init --table` | none |
| `capture.exclude` | list of `schema.table.column` (non-primary-key columns only) | `[]` | `init --exclude` | none |
| `migrations.format` | `prisma`, `drizzle` or `sql` | none | `init --migrations` | none |
| `migrations.dir` | path | per format | `init --out` | none |
| `export.cursor_file` | path | none | `export --cursor-file` | none |
| `export.batch_size` | integer | `1000` | `export --batch-size` | none |
| `export.sink` | table with `type` (`jsonl` or `csv`) and optional `path` | JSON Lines on stdout | `export --format` overrides `type` and `--output` overrides `path` | none |
| `export.redaction.rules` | list of redaction rule tables (`table` or `action`, `columns`, `mode`) | `[]` | `history --redact` (require) / `history --unredacted` (disable in `history`) | none |

## Export redaction (`[[export.redaction.rules]]`)

Each rule masks (`mode = "mask"`, the default) or removes (`mode = "remove"`) top-level keys inside `before` and `after` before events reach any export sink or `auditstream history` output (unless `auditstream history --unredacted` is passed):

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `table` | string | exactly one of `table` or `action` | `"table"`, `"schema.table"`, or `"*"` (all `db` and `app` events) |
| `action` | string | exactly one of `table` or `action` | Application action (`op`) for `source = "app"`, or `"*"` (all `app` events) |
| `columns` | non-empty list of strings | yes | Top-level keys in `before` and `after` to redact (dot-paths such as `metadata.ssn` are rejected with exit code `2`) |
| `mode` | `mask` or `remove` | no (`mask`) | `mask` replaces values with `"[redacted]"` and keeps column names in `changed`; `remove` drops keys from `before`, `after`, and `changed`. `remove` overrides `mask` when both match |

- **When to use `capture.exclude` vs. `export.redaction`:** Always put secrets, credentials, password hashes, payment card numbers (PAN/CVV), and API tokens in **`capture.exclude`** so capture triggers never write them into `auditstream.changes`. Use **`export.redaction`** for application events (`auditstream.log_event()`, which has no capture-time column exclusion), retroactive minimization of historical rows, and two-tier exports (keeping full operational rows in PostgreSQL while sending minimized JSONL/CSV feeds to external SIEMs or customers).
- **Top-level keys only & fail-closed:** Rules match top-level JSON object keys only (leaving `record_id`, untouched key order, and arbitrary-precision PostgreSQL `numeric` values intact). If a matching rule encounters a non-object or malformed JSON payload (for example, an `app` event whose `data` is a JSON array), `auditstream export` fails closed with exit code `5` before writing to the sink or advancing the cursor, and `auditstream history` fails closed with exit code `5` before emitting the redaction note or writing to stdout (unless `--omit-payload` or `--unredacted` is passed).
- **`record_id` is never redacted (natural-PK and `app` target PII caveat):** `[[export.redaction.rules]]` intentionally leaves `record_id` unmodified (and `capture.exclude` cannot exclude primary key columns) so downstream consumers and operators can correlate events across operations and reconstruct per-record histories. If an audited table uses a natural PII primary key (such as `email`, `ssn`, or `phone`) — or an `auditstream.log_event()` call places PII inside its `target` JSON object — matching redaction rules mask or remove those keys from `before` and `after` while still outputting `record_id` in plaintext in both `export` and `history`. Prefer surrogate primary keys (such as UUIDs or integer IDs) on audited tables and pass opaque identifiers in `log_event()` `target` payloads when exported streams or history output must exclude PII.

## Example

```toml
schema = "auditstream"

[capture]
tables = ["public.invoice", "public.line_item"]
exclude = ["public.invoice.password_hash"]

[migrations]
format = "prisma"
dir = "prisma/migrations"

[export]
cursor_file = ".auditstream.cursor"
batch_size = 1000

[export.sink]
type = "jsonl"
path = "events.jsonl"

[[export.redaction.rules]]
table = "public.invoice"
columns = ["ssn", "tax_id"]
mode = "mask"

[[export.redaction.rules]]
action = "invoice.exported"
columns = ["recipient_email"]
mode = "remove"
```
