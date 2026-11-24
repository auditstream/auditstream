# Security and community

Community and security files live in each repository until launch, then move to a public organization `.github` repository.

| File | Content | When |
| --- | --- | --- |
| `LICENSE` | MIT, per repository; GitHub cannot inherit it | Done |
| `SECURITY.md` | Supported versions, a private reporting address, `security@auditstream.dev`, a 3 business day acknowledgment target, and the rule that AI-found reports need a verified reproducer | Now |
| `CODE_OF_CONDUCT.md` | Contributor Covenant 3.0, with Gabriel and Cleisson as enforcement contacts at `conduct@auditstream.dev` | Now |
| `SUPPORT.md` | Questions go to GitHub Discussions | At launch |
| `.github/CODEOWNERS` | `* @auditstream/maintainers`, an organization team with Gabriel and Cleisson | At launch |
| `VULNERABILITY_REPORT.yml` | GitHub private vulnerability reporting form, which only works on public repositories | At launch |

Why per repository first: the organization `.github` repository must be public, and inherited files do not appear in a clone, so agents would not see them. At launch, shared files move there and each repository's `AGENTS.md` links to them. Because that repository is public, it holds no internal contacts or infrastructure details, and every label a default template sets must exist in each repository.

Licensing: no CLA and no DCO, by choice. Under section D.6 of GitHub's terms of service, a contribution to a repository with a license notice is licensed under that license. That grants no right to relicense later, which this project accepts.
