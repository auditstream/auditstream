# Auditstream Specification

**Version:** 0.1.0 (unreleased)

This folder defines the normative Auditstream contract. Every engine and language adapter implementation MUST conform to the requirements here for the version it declares.

## Scope

### What this contract covers

The contract defines the cross-repository wire format and database interface shared between the Rust engine (`auditstream`) and every language adapter (`auditstream-js` and future language repositories):

- The actor and context JSON objects and size limits.
- The SQL actor transports (trailing SQL comment and transaction-local `set_config`), their encoding, and their resolution precedence.
- The history table schema, helper functions (`auditstream.log_event`), and observable guarantees of the generated capture triggers.
- The export record structure in JSON Lines and CSV formats.
- Language-neutral conformance fixtures that every implementation MUST pass.

### What this contract leaves to implementations

The contract does NOT prescribe:

- Language idioms, framework middleware signatures, or ORM integration ergonomics.
- Connection pooling libraries, async runtimes, or internal crate and package architecture.
- CLI flags, configuration file layout, logging format, or process exit codes.
- Packaging, installers, container images, or release pipelines.

## Normative wording

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this specification are to be interpreted as described in BCP 14 (RFC 2119 and RFC 8174) when, and only when, they appear in all capitals, as shown here.

All prose outside normative requirements — including sections labeled *(Informative)*, notes, rationale, and illustrative examples — is informative and does not add requirements.

## Specification files

Files not yet written appear as inline code rather than links.

| File | Defines | Issue |
| --- | --- | --- |
| [spec/README.md](README.md) | Scope, RFC 2119 / RFC 8174 wording, versioning rules | #13 (B1) |
| [spec/compliance.md](compliance.md) | Feature support matrix across implementations | #13 (B1) |
| [spec/CHANGELOG.md](CHANGELOG.md) | Independent spec release history | #13 (B1) |
| [spec/AGENTS.md](AGENTS.md) | *(Informative)* Agent contribution rules for `spec/` | #10 (A3) |
| `spec/actor.md` and `spec/schemas/actor.schema.json` | Actor and context object schemas and constraints | #14 (B2) |
| `spec/transport.md` | Comment transport, `set_config` transport, precedence, size limit, prepared statement rule | #15 (B3) |
| `spec/history-table.md` | History table columns, `log_event` function, generated SQL guarantees | #16 (B4) |
| `spec/export.md` and `spec/schemas/event.schema.json` | Export record format in JSON Lines and CSV | #17 (B5) |
| `spec/conformance/` | Language-neutral conformance fixture format and test cases | #18 (B6) |

## Versioning

The specification follows Semantic Versioning 2.0.0 starting at `0.1.0`, versioned independently from the engine and from every adapter repository. Spec `0.1.0` remains unreleased until conformance fixtures are tagged in `#18` (B6) as `spec-v0.1.0`.

- **Patch (`0.x.Y` / `X.Y.Z`):** editorial clarifications, typo fixes, or informative examples that do not alter required behavior.
- **Minor (`0.X.0` / `X.Y.0`):** additive fields or backwards-compatible extensions that existing conforming implementations continue to accept.
- **Breaking:** any change that an existing conforming engine, adapter, or export consumer would reject. While the specification is `0.x`, a breaking change bumps the minor version (`0.(X+1).0`) and MUST be marked `BREAKING` in [spec/CHANGELOG.md](CHANGELOG.md), matching the engine's `0.x` version policy. From `1.0.0` onward, a breaking change bumps the major version.

### When an RFC is required

Every breaking contract change and every new cross-repository capability MUST start with an RFC under `rfcs/NNNN-title.md` stating the problem, proposal, alternatives, and migration path. An RFC MUST allow at least 3 business days of final comments before merging.

### Order of a contract change

Contract changes MUST follow a fixed cross-repository order:

1. Merge the RFC (when required) and the specification pull request in `auditstream/auditstream` with review from both maintainers.
2. Release the specification with a version bump, a [spec/CHANGELOG.md](CHANGELOG.md) entry, and tagged conformance fixtures (`spec-v<version>`).
3. Open one implementation sub-issue per repository (`auditstream`, `auditstream-js`, and any other adapter repository) against the released spec version.

## Declaring a spec version

Every implementation MUST declare which specification version it targets:

- **Compliance matrix:** each implementation column header in [spec/compliance.md](compliance.md) states the declared spec version.
- **Adapters:** each adapter repository MUST pin a single tagged conformance fixture release (`spec-v<version>`) in CI and pass all applicable cases for that tag.

## References *(Informative)*

- [The contract](../docs/handbook/contract.md)
- [Documentation](../docs/handbook/documentation.md)
- [Parallel work](../docs/handbook/parallel-work.md)
- [ADR 0007: Normative spec versioned separately](../docs/decisions/0007-normative-spec-versioned-separately.md)
