# Auditstream: pooler and serverless driver tests

You are helping me check how Auditstream's audit capture behaves behind managed Postgres poolers and serverless drivers. Auditstream records every row change with a Postgres trigger and reads the actor (who made the change) either from a SQL comment at the end of each statement (`/*auditstream='<base64url JSON>'*/`, read with `current_query()`) or from `set_config('auditstream.actor', <json>, true)` inside a transaction. This was already proven on PostgreSQL 15 to 19 and behind PgBouncer 1.26 in transaction mode. What is still unproven is whether the comment and the `set_config` value arrive intact through Neon (serverless HTTP and WebSocket drivers, and the Neon pooler) and through Supabase's Supavisor.

Everything you need is in this folder: `sql/` (schema, SQL suite, expected output, pgbench scripts) and `js/` (Prisma, Drizzle, postgres.js and Neon test harnesses). All of it passed against a local PostgreSQL 16 behind PgBouncer before it reached you.

## Rules

- Work in this folder only. Do not commit, push or publish anything.
- Ask me before creating any account, project or cloud resource, and before anything that costs money. If you need connection strings, tell me exactly which ones and I will put them in a `.env` file here.
- Never print passwords or connection strings, and keep them out of RESULTS.md. Load them from `.env` (for example `set -a; . ./.env; set +a`).
- Do not change the capture logic in `sql/schema.sql` to make a test pass. If a platform refuses a statement (creating roles, changing owners), make the smallest change that works and record the diff. You may fix bugs in the harness under `js/`; list every fix.
- You need Node 22 or newer, and `psql` and `pgbench` from PostgreSQL 15 or newer. Install what is missing and say what you installed.

## Targets, in this order

1. **Neon**, free plan, one project on the default Postgres version. `ADMIN_URL` is the `neondb_owner` connection on the direct host. After setup creates `app_user`, build two app URLs with the same password: `APP_URL_POOLED` on the `-pooler` host and `APP_URL_DIRECT` on the direct host. Keep `sslmode=require`.
2. **Supabase Supavisor**: a free cloud project (preferred), or the Supabase CLI local stack with the pooler enabled in transaction mode. `ADMIN_URL` is the `postgres` role on the session pooler or direct connection. `APP_URL` is `app_user` on the transaction pooler, port 6543. Supavisor expects the user name as `app_user.<project-ref>`.
3. **AWS RDS Proxy**: only if I explicitly say yes. Skip it otherwise.

## For each target

**A. Setup.** Generate a strong random `APP_PW` and keep it in `.env`, then run:

    psql "$ADMIN_URL" -v app_pw="$APP_PW" -f sql/schema.sql

Setup can be re-run: it resets the tables and reuses the roles. It never drops `app_user`, because a pooler keeps sessions logged in as a dropped role and every later statement fails with "permission denied".

**B. SQL suite through the pooler.** Run this right after setup, before anything else touches the tables:

    psql "$APP_URL" -v ON_ERROR_STOP=0 -q -f sql/t_app.sql 2>&1 | grep -c "permission denied"
    mkdir -p out && psql "$ADMIN_URL" -Atf sql/capture.sql > out/capture_<target>.txt
    diff sql/expected_capture.txt out/capture_<target>.txt

Pass means the count is 3 and the diff is empty. For Neon, use `APP_URL_POOLED` here.

**C. ORM and driver suite.** Run `cd js && npm install && npx prisma generate` once. Every run prints a JSON block with `"result": "PASS"` or `"FAIL"`; `ADMIN_URL` stays the admin connection.

- Neon, with `APP_URL=$APP_URL_POOLED`:
  `DRIVER=pg npx tsx prisma-test.ts`, then `MODE=pg`, `MODE=pgjs-noprepare` and `MODE=pgjs-prepare` with `npx tsx drizzle-test.ts`.
- Neon serverless drivers, first with `APP_URL=$APP_URL_DIRECT` and then again with `APP_URL=$APP_URL_POOLED`:
  `DRIVER=neon-ws` and `DRIVER=neon-http` with `prisma-test.ts`; `MODE=neon-ws` and `MODE=neon-http` with `drizzle-test.ts`; and `npx tsx neon-raw.ts`.
- Supabase, with `APP_URL` on the transaction pooler:
  `DRIVER=pg` with `prisma-test.ts`; `MODE=pg`, `MODE=pgjs-noprepare` and `MODE=pgjs-prepare` with `drizzle-test.ts`.

Steps a driver cannot run (for example, interactive transactions over Neon HTTP) are reported as `N/A` in `notes`. That is expected; a FAIL is not.

**D. pgbench through the pooler.** From the kit root:

    sql/bench.sh <target> <clients> 15

Use more clients than the pooler's server-side pool size, so clients queue for server connections. Use 24 on Supavisor (the free tier pool is 15) and 24 on the Neon pooler, plus 64 there if the plan allows it. Each line shows transactions, failures and audit rows by actor source. Pass means every run has 0 failures, audit rows equal to the processed transactions, every row carrying the expected source, and the leak line showing `up_plain` rows only as `none` and `up_t` rows only as `set_config`. Throughput over the internet is not a performance measure; I only need correctness.

If a `-M prepared` run stalls or errors, run the same with a plain transaction to tell a pooler problem from an Auditstream one:

    pgbench -n -M prepared -c <clients> -j 4 -T 15 -f sql/bench_txplain.sql "$APP_URL"

**E. Cleanup.** On each target, drop what setup created:

    DROP SCHEMA auditstream CASCADE; DROP TABLE line_item, invoice, up_t, up_plain;
    DROP OWNED BY app_user; DROP ROLE app_user; DROP ROLE auditstream_owner;

Then tell me which cloud projects I can delete.

## What to give me: RESULTS.md

1. Environment: OS, Node, `psql` and `pgbench` versions. For each target, the output of `SELECT version()`, the region, and the pooler and its version if visible.
2. Every change you made to the kit, as a diff, with the reason.
3. For each target: the B count and the diff result; every JSON block from C, verbatim; every line from `bench.sh`, verbatim; and any `bench_txplain.sql` run.
4. Anything surprising, with the exact error messages.

No passwords or connection strings anywhere in RESULTS.md.
