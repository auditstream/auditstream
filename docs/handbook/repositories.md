# Repositories and ownership

The auditstream organization holds one repository per deliverable, and the engine repository owns the contract every other repository depends on.

| Repository | Holds | Publishes | Status |
| --- | --- | --- | --- |
| [`auditstream/auditstream`](https://github.com/auditstream/auditstream) | Rust engine and CLI, the contract in `spec/`, architecture docs, ADRs and the shared contribution standards | Binaries through dist: npm `auditstream`, Homebrew, shell installer, `ghcr.io/auditstream/auditstream`. Crates go to crates.io once the event schema is stable | Live, public |
| [`auditstream/auditstream-js`](https://github.com/auditstream/auditstream-js) | TypeScript adapters | npm `@auditstream/node`, `@auditstream/prisma`, `@auditstream/drizzle` | Live, public |
| [`auditstream/site`](https://github.com/auditstream/site) | Documentation and marketing site (`auditstream.dev`) | `https://auditstream.dev` | Live, public |
| [`auditstream/homebrew-tap`](https://github.com/auditstream/homebrew-tap) | Homebrew formula written by the release pipeline | Homebrew tap `auditstream/tap` | Live, public |
| [`auditstream/.github`](https://github.com/auditstream/.github) | Organization profile, reusable workflows, and shared community files | None | Live, public |
| `auditstream-python`, `-go`, `-ruby` | Language adapters | PyPI `auditstream`, Go module | Planned (when users ask) |

Ownership rules:

- The engine repository owns the contract. A change to `spec/` lands there first, with a spec version bump, before any adapter depends on it.
- Adapter repositories own their language idioms and packaging. They never redefine capture semantics.
- Work that touches several repositories is split into one issue per repository, each linked to a parent epic in the engine repository.
- This handbook lives in the engine repository under `docs/`. Adapter repositories link to it and add only what is specific to their language.
