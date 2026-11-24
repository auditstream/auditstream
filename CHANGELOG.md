# Changelog

All notable changes to Auditstream are documented in this file.


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


