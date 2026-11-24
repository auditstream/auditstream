# The contract

The contract is the normative `spec/` folder in the engine repository, versioned on its own, and every adapter declares which spec version it implements and proves it with shared conformance fixtures.

## What the spec contains

| File | Defines |
| --- | --- |
| `spec/README.md` | Scope, RFC 2119 wording, versioning rules |
| `spec/actor.md` and `schemas/actor.schema.json` | The actor and context objects |
| `spec/transport.md` | How the actor reaches Postgres: comment key, `set_config` key, precedence, size limit |
| `spec/history-table.md` | Table, columns, functions and the guarantees of the generated SQL |
| `spec/export.md` and `schemas/event.schema.json` | The record written by `export` in JSON Lines and CSV |
| `spec/conformance/` | Language-neutral fixtures every implementation must pass |
| `spec/compliance.md` | Matrix of features by implementation: `+`, `-` or `N/A` |
| `spec/CHANGELOG.md` | Spec releases, separate from engine releases |

## Actor and context

```json
{
  "type": "ai_agent",
  "id": "billing-agent",
  "onBehalfOf": { "type": "user", "id": "usr_42" },
  "tenant": "org_acme"
}
```

- `type` MUST be one of `user`, `api_key`, `ai_agent`, `service`, `system`.
- `id` MUST be present. `onBehalfOf` MAY hold one nested actor, one level deep. `tenant` MAY be present.
- Names, emails and IP addresses SHOULD NOT be sent. `context` MAY carry `requestId` and `source`; IP and user agent are opt-in.
- The serialized actor plus context MUST NOT exceed 512 bytes of JSON.

## Transport

```sql
-- per statement (sqlcommenter format, appended at the end)
UPDATE invoice SET status = $1 WHERE id = $2 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/

-- per transaction
SELECT set_config('auditstream.actor', '{"type":"user","id":"usr_42"}', true);
```

- The comment value MUST be the actor JSON encoded as unpadded base64url. Base64url cannot contain `*/`, so the payload can never close the comment and inject SQL.
- The comment MUST be the last comment in the statement and MAY share the comment with other sqlcommenter keys such as `traceparent`.
- Adapters MUST NOT combine per-actor comments with named prepared statements. Each distinct comment becomes a new statement, and node-postgres never evicts them.
- Precedence: `set_config` first, then the comment, then `request.jwt.claims`, as in [the capture design](capture-design.md#who-acted-resolution-order).

## Versioning and conformance

- The spec follows SemVer from `0.1.0`. An additive field is a minor release; any change an existing adapter would reject is breaking and needs an RFC.
- Conformance fixtures are JSON files of inputs and expected outputs: actor to encoded comment, comment to decoded actor, and SQL scenario to expected history rows. They ship as a tagged artifact of the engine repository, and each adapter's CI pins one tag.
- A contract change follows a fixed order: RFC or spec pull request merged, spec released, then one sub-issue per repository to implement it.
