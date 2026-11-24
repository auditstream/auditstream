# Documentation

Decisions are recorded as ADRs, the contract as a normative spec, and user docs follow Diátaxis. Every document lives next to the code it describes and changes in the same pull request.

| Kind | Where | Format | Written when |
| --- | --- | --- | --- |
| Architecture decision (ADR) | `auditstream/docs/decisions/NNNN-title.md` | MADR 4.0 | A decision is hard to reverse or crosses repositories |
| Contract | `auditstream/spec/` | RFC 2119 wording (MUST, SHOULD, MAY), its own SemVer and CHANGELOG | Before any code that changes the contract |
| RFC | `auditstream/rfcs/NNNN-title.md` | Problem, proposal, alternatives, migration; 3 business days of final comments | A breaking contract change or a new capability across repositories |
| User guide | `auditstream/docs/` in four parts: tutorials, how-to, reference, explanation | Diátaxis, added page by page, no empty sections | With the feature's pull request |
| API reference | rustdoc for crates; TSDoc plus a committed API Extractor report in auditstream-js | Generated | Every public item has a doc comment |
| Package README | Each crate and package | Purpose, install, supported versions, quick start, limits, link to the engine | When the package is created |
| This handbook | `auditstream/docs/handbook/`, one file per section | Markdown | Changed only by pull request |

Writing rules: English, sentences under 25 words, diagrams in Mermaid inside Markdown, and examples that run. The [ADR index](../decisions/README.md) starts with the decisions taken in this handbook.
