# Evidence

Every load-bearing claim in [the handbook](README.md) was re-checked on 2026-11-24, by experiment or against its primary source. Seven experiments and ten source checks changed the handbook; everything else was confirmed as written.

## Experiments

PostgreSQL 16.15, Node 22.23, 4 vCPUs, local socket. pgbench ran 4 clients for 12 to 15 seconds per scenario.

| Test | Result | Changed in the handbook |
| --- | --- | --- |
| Actor in a sqlcommenter tag next to `traceparent` | Captured | No |
| Precedence of `set_config`, comment and JWT claims | `set_config` won over a comment; JWT `sub` used when neither was present | No |
| Spoofing: comment-like text inside string literals, twice | Ignored both times | No |
| Malformed payloads: bad characters, valid base64 of non-JSON | Statement succeeded; actor stored as NULL | Rule: a malformed payload never aborts the write |
| Regex with a `{1,700}` length limit | Postgres rejected it (repetition counts stop at 255) and the error aborted the user's INSERT | Length checked outside the regex; every trigger path covered by tests |
| Application role writing to the history table | INSERT, UPDATE and DELETE denied; its own writes still captured through the SECURITY DEFINER function | No |
| Agent on behalf of a user, bulk update, cascade delete, TRUNCATE, no-op update, excluded-column-only update | All recorded or skipped as specified; the password column never appeared | No |
| A row lands in the DEFAULT partition, then that month's partition is created | `ERROR: updated partition constraint for default partition would be violated`; repair needed detach, move and re-attach | Partitions created three months ahead; `verify` fails when DEFAULT holds rows |
| Export cursor with transactions committing out of order | `id > last_id` lost a row; the xid8 window exported it | Cursor is the xid8 window |
| Single-row UPDATE, full design | 11,105 tps without capture; 6,630 with the comment (−40%); 5,825 with `set_config` (−48%) | Cost corrected from 27% |
| Same, cost breakdown | Dropping the `(table_name, record_id, occurred_at)` btree raised throughput from 6,269 to 7,436 tps | Lookup index shape left to the benchmark issue |
| 10,000-row INSERT in one statement | 26 ms without capture; 541 to 550 ms row-level; 155 to 180 ms statement-level | INSERT and DELETE use statement-level triggers |
| Single-row INSERT | 9,572 tps without capture; 6,913 row-level; 7,906 statement-level | Same |
| Prisma 7.10.1, adapter-pg, official comments plugin: create, nested create, `createMany` (40 of 40 rows), interactive and batch transactions, `$executeRaw`, `upsert`, `updateMany`, delete with cascade | Every row captured with the right actor | No |
| Prisma query returned from `als.run()` without being awaited inside the scope | Actor lost on all 7 operations awaited outside the scope; all captured once the scope awaited inside | `audit.as()` awaits inside its scope; conformance test added |
| Prisma, 40 concurrent requests, 80 updates | 0 wrong actors | No |
| Drizzle 1.0.0-rc.6 `.comment()` | Captured | No |
| Drizzle 0.45.4 with a `pg` client wrapper reading the actor inside `client.query` | 4 of 20 concurrent updates got another request's actor | Wrapper reads the actor at `pool.query` entry and never re-tags; 0 of 60 wrong after the change |

### Second round: open items from the first round

Run the same day on PostgreSQL 15.20, 16.15, 17.12, 18.7 and 19.0, PgBouncer 1.26.1, postgres.js 3.4.7 and Drizzle 0.45.4.

| Test | Result | Changed in the handbook |
| --- | --- | --- |
| Capture, tamper, partition and cursor suite on PostgreSQL 15, 17, 18 and 19 | Output identical to PostgreSQL 16, row for row (17 audit rows); same tamper denials, same DEFAULT-partition error, same cursor result | Version range confirmed |
| Statement-level UPDATE with primary-key pairing | 10,000-row update: 535 to 605 ms, against 599 to 665 ms row-level; single-row: 7,173 against 7,455 tps; a primary-key change split into two half-events | UPDATE stays row-level |
| Prisma 7.10 through PgBouncer in transaction mode, pool of 5 | Every write path captured; 80 concurrent updates, 0 wrong actors | Proven |
| Drizzle 0.45 `pg` wrapper and Drizzle 1.0 `.comment()` through PgBouncer | Every write captured; 0 wrong actors in two runs | Proven |
| Comment transport with named prepared statements through PgBouncer, 8 clients on a pool of 5 | 0 failed transactions | Proven |
| `set_config` with named prepared statements through PgBouncer, 8 clients on a pool of 5 | Stalled under contention (`query_wait_timeout`). A plain BEGIN, UPDATE, COMMIT stalls the same way in pgbench, while postgres.js with named statements on 16 connections over a pool of 5 ran 41,604 transactions with no failure: a pgbench and PgBouncer interaction, not a transport problem | The `set_config` path needs no special rule |
| postgres.js wrapper for Drizzle 0.45 (`unsafe`, `begin`, `savepoint`), direct with prepare on and off, and through PgBouncer | Inserts, a transaction with a savepoint and 40 concurrent updates captured; 0 wrong actors in all three setups | Proven |

A leak test through PgBouncer ran set_config transactions and actor-less updates on the same pooled connections for 15 seconds: 9,436 set_config rows and 9,312 actor-less rows, none crossed over.

### Third round: Neon and Supabase

Run on 2026-11-24 by a local agent with [the test kit](test-kit/README.md) (SQL suite, Prisma, Drizzle, postgres.js and Neon harnesses, pgbench scripts) on a free Neon project (PostgreSQL 18.7, AWS us-east-1) and a free Supabase project (PostgreSQL 17.12, AWS us-east-1). Both were cleaned up afterwards. RDS Proxy was left out on purpose.

| Test | Neon | Supabase Supavisor |
| --- | --- | --- |
| SQL suite through the pooler | Output identical to the expected 17 rows; 3 tamper attempts denied | Same |
| Prisma 7.10 with adapter-pg through the pooler | Pass | Pass |
| Drizzle 0.45 with the `pg` wrapper and with postgres.js (prepare on and off), and Drizzle 1.0 `.comment()` | Pass in all three | Pass in all three |
| Prisma and Drizzle on the Neon WebSocket and HTTP drivers, direct and pooled | Pass; interactive transactions are not available over HTTP | Does not apply |
| Neon drivers without an ORM: comment, set_config in an HTTP and in a WebSocket transaction, and the next statement without an actor | Pass, direct and pooled | Does not apply |
| pgbench, comment and set_config, simple and extended protocol, more clients than server connections | Pass at 24 and 64 clients: no failures, one audit row per transaction, each with the right source | Pass at 24 clients on a pool of 15 |
| pgbench with named prepared statements | Pass; at 64 clients the set_config run slowed to 64 transactions in 15 seconds, all correct, likely the same pgbench interaction that stalls plain transactions behind PgBouncer | Aborted with `prepared statement "P_0" already exists`; a plain transaction without Auditstream failed the same way |
| Leak test | No actor crossed | No actor crossed |

Two fixes came back. The kit's setup now grants the owner role to a non-superuser admin before creating objects owned by it, because from PostgreSQL 16 the creator of a role holds only ADMIN on it; the migration generator has to do the same. The Prisma harness no longer depends on a step the HTTP adapter cannot run.

## Source checks

Fifty-two claims were checked against their primary sources, and 57 package, runtime and database versions against the npm, crates.io, Node.js and PostgreSQL release records. All versions matched. These ten claims were corrected or made more precise:

| Claim as first written | What the source says | Source |
| --- | --- | --- |
| The kernel dropped `Assisted-by:` because few commits used it | Removed on 13 October 2026 because "the tag does not provide the signal it was kept for"; disclosure moved to the cover letter | [commit 1e035297](https://github.com/torvalds/linux/commit/1e035297ffdadf2184c3c773b8c098e372324aaf) |
| Claude Code needs a CLAUDE.md that imports `@AGENTS.md` | AGENTS.md works alone; a CLAUDE.md or CLAUDE.local.md switches it off by default | [Claude Code memory](https://code.claude.com/docs/en/memory) |
| GitHub's terms make a CLA unnecessary | Section D.6 licenses contributions under the repository's license; it grants no relicensing right and does not rule on CLAs | [GitHub terms of service](https://docs.github.com/en/site-policy/github-terms/github-terms-of-service) |
| Organization rulesets need Enterprise | Available on GitHub Team since June 2025 | [changelog](https://github.blog/changelog/2025-06-16-organization-rulesets-now-available-for-github-team-plans/) |
| Any file in `.github/ISSUE_TEMPLATE` stops template inheritance | Only valid templates or a template config do | [default community health files](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/creating-a-default-community-health-file) |
| npm trusted publishing handles every publish | It cannot create a package; the first publish of a new name is manual or staged | [npm trust](https://docs.npmjs.com/cli/v11/commands/npm-trust) |
| `set_config(..., true)` is safe behind every pooler | It pins the session on RDS Proxy; `SET LOCAL` and comments do not | [RDS Proxy pinning](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/rds-proxy-pinning.html) |
| Cloudflare Workers need `nodejs_compat` | The `nodejs_als` flag is enough; Node compatibility is on by default from compatibility date 2026-08-04 | [Cloudflare AsyncLocalStorage](https://developers.cloudflare.com/workers/runtime-apis/nodejs/asynclocalstorage/) |
| Next.js says context set in `proxy` does not reach route handlers | Next.js says to pass data through headers, cookies, rewrites, redirects or the URL, and not to rely on shared modules or globals | [Next.js proxy](https://nextjs.org/docs/app/api-reference/file-conventions/proxy) |
| Prisma Accelerate does not pass SQL comments | Plugins do not receive the SQL under Accelerate; whether comments reach the database is not documented | [Prisma SQL comments](https://www.prisma.io/docs/orm/prisma-client/observability-and-logging/sql-comments) |

One implication came from the version checks: tsdown 0.23 needs Node 22.18 or newer and npm 12 needs 22.22 or newer, so contributors develop on Node 26 even though the published packages support 22.12.
