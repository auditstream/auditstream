# auditstream-export

Export sinks and pipeline crate for the Auditstream Rust engine.

## Owns

- Export sinks (JSON Lines and CSV first; heavier sinks behind Cargo features)
- Export pipeline acknowledging cursors only after sink flush

## Must not depend on

- `sqlx`
- `auditstream-pg`

May depend on `auditstream-core` and `tokio`.

See the [repository README](../../README.md) and [auditstream.dev/docs](https://auditstream.dev/docs) for full documentation.
