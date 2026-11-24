# auditstream-testkit

Shared PostgreSQL test harness for Auditstream integration tests. This crate is
unpublished (`publish = false`) and must only be taken as a `dev-dependency`.

## What it provides

- `TestDb::new().await` — creates a fresh `auditstream_test_<uuid>` database and
  returns its connection URL and `sqlx::PgPool`. Dropping `TestDb` closes its
  pool and runs `DROP DATABASE ... WITH (FORCE)` from the `postgres` maintenance
  database, including when a test panics.
- `TestDb::non_superuser().await` — creates a fresh `LOGIN CREATEROLE NOSUPERUSER`
  role, creates a database owned by that role, and connects as that role to
  mirror managed PostgreSQL environments (such as Neon and Supabase). Dropping
  `TestDb` drops both the database and the role.
- `TestDb::cluster_lock().await` — acquires a session-level advisory lock in the
  `postgres` maintenance database so concurrent tests that create or drop a
  fixed-name role serialize safely.

## Running locally

### Default (reusable testcontainers container)

With Docker running and `DATABASE_URL` unset, the harness starts or reuses one
named container per PostgreSQL major version (`auditstream-testkit-pg<major>`,
default major `19`):

```sh
cargo nextest run -p auditstream-testkit --test it
```

Select another supported major version (15 through 19) with
`AUDITSTREAM_TEST_PG`:

```sh
AUDITSTREAM_TEST_PG=15 cargo nextest run -p auditstream-testkit --test it
```

To remove a reused container when you are done:

```sh
docker rm -f auditstream-testkit-pg19
```

### Against an existing PostgreSQL server (`DATABASE_URL`)

When `DATABASE_URL` is set, the harness uses that server directly instead of
starting a container. Admin operations always connect to the `postgres`
maintenance database (never `template1`).

```sh
docker run -d --rm --name as-pg16 -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:16
DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres AUDITSTREAM_TEST_PG=16 \
  cargo nextest run -p auditstream-testkit --test it
```
