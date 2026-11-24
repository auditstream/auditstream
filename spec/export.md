# Export Record Format

**Spec version:** 0.1.0 (unreleased)

This document defines the normative record format written by `auditstream export` in JSON Lines and CSV encodings. The key words "MUST", "MUST NOT", "SHOULD", "SHOULD NOT", and "MAY" are interpreted as described in [spec/README.md](README.md#normative-wording).

The machine-readable JSON Schema lives at [spec/schemas/event.schema.json](schemas/event.schema.json) (JSON Schema draft 2020-12). Source table columns are defined in `spec/history-table.md`.

## Delivery and deduplication guarantee

Export delivery is pull-based and **at least once**: `auditstream export` reads an `xid8` snapshot window and acknowledges the cursor only after the sink flushes successfully. If a failure occurs after writing to a destination and before acknowledging the cursor, a subsequent export MAY emit the same event again.

Every exported record MUST carry its stable `event_id` (`uuid`) and `schema_version` (`integer`). Downstream consumers MUST deduplicate on `event_id`.

## Exported fields

Each exported record contains the following 15 fields in fixed `snake_case` order. Internal engine columns (`id`, `xact_id`, and `table_oid`) MUST NOT be exported.

| Order | Field | JSON type | Nullable | Source column (`auditstream.changes`) |
| --- | --- | --- | --- | --- |
| 1 | `event_id` | `string` (UUID) | No | `event_id` |
| 2 | `occurred_at` | `string` (RFC 3339 UTC, microseconds) | No | `occurred_at` |
| 3 | `source` | `string` (`"db"` or `"app"`) | No | `source` |
| 4 | `schema_name` | `string` or `null` | Yes | `schema_name` |
| 5 | `table_name` | `string` or `null` | Yes | `table_name` |
| 6 | `op` | `string` | No | `op` |
| 7 | `record_id` | `object` or `null` | Yes | `record_id` |
| 8 | `before` | `object` or `null` | Yes | `before` |
| 9 | `after` | `object` or `null` | Yes | `after` |
| 10 | `changed` | `array` of `string`, or `null` | Yes | `changed` |
| 11 | `actor` | `object` or `null` | Yes | `actor` |
| 12 | `context` | `object` or `null` | Yes | `context` |
| 13 | `actor_source` | `string` or `null` | Yes | `actor_source` |
| 14 | `db_user` | `string` | No | `db_user` |
| 15 | `schema_version` | `integer` (`>= 1`) | No | `schema_version` |

### Timestamp format (`occurred_at`)

`occurred_at` MUST be formatted as an RFC 3339 timestamp in UTC (`Z` suffix) with microsecond precision (6 fractional digits), for example `2026-11-24T10:15:30.123456Z`.

### Forward compatibility

Readers MUST ignore unknown fields in exported JSON objects so additive fields remain backwards-compatible minor releases.

## JSON Lines encoding

JSON Lines (`jsonl`) sinks MUST follow these rules:

- **One record per line:** each exported record MUST be serialized as a single compact JSON object with no unescaped embedded newlines.
- **Line endings and charset:** lines MUST be terminated with `\n` (LF) and encoded as UTF-8 without a byte-order mark (BOM).
- **Key order:** object keys MUST be written in the exact 15-field order listed in [Exported fields](#exported-fields).
- **Absent values:** nullable fields whose SQL value is `NULL` MUST be emitted as explicit JSON `null` (never omitted).
- **Number passthrough:** PostgreSQL `jsonb` numeric values inside `record_id`, `before`, `after`, `actor`, or `context` MUST pass through with arbitrary precision preserved (no rounding through IEEE-754 binary64). A 30-digit PostgreSQL `numeric` value MUST appear with all digits intact.

## CSV encoding

CSV (`csv`) sinks MUST follow RFC 4180 with these rules:

- **Charset and line endings:** UTF-8 without BOM, with `\r\n` (CRLF) line endings on every row (including the header and final row).
- **Header row:** the first row MUST be a header row containing the exact 15 field names in the order listed in [Exported fields](#exported-fields).
- **Quoting:** fields containing `,`, `"`, `\r`, or `\n` MUST be enclosed in double quotes (`"`), escaping internal double quotes by doubling them (`""`).
- **`NULL` versus empty string:** SQL `NULL` MUST be serialized as an empty unquoted cell (`,,`), whereas an empty string value `""` MUST be serialized as a quoted empty string (`,"",`).
- **JSON columns:** `record_id`, `before`, `after`, `changed`, `actor`, and `context` MUST be serialized as compact JSON text (preserving arbitrary-precision numbers and quoted per RFC 4180), or as an empty unquoted cell when `NULL`.
- **No formula escaping:** CSV writers MUST NOT prefix or alter cell values beginning with `=`, `+`, `-`, or `@`, because mutating values would corrupt the audit record.

> **Security note *(Informative)*:** Spreadsheet applications (such as Excel or Calc) may interpret unescaped cells starting with `=`, `+`, `-`, or `@` as formulas. Downstream users opening exported CSV files in spreadsheet tools SHOULD import columns as plain text rather than evaluating formulas.

## Examples *(Informative)*

Valid JSON examples covering `INSERT` (with a 30-digit `numeric` value preserved verbatim), `UPDATE` (with `changed`), `TRUNCATE`, and an `app` event live in `spec/schemas/examples/event/valid/`. Matching CSV output lives in [spec/schemas/examples/event/records.csv](schemas/examples/event/records.csv).
