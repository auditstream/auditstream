# Security Policy

## Supported versions

During v0.x development, security fixes apply to the latest 0.x release (currently 0.1.x) and `main`, supporting PostgreSQL 15 through 19 (see [Releases: Supported versions](docs/handbook/releases.md#supported-versions)).

| Version | Supported |
| --- | --- |
| Latest `0.x` release (currently `0.1.x`) / `main` | Yes |
| Earlier versions | No |

## Security model

For in-scope engine guarantees, operator responsibilities, and documented PostgreSQL/runtime threat-model boundaries, see [Security model and threat-model boundaries](docs/explanation/security-model.md).

## Reporting a vulnerability

Please do **not** open a public GitHub issue for security vulnerabilities.

Report vulnerabilities privately via **[GitHub Private Vulnerability Reporting](https://github.com/auditstream/auditstream/security/advisories/new)** or by emailing **security@auditstream.dev**. We aim to acknowledge every report within **3 business days** and coordinate disclosure with you once a fix is ready.

Include in your report:

- A clear description of the vulnerability and its impact.
- Affected component(s) and version(s) (engine, generated triggers/SQL, CLI or adapters).
- Minimal steps to reproduce or a proof-of-concept.

### Reports found with AI tools

If a vulnerability was discovered or drafted with the help of an AI tool, your report **must** state so explicitly and include a working reproducer **verified by a human** before filing. Unverified AI-generated reports will be closed without investigation (see [AI usage policy](docs/handbook/ai-policy.md)).
