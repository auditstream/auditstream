# Releases

The engine releases with release-plz and dist, the adapters with Changesets, and the spec has its own version; all three follow SemVer.

## Engine

1. release-plz opens a release pull request with the version bump and a changelog written by git-cliff from the squashed commit titles.
2. Merging it makes release-plz push the tag. It uses a GitHub App token, because tags pushed with the default `GITHUB_TOKEN` do not start other workflows.
3. The tag starts the dist workflow: GitHub Release, shell and PowerShell installers, the npm package `auditstream`, and the Homebrew formula in `auditstream/homebrew-tap`.
4. A custom dist publish job builds a multi-architecture image from the musl binaries and pushes it to `ghcr.io/auditstream/auditstream`.

## Adapters

- Every pull request that changes a package adds a changeset file, so parallel pull requests never conflict on version numbers. CI fails without one, except for docs and CI-only changes.
- The three packages are a Changesets `fixed` group and always share one version.
- Publishing uses npm trusted publishing over OIDC from a protected GitHub environment, with provenance. Classic npm tokens were revoked in December 2025, and no long-lived token is stored.
- Trusted publishing cannot create a package. Each new name is published once by hand, or staged with `npm stage publish` (npm 11.15 or newer); the trusted publisher is configured after that.

## Version policy

Each release line is versioned independently during `0.x` so engine fixes, TypeScript adapter APIs, and specification updates do not force artificial lockstep bumps:

- **Engine (`auditstream` CLI & crates, via `release-plz`):**
  - During `0.x`, breaking changes bump the **minor** version (`0.X.0`) and non-breaking features or fixes bump the **patch** version (`0.x.Y`, configured via `features_always_increment_minor = false` in `release-plz.toml`, matching `uv`).
  - From `1.0.0` onward, standard SemVer 2.0.0 applies (breaking → major, additive → minor, fixes → patch).
  - **The engine's public contract** is the CLI flags and exit codes, the `auditstream.toml` schema, the history table layout and SQL function signatures, and the export record format. Every exported record carries `schema_version`.
  - **A breaking change to the history table** ships with an upgrade migration written by `auditstream init`.
  - All engine workspace crates share one version; `spec/` moves independently.
- **TypeScript adapters (`auditstream-js`, via Changesets):**
  - `@auditstream/node`, `@auditstream/prisma`, and `@auditstream/drizzle` are a Changesets `fixed` group and share one version.
  - During `0.x`, additive API capabilities bump the **minor** version (for example, `0.2.0` introduced `audit.history()`, `audit.log()`, and `audit.query()`), fixes and docs bump the **patch** version (for example, `0.2.1`), and breaking changes bump the **minor** version and are explicitly marked **BREAKING** in the changeset and changelog. From `1.0.0` onward, breaking changes bump the major version.
- **Specification (`spec/`, tag `spec-v<version>`):**
  - Follows Semantic Versioning 2.0.0 independently (see [`spec/README.md`](../../spec/README.md#versioning)): editorial clarifications and conformance tightening that align implementations with already-specified encoder rules ship as **patch** releases (for example, `spec-v0.1.1`); additive contract fields or `0.x` breaking changes (marked **BREAKING** in `spec/CHANGELOG.md`) bump the **minor** version (`0.X.0`). From `1.0.0` onward, breaking changes bump the major version.

## Cross-repository compatibility

| Engine CLI (`auditstream`) | Specification (`spec-v*`) | TypeScript SDK (`@auditstream/*`) | History `schema_version` | PostgreSQL |
| --- | --- | --- | :---: | --- |
| `0.1.x` | `0.1.0` – `0.1.1` | `0.2.x` | `1` | `15` – `19` |

Adapters pin a specific tagged conformance fixture archive (`spec-v<version>`) in CI; see [`spec/compliance.md`](../../spec/compliance.md) for the per-feature conformance matrix.

## Supported versions

| Dependency | Supported in v0.1 |
| --- | --- |
| PostgreSQL | 15 to 19 (14 reached end of life on 2026-11-12) |
| Node.js | 22.12 or newer; tested on 22, 24 and 26 |
| Prisma | `@prisma/client` 7.1 and newer within 7.x; Prisma 8 after its stable release |
| Drizzle | 0.45 and 1.x |
