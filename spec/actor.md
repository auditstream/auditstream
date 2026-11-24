# Actor and Context Schema

**Spec version:** 0.1.0 (unreleased)

This document defines the normative JSON structure for Auditstream actor and context payloads. The key words "MUST", "MUST NOT", "SHOULD", "SHOULD NOT", and "MAY" are interpreted as described in [spec/README.md](README.md#normative-wording).

The machine-readable schema lives at [spec/schemas/actor.schema.json](schemas/actor.schema.json) (JSON Schema draft 2020-12).

## Actor object

An actor payload MUST be a JSON object describing who initiated a database write or application event.

| Field | JSON type | Presence | Meaning |
| --- | --- | --- | --- |
| `type` | `string` | MUST | Actor category. MUST be one of `user`, `api_key`, `ai_agent`, `service`, `system`. |
| `id` | `string` | MUST | Non-empty opaque identifier for the actor (`minLength: 1`). No other format rule is imposed. |
| `onBehalfOf` | `object` | MAY | One nested actor on whose behalf the primary actor acted (for example, an `ai_agent` acting for a `user`). |
| `tenant` | `string` | MAY | Non-empty opaque tenant or organization identifier (`minLength: 1`). No other format rule is imposed. |
| `context` | `object` | MAY | Request or execution context traveling in the same payload as the actor. |

### `onBehalfOf` nesting limit

The `onBehalfOf` object MUST satisfy the `type`, `id`, and optional `tenant` rules above. A nested actor inside `onBehalfOf` MUST NOT carry its own `onBehalfOf` key (nesting is strictly one level deep).

## Context object

When present, `context` MUST be a JSON object carried under the `context` key of the actor payload so a single size limit covers both actor and context.

| Field | JSON type | Presence | Meaning |
| --- | --- | --- | --- |
| `requestId` | `string` | MAY | Non-empty correlation identifier for the originating request or background job. |
| `source` | `string` | MAY | Non-empty label for the entry surface (for example `web`, `api`, `cli`, `worker`, `migration`). |
| `ip` | `string` | MAY (opt-in) | Client IP address. SHOULD NOT be sent unless explicitly enabled by the application. |
| `userAgent` | `string` | MAY (opt-in) | Client user-agent string. SHOULD NOT be sent unless explicitly enabled by the application. |

## Privacy rule (IDs only)

Actor payloads SHOULD carry opaque identifiers only. Personal data such as names, email addresses, and IP addresses SHOULD NOT be sent by default, because SQL comments appear in `pg_stat_activity`, PostgreSQL server logs, and the first query sample kept by `pg_stat_statements`. The `context.ip` and `context.userAgent` fields are opt-in only.

## Forward compatibility and unknown keys

Readers MUST ignore unknown keys in both actor and context objects. Adding a new optional field to the contract is therefore a backwards-compatible minor specification release. Accordingly, [spec/schemas/actor.schema.json](schemas/actor.schema.json) sets `additionalProperties: true` on actor and context objects (while explicitly forbidding `onBehalfOf` inside `onBehalfOf`).

## Size limit

The compact UTF-8 JSON representation of the complete payload (actor plus `context`, when present) MUST NOT exceed **512 bytes**.

- Size is measured as the UTF-8 byte length of the compact serialized JSON payload (not UTF-16 code units or Unicode scalar values).
- Encoders MUST reject any payload whose compact UTF-8 JSON exceeds 512 bytes with an error instead of silently truncating it.

*(Informative)* JSON Schema cannot count UTF-8 byte lengths, so the 512-byte limit is enforced by the encoder and decoder implementations and verified by the shared conformance fixtures in `spec/conformance/`.

## Example *(Informative)*

```json
{
  "type": "ai_agent",
  "id": "billing-agent",
  "onBehalfOf": { "type": "user", "id": "usr_42" },
  "tenant": "org_acme"
}
```
