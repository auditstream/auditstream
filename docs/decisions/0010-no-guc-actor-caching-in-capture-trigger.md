---
status: accepted
date: 2026-11-27
decision-makers: Gabriel Lobo, Cleisson
---

# Do not use GUC-based actor caching or statement-level `AFTER UPDATE` in `auditstream.capture()`

## Context and Problem Statement

In Auditstream's capture architecture ([ADR-0002](0002-capture-changes-with-generated-triggers.md), [ADR-0003](0003-carry-the-actor-in-a-trailing-sql-comment.md)), `INSERT` and `DELETE` triggers run `FOR EACH STATEMENT` using PostgreSQL transition tables (`NEW TABLE` / `OLD TABLE`), resolving the actor once per statement. By contrast, `UPDATE` triggers run `FOR EACH ROW` so `capture()` can pair each row's exact `OLD` and `NEW` images and skip no-op or excluded-column-only updates.

Profiling bulk `UPDATE` workloads across PostgreSQL 15.20, 17.12, and 19.0 (`public.customers`, 7 columns, 200k rows, and 100k-row `UPDATE` micro-ablations) identified four per-row CPU bottlenecks in `auditstream.capture()`:

1. A cross-function `SECURITY DEFINER` call from `capture()` into `auditstream.resolve_actor()` on every updated row (`1,202.4 ms` vs `581.6 ms` pure trigger overhead over 100k rows on PostgreSQL 17, **-51.6%** when inlined).
2. `pg_catalog.ltrim(raw_b64, <64 chars>)` for base64url alphabet validation, which scans up to 64 characters per input character (`683.9 ms` on PostgreSQL 17 / `755.4 ms` on PostgreSQL 19 vs `39.9 ms` / `40.4 ms` with DFA-cached `pg_catalog.textregexeq(raw_b64 COLLATE pg_catalog."C", '[^A-Za-z0-9_-]')`, **17.1x–18.7x faster**).
3. Unconditional `rtrim(...)` and `reverse(...)` scans over the trailing 4,096-character query tail even when the query ends with `'/'` or contains a single `'/'`.
4. A separate SPI execution (`v_changed := ARRAY(SELECT ...)`) before `INSERT INTO auditstream.changes` on every updated row.

While exploring deeper optimizations for bulk `UPDATE` statements, two more aggressive designs were prototyped and evaluated:

* **GUC-based actor caching (`auditstream._ca_*`):** caching the parsed `(actor, context, actor_source)` for `(txid_current(), current_query())` in custom placeholder GUCs via `set_config('auditstream._ca_*', ..., true)` so rows 2..N of a bulk `UPDATE` skip re-parsing the trailing SQL comment.
* **Statement-level `AFTER UPDATE` (`FOR EACH STATEMENT` with `REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows`):** pairing `old_rows` and `new_rows` via `row_number() OVER ()` so actor resolution runs once per statement.

We need a durable decision on which optimizations belong in `auditstream.capture()` and why GUC-based actor caching and statement-level `AFTER UPDATE` are explicitly rejected despite showing lower bulk-`UPDATE` latency in early prototypes.

## Considered Options

### For per-row actor resolution and `UPDATE` capture

* **Option 1 (Inline stateless fast-path in `capture()` + `COLLATE "C"` `textregexeq` + single-SPI `INSERT`):** Inline the exact actor-resolution logic from the same Rust template as `STABLE auditstream.resolve_actor()` into `capture()`, validate base64url with `pg_catalog.textregexeq(v_raw_b64 COLLATE pg_catalog."C", '[^A-Za-z0-9_-]')`, short-circuit `rtrim` and single-`/` comment scans, fold `changed[]` into `INSERT ... VALUES (...)`, decode unified `TG_ARGV` formats (existing `v0.1.x` JSON arrays `'["id"]'`/`'[]'` plus future plain single-column and PostgreSQL `text[]` `'{...}'` arguments) while keeping generated triggers unchanged for lock-free `v0.1.x` upgrades, and keep `auditstream_update` `FOR EACH ROW` with zero mutable GUC cache state. Recommend `set_config('auditstream.actor', ..., true)` for high-volume bulk updates.
* **Option 2 (GUC-based actor cache `auditstream._ca_*` inside `capture()`):** Cache resolved actor fields in transaction-local custom placeholder GUCs keyed by `(txid, query)` inside `capture()`.
* **Option 3 (Statement-level `AFTER UPDATE` trigger pairing transition tables with `row_number() OVER ()`):** Switch `auditstream_update` from `FOR EACH ROW` to `FOR EACH STATEMENT` and join `old_rows` and `new_rows` by synthetic `row_number()`.
* **Option 4 (Other micro-variants):** `ROWS FROM(jsonb_each(v_before), jsonb_each(v_after))` for `changed[]`, `translate(raw_b64, <64 chars>, '')` for base64url checks, or secondary index tweaks (dropping `WHERE record_id IS NOT NULL`, `fillfactor = 100`, sequence `CACHE 256`).

## Decision Outcome

Chosen option: **Option 1 (Inline stateless fast-path in `capture()` + `COLLATE "C"` `textregexeq` + single-SPI `INSERT`)**.

Across PostgreSQL 15.20, 17.12, and 19.0, Option 1 cuts 100k bulk `UPDATE` time by **15.2%–19.6%** (`4,296` / `4,111` / `4,307 ms` down to `3,642` / `3,339` / `3,464 ms` when triggers pass plain single-column / PostgreSQL `text[]` `TG_ARGV` arguments; **6.7%–11.1%+** with the unchanged `v0.1.x` JSON-array `'["id"]'` trigger arguments emitted by default, with additional savings from preserving the existing zero-SPI 1-element JSON-array fast path), cuts actor-less 100k bulk `UPDATE` time by **7.6%–12.6%** (`2,587` / `2,553` / `2,573 ms` down to `2,390` / `2,314` / `2,248 ms`), and lifts 16-client `pgbench` throughput (`synchronous_commit = off`, PostgreSQL 19, 3x8s median) from `53,750.5 TPS` to **`54,911.0 TPS` (+2.2%)** — with zero schema changes, zero breaking changes, and zero `SHARE ROW EXCLUSIVE` locks on audited tables during `v0.1.x` function upgrades (`CREATE OR REPLACE FUNCTION` only).

### Why GUC actor caching (`auditstream._ca_*`) is rejected

Custom PostgreSQL placeholder GUCs are `PGC_USERSET`: any session role can read or write them with `SET` / `set_config()`. `SECURITY DEFINER` on `auditstream.capture()` does **not** isolate custom GUCs from the calling application role. All three security and reliability exploits below were reproduced live on PostgreSQL 19.0 as an unprivileged `app_user`:

1. **Write-aborting DoS (violates [spec/transport.md](../../spec/transport.md) "A malformed actor payload MUST NEVER abort the application's SQL write"):** Pre-seeding `auditstream._ca_k` with the matching key and `auditstream._ca_a = 'NOT_VALID_JSON{{{'` aborts the application's `UPDATE` with SQLSTATE `22P02` (`invalid_text_representation`) on `::jsonb` cast. Pre-seeding `auditstream._ca_s = 'evil_source'` aborts the `UPDATE` with SQLSTATE `23514` (`check_violation` on `changes_actor_source_check`).
2. **Actor schema-validation bypass and permanent reader breakage:** Pre-seeding `auditstream._ca_a = '{"type":"FORGED_INVALID_TYPE","id":99999}'` and `auditstream._ca_s = 'jwt'` bypasses every validation rule in [spec/actor.md](../../spec/actor.md) and [spec/transport.md](../../spec/transport.md) (including the 512-byte limit, the 5-value `type` enum, and `'jwt'` invariants) and persists illegal rows into `auditstream.changes`. Downstream strict readers such as `@auditstream/node`'s `rowToHistoryEntry()` and `parseExportRecord()` then throw `AuditstreamError("INVALID_RECORD")` on every subsequent read of that history.
3. **Cross-transaction and cross-tenant PgBouncer poisoning:** A session-level `SET auditstream._ca_k = ...` sets PostgreSQL's GUC reset value for that backend session. Even when `capture()` writes cache entries with `is_local = true`, `COMMIT` reverts each GUC to the poisoned session-level reset value rather than clearing it, contaminating subsequent transactions and tenants sharing the same pooled backend connection.
   * *Why signing the cache value does not fix this:* A hardcoded MAC secret inside `capture()` is ineffective because `pg_catalog.pg_proc.prosrc` is readable by `PUBLIC` by default, and storing a secret in an `auditstream_owner`-only table adds a per-row SPI lookup that erases the cache benefit.
4. **A safe operational alternative already exists:** Workloads running large bulk updates can pass the actor via `SELECT pg_catalog.set_config('auditstream.actor', '<json>', true)` inside the transaction. That path skips SQL comment parsing entirely, achieves **~32%–40% lower bulk latency** than Option 1 comment triggers (`2,257–2,423 ms` on 100k updates, ~44% below the pre-`#162` baseline), stays strictly within the spec's existing precedence contract, and introduces zero cache attack surface.

### Why statement-level `AFTER UPDATE` (`row_number()`) and other micro-variants are rejected

* **Statement-level `AFTER UPDATE` with `row_number() OVER ()`:** Live-proven **silent data corruption** on PostgreSQL 17.12. On a partitioned table where an `UPDATE` moves a row across partitions and a target partition's `BEFORE INSERT` trigger returns `NULL` (suppressing the insert), `old_rows` and `new_rows` have different cardinalities. Joining them on synthetic `row_number() OVER ()` silently pairs `OLD` and `NEW` images from **different rows**, corrupting `before`, `after`, `changed`, and `record_id` without raising an error. In addition, statement-level `AFTER UPDATE` measured **+29.0% higher latency** (**22.5% lower throughput**, `1,179 ms` vs `914 ms`) on single-row OLTP updates due to transition-table setup overhead.
* **`ROWS FROM(jsonb_each(v_before), jsonb_each(v_after))`:** Measured slower than folding `ARRAY(SELECT e.k FROM pg_catalog.jsonb_each(v_after) AS e(k, v) WHERE v_before->e.k IS DISTINCT FROM e.v)` directly into `INSERT ... VALUES (...)` across 2-, 7-, 30-, and 60-column tables.
* **`pg_catalog.translate(raw_b64, <64 chars>, '')`:** Measured `1,413.9 ms` per 100k rows — **35x slower** than `pg_catalog.textregexeq(v_raw_b64 COLLATE pg_catalog."C", '[^A-Za-z0-9_-]')`.
* **Index and sequence micro-tweaks (dropping `WHERE record_id IS NOT NULL`, `fillfactor = 100`, `CACHE 256`):** Measured within noise on PostgreSQL 17 (`1,368` vs `1,401` vs `1,381 ms`); the [ADR-0008](0008-history-lookup-index.md) layout remains optimal.

### Consequences

* Good, because `capture()` eliminates the cross-function `SECURITY DEFINER` hop, the `O(64 * N)` `ltrim` scan, redundant `rtrim`/`reverse` work, and the separate `v_changed` SPI query while remaining 100% stateless and side-effect-free.
* Good, because `auditstream.resolve_actor()` and inlined `auditstream.capture()` are emitted from a single Rust template in `crates/auditstream-pg/src/triggers/mod.rs`, preventing behavioral drift while keeping `resolve_actor()` `STABLE` for `log_event()` and direct SQL callers.
* Good, because `spec/transport.md` guarantees (never aborting application writes, full actor schema enforcement, and zero cross-transaction state leakage behind poolers) remain inviolate even against untrusted or buggy application sessions.
* Good, because keeping existing `v0.1.x` trigger definitions (`'["id"]'`, `'[]'`) unchanged allows upgrading trigger functions with `CREATE OR REPLACE FUNCTION` only and zero `SHARE ROW EXCLUSIVE` locks on audited tables.
* Bad, because very large single-statement `UPDATE`s using trailing SQL comments still re-parse the comment per row; teams optimizing 100k-row batch updates should use transaction-local `set_config('auditstream.actor', ..., true)` for an additional ~32%–40% latency reduction.

## More Information

* [Actor Transport and Precedence (`spec/transport.md`)](../../spec/transport.md)
* [Actor and Context Schema (`spec/actor.md`)](../../spec/actor.md)
* [History Table and Functions Contract (`spec/history-table.md`)](../../spec/history-table.md)
* [ADR-0002: Capture changes with generated triggers](0002-capture-changes-with-generated-triggers.md)
* [ADR-0003: Carry the actor in a trailing SQL comment](0003-carry-the-actor-in-a-trailing-sql-comment.md)
* [ADR-0008: History table lookup index shape](0008-history-lookup-index.md)
* [Capture design](../handbook/capture-design.md)
