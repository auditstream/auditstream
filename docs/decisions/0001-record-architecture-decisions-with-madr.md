---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# Record architecture decisions with MADR

## Context and Problem Statement

Auditstream is built by several people and AI agents in parallel across multiple repositories. Architecture decisions that are hard to reverse or cross repositories must be recorded next to the code they describe and updated through pull requests so contributors do not revisit settled choices.

## Considered Options

* Record architecture decisions as MADR 4.0 Markdown files under `docs/decisions/NNNN-title.md`
* Use RFCs under `rfcs/NNNN-title.md` for every architecture decision
* Keep decisions only inside the engineering handbook without dedicated decision records

## Decision Outcome

Chosen option: "Record architecture decisions as MADR 4.0 Markdown files under `docs/decisions/NNNN-title.md`", because MADR 4.0 gives every decision a concise, standard structure next to the code, while RFCs remain reserved for breaking contract changes or new capabilities across repositories.

### Consequences

* Good, because every settled decision lives in the engine repository under `docs/decisions/NNNN-title.md` and is indexed in `docs/decisions/README.md`.
* Good, because issues and pull requests can link directly to the decision record that governs an area.
* Good, because breaking contract changes and cross-repository capabilities still go through RFCs with 3 business days of final comments.
* Bad, because adding or superseding a decision requires maintaining both the MADR file and its entry in the index.

## More Information

* [Documentation](../handbook/documentation.md)
