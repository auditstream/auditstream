# Adapters (TypeScript)

The auditstream-js repository publishes three ESM-only packages, released together. `@auditstream/node` owns the actor context; the ORM packages only hand that actor to Postgres.

| Package | Does | Peer dependencies |
| --- | --- | --- |
| `@auditstream/node` | Actor context (`AsyncLocalStorage`, `run()` only), `audit.as()`, `audit.log()`, history helpers. Framework entry points `/express`, `/hono`, `/next`. | `express`, `hono`, `next`, all optional |
| `@auditstream/prisma` | A Prisma SQL comments plugin (Prisma 7.1+) that returns one `auditstream` tag per statement. Tested on 7.10.1. Fallback: `set_config` inside an interactive transaction. | `@prisma/client >=7.1.0 <8`, `@auditstream/node` |
| `@auditstream/drizzle` | Drizzle 1.x `.comment()` where a query allows it; for Drizzle 0.45, a wrapper for the `pg` pool or the postgres.js client that tags each statement. All three tested, directly and through PgBouncer. | `drizzle-orm ^0.45.2 \|\| >=1.0.0-rc.1 <2.0.0`, `@auditstream/node` |

Rules every adapter follows:

- **Scopes await inside.** `audit.as()` awaits the callback inside the `AsyncLocalStorage` scope. Prisma queries are lazy and only run when awaited, so a query returned unawaited from the scope lost its actor in all 7 cases tested.
- **Read the actor where the caller is.** A driver wrapper reads the actor when `pool.query` is called and never tags the same statement twice. `pg-pool` can run a queued callback in the context of whichever request freed a connection; reading the actor inside `client.query` gave 4 of 20 concurrent writes the wrong actor in testing.
- **No named prepared statements with per-actor comments.** Each distinct comment would become a new statement that node-postgres never evicts, so wrappers drop statement names when they add a tag. Supavisor in transaction mode rejected pgbench's named statements (prepared statement "P\_0" already exists) with or without Auditstream, while postgres.js with prepare on passed there; [the test kit](test-kit/README.md) keeps both cases.
- **ESM only.** A dual ESM and CommonJS build can load the package twice, and two `AsyncLocalStorage` instances would hide the actor. For the same reason `@auditstream/node` is a peer dependency of the adapters.
- **Node.js.** Published packages support 22.12 or newer; CI runs 22, 24 and 26. Contributors develop on Node 26, because tsdown 0.23 needs 22.18 or newer and npm 12 needs 22.22 or newer. Node 22 is dropped in a major release after its end of life on 2027-04-30.
- **Runtimes.** Node is the reference; Bun and Deno run in CI. Cloudflare Workers need the `nodejs_als` flag, or a compatibility date of 2026-08-04 or later, where Node compatibility is on by default; `enterWith()` does not exist there, so the adapter uses only `run()`. In Next.js 16, data leaves `proxy` (the former middleware) only through headers, cookies, rewrites, redirects or the URL, so the adapter wraps Route Handlers and Server Actions.
- **Supported ORM versions.** Prisma 7.1 and newer within 7.x; Prisma 6 is not supported, and the `prisma` CLI is never a peer, because npm `latest` points to the Prisma 8 release candidate. Drizzle 0.45 and 1.x.
- **No capture logic in TypeScript.** Adapters never build audit rows for database changes; the triggers do. `audit.log()` is the only place where an adapter writes an event, through `auditstream.log_event`.
- **README for each package:** purpose, install with peers, supported Node, ORM and runtime versions, a quick start, how the context propagates and where it stops (edge runtimes, transactions), and a link to the engine.
