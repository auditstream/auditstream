# History Table and `log_event` Reference

> **Public contract:** Columns, partition naming, `log_event`, and cursor semantics are governed by [spec/history-table.md](../../spec/history-table.md) and [Releases: Version policy](../handbook/releases.md#version-policy).

Every captured database write and every application event lands in `auditstream.changes`.
If you configure a custom `--schema`, substitute that schema name for `auditstream`.

## Columns (`auditstream.changes`)

| Column | Type | Nullability | Default | Meaning |
| --- | --- | --- | --- | --- |
| `id` | `bigint` | `NOT NULL` | `GENERATED ALWAYS AS IDENTITY` | Monotonic insert sequence (insert order, not commit order) |
| `event_id` | `uuid` | `NOT NULL` | `gen_random_uuid()` | Stable unique ID for downstream deduplication |
| `occurred_at` | `timestamptz` | `NOT NULL` | `clock_timestamp()` | Recording timestamp and range partition key |
| `xact_id` | `xid8` | `NOT NULL` | `pg_current_xact_id()` | 64-bit writing transaction ID used by export cursors |
| `source` | `text` | `NOT NULL` | `'db'` | `'db'` for trigger captures; `'app'` for `log_event` |
| `schema_name` | `text` | `NULL` | `NULL` | Audited table schema for `'db'`; `NULL` for `'app'` |
| `table_name` | `text` | `NULL` | `NULL` | Audited table name for `'db'`; `NULL` for `'app'` |
| `table_oid` | `oid` | `NULL` | `NULL` | Audited table OID (`TG_RELID`) for `'db'`; `NULL` for `'app'` |
| `op` | `text` | `NOT NULL` | — | `'INSERT'`, `'UPDATE'`, `'DELETE'`, `'TRUNCATE'`, or app action |
| `record_id` | `jsonb` | `NULL` | `NULL` | Primary key JSON object, `target` for `'app'`, or `NULL` |
| `before` | `jsonb` | `NULL` | `NULL` | Pre-change row minus excluded columns (`UPDATE`, `DELETE`) |
| `after` | `jsonb` | `NULL` | `NULL` | Post-change row minus excluded columns, or `'app'` `data` |
| `changed` | `text[]` | `NULL` | `NULL` | Changed column names on `'UPDATE'`; `NULL` otherwise |
| `actor` | `jsonb` | `NULL` | `NULL` | Resolved actor object without `context`, or `NULL` |
| `context` | `jsonb` | `NULL` | `NULL` | Resolved `context` object, or `NULL` |
| `actor_source` | `text` | `NULL` | `NULL` | `'set_config'`, `'comment'`, `'jwt'`, `'invalid'`, or `NULL` |
| `db_user` | `text` | `NOT NULL` | `session_user` | Session role that initiated the statement |
| `schema_version` | `smallint` | `NOT NULL` | `1` | History table layout version (`1` in v0.1) |

The primary key is `(id, occurred_at)` to satisfy PostgreSQL range partitioning.

## Partitioning and indexes

- `auditstream.changes` uses `PARTITION BY RANGE (occurred_at)` with monthly UTC partitions named `changes_YYYY_MM`.
- A `DEFAULT` partition named `changes_default` always exists so writes never fail when maintenance lags.
- Setup and `auditstream verify --fix` maintain monthly partitions three months ahead of the current UTC month.
- If rows land in `changes_default`, PostgreSQL blocks attaching overlapping monthly partitions until those rows move.
- `auditstream verify` exits `1` whenever `changes_default` holds any row.
- A `BRIN` index on `(occurred_at)` always exists on `auditstream.changes`.

## Capture triggers

`auditstream init` installs four triggers on each audited table:

| Trigger name | Event and granularity | Behavior |
| --- | --- | --- |
| `auditstream_insert` | `AFTER INSERT ... FOR EACH STATEMENT` | Resolves actor once per statement; records `after` |
| `auditstream_update` | `AFTER UPDATE ... FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)` | Skips no-ops and excluded-only updates; sets `changed` |
| `auditstream_delete` | `AFTER DELETE ... FOR EACH STATEMENT` | Resolves actor once per statement; records `before` |
| `auditstream_truncate` | `AFTER TRUNCATE ... FOR EACH STATEMENT` | Records table and actor with `NULL` row images |

Excluded columns never appear in `before`, `after`, or `changed`.

## Application events (`auditstream.log_event`)

Record domain events without writing directly to `auditstream.changes`:

```sql
SELECT auditstream.log_event(
  action text,
  target jsonb DEFAULT NULL,
  data jsonb DEFAULT NULL
) RETURNS uuid;
```

- `action` must be non-NULL, 1 to 64 characters long, and not `INSERT`, `UPDATE`, `DELETE`, or `TRUNCATE`.
- Invalid actions raise PostgreSQL error `22023` (`invalid_parameter_value`).
- Rows written by `log_event` set `source = 'app'`, `op = action`, `record_id = target`, and `after = data`.
- `log_event` resolves `actor`, `context`, and `actor_source` using `auditstream.resolve_actor()`, and returns the generated `event_id`.

## Security model

- A `NOLOGIN` role named `auditstream_owner` owns the schema, history table, partitions, and functions.
- `auditstream.capture()`, `auditstream.resolve_actor()`, and `auditstream.log_event()` run as `SECURITY DEFINER` with `SET search_path = pg_catalog, pg_temp`.
- Application roles should hold `USAGE` on `auditstream`, `SELECT` on `auditstream.changes`, and `EXECUTE` on `auditstream.log_event`.
- Application roles hold no `INSERT`, `UPDATE`, `DELETE`, or `TRUNCATE` privileges on `auditstream.changes`.

## Export cursor contract

Concurrent transactions can commit out of `id` order.
`auditstream export` advances an `xid8` snapshot window instead of an `id` cursor:

```sql
SELECT *
FROM auditstream.changes
WHERE ((xact_id > $cursor_xact) OR (xact_id = $cursor_xact AND id > $cursor_id))
  AND xact_id < pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())
ORDER BY xact_id ASC, id ASC;
```

Rows below `pg_snapshot_xmin` have already committed or rolled back, so no later commit can appear behind the cursor.
