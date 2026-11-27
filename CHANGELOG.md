# Changelog

All notable changes to Auditstream are documented in this file.


## [0.1.1] - 2026-11-27

### Features
- **(cli)** add history --diff, sanitize terminal output, and improve init ergonomics ([#149](https://github.com/auditstream/auditstream/pull/149))
- **(cli)** apply configured redaction rules and --omit-payload in history ([#192](https://github.com/auditstream/auditstream/pull/192))
- **(pg)** add RLS safety checks to verify and fail-closed RLS guard to export ([#193](https://github.com/auditstream/auditstream/pull/193))

### Bug Fixes
- **(export)** enforce single-line JSONL framing on RawValue and document record_id redaction caveat ([#184](https://github.com/auditstream/auditstream/pull/184))
- **(pg)** qualify ORDER BY columns in history reader queries ([#185](https://github.com/auditstream/auditstream/pull/185))
- **(core)** harden SQL comment tag parsing and trailing line-comment handling ([#179](https://github.com/auditstream/auditstream/pull/179))
- **(pg)** refresh generated functions and bound migration lock waits ([#148](https://github.com/auditstream/auditstream/pull/148))
- **(pg)** harden v0.1 defaults across spec, core, pg, export, and cli ([#159](https://github.com/auditstream/auditstream/pull/159))
- **(pg)** add canonical install SQL helpers and fix nightly bench transaction guard ([#171](https://github.com/auditstream/auditstream/pull/171))
- **(pg)** guard cursor > xmin, verify failover timeline/anchor, and add JSON telemetry ([#174](https://github.com/auditstream/auditstream/pull/174))
- **(pg)** harden auditstream verify checks and warning exit codes ([#175](https://github.com/auditstream/auditstream/pull/175))
- **(pg)** harden generated SQL boundaries, log_event, lock_timeout, JWT sub, and Drizzle tags ([#180](https://github.com/auditstream/auditstream/pull/180))

### Performance
- **(pg)** inline fast-path actor resolution, replace ltrim with COLLATE C regex, fold changed[] into single-SPI INSERT, and record ADR-0010 ([#190](https://github.com/auditstream/auditstream/pull/190))
- **(pg)** enable LZ4 TOAST compression on changes.(before, after) ([#191](https://github.com/auditstream/auditstream/pull/191))
- **(pg)** enable autosummarize = on for changes_occurred_at_brin_idx ([#194](https://github.com/auditstream/auditstream/pull/194))

### Documentation
- spell out PostgreSQL and clarify what auditstream init installs ([#201](https://github.com/auditstream/auditstream/pull/201))
- add verified badges, update PG 19 benchmarks, and align v0.1.1 docs ([#133](https://github.com/auditstream/auditstream/pull/133))
- **(cli)** align CLI help description to database audit trail engine ([#158](https://github.com/auditstream/auditstream/pull/158))



## [0.1.0] - 2026-11-24

### Features
- **(cli)** add cli skeleton with config, logging and exit codes ([#51](https://github.com/auditstream/auditstream/pull/51))
- **(cli)** add init command ([#88](https://github.com/auditstream/auditstream/pull/88))
- **(cli)** add status and verify commands ([#89](https://github.com/auditstream/auditstream/pull/89))
- **(cli)** add history command ([#91](https://github.com/auditstream/auditstream/pull/91))
- **(cli)** add export command with a stored cursor ([#96](https://github.com/auditstream/auditstream/pull/96))
- **(export)** wire redaction into pipeline, config, and export command ([#118](https://github.com/auditstream/auditstream/pull/118))
- **(cli)** add first-class --tenant filter to history and export ([#119](https://github.com/auditstream/auditstream/pull/119))

### Bug Fixes
- resolve SonarCloud and CodeQL security findings ([#94](https://github.com/auditstream/auditstream/pull/94))
- **(cli)** harden export checkpoints, database identity and error handling ([#101](https://github.com/auditstream/auditstream/pull/101))
- **(cli)** silence unused sync_dir warning on non-unix targets ([#115](https://github.com/auditstream/auditstream/pull/115))

### Refactors
- deduplicate shared CLI, migration, actor and sink helpers ([#104](https://github.com/auditstream/auditstream/pull/104))


