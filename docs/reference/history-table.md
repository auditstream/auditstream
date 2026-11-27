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
- A `BRIN` index on `(occurred_at)` (`changes_occurred_at_brin_idx`) always exists on `auditstream.changes`. On fresh installations, it is created with `autosummarize = on` (inherited by all future partitions) so PostgreSQL autovacuum summarizes newly filled 128-page ranges incrementally on each `autovacuum_naptime` tick without waiting for a table `VACUUM`. Because PostgreSQL does not support `ALTER INDEX ... SET (...)` on partitioned parent indexes, upgrades from earlier releases leave the partitioned parent index unchanged and enable `autosummarize = on` on `changes_default` and current/future monthly leaf partition indexes (`changes_YYYY_MM` from the current UTC month onward, plus partitions later created by migrations or `auditstream verify --fix`), leaving historical partitions older than the current UTC month untouched.

### TOAST compression

When the target PostgreSQL server supports LZ4 TOAST compression (`'lz4' = ANY(enumvals)` on `default_toast_compression` in `pg_catalog.pg_settings`), Auditstream sets `COMPRESSION lz4` on the `before` and `after` `jsonb` columns for parent `auditstream.changes`, `changes_default`, and current/future monthly partitions (`changes_YYYY_MM` from the current UTC month onward).

- **Future partitions:** Monthly partitions created afterward by migrations or `auditstream verify --fix` automatically inherit `COMPRESSION lz4` from parent `auditstream.changes`.
- **Historical partitions:** Partitions older than the current UTC month are left untouched because `ALTER TABLE ... SET COMPRESSION` does not rewrite already-stored TOAST values, avoiding unnecessary `ACCESS EXCLUSIVE` locks on archived months during migrations.
- **Servers without LZ4:** On PostgreSQL builds compiled without `--with-lz4`, migrations detect the missing codec lock-free via `pg_catalog.pg_settings`, skip `SET COMPRESSION lz4`, and fall back to PostgreSQL's built-in `pglz` compression.
- **Dump/restore portability:** When dumping from an LZ4-enabled server and restoring onto a PostgreSQL server compiled without `--with-lz4`, pass `--no-toast-compression` to `pg_dump` or `pg_dumpall` at dump time so standalone `ALTER TABLE ... SET COMPRESSION lz4;` statements are omitted (rows are logically decompressed on dump and re-compressed on restore). `pg_restore` does not accept `--no-toast-compression`; if an archive was already dumped without that flag, re-dump with `pg_dump --no-toast-compression` or remove those `ALTER TABLE ... SET COMPRESSION lz4;` statements from SQL emitted by `pg_restore -f -`.
- **Replication:** Physical (streaming / WAL-based) standbys replicate compressed TOAST tuples verbatim and must also be compiled with `--with-lz4`. Logical replication decodes tuples before sending and is unaffected.

## Capture triggers

`auditstream init` installs four triggers on each audited table:

| Trigger name | Event and granularity | Behavior |
| --- | --- | --- |
| `auditstream_insert` | `AFTER INSERT ... FOR EACH STATEMENT` | Resolves actor once per statement; records `after` |
| `auditstream_update` | `AFTER UPDATE ... FOR EACH ROW` | Compares JSONB row images after exclusions and sets `changed` for recorded updates |
| `auditstream_delete` | `AFTER DELETE ... FOR EACH STATEMENT` | Resolves actor once per statement; records `before` |
| `auditstream_truncate` | `AFTER TRUNCATE ... FOR EACH STATEMENT` | Records table and actor with `NULL` row images |

Excluded columns never appear in `before`, `after`, or `changed`. Primary key columns cannot be excluded so `record_id` is never nullified.

## Application events (`auditstream.log_event`)

Record domain events without writing directly to `auditstream.changes`:

```sql
SELECT auditstream.log_event(
  action text,
  target jsonb DEFAULT NULL,
  data jsonb DEFAULT NULL
) RETURNS uuid;
```

- `action` must be non-NULL, 1 to 64 characters long, contain no control characters (`Cc`), have no leading or trailing ASCII or Unicode whitespace (`White_Space`), and not match `INSERT`, `UPDATE`, `DELETE`, or `TRUNCATE` (ASCII case-insensitively).
- Invalid actions raise PostgreSQL error `22023` (`invalid_parameter_value`).
- Rows written by `log_event` set `source = 'app'`, `op = action`, `record_id = target`, and `after = data`.
- `log_event` resolves `actor`, `context`, and `actor_source` using `auditstream.resolve_actor()`, and returns the generated `event_id`.

## Security model

- A `NOLOGIN` role named `auditstream_owner` owns the schema, history table, all child partitions, the `changes_id_seq` identity sequence, and all helper functions (`capture()`, `resolve_actor()`, and `log_event()`).
- `auditstream.capture()`, `auditstream.resolve_actor()`, and `auditstream.log_event()` run as `SECURITY DEFINER` with `SET search_path = pg_catalog, pg_temp`.
- `auditstream.capture()` verifies that `TG_RELID` matches or is a partition descendant (`pg_partition_ancestors`) of its configured parent table, preventing forged `source = 'db'` rows from triggers attached to unrelated tables.
- Migrations and partition maintenance sanitize ACLs with `CASCADE` to neutralize permissive `ALTER DEFAULT PRIVILEGES` rules: `PUBLIC` has no grants, child partitions and `changes_id_seq` have zero non-owner grants, `capture()` and `resolve_actor()` have zero non-owner `EXECUTE` grants, and `changes` only permits non-grantable `SELECT` for non-owners.
- Application roles should hold `USAGE` on `auditstream`, `SELECT` on `auditstream.changes`, and `EXECUTE` on `auditstream.log_event`.
- Application roles hold no `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, or `WITH GRANT OPTION` privileges on `auditstream.changes` or its partitions.

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
