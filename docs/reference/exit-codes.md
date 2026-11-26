# Exit Codes Reference

> **Public contract:** CLI exit codes are part of the engine public contract governed by [Releases: Version policy](../handbook/releases.md#version-policy) and mirrored in `crates/auditstream/src/exit.rs`.

## Codes

| Code | Name | Meaning |
| --- | --- | --- |
| `0` | Success | Command succeeded (including `--help`, `--version` and closed stdout pipe `BrokenPipe`) |
| `1` | Check failed | Verification or health check found problems (for example `auditstream verify` drift, unsafe privileges, invalid ownership, primary-key exclusions, or rows in `DEFAULT` partition) |
| `2` | Usage or config error | Invalid CLI arguments (clap), missing `--config` file, unknown config key or invalid config value type |
| `3` | Database error | Postgres connection, permission, query or migration failure |
| `4` | Output or cursor error | Failure reading or writing an output file, migration directory or cursor file |
| `5` | Internal error | Unexpected internal failure or unimplemented command stub |
| `101` | Panic | Uncaught Rust panic (standard Rust runtime exit status) |

## Error formatting

Every non-clap failure prints a single `error:` line on stderr joining the top-level message and its underlying causes, leaving stdout untouched.
