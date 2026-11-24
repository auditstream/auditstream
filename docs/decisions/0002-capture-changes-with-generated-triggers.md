---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# Capture changes with generated triggers

## Context and Problem Statement

Auditstream v0.1 needs to capture `INSERT`, `UPDATE`, `DELETE` and `TRUNCATE` changes on any Postgres database an application can migrate, including managed free tiers, recording before and after row images alongside the actor without letting application code write audit rows directly.

## Considered Options

* Generated triggers writing to a partitioned `auditstream.changes` table (statement-level with transition tables for `INSERT` and `DELETE`, row-level with `WHEN (OLD.* IS DISTINCT FROM NEW.*)` for `UPDATE`, statement-level for `TRUNCATE`)
* Logical replication reading the WAL through `pgoutput`
* An outbox drained by a daemon with `LISTEN`/`NOTIFY` and `FOR UPDATE SKIP LOCKED`

## Decision Outcome

Chosen option: "Generated triggers writing to a partitioned `auditstream.changes` table", because triggers work on any Postgres the application can migrate without requiring `wal_level = logical`, a `REPLICATION` role, `REPLICA IDENTITY FULL`, or an always-running consumer. The `EventSource` port leaves room for logical replication later when users want lower write overhead and accept its operational requirements.

Statement-level triggers are used for `INSERT` and `DELETE` because a 10,000-row insert took 155 to 180 ms against 541 to 550 ms with row-level triggers, and single-row inserts ran at 7,906 against 6,913 transactions per second. `UPDATE` stays row-level because a statement-level version was only about 10% faster on a 10,000-row update, 4% slower on single rows, and split a primary-key change into two half-events.

An outbox drained with `LISTEN`/`NOTIFY` and `FOR UPDATE SKIP LOCKED` was rejected because the history table stays append-only and marking rows as processed would make it mutable, and because `NOTIFY` can serialize commits under load on older supported versions.

### Consequences

* Good, because setup works on any Postgres the application can migrate, including managed free tiers; identical results were verified on PostgreSQL 15 to 19 locally, on Neon (PostgreSQL 18.7) and on Supabase (PostgreSQL 17.12).
* Good, because `OLD` and `NEW` row images are available inside the trigger without `REPLICA IDENTITY FULL` growing the WAL.
* Good, because nothing runs between exports: `auditstream export` reads on demand, with no replication slot that can retain WAL and fill the disk.
* Good, because the history table lives in the user's database, owned by a `NOLOGIN` role, append-only and readable by the application and the CLI.
* Bad, because capture adds synchronous write overhead: in the worst case measured (single-row updates), throughput dropped 40% with comment transport (11,105 to 6,630 transactions per second) and 48% with `set_config` (to 5,825 transactions per second).
* Bad, because trigger bypasses exist (`session_replication_role = replica`, disabling or dropping a trigger), requiring `auditstream verify` to report missing, disabled or outdated triggers.
* Bad, because triggers do not run on Aurora DSQL, and statements targeting a partition of a partitioned user table directly do not fire parent statement triggers.

## More Information

* [Capture design](../handbook/capture-design.md#why-triggers-and-when-logical-replication)
* [Evidence](../handbook/evidence.md)
