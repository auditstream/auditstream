# Auditstream CLI Reference

> **Public contract:** Global flags, subcommand names, stdout/stderr separation and exit codes are part of the engine public contract governed by [Releases: Version policy](../../handbook/releases.md#version-policy). Breaking changes require `!` in the pull request title and the `breaking-change` label.

## Synopsis

```sh
auditstream [GLOBAL OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

## Global options

| Flag | Environment | Default | Description |
| --- | --- | --- | --- |
| `--config <PATH>` | `AUDITSTREAM_CONFIG` | `./auditstream.toml` (if present) | Path to TOML configuration file |
| `--database-url <URL>` | `AUDITSTREAM_DATABASE_URL`, then `DATABASE_URL` | none | Postgres connection URL (hidden in `--help` and redacted in logs) |
| `--schema <SCHEMA>` | `AUDITSTREAM_SCHEMA` | `auditstream` | Postgres schema holding Auditstream tables and functions |
| `-v`, `--verbose` | `AUDITSTREAM_LOG` | `warn` | Increase stderr log verbosity (`-v` info, `-vv` debug) |
| `-q`, `--quiet` | `AUDITSTREAM_LOG` | off | Emit error logs only |
| `-h`, `--help` | — | — | Print help on stdout and exit `0` |
| `-V`, `--version` | — | — | Print version on stdout and exit `0` |

## Subcommands

| Command | Summary |
| --- | --- |
| [`init`](init.md) | Generate audit schema and trigger migrations (`--table`, `--exclude`, `--migrations`, `--out`, `--dry-run`) |
| [`status`](status.md) | Show audit schema, trigger and partition status (`--format`) |
| [`verify`](verify.md) | Verify triggers, schema integrity and default partition health (`--format`, `--fix`) |
| [`history`](history.md) | Inspect recorded audit history and row diffs (`--table`, `--record-id`, `--actor`, `--tenant`, `--since`, `--until`, `--limit`, `--format`, `--diff`) |
| [`export`](export.md) | Export recorded audit events (`--format`, `--output`, `--cursor-file`, `--batch-size`, `--from-start`, `--since`, `--until`, `--actor`, `--tenant`, `--source`) |

## Streams, logging and secrets

- **Data goes to stdout; logs and diagnostics go to stderr.** Running with `-vv` never writes log output to stdout.
- **Broken pipe (`SIGPIPE`):** If stdout closes early (for example `auditstream export | head -n 1`), the CLI stops cleanly and exits `0`.
- **Log filtering:** Default level is `warn`. `-v` sets `info`, `-vv` sets `debug`, `-q` sets `error`. Setting `AUDITSTREAM_LOG` overrides `-v` and `-q`.
- **Secrets:** Database passwords in URI authority (`user:pass@`), URI query parameters (`password`, `sslpassword`), and libpq key-value strings (`password=...`, `sslpassword=...`) are hidden from `--help` output and masked (`***`) in debug logs and error messages.
- **TLS:** For non-loopback PostgreSQL hosts, configure `sslmode=verify-full` in your connection URL (for example `postgresql://user:pass@db.example.com:5432/prod?sslmode=verify-full`). Connecting to a non-loopback host with `sslmode=disable`, `allow`, or `prefer` emits a warning on stderr.

## Related references

- [Reference index](../README.md)
- [Configuration schema](../config.md)
- [Exit codes](../exit-codes.md)
- [History table and `log_event`](../history-table.md)
