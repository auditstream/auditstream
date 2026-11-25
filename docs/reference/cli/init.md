# `auditstream init`

> **Public contract:** Flags, default output directories, file layouts and exit codes are part of the engine public contract governed by [Releases: Version policy](../../handbook/releases.md#version-policy).

`auditstream init` introspects your PostgreSQL database, validates the selected tables and excluded columns, and writes an idempotent capture migration.
It writes migration files and updates `auditstream.toml` only.
Apply the generated files using your project's own migration tool (Prisma Migrate, Drizzle, or `psql`).

## Synopsis

```sh
auditstream [GLOBAL OPTIONS] init [OPTIONS]
```

## Options

| Flag | Value | Default | Description |
| --- | --- | --- | --- |
| `--migrations` | `prisma`, `drizzle`, or `sql` | `migrations.format` in config, or autodetected from workspace | Target migration format |
| `--table` | `<[schema.]table>` | `capture.tables` in config | Table to audit (repeatable; unqualified names default to `public`) |
| `--exclude` | `<schema.table.column>` | `capture.exclude` in config | Column omitted from `before` and `after` images (repeatable) |
| `--out` | `<dir>` | Format default (below) | Output directory for generated migration files |
| `--dry-run` | — | off | Print generated SQL on stdout without writing any files or changing config |

Flags override the corresponding keys in `auditstream.toml`.
Unqualified CLI `--table <name>` values resolve to `public.<name>` and are written normalized as `public.<name>` in `auditstream.toml`.
Tables read from `auditstream.toml` (`capture.tables`) and `--exclude` entries require explicit schema qualification.

## Migration layout autodetection

When neither `--migrations` nor `migrations.format` in `auditstream.toml` is set, `init` inspects the current working directory:

- `prisma/` directory or `prisma.config.{ts,js,mjs,cjs,mts,cts}` -> `prisma`
- `drizzle/` directory or `drizzle.config.{ts,js,mjs,cjs,mts,cts,json}` -> `drizzle`
- `migrations/` directory (when neither Prisma nor Drizzle markers exist) -> `sql`

If both Prisma and Drizzle markers exist, or if no layout is detected, `init` exits with code `2` and prompts you to pass `--migrations prisma|drizzle|sql`.

## Default directories and file layouts

| Format | Default `--out` | File layout |
| --- | --- | --- |
| `prisma` | `prisma/migrations` | `<out>/<YYYYMMDDHHMMSS>_auditstream/migration.sql` |
| `drizzle` (0.45) | `drizzle` | `<out>/<NNNN>_auditstream.sql` plus updated `<out>/meta/_journal.json` |
| `drizzle` (1.x) | `drizzle` | `<out>/<YYYYMMDDHHMMSS>_auditstream/migration.sql` (no journal file) |
| `sql` | `migrations` | `<out>/<YYYYMMDDHHMMSS>_auditstream.sql` wrapped in `BEGIN; ... COMMIT;` |

Drizzle layout detection checks for `<out>/meta/_journal.json`.
When that file exists, `init` writes the 0.45 numbered format and appends one journal entry.
Otherwise it writes the Drizzle 1.x folder format.
All Drizzle files separate statements with `--> statement-breakpoint` comments.

## Configuration file updates

When writing a new migration, `init` creates `auditstream.toml` (or the `--config` path) if it does not exist.
When the file already exists, `init` updates only `[capture]` and `[migrations]` using `toml_edit`.
Comments, formatting, `schema`, and `[export]` remain untouched.
`init` never writes `database_url` or any secret into `auditstream.toml`.

## Validation and re-run behavior

- **No tables selected:** Lists all introspected user tables on stderr and exits `2`.
- **Unknown schema, table, or column:** Names the invalid entry on stderr and exits `2`.
- **Same selection re-run:** Compares against the manifest in the newest Auditstream migration in `--out`. Writes nothing, prints `up to date` on stderr, and exits `0`.
- **Changed selection:** Leaves earlier migrations byte-identical and writes one new migration. Removed tables receive `DROP TRIGGER IF EXISTS` statements while keeping their recorded history.

## Streams and exit codes

- **stdout:** Prints each written migration path (one per line), or the rendered SQL under `--dry-run`.
- **stderr:** Prints diagnostics, available table lists, and `up to date` notices.
- **Exit codes:** `0` on success, `2` on invalid flags/config or unknown tables/columns, `3` on database errors, `4` on filesystem errors. See [Exit codes](../exit-codes.md).

## Examples

Preview plain SQL without writing files:

```sh
auditstream init --migrations sql \
  --table public.invoice \
  --exclude public.invoice.password_hash \
  --dry-run
```

Generate a Prisma migration and save the selection in `auditstream.toml`:

```sh
auditstream init --migrations prisma \
  --table public.invoice \
  --table public.line_item \
  --exclude public.invoice.password_hash
```
