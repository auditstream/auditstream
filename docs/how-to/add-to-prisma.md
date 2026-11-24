# Add Auditstream to a Prisma Project

Generate a Prisma migration for your audited tables, apply it with Prisma Migrate, and attach the Prisma adapter.

## 1. Apply your existing Prisma schema

Ensure your target tables already exist in PostgreSQL so `auditstream init` can introspect primary keys and columns:

```sh
npx prisma migrate deploy
```

## 2. Generate the Auditstream migration

Point `AUDITSTREAM_DATABASE_URL` at your database and run `auditstream init`:

```sh
auditstream init --migrations prisma \
  --table public.invoice \
  --table public.line_item \
  --exclude public.invoice.password_hash
```

`init` writes `<YYYYMMDDHHMMSS>_auditstream/migration.sql` under `prisma/migrations/` and updates `auditstream.toml`.
Pass `--out <dir>` if your migrations live in a different folder.

## 3. Apply and verify the migration

Apply the generated migration with Prisma Migrate, then confirm all triggers and partitions are healthy:

```sh
npx prisma migrate deploy
auditstream verify
```

Re-run `auditstream init` whenever you add or remove audited tables.
Earlier migration files stay untouched, and one new migration updates triggers cleanly.

## 4. Wire the Prisma adapter in application code

Install `@auditstream/node` and `@auditstream/prisma`, then follow the setup guide in the package README:

- [`@auditstream/prisma` README](https://github.com/auditstream/auditstream-js/blob/main/packages/prisma/README.md)
- [`@auditstream/node` README](https://github.com/auditstream/auditstream-js/blob/main/packages/node/README.md)

Under Prisma Accelerate, client extensions do not see raw SQL comments.
Use the transaction-local `set_config` helper (`auditTransaction`) documented in the `@auditstream/prisma` README.
