# AGENTS.md

Instructions for AI coding agents working in `auditstream/auditstream`.
This is the only committed instruction file; do not commit `CLAUDE.md`. A personal `CLAUDE.md` or `CLAUDE.local.md` must start with `@AGENTS.md`, or Claude Code stops reading this file.

See the [engineering handbook](docs/handbook/README.md) for full architecture and workflow rules, especially [AI usage policy](docs/handbook/ai-policy.md), [Workflow](docs/handbook/workflow.md) and [Parallel work](docs/handbook/parallel-work.md). Folder-level `AGENTS.md` files (such as [spec/AGENTS.md](spec/AGENTS.md)) override this file inside their folder.

## Commands

Quality gates (defined in [Engineering standards](docs/handbook/engineering-standards.md); workspace added in A1, `xtask` gates in A2, Postgres harness in A5):

```sh
cargo xtask ci                                            # local entry point: all quality gates
cargo fmt --all -- --check                                # formatting
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked                    # unit and integration tests
cargo test --doc --workspace --locked                     # doctests (nextest skips them)
cargo deny check                                          # advisories, licenses, bans, sources
cargo hack check --rust-version --workspace --locked      # MSRV 1.96 check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
```

Toolchain: Rust `1.100` pinned in `rust-toolchain.toml`, edition `2024`, MSRV `1.96`. Lints live in `[workspace.lints]`, never in `RUSTFLAGS`.

## Repository map

```text
Cargo.toml                  virtual workspace root (members, dependencies, lints)
rust-toolchain.toml         pinned stable Rust toolchain
deny.toml                   cargo-deny advisories, licenses, bans and sources
crates/
  auditstream-core/         domain types, port traits, actor codec, diff, redaction (no I/O)
  auditstream-pg/           Postgres introspection, DDL/trigger generator, migrations, history reader
  auditstream-export/       sinks (JSON Lines, CSV)
  auditstream/              CLI binary: clap, config, logging, exit codes only
  auditstream-testkit/      shared Postgres test harness (added in A5)
xtask/                      cargo xtask ci | codegen | bench
spec/                       normative contract, schemas and conformance fixtures
docs/
  handbook/                 engineering handbook, evidence and prototype test-kit
  decisions/                MADR 4.0 architecture decision records
rfcs/                       RFCs for breaking contract changes and cross-repo features
```

Crate dependency rules ([Engine architecture](docs/handbook/engine-architecture.md)):

- `auditstream-core`: `serde` and `thiserror` only — never `tokio`, `sqlx` or any I/O crate.
- `auditstream-pg`: `auditstream-core`, `sqlx` (runtime API only, no `query!` macros), `tokio` — never `auditstream-export` or `auditstream`.
- `auditstream-export`: `auditstream-core`, `tokio` — never `sqlx` or `auditstream-pg`.
- `auditstream`: crates above plus `clap` and `anyhow` — wiring only, no business logic.

## Postgres tests

- Integration tests live in one test binary per crate at `tests/it/main.rs`; end-to-end CLI tests live in `crates/auditstream/tests`.
- The shared test harness uses `DATABASE_URL` when set and starts a container with `testcontainers` otherwise.
- Each test runs against its own fresh database. CI tests PostgreSQL 15 through 19.

## Boundaries

- **Generated files and lockfiles:** Regenerate `Cargo.lock` with Cargo; never edit it by hand. Review every `insta` snapshot change before accepting it.
- **Dependency bans:** `cargo-deny` allows `sqlx` only inside `auditstream-pg`. Keep `unsafe_code = "forbid"` and `unreachable_pub` clean.
- **Scope:** Take only issues labeled `agent-ready` with no open blockers and no assignee. Claim by assigning the human running you before starting. Modify only paths listed in the issue's **In** scope. If more paths are needed or an acceptance criterion looks wrong, stop and report to the maintainer; do not edit issue bodies.
- **Permissions and conversation:** Never approve or merge a pull request, push to `main`, change repository or organization settings, rulesets, secrets or tokens, publish releases or packages, or include credentials or customer data in prompts or fixtures. People write GitHub comments and review replies; draft them for the maintainer instead of posting.
- **Human-led areas:** Spec changes, security fixes, releases and CI permission changes are led by a maintainer; agents may only draft them when asked.

## Commits and pull requests

- One issue, one branch, one pull request. Use a dedicated git worktree under `worktrees/` and rebase on `main` at least daily.
- Branch name: `<type>/<issue>-<slug>` (for example `feat/25-trigger-generator`).
- Pull request title: Conventional Commits 1.0 (enforced because squash merge uses it on `main`).
  - Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore`, `revert`, `style`.
  - Scopes: `core`, `pg`, `export`, `cli`, `spec`, `docs`, `ci`, `deps`.
- Keep pull requests at or under ~400 changed lines (excluding lockfiles and generated files).
- Open pull requests as drafts and mark ready for review only after all verification commands pass.
- Include in the PR body: Summary, `Closes #N`, Spec impact (with spec version), How it was tested (exact commands and output; show new tests failing without the change), AI disclosure (`none`, `assisted` or `agent-authored` plus tool name), and the Definition of done checklist.

## Definition of done

- [ ] Every acceptance criterion in the issue is met
- [ ] New tests cover the change and pass in CI
- [ ] Docs, changelog entry or changeset, and the spec are updated where the change touches them
- [ ] No new warnings from clippy or Biome
- [ ] Reviewed, squash-merged, and the issue closed by the merge
