# Security and community

Each repository ships its own security, ownership, and community files so contributors and coding agents always see them in a local clone (`auditstream/.github` currently hosts the organization profile and reusable workflows).

| File | Content | Status |
| --- | --- | --- |
| `LICENSE` | MIT, per repository; GitHub cannot inherit it | Done |
| `SECURITY.md` | Repo-specific supported versions, GitHub Private Vulnerability Reporting + `security@auditstream.dev`, 3 business day acknowledgment target, and verified reproducer rule for AI-found reports | Done |
| `CODE_OF_CONDUCT.md` | Contributor Covenant 3.0, with Gabriel and Cleisson as enforcement contacts at `conduct@auditstream.dev` | Done |
| `SUPPORT.md` | Questions go to GitHub Discussions, bugs to Issues, security reports to Private Vulnerability Reporting | Done |
| `.github/CODEOWNERS` | Per repository (GitHub cannot inherit `CODEOWNERS`); names `@gabrielrbl @cleissonom` (switchable to `@auditstream/maintainers` once an org team is created) | Done |
| `.github/VULNERABILITY_REPORT.yml` | Custom GitHub Private Vulnerability Reporting form tailored to engine/SQL/CLI reports | Done |

Why per repository: GitHub cannot inherit `LICENSE` or `.github/CODEOWNERS` from an organization `.github` repository, `SECURITY.md` and `.github/VULNERABILITY_REPORT.yml` carry repository-specific version and component details, and keeping `CODE_OF_CONDUCT.md` and `SUPPORT.md` in the repository ensures every local clone and coding agent can read them directly.

Licensing: no CLA and no DCO, by choice. Under section D.6 of GitHub's terms of service, a contribution to a repository with a license notice is licensed under that license. That grants no right to relicense later, which this project accepts.
