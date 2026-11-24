# Pooler Verification Suite (`bench/poolers`)

Runs Auditstream's engine-generated capture SQL (`D2` + `D3`) through a connection pooler in transaction mode, verifying:

1. **SQL capture & tamper suite (`T1`–`T9`)** as `app_user` through the pooler (17 expected rows matching `expected_capture.txt`, and exactly 3 `permission denied` tamper denials).
2. **`pgbench` protocol matrix** with 16 clients over a smaller server pool (`default_pool_size = 4`, `max_prepared_statements = 200`) across `-M simple`, `-M extended`, and `-M prepared`, for both SQL comments and transaction-local `set_config` (0 failed transactions, 1 history row per transaction with the exact expected `actor_source`).
3. **Actor leak test**: interleaves `set_config` transactions on `up_t` with actor-less writes on `up_plain` sharing the same pooled server connections, asserting zero actor bleed into `up_plain` and 100% `set_config` attribution on `up_t`.

## Local run (PostgreSQL + PgBouncer 1.26)

Start PostgreSQL and PgBouncer 1.26 in transaction mode (using more clients than `default_pool_size = 4`), then run `bench/poolers/run.sh`:

```sh
docker network create auditstream-poolers
docker run -d --name as-pg --network auditstream-poolers \
  -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=pg -e POSTGRES_DB=postgres \
  -p 5432:5432 postgres:16-alpine

docker run -d --name as-pgbouncer --network auditstream-poolers \
  -e DB_HOST=as-pg -e DB_PORT=5432 -e DB_USER=app_user -e DB_PASSWORD=app \
  -e LISTEN_PORT=6432 -e POOL_MODE=transaction -e DEFAULT_POOL_SIZE=4 \
  -e MAX_PREPARED_STATEMENTS=200 -e MAX_CLIENT_CONN=200 \
  -e AUTH_TYPE=scram-sha-256 -e IGNORE_STARTUP_PARAMETERS=extra_float_digits,options \
  -p 6432:6432 edoburu/pgbouncer:v1.26.1-p0

ADMIN_URL=postgres://postgres:pg@127.0.0.1:5432/postgres \
APP_URL=postgres://app_user:app@127.0.0.1:6432/postgres \
bench/poolers/run.sh

docker rm -f as-pgbouncer as-pg
docker network rm auditstream-poolers
```

Preview the generated capture DDL and triggers directly:

```sh
cargo run -q -p auditstream-pg --example pooler_schema | head -20
```

## Manual pre-release checklist (Neon & Supabase Supavisor)

Managed poolers stay outside automated PR CI and are verified manually before each release:

### 1. Provision & setup
- Create a temporary Neon project (PostgreSQL 18+) or Supabase project (PostgreSQL 17+).
- Export `ADMIN_URL` as the **direct** admin connection string (never pooled) and `APP_URL` as the **pooled** transaction-mode connection string for `app_user` (`APP_PW` matching the password in `APP_URL`).
- Confirm roles are **reused, never dropped** if re-running against an existing project (poolers retain cached sessions for dropped role OIDs).

### 2. Run SQL suite (`T1`–`T9`)
```sh
psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -v app_pw="$APP_PW" -f bench/poolers/fixtures.sql
cargo run -q -p auditstream-pg --example pooler_schema | psql "$ADMIN_URL" -v ON_ERROR_STOP=1
psql "$APP_URL" -f bench/poolers/t_app.sql 2>t_app.err
grep -c "permission denied" t_app.err   # must be 3
diff -u <(LC_ALL=C sort bench/poolers/expected_capture.txt) \
        <(psql "$ADMIN_URL" -At -f bench/poolers/capture.sql | LC_ALL=C sort)
```

### 3. Run `pgbench` & leak test
- **Neon pooler:** run full `bench/poolers/bench.sh neon 24 10` (simple, extended, prepared, and leak test; expect 0 failures and zero leaks).
- **Supabase Supavisor (transaction mode):** run `simple` and `extended` modes plus the leak test (`pgbench -M prepared` fails on Supavisor with `prepared statement "P_0" already exists` both with and without Auditstream; note this in the release checklist rather than treating it as an Auditstream regression).

### 4. Cleanup
- Delete the temporary Neon and Supabase projects (or drop the test schema and tables if using a shared staging branch) and revoke any temporary credentials.
