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

/// Transaction-local PostgreSQL setting proving setup statements run inside one transaction.
pub(crate) const TX_GUARD_SETTING: &str = "auditstream._migration_tx";

/// Marks the enclosing transaction for standalone [`generate`] and [`crate::triggers::generate`]
/// sequences so role self-grant setup rejects autocommit callers before mutating roles.
pub(crate) fn tx_begin_guard_sql() -> String {
    format!(
        "DO $$\n\
BEGIN\n  \
  PERFORM pg_catalog.set_config('{TX_GUARD_SETTING}', 'on', true);\n\
END\n\
$$"
    )
}

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

/// Structured parts of [`generate`] so migration composers can interleave trigger statements
/// before creating future monthly partitions and revoking the temporary self-grant.
pub(crate) struct GeneratedDdl {
    pub(crate) setup: Vec<String>,
    pub(crate) future_partitions: String,
    pub(crate) role_cleanup: String,
}

/// Generates the ordered list of single SQL statements that initialize or idempotently
/// reconcile the Auditstream history schema.
///
/// Output is completely deterministic: monthly partitions (`changes_YYYY_MM` for the
/// current UTC month and the next three months, plus `changes_default`) are computed
/// inside PostgreSQL when the SQL executes rather than when `generate()` is called.
///
/// Callers MUST execute the returned statements sequentially inside a single transaction
/// (`BEGIN; ... COMMIT;`) so the temporary `auditstream_owner` self-grant is atomic and never
/// outlives a failed setup run. Autocommit execution is rejected before any role or schema DDL
/// executes.
///
/// The second-to-last statement creates the next three monthly partitions. Creating a
/// partition locks `changes` until commit, so migrations run it after their trigger statements.
pub fn generate(options: &DdlOptions) -> Vec<String> {
    let parts = generate_parts(options);
    let mut stmts = Vec::with_capacity(parts.setup.len() + 3);
    stmts.push(tx_begin_guard_sql());
    stmts.extend(parts.setup);
    stmts.push(parts.future_partitions);
    stmts.push(parts.role_cleanup);
    stmts
}

pub(crate) fn generate_parts(options: &DdlOptions) -> GeneratedDdl {
    let schema_q = quote_ident(&options.schema);
    let schema_lit = quote_literal(&options.schema);
    let owner_q = quote_ident(OWNER_ROLE);
    let owner_lit = quote_literal(OWNER_ROLE);

    let mut setup = Vec::with_capacity(14 + options.app_roles.len() * 3);

    setup.push(role_setup_sql(&owner_q, &owner_lit));
    setup.push(format!(
        "CREATE SCHEMA IF NOT EXISTS {schema_q} AUTHORIZATION {owner_q}"
    ));
    setup.push(changes_table_sql(&schema_q, &owner_lit));
    setup.push(partitions_sql(&schema_lit, &owner_lit, 0, 0));

    // Named indexes: BRIN on occurred_at, B-tree on (xact_id, id) for export cursor reads
    // (PostgreSQL 15-19 has no BRIN operator class for xid8), and partial hash on record_id
    // per ADR-0008 (`docs/decisions/0008-history-lookup-index.md`).
    setup.push(history_index_sql(
        &schema_q,
        &schema_lit,
        "changes_occurred_at_brin_idx",
        "USING brin (occurred_at)",
    ));
    setup.push(history_index_sql(
        &schema_q,
        &schema_lit,
        "changes_xact_id_idx",
        "(xact_id, id)",
    ));
    setup.push(history_index_sql(
        &schema_q,
        &schema_lit,
        "changes_lookup_idx",
        "USING hash (record_id) WHERE record_id IS NOT NULL",
    ));

    setup.push(log_event_sql(&schema_q));
    setup.push(function_owner_sql(
        &schema_q,
        "log_event(text, jsonb, jsonb)",
    ));

    setup.push(format!("REVOKE ALL ON SCHEMA {schema_q} FROM PUBLIC"));
    setup.push(format!("REVOKE ALL ON {schema_q}.changes FROM PUBLIC"));
    setup.push(format!(
        "REVOKE ALL ON FUNCTION {schema_q}.log_event(text, jsonb, jsonb) FROM PUBLIC"
    ));

    for role in &options.app_roles {
        let role_q = quote_ident(role);
        setup.push(format!("GRANT USAGE ON SCHEMA {schema_q} TO {role_q}"));
        setup.push(format!("GRANT SELECT ON {schema_q}.changes TO {role_q}"));
        setup.push(format!(
            "GRANT EXECUTE ON FUNCTION {schema_q}.log_event(text, jsonb, jsonb) TO {role_q}"
        ));
    }

    GeneratedDdl {
        setup,
        future_partitions: partitions_sql(&schema_lit, &owner_lit, 1, 3),
        role_cleanup: role_cleanup_sql(&owner_q),
    }
}

fn role_setup_sql(owner_q: &str, owner_lit: &str) -> String {
    format!(
        "DO $$\n\
DECLARE\n  \
  granted_self boolean := false;\n  \
  needs_grant boolean := false;\n\
BEGIN\n  \
  IF pg_catalog.current_setting('{TX_GUARD_SETTING}', true) IS DISTINCT FROM 'on' THEN\n    \
    RAISE EXCEPTION 'Auditstream DDL statements must run inside a single transaction'\n      \
      USING HINT = 'Wrap the generated statements in BEGIN; ... COMMIT; or use a transactional migrator.';\n  \
  END IF;\n  \
  PERFORM pg_catalog.pg_advisory_xact_lock({ROLE_LOCK_KEY});\n  \
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {owner_lit}) THEN\n    \
    EXECUTE 'CREATE ROLE {owner_q} NOLOGIN';\n  \
  END IF;\n  \
  needs_grant := NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'USAGE');\n  \
  IF NOT needs_grant AND pg_catalog.current_setting('server_version_num')::int >= 160000 THEN\n    \
    needs_grant := NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'SET');\n  \
  END IF;\n  \
  IF needs_grant THEN\n    \
    EXECUTE pg_catalog.format('GRANT {owner_q} TO %I', current_user);\n    \
    granted_self := true;\n  \
  END IF;\n  \
  PERFORM pg_catalog.set_config(\n    \
    'auditstream._granted_self',\n    \
    CASE WHEN granted_self THEN '1' ELSE '0' END,\n    \
    true\n  \
  );\n\
END\n\
$$"
    )
}

fn changes_table_sql(schema_q: &str, owner_lit: &str) -> String {
    do_block(&format!(
        "DECLARE\n  \
  prev_role name := current_user;\n\
BEGIN\n  \
  EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});\n  \
  CREATE TABLE IF NOT EXISTS {schema_q}.changes (\n    \
    id bigint GENERATED ALWAYS AS IDENTITY,\n    \
    event_id uuid NOT NULL DEFAULT gen_random_uuid(),\n    \
    occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),\n    \
    xact_id xid8 NOT NULL DEFAULT pg_current_xact_id(),\n    \
    source text NOT NULL DEFAULT 'db',\n    \
    schema_name text,\n    \
    table_name text,\n    \
    table_oid oid,\n    \
    op text NOT NULL,\n    \
    record_id jsonb,\n    \
    before jsonb,\n    \
    after jsonb,\n    \
    changed text[],\n    \
    actor jsonb,\n    \
    context jsonb,\n    \
    actor_source text,\n    \
    db_user text NOT NULL DEFAULT session_user,\n    \
    schema_version smallint NOT NULL DEFAULT 1,\n    \
    PRIMARY KEY (id, occurred_at),\n    \
    CONSTRAINT changes_source_check CHECK (source IN ('db', 'app')),\n    \
    CONSTRAINT changes_actor_source_check CHECK (\n      \
      actor_source IS NULL OR actor_source IN ('set_config', 'comment', 'jwt', 'invalid')\n    \
    ),\n    \
    CONSTRAINT changes_schema_version_check CHECK (schema_version >= 1)\n  \
  ) PARTITION BY RANGE (occurred_at);\n  \
  SET LOCAL ROLE NONE;\n  \
  IF prev_role <> current_user THEN\n    \
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', prev_role);\n  \
  END IF;\n\
END"
    ))
}

/// Creates the missing monthly partitions `first_month..=last_month` months after the current
/// UTC month under `auditstream_owner` (preventing `ALTER DEFAULT PRIVILEGES` for the migrator
/// role from granting table writes on child partitions), reconciles ownership if needed, and
/// strips any leaked non-owner write/public grants on managed tables. Month 0 also covers
/// `changes` and its DEFAULT partition, which captured writes need as soon as the triggers exist.
fn partitions_sql(schema_lit: &str, owner_lit: &str, first_month: u32, last_month: u32) -> String {
    let (default_partition, managed_tables) = if first_month == 0 {
        let create = format!(
            "  EXECUTE pg_catalog.format(
    'CREATE TABLE IF NOT EXISTS %I.changes_default PARTITION OF %I.changes DEFAULT',
    {schema_lit},
    {schema_lit}
  );
"
        );
        (create, "ARRAY['changes', 'changes_default']")
    } else {
        (String::new(), "ARRAY[]::name[]")
    };
    do_block(&format!(
        "DECLARE
  prev_role name := current_user;
  base_month timestamp := pg_catalog.date_trunc('month', pg_catalog.now() AT TIME ZONE 'UTC');
  m int;
  month_start timestamp;
  month_end timestamp;
  part_name name;
  grantee_oid oid;
  managed_tables name[] := {managed_tables};
BEGIN
  EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});
{default_partition}  FOR m IN {first_month}..{last_month} LOOP
    month_start := base_month + pg_catalog.make_interval(months => m);
    month_end := base_month + pg_catalog.make_interval(months => m + 1);
    part_name := 'changes_' || pg_catalog.to_char(month_start, 'YYYY_MM');
    EXECUTE pg_catalog.format(
      'CREATE TABLE IF NOT EXISTS %I.%I PARTITION OF %I.changes FOR VALUES FROM (%L) TO (%L)',
      {schema_lit},
      part_name,
      {schema_lit},
      pg_catalog.to_char(month_start, 'YYYY-MM-DD HH24:MI:SS') || '+00',
      pg_catalog.to_char(month_end, 'YYYY-MM-DD HH24:MI:SS') || '+00'
    );
    managed_tables := pg_catalog.array_append(managed_tables, part_name);
  END LOOP;
  SET LOCAL ROLE NONE;
  IF prev_role <> current_user THEN
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', prev_role);
  END IF;
  FOR part_name IN
    SELECT c.relname FROM pg_catalog.pg_class c
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = {schema_lit} AND c.relname = ANY (managed_tables)
      AND c.relowner <> {owner_lit}::pg_catalog.regrole
    ORDER BY c.relname
  LOOP
    EXECUTE pg_catalog.format('ALTER TABLE %I.%I OWNER TO %I', {schema_lit}, part_name, {owner_lit});
  END LOOP;
  FOR part_name, grantee_oid IN
    SELECT DISTINCT c.relname, a.grantee
    FROM pg_catalog.pg_class c
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS a
    WHERE n.nspname = {schema_lit}
      AND c.relname = ANY (managed_tables)
      AND c.relacl IS NOT NULL
      AND a.grantee <> c.relowner
      AND (a.grantee = 0 OR a.privilege_type <> 'SELECT')
    ORDER BY c.relname, a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON TABLE %I.%I FROM PUBLIC', {schema_lit}, part_name);
    ELSIF part_name = 'changes' THEN
      EXECUTE pg_catalog.format(
        'REVOKE INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON TABLE %I.%I FROM %I',
        {schema_lit},
        part_name,
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
      IF pg_catalog.current_setting('server_version_num')::int >= 170000 THEN
        EXECUTE pg_catalog.format(
          'REVOKE MAINTAIN ON TABLE %I.%I FROM %I',
          {schema_lit},
          part_name,
          pg_catalog.pg_get_userbyid(grantee_oid)
        );
      END IF;
    ELSE
      EXECUTE pg_catalog.format(
        'REVOKE ALL ON TABLE %I.%I FROM %I',
        {schema_lit},
        part_name,
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
    END IF;
  END LOOP;
END"
    ))
}

/// Creates a history index unless one with this name exists. `CREATE INDEX IF NOT EXISTS`
/// takes a SHARE lock on `changes` and every partition before it checks the name, and a
/// migration holds that lock until it commits, blocking every audited write meanwhile.
fn history_index_sql(schema_q: &str, schema_lit: &str, name: &str, definition: &str) -> String {
    do_block(&format!(
        "BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_class c
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = {schema_lit} AND c.relname = '{name}') THEN
    CREATE INDEX IF NOT EXISTS {name} ON {schema_q}.changes {definition};
  END IF;
END"
    ))
}

pub(crate) fn function_owner_sql(schema_q: &str, signature: &str) -> String {
    let function = format!("{schema_q}.{signature}");
    let function_lit = quote_literal(&function);
    let owner_q = quote_ident(OWNER_ROLE);
    let owner_lit = quote_literal(OWNER_ROLE);
    do_block(&format!(
        "BEGIN
  IF EXISTS (SELECT 1 FROM pg_catalog.pg_proc
             WHERE oid = {function_lit}::pg_catalog.regprocedure
               AND proowner <> {owner_lit}::pg_catalog.regrole) THEN
    ALTER FUNCTION {function} OWNER TO {owner_q};
  END IF;
END"
    ))
}

fn do_block(body: &str) -> String {
    // A quoted schema identifier can itself contain the normal dollar delimiter.
    if body.contains("$$") {
        format!("DO {}", quote_literal(body))
    } else {
        format!("DO $$\n{body}\n$$")
    }
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
     OR pg_catalog.upper(pg_catalog.btrim(action)) IN ('INSERT', 'UPDATE', 'DELETE', 'TRUNCATE') THEN\n    \
    RAISE EXCEPTION 'auditstream.log_event: invalid action %', action\n      \
      USING ERRCODE = '22023';\n  \
  END IF;\n\n  \
  IF target IS NOT NULL\n     \
     AND pg_catalog.jsonb_typeof(target) IS DISTINCT FROM 'object' THEN\n    \
    RAISE EXCEPTION 'auditstream.log_event: target must be a JSON object or NULL'\n      \
      USING ERRCODE = '22023';\n  \
  END IF;\n\n  \
  IF data IS NOT NULL\n     \
     AND pg_catalog.jsonb_typeof(data) IS DISTINCT FROM 'object' THEN\n    \
    RAISE EXCEPTION 'auditstream.log_event: data must be a JSON object or NULL'\n      \
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
    PERFORM pg_catalog.set_config('auditstream._granted_self', '', true);\n  \
  END IF;\n\
END\n\
$$"
    )
}
