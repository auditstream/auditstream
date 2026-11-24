---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# Hexagonal Cargo workspace with port traits

## Context and Problem Statement

The Rust engine owns domain types, Postgres introspection, DDL and trigger generation, migration writers, history reading, export sinks and the CLI. Multiple people and agents work on these areas in parallel, and the design must allow future capture modes (such as logical replication) and new sinks (such as S3, webhooks, SIEM or MCP) without leaking I/O dependencies across crates.

## Considered Options

* A hexagonal virtual Cargo workspace (`auditstream-core`, `auditstream-pg`, `auditstream-export`, `auditstream` binary, `xtask`) with `EventSource` and `Sink` port traits, `sqlx` 0.9 runtime API (`query_as` with `FromRow`), closed `SinkConfig`/`AnySink` enums, and `cargo-deny` bans enforcing crate boundaries
* Using `sqlx` compile-time `query!` macros with a committed `.sqlx` cache
* Using a dyn-compatible `DynSink` trait object from v0.1

## Decision Outcome

Chosen option: "A hexagonal virtual Cargo workspace with `EventSource` and `Sink` port traits, `sqlx` 0.9 runtime API, closed `SinkConfig`/`AnySink` enums, and `cargo-deny` bans", because it keeps domain types and pure transforms isolated from I/O, avoids merge conflicts across parallel branches, and enforces crate boundaries automatically.

`sqlx` `query!` macros were rejected because most SQL is generated DDL, the audit schema name is configurable, and a committed `.sqlx` cache would conflict across parallel branches. `DynSink` waits until there are out-of-tree plugins.

### Consequences

* Good, because `auditstream-core` depends only on `serde` and `thiserror` (no `tokio`, `sqlx` or other I/O crates), keeping unit and property tests fast and deterministic.
* Good, because `cargo-deny` `bans` allows `sqlx` only inside `auditstream-pg`, and `unreachable_pub` is enabled, enforcing boundaries rather than only documenting them.
* Good, because `EventSource` leaves room for logical replication in its own unpublished crate later, and `rmcp` can be confined to its own crate.
* Good, because closed `#[serde(tag = "type")] enum SinkConfig` and `enum AnySink` dispatched with `match` avoid boxing, stay exhaustive, and allow config schema generation.
* Good, because workspace lints live in `[workspace.lints]` rather than `RUSTFLAGS`, avoiding CI cache invalidation.
* Bad, because runtime `sqlx` queries are checked by integration tests against real Postgres rather than at compile time.
* Bad, because out-of-tree sink plugins cannot be loaded until a dyn-compatible `DynSink` is introduced later.

## More Information

* [Engine architecture (Rust)](../handbook/engine-architecture.md)
* [Engineering standards](../handbook/engineering-standards.md)
