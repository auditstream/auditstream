# Security and community

Community and security files live in each repository until launch, then move to a public organization `.github` repository.

| File | Content | Status |
| --- | --- | --- |
| `LICENSE` | MIT, per repository; GitHub cannot inherit it | Done |
| `SECURITY.md` | Supported versions, GitHub Private Vulnerability Reporting + `security@auditstream.dev`, 3 business day acknowledgment target, and verified reproducer rule for AI-found reports | Done |
| `CODE_OF_CONDUCT.md` | Contributor Covenant 3.0, with Gabriel and Cleisson as enforcement contacts at `conduct@auditstream.dev` | Done |
| `SUPPORT.md` | Questions go to GitHub Discussions, bugs to Issues, security reports to Private Vulnerability Reporting | Done |
| `.github/CODEOWNERS` | `*`, `/spec/`, and `/.github/workflows/` owned by `@gabrielrbl @cleissonom` (switchable to `@auditstream/maintainers` once an org team is created) | Done |
| `.github/VULNERABILITY_REPORT.yml` | Custom GitHub Private Vulnerability Reporting form matching `SECURITY.md` | Done |

Why per repository first: the organization `.github` repository must be public, and inherited files do not appear in a clone, so agents would not see them. At launch, shared files move there and each repository's `AGENTS.md` links to them. Because that repository is public, it holds no internal contacts or infrastructure details, and every label a default template sets must exist in each repository.

Licensing: no CLA and no DCO, by choice. Under section D.6 of GitHub's terms of service, a contribution to a repository with a license notice is licensed under that license. That grants no right to relicense later, which this project accepts.
