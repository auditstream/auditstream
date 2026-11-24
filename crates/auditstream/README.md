# auditstream

CLI binary package for Auditstream.

## Owns

- CLI argument parsing (`clap`), config loading, structured logging and exit codes
- Wiring `auditstream-core`, `auditstream-pg` and `auditstream-export` together

## Must not depend on

- Business logic of its own

May depend on `auditstream-core`, `auditstream-pg`, `auditstream-export`, `clap`, `anyhow` and `tokio`.
