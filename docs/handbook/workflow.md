# Workflow

Every change follows one path: issue, branch, pull request, squash merge. One issue maps to one pull request and one owner.

## Issues

- **Issue types** (organization level): Feature, Bug, Task, Spec, Docs. GitHub creates Task, Bug and Feature by default; Spec and Docs are added. An epic is a parent issue with sub-issues: up to 100 per parent, eight levels deep.
- **Dependencies** use GitHub's native "blocked by" relation, available on the Free plan. `gh issue create --blocked-by` needs a gh release newer than 2.89.
- **Issue fields** are switched on for the organization, which creates Priority, Effort, Start date and Target date. Priority and Effort live in these fields, not in labels, with the organization's default options: Priority is Urgent, High, Medium or Low; Effort is Low for size S and Medium for size M. Effort High is never used, because anything that large is split first.
- **Issue forms** (still a GitHub public preview): bug, feature request, agent-ready task, spec change. A label a form sets is applied only if it already exists in the repository. Blank issues are disabled; questions go to Discussions once the repositories are public.
- **Every task issue states:** goal; context with links to the spec section, its version and the ADR; in-scope paths; out-of-scope paths; testable acceptance criteria; the verification command; blockers; size; definition of done.
- **Size:** S is up to one day, M up to two. Anything larger is split before it is marked ready.

## Labels

| Label | Meaning |
| --- | --- |
| `area:core`, `area:pg`, `area:export`, `area:cli`, `area:spec` | Engine repository areas, one per crate plus the spec |
| `area:node`, `area:prisma`, `area:drizzle` | auditstream-js areas, one per package |
| `area:ci`, `area:docs` | Both repositories |
| `status:needs-triage` | New, not yet reviewed by a maintainer |
| `status:needs-design` | Needs an ADR, an RFC or a spec change before code |
| `status:ready` | Fully specified and every blocker is closed |
| `status:blocked` | Waiting on another issue or an outside decision |
| `agent-ready` | Small and precise enough to hand to a coding agent |
| `good first issue` | Reserved for human newcomers |
| `help wanted` | Maintainers welcome outside help |
| `breaking-change` | Changes public behavior or the contract |

Labels are defined in `.github/labels.yml` in each repository and synced by a workflow, because organization default labels only reach new repositories.

## Branches

- **Trunk-based.** `main` is always releasable, and branches live two days or less.
- **Names:** `<type>/<issue>-<slug>`, for example `feat/42-trigger-generator`. Agent defaults such as `claude/...`, `copilot/...` or `codex/...` are accepted when the pull request links its issue.
- **One branch, one owner.** A person working with their own agents counts as one owner.

## Commits

- **Format:** Conventional Commits 1.0. Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore`, `revert`, `style`.
- **Scopes, engine repository:** `core`, `pg`, `export`, `cli`, `spec`, `docs`, `ci`, `deps`.
- **Scopes, auditstream-js:** `node`, `prisma`, `drizzle`, `docs`, `ci`, `deps`.
- **Breaking changes** use `!` after the scope, for example `feat(spec)!: rename actor.kind to actor.type`.
- **What is enforced:** the pull request title, because it becomes the commit on `main`. Commits inside a branch are free-form and disappear in the squash.

## Pull requests

- One pull request closes one issue with `Closes #N`.
- Work in progress opens as a draft pull request; drafts work in private repositories on the Free plan.
- Aim for 400 changed lines or fewer, not counting lockfiles and generated files. Refactors go in their own pull request. Dependent changes use stacked pull requests, generally available on every plan since October 2026.
- **Template sections:** Summary; Linked issue; Spec impact (none, additive or breaking, with the spec version); How it was tested, with the commands and their output; AI disclosure; Checklist.

## Review and merge

- One human approval. Changes to `spec/` and `.github/workflows/` need both maintainers.
- CI green, conversations resolved, squash merge only. The default squash message is set to "Pull request title and description"; otherwise a one-commit pull request lands with that commit's message. The branch is deleted after merge.
- Nobody approves their own pull request, and agents never approve or merge.
- The organization is on GitHub Team, so a ruleset on main enforces these rules in the private repositories: changes only through pull requests, one approval, green CI, resolved conversations and squash merge, with no bypass for agents. The two-maintainer rule for spec/ and .github/workflows/ holds by agreement until CODEOWNERS is added at the public launch. A merge queue in private repositories would still need Enterprise Cloud.

## Definition of done

- [ ] Every acceptance criterion in the issue is met
- [ ] New tests cover the change and pass in CI
- [ ] Docs, changelog entry or changeset, and the spec are updated where the change touches them
- [ ] No new warnings from clippy or Biome
- [ ] Reviewed, squash-merged, and the issue closed by the merge
