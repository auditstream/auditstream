# Parallel work

Parallel work stays conflict-free when each issue owns its own paths and contract changes land before the code that uses them.

- **Claim by assignment.** Assign the issue before starting. For an agent, assign the human who runs it. An unassigned issue is free.
- **Disjoint paths.** Each issue lists its in-scope paths. Two issues in progress never share a path, except the shared files below.
- **Contract first.** A spec change merges in the engine repository, then each repository gets its own sub-issue to implement it.
- **Interfaces before implementations.** Types and traits in `auditstream-core` land first, with stub implementations. Work on each crate then fans out against those types.
- **Shared files stay small.** Changes to the workspace `Cargo.toml`, `pnpm-workspace.yaml` or re-exports in `lib.rs` and `index.ts` are kept to the lines the issue needs. Lockfile conflicts are resolved by regenerating, never by hand.
- **Isolation.** Every person or agent works on its own branch, in its own git worktree, and rebases on `main` at least daily.
- **Ready means unblocked.** `status:ready` is set only when every blocker is closed.
- **Who takes what.** Agents get only `agent-ready` issues. Spec changes, security fixes, releases and CI permission changes are led by a human; an agent may draft them.
- **Visible plan.** The [backlog section](roadmap-v0.1.md) shows which issues can run at the same time.
