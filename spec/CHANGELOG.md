# Specification Changelog

All notable changes to the Auditstream specification (`spec/`) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the specification adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) independently of engine and adapter releases. While the specification is `0.x`, breaking changes bump the minor version and are marked **BREAKING**.

## [Unreleased]

## [0.1.0] - 2026-11-24

Tag: `spec-v0.1.0` (`auditstream-spec-0.1.0.tar.gz`).

### Added

- Initial specification skeleton (`spec/README.md`), RFC 2119 / RFC 8174 normative wording rules, independent versioning policy, and cross-repository change order.
- Implementation compliance matrix (`spec/compliance.md`) covering the Rust engine, generated SQL, and TypeScript adapters.
- Actor and context schema (`spec/actor.md`, `spec/schemas/actor.schema.json`) with valid and invalid JSON examples and the 512-byte payload limit.
- Actor transport and precedence specification (`spec/transport.md`), including canonical JSON, unpadded base64url comments, transaction-local `set_config`, JWT fallback, and malformed-input handling.
- History table, partitioning, trigger guarantees, `auditstream.log_event()`, security model, export cursor contract, and reference DDL (`spec/history-table.md`).
- Export record format (`spec/export.md`, `spec/schemas/event.schema.json`) for JSON Lines and RFC 4180 CSV with valid/invalid examples and CSV fixture.
- Language-neutral conformance fixture format (`spec/conformance/README.md`, `spec/conformance/fixture.schema.json`), transport fixtures (`encode.json`, `decode.json`, `precedence.json`), and capture fixtures (`kit-t1-t9.json`, `edge-tables.json`).
