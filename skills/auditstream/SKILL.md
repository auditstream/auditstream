---
name: auditstream
description: Configure and operate Auditstream (transactional PostgreSQL audit trail engine, CLI, and TypeScript adapters for Prisma, Drizzle, Express, Hono, Next.js, and AI agents) without breaking capture triggers, connection poolers, RLS, or privacy boundaries.
---

# Auditstream Agent Skill

Auditstream is a transactional PostgreSQL audit trail — built for humans and AI agents. Database triggers capture row changes into `auditstream.changes` inside PostgreSQL; application code supplies only who acted.

## Authoritative references

- **Exact CLI flags:** run `auditstream --help` or `auditstream <cmd> --help` (`init`, `status`, `verify`, `history`, `export`). Do not memorize flags from stale examples.
- **Documentation & exit codes:** `https://auditstream.dev/docs` (CLI exit codes at `https://auditstream.dev/docs/cli/`; LLM index: `https://auditstream.dev/llms.txt`).
- **Supported versions:** Engine CLI `0.1.x` (`v0.1.1+` for RLS guards and `history` redaction), Spec `0.1.0`–`0.1.1`, TypeScript SDK `@auditstream/*` `0.2.x`, PostgreSQL `15`–`19`, Node.js `22.12+`, `@prisma/client` `7.1+` (`7.x`; Prisma 6 is unsupported), Drizzle `0.45` and `1.x`.

## 1. Before generating migrations (`auditstream init`)

1. **Confirm scope with the developer first:** ask which tables belong in `capture.tables` and which columns belong in `capture.exclude` (password hashes, secrets, API tokens, payment card numbers, and PII that should not be stored) before running `auditstream init`.
2. **Never exclude primary key columns:** `capture.exclude` forbids primary keys so `record_id` is never nullified (`auditstream verify` flags them as `invalid_exclusion`).
3. **Apply migrations over a direct, transactional connection:** migrations require one transaction for lock bounds, DDL atomicity, and revoking temporary `auditstream_owner` elevation. Never apply migrations through stateless per-statement runners such as the Drizzle Neon HTTP migrator.
4. **Always verify after migrating:** run `auditstream verify` (or `auditstream verify --format json`) after applying any Auditstream or schema migration.

## 2. Actor and context contract (IDs only, 512-byte limit)

- **Valid `actor.type` values:** `"user"`, `"api_key"`, `"ai_agent"`, `"service"`, `"system"`.
- **IDs only — never PII or prompts:** trailing SQL comments appear in `pg_stat_activity`, server logs, and `pg_stat_statements`. Never place names, emails, prompts, or LLM tool arguments in `actor` or `context`. `context.ip` and `context.userAgent` are opt-in only.
- **Hard 512-byte UTF-8 budget:** compact serialized JSON for `actor` plus `context` must not exceed `512` bytes, and keys/strings must contain no NUL (`U+0000`) or lone UTF-16 surrogates. The TypeScript SDK throws (`PAYLOAD_TOO_LARGE`, `INVALID_ACTOR`, `INVALID_CONTEXT`) before SQL runs; malformed payloads reaching PostgreSQL triggers never abort the write and instead record `actor = NULL, actor_source = 'invalid'`.
- **`onBehalfOf` depth limit:** strictly one level deep (`onBehalfOf` inside `onBehalfOf` is invalid).
- **Server-side trust boundary:** always resolve `actor.id`, `onBehalfOf`, and `tenant` from a verified session, signed cookie, or validated JWT — never from unverified client headers or LLM tool `arguments`.
- **Validate client correlation IDs:** bound `context.requestId` (for example `/^[A-Za-z0-9._-]{1,64}$/`) before passing it to `audit.as()` so oversized headers cannot trigger `PAYLOAD_TOO_LARGE`.

## 3. Transport, connection poolers, and `AsyncLocalStorage`

- **Default transport:** use trailing SQL comments (`/*auditstream='...'*/`) for normal OLTP writes. Trailing comments need no explicit transaction and do not pin AWS RDS Proxy sessions.
- **Transaction-local `set_config` only:** when using `set_config`, always pass `is_local = true` inside an explicit transaction:

  ```sql
  BEGIN;
  SELECT pg_catalog.set_config('auditstream.actor', '<compact-json>', true);
  -- audited writes here
  COMMIT;
  ```

  Never run session-level `SET auditstream.actor = ...`, which leaks actors across pooled connections.
- **Never combine per-actor comments with named prepared statements:** varying comment text per actor causes unbounded server-side prepared statement cache growth.
- **Always `await` queries inside `audit.as()`:** fire-and-forget queries or lazy Prisma/Drizzle queries awaited after `audit.as()` resolves run outside the `AsyncLocalStorage` scope.
- **AI agents and async streams:** wrap mutating tool handlers (Vercel AI SDK, LangGraph, write-side MCP servers) directly at the leaf tool handler (`execute` / tool callback) with `audit.as({ type: "ai_agent", id, onBehalfOf, tenant }, ..., { context: { requestId, source } })`. Keep prompts and tool I/O in your LLM observability store and correlate via `requestId`.

## 4. TypeScript SDK and adapter rules (`@auditstream/*` `0.2.x`)

- **Core (`@auditstream/node`, ESM-only):** install `@auditstream/node` explicitly alongside any ORM adapter so all packages share one `AsyncLocalStorage`. Provides `audit.as()`, `audit.current()`, `audit.log()`, `audit.history()`, and `audit.query()` (`audit.query()` requires a bounding filter — `recordId`, `since`, or `until` — unless `unbounded: true` is passed). On Cloudflare Workers, enable the `nodejs_als` flag or compatibility date `2026-08-04` or later.
- **Express (`@auditstream/node/express`) and Hono (`@auditstream/node/hono`):** both export `auditstream({ actor, context })`; mount Express middleware *after* authentication populates the verified user.
- **Prisma (`@auditstream/prisma`, `@prisma/client` `7.1+` in `7.x`):**
  - Register `comments: [auditstream()]` on `new PrismaClient({ adapter, comments: [auditstream()] })`.
  - Never configure `PrismaPg`'s `statementNameGenerator` alongside SQL comments.
  - Under **Prisma Accelerate** (or proxies that strip SQL comments), use `auditTransaction(prisma, async (tx) => ...)` instead of SQL comments.
- **Drizzle (`@auditstream/drizzle`, `0.45` and `1.x`):**
  - Use `wrapPgPool()` (`@auditstream/drizzle/pg`) for `pg.Pool` / Neon WebSocket, `wrapPostgresJs()` (`@auditstream/drizzle/postgres-js`) for `postgres.js`, `wrapNeonHttp()` (`@auditstream/drizzle/neon-http`) for Neon HTTP runtime queries, or `auditComment()` (`@auditstream/drizzle`) on Drizzle 1.x builders (still wrap the driver for raw `db.execute()` and transactions).
  - `wrapPostgresJs()` forces `prepare: false` only on statements it tags via `unsafe()`, not on untagged statements or queries already tagged by `auditComment()`. Always configure `prepare: false` on `postgres(...)` behind transaction-mode poolers (PgBouncer, Supavisor, Neon pooler) and whenever combining `postgres.js` with `auditComment()`.
  - Never use stateless Neon HTTP for DDL migrations.
- **Next.js 16 (`@auditstream/node/next`):**
  - `AsyncLocalStorage` does not cross Next.js 16 `proxy` (formerly `middleware`). If `proxy` forwards identity in a header, it must strip or overwrite that header on every incoming request.
  - Wrap Route Handlers with `withAudit()` and Server Actions with `auditAction()`.

## 5. Security, Row-Level Security (RLS), and BFF rules

- **Least-privilege roles:** grant application roles only `USAGE` on `auditstream`, `SELECT` on parent `auditstream.changes`, and `EXECUTE` on `auditstream.log_event(text, jsonb, jsonb)` (revoke `log_event` from untrusted client-reachable roles such as PostgREST or Neon Authorize `authenticated`). Never grant `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, or `WITH GRANT OPTION` on `auditstream.changes`, and never run `GRANT SELECT ON ALL TABLES IN SCHEMA auditstream` or grant privileges on child partitions (`changes_default`, `changes_YYYY_MM`) or `changes_id_seq`.
- **Never enable `FORCE ROW LEVEL SECURITY`:** `auditstream.capture()` and `auditstream.log_event()` run `SECURITY DEFINER` as `auditstream_owner`. `FORCE ROW LEVEL SECURITY` breaks audited writes with SQLSTATE `42501` and fails `auditstream verify` with `forced_rls`.
- **Multi-tenant RLS (v0.1.x):** run `ALTER TABLE auditstream.changes ENABLE ROW LEVEL SECURITY` on the parent table only. Create `app_user` via SQL with `NOSUPERUSER NOBYPASSRLS NOINHERIT` (never use Neon `neondb_owner`/`neon_superuser` or Supabase `postgres`/`service_role`, which silently bypass RLS). Derive DB-row tenant ownership from `after->>'tenant_id'` / `before->>'tenant_id'` (never `COALESCE(actor->>'tenant', ...)`); allowlist `(schema_name, table_name)` pairs; guard cross-tenant `UPDATE`s with `IS NOT DISTINCT FROM`; wrap `current_setting(..., true)` in `NULLIF(..., '')`; scope tenant policies `TO app_user` (never `TO PUBLIC`). Index tenant feeds with an AST-identical partial expression B-tree — never `GIN (after jsonb_path_ops)`.
- **Exporter and verifier roles under RLS (`v0.1.1+`):** create dedicated `NOSUPERUSER NOBYPASSRLS NOINHERIT` roles (`auditstream_exporter`, `auditstream_verify`) with `PERMISSIVE USING (true)` `SELECT` policies and zero blocking `RESTRICTIVE` policies. List tenant runtime roles in `capture.app_roles` and run `auditstream verify --fail-on warning`. Note that `auditstream export` fails closed (exit `3`) under RLS unless unfiltered or `--allow-rls-filtered` is explicitly passed.
- **ADR-0009 BFF projection:** never return raw `auditstream.changes` rows (`audit.query()` / `audit.history()`) directly to browsers — they contain internal DB metadata (`db_user`, `xact_id`, `table_oid`) and raw `before`/`after` snapshots. Always project an allowlisted DTO on the server.
- **Secrets vs. export redaction:** exclude secrets at capture time via `capture.exclude`. Configure `[[export.redaction.rules]]` for top-level `before`/`after` PII minimization in `auditstream export` and `auditstream history` (`history` auto-applies configured rules; pass `--redact` to fail closed with exit `2` when none are configured, or `--omit-payload` to drop `before`/`after` independently of redaction rules; `record_id` is never redacted).
- **TLS:** require `sslmode=verify-full` on non-loopback PostgreSQL URLs.

## 6. Bulk operations and partition scaling

- **Bulk `UPDATE`s and backfills:** use transaction-local `set_config('auditstream.actor', ..., true)` instead of trailing SQL comments (`~32%–40%` faster on 100k updates; run on a direct connection when behind RDS Proxy).
- **Large single-statement `INSERT` / `DELETE`:** batch to `<= 50,000–100,000` rows per statement on managed PostgreSQL with strict `temp_file_limit` to bound `pgsql_tmp` transition-table spills.
- **Time-bounded queries across many partitions:** pass application-computed constant RFC 3339 timestamps (`WHERE occurred_at >= '<RFC3339>'::timestamptz`), not `STABLE` `now() - interval '...'`, so PostgreSQL prunes partitions at plan time and avoids `LWLock:LockManager` contention.
- **Retention:** keep `changes_default` empty (maintain future monthly partitions with `auditstream verify --fix`). Before dropping old monthly partitions, confirm every unfiltered export cursor and its saved boundary `anchor` row have advanced into a newer partition.
