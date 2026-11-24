# Specification Changelog

All notable changes to the Auditstream specification (`spec/`) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the specification adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) independently of engine and adapter releases. While the specification is `0.x`, breaking changes bump the minor version and are marked **BREAKING**.

## [Unreleased]

Target release: `0.1.0` (unreleased until conformance fixtures are tagged in `#18`).

### Added

- Initial specification skeleton (`spec/README.md`), RFC 2119 / RFC 8174 normative wording rules, independent versioning policy, and cross-repository change order.
- Implementation compliance matrix (`spec/compliance.md`) covering the Rust engine, generated SQL, and TypeScript adapters.
