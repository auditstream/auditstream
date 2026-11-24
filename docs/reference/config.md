# Configuration Reference

> **Public contract:** The `auditstream.toml` schema, keys, defaults and precedence order are part of the engine public contract governed by [Releases: Version policy](../handbook/releases.md#version-policy) and mirrored in `crates/auditstream/src/config.rs`.

## File discovery

1. If `--config <PATH>` or `AUDITSTREAM_CONFIG` is set, that file is loaded; if it does not exist, the CLI exits with code `2` and names the path.
2. Otherwise, `./auditstream.toml` is loaded if it exists in the current working directory.
3. Unknown keys or wrong value types always exit with code `2` and name the offending key.

## Precedence

Values resolve in strict order (first non-empty source wins):

1. Command-line flag
2. Environment variable (global options only, so subcommand flags never collide)
3. Configuration file (`auditstream.toml`)
4. Built-in default

## Schema

| Key | Type | Default | Flag | Environment |
| --- | --- | --- | --- | --- |
| `database_url` | string | none | `--database-url` | `AUDITSTREAM_DATABASE_URL`, then `DATABASE_URL` |
| `schema` | string | `auditstream` | `--schema` | `AUDITSTREAM_SCHEMA` |
| `capture.tables` | list of `schema.table` | `[]` | `init --table` | none |
| `capture.exclude` | list of `schema.table.column` | `[]` | `init --exclude` | none |
| `migrations.format` | `prisma`, `drizzle` or `sql` | none | `init --migrations` | none |
| `migrations.dir` | path | per format | `init --out` | none |
| `export.cursor_file` | path | none | `export --cursor-file` | none |
| `export.batch_size` | integer | `1000` | `export --batch-size` | none |
| `export.sink` | table with `type` (`jsonl` or `csv`) and optional `path` | JSON Lines on stdout | `export --format` overrides `type` and `--output` overrides `path` | none |

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
```
