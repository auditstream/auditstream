# AI usage policy

AI tools are welcome. A named human is accountable for everything an agent produces, and every pull request says how AI was used.

- **Accountability.** The person who assigns an agent or opens its pull request is the author of record. They must be able to explain every line and review it as their own work.
- **Disclosure.** The pull request form has a required field: none, assisted or agent-authored, plus the tool name. Tool-native trailers such as `Co-Authored-By` are allowed, never required. The Linux kernel removed its `Assisted-by:` tag on 13 October 2026 because "the tag does not provide the signal it was kept for", and moved disclosure to the cover letter; the pull request description is our cover letter.
- **Limits.** Agents never approve, merge, sign off, publish releases, or change repository settings and secrets. On GitHub, the person who assigned Copilot to an issue cannot approve the resulting pull request, so the other maintainer reviews it.
- **Scope.** Agents work on `agent-ready` issues. `good first issue` is for human newcomers, as in LLVM and Node.js. Spec changes, security fixes, releases and CI permissions are led by a human.
- **Evidence.** A pull request lists the commands that were run and their output. A new test must be shown failing without the change.
- **Security reports** found with AI say so and carry a reproducer verified by a human before they are filed, as curl requires.
- **Data.** No secrets, credentials or customer data in prompts; use fixtures.
- **Conversation.** Comments and review replies on GitHub are written by people. An agent may draft them.

## Instruction files

`AGENTS.md` at each repository root is the only committed instruction file. Claude Code (2.1.277 or newer), GitHub Copilot cloud agent and Codex all read it directly.

| File | Holds |
| --- | --- |
| `AGENTS.md` at the repository root | Commands to build, test and lint; repository map; boundaries such as generated files and lockfiles; how to run the Postgres tests; commit and pull request rules; definition of done |
| `spec/AGENTS.md`, per-crate or per-package `AGENTS.md` | Rules that differ in that folder; the nearest file wins |
| `CLAUDE.md` | Not committed. When a CLAUDE.md or a personal CLAUDE.local.md exists, Claude Code stops reading AGENTS.md by default, so a personal file must start with `@AGENTS.md` |
