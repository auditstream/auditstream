---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# History table lookup index shape

## Context and Problem Statement

`auditstream.changes` is partitioned monthly on `occurred_at` and already maintains three structural indexes from D2 (`crates/auditstream-pg/src/ddl/mod.rs`):

* `PRIMARY KEY (id, occurred_at)`
* `changes_occurred_at_brin_idx` — `USING brin (occurred_at)` for time-range scans
* `changes_xact_id_idx` — `(xact_id, id)` for commit-safe `xid8` export cursors

D2 also creates a provisional lookup index `changes_lookup_idx` on `(table_name, record_id, occurred_at)` to accelerate point-in-time record history lookups (`auditstream history` / `auditstream_pg::reader::lookup()`). Early evidence ([Evidence](../handbook/evidence.md)) found that this composite `jsonb` B-tree was the single largest write-side cost of capture. Before v0.1 ships, we need to measure candidate lookup index shapes using `cargo xtask bench` and decide which shape `auditstream-pg` should generate by default.

## Considered Options

* **Option 1 (`none`)**: No secondary lookup index (`changes_lookup_idx` removed); rely on partition pruning, BRIN on `occurred_at`, and sequential/bitmap filtering inside a monthly partition.
* **Option 2 (`current`)**: Keep the provisional composite B-tree `ON auditstream.changes (table_name, record_id, occurred_at)`.
* **Option 3 (`table_occurred_at`)**: Narrow B-tree `ON auditstream.changes (table_name, occurred_at DESC)` without `record_id`.
* **Option 4 (`record_id_hash`)**: Partial hash index `ON auditstream.changes USING hash (record_id) WHERE record_id IS NOT NULL`.

## Benchmark Measurements

Measured across three fresh runs of `cargo xtask bench --quick` (`-M prepared -c 8 -j 4 -T 5`, 3 iterations of 10,000-row bulk `INSERT`, tables vacuumed and checkpointed between windows) against `PostgreSQL 19.0 (Debian 19.0-2.pgdg13+1) on x86_64-pc-linux-gnu` backed by `tmpfs` (`shared_buffers=256MB`, `max_wal_size=2GB`, `checkpoint_timeout=15min`) to isolate engine and index maintenance cost from container overlay-filesystem `fsync` jitter. Every run verified 0 failed transactions and exact captured row counts.

### Core Baselines (Default D2 Schema)

| Workload | Run 1 | Run 2 | Run 3 | Median |
| --- | ---: | ---: | ---: | ---: |
| Single-row UPDATE (baseline) | 207,149.9 tps (4.8 µs/row) | 210,148.8 tps (4.8 µs/row) | 205,096.5 tps (4.9 µs/row) | **207,149.9 tps** (4.8 µs/row) |
| Single-row UPDATE (comment) | 59,969.6 tps (+11.8 µs/row) | 61,538.7 tps (+11.5 µs/row) | 56,496.8 tps (+12.8 µs/row) | **59,969.6 tps** (+11.8 µs/row) |
| Single-row UPDATE (`set_config`) | 35,360.5 tps (+23.5 µs/row) | 36,453.1 tps (+22.7 µs/row) | 35,848.8 tps (+23.0 µs/row) | **35,848.8 tps** (+23.0 µs/row) |
| Single-row INSERT (baseline) | 218,389.9 tps (4.6 µs/row) | 222,060.7 tps (4.5 µs/row) | 216,365.7 tps (4.6 µs/row) | **218,389.9 tps** (4.6 µs/row) |
| Single-row INSERT (comment) | 72,496.1 tps (+9.2 µs/row) | 73,642.9 tps (+9.1 µs/row) | 69,290.2 tps (+9.8 µs/row) | **72,496.1 tps** (+9.2 µs/row) |
| 10,000-row INSERT (baseline) | 13.9 ms (1.4 µs/row) | 13.2 ms (1.3 µs/row) | 13.3 ms (1.3 µs/row) | **13.3 ms** (1.3 µs/row) |
| 10,000-row INSERT (comment) | 66.2 ms (+5.2 µs/row) | 65.7 ms (+5.2 µs/row) | 66.3 ms (+5.3 µs/row) | **66.2 ms** (+5.3 µs/row) |

### Lookup Index Variants (3 Runs)

| Variant | Metric | Run 1 | Run 2 | Run 3 | Median | Delta vs `none` |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `none` | UPDATE comment | 62,001.3 tps (+11.3 µs/row) | 65,729.7 tps (+10.5 µs/row) | 65,047.8 tps (+10.5 µs/row) | **65,047.8 tps** | baseline |
| `none` | UPDATE `set_config` | 34,284.8 tps (+24.3 µs/row) | 37,391.6 tps (+22.0 µs/row) | 37,864.0 tps (+21.5 µs/row) | **37,391.6 tps** | baseline |
| `none` | 10k bulk INSERT | 45.6 ms (+3.2 µs/row) | 45.4 ms (+3.2 µs/row) | 46.2 ms (+3.3 µs/row) | **45.6 ms** | baseline |
| `current` | UPDATE comment | 53,970.5 tps (+13.7 µs/row) | 63,390.0 tps (+11.0 µs/row) | 64,957.3 tps (+10.5 µs/row) | **63,390.0 tps** | −2.5% tps |
| `current` | UPDATE `set_config` | 32,994.6 tps (+25.5 µs/row) | 36,099.1 tps (+22.9 µs/row) | 36,663.3 tps (+22.4 µs/row) | **36,099.1 tps** | −3.5% tps |
| `current` | 10k bulk INSERT | 66.8 ms (+5.3 µs/row) | 66.4 ms (+5.3 µs/row) | 67.5 ms (+5.4 µs/row) | **66.8 ms** | **+21.2 ms (+46.5%)** |
| `table_occurred_at` | UPDATE comment | 56,027.8 tps (+13.0 µs/row) | 53,387.2 tps (+14.0 µs/row) | 64,637.5 tps (+10.6 µs/row) | **56,027.8 tps** | −13.9% tps |
| `table_occurred_at` | UPDATE `set_config` | 29,730.9 tps (+28.8 µs/row) | 35,931.9 tps (+23.1 µs/row) | 37,284.9 tps (+21.9 µs/row) | **35,931.9 tps** | −3.9% tps |
| `table_occurred_at` | 10k bulk INSERT | 59.4 ms (+4.6 µs/row) | 57.2 ms (+4.4 µs/row) | 56.8 ms (+4.3 µs/row) | **57.2 ms** | +11.6 ms (+25.4%) |
| `record_id_hash` | UPDATE comment | 55,177.2 tps (+13.3 µs/row) | 62,852.5 tps (+11.2 µs/row) | 65,247.2 tps (+10.5 µs/row) | **62,852.5 tps** | −3.4% tps |
| `record_id_hash` | UPDATE `set_config` | 35,687.0 tps (+23.2 µs/row) | 36,102.7 tps (+22.9 µs/row) | 37,408.0 tps (+21.9 µs/row) | **36,102.7 tps** | −3.4% tps |
| `record_id_hash` | 10k bulk INSERT | 54.5 ms (+4.1 µs/row) | 53.1 ms (+4.0 µs/row) | 52.8 ms (+3.9 µs/row) | **53.1 ms** | **+7.5 ms (+16.4%)** |

## Decision Outcome

Chosen option: **Option 4 (`record_id_hash`) — `CREATE INDEX IF NOT EXISTS changes_lookup_idx ON <schema>.changes USING hash (record_id) WHERE record_id IS NOT NULL`**, because it cuts bulk-insert lookup-index maintenance cost by **65%** relative to `current` (`+7.5 ms` vs `+21.2 ms` per 10,000 rows, bringing 10k bulk insert down from `66.8 ms` to `53.1 ms`), avoids the concurrent B-tree hot-leaf contention measured on `table_occurred_at` (`62.9k` vs `56.0k` median tps on concurrent single-row updates), and accelerates the primary point-in-time record history query (`WHERE table_name = $1 AND record_id = $2::jsonb`) natively with no query rewrites.

Why the other options were not chosen:

* **`none`** delivers the lowest write cost (`45.6 ms` bulk insert, `65.0k` tps single-row update), but forces every `auditstream history --table T --record-id K` lookup to scan an entire monthly partition.
* **`current` (`(table_name, record_id, occurred_at)`)** carries the highest bulk-insert maintenance cost (`+21.2 ms` over `none`, nearly 3x `record_id_hash`) because every insert encodes and compares composite `(text, jsonb, timestamptz)` B-tree tuples across random `record_id` leaf pages.
* **`table_occurred_at` (`(table_name, occurred_at DESC)`)** still cannot index `record_id` filters, costs more than `record_id_hash` on 10k bulk inserts (`57.2 ms` vs `53.1 ms`), and showed the worst concurrent single-row UPDATE contention (`56.0k` median tps, dropping to `53.4k` tps) because all concurrent writers on a hot table contend on the same leading `(table_name, newest occurred_at)` B-tree leaf page.

Adopted in `crates/auditstream-pg/src/ddl/mod.rs` (#92); `spec/history-table.md` already leaves secondary lookup index shapes to implementations and benchmarks.

### Consequences

* Good, because bulk-insert lookup-index overhead drops by ~65% (`+0.75 µs/row` instead of `+2.12 µs/row`) and single-row UPDATE throughput stays within ~3.4% of having no lookup index.
* Good, because `WHERE record_id = $1::jsonb` in `auditstream_pg::reader::lookup()` uses the hash index automatically inside each pruned monthly partition without requiring expression predicates.
* Good, because `WHERE record_id IS NOT NULL` excludes `TRUNCATE` events and targetless app events from the index, and PostgreSQL hash indexes are WAL-logged, crash-safe, replica-safe, and supported on partitioned tables across the entire supported PostgreSQL 15–19 range.
* Good, because uniform hashing spreads concurrent writes across buckets rather than funneling hot-table updates onto a single B-tree leaf page.
* Bad, because a hash index on `record_id` only accelerates equality lookups (`record_id = $1::jsonb`), not `WHERE table_name = $1` scans without a `record_id` (which rely on monthly partition pruning and `changes_occurred_at_brin_idx`) and not `@>` partial-key JSONB containment queries.
* Bad, because `record_id` lookups across tables that happen to share identical primary-key JSONB values (e.g., `{"id": 1}`) fetch matching heap tuples before filtering on `table_name` (bounded to the pruned monthly partition).

## More Information

* [Capture design — The history table](../handbook/capture-design.md#the-history-table)
* [Evidence](../handbook/evidence.md)
* [Capture benchmark suite](../../bench/capture/README.md)
* [Nightly benchmark workflow](../../.github/workflows/nightly.yml)
