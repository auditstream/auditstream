# `auditstream status`

> **Public contract:** Flags, text and JSON output fields, and exit codes are part of the engine public contract governed by [Releases: Version policy](../../handbook/releases.md#version-policy).

`auditstream status` summarizes capture coverage across tables in scope and reports history table health from PostgreSQL system catalogs.
It never runs `count(*)` over monthly partitions.

## Synopsis

```sh
auditstream [GLOBAL OPTIONS] status [--format text|json]
```

## Options

| Flag | Values | Default | Description |
| --- | --- | --- | --- |
| `--format` | `text`, `json` | `text` | Output format written to stdout |

## Scope

`status` inspects every user table in the schemas referenced by `capture.tables` in `auditstream.toml`.
When `capture.tables` is empty, it inspects `public` plus any schema holding installed Auditstream triggers.

## Output

### Text output

Lists every table in scope as `captured` or `uncaptured`, with its trigger state (`enabled`, `disabled`, `partial`, or `none`) and excluded columns.
Then prints history table statistics from PostgreSQL catalogs (`pg_class.reltuples`, `pg_total_relation_size`, and `EXISTS (... LIMIT 1)` on `changes_default`).
When drift or partition problems exist, the output ends with a hint to run `auditstream verify`.

### JSON output

```json
{
  "schema": "auditstream",
  "tables": [
    {
      "table": "public.invoice",
      "captured": true,
      "configured": true,
      "trigger_state": "enabled",
      "excluded_columns": ["password_hash"]
    }
  ],
  "history": {
    "exists": true,
    "partitions": [
      "changes_2026_11",
      "changes_2026_12",
      "changes_2027_01",
      "changes_2027_02"
    ],
    "months_covered_ahead": 3,
    "default_has_rows": false,
    "estimated_rows": 0,
    "total_size_bytes": 163840
  },
  "problems_found": 0
}
```

When `problems_found` is greater than zero, the JSON object includes a `hint` field directing operators to `auditstream verify`.

## Exit codes

- `0`: Database read succeeded (even when problems are present).
- `2`: Invalid CLI flags or configuration file.
- `3`: Database unreachable or catalog query failed.
- `4`: Writing output failed.

See [Exit codes](../exit-codes.md) for the full table.
