//! History schema DDL generator matching `spec/history-table.md`.
//!
//! Generates an ordered, deterministic sequence of single SQL statements that create
//! `auditstream_owner`, the audit schema, the monthly-partitioned `changes` table,
//! runtime partitions (DEFAULT + current UTC month + next 3 months), indexes,
//! `log_event()`, strict ACL sanitization across tables, partitions, identity sequences,
//! schema, and functions, grants for application roles, and cleanup of any temporary self-grant.
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

/// Transaction-local PostgreSQL setting storing the caller's role before entering `auditstream_owner`.
const PREV_ROLE_SETTING: &str = "auditstream._prev_role";

/// Selects a dollar-quote tag for `CREATE OR REPLACE FUNCTION` bodies that does not appear inside
/// `avoid` (such as a quoted custom schema identifier containing `$fn$`).
pub(crate) fn dollar_tag(avoid: &str) -> &'static str {
    for candidate in [
        "$fn$",
        "$auditfn$",
        "$auditstream_fn$",
        "$asfn$",
        "$auditstream_body$",
    ] {
        if !avoid.contains(candidate) {
            return candidate;
        }
    }
    "$auditstream_fn_body$"
}

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

/// Switches `current_user` to `auditstream_owner` for subsequent top-level DDL statements
/// within the enclosing transaction, saving the previous role in a transaction-local setting.
pub(crate) fn enter_owner_role_sql(owner_lit: &str) -> String {
    format!(
        "DO $$\n\
BEGIN\n  \
  PERFORM pg_catalog.set_config('{PREV_ROLE_SETTING}', current_user, true);\n  \
  EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});\n\
END\n\
$$"
    )
}

/// Restores the role saved by [`enter_owner_role_sql`] without clobbering any prior session-level
/// `SET ROLE`.
pub(crate) fn restore_prev_role_sql() -> String {
    format!(
        "DO $$\n\
DECLARE\n  \
  prev_role text := pg_catalog.current_setting('{PREV_ROLE_SETTING}', true);\n\
BEGIN\n  \
  SET LOCAL ROLE NONE;\n  \
  IF prev_role IS NOT NULL AND prev_role <> '' AND prev_role <> current_user THEN\n    \
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', prev_role);\n  \
  END IF;\n  \
  PERFORM pg_catalog.set_config('{PREV_ROLE_SETTING}', '', true);\n\
END\n\
$$"
    )
}

/// Transaction-scoped role elevation setup shared by [`generate`] and [`crate::triggers::generate`].
///
/// Rejects autocommit callers via [`TX_GUARD_SETTING`], creates `auditstream_owner` when
/// `create_role` is `true`, and records the exact temporary grant mode (`none`, `member`,
/// `inherit`, `set`, or `both`) in a transaction-local GUC so cleanup revokes only what this
/// transaction granted without disturbing pre-existing memberships or failing on `NOINHERIT` roles.
pub(crate) fn role_grant_setup_sql(
    owner_q: &str,
    owner_lit: &str,
    setting_name: &str,
    context_label: &str,
    create_role: bool,
) -> String {
    let create_clause = if create_role {
        format!(
            "  PERFORM pg_catalog.pg_advisory_xact_lock({ROLE_LOCK_KEY});\n  \
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {owner_lit}) THEN\n    \
    EXECUTE 'CREATE ROLE {owner_q} NOLOGIN';\n  \
  END IF;\n"
        )
    } else {
        String::new()
    };

    format!(
        "DO $$\n\
DECLARE\n  \
  grant_mode text := 'none';\n  \
  need_inherit boolean := false;\n  \
  need_set boolean := false;\n\
BEGIN\n  \
  IF pg_catalog.current_setting('{TX_GUARD_SETTING}', true) IS DISTINCT FROM 'on' THEN\n    \
    RAISE EXCEPTION '{context_label} must run inside a single transaction'\n      \
      USING HINT = 'Wrap the generated statements in BEGIN; ... COMMIT; or use a transactional migrator.';\n  \
  END IF;\n\
{create_clause}  \
  IF NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'MEMBER') THEN\n    \
    IF pg_catalog.current_setting('server_version_num')::int >= 160000 THEN\n      \
      EXECUTE pg_catalog.format('GRANT {owner_q} TO %I WITH INHERIT TRUE, SET TRUE', current_user);\n    \
    ELSE\n      \
      EXECUTE pg_catalog.format('GRANT {owner_q} TO %I', current_user);\n    \
    END IF;\n    \
    grant_mode := 'member';\n  \
  ELSIF pg_catalog.current_setting('server_version_num')::int >= 160000 THEN\n    \
    need_inherit := NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'USAGE');\n    \
    need_set := NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'SET');\n    \
    IF need_inherit AND need_set THEN\n      \
      EXECUTE pg_catalog.format('GRANT {owner_q} TO %I WITH INHERIT TRUE, SET TRUE', current_user);\n      \
      grant_mode := 'both';\n    \
    ELSIF need_inherit THEN\n      \
      EXECUTE pg_catalog.format('GRANT {owner_q} TO %I WITH INHERIT TRUE', current_user);\n      \
      grant_mode := 'inherit';\n    \
    ELSIF need_set THEN\n      \
      EXECUTE pg_catalog.format('GRANT {owner_q} TO %I WITH SET TRUE', current_user);\n      \
      grant_mode := 'set';\n    \
    END IF;\n  \
  END IF;\n  \
  PERFORM pg_catalog.set_config('{setting_name}', grant_mode, true);\n\
END\n\
$$"
    )
}

/// Revokes only the temporary membership or role option acquired by [`role_grant_setup_sql`].
pub(crate) fn role_grant_cleanup_sql(owner_q: &str, setting_name: &str) -> String {
    format!(
        "DO $$\n\
DECLARE\n  \
  grant_mode text := pg_catalog.current_setting('{setting_name}', true);\n\
BEGIN\n  \
  IF grant_mode = 'member' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE {owner_q} FROM %I', current_user);\n  \
  ELSIF grant_mode = 'both' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE INHERIT OPTION FOR {owner_q} FROM %I', current_user);\n    \
    EXECUTE pg_catalog.format('REVOKE SET OPTION FOR {owner_q} FROM %I', current_user);\n  \
  ELSIF grant_mode = 'inherit' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE INHERIT OPTION FOR {owner_q} FROM %I', current_user);\n  \
  ELSIF grant_mode = 'set' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE SET OPTION FOR {owner_q} FROM %I', current_user);\n  \
  END IF;\n  \
  IF grant_mode IS NOT NULL AND grant_mode <> '' THEN\n    \
    PERFORM pg_catalog.set_config('{setting_name}', '', true);\n  \
  END IF;\n\
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

    let setup = vec![
        role_grant_setup_sql(
            &owner_q,
            &owner_lit,
            "auditstream._granted_self",
            "Auditstream DDL statements",
            true,
        ),
        format!("CREATE SCHEMA IF NOT EXISTS {schema_q} AUTHORIZATION {owner_q}"),
        reconcile_ddl_ownership_sql(&schema_lit, &owner_lit),
        enter_owner_role_sql(&owner_lit),
        changes_table_sql(&schema_q),
        partitions_sql(&schema_lit, &owner_lit, 0, 0),
        // Named indexes: BRIN on occurred_at, B-tree on (xact_id, id) for export cursor reads
        // (PostgreSQL 15-19 has no BRIN operator class for xid8), and partial hash on record_id
        // per ADR-0008 (`docs/decisions/0008-history-lookup-index.md`).
        history_index_sql(
            &schema_q,
            &schema_lit,
            "changes_occurred_at_brin_idx",
            "USING brin (occurred_at)",
        ),
        history_index_sql(
            &schema_q,
            &schema_lit,
            "changes_xact_id_idx",
            "(xact_id, id)",
        ),
        history_index_sql(
            &schema_q,
            &schema_lit,
            "changes_lookup_idx",
            "USING hash (record_id) WHERE record_id IS NOT NULL",
        ),
        log_event_sql(&schema_q),
        harden_ddl_acls_sql(&schema_lit, &options.app_roles),
        restore_prev_role_sql(),
    ];

    GeneratedDdl {
        setup,
        future_partitions: partitions_sql(&schema_lit, &owner_lit, 1, 3),
        role_cleanup: role_grant_cleanup_sql(&owner_q, "auditstream._granted_self"),
    }
}

/// Reconciles pre-existing ownership of the audit schema, `changes`, all attached child
/// partitions, identity sequences, and `log_event()` as the caller role *before* switching to
/// `SET LOCAL ROLE auditstream_owner`.
fn reconcile_ddl_ownership_sql(schema_lit: &str, owner_lit: &str) -> String {
    do_block(&format!(
        "DECLARE
  obj_schema name;
  obj_name name;
  fn_oid oid;
BEGIN
  IF EXISTS (
    SELECT 1 FROM pg_catalog.pg_namespace
     WHERE nspname = {schema_lit}
       AND nspowner <> {owner_lit}::pg_catalog.regrole
  ) THEN
    EXECUTE pg_catalog.format('ALTER SCHEMA %I OWNER TO %I', {schema_lit}, {owner_lit});
  END IF;

  FOR obj_schema, obj_name IN
    WITH RECURSIVE tree(relid) AS (
      SELECT c.oid
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
       WHERE n.nspname = {schema_lit}
         AND c.relname = 'changes'
         AND c.relkind IN ('r', 'p')
      UNION ALL
      SELECT i.inhrelid
        FROM tree
        JOIN pg_catalog.pg_inherits i ON i.inhparent = tree.relid
    )
    SELECT cn.nspname, c.relname
      FROM tree
      JOIN pg_catalog.pg_class c ON c.oid = tree.relid
      JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace
     WHERE c.relowner <> {owner_lit}::pg_catalog.regrole
     ORDER BY cn.nspname, c.relname
  LOOP
    EXECUTE pg_catalog.format('ALTER TABLE %I.%I OWNER TO %I', obj_schema, obj_name, {owner_lit});
  END LOOP;

  FOR obj_schema, obj_name IN
    SELECT sn.nspname, s.relname
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_catalog.pg_depend d ON d.refobjid = c.oid AND d.deptype = 'i'
      JOIN pg_catalog.pg_class s ON s.oid = d.objid AND s.relkind = 'S'
      JOIN pg_catalog.pg_namespace sn ON sn.oid = s.relnamespace
     WHERE n.nspname = {schema_lit}
       AND c.relname = 'changes'
       AND s.relowner <> {owner_lit}::pg_catalog.regrole
     ORDER BY sn.nspname, s.relname
  LOOP
    EXECUTE pg_catalog.format('ALTER SEQUENCE %I.%I OWNER TO %I', obj_schema, obj_name, {owner_lit});
  END LOOP;

  SELECT p.oid INTO fn_oid
    FROM pg_catalog.pg_proc p
    JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = {schema_lit}
     AND p.proname = 'log_event'
     AND p.pronargs = 3
     AND pg_catalog.pg_get_function_identity_arguments(p.oid) = 'action text, target jsonb, data jsonb';
  IF fn_oid IS NOT NULL AND EXISTS (
    SELECT 1 FROM pg_catalog.pg_proc
     WHERE oid = fn_oid AND proowner <> {owner_lit}::pg_catalog.regrole
  ) THEN
    EXECUTE pg_catalog.format('ALTER FUNCTION %I.log_event(text, jsonb, jsonb) OWNER TO %I', {schema_lit}, {owner_lit});
  END IF;
END"
    ))
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
  PRIMARY KEY (id, occurred_at),\n  \
  CONSTRAINT changes_source_check CHECK (source IN ('db', 'app')),\n  \
  CONSTRAINT changes_actor_source_check CHECK (\n    \
    actor_source IS NULL OR actor_source IN ('set_config', 'comment', 'jwt', 'invalid')\n  \
  ),\n  \
  CONSTRAINT changes_schema_version_check CHECK (schema_version >= 1)\n\
) PARTITION BY RANGE (occurred_at)"
    )
}

/// Creates the missing monthly partitions `first_month..=last_month` months after the current
/// UTC month under `auditstream_owner` (preventing `ALTER DEFAULT PRIVILEGES` for the migrator
/// role from granting table writes on child partitions) and strips any leaked non-owner grants
/// while still running as `auditstream_owner` (safe for `NOINHERIT` migrator roles).
///
/// Month 0 also creates `changes_default`, strips leaked non-owner privileges across **all**
/// attached child partitions (including historical partitions from prior upgrades), revokes
/// dangerous/grantable/PUBLIC privileges on parent `changes`, and strips all non-owner grants on
/// the implicit `changes_id_seq` identity sequence.
#[allow(clippy::too_many_lines)]
fn partitions_sql(schema_lit: &str, owner_lit: &str, first_month: u32, last_month: u32) -> String {
    let default_partition = if first_month == 0 {
        format!(
            "  EXECUTE pg_catalog.format(
    'CREATE TABLE IF NOT EXISTS %I.changes_default PARTITION OF %I.changes DEFAULT',
    {schema_lit},
    {schema_lit}
  );
"
        )
    } else {
        String::new()
    };

    let acl_cleanup = if first_month == 0 {
        format!(
            "  FOR obj_schema, part_name, grantee_oid IN
    WITH RECURSIVE tree(relid) AS (
      SELECT i.inhrelid
        FROM pg_catalog.pg_inherits i
        JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent
        JOIN pg_catalog.pg_namespace pn ON pn.oid = parent.relnamespace
       WHERE pn.nspname = {schema_lit}
         AND parent.relname = 'changes'
      UNION ALL
      SELECT i.inhrelid
        FROM tree
        JOIN pg_catalog.pg_inherits i ON i.inhparent = tree.relid
    )
    SELECT DISTINCT cn.nspname, c.relname, a.grantee
      FROM tree
      JOIN pg_catalog.pg_class c ON c.oid = tree.relid
      JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
      ) AS a
     WHERE a.grantee <> c.relowner
     ORDER BY cn.nspname, c.relname, a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON TABLE %I.%I FROM PUBLIC CASCADE', obj_schema, part_name);
    ELSE
      EXECUTE pg_catalog.format(
        'REVOKE ALL ON TABLE %I.%I FROM %I CASCADE',
        obj_schema,
        part_name,
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
    END IF;
  END LOOP;

  FOR grantee_oid IN
    SELECT DISTINCT a.grantee
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
      ) AS a
     WHERE n.nspname = {schema_lit}
       AND c.relname = 'changes'
       AND a.grantee <> c.relowner
       AND (a.grantee = 0 OR a.privilege_type <> 'SELECT' OR a.is_grantable)
     ORDER BY a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON TABLE %I.changes FROM PUBLIC CASCADE', {schema_lit});
    ELSE
      EXECUTE pg_catalog.format(
        'REVOKE INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON TABLE %I.changes FROM %I CASCADE',
        {schema_lit},
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
      EXECUTE pg_catalog.format(
        'REVOKE GRANT OPTION FOR SELECT ON TABLE %I.changes FROM %I CASCADE',
        {schema_lit},
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
      IF pg_catalog.current_setting('server_version_num')::int >= 170000 THEN
        EXECUTE pg_catalog.format(
          'REVOKE MAINTAIN ON TABLE %I.changes FROM %I CASCADE',
          {schema_lit},
          pg_catalog.pg_get_userbyid(grantee_oid)
        );
      END IF;
    END IF;
  END LOOP;

  FOR obj_schema, part_name, grantee_oid IN
    SELECT DISTINCT sn.nspname, s.relname, a.grantee
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_catalog.pg_depend d ON d.refobjid = c.oid AND d.deptype = 'i'
      JOIN pg_catalog.pg_class s ON s.oid = d.objid AND s.relkind = 'S'
      JOIN pg_catalog.pg_namespace sn ON sn.oid = s.relnamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(s.relacl, pg_catalog.acldefault('s', s.relowner))
      ) AS a
     WHERE n.nspname = {schema_lit}
       AND c.relname = 'changes'
       AND a.grantee <> s.relowner
     ORDER BY sn.nspname, s.relname, a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON SEQUENCE %I.%I FROM PUBLIC CASCADE', obj_schema, part_name);
    ELSE
      EXECUTE pg_catalog.format(
        'REVOKE ALL ON SEQUENCE %I.%I FROM %I CASCADE',
        obj_schema,
        part_name,
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
    END IF;
  END LOOP;"
        )
    } else {
        format!(
            "  FOR part_name, grantee_oid IN
    SELECT DISTINCT c.relname, a.grantee
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
      ) AS a
     WHERE n.nspname = {schema_lit}
       AND c.relname = ANY (managed_tables)
       AND a.grantee <> c.relowner
     ORDER BY c.relname, a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON TABLE %I.%I FROM PUBLIC CASCADE', {schema_lit}, part_name);
    ELSE
      EXECUTE pg_catalog.format(
        'REVOKE ALL ON TABLE %I.%I FROM %I CASCADE',
        {schema_lit},
        part_name,
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
    END IF;
  END LOOP;"
        )
    };

    do_block(&format!(
        "DECLARE
  prev_role name := current_user;
  switched_role boolean := false;
  base_month timestamp := pg_catalog.date_trunc('month', pg_catalog.now() AT TIME ZONE 'UTC');
  m int;
  month_start timestamp;
  month_end timestamp;
  obj_schema name;
  part_name name;
  grantee_oid oid;
  managed_tables name[] := ARRAY[]::name[];
BEGIN
  IF prev_role <> {owner_lit} THEN
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});
    switched_role := true;
  END IF;
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
{acl_cleanup}
  IF switched_role THEN
    SET LOCAL ROLE NONE;
    IF prev_role <> current_user THEN
      EXECUTE pg_catalog.format('SET LOCAL ROLE %I', prev_role);
    END IF;
  END IF;
END"
    ))
}

/// Sanitizes schema `{schema}`, parent `{schema}.changes`, and `{schema}.log_event()` ACLs under
/// `auditstream_owner` without taking locks on clean refreshes, and applies explicit grants for
/// `app_roles` when configured.
#[allow(clippy::too_many_lines)]
fn harden_ddl_acls_sql(schema_lit: &str, app_roles: &[String]) -> String {
    let roles_array = if app_roles.is_empty() {
        "ARRAY[]::text[]".to_owned()
    } else {
        let items: Vec<String> = app_roles.iter().map(|r| quote_literal(r)).collect();
        format!("ARRAY[{}]::text[]", items.join(", "))
    };

    let apply_grants = if app_roles.is_empty() {
        String::new()
    } else {
        format!(
            "\n  FOREACH target_role IN ARRAY allowed_roles LOOP
    IF NOT EXISTS (
      SELECT 1 FROM pg_catalog.pg_namespace n
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(n.nspacl, pg_catalog.acldefault('n', n.nspowner))
      ) AS a
       WHERE n.nspname = {schema_lit}
         AND a.grantee = target_role::pg_catalog.regrole
         AND a.privilege_type = 'USAGE'
    ) THEN
      EXECUTE pg_catalog.format('GRANT USAGE ON SCHEMA %I TO %I', {schema_lit}, target_role);
    END IF;
    IF NOT EXISTS (
      SELECT 1 FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
      ) AS a
       WHERE n.nspname = {schema_lit}
         AND c.relname = 'changes'
         AND a.grantee = target_role::pg_catalog.regrole
         AND a.privilege_type = 'SELECT'
    ) THEN
      EXECUTE pg_catalog.format('GRANT SELECT ON TABLE %I.changes TO %I', {schema_lit}, target_role);
    END IF;
    IF NOT EXISTS (
      SELECT 1 FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(p.proacl, pg_catalog.acldefault('f', p.proowner))
      ) AS a
       WHERE n.nspname = {schema_lit}
         AND p.proname = 'log_event'
         AND p.pronargs = 3
         AND pg_catalog.pg_get_function_identity_arguments(p.oid) = 'action text, target jsonb, data jsonb'
         AND a.grantee = target_role::pg_catalog.regrole
         AND a.privilege_type = 'EXECUTE'
    ) THEN
      EXECUTE pg_catalog.format(
        'GRANT EXECUTE ON FUNCTION %I.log_event(text, jsonb, jsonb) TO %I',
        {schema_lit},
        target_role
      );
    END IF;
  END LOOP;"
        )
    };

    let authoritative = !app_roles.is_empty();
    let schema_filter = if authoritative {
        "a.grantee = 0 OR a.privilege_type <> 'USAGE' OR a.is_grantable OR pg_catalog.pg_get_userbyid(a.grantee)::text <> ALL (allowed_roles)"
    } else {
        "a.grantee = 0 OR a.privilege_type <> 'USAGE' OR a.is_grantable"
    };
    let schema_revoke = if authoritative {
        format!(
            "IF pg_catalog.pg_get_userbyid(grantee_oid)::text <> ALL (allowed_roles) THEN
        EXECUTE pg_catalog.format('REVOKE ALL ON SCHEMA %I FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));
      ELSE
        EXECUTE pg_catalog.format('REVOKE CREATE ON SCHEMA %I FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));
        EXECUTE pg_catalog.format('REVOKE GRANT OPTION FOR USAGE ON SCHEMA %I FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));
      END IF;"
        )
    } else {
        format!(
            "EXECUTE pg_catalog.format('REVOKE CREATE ON SCHEMA %I FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));
      EXECUTE pg_catalog.format('REVOKE GRANT OPTION FOR USAGE ON SCHEMA %I FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));"
        )
    };

    let changes_extra_revoke = if authoritative {
        format!(
            "\n  FOR grantee_oid IN
    SELECT DISTINCT a.grantee
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
      ) AS a
     WHERE n.nspname = {schema_lit}
       AND c.relname = 'changes'
       AND a.grantee <> c.relowner
       AND a.grantee <> 0
       AND pg_catalog.pg_get_userbyid(a.grantee)::text <> ALL (allowed_roles)
     ORDER BY a.grantee
  LOOP
    EXECUTE pg_catalog.format(
      'REVOKE ALL ON TABLE %I.changes FROM %I CASCADE',
      {schema_lit},
      pg_catalog.pg_get_userbyid(grantee_oid)
    );
  END LOOP;"
        )
    } else {
        String::new()
    };

    let log_event_filter = if authoritative {
        "a.grantee = 0 OR a.is_grantable OR pg_catalog.pg_get_userbyid(a.grantee)::text <> ALL (allowed_roles)"
    } else {
        "a.grantee = 0 OR a.is_grantable"
    };
    let log_event_revoke = if authoritative {
        format!(
            "IF pg_catalog.pg_get_userbyid(grantee_oid)::text <> ALL (allowed_roles) THEN
        EXECUTE pg_catalog.format('REVOKE ALL ON FUNCTION %I.log_event(text, jsonb, jsonb) FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));
      ELSE
        EXECUTE pg_catalog.format('REVOKE GRANT OPTION FOR EXECUTE ON FUNCTION %I.log_event(text, jsonb, jsonb) FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));
      END IF;"
        )
    } else {
        format!(
            "EXECUTE pg_catalog.format('REVOKE GRANT OPTION FOR EXECUTE ON FUNCTION %I.log_event(text, jsonb, jsonb) FROM %I CASCADE', {schema_lit}, pg_catalog.pg_get_userbyid(grantee_oid));"
        )
    };

    let target_role_decl = if authoritative {
        "\n  target_role text;"
    } else {
        ""
    };
    let allowed_roles_decl = if authoritative {
        format!("\n  allowed_roles text[] := {roles_array};")
    } else {
        String::new()
    };

    do_block(&format!(
        "DECLARE
  grantee_oid oid;{target_role_decl}{allowed_roles_decl}
BEGIN
  FOR grantee_oid IN
    SELECT DISTINCT a.grantee
      FROM pg_catalog.pg_namespace n
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(n.nspacl, pg_catalog.acldefault('n', n.nspowner))
      ) AS a
     WHERE n.nspname = {schema_lit}
       AND a.grantee <> n.nspowner
       AND ({schema_filter})
     ORDER BY a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON SCHEMA %I FROM PUBLIC CASCADE', {schema_lit});
    ELSE
      {schema_revoke}
    END IF;
  END LOOP;{changes_extra_revoke}

  FOR grantee_oid IN
    SELECT DISTINCT a.grantee
      FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(p.proacl, pg_catalog.acldefault('f', p.proowner))
      ) AS a
     WHERE n.nspname = {schema_lit}
       AND p.proname = 'log_event'
       AND p.pronargs = 3
       AND pg_catalog.pg_get_function_identity_arguments(p.oid) = 'action text, target jsonb, data jsonb'
       AND a.grantee <> p.proowner
       AND ({log_event_filter})
     ORDER BY a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON FUNCTION %I.log_event(text, jsonb, jsonb) FROM PUBLIC CASCADE', {schema_lit});
    ELSE
      {log_event_revoke}
    END IF;
  END LOOP;{apply_grants}
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

pub(crate) fn do_block(body: &str) -> String {
    // A quoted schema identifier can itself contain the normal dollar delimiter.
    if body.contains("$$") {
        format!("DO {}", quote_literal(body))
    } else {
        format!("DO $$\n{body}\n$$")
    }
}

fn log_event_sql(schema_q: &str) -> String {
    let tag = dollar_tag(schema_q);
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
AS {tag}\n\
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
{tag}"
    )
}
