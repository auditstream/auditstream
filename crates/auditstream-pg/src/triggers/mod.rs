//! Capture trigger and actor-resolution SQL generator matching `spec/history-table.md`
//! and `spec/transport.md`.
//!
//! Generates an ordered, deterministic sequence of single SQL statements that install or
//! idempotently replace:
//!
//! - `{schema}.resolve_actor()` — `SECURITY DEFINER`, owned by `auditstream_owner`,
//!   `SET search_path = pg_catalog, pg_temp`, resolving actors in spec precedence order
//!   (`set_config` > trailing comment > `request.jwt.claims.sub` > `NULL`) without ever
//!   aborting writes on malformed payloads.
//! - `{schema}.capture()` — `SECURITY DEFINER`, owned by `auditstream_owner`,
//!   `SET search_path = pg_catalog, pg_temp`, recording statement-level `INSERT` / `DELETE`
//!   (via transition tables, resolving the actor once per statement), row-level `UPDATE`
//!   (skipping no-op and excluded-column-only updates), and statement-level `TRUNCATE`.
//! - Per-table `CREATE OR REPLACE TRIGGER` definitions (`auditstream_insert`,
//!   `auditstream_update`, `auditstream_delete`, `auditstream_truncate`).
//!
//! All regexes avoid repetition counts above 255 (length bounds are checked in SQL).

use crate::ddl::{DEFAULT_SCHEMA, OWNER_ROLE, quote_ident, quote_literal};

/// Trigger specification for a single audited PostgreSQL table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableTriggerSpec {
    /// Table schema name (for example `"public"`).
    pub schema: String,

    /// Table name (for example `"invoice"`).
    pub table: String,

    /// Ordered primary key column names; empty when the table has no primary key
    /// (in which case `record_id` is stored as `NULL`).
    pub primary_key: Vec<String>,

    /// Column names excluded from `before`, `after`, and `changed`.
    pub excluded_columns: Vec<String>,

    /// Whether the audited table is a partitioned parent table (`relkind = 'p'`).
    ///
    /// When `true`, triggers pass the logical parent `(schema, table)` via `TG_ARGV` so
    /// row-level `AFTER UPDATE` triggers cloned onto child partitions still record the
    /// audited parent's `schema_name`, `table_name`, and `table_oid`.
    pub is_partitioned: bool,
}

impl TableTriggerSpec {
    /// Creates a trigger specification for a regular (non-partitioned) table.
    pub fn new(
        schema: impl Into<String>,
        table: impl Into<String>,
        primary_key: Vec<String>,
        excluded_columns: Vec<String>,
    ) -> Self {
        Self {
            schema: schema.into(),
            table: table.into(),
            primary_key,
            excluded_columns,
            is_partitioned: false,
        }
    }

    /// Marks this table specification as a partitioned parent table (`relkind = 'p'`).
    #[must_use]
    pub fn partitioned(mut self) -> Self {
        self.is_partitioned = true;
        self
    }
}

/// Configuration for [`generate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerOptions {
    /// Audit history schema name (defaults to [`DEFAULT_SCHEMA`], `"auditstream"`).
    pub audit_schema: String,

    /// Audited tables to attach capture triggers to.
    pub tables: Vec<TableTriggerSpec>,
}

impl Default for TriggerOptions {
    fn default() -> Self {
        Self {
            audit_schema: DEFAULT_SCHEMA.to_owned(),
            tables: Vec::new(),
        }
    }
}

impl TriggerOptions {
    /// Creates [`TriggerOptions`] targeting the given audit schema and audited tables.
    pub fn new(audit_schema: impl Into<String>, tables: Vec<TableTriggerSpec>) -> Self {
        Self {
            audit_schema: audit_schema.into(),
            tables,
        }
    }
}

/// Generates the ordered list of single SQL statements that install or idempotently update
/// `resolve_actor()`, `capture()`, and per-table capture triggers.
///
/// Output is completely deterministic and returns one statement per `Vec` element so
/// downstream migration writers (for example Drizzle's statement-breakpoint format) can
/// emit each statement cleanly. Re-running the returned statements succeeds and leaves
/// exactly one trigger per event (`CREATE OR REPLACE TRIGGER`).
///
/// Callers should execute the statements sequentially on a single connection so any
/// temporary `auditstream_owner` self-grant acquired for a non-superuser migrator is
/// cleanly revoked by the final statement.
pub fn generate(options: &TriggerOptions) -> Vec<String> {
    let schema_q = quote_ident(&options.audit_schema);
    let owner_q = quote_ident(OWNER_ROLE);
    let owner_lit = quote_literal(OWNER_ROLE);

    let mut stmts = Vec::with_capacity(8 + options.tables.len() * 4);

    stmts.push(role_grant_setup_sql(&owner_q, &owner_lit));
    stmts.push(resolve_actor_sql(&schema_q));
    stmts.push(format!(
        "ALTER FUNCTION {schema_q}.resolve_actor() OWNER TO {owner_q}"
    ));
    stmts.push(format!(
        "REVOKE ALL ON FUNCTION {schema_q}.resolve_actor() FROM PUBLIC"
    ));

    stmts.push(capture_function_sql(&schema_q));
    stmts.push(format!(
        "ALTER FUNCTION {schema_q}.capture() OWNER TO {owner_q}"
    ));
    stmts.push(format!(
        "REVOKE ALL ON FUNCTION {schema_q}.capture() FROM PUBLIC"
    ));

    for table in &options.tables {
        append_table_triggers(&mut stmts, &schema_q, table);
    }

    stmts.push(role_grant_cleanup_sql(&owner_q));
    stmts
}

fn append_table_triggers(stmts: &mut Vec<String>, schema_q: &str, table: &TableTriggerSpec) {
    let table_fq = format!(
        "{}.{}",
        quote_ident(&table.schema),
        quote_ident(&table.table)
    );
    let pk_arg = quote_literal(&encode_json_string_array(&table.primary_key));
    let ex_arg = quote_literal(&encode_json_string_array(&table.excluded_columns));
    let args = if table.is_partitioned {
        format!(
            "{pk_arg}, {ex_arg}, {}, {}",
            quote_literal(&table.schema),
            quote_literal(&table.table)
        )
    } else {
        format!("{pk_arg}, {ex_arg}")
    };

    stmts.push(format!(
        "CREATE OR REPLACE TRIGGER auditstream_insert\n  \
         AFTER INSERT ON {table_fq}\n  \
         REFERENCING NEW TABLE AS new_rows\n  \
         FOR EACH STATEMENT\n  \
         EXECUTE FUNCTION {schema_q}.capture({args})"
    ));

    stmts.push(format!(
        "CREATE OR REPLACE TRIGGER auditstream_update\n  \
         AFTER UPDATE ON {table_fq}\n  \
         FOR EACH ROW\n  \
         WHEN (OLD.* IS DISTINCT FROM NEW.*)\n  \
         EXECUTE FUNCTION {schema_q}.capture({args})"
    ));

    stmts.push(format!(
        "CREATE OR REPLACE TRIGGER auditstream_delete\n  \
         AFTER DELETE ON {table_fq}\n  \
         REFERENCING OLD TABLE AS old_rows\n  \
         FOR EACH STATEMENT\n  \
         EXECUTE FUNCTION {schema_q}.capture({args})"
    ));

    stmts.push(format!(
        "CREATE OR REPLACE TRIGGER auditstream_truncate\n  \
         AFTER TRUNCATE ON {table_fq}\n  \
         FOR EACH STATEMENT\n  \
         EXECUTE FUNCTION {schema_q}.capture({args})"
    ));
}

fn encode_json_string_array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in item.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

fn role_grant_setup_sql(owner_q: &str, owner_lit: &str) -> String {
    format!(
        "DO $$\n\
DECLARE\n  \
  granted_self boolean := false;\n\
BEGIN\n  \
  IF NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'USAGE') THEN\n    \
    EXECUTE pg_catalog.format('GRANT {owner_q} TO %I', current_user);\n    \
    granted_self := true;\n  \
  END IF;\n  \
  PERFORM pg_catalog.set_config(\n    \
    'auditstream._triggers_granted_self',\n    \
    CASE WHEN granted_self THEN '1' ELSE '0' END,\n    \
    false\n  \
  );\n\
END\n\
$$"
    )
}

fn role_grant_cleanup_sql(owner_q: &str) -> String {
    format!(
        "DO $$\n\
BEGIN\n  \
  IF pg_catalog.current_setting('auditstream._triggers_granted_self', true) = '1' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE {owner_q} FROM %I', current_user);\n    \
    PERFORM pg_catalog.set_config('auditstream._triggers_granted_self', '', false);\n  \
  END IF;\n\
END\n\
$$"
    )
}

#[allow(clippy::too_many_lines)]
fn resolve_actor_sql(schema_q: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION {schema_q}.resolve_actor(\n  \
  OUT actor jsonb,\n  \
  OUT context jsonb,\n  \
  OUT actor_source text\n\
)\n\
LANGUAGE plpgsql\n\
STABLE\n\
SECURITY DEFINER\n\
SET search_path = pg_catalog, pg_temp\n\
AS $fn$\n\
DECLARE\n  \
  raw_setting text;\n  \
  body text[];\n  \
  matched text[];\n  \
  raw_b64 text;\n  \
  decoded_bytes bytea;\n  \
  payload jsonb;\n  \
  claims jsonb;\n  \
  sub_val text;\n\
BEGIN\n  \
  raw_setting := NULLIF(pg_catalog.current_setting('auditstream.actor', true), '');\n  \
  IF raw_setting IS NOT NULL THEN\n    \
    IF pg_catalog.octet_length(raw_setting) > 512 THEN\n      \
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n    \
    END IF;\n    \
    BEGIN\n      \
      payload := raw_setting::jsonb;\n    \
    EXCEPTION WHEN others THEN\n      \
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n    \
    END;\n    \
    IF pg_catalog.jsonb_typeof(payload) IS DISTINCT FROM 'object'\n       \
       OR pg_catalog.jsonb_typeof(payload->'type') IS DISTINCT FROM 'string'\n       \
       OR pg_catalog.jsonb_typeof(payload->'id') IS DISTINCT FROM 'string'\n       \
       OR pg_catalog.length(payload->>'type') = 0\n       \
       OR pg_catalog.length(payload->>'id') = 0 THEN\n      \
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n    \
    END IF;\n    \
    IF pg_catalog.jsonb_typeof(payload->'context') = 'object' THEN\n      \
      context := payload->'context';\n    \
    END IF;\n    \
    actor := payload - 'context';\n    \
    actor_source := 'set_config';\n    \
    RETURN;\n  \
  END IF;\n\n  \
  body := pg_catalog.regexp_match(\n    \
    pg_catalog.right(pg_catalog.current_query(), 4096),\n    \
    '/\\*([^/]*)\\*/[\\s;]*$'\n  \
  );\n  \
  IF body IS NOT NULL THEN\n    \
    matched := pg_catalog.regexp_match(body[1], '(?:^|,)auditstream=''([^'']*)''');\n    \
    IF matched IS NOT NULL THEN\n      \
      raw_b64 := matched[1];\n      \
      IF pg_catalog.length(raw_b64) < 1\n         \
         OR pg_catalog.length(raw_b64) > 683\n         \
         OR pg_catalog.length(raw_b64) % 4 = 1\n         \
         OR raw_b64 !~ '^[A-Za-z0-9_-]+$' THEN\n        \
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n      \
      END IF;\n      \
      BEGIN\n        \
        decoded_bytes := pg_catalog.decode(\n          \
          pg_catalog.translate(raw_b64, '-_', '+/')\n            \
            || pg_catalog.repeat('=', (4 - pg_catalog.length(raw_b64) % 4) % 4),\n          \
          'base64'\n        \
        );\n        \
        IF pg_catalog.octet_length(decoded_bytes) > 512 THEN\n          \
          actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n        \
        END IF;\n        \
        payload := pg_catalog.convert_from(decoded_bytes, 'UTF8')::jsonb;\n      \
      EXCEPTION WHEN others THEN\n        \
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n      \
      END;\n      \
      IF pg_catalog.jsonb_typeof(payload) IS DISTINCT FROM 'object'\n         \
         OR pg_catalog.jsonb_typeof(payload->'type') IS DISTINCT FROM 'string'\n         \
         OR pg_catalog.jsonb_typeof(payload->'id') IS DISTINCT FROM 'string'\n         \
         OR pg_catalog.length(payload->>'type') = 0\n         \
         OR pg_catalog.length(payload->>'id') = 0 THEN\n        \
        actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n      \
      END IF;\n      \
      IF pg_catalog.jsonb_typeof(payload->'context') = 'object' THEN\n        \
        context := payload->'context';\n      \
      END IF;\n      \
      actor := payload - 'context';\n      \
      actor_source := 'comment';\n      \
      RETURN;\n    \
    END IF;\n  \
  END IF;\n\n  \
  raw_setting := NULLIF(pg_catalog.current_setting('request.jwt.claims', true), '');\n  \
  IF raw_setting IS NOT NULL THEN\n    \
    BEGIN\n      \
      claims := raw_setting::jsonb;\n    \
    EXCEPTION WHEN others THEN\n      \
      actor := NULL; context := NULL; actor_source := 'invalid'; RETURN;\n    \
    END;\n    \
    IF pg_catalog.jsonb_typeof(claims) = 'object'\n       \
       AND pg_catalog.jsonb_typeof(claims->'sub') = 'string' THEN\n      \
      sub_val := claims->>'sub';\n      \
      IF pg_catalog.length(sub_val) > 0 THEN\n        \
        actor := pg_catalog.jsonb_build_object('type', 'user', 'id', sub_val);\n        \
        context := NULL;\n        \
        actor_source := 'jwt';\n        \
        RETURN;\n      \
      END IF;\n    \
    END IF;\n  \
  END IF;\n\n  \
  actor := NULL;\n  \
  context := NULL;\n  \
  actor_source := NULL;\n\
END\n\
$fn$"
    )
}

#[allow(clippy::too_many_lines)]
fn capture_function_sql(schema_q: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION {schema_q}.capture()\n\
RETURNS trigger\n\
LANGUAGE plpgsql\n\
SECURITY DEFINER\n\
SET search_path = pg_catalog, pg_temp\n\
AS $fn$\n\
DECLARE\n  \
  resolved record;\n  \
  pk_cols text[] := '{{}}';\n  \
  ex_cols text[] := '{{}}';\n  \
  v_schema text;\n  \
  v_table text;\n  \
  v_relid oid;\n  \
  v_before jsonb;\n  \
  v_after jsonb;\n  \
  v_record_id jsonb;\n  \
  v_changed text[];\n\
BEGIN\n  \
  IF TG_NARGS >= 1 THEN\n    \
    pk_cols := COALESCE(\n      \
      ARRAY(SELECT pg_catalog.jsonb_array_elements_text(TG_ARGV[0]::jsonb)),\n      \
      '{{}}'::text[]\n    \
    );\n  \
  END IF;\n  \
  IF TG_NARGS >= 2 THEN\n    \
    ex_cols := COALESCE(\n      \
      ARRAY(SELECT pg_catalog.jsonb_array_elements_text(TG_ARGV[1]::jsonb)),\n      \
      '{{}}'::text[]\n    \
    );\n  \
  END IF;\n  \
  IF TG_NARGS >= 4 THEN\n    \
    v_schema := TG_ARGV[2];\n    \
    v_table := TG_ARGV[3];\n    \
    v_relid := pg_catalog.to_regclass(pg_catalog.format('%I.%I', v_schema, v_table));\n  \
  ELSE\n    \
    v_schema := TG_TABLE_SCHEMA;\n    \
    v_table := TG_TABLE_NAME;\n    \
    v_relid := TG_RELID;\n  \
  END IF;\n\n  \
  SELECT r.actor, r.context, r.actor_source\n    \
    INTO resolved\n    \
    FROM {schema_q}.resolve_actor() AS r;\n\n  \
  IF TG_OP = 'TRUNCATE' THEN\n    \
    INSERT INTO {schema_q}.changes (\n      \
      source, schema_name, table_name, table_oid, op,\n      \
      actor, context, actor_source, db_user, schema_version\n    \
    )\n    \
    VALUES (\n      \
      'db', v_schema, v_table, v_relid, 'TRUNCATE',\n      \
      resolved.actor, resolved.context, resolved.actor_source,\n      \
      session_user, 1\n    \
    );\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  IF TG_OP = 'INSERT' THEN\n    \
    INSERT INTO {schema_q}.changes (\n      \
      source, schema_name, table_name, table_oid, op,\n      \
      record_id, before, after, changed,\n      \
      actor, context, actor_source, db_user, schema_version\n    \
    )\n    \
    SELECT\n      \
      'db',\n      \
      v_schema,\n      \
      v_table,\n      \
      v_relid,\n      \
      'INSERT',\n      \
      CASE WHEN pg_catalog.cardinality(pk_cols) > 0 THEN (\n        \
        SELECT pg_catalog.jsonb_object_agg(k, s.row_after->k)\n          \
          FROM pg_catalog.unnest(pk_cols) AS k\n      \
      ) END,\n      \
      NULL,\n      \
      s.row_after,\n      \
      NULL,\n      \
      resolved.actor,\n      \
      resolved.context,\n      \
      resolved.actor_source,\n      \
      session_user,\n      \
      1\n    \
    FROM (\n      \
      SELECT pg_catalog.to_jsonb(r) - ex_cols AS row_after\n        \
        FROM new_rows AS r\n    \
    ) AS s;\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  IF TG_OP = 'DELETE' THEN\n    \
    INSERT INTO {schema_q}.changes (\n      \
      source, schema_name, table_name, table_oid, op,\n      \
      record_id, before, after, changed,\n      \
      actor, context, actor_source, db_user, schema_version\n    \
    )\n    \
    SELECT\n      \
      'db',\n      \
      v_schema,\n      \
      v_table,\n      \
      v_relid,\n      \
      'DELETE',\n      \
      CASE WHEN pg_catalog.cardinality(pk_cols) > 0 THEN (\n        \
        SELECT pg_catalog.jsonb_object_agg(k, s.row_before->k)\n          \
          FROM pg_catalog.unnest(pk_cols) AS k\n      \
      ) END,\n      \
      s.row_before,\n      \
      NULL,\n      \
      NULL,\n      \
      resolved.actor,\n      \
      resolved.context,\n      \
      resolved.actor_source,\n      \
      session_user,\n      \
      1\n    \
    FROM (\n      \
      SELECT pg_catalog.to_jsonb(r) - ex_cols AS row_before\n        \
        FROM old_rows AS r\n    \
    ) AS s;\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  IF TG_OP = 'UPDATE' THEN\n    \
    v_before := pg_catalog.to_jsonb(OLD) - ex_cols;\n    \
    v_after := pg_catalog.to_jsonb(NEW) - ex_cols;\n    \
    IF v_after = v_before THEN\n      \
      RETURN NULL;\n    \
    END IF;\n    \
    IF pg_catalog.cardinality(pk_cols) > 0 THEN\n      \
      SELECT pg_catalog.jsonb_object_agg(k, v_after->k)\n        \
        INTO v_record_id\n        \
        FROM pg_catalog.unnest(pk_cols) AS k;\n    \
    END IF;\n    \
    v_changed := ARRAY(\n      \
      SELECT e.k\n        \
        FROM pg_catalog.jsonb_each(v_after) AS e(k, v)\n       \
       WHERE v_before->e.k IS DISTINCT FROM e.v\n    \
    );\n    \
    INSERT INTO {schema_q}.changes (\n      \
      source, schema_name, table_name, table_oid, op,\n      \
      record_id, before, after, changed,\n      \
      actor, context, actor_source, db_user, schema_version\n    \
    )\n    \
    VALUES (\n      \
      'db',\n      \
      v_schema,\n      \
      v_table,\n      \
      v_relid,\n      \
      'UPDATE',\n      \
      v_record_id,\n      \
      v_before,\n      \
      v_after,\n      \
      v_changed,\n      \
      resolved.actor,\n      \
      resolved.context,\n      \
      resolved.actor_source,\n      \
      session_user,\n      \
      1\n    \
    );\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  RETURN NULL;\n\
END\n\
$fn$"
    )
}
