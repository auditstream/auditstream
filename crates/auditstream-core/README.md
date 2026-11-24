# auditstream-core

Core domain crate for the Auditstream Rust engine.

## Owns

- Domain types (`Actor`, `Context`, `Event`, `Op`, `Cursor`, `Batch`)
- Port traits (`EventSource`, `Sink`) and error types
- Actor transport codec (sqlcommenter base64url encoding/decoding)
- Pure synchronous transforms: diff and redaction

## Must not depend on

- `tokio`
- `sqlx`
- Any I/O or database crate

May depend only on `serde` and `thiserror` (plus pure data helpers).

See the [repository README](../../README.md) and [auditstream.dev/docs](https://auditstream.dev/docs) for full documentation.
