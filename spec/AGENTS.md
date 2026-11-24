# spec/AGENTS.md

Additional rules for work inside `spec/`. These rules apply on top of the root [AGENTS.md](../AGENTS.md) and win where they are more specific.

See [The contract](../docs/handbook/contract.md) and [Documentation](../docs/handbook/documentation.md) for the full contract rules.

## Ownership and review

- **Human-led:** A maintainer leads every change to `spec/`. Spec issues are not labeled `agent-ready`; an agent may only draft a change when asked by a maintainer.
- **Two-maintainer approval:** Changes under `spec/` require review and approval from both maintainers before merge.
- **Cross-repository order:** Contract changes land in `auditstream/spec/` first, are released with a spec version bump and tagged conformance fixtures, and only then are implemented in `auditstream` and adapter repositories through separate sub-issues.

## Requirements for normative changes

Every normative change inside `spec/` must:

1. **Use RFC 2119 wording** (`MUST`, `MUST NOT`, `SHOULD`, `SHOULD NOT`, `MAY`) consistently for requirements.
2. **Bump the spec SemVer version** (starting from `0.1.0`, independent of engine releases):
   - Patch (`0.x.Y`): editorial clarifications with no behavioral change.
   - Minor (`0.X.0`): additive fields or backwards-compatible extensions.
   - Breaking (`0.(X+1).0` pre-1.0 / `(X+1).0.0` post-1.0): any change an existing adapter would reject; requires an RFC in `rfcs/NNNN-title.md` with 3 business days of final comments before merging.
3. **Add a `spec/CHANGELOG.md` entry** describing the change under the new spec version.
4. **Update schemas, conformance fixtures and compliance matrix:** keep `spec/schemas/*.json`, language-neutral fixtures in `spec/conformance/`, and `spec/compliance.md` in sync with the normative text.
