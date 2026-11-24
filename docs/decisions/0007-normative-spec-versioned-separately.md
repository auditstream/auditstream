---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# Normative spec versioned separately

## Context and Problem Statement

The Rust engine (`auditstream`) and TypeScript adapters (`auditstream-js`), along with future Python, Go and Ruby adapters, live in separate repositories and release independently. All implementations must agree on the actor and context schema, SQL transport format, history table layout and functions, and export record format.

## Considered Options

* Own a normative `spec/` directory in the engine repository using RFC 2119 wording, versioned independently with SemVer starting at `0.1.0`, with JSON schemas, `spec/compliance.md`, `spec/CHANGELOG.md` and language-neutral conformance fixtures shipped as a tagged artifact
* Couple the contract version to engine releases without a standalone specification or shared conformance fixtures

## Decision Outcome

Chosen option: "Own a normative `spec/` directory in the engine repository, versioned independently with SemVer starting at `0.1.0`, with shared conformance fixtures", because it lets the engine and each language adapter evolve and release independently while proving conformance against the exact same fixtures.

An additive field is a minor spec release; any change an existing adapter would reject is breaking and requires an RFC (with 3 business days of final comments). Every contract change follows a fixed order: RFC or spec pull request merged in `auditstream`, spec released, then one sub-issue per repository to implement it.

### Consequences

* Good, because engine releases, adapter releases and spec releases each follow SemVer independently without forcing lockstep version bumps across repositories.
* Good, because every adapter declares which spec version it implements and validates its behavior in CI against a pinned conformance fixture tag.
* Good, because adapter repositories own only language idioms and packaging and never redefine capture semantics.
* Bad, because contract changes require an ordered cross-repository workflow (spec merged and tagged before adapter sub-issues implement it).

## More Information

* [The contract](../handbook/contract.md)
* [Repositories and ownership](../handbook/repositories.md)
* [Documentation](../handbook/documentation.md)
* [Releases](../handbook/releases.md)
