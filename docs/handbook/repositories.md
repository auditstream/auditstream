# Repositories and ownership

The auditstream organization holds one repository per deliverable, and the engine repository owns the contract every other repository depends on.

| Repository | Holds | Publishes | When |
| --- | --- | --- | --- |
| auditstream | Rust engine and CLI, the contract in `spec/`, architecture docs, ADRs and the shared contribution standards | Binaries through dist: npm `auditstream`, Homebrew, shell installer, `ghcr.io/auditstream/auditstream`. Crates go to crates.io once the event schema is stable | Created, private |
| auditstream-js | TypeScript adapters | npm `@auditstream/node`, `@auditstream/prisma`, `@auditstream/drizzle` | Created, private |
| homebrew-tap | Homebrew formula written by the release pipeline | Homebrew tap `auditstream/tap` | First release |
| .github | Organization profile and shared community files | None | When the code goes public |
| auditstream-python, -go, -ruby | Language adapters | PyPI `auditstream`, Go module | When users ask |

Ownership rules:

- The engine repository owns the contract. A change to `spec/` lands there first, with a spec version bump, before any adapter depends on it.
- Adapter repositories own their language idioms and packaging. They never redefine capture semantics.
- Work that touches several repositories is split into one issue per repository, each linked to a parent epic in the engine repository.
- This handbook lives in the engine repository under `docs/`. Adapter repositories link to it and add only what is specific to their language.
