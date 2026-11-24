# auditstream-pg

Postgres adapter crate for the Auditstream Rust engine.

## Owns

- Everything that speaks to Postgres
- Schema introspection and drift checks (`verify`)
- History schema DDL, partition maintenance and trigger generation
- Migration writers (Prisma, Drizzle, plain SQL)
- History reader with commit-safe `xid8` snapshot window cursor

## Must not depend on

- `auditstream-export`
- `auditstream` (the CLI binary)

May depend on `auditstream-core`, `sqlx` (runtime API only, no `query!` macros), and `tokio`.
