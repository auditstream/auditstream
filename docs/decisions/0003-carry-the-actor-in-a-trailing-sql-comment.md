---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# Carry the actor in a trailing SQL comment

## Context and Problem Statement

Adapters need to tell Postgres which actor performed each write without writing audit rows themselves, across ORMs and connection poolers such as PgBouncer, Supavisor, the Neon pooler and RDS Proxy.

## Considered Options

* Trailing sqlcommenter-formatted SQL comment (`/*auditstream='<base64url>'*/`) as the default transport, with `set_config('auditstream.actor', ..., true)` as fallback and `request.jwt.claims` `sub` third
* `set_config('auditstream.actor', ..., true)` as the main transport
* Logical decoding messages via `pg_logical_emit_message`

## Decision Outcome

Chosen option: "Trailing sqlcommenter-formatted SQL comment as the default transport, with `set_config('auditstream.actor', ..., true)` as fallback", because comments need no explicit transaction, carry lower write overhead than `set_config` (40% against 48% throughput drop on single-row updates), and do not pin sessions behind RDS Proxy.

The trigger resolves the actor in fixed precedence order:

1. `nullif(current_setting('auditstream.actor', true), '')`, set with `set_config(..., true)` inside the transaction.
2. The `auditstream` key of the last comment at the end of `right(current_query(), 4096)`, whose body contains no `/` and whose unpadded base64url payload length is checked outside the regex (because PostgreSQL caps regex repetition counts at 255).
3. The `sub` of `request.jwt.claims`, for Supabase and PostgREST callers.
4. No actor: the row is still written with `actor` NULL and `db_user` set to `session_user`.

`pg_logical_emit_message` is deferred until logical replication mode is added.

### Consequences

* Good, because single statements carry their actor without opening an explicit transaction.
* Good, because unpadded base64url cannot contain `*/`, so the payload can never close the comment and inject SQL.
* Good, because the comment can share the sqlcommenter block with other keys such as `traceparent`.
* Good, because a malformed payload never aborts the write; the row is stored with `actor` NULL and `actor_source` set to `invalid`.
* Good, because comments do not pin sessions behind RDS Proxy (unlike `set_config(..., true)`), and both paths pass leak tests behind PgBouncer, Supavisor and the Neon pooler.
* Bad, because the comment appears in `pg_stat_activity`, server logs and the first sample `pg_stat_statements` keeps, so actor payloads must carry IDs only (no names, emails or IP addresses unless opted in) and stay within 512 bytes of JSON.
* Bad, because adapters must not combine per-actor comments with named prepared statements, since each distinct comment becomes a new statement that `node-postgres` never evicts.
* Bad, because under Prisma Accelerate plugins do not receive the SQL, so those applications fall back to `set_config`.

## More Information

* [Capture design: Who acted](../handbook/capture-design.md#who-acted-resolution-order)
* [The contract: Transport](../handbook/contract.md#transport)
* [Evidence](../handbook/evidence.md)
