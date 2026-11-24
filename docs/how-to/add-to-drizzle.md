# Add Auditstream to a Drizzle Project

Generate a Drizzle migration for your audited tables, apply it with Drizzle, and attach the Drizzle adapter.

## 1. Ensure your tables exist in PostgreSQL

Run your existing Drizzle migrations first so `auditstream init` can introspect your live schema:

```sh
npx drizzle-kit migrate
```

## 2. Generate the Auditstream migration

Run `auditstream init --migrations drizzle` with your selected tables and excluded columns:

```sh
auditstream init --migrations drizzle \
  --table public.invoice \
  --table public.line_item \
  --exclude public.invoice.password_hash
```

`init` checks `<out>/meta/_journal.json` (defaulting `--out` to `drizzle/`):

- **Drizzle 0.45:** when `_journal.json` exists, `init` writes `<NNNN>_auditstream.sql` and appends one journal entry.
- **Drizzle 1.x:** otherwise, `init` writes `<YYYYMMDDHHMMSS>_auditstream/migration.sql`.

Every Drizzle file separates statements with `--> statement-breakpoint` comments.

## 3. Apply and verify the migration

Apply the new migration through Drizzle, then verify installed triggers and monthly partitions:

```sh
npx drizzle-kit migrate
auditstream verify
```

## 4. Wire the Drizzle adapter in application code

Install `@auditstream/node` and `@auditstream/drizzle`, then follow the driver-specific setup in the package README:

- [`@auditstream/drizzle` README](https://github.com/auditstream/auditstream-js/blob/main/packages/drizzle/README.md)
- [`@auditstream/node` README](https://github.com/auditstream/auditstream-js/blob/main/packages/node/README.md)

Supported drivers include `node-postgres` (`pg`), `postgres.js`, and Neon HTTP or WebSocket drivers.
