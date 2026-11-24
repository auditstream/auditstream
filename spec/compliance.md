# Auditstream Compliance Matrix

*(Informative)* Tracks which specification features each implementation currently supports and verifies against conformance tests.

Each implementation column header declares the specification version it targets (see [spec/README.md](README.md#declaring-a-spec-version)).

## Matrix

| Feature | engine SQL (`0.1.1`) | engine Rust (`0.1.1`) | `@auditstream/node` (`0.1.0`) | `@auditstream/prisma` (`0.1.0`) | `@auditstream/drizzle` (`0.1.0`) |
| --- | :---: | :---: | :---: | :---: | :---: |
| actor schema | `+` | `+` | `+` | `+` | `+` |
| comment encode | `N/A` | `+` | `+` | `+` | `+` |
| comment decode | `+` | `+` | `+` | `N/A` | `N/A` |
| set_config transport | `+` | `+` | `+` | `+` | `N/A` |
| precedence | `+` | `+` | `N/A` | `N/A` | `N/A` |
| log_event | `+` | `+` | `+` | `N/A` | `N/A` |
| capture triggers | `+` | `+` | `N/A` | `N/A` | `N/A` |
| export JSON Lines | `N/A` | `+` | `+` | `N/A` | `N/A` |
| export CSV | `N/A` | `+` | `N/A` | `N/A` | `N/A` |

## Notes

- **engine SQL (`0.1.1`)**: Generated DDL, `auditstream.log_event()`, `auditstream.resolve_actor()`, and `auditstream.capture()` verified against `spec/conformance/transport/decode.json`, `precedence.json`, `capture/kit-t1-t9.json`, and `capture/edge-tables.json` on PostgreSQL 15–19 (`crates/auditstream-pg/tests/it/triggers.rs`).
- **engine Rust (`0.1.1`)**: Codec, event model, and JSONL/CSV sinks verified against `spec/conformance/transport/` and `spec/schemas/event.schema.json` (`crates/auditstream-core`, `crates/auditstream-export`, and `crates/auditstream`).
- **`@auditstream/node` (`0.1.0`)**: Actor validation, codec (`encodeScope`/`decodeScope`/`setConfigValue`), `audit.log()`, and `parseExportRecord()` verified against the pinned `spec-v0.1.0` fixtures (`packages/node/test/conformance.test.ts`). Transport precedence is resolved inside PostgreSQL triggers rather than in the adapter.
- **`@auditstream/prisma` (`0.1.0`)**: Prisma 7 SQL comments plugin (`auditstream()`) and transaction-local `set_config` fallback (`auditTransaction()`) delegate actor validation and encoding to `@auditstream/node` and are verified end-to-end against PostgreSQL 15–19.
- **`@auditstream/drizzle` (`0.1.0`)**: Driver wrappers (`wrapPgPool`, `wrapPostgresJs`, `wrapNeonHttp`) and Drizzle 1.x `auditComment()` delegate actor validation and encoding to `@auditstream/node` and are verified end-to-end against PostgreSQL 15–19. Users needing `set_config` with Drizzle call `setConfigValue()` from `@auditstream/node` directly.

## Legend

- `+` — Supported and verified against the declared specification version's conformance fixtures.
- `-` — Not yet implemented or not yet verified.
- `N/A` — Not applicable to this implementation.
