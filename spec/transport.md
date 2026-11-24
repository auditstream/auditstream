# Actor Transport and Precedence

**Spec version:** 0.1.0 (tag: `spec-v0.1.0`)

This document defines how adapters send an actor payload to PostgreSQL, how the engine resolves it, and how malformed inputs are handled. The key words "MUST", "MUST NOT", "SHOULD", "SHOULD NOT", and "MAY" are interpreted as described in [spec/README.md](README.md#normative-wording).

The actor and context payload schema and the 512-byte limit are defined in `spec/actor.md`.

## Trailing SQL comment transport

The trailing SQL comment is the default transport. It requires no explicit transaction and does not pin sessions behind AWS RDS Proxy.

### Comment placement and grammar

An encoder MUST append the comment at the very end of the SQL statement, preceded by a single ASCII space (`0x20`). Only ASCII whitespace or semicolons (`;`) MAY follow the closing `*/`.

The comment follows sqlcommenter syntax and MAY share the comment block with other sqlcommenter key-value pairs (such as `traceparent`), separated by `,` with no spaces around the comma. The comment body MUST NOT contain `/`. Encoders MUST emit at most one `auditstream` key in the trailing comment.

```abnf
trailing-comment   = SP "/*" comment-body "*/" *( SP / HTAB / CR / LF / ";" )
comment-body       = pair *( "," pair )
pair               = auditstream-pair / other-pair
auditstream-pair   = "auditstream='" base64url-unpadded "'"
base64url-unpadded = 1*( ALPHA / DIGIT / "-" / "_" )
other-pair         = other-key "='" other-value "'"
other-key          = 1*( ALPHA / DIGIT / "_" / "-" ) ; MUST NOT equal "auditstream"
other-value        = *( %x20-26 / %x28-2E / %x30-7E ) ; MUST NOT contain "'" or "/"
```

Because `base64url-unpadded` contains only ASCII letters, digits, `-`, and `_`, it can never contain `'`, `/`, or `*/` and therefore cannot break out of the single-quoted value or close the block comment.

### Canonical JSON and base64url encoding

Before encoding, encoders MUST validate the payload against the full actor and context schema (`spec/actor.md`) and serialize it to **canonical compact JSON**:

- No insignificant whitespace between tokens.
- Raw UTF-8 characters (do not needlessly `\uXXXX`-escape non-ASCII characters).
- Omit absent optional keys.
- Emit known keys in fixed order:
  - Top-level actor object: `type`, `id`, `onBehalfOf`, `tenant`, `context`.
  - Nested `onBehalfOf` object: `type`, `id`, `tenant`.
  - `context` object: `requestId`, `source`, `ip`, `userAgent`.
  - Any additional unknown keys follow known keys in lexicographic order.

Encoders MUST then encode the UTF-8 bytes of the canonical JSON using RFC 4648 Section 5 **unpadded base64url** (`A-Z`, `a-z`, `0-9`, `-`, `_` with all trailing `=` stripped).

Decoders MUST accept any valid JSON key order and any valid JSON whitespace once decoded.

#### Worked examples *(Informative)*

1. Minimal user actor:
   - Canonical JSON (30 UTF-8 bytes): `{"type":"user","id":"usr_42"}`
   - Unpadded base64url (40 chars): `eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0`
   - SQL statement:
     ```sql
     UPDATE invoice SET status = $1 WHERE id = $2 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/
     ```
2. Handbook AI agent actor:
   - Canonical JSON (103 UTF-8 bytes): `{"type":"ai_agent","id":"billing-agent","onBehalfOf":{"type":"user","id":"usr_42"},"tenant":"org_acme"}`
   - Unpadded base64url (138 chars): `eyJ0eXBlIjoiYWlfYWdlbnQiLCJpZCI6ImJpbGxpbmctYWdlbnQiLCJvbkJlaGFsZk9mIjp7InR5cGUiOiJ1c2VyIiwiaWQiOiJ1c3JfNDIifSwidGVuYW50Ijoib3JnX2FjbWUifQ`
   - SQL statement (combined with another sqlcommenter key):
     ```sql
     UPDATE invoice SET status = $1 WHERE id = $2 /*traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01',auditstream='eyJ0eXBlIjoiYWlfYWdlbnQiLCJpZCI6ImJpbGxpbmctYWdlbnQiLCJvbkJlaGFsZk9mIjp7InR5cGUiOiJ1c2VyIiwiaWQiOiJ1c3JfNDIifSwidGVuYW50Ijoib3JnX2FjbWUifQ'*/
     ```

### Named prepared statements prohibition

Adapters MUST NOT combine per-actor trailing SQL comments with named prepared statements (for example `node-postgres` `name` or `postgres.js` default `prepare: true`). Each distinct actor produces a different SQL string and creates a distinct server-side prepared statement; drivers such as `node-postgres` never evict named statements, causing unbounded backend memory growth. When named prepared statements are required, adapters MUST use the `set_config` transport inside a transaction instead.

## Transaction-local `set_config` transport

When trailing comments are unavailable (such as behind Prisma Accelerate or when named prepared statements are required), adapters MAY send the actor JSON inside an explicit transaction using `set_config`:

```sql
SELECT set_config('auditstream.actor', '{"type":"user","id":"usr_42"}', true);
```

- The configuration parameter name MUST be `auditstream.actor`.
- The value MUST be valid compact UTF-8 JSON conforming to the actor schema and the 512-byte limit (raw JSON, not base64url-encoded).
- The third argument (`is_local`) MUST be `true`. A session-level setting (`is_local = false`) would outlive the transaction and leak the actor onto subsequent requests sharing the same server connection behind transaction-mode poolers (PgBouncer, Supavisor, Neon pooler).
- After the transaction ends, PostgreSQL reports `current_setting('auditstream.actor', true)` as an empty string (`''`), so readers MUST read it through `nullif(current_setting('auditstream.actor', true), '')`.

*(Informative)* Behind AWS RDS Proxy, `set_config(..., true)` pins the server session, whereas trailing comments do not.

## Resolution precedence and `actor_source`

Database readers MUST resolve the actor in the following strict precedence order and set `actor_source` accordingly:

| Priority | Source | Condition | Resulting `actor` | Resulting `actor_source` |
| --- | --- | --- | --- | --- |
| 1 | `set_config` | `nullif(current_setting('auditstream.actor', true), '')` is non-NULL and valid | Parsed JSON object | `'set_config'` |
| 1 (malformed) | `set_config` | `nullif(current_setting('auditstream.actor', true), '')` is non-NULL and malformed | `NULL` | `'invalid'` (MUST NOT fall through) |
| 2 | `comment` | Trailing comment in `right(current_query(), 4096)` has key `auditstream` and valid payload | Decoded JSON object | `'comment'` |
| 2 (malformed) | `comment` | Trailing comment in `right(current_query(), 4096)` has key `auditstream` and malformed payload | `NULL` | `'invalid'` (MUST NOT fall through) |
| 3 | `jwt` | `nullif(current_setting('request.jwt.claims', true), '')` is valid JSON object with non-empty string `sub` | `{"type":"user","id":"<sub>"}` | `'jwt'` |
| 3 (malformed) | `jwt` | `nullif(current_setting('request.jwt.claims', true), '')` is invalid JSON | `NULL` | `'invalid'` |
| 4 | none | None of the above produced an actor | `NULL` | `NULL` |

In all cases — including when `actor` is `NULL` — the trigger MUST still write the history row with `db_user` set to `session_user`.

### Reader extraction algorithm for comments

To avoid scanning arbitrarily large SQL statements and to work within PostgreSQL's regex quantifier limit (maximum repetition count of 255):

1. Slice the tail of the query with `right(current_query(), 4096)`.
2. Extract the body of the final comment whose body contains no `/`:
   ```sql
   regexp_match(right(current_query(), 4096), '/\*([^/]*)\*/[\s;]*$')
   ```
3. If a trailing comment exists, locate the `auditstream` key value:
   ```sql
   regexp_match(body[1], '(?:^|,)auditstream=''([^'']*)''')
   ```
   - If the `auditstream` key is absent (for example `/*traceparent='00-...'*/`), fall through to priority 3 (`jwt`).
   - If the `auditstream` key is present, validate outside the regex that `length(raw) BETWEEN 1 AND 683`, `length(raw) % 4 <> 1`, and `raw ~ '^[A-Za-z0-9_-]+$'`.
   - Convert unpadded base64url to standard padded base64, decode to UTF-8, verify decoded octet length `<= 512`, and parse as JSON.
   - Any failure in validation or decoding yields `actor = NULL`, `actor_source = 'invalid'`, and MUST NOT fall through to `jwt`.

*(Informative)* Because step 3 captures any characters inside `auditstream='...'` before validating the base64url alphabet, a malformed value such as `/*auditstream='!!!notbase64'*/` produces `actor_source = 'invalid'` rather than silently being skipped as `NULL`.

## Size limits

| Representation | Maximum |
| --- | --- |
| Compact UTF-8 JSON payload (actor + optional `context`) | **512 bytes** |
| Unpadded base64url encoded value (`ceil(512 * 4 / 3)`) | **683 characters** |

- **Encoders** MUST reject any payload exceeding 512 UTF-8 bytes (or 683 unpadded base64url characters) with an error and MUST NOT truncate or send it.
- **Database readers** MUST check the length outside the regex (because PostgreSQL caps regex repetition counts at 255) and treat any `set_config` value over 512 UTF-8 bytes or any `auditstream` comment value over 683 characters (or decoding to over 512 UTF-8 bytes) as malformed (`actor = NULL`, `actor_source = 'invalid'`).

## Malformed inputs and validation split

A malformed actor payload MUST NEVER abort the application's SQL write. Any of the following inputs count as malformed and MUST yield `actor = NULL` and `actor_source = 'invalid'`:

- Invalid JSON syntax in `auditstream.actor`, decoded comment payload, or `request.jwt.claims`.
- Non-object JSON (for example a JSON string, number, array, boolean, or `null`).
- Missing or non-string (or empty string) `type` or `id`.
- `auditstream` comment value that is empty (`auditstream=''`), contains characters outside `[A-Za-z0-9_-]` (for example `auditstream='!!!notbase64'`), has invalid base64url length (`length % 4 = 1`), or decodes to invalid UTF-8.
- Payload exceeding 512 UTF-8 bytes or encoded comment value exceeding 683 characters.

### Validation split between encoders and database readers

- **Encoders** MUST validate the complete actor and context schema (`spec/actor.md`, including allowed `type` enum values, one-level `onBehalfOf`, and `context` field types) and the 512-byte limit before sending.
- **Database trigger readers** MUST validate JSON syntax, the size limit, and that `type` and `id` are non-empty strings. They MAY omit full enum and nested-field schema validation to keep trigger overhead minimal on the write path.
