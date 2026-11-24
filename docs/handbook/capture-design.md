# Capture design

v0.1 captures with generated triggers that write to a partitioned `auditstream.changes` table, owned by a dedicated role and readable but not writable by the application. Every rule below was tested on PostgreSQL 16; the numbers are in [the Evidence tab](evidence.md).

## Triggers

- **INSERT and DELETE are statement-level**, with transition tables (`REFERENCING NEW TABLE` or `OLD TABLE`) and the actor resolved once per statement. A 10,000-row insert took 155 to 180 ms this way against 541 to 550 ms with row-level triggers, and single-row inserts also ran faster (7,906 against 6,913 transactions per second).
- **UPDATE stays row-level**, with `WHEN (OLD.* IS DISTINCT FROM NEW.*)` so no-op writes are skipped. A statement-level version was built and measured: about 10% faster on a 10,000-row update, 4% slower on single rows, and it splits a primary-key change into two half-events. Not worth it.
- **TRUNCATE** has its own `AFTER TRUNCATE FOR EACH STATEMENT` trigger. It records the table and the actor, with no rows.
- **Row data** is `to_jsonb(OLD)` and `to_jsonb(NEW)` minus the excluded columns. `changed` lists the keys that differ, and an update that only touches excluded columns writes nothing.
- **Partitioned user tables** get their statement triggers on the parent. A statement that names a partition directly does not fire them, so `verify` reports such tables.

## Who acted: resolution order

1. `nullif(current_setting('auditstream.actor', true), '')`, set with `set_config(..., true)` inside the transaction. After the transaction the setting reads as an empty string, hence the `nullif`.
2. The `auditstream` key of the last comment at the end of `right(current_query(), 4096)`, whose body contains no `/`. The value must be base64url; its length is checked outside the regex, because PostgreSQL caps regex repetition counts at 255.
3. The `sub` of `request.jwt.claims`, for Supabase and PostgREST callers.
4. No actor: the row is still written, with `actor` NULL and `db_user` set to `session_user`.

- **A malformed payload never aborts the write.** The row is stored with `actor` NULL and `actor_source` set to `invalid`. Any other error inside a trigger aborts the user's own transaction, so every path of the generated SQL is covered by tests.
- **IDs only.** The comment shows up in `pg_stat_activity`, server logs and the first sample `pg_stat_statements` keeps, so names, emails and IP addresses stay out unless a project opts in.
- **Behind RDS Proxy the comment is the default transport.** `set_config(..., true)` pins the session there; comments and `SET LOCAL` do not. Through PgBouncer in transaction mode both paths pass a leak test: actor-less statements sharing server connections with set\_config transactions never pick up an actor.

## The history table

| Column | Type | Note |
| --- | --- | --- |
| `id` | bigint identity | Insert order, not commit order |
| `event_id` | uuid | Stable ID for deduplication downstream |
| `occurred_at` | timestamptz | Partition key |
| `xact_id` | xid8, default `pg_current_xact_id()` | Export cursor |
| `source` | text | `db` for captured changes, `app` for `audit.log()` |
| `schema_name`, `table_name`, `table_oid` | text, text, oid | Target table |
| `op` | text | INSERT, UPDATE, DELETE, TRUNCATE, or the app action name |
| `record_id` | jsonb | Primary key values |
| `before`, `after` | jsonb | Row images without excluded columns |
| `changed` | text\[\] | Keys that differ on UPDATE |
| `actor`, `context` | jsonb | As defined in [the contract](contract.md#actor-and-context) |
| `actor_source` | text | `set_config`, `comment`, `jwt`, `invalid` or NULL |
| `db_user` | text | `session_user` |
| `schema_version` | smallint | Version of this layout |

- **Partitioned by month** on `occurred_at`; the primary key includes it, and a DEFAULT partition always exists. Once a row lands in DEFAULT, PostgreSQL refuses to create that month's partition until the row is moved, which takes a detach, move and re-attach under lock. So the CLI creates partitions three months ahead, and `verify` fails when DEFAULT holds rows.
- **Indexes.** BRIN on `occurred_at` always. The lookup index on `(table_name, record_id, occurred_at)` was the largest single write cost measured, so its final shape is decided by the benchmark issue.
- **The export cursor is an xid8 window.** Each export reads rows where `xact_id >= cursor` and `xact_id < pg_snapshot_xmin(pg_current_snapshot())`, ordered by `xact_id` then `id`, and stores that xmin as the next cursor. A cursor on `id` lost a row in testing when an earlier transaction committed after a later one.
- **App events** go through `auditstream.log_event(action, target, data)`, so adapters never depend on the table layout.

## Security

- The trigger function is `SECURITY DEFINER`, owned by a `NOLOGIN` role, with `SET search_path = pg_catalog, pg_temp` and fully qualified names.
- On managed Postgres the migration runs as a non-superuser, and from PostgreSQL 16 the creator of a role holds only ADMIN on it. The migration therefore grants the owner role to itself when `pg_has_role(current_user, 'auditstream_owner', 'USAGE')` is false, before creating objects owned by it. Found on Neon and Supabase.
- Application roles get `SELECT` on the history table and no write grants. Tested: the application role's INSERT, UPDATE and DELETE on the history table were denied, and its own writes were still captured.
- Bypasses exist (`session_replication_role = replica`, disabling or dropping a trigger). `auditstream verify` reports missing, disabled or outdated triggers; an event trigger that alerts on them comes later.

## Cost

The worst case is a stream of small single-row updates. There the full design cost 40% of throughput with the comment (11,105 to 6,630 transactions per second) and 48% with `set_config`. Bulk writes cost much less per row, because INSERT and DELETE triggers run once per statement.

## Where it does not run

Aurora DSQL has no triggers. CockroachDB triggers are in preview, and its `current_query()` behavior is unknown. Under Prisma Accelerate, plugins do not see the SQL, and whether the comments reach the database is undocumented; those apps use `set_config` until a test shows otherwise.

## Why triggers, and when logical replication

v0.1 captures with triggers. Logical replication, which reads the WAL through `pgoutput`, was weighed and deferred. The `EventSource` port leaves room for it.

| | Triggers (v0.1) | Logical replication (later) |
| --- | --- | --- |
| Database setup | Any Postgres the app can migrate, managed free tiers included | `wal_level = logical` and a role with REPLICATION. Neon and RDS need it switched on first; RDS also needs a reboot |
| Before and after | `OLD` and `NEW` inside the trigger | Full old rows need `REPLICA IDENTITY FULL` on every audited table, which grows the WAL |
| Actor | Trailing SQL comment per statement, or `set_config` per transaction | Neither reaches the WAL. Each write needs `pg_logical_emit_message(true, 'auditstream', ...)` in an explicit transaction, or a trigger that emits it |
| Write cost | Synchronous: 40% on single-row updates, the worst case measured | Lower on the write path; the cost moves to WAL volume and a consumer |
| Operations | Nothing runs between exports; `export` reads on demand | A consumer runs all the time. A stopped consumer's slot keeps WAL and can fill the disk; `max_slot_wal_keep_size` caps it by invalidating the slot |
| History | In the user's database, queried by the app and the CLI | Wherever the consumer writes it |

Two other options were rejected:

- **An outbox drained by a daemon** with LISTEN/NOTIFY and `FOR UPDATE SKIP LOCKED`. The history table is the product and stays append-only, and marking rows as processed would make it mutable. Production reports also show NOTIFY serializing commits under load; a recent core change targets it, but older supported versions still have it.
- **`set_config` as the main transport.** Comments need no explicit transaction, cost less (40% against 48%) and do not pin RDS Proxy sessions.

A replication mode comes when users ask for less write overhead and accept the operations it needs. It would read the actor from logical decoding messages, which `pgoutput` delivers since PostgreSQL 14 with its `messages` option. Like the comment, any session can send such a message, so it records who the application says acted; it is not proof of identity. The mode would keep the same history record and contract, in its own crate (see [Cross-cutting choices](engine-architecture.md#cross-cutting-choices)).

## Still to prove

Proven on 2026-11-24: identical results on PostgreSQL 15 to 19 locally, and on Neon (PostgreSQL 18.7) and Supabase (PostgreSQL 17.12). Prisma 7.10, Drizzle 0.45 and 1.0, and postgres.js pass on every write path and under concurrency, with zero wrong actors: directly, through PgBouncer and Supavisor in transaction mode, and through the Neon pooler. The Neon HTTP and WebSocket drivers carry the comment and the set\_config transaction intact, direct and pooled. No actor leaked between pooled connections on any platform.

- [ ] RDS Proxy, deferred until a user asks for it. AWS documents that set\_config pins the session and comments do not; nobody has run it yet.
