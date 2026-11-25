# Contributing to Auditstream

Thank you for contributing to Auditstream. This repository follows the engineering handbook in [`docs/handbook/`](docs/handbook/README.md); this guide summarizes the workflow and links to the authoritative sections.

## Finding and claiming work

- Questions and setup help belong in [GitHub Discussions](https://github.com/auditstream/auditstream/discussions) (see [SUPPORT.md](SUPPORT.md)).
- Non-trivial changes start with an issue; see [Workflow: Issues](docs/handbook/workflow.md#issues). Small, self-contained documentation fixes or obvious bug fixes may open a pull request directly without waiting for an issue to be assigned.
- **Claiming an issue:**
  - **External contributors:** leave a short comment on an unassigned issue to claim it; a maintainer will assign it to you. You do not need triage permissions to contribute.
  - **Maintainers (and agents run by a maintainer):** assign the human running the work before starting; see [Parallel work](docs/handbook/parallel-work.md).
- Stay within the issue's **In** scope paths so parallel branches remain conflict-free.
- `good first issue` is reserved for human newcomers; `agent-ready` marks small, self-contained tasks suitable for coding agents.

## Branches and commits

- Work trunk-based from an up-to-date `main` branch using a short-lived branch named `<type>/<issue>-<slug>` (for example `feat/25-trigger-generator`); see [Workflow: Branches](docs/handbook/workflow.md#branches).
- Pull request titles must follow [Conventional Commits 1.0](https://www.conventionalcommits.org/en/v1.0.0/), because squash merges use the pull request title as the commit message on `main`; see [Workflow: Commits](docs/handbook/workflow.md#commits).
  - **Types:** `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore`, `revert`, `style`.
  - **Scopes (engine repository):** `core`, `pg`, `export`, `cli`, `spec`, `docs`, `ci`, `deps`.
  - **Breaking changes:** add `!` after the scope (for example `feat(spec)!: ...`).

## Pull requests

- Link the matching issue with `Closes #N` when one exists (small standalone docs or typo fixes do not require a separate issue).
- Aim for **400 changed lines or fewer**, excluding lockfiles and generated files. Put refactors in their own pull request and stack dependent changes; see [Workflow: Pull requests](docs/handbook/workflow.md#pull-requests).
- For larger changes, open work in progress as a draft pull request and mark it ready once `cargo xtask ci` and the issue's verification commands pass; see [Engineering standards](docs/handbook/engineering-standards.md). Docs-only pull requests run a fast path in CI and do not need a local Rust toolchain.
- Fill in every section of the pull request template, including **Spec impact**, **How it was tested** (exact commands run for code changes, or `N/A — docs-only` for pure documentation edits) and **AI disclosure**.

## AI usage policy

AI tools are welcome under the rules in [AI usage policy](docs/handbook/ai-policy.md) and [AGENTS.md](AGENTS.md):

- A named human is accountable for every line submitted and is the author of record.
- Every pull request must disclose AI usage in the **AI disclosure** section (`none`, `assisted` or `agent-authored`, plus the tool name when assisted or agent-authored). Commit trailers such as `Co-Authored-By` are allowed, never required.
- Agents never approve, merge, sign off, publish releases or change repository settings.

## Review and merge

See [Workflow: Review and merge](docs/handbook/workflow.md#review-and-merge):

- Every pull request requires at least one human approval, green CI and resolved review conversations.
- Changes to `spec/` and `.github/workflows/` require approval from both maintainers.
- Pull requests are squash-merged only, and branches are deleted after merge. Nobody approves their own pull request.

## Definition of done

See [Workflow: Definition of done](docs/handbook/workflow.md#definition-of-done):

- [ ] Every acceptance criterion in the issue is met
- [ ] New tests cover the change and pass in CI
- [ ] Docs, changelog entry or changeset, and the spec are updated where the change touches them
- [ ] No new warnings from clippy
- [ ] Reviewed, squash-merged, and the issue closed by the merge

## Licensing and community

- This repository is licensed under the [MIT License](LICENSE). There is no CLA and no DCO; contributions are licensed under the repository license per section D.6 of GitHub's Terms of Service (see [Security and community](docs/handbook/security-and-community.md)).
- All participation is governed by our [Code of Conduct](CODE_OF_CONDUCT.md). Report security vulnerabilities privately following [SECURITY.md](SECURITY.md).
