# `auditstream verify`

> **Public contract:** Flags, finding kinds, JSON output shape, and exit codes are part of the engine public contract governed by [Releases: Version policy](../../handbook/releases.md#version-policy).

`auditstream verify` checks live PostgreSQL capture objects against `capture.tables` in `auditstream.toml` and exits non-zero on any finding.
All catalog checks run inside a `READ ONLY` transaction.
Use `verify` as a CI gate and scheduled health check.

## Synopsis

```sh
auditstream [GLOBAL OPTIONS] verify [--format text|json] [--fix]
```

## Options

| Flag | Values | Default | Description |
| --- | --- | --- | --- |
| `--format` | `text`, `json` | `text` | Output format written to stdout |
| `--fix` | — | off | Create missing monthly partitions 3 months ahead before verifying |

When `capture.tables` is absent or empty, `verify` checks installed triggers only and prints a notice on stderr.

## Checks and finding kinds

| Kind | Severity | Meaning |
| --- | --- | --- |
| `missing` | `error` | History table, capture function, audited table, or trigger does not exist |
| `disabled` | `error` | A trigger (or partition child clone) is disabled (`D`) or replica-only (`R`) |
| `outdated` | `error` | Function body, security settings, or trigger definition/exclusions drifted |
| `partitioned_table` | `warning` | Audited table is partitioned; direct child-partition writes bypass statement triggers |
| `default_partition_rows` | `error` | `changes_default` holds rows, blocking monthly partition attachment |
| `invalid_exclusion` | `error` | A primary key column is listed in `capture.exclude` or on an installed trigger, nullifying `record_id` |
| `insufficient_partitions` | `error` | Fewer than 2 contiguous monthly partitions exist ahead of the current UTC month |

## Output

### Text output

Prints `ok: no findings` when clean.
Otherwise prints one line per finding with severity, table or object, kind, detail, and a fix hint.
With `--fix`, partition creation results are reported on stderr.

### JSON output

```json
{
  "ok": false,
  "schema": "auditstream",
  "months_covered_ahead": 3,
  "created_partitions": [],
  "findings": [
    {
      "severity": "error",
      "kind": "disabled",
      "table": "public.invoice",
      "object": "public.invoice.auditstream_update",
      "detail": "trigger is not enabled for origin writes (tgenabled = 'D', expected 'O' or 'A')",
      "hint": "Re-enable `public.invoice.auditstream_update` or re-apply the Auditstream migration."
    }
  ]
}
```

## Exit codes

- `0`: Verification passed with zero findings.
- `1`: One or more findings were detected.
- `2`: Invalid CLI flags or configuration file.
- `3`: Database unreachable, query failed, or `--fix` partition maintenance failed.
- `4`: Writing output failed.

See [Exit codes](../exit-codes.md) for the full table.

## GitHub Actions example

Gate deployments or run a nightly drift and partition check (configure `sslmode=verify-full` in `AUDITSTREAM_DATABASE_URL` for non-loopback hosts):

```yaml
- name: Verify Auditstream triggers and partitions
  env:
    AUDITSTREAM_DATABASE_URL: ${{ secrets.AUDITSTREAM_DATABASE_URL }}
  run: |
    auditstream verify --fix --format text
```
