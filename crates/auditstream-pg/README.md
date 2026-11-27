# auditstream-pg

PostgreSQL adapter crate for the Auditstream Rust engine.

## Owns

- Everything that speaks to PostgreSQL
- Schema introspection and drift checks (`verify`)
- History schema DDL, partition maintenance and trigger generation
- Migration writers (Prisma, Drizzle, plain SQL)
- History reader with commit-safe `xid8` snapshot window cursor

## Must not depend on

- `auditstream-export`
- `auditstream` (the CLI binary)

May depend on `auditstream-core`, `sqlx` (runtime API only, no `query!` macros), `tokio`, and `tracing`.

See the [repository README](../../README.md) and [auditstream.dev/docs](https://auditstream.dev/docs) for full documentation.
