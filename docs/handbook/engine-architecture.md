# Engine architecture (Rust)

The engine is a hexagonal Cargo workspace: domain types and port traits sit in `auditstream-core`, and every crate that touches the outside world implements those ports.

## Workspace layout

```text
Cargo.toml                  virtual root: workspace.package, dependencies, lints
rust-toolchain.toml         pinned stable toolchain
crates/
  auditstream-core/         Actor, Context, Event, Op, Cursor, Batch, transport codec, diff, port traits
  auditstream-pg/           schema introspection, DDL and trigger generation, migration writers, history reader
  auditstream-export/       one module per sink; heavy sinks behind Cargo features
  auditstream/              the binary: clap, config, wiring only
xtask/                      cargo xtask ci | codegen | bench
spec/                       the contract (next sections)
docs/                       handbook, decisions, user guide
```

| Crate | Owns | May depend on | Must not depend on |
| --- | --- | --- | --- |
| `auditstream-core` | Domain types, port traits, actor transport codec, diff and redaction | serde, thiserror | tokio, sqlx, any I/O crate |
| `auditstream-pg` | Everything that speaks to Postgres | core, sqlx, tokio, tracing | export, the binary |
| `auditstream-export` | Sinks: JSON Lines and CSV first | core, tokio, libc (Unix) | sqlx, pg |
| `auditstream` (binary) | Commands, config, logging, exit codes | all crates above, clap, anyhow | Business logic of its own |

- **Names.** The binary package is `auditstream`, which also names the release tag, the npm package and the Homebrew formula. Folder name equals crate name.
- **Publishing.** Every crate starts with `publish = false`; only binaries ship. `auditstream-core` is published once the event schema is stable.
- **Boundaries are enforced, not just documented.** cargo-deny `bans` allows `sqlx` only inside `auditstream-pg` (and the same pattern for later SDKs such as aws-sdk or rmcp); `unreachable_pub` is on; each crate README has an "owns / must not depend on" section.
- **Tests.** One integration test binary per crate at `tests/it/main.rs`. End-to-end CLI tests live in `crates/auditstream/tests`.

## Ports and data flow

```rust
pub trait EventSource: Send {
    fn next_batch(&mut self, max: usize)
        -> impl Future<Output = Result<Option<Batch>, SourceError>> + Send;
    fn ack(&mut self, upto: &Cursor)
        -> impl Future<Output = Result<(), SourceError>> + Send;
}

pub trait Sink: Send {
    fn write(&mut self, batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send;
    fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send;
}
```

- **Delivery is pull-based, in batches, at least once.** The cursor is acknowledged only after the sink flushes, and every event carries a stable `event_id` so destinations can deduplicate.
- **Runtime selection** uses a closed `#[serde(tag = "type")] enum SinkConfig` and an `enum AnySink` dispatched with `match`. No boxing, exhaustive, and the config schema can be generated. A dyn-compatible `DynSink` waits until there are out-of-tree plugins.
- **Transforms** such as redaction and OCSF mapping are pure, synchronous functions in core.

## Cross-cutting choices

| Concern | Choice | Note |
| --- | --- | --- |
| Async runtime | tokio 1.54 |  |
| Postgres | sqlx 0.9, runtime API (`query_as` with `FromRow`) | No `query!` macros: most SQL is generated DDL, the audit schema name is configurable, and a committed `.sqlx` cache would conflict across parallel branches |
| CLI | clap 4 derive API with the `env` feature |  |
| Config | `auditstream.toml` plus environment variables, read with serde and `toml` | Move to config-rs only if layering grows |
| Errors | thiserror in libraries, one `#[non_exhaustive]` enum per crate; anyhow in the binary | miette is not used: no release since April 2025 |
| Logging | tracing and tracing-subscriber | Logs to stderr, data to stdout |
| JSON | serde\_json with `raw_value` and `arbitrary_precision` | jsonb passes through without losing `numeric` precision |
| MCP, later | rmcp, the official Rust SDK | Confined to its own crate |
| Replication capture, later | supabase/etl as a git dependency | Confined to an unpublished crate |
