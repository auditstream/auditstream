# Auditstream Compliance Matrix

*(Informative)* Tracks which specification features each implementation currently supports and verifies against conformance tests.

Each implementation column header declares the specification version it targets (see [spec/README.md](README.md#declaring-a-spec-version)).

## Matrix

| Feature | engine SQL (`0.1.0` unreleased) | engine Rust (`0.1.0` unreleased) | `@auditstream/node` (`0.1.0` unreleased) | `@auditstream/prisma` (`0.1.0` unreleased) | `@auditstream/drizzle` (`0.1.0` unreleased) |
| --- | :---: | :---: | :---: | :---: | :---: |
| actor schema | `-` | `-` | `-` | `-` | `-` |
| comment encode | `-` | `-` | `-` | `-` | `-` |
| comment decode | `-` | `-` | `-` | `-` | `-` |
| set_config transport | `-` | `-` | `-` | `-` | `-` |
| precedence | `-` | `-` | `-` | `-` | `-` |
| log_event | `-` | `-` | `-` | `-` | `-` |
| capture triggers | `-` | `-` | `-` | `-` | `-` |
| export JSON Lines | `-` | `-` | `-` | `-` | `-` |
| export CSV | `-` | `-` | `-` | `-` | `-` |

## Legend

- `+` — Supported and verified against the declared specification version's conformance fixtures.
- `-` — Not yet implemented or not yet verified.
- `N/A` — Not applicable to this implementation.
