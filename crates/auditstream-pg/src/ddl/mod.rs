//! History schema DDL generator matching `spec/history-table.md`.
//!
//! Generates an ordered, deterministic sequence of single SQL statements that create
//! `auditstream_owner`, the audit schema, the monthly-partitioned `changes` table,
//! runtime partitions (DEFAULT + current UTC month + next 3 months), indexes,
//! `log_event()`, grants for application roles, and cleanup of any temporary self-grant.
//!
//! No generated statement drops a role, schema, table, or owned object.

/// Default audit schema name from `spec/history-table.md`.
pub const DEFAULT_SCHEMA: &str = "auditstream";

/// NOLOGIN role owning all Auditstream database objects.
pub const OWNER_ROLE: &str = "auditstream_owner";

/// Fixed 64-bit transaction advisory lock key (`"audstrow"` in ASCII) serializing
/// `auditstream_owner` role creation across concurrent setup runs.
const ROLE_LOCK_KEY: i64 = 0x6175_6473_7472_6f77;

/// Configuration for [`generate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlOptions {
    /// Target schema name (defaults to [`DEFAULT_SCHEMA`], `"auditstream"`).
    pub schema: String,

    /// Application database roles granted `USAGE` on the schema, `SELECT` on `changes`,
    /// and `EXECUTE` on `log_event()`.
    pub app_roles: Vec<String>,
}

impl Default for DdlOptions {
    fn default() -> Self {
        Self {
            schema: DEFAULT_SCHEMA.to_owned(),
            app_roles: Vec::new(),
        }
    }
}

impl DdlOptions {
    /// Creates [`DdlOptions`] with the given schema name and application roles.
    pub fn new(schema: impl Into<String>, app_roles: Vec<String>) -> Self {
        Self {
            schema: schema.into(),
            app_roles,
        }
    }
}

/// Quotes a PostgreSQL identifier with double quotes, doubling embedded `"` characters.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quotes a PostgreSQL string literal with single quotes, doubling embedded `'` characters.
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Generates the ordered list of single SQL statements that initialize or idempotently
/// reconcile the Auditstream history schema.
///
/// Output is completely deterministic: monthly partitions (`changes_YYYY_MM` for the
/// current UTC month and the next three months, plus `changes_default`) are computed
/// inside PostgreSQL when the SQL executes rather than when `generate()` is called.
///
/// Callers should execute the returned statements sequentially on a single connection
/// (typically inside one transaction) so the temporary `auditstream._granted_self`
/// session flag set in statement 1 is visible to the final cleanup statement.
pub fn generate(options: &DdlOptions) -> Vec<String> {
    let schema_q = quote_ident(&options.schema);
    let schema_lit = quote_literal(&options.schema);
    let owner_q = quote_ident(OWNER_ROLE);
    let owner_lit = quote_literal(OWNER_ROLE);

    let mut stmts = Vec::with_capacity(14 + options.app_roles.len() * 3);

    stmts.push(role_setup_sql(&owner_q, &owner_lit));
    stmts.push(format!(
        "CREATE SCHEMA IF NOT EXISTS {schema_q} AUTHORIZATION {owner_q}"
    ));
    stmts.push(changes_table_sql(&schema_q));
    stmts.push(format!("ALTER TABLE {schema_q}.changes OWNER TO {owner_q}"));
    stmts.push(runtime_partitions_sql(&schema_lit, &owner_lit));

    // Named indexes: BRIN on occurred_at, B-tree on (xact_id, id) for export cursor reads
    // (PostgreSQL 15-19 has no BRIN operator class for xid8), and partial hash on record_id
    // per ADR-0008 (`docs/decisions/0008-history-lookup-index.md`).
    stmts.push(format!(
        "CREATE INDEX IF NOT EXISTS changes_occurred_at_brin_idx ON {schema_q}.changes USING brin (occurred_at)"
    ));
    stmts.push(format!(
        "CREATE INDEX IF NOT EXISTS changes_xact_id_idx ON {schema_q}.changes (xact_id, id)"
    ));
    stmts.push(format!(
        "CREATE INDEX IF NOT EXISTS changes_lookup_idx ON {schema_q}.changes USING hash (record_id) WHERE record_id IS NOT NULL"
    ));

    stmts.push(log_event_sql(&schema_q));
    stmts.push(format!(
        "ALTER FUNCTION {schema_q}.log_event(text, jsonb, jsonb) OWNER TO {owner_q}"
    ));

    stmts.push(format!("REVOKE ALL ON SCHEMA {schema_q} FROM PUBLIC"));
    stmts.push(format!("REVOKE ALL ON {schema_q}.changes FROM PUBLIC"));
    stmts.push(format!(
        "REVOKE ALL ON FUNCTION {schema_q}.log_event(text, jsonb, jsonb) FROM PUBLIC"
    ));

    for role in &options.app_roles {
        let role_q = quote_ident(role);
        stmts.push(format!("GRANT USAGE ON SCHEMA {schema_q} TO {role_q}"));
        stmts.push(format!("GRANT SELECT ON {schema_q}.changes TO {role_q}"));
        stmts.push(format!(
            "GRANT EXECUTE ON FUNCTION {schema_q}.log_event(text, jsonb, jsonb) TO {role_q}"
        ));
    }

    stmts.push(role_cleanup_sql(&owner_q));
    stmts
}

fn role_setup_sql(owner_q: &str, owner_lit: &str) -> String {
    format!(
        "DO $$\n\
DECLARE\n  \
  granted_self boolean := false;\n\
BEGIN\n  \
  PERFORM pg_catalog.pg_advisory_xact_lock({ROLE_LOCK_KEY});\n  \
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {owner_lit}) THEN\n    \
    EXECUTE 'CREATE ROLE {owner_q} NOLOGIN';\n  \
  END IF;\n  \
  IF NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'USAGE') THEN\n    \
    EXECUTE pg_catalog.format('GRANT {owner_q} TO %I', current_user);\n    \
    granted_self := true;\n  \
  END IF;\n  \
  PERFORM pg_catalog.set_config(\n    \
    'auditstream._granted_self',\n    \
    CASE WHEN granted_self THEN '1' ELSE '0' END,\n    \
    false\n  \
  );\n\
END\n\
$$"
    )
}

fn changes_table_sql(schema_q: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {schema_q}.changes (\n  \
  id bigint GENERATED ALWAYS AS IDENTITY,\n  \
  event_id uuid NOT NULL DEFAULT gen_random_uuid(),\n  \
  occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),\n  \
  xact_id xid8 NOT NULL DEFAULT pg_current_xact_id(),\n  \
  source text NOT NULL DEFAULT 'db',\n  \
  schema_name text,\n  \
  table_name text,\n  \
  table_oid oid,\n  \
  op text NOT NULL,\n  \
  record_id jsonb,\n  \
  before jsonb,\n  \
  after jsonb,\n  \
  changed text[],\n  \
  actor jsonb,\n  \
  context jsonb,\n  \
  actor_source text,\n  \
  db_user text NOT NULL DEFAULT session_user,\n  \
  schema_version smallint NOT NULL DEFAULT 1,\n  \
  PRIMARY KEY (id, occurred_at)\n\
) PARTITION BY RANGE (occurred_at)"
    )
}

fn runtime_partitions_sql(schema_lit: &str, owner_lit: &str) -> String {
    format!(
        "DO $$\n\
DECLARE\n  \
  base_month timestamp := pg_catalog.date_trunc('month', pg_catalog.now() AT TIME ZONE 'UTC');\n  \
  m int;\n  \
  month_start timestamp;\n  \
  month_end timestamp;\n  \
  part_name text;\n\
BEGIN\n  \
  EXECUTE pg_catalog.format(\n    \
    'CREATE TABLE IF NOT EXISTS %I.changes_default PARTITION OF %I.changes DEFAULT',\n    \
    {schema_lit},\n    \
    {schema_lit}\n  \
  );\n  \
  EXECUTE pg_catalog.format(\n    \
    'ALTER TABLE %I.changes_default OWNER TO %I',\n    \
    {schema_lit},\n    \
    {owner_lit}\n  \
  );\n  \
  FOR m IN 0..3 LOOP\n    \
    month_start := base_month + pg_catalog.make_interval(months => m);\n    \
    month_end := base_month + pg_catalog.make_interval(months => m + 1);\n    \
    part_name := 'changes_' || pg_catalog.to_char(month_start, 'YYYY_MM');\n    \
    EXECUTE pg_catalog.format(\n      \
      'CREATE TABLE IF NOT EXISTS %I.%I PARTITION OF %I.changes FOR VALUES FROM (%L) TO (%L)',\n      \
      {schema_lit},\n      \
      part_name,\n      \
      {schema_lit},\n      \
      pg_catalog.to_char(month_start, 'YYYY-MM-DD HH24:MI:SS') || '+00',\n      \
      pg_catalog.to_char(month_end, 'YYYY-MM-DD HH24:MI:SS') || '+00'\n    \
    );\n    \
    EXECUTE pg_catalog.format(\n      \
      'ALTER TABLE %I.%I OWNER TO %I',\n      \
      {schema_lit},\n      \
      part_name,\n      \
      {owner_lit}\n    \
    );\n  \
  END LOOP;\n\
END\n\
$$"
    )
}

fn log_event_sql(schema_q: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION {schema_q}.log_event(\n  \
  action text,\n  \
  target jsonb DEFAULT NULL,\n  \
  data jsonb DEFAULT NULL\n\
)\n\
RETURNS uuid\n\
LANGUAGE plpgsql\n\
SECURITY DEFINER\n\
SET search_path = pg_catalog, pg_temp\n\
AS $fn$\n\
DECLARE\n  \
  resolved record;\n  \
  v_event_id uuid;\n\
BEGIN\n  \
  IF action IS NULL\n     \
     OR pg_catalog.char_length(pg_catalog.btrim(action)) < 1\n     \
     OR pg_catalog.char_length(action) > 64\n     \
     OR pg_catalog.upper(action) IN ('INSERT', 'UPDATE', 'DELETE', 'TRUNCATE') THEN\n    \
    RAISE EXCEPTION 'auditstream.log_event: invalid action %', action\n      \
      USING ERRCODE = '22023';\n  \
  END IF;\n\n  \
  SELECT r.actor, r.context, r.actor_source\n    \
    INTO resolved\n    \
    FROM {schema_q}.resolve_actor() AS r;\n\n  \
  INSERT INTO {schema_q}.changes (\n    \
    source,\n    \
    op,\n    \
    record_id,\n    \
    after,\n    \
    actor,\n    \
    context,\n    \
    actor_source,\n    \
    db_user,\n    \
    schema_version\n  \
  )\n  \
  VALUES (\n    \
    'app',\n    \
    action,\n    \
    target,\n    \
    data,\n    \
    resolved.actor,\n    \
    resolved.context,\n    \
    resolved.actor_source,\n    \
    session_user,\n    \
    1\n  \
  )\n  \
  RETURNING event_id INTO v_event_id;\n\n  \
  RETURN v_event_id;\n\
END\n\
$fn$"
    )
}

fn role_cleanup_sql(owner_q: &str) -> String {
    format!(
        "DO $$\n\
BEGIN\n  \
  IF pg_catalog.current_setting('auditstream._granted_self', true) = '1' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE {owner_q} FROM %I', current_user);\n    \
    PERFORM pg_catalog.set_config('auditstream._granted_self', '', false);\n  \
  END IF;\n\
END\n\
$$"
    )
}
