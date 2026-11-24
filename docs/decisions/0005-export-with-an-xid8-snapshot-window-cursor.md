---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# Export with an xid8 snapshot window cursor

## Context and Problem Statement

`auditstream export` pulls batches from `auditstream.changes` with at-least-once delivery and acknowledges the cursor only after the sink flushes. The `id` identity column records insert order, not commit order: an earlier transaction can insert a lower `id` and commit after a later transaction with a higher `id`.

## Considered Options

* An `xid8` snapshot window cursor (`xact_id >= cursor` and `xact_id < pg_snapshot_xmin(pg_current_snapshot())`, ordered by `xact_id` then `id`, storing that `xmin` as the next cursor)
* A cursor on the identity column `id` (`id > last_id`)
* Marking exported rows as processed in a mutable outbox table

## Decision Outcome

Chosen option: "An `xid8` snapshot window cursor", because a cursor on `id` lost a row in testing when an earlier transaction committed after a later one, while the `xid8` window exported it without mutating the history table.

Every row in `auditstream.changes` records `xact_id` (`xid8`, default `pg_current_xact_id()`) and a stable `event_id` (`uuid`) so downstream destinations can deduplicate under at-least-once delivery.

### Consequences

* Good, because concurrent transactions committing out of `id` order never cause rows to be skipped.
* Good, because `auditstream.changes` stays strictly append-only and immutable.
* Good, because acknowledging the cursor only after `Sink::flush()` guarantees at-least-once delivery, and `event_id` enables downstream deduplication.
* Bad, because delivery is at-least-once rather than exactly-once, so consumers must handle duplicate `event_id` values if an export retries before acknowledgement.
* Bad, because rows belonging to transactions at or above `pg_snapshot_xmin(pg_current_snapshot())` wait until the next export window after earlier in-flight transactions finish.

## More Information

* [Capture design: The history table](../handbook/capture-design.md#the-history-table)
* [Engine architecture: Ports and data flow](../handbook/engine-architecture.md#ports-and-data-flow)
* [Evidence](../handbook/evidence.md)
