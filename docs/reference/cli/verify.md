# `auditstream verify`

> **Public contract:** Flags, finding kinds, JSON output shape, and exit codes are part of the engine public contract governed by [Releases: Version policy](../../handbook/releases.md#version-policy).

`auditstream verify` checks live PostgreSQL capture objects against `capture.tables` in `auditstream.toml` and exits non-zero when findings meet or exceed the `--fail-on` severity threshold (default: `error`).
All catalog checks run inside a `READ ONLY` transaction.
Use `verify` as a CI gate and scheduled health check.

## Synopsis

```sh
auditstream [GLOBAL OPTIONS] verify [--format text|json] [--fix] [--fail-on error|warning] [--app-role ROLE]...
```

## Options

| Flag | Values | Default | Description |
| --- | --- | --- | --- |
| `--format` | `text`, `json` | `text` | Output format written to stdout |
| `--fix` | — | off | Create missing monthly partitions 3 months ahead before verifying |
| `--fail-on` | `error`, `warning` | `error` | Minimum finding severity that causes exit code `1` (`warning` fails on advisory warnings too) |
| `--app-role` | PostgreSQL role name | `capture.app_roles` | Repeatable application role checked for RLS bypasses (`SUPERUSER`, `BYPASSRLS`, or `auditstream_owner` membership) when RLS is enabled on `<schema>.changes` |

When `capture.tables` is absent or empty, `verify` checks installed triggers, configured `capture.exclude` entries, and audit schema/history security, and prints a notice on stderr.
The connecting PostgreSQL role needs read access to system catalogs plus `USAGE` on the audit schema and `SELECT` on `<schema>.changes` (for example any configured application role or a superuser). When Row-Level Security is enabled on `<schema>.changes`, verify with a role that has exclusively unobstructed `SELECT` visibility (for example `auditstream_exporter` or `auditstream_verify` with a `PERMISSIVE USING (true)` policy, while scoping tenant `PERMISSIVE` policies to `TO <app_role>` rather than `PUBLIC`) so `changes_default` row counts can be inspected reliably.

## Checks and finding kinds

| Kind | Severity | Meaning |
| --- | --- | --- |
| `missing` | `error` | Owner role (`auditstream_owner`), history table, attached `changes_default` partition, history primary key (`changes_pkey`), required history index, helper function, audited table, or trigger does not exist |
| `disabled` | `error` | A trigger (or partition child clone) is disabled (`D`) or replica-only (`R`) |
| `outdated` | `error` | Function body/security (`SECURITY DEFINER`, `STRICT`, parallel/leakproof/support)/signature/defaults/volatility/`proconfig` (`SET search_path`), history primary key (`changes_pkey`) or index definition/validity/options/opclass, history child partition `relkind` or `DEFAULT` partition name, or trigger definition/exclusions drifted |
| `partitioned_table` | `warning` | Audited table is partitioned; direct child-partition writes bypass statement triggers |
| `default_partition_rows` | `error` | `changes_default` holds rows, blocking monthly partition attachment |
| `invalid_exclusion` | `error` | A primary key column, non-existent (renamed/dropped) column, or unverified table is listed in `capture.exclude` or on an installed trigger |
| `insufficient_partitions` | `error` | Fewer than 2 contiguous monthly partitions exist ahead of the current UTC month |
| `unsafe_privileges` | `error` | Unexpected `PUBLIC` or non-owner privileges exist on the audit schema, `changes`, child partitions, `changes_id_seq`, or helper functions; `auditstream_owner` has unsafe attributes, direct or recursive PG 16+ indirect `ADMIN` memberships, or roles granted to it; or `session_replication_role` is overridden to `replica` or granted to `PUBLIC`/non-superusers |
| `invalid_ownership` | `error` | Audit schema, `changes`, a child partition, `changes_id_seq`, or a helper function is not owned by `auditstream_owner` |
| `tampered_history` | `error` | Unexpected trigger (including internal RI triggers), rewrite rule, child-partition Row-Level Security policy, or unexpected helper function overload in the audit schema |
| `forced_rls` | `error` | `FORCE ROW LEVEL SECURITY` is enabled on `<schema>.changes` or an attached child partition, breaking `auditstream_owner` `SECURITY DEFINER` trigger writes and partition maintenance |
| `rls_without_select_policy` | `error` | Row-Level Security is enabled on `<schema>.changes` with zero `PERMISSIVE` policies covering `SELECT`, causing non-owner reads to silently return 0 rows |
| `rls_hides_default_partition` | `warning` | Row-Level Security is active on `<schema>.changes` for the verifying role without exclusively unobstructed `SELECT` visibility (either filtered/no `USING (true)` policy, a blocking `RESTRICTIVE` policy, or a non-trivial sibling `PERMISSIVE` policy such as `TO PUBLIC`), so `changes_default` row counts cannot be inspected reliably |
| `rls_bypassed_by_app_role` | `warning` | Row-Level Security is enabled on `<schema>.changes`, but a configured application role has `SUPERUSER`, `BYPASSRLS`, or membership in `auditstream_owner` (or the history table owner) and bypasses RLS |

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
      "detail": "trigger `auditstream_update` on `public.invoice` is disabled (`tgenabled = 'D'`)",
      "hint": "Re-enable `public.invoice.auditstream_update` or re-apply the Auditstream migration."
    }
  ]
}
```

## Exit codes

- `0`: Verification passed with zero findings meeting `--fail-on` (warnings still print and exit `0` under the default `--fail-on error`).
- `1`: One or more findings met or exceeded the `--fail-on` threshold.
- `2`: Invalid CLI flags or configuration file.
- `3`: Database unreachable, query failed, or `--fix` partition maintenance failed.
- `4`: Writing output failed.

See [Exit codes](../exit-codes.md) for the full table.

## GitHub Actions example

Install a pinned CLI binary with SHA-256 verification using the reusable subpath action, then run `verify` with a least-privilege `SELECT`-only role (`v0.1.1+`) and `sslmode=verify-full` (see [Check triggers in CI with `verify`](../../how-to/verify-in-ci.md)):

```yaml
- uses: auditstream/auditstream/actions/setup@auditstream-v0.1.1 # pin to a release tag (>= auditstream-v0.1.1) or full commit SHA
  with:
    version: "0.1.1"
- name: Verify Auditstream triggers and partitions
  env:
    AUDITSTREAM_DATABASE_URL: ${{ secrets.AUDITSTREAM_VERIFY_URL }}
  run: auditstream verify --format json
```
