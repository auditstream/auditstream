# History Table and Functions Contract

**Spec version:** 0.1.0 (tag: `spec-v0.1.0`)

This document defines the normative PostgreSQL history table layout, partitioning rules, trigger guarantees, application event function (`auditstream.log_event`), security model, and export cursor contract. The key words "MUST", "MUST NOT", "SHOULD", "SHOULD NOT", and "MAY" are interpreted as described in [spec/README.md](README.md#normative-wording).

Actor payloads are defined in [spec/actor.md](actor.md) and actor resolution / `actor_source` values are defined in [spec/transport.md](transport.md).

The default audit schema name is `auditstream`. The engine allows configuring another schema name; everywhere this specification names `auditstream`, implementations MUST substitute the configured schema name consistently.

## Public contract and breaking changes

The following database surface is part of the public contract:

- The `auditstream.changes` table columns, PostgreSQL types, nullability, defaults, and column semantics.
- The `schema_version` column value (`1` for specification `0.1.0`).
- Partition naming (`changes_YYYY_MM` and `changes_default`) and the `DEFAULT` partition invariant.
- Function names and signatures: `auditstream.log_event(text, jsonb, jsonb)`, `auditstream.capture()`, and `auditstream.resolve_actor()`.
- Trigger names on audited tables: `auditstream_insert`, `auditstream_update`, `auditstream_delete`, and `auditstream_truncate`.
- The export cursor window and ordering semantics.

Any breaking change to the history table layout or functions MUST increment `schema_version`, follow the RFC process in [spec/README.md](README.md#when-an-rfc-is-required), and ship with an upgrade migration written by `auditstream init`.

## History table (`auditstream.changes`)

Every captured database change and every application event MUST be written as a row in `auditstream.changes` with the following columns:

| Column | PostgreSQL type | Nullability | Default | Meaning |
| --- | --- | --- | --- | --- |
| `id` | `bigint` | `NOT NULL` | `GENERATED ALWAYS AS IDENTITY` | Monotonic insert sequence; **insert order, NOT commit order**. |
| `event_id` | `uuid` | `NOT NULL` | `gen_random_uuid()` | Stable unique identifier used by export consumers for deduplication. |
| `occurred_at` | `timestamptz` | `NOT NULL` | `clock_timestamp()` | Wall-clock time when the row was recorded; partition key. |
| `xact_id` | `xid8` | `NOT NULL` | `pg_current_xact_id()` | 64-bit transaction ID of the writing transaction; export cursor key. |
| `source` | `text` | `NOT NULL` | `'db'` | `'db'` for trigger-captured writes; `'app'` for `auditstream.log_event()`. |
| `schema_name` | `text` | `NULL` | `NULL` | Audited table schema for `'db'` events; `NULL` for `'app'` events. |
| `table_name` | `text` | `NULL` | `NULL` | Audited table name for `'db'` events; `NULL` for `'app'` events. |
| `table_oid` | `oid` | `NULL` | `NULL` | Audited table OID (`TG_RELID`) for `'db'` events; `NULL` for `'app'` events. |
| `op` | `text` | `NOT NULL` | — | `'INSERT'`, `'UPDATE'`, `'DELETE'`, `'TRUNCATE'`, or the `'app'` action name. |
| `record_id` | `jsonb` | `NULL` | `NULL` | JSON object mapping primary key column names to values; `NULL` when table has no primary key or on `'TRUNCATE'`; `target` for `'app'` events. |
| `before` | `jsonb` | `NULL` | `NULL` | Pre-change row image minus excluded columns (`UPDATE` and `DELETE` only). |
| `after` | `jsonb` | `NULL` | `NULL` | Post-change row image minus excluded columns (`INSERT` and `UPDATE`), or `data` for `'app'` events. |
| `changed` | `text[]` | `NULL` | `NULL` | Column names whose values differ between `before` and `after` (excluding excluded columns) on `'UPDATE'`; `NULL` otherwise. |
| `actor` | `jsonb` | `NULL` | `NULL` | Resolved actor object (without `context`), or `NULL` when absent or malformed. |
| `context` | `jsonb` | `NULL` | `NULL` | Resolved `context` object extracted from the actor payload, or `NULL` when absent or malformed. |
| `actor_source` | `text` | `NULL` | `NULL` | `'set_config'`, `'comment'`, `'jwt'`, `'invalid'`, or `NULL` per [spec/transport.md](transport.md#resolution-precedence-and-actor_source). |
| `db_user` | `text` | `NOT NULL` | `session_user` | Database session role that initiated the write. MUST use `session_user` (never `current_user`, which inside `SECURITY DEFINER` is `auditstream_owner`). |
| `schema_version` | `smallint` | `NOT NULL` | `1` | Layout version of the history table (`1` in spec `0.1.0`). |

The primary key MUST be `(id, occurred_at)` so PostgreSQL range partitioning on `occurred_at` is satisfied.

## Partitioning and indexes

- `auditstream.changes` MUST be `PARTITION BY RANGE (occurred_at)` using monthly UTC boundaries `[YYYY-MM-01 00:00:00+00, next_month 00:00:00+00)`.
- Monthly partitions MUST be named `changes_YYYY_MM` (for example `changes_2026_11`).
- A `DEFAULT` partition named `changes_default` MUST always exist so writes never fail when partition maintenance lags.
- Setup and partition maintenance MUST maintain monthly partitions at least **3 months ahead** of the current month (current month plus the next 3 months).
- Once any row lands in `changes_default`, PostgreSQL refuses to create or attach a monthly partition overlapping that row's `occurred_at` until the row is detached, moved, and re-attached under lock. Therefore, `auditstream verify` MUST fail whenever `changes_default` contains one or more rows.
- A `BRIN` index on `(occurred_at)` MUST always exist on `auditstream.changes`. Additional lookup indexes (such as on `(table_name, record_id, occurred_at)`) MAY be created; their exact shape is left to implementations and benchmarks.

## Capture trigger guarantees

Generated triggers on audited tables MUST use `auditstream.capture()` (and `auditstream.resolve_actor()`) with the following trigger names and observable guarantees:

| Operation | Trigger name | Granularity and condition | Guarantees |
| --- | --- | --- | --- |
| `INSERT` | `auditstream_insert` | `AFTER INSERT ... REFERENCING NEW TABLE ... FOR EACH STATEMENT` | Resolves the actor once per statement. Writes one row per inserted row with `op = 'INSERT'`, `before = NULL`, `after = to_jsonb(row) - excluded`, `changed = NULL`. |
| `UPDATE` | `auditstream_update` | `AFTER UPDATE ... FOR EACH ROW` | Strips excluded columns from `before` and `after`; if `before = after` after stripping (a no-op update, or an update touching only excluded columns), MUST write no history row. Sets `changed` to the keys where `before->k IS DISTINCT FROM after->k`. MUST NOT have a `WHEN` clause: comparing `OLD.*` with `NEW.*` fails for column types without an equality operator (`json`, `xml`, `point`) and skips changes that type equality ignores (for example a case-only change in a `citext` column). |
| `DELETE` | `auditstream_delete` | `AFTER DELETE ... REFERENCING OLD TABLE ... FOR EACH STATEMENT` | Resolves the actor once per statement. Writes one row per deleted row with `op = 'DELETE'`, `before = to_jsonb(row) - excluded`, `after = NULL`, `changed = NULL`. |
| `TRUNCATE` | `auditstream_truncate` | `AFTER TRUNCATE ... FOR EACH STATEMENT` | Writes one row with `op = 'TRUNCATE'`, `record_id = NULL`, `before = NULL`, `after = NULL`, `changed = NULL`, and the resolved actor. |

Additional capture rules:

- **Excluded columns:** configured excluded columns MUST NEVER appear in `before`, `after`, or `changed`. Primary key columns MUST NOT be configured in excluded columns, so `record_id` is never silently nullified.
- **`record_id`:** MUST be a JSON object mapping each primary key column name to its value (taken from `after` on `INSERT`/`UPDATE` and from `before` on `DELETE`, supporting single and composite primary keys), and MUST be `NULL` for tables without a primary key and for `TRUNCATE`.
- **Partitioned user tables:** statement-level triggers (`auditstream_insert`, `auditstream_delete`, `auditstream_truncate`) attach to the parent partitioned table. A SQL statement that targets a child partition directly does not fire statement triggers attached to the parent, so `auditstream verify` MUST report audited partitioned tables and warn about direct partition writes.
- **Error behavior:** malformed actor payloads MUST NEVER abort the write (per [spec/transport.md](transport.md#malformed-inputs-and-validation-parity)). Any other trigger error aborts the user's transaction so audit gaps are not silently ignored.

## Application events (`auditstream.log_event`)

Adapters MUST NOT insert directly into `auditstream.changes`. Application-level events MUST go through:

```text
auditstream.log_event(action text, target jsonb DEFAULT NULL, data jsonb DEFAULT NULL) RETURNS uuid
```

Requirements:

- **Validation:**
  - `action` MUST be non-NULL, MUST have trimmed length between **1 and 64 characters** inclusive, MUST have total length at most **64 characters**, and `upper(btrim(action))` MUST NOT be `'INSERT'`, `'UPDATE'`, `'DELETE'`, or `'TRUNCATE'`.
  - `target`, when non-NULL, MUST be a JSON object (`jsonb_typeof(target) = 'object'`).
  - `data`, when non-NULL, MUST be a JSON object (`jsonb_typeof(data) = 'object'`).
  - Any invalid parameter MUST raise an exception (`22023` / `invalid_parameter_value`).
- **Columns written:** `source = 'app'`, `op = action`, `record_id = target`, `after = data`, `schema_name = NULL`, `table_name = NULL`, `table_oid = NULL`, `before = NULL`, `changed = NULL`, `actor` / `context` / `actor_source` from `auditstream.resolve_actor()`, `db_user = session_user`, `schema_version = 1`.
- **Return value:** MUST return the generated `event_id` (`uuid`).
- **Grants:** application roles MUST receive `EXECUTE` on `auditstream.log_event(text, jsonb, jsonb)` while keeping no write grants on `auditstream.changes`.

## Security model

- **Owner role:** a `NOLOGIN` role named `auditstream_owner` MUST own the `auditstream` schema, `auditstream.changes` (and its partitions), and all functions in the schema.
- **Partition creation under owner role (`ALTER DEFAULT PRIVILEGES` immunity):** `auditstream.changes`, `changes_default`, and every `changes_YYYY_MM` partition MUST be created under `auditstream_owner` (via `SET LOCAL ROLE auditstream_owner`) and child partitions MUST NOT grant write privileges (`INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `REFERENCES`, `TRIGGER`, or `MAINTAIN`) to `PUBLIC` or non-owner roles, even when the migration role has `ALTER DEFAULT PRIVILEGES ... GRANT ALL ON TABLES TO <app_role>`.
- **`SECURITY DEFINER` and `search_path`:** `auditstream.capture()`, `auditstream.log_event()`, and any helper invoked by them MUST be `SECURITY DEFINER`, owned by `auditstream_owner`, declared with `SET search_path = pg_catalog, pg_temp`, and MUST reference all objects by schema-qualified names.
- **Application role grants:** application roles MUST receive `USAGE` on `auditstream`, `SELECT` on `auditstream.changes`, and `EXECUTE` on `auditstream.log_event(text, jsonb, jsonb)`. Application roles MUST NOT receive `INSERT`, `UPDATE`, `DELETE`, or `TRUNCATE` on `auditstream.changes` or any child partition; their writes to audited tables are still captured through the `SECURITY DEFINER` trigger.
- **Managed PostgreSQL self-grant rule (PostgreSQL 16+):** on managed PostgreSQL (such as Neon and Supabase), migrations run as a `CREATEROLE` non-superuser role. Starting in PostgreSQL 16 (`createrole_self_grant = ''`), creating a role grants only `ADMIN`, not `USAGE` or `SET`. Setup MUST run inside a single transaction, check `pg_has_role(current_user, 'auditstream_owner', 'USAGE')` (and `'SET'` on PostgreSQL 16+), execute `GRANT auditstream_owner TO <current_user>` when false using a transaction-local flag (`set_config(..., true)`), and revoke that temporary grant before commit so failed or interrupted migrations never persist elevated role membership.
- **Role reuse rule:** setup MUST reuse existing roles (`auditstream_owner` and application roles) and MUST NEVER drop and recreate them. Connection poolers retain sessions authenticated as the original role OID, causing subsequent queries to fail with `permission denied` if the role is dropped.

## Export cursor contract

Because concurrent transactions can allocate `id` values in a different order than they commit, **`id` order is NOT commit order**. An export cursor based on `id` skips rows when an earlier transaction commits after a later transaction.

Export readers MUST advance a transaction-ID cursor using the `xid8` snapshot window:

```text
SELECT ...
FROM auditstream.changes
WHERE xact_id >= $cursor
  AND xact_id < pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())
ORDER BY xact_id ASC, id ASC
```

1. Compute `next_cursor := pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())` once at the start of the export window.
2. Read all rows where `xact_id >= cursor AND xact_id < next_cursor`, ordered by `xact_id ASC, id ASC`.
3. Persist `next_cursor` only after the sink has flushed the batch (at-least-once delivery).

## Reference DDL

The reference DDL below establishes the roles, schema, partitioned history table, monthly partitions (3-month horizon), `DEFAULT` partition, `BRIN` index, `resolve_actor()`, `log_event()`, and grants. It is verified on PostgreSQL 15 through 19 as both a `CREATEROLE` non-superuser and a superuser.

```sql
BEGIN;

DO $$
DECLARE
  granted_self boolean := false;
  needs_grant boolean := false;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'auditstream_owner') THEN
    EXECUTE 'CREATE ROLE auditstream_owner NOLOGIN';
  END IF;

  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'auditstream_app') THEN
    EXECUTE 'CREATE ROLE auditstream_app NOLOGIN';
  END IF;

  needs_grant := NOT pg_catalog.pg_has_role(current_user, 'auditstream_owner', 'USAGE');
  IF NOT needs_grant AND pg_catalog.current_setting('server_version_num')::int >= 160000 THEN
    needs_grant := NOT pg_catalog.pg_has_role(current_user, 'auditstream_owner', 'SET');
  END IF;

  IF needs_grant THEN
    EXECUTE pg_catalog.format('GRANT auditstream_owner TO %I', current_user);
    granted_self := true;
  END IF;

  PERFORM pg_catalog.set_config(
    'auditstream._granted_self',
    CASE WHEN granted_self THEN '1' ELSE '0' END,
    true
  );
END
$$;

CREATE SCHEMA IF NOT EXISTS auditstream AUTHORIZATION auditstream_owner;

SET LOCAL ROLE auditstream_owner;

CREATE TABLE IF NOT EXISTS auditstream.changes (
  id bigint GENERATED ALWAYS AS IDENTITY,
  event_id uuid NOT NULL DEFAULT gen_random_uuid(),
  occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  xact_id xid8 NOT NULL DEFAULT pg_current_xact_id(),
  source text NOT NULL DEFAULT 'db',
  schema_name text,
  table_name text,
  table_oid oid,
  op text NOT NULL,
  record_id jsonb,
  before jsonb,
  after jsonb,
  changed text[],
  actor jsonb,
  context jsonb,
  actor_source text,
  db_user text NOT NULL DEFAULT session_user,
  schema_version smallint NOT NULL DEFAULT 1,
  PRIMARY KEY (id, occurred_at),
  CONSTRAINT changes_source_check CHECK (source IN ('db', 'app')),
  CONSTRAINT changes_actor_source_check CHECK (
    actor_source IS NULL OR actor_source IN ('set_config', 'comment', 'jwt', 'invalid')
  ),
  CONSTRAINT changes_schema_version_check CHECK (schema_version >= 1)
) PARTITION BY RANGE (occurred_at);

CREATE TABLE IF NOT EXISTS auditstream.changes_2026_11
  PARTITION OF auditstream.changes
  FOR VALUES FROM ('2026-11-01 00:00:00+00') TO ('2026-12-01 00:00:00+00');

CREATE TABLE IF NOT EXISTS auditstream.changes_2026_12
  PARTITION OF auditstream.changes
  FOR VALUES FROM ('2026-12-01 00:00:00+00') TO ('2027-01-01 00:00:00+00');

CREATE TABLE IF NOT EXISTS auditstream.changes_2027_01
  PARTITION OF auditstream.changes
  FOR VALUES FROM ('2027-01-01 00:00:00+00') TO ('2027-02-01 00:00:00+00');

CREATE TABLE IF NOT EXISTS auditstream.changes_2027_02
  PARTITION OF auditstream.changes
  FOR VALUES FROM ('2027-02-01 00:00:00+00') TO ('2027-03-01 00:00:00+00');

CREATE TABLE IF NOT EXISTS auditstream.changes_default
  PARTITION OF auditstream.changes DEFAULT;

SET LOCAL ROLE NONE;

CREATE INDEX IF NOT EXISTS changes_occurred_at_brin_idx
  ON auditstream.changes USING brin (occurred_at);

CREATE OR REPLACE FUNCTION auditstream.resolve_actor(
  OUT actor jsonb,
  OUT context jsonb,
  OUT actor_source text
)
LANGUAGE plpgsql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
  raw_setting text;
  q_tail text;
  q_len integer;
  prefix text;
  slash_rev integer;
  open_pos integer;
  comment_body text;
  key_pos integer;
  quote_pos integer;
  has_tag boolean := false;
  raw_b64 text;
  b64_len integer;
  decoded_bytes bytea;
  payload jsonb;
  v_source text;
  obo jsonb;
  ctx jsonb;
  claims jsonb;
BEGIN
  raw_setting := NULLIF(pg_catalog.current_setting('auditstream.actor', true), '');
  IF raw_setting IS NOT NULL THEN
    IF pg_catalog.octet_length(raw_setting) > 512 THEN
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
    END IF;
    BEGIN
      payload := raw_setting::jsonb;
    EXCEPTION WHEN others THEN
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
    END;
    v_source := 'set_config';
  ELSE
    q_tail := pg_catalog.rtrim(
      pg_catalog.right(pg_catalog.current_query(), 4096),
      E' \t\n\r\f\v;'
    );
    q_len := pg_catalog.length(q_tail);
    IF q_len >= 4
       AND pg_catalog.right(q_tail, 2) = '*/'
       AND pg_catalog.strpos(q_tail, 'auditstream=''') > 0 THEN
      prefix := pg_catalog.left(q_tail, q_len - 2);
      slash_rev := pg_catalog.strpos(pg_catalog.reverse(prefix), '/');
      IF slash_rev > 0 THEN
        open_pos := q_len - 1 - slash_rev;
        IF pg_catalog.substr(prefix, open_pos, 2) = '/*' THEN
          comment_body := pg_catalog.substr(prefix, open_pos + 2);
          IF pg_catalog.left(comment_body, 13) = 'auditstream=''' THEN
            quote_pos := pg_catalog.strpos(pg_catalog.substr(comment_body, 14), '''');
            IF quote_pos > 0 THEN
              raw_b64 := pg_catalog.substr(comment_body, 14, quote_pos - 1);
              has_tag := true;
            END IF;
          ELSE
            key_pos := pg_catalog.strpos(comment_body, ',auditstream=''');
            IF key_pos > 0 THEN
              quote_pos := pg_catalog.strpos(pg_catalog.substr(comment_body, key_pos + 14), '''');
              IF quote_pos > 0 THEN
                raw_b64 := pg_catalog.substr(comment_body, key_pos + 14, quote_pos - 1);
                has_tag := true;
              END IF;
            END IF;
          END IF;
        END IF;
      END IF;
    END IF;

    IF has_tag THEN
      b64_len := pg_catalog.length(raw_b64);
      IF b64_len < 1
         OR b64_len > 683
         OR b64_len % 4 = 1
         OR pg_catalog.ltrim(
              raw_b64,
              'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-'
            ) <> '' THEN
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
      END IF;
      BEGIN
        decoded_bytes := pg_catalog.decode(
          pg_catalog.translate(raw_b64, '-_', '+/')
            || pg_catalog.repeat('=', (4 - b64_len % 4) % 4),
          'base64'
        );
        IF pg_catalog.octet_length(decoded_bytes) > 512 THEN
          actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
        END IF;
        payload := pg_catalog.convert_from(decoded_bytes, 'UTF8')::jsonb;
      EXCEPTION WHEN others THEN
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
      END;
      v_source := 'comment';
    END IF;
  END IF;

  IF v_source IS NOT NULL THEN
    IF pg_catalog.jsonb_typeof(payload) IS DISTINCT FROM 'object'
       OR pg_catalog.jsonb_typeof(payload->'type') IS DISTINCT FROM 'string'
       OR payload->>'type' NOT IN ('user', 'api_key', 'ai_agent', 'service', 'system')
       OR pg_catalog.jsonb_typeof(payload->'id') IS DISTINCT FROM 'string'
       OR pg_catalog.length(payload->>'id') = 0 THEN
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
    END IF;
    IF pg_catalog.jsonb_exists(payload, 'tenant')
       AND (
         pg_catalog.jsonb_typeof(payload->'tenant') IS DISTINCT FROM 'string'
         OR pg_catalog.length(payload->>'tenant') = 0
       ) THEN
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
    END IF;
    IF pg_catalog.jsonb_exists(payload, 'onBehalfOf') THEN
      obo := payload->'onBehalfOf';
      IF pg_catalog.jsonb_typeof(obo) IS DISTINCT FROM 'object'
         OR pg_catalog.jsonb_typeof(obo->'type') IS DISTINCT FROM 'string'
         OR obo->>'type' NOT IN ('user', 'api_key', 'ai_agent', 'service', 'system')
         OR pg_catalog.jsonb_typeof(obo->'id') IS DISTINCT FROM 'string'
         OR pg_catalog.length(obo->>'id') = 0
         OR pg_catalog.jsonb_exists(obo, 'onBehalfOf')
         OR (
           pg_catalog.jsonb_exists(obo, 'tenant')
           AND (
             pg_catalog.jsonb_typeof(obo->'tenant') IS DISTINCT FROM 'string'
             OR pg_catalog.length(obo->>'tenant') = 0
           )
         ) THEN
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
      END IF;
    END IF;
    IF pg_catalog.jsonb_exists(payload, 'context') THEN
      ctx := payload->'context';
      IF pg_catalog.jsonb_typeof(ctx) IS DISTINCT FROM 'object'
         OR (
           pg_catalog.jsonb_exists(ctx, 'requestId')
           AND (pg_catalog.jsonb_typeof(ctx->'requestId') IS DISTINCT FROM 'string' OR pg_catalog.length(ctx->>'requestId') = 0)
         )
         OR (
           pg_catalog.jsonb_exists(ctx, 'source')
           AND (pg_catalog.jsonb_typeof(ctx->'source') IS DISTINCT FROM 'string' OR pg_catalog.length(ctx->>'source') = 0)
         )
         OR (
           pg_catalog.jsonb_exists(ctx, 'ip')
           AND (pg_catalog.jsonb_typeof(ctx->'ip') IS DISTINCT FROM 'string' OR pg_catalog.length(ctx->>'ip') = 0)
         )
         OR (
           pg_catalog.jsonb_exists(ctx, 'userAgent')
           AND (pg_catalog.jsonb_typeof(ctx->'userAgent') IS DISTINCT FROM 'string' OR pg_catalog.length(ctx->>'userAgent') = 0)
         ) THEN
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
      END IF;
      context := ctx;
      actor := payload - 'context';
    ELSE
      actor := payload;
    END IF;
    actor_source := v_source;
    RETURN;
  END IF;

  raw_setting := NULLIF(pg_catalog.current_setting('request.jwt.claims', true), '');
  IF raw_setting IS NOT NULL THEN
    BEGIN
      claims := raw_setting::jsonb;
    EXCEPTION WHEN others THEN
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
    END;
    IF pg_catalog.jsonb_typeof(claims) IS DISTINCT FROM 'object' THEN
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
    END IF;
    IF pg_catalog.jsonb_exists(claims, 'sub')
       AND pg_catalog.jsonb_typeof(claims->'sub') IS DISTINCT FROM 'null' THEN
      IF pg_catalog.jsonb_typeof(claims->'sub') IS DISTINCT FROM 'string'
         OR pg_catalog.length(claims->>'sub') = 0 THEN
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;
      END IF;
      actor := pg_catalog.jsonb_build_object('type', 'user', 'id', claims->>'sub');
      context := NULL;
      actor_source := 'jwt';
      RETURN;
    END IF;
  END IF;

  actor := NULL;
  context := NULL;
  actor_source := NULL;
END
$fn$;

ALTER FUNCTION auditstream.resolve_actor() OWNER TO auditstream_owner;

CREATE OR REPLACE FUNCTION auditstream.log_event(
  action text,
  target jsonb DEFAULT NULL,
  data jsonb DEFAULT NULL
)
RETURNS uuid
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
  resolved record;
  v_event_id uuid;
BEGIN
  IF action IS NULL
     OR pg_catalog.char_length(pg_catalog.btrim(action)) < 1
     OR pg_catalog.char_length(action) > 64
     OR pg_catalog.upper(pg_catalog.btrim(action)) IN ('INSERT', 'UPDATE', 'DELETE', 'TRUNCATE') THEN
    RAISE EXCEPTION 'auditstream.log_event: invalid action %', action
      USING ERRCODE = '22023';
  END IF;

  IF target IS NOT NULL
     AND pg_catalog.jsonb_typeof(target) IS DISTINCT FROM 'object' THEN
    RAISE EXCEPTION 'auditstream.log_event: target must be a JSON object or NULL'
      USING ERRCODE = '22023';
  END IF;

  IF data IS NOT NULL
     AND pg_catalog.jsonb_typeof(data) IS DISTINCT FROM 'object' THEN
    RAISE EXCEPTION 'auditstream.log_event: data must be a JSON object or NULL'
      USING ERRCODE = '22023';
  END IF;

  SELECT r.actor, r.context, r.actor_source
    INTO resolved
    FROM auditstream.resolve_actor() AS r;

  INSERT INTO auditstream.changes (
    source,
    op,
    record_id,
    after,
    actor,
    context,
    actor_source,
    db_user,
    schema_version
  )
  VALUES (
    'app',
    action,
    target,
    data,
    resolved.actor,
    resolved.context,
    resolved.actor_source,
    session_user,
    1
  )
  RETURNING event_id INTO v_event_id;

  RETURN v_event_id;
END
$fn$;

ALTER FUNCTION auditstream.log_event(text, jsonb, jsonb) OWNER TO auditstream_owner;

REVOKE ALL ON SCHEMA auditstream FROM PUBLIC;
REVOKE ALL ON auditstream.changes FROM PUBLIC;
REVOKE ALL ON FUNCTION auditstream.resolve_actor() FROM PUBLIC;
REVOKE ALL ON FUNCTION auditstream.log_event(text, jsonb, jsonb) FROM PUBLIC;

GRANT USAGE ON SCHEMA auditstream TO auditstream_app;
GRANT SELECT ON auditstream.changes TO auditstream_app;
GRANT EXECUTE ON FUNCTION auditstream.log_event(text, jsonb, jsonb) TO auditstream_app;

DO $$
BEGIN
  IF pg_catalog.current_setting('auditstream._granted_self', true) = '1' THEN
    EXECUTE pg_catalog.format('REVOKE auditstream_owner FROM %I', current_user);
    PERFORM pg_catalog.set_config('auditstream._granted_self', '', true);
  END IF;
END
$$;

COMMIT;
```
