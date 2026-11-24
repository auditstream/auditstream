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

- **0.x:** a breaking change bumps the minor version, anything else the patch, as in uv. 1.0 comes after the contract has held through real use.
- **The engine's public contract** is the CLI flags and exit codes, the config file schema, the history table layout and function names, and the export record. Every exported record carries `schema_version`.
- **A breaking change to the history table** ships with an upgrade migration that `auditstream init` writes.
- **The engine crates share one workspace version.** The spec version moves independently.

## Supported versions

| Dependency | Supported in v0.1 |
| --- | --- |
| PostgreSQL | 15 to 19 (14 reached end of life on 2026-11-12) |
| Node.js | 22.12 or newer; tested on 22, 24 and 26 |
| Prisma | `@prisma/client` 7.1 and newer within 7.x; Prisma 8 after its stable release |
| Drizzle | 0.45 and 1.x |
