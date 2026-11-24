# Architecture Decision Records

This index lists the architecture decisions recorded for Auditstream, starting with the decisions taken in the [engineering handbook](../handbook/README.md).

## Index

| Number | Title | Status | Date |
| --- | --- | --- | --- |
| 0001 | [Record architecture decisions with MADR](0001-record-architecture-decisions-with-madr.md) | accepted | 2026-11-24 |
| 0002 | [Capture changes with generated triggers](0002-capture-changes-with-generated-triggers.md) | accepted | 2026-11-24 |
| 0003 | [Carry the actor in a trailing SQL comment](0003-carry-the-actor-in-a-trailing-sql-comment.md) | accepted | 2026-11-24 |
| 0004 | [Hexagonal Cargo workspace with port traits](0004-hexagonal-cargo-workspace-with-port-traits.md) | accepted | 2026-11-24 |
| 0005 | [Export with an xid8 snapshot window cursor](0005-export-with-an-xid8-snapshot-window-cursor.md) | accepted | 2026-11-24 |
| 0006 | [ESM-only TypeScript packages sharing one context](0006-esm-only-typescript-packages-sharing-one-context.md) | accepted | 2026-11-24 |
| 0007 | [Normative spec versioned separately](0007-normative-spec-versioned-separately.md) | accepted | 2026-11-24 |

## How to add a decision

Write an ADR when a decision is hard to reverse or crosses repositories (see [Documentation](../handbook/documentation.md)).

1. Pick the next sequential four-digit number and create `docs/decisions/NNNN-title.md`.
2. Write the record using [MADR 4.0](https://adr.github.io/madr/): YAML front matter (`status`, `date`, `decision-makers`), `Context and Problem Statement`, `Considered Options`, `Decision Outcome` with `Consequences`, and links to the relevant handbook or spec files.
3. Add a row for the new ADR to the index table above in the same pull request.
