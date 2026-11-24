# Check Triggers in CI with `verify`

Run `auditstream verify` after database migrations and on a schedule to catch trigger drift and partition gaps early.

## Run a verification check

```sh
auditstream verify
```

`verify` inspects live PostgreSQL catalogs inside a `READ ONLY` transaction and compares them against `capture.tables` and `capture.exclude` in `auditstream.toml`.
It exits `0` when clean.
It exits `1` on missing, disabled, or outdated triggers, rows in `changes_default`, or fewer than two future monthly partitions.

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

## GitHub Actions workflow

Run `verify` after migrations in deployment workflows and weekly on a schedule:

```yaml
name: Verify Auditstream

on:
  workflow_dispatch:
  schedule:
    - cron: "0 6 * * 1"

jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Verify triggers and monthly partitions
        env:
          AUDITSTREAM_DATABASE_URL: ${{ secrets.AUDITSTREAM_DATABASE_URL }}
        run: auditstream verify --fix --format text
```

Use `auditstream status` when you want a human-readable health overview that always exits `0` on a reachable database.
See [`auditstream verify`](../reference/cli/verify.md) and [Exit codes](../reference/exit-codes.md) for all finding kinds and exit statuses.
