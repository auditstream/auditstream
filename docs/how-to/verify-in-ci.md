# Check Triggers in CI with `verify`

Run `auditstream verify` after database migrations and on a schedule to catch trigger drift and partition gaps early.

## Run a verification check

```sh
auditstream verify
```

`verify` inspects live PostgreSQL catalogs inside a `READ ONLY` transaction and compares them against `capture.tables` and `capture.exclude` in `auditstream.toml`.
It exits `0` when no findings meet the `--fail-on` threshold (by default `--fail-on error`, so advisory warnings such as `partitioned_table` print without failing CI; pass `--fail-on warning` to fail on warnings too).
It exits `1` on missing `auditstream_owner`, missing, disabled, or outdated triggers or functions (`resolve_actor`, `capture`, `log_event`), primary-key, renamed/dropped column, or unverified table exclusions (`invalid_exclusion`), unsafe privileges, `auditstream_owner` role attributes/memberships, or `session_replication_role` parameter grants/`replica` overrides (`unsafe_privileges`), invalid ownership (`invalid_ownership`), history tampering such as triggers, rewrite rules, Row-Level Security, or sibling function overloads (`tampered_history`), missing or detached `changes_default`, non-regular history child partitions, missing/outdated `changes_pkey` or required history indexes, rows in `changes_default`, or fewer than two future monthly partitions.

Pass `--format json` for structured output:

```sh
auditstream verify --format json
```

## Maintain monthly partitions automatically

Pass `--fix` to create missing monthly partitions three months ahead before running the verification check:

```sh
auditstream verify --fix
```

Use `--fix` in scheduled maintenance jobs with a role authorized to create partitions under `auditstream_owner`.

## Least-privilege verification role

For read-only CI checks (without `--fix`) on Auditstream `v0.1.1+`, create a dedicated PostgreSQL role that can read system catalogs and `auditstream.changes` without write access:

```sql
CREATE ROLE auditstream_verify
  LOGIN PASSWORD '<strong-password>'
  NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT;

GRANT CONNECT ON DATABASE <your_db> TO auditstream_verify;
GRANT USAGE ON SCHEMA auditstream TO auditstream_verify;
GRANT SELECT ON auditstream.changes TO auditstream_verify;
```

- Run the `GRANT USAGE` and `GRANT SELECT` statements as a PostgreSQL superuser, or on managed PostgreSQL (`CREATEROLE` non-superuser such as RDS, Cloud SQL, Neon, or Supabase) inside a transaction that temporarily elevates to `auditstream_owner` and revokes the temporary membership before committing (so `verify` does not report `unsafe_privileges` on `auditstream_owner`).
- Parent-only `SELECT` on `auditstream.changes` requires CLI `v0.1.1+` (`v0.1.0` queried `ONLY auditstream.changes_default` directly).
- Do **not** grant `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `WITH GRANT OPTION`, or `auditstream_owner` membership to `auditstream_verify`.
- Do **not** grant privileges directly on child partitions (`auditstream.changes_default` or `auditstream.changes_YYYY_MM`) — `verify` probes `changes_default` through the parent table `auditstream.changes` and flags direct child-partition grants as `unsafe_privileges`.
- Always enforce verified TLS on non-loopback hosts by setting `sslmode=verify-full` (and `sslrootcert=/path/to/ca.crt` when using a custom CA) in `AUDITSTREAM_DATABASE_URL`:

```text
postgresql://auditstream_verify:<password>@db.example.com:5432/<your_db>?sslmode=verify-full
```

For scheduled partition maintenance (`auditstream verify --fix`), use a separate job with a direct (non-pooled) migration connection string authorized to manage partitions under `auditstream_owner`.

## GitHub Actions workflow

Use the reusable `auditstream/auditstream/actions/setup` composite action to install a pinned CLI release binary with SHA-256 verification before running `auditstream verify`.
Pin both the action ref (release tag or commit SHA) and the `version` input:

```yaml
name: Verify Auditstream

on:
  workflow_dispatch:
  schedule:
    - cron: "0 6 * * 1"

permissions: {}

jobs:
  verify:
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false
      - uses: auditstream/auditstream/actions/setup@auditstream-v0.1.1 # pin to a release tag (>= auditstream-v0.1.1) or full commit SHA
        with:
          version: "0.1.1"
      - name: Verify triggers and monthly partitions
        env:
          AUDITSTREAM_DATABASE_URL: ${{ secrets.AUDITSTREAM_VERIFY_URL }}
        run: auditstream verify --format json
```

To enforce an additional supply-chain pin in your own workflow, pass the expected archive digest via `sha256: "<64-hex-digest>"` from the release `sha256.sum` file (Linux runners download `auditstream-<arch>-unknown-linux-musl.tar.xz`).
For scheduled `--fix` runs, point `AUDITSTREAM_DATABASE_URL` at your direct migration secret (`sslmode=verify-full`) and run `auditstream verify --fix --format text`.

Use `auditstream status` when you want a human-readable health overview that always exits `0` on a reachable database.
See [`auditstream verify`](../reference/cli/verify.md) and [Exit codes](../reference/exit-codes.md) for all finding kinds and exit statuses.
