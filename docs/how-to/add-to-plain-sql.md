# Add Auditstream to a Plain SQL Project

Generate plain SQL migration files, apply them with `psql`, and keep your table selection in `auditstream.toml`.

## 1. Preview or generate the SQL migration

Inspect the generated SQL without writing files by adding `--dry-run`:

```sh
auditstream init --migrations sql \
  --table public.invoice \
  --exclude public.invoice.password_hash \
  --dry-run
```

Write the migration file under `migrations/` (or pass `--out <dir>`) and save your selection to `auditstream.toml`:

```sh
auditstream init --migrations sql \
  --table public.invoice \
  --exclude public.invoice.password_hash
```

Plain SQL migrations are wrapped in `BEGIN; ... COMMIT;`.

## 2. Apply and verify

Apply the generated file with `psql` and verify the live installation:

```sh
psql "$AUDITSTREAM_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/*_auditstream.sql
auditstream verify
```

## 3. Change audited tables later

Pass the updated `--table` and `--exclude` flags to `auditstream init`:

```sh
auditstream init --migrations sql \
  --table public.invoice \
  --table public.line_item \
  --exclude public.invoice.password_hash
```

`init` leaves previous migrations untouched and writes one new migration.
Removed tables receive `DROP TRIGGER IF EXISTS` while keeping all existing rows in `auditstream.changes`.
Re-running with the same selection and generated function and trigger definitions prints `up to date` to stderr and exits `0`.
