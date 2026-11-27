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

/// Maximum UTF-8 byte length of a PostgreSQL identifier (`NAMEDATALEN - 1`).
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// NOLOGIN role owning all Auditstream database objects.
pub const OWNER_ROLE: &str = "auditstream_owner";

/// Fixed 64-bit transaction advisory lock key (`"audstrow"` in ASCII) serializing
/// `auditstream_owner` role creation across concurrent setup runs.
const ROLE_LOCK_KEY: i64 = 0x6175_6473_7472_6f77;

/// Transaction-local PostgreSQL setting proving setup statements run inside one transaction.
pub(crate) const TX_GUARD_SETTING: &str = "auditstream._migration_tx";

/// Transaction-local PostgreSQL setting storing the caller's role before entering `auditstream_owner`.
const PREV_ROLE_SETTING: &str = "auditstream._prev_role";

/// Validates an Auditstream schema name for CLI configuration and migration generation.
///
/// Enforces safe PostgreSQL identifier rules:
/// - non-empty and at most [`MAX_IDENTIFIER_BYTES`] (63) bytes, avoiding silent catalog truncation
/// - starts with an ASCII letter or underscore (`[A-Za-z_]`)
/// - remaining characters are ASCII letters, digits, or underscores (`[A-Za-z0-9_]`),
///   excluding `$`, quotes, whitespace, punctuation, and control characters
/// - does not start with PostgreSQL's reserved `pg_` system schema prefix (case-insensitive)
/// - is not PostgreSQL's built-in `information_schema` (case-insensitive)
///
/// # Errors
///
/// Returns a human-readable validation error message when `schema` violates any rule above.
pub fn validate_schema_name(schema: &str) -> Result<(), String> {
    if schema.is_empty() {
        return Err("audit schema name must not be empty".to_owned());
    }
    if schema.len() > MAX_IDENTIFIER_BYTES {
        return Err(format!(
            "audit schema name length {} exceeds PostgreSQL's {MAX_IDENTIFIER_BYTES}-byte identifier limit",
            schema.len()
        ));
    }
    let bytes = schema.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return Err(format!(
            "invalid audit schema name {schema:?}: must start with an ASCII letter or underscore"
        ));
    }
    if !bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(format!(
            "invalid audit schema name {schema:?}: must contain only ASCII letters, digits, and underscores"
        ));
    }
    if schema
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("pg_"))
    {
        return Err(format!(
            "invalid audit schema name {schema:?}: prefix `pg_` is reserved by PostgreSQL"
        ));
    }
    if schema.eq_ignore_ascii_case("information_schema") {
        return Err(format!(
            "invalid audit schema name {schema:?}: `information_schema` is reserved by PostgreSQL"
        ));
    }
    Ok(())
}

/// Selects a PostgreSQL dollar-quote tag (`$tag$`) for `CREATE OR REPLACE FUNCTION` bodies
/// that does not appear anywhere inside any string in `avoids`.
pub(crate) fn dollar_tag_avoiding(avoids: &[&str]) -> String {
    let collides = |tag: &str| avoids.iter().any(|text| text.contains(tag));
    for candidate in [
        "$fn$",
        "$auditfn$",
        "$auditstream_fn$",
        "$asfn$",
        "$auditstream_body$",
        "$auditstream_fn_body$",
    ] {
        if !collides(candidate) {
            return candidate.to_owned();
        }
    }
    let mut suffix: u64 = 1;
    loop {
        let candidate = format!("$auditstream_fn_{suffix}$");
        if !collides(&candidate) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
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
/// `create_role` is `true` (clearing `createrole_self_grant` on PG 16+ so creation never persists
/// implicit `INHERIT`/`SET` membership), resolves the deterministic `ADMIN` grantor role for
/// explicit `GRANTED BY`, acquires only `SET` (never `INHERIT`) on PG 16+, and records the exact
/// temporary grant mode (`none`, `self_row`, or `set`) in transaction-local GUCs so cleanup
/// revokes only what this transaction granted without disturbing pre-existing memberships or
/// breaking `SET`-only / `NOINHERIT` migrator roles.
#[allow(clippy::too_many_lines)]
pub(crate) fn role_grant_setup_sql(
    owner_q: &str,
    owner_lit: &str,
    setting_name: &str,
    context_label: &str,
    create_role: bool,
) -> String {
    let grantor_setting = format!("{setting_name}_grantor");
    let prev_self_grant_decl = if create_role {
        "  prev_self_grant text;\n"
    } else {
        ""
    };
    let create_clause = if create_role {
        format!(
            "  PERFORM pg_catalog.pg_advisory_xact_lock({ROLE_LOCK_KEY});\n  \
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {owner_lit}) THEN\n    \
    IF pg_catalog.current_setting('server_version_num')::int >= 160000 THEN\n      \
      prev_self_grant := pg_catalog.current_setting('createrole_self_grant');\n      \
      PERFORM pg_catalog.set_config('createrole_self_grant', '', true);\n      \
      EXECUTE 'CREATE ROLE {owner_q} NOLOGIN';\n      \
      PERFORM pg_catalog.set_config('createrole_self_grant', prev_self_grant, true);\n    \
    ELSE\n      \
      EXECUTE 'CREATE ROLE {owner_q} NOLOGIN';\n    \
    END IF;\n  \
  END IF;\n"
        )
    } else {
        String::new()
    };

    format!(
        "DO $$\n\
DECLARE\n  \
  grant_mode text := 'none';\n  \
  grantor_role text := '';\n  \
  grantor_oid oid;\n  \
  has_grantor_row boolean := false;\n\
{prev_self_grant_decl}\
BEGIN\n  \
  IF pg_catalog.current_setting('{TX_GUARD_SETTING}', true) IS DISTINCT FROM 'on' THEN\n    \
    RAISE EXCEPTION '{context_label} must run inside a single transaction'\n      \
      USING HINT = 'Wrap the generated statements in BEGIN; ... COMMIT; or use a transactional migrator.';\n  \
  END IF;\n\
{create_clause}  \
  IF pg_catalog.current_setting('server_version_num')::int >= 160000 THEN\n    \
    IF NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'SET') THEN\n      \
      SELECT m.member\n        \
        INTO grantor_oid\n        \
        FROM pg_catalog.pg_auth_members m\n        \
        JOIN pg_catalog.pg_roles r ON r.oid = m.roleid\n        \
        JOIN pg_catalog.pg_roles u ON u.rolname = current_user\n       \
       WHERE r.rolname = {owner_lit}\n         \
         AND m.admin_option\n         \
         AND (m.member = u.oid OR pg_catalog.pg_has_role(u.oid, m.member, 'USAGE'))\n       \
       ORDER BY (m.member = u.oid) DESC, m.member\n       \
       LIMIT 1;\n      \
      IF grantor_oid IS NULL THEN\n        \
        EXECUTE pg_catalog.format(\n          \
          'GRANT {owner_q} TO %I WITH INHERIT FALSE, SET TRUE',\n          \
          current_user\n        \
        );\n        \
        grant_mode := 'self_row';\n      \
      ELSE\n        \
        grantor_role := pg_catalog.pg_get_userbyid(grantor_oid)::text;\n        \
        SELECT EXISTS (\n          \
          SELECT 1\n            \
            FROM pg_catalog.pg_auth_members m\n            \
            JOIN pg_catalog.pg_roles r ON r.oid = m.roleid\n            \
            JOIN pg_catalog.pg_roles u ON u.oid = m.member\n           \
           WHERE r.rolname = {owner_lit}\n             \
             AND u.rolname = current_user\n             \
             AND m.grantor = grantor_oid\n        \
        ) INTO has_grantor_row;\n        \
        IF NOT has_grantor_row THEN\n          \
          EXECUTE pg_catalog.format(\n            \
            'GRANT {owner_q} TO %I WITH INHERIT FALSE, SET TRUE GRANTED BY %I',\n            \
            current_user,\n            \
            grantor_role\n          \
          );\n          \
          grant_mode := 'self_row';\n        \
        ELSE\n          \
          EXECUTE pg_catalog.format(\n            \
            'GRANT {owner_q} TO %I WITH SET TRUE GRANTED BY %I',\n            \
            current_user,\n            \
            grantor_role\n          \
          );\n          \
          grant_mode := 'set';\n        \
        END IF;\n      \
      END IF;\n    \
    END IF;\n  \
  ELSIF NOT pg_catalog.pg_has_role(current_user, {owner_lit}, 'MEMBER') THEN\n    \
    EXECUTE pg_catalog.format('GRANT {owner_q} TO %I', current_user);\n    \
    grant_mode := 'self_row';\n  \
  END IF;\n  \
  PERFORM pg_catalog.set_config('{setting_name}', grant_mode, true);\n  \
  PERFORM pg_catalog.set_config('{grantor_setting}', grantor_role, true);\n\
END\n\
$$"
    )
}

/// Revokes only the temporary membership or role option acquired by [`role_grant_setup_sql`].
pub(crate) fn role_grant_cleanup_sql(owner_q: &str, setting_name: &str) -> String {
    let grantor_setting = format!("{setting_name}_grantor");
    format!(
        "DO $$\n\
DECLARE\n  \
  grant_mode text := pg_catalog.current_setting('{setting_name}', true);\n  \
  grantor_role text := pg_catalog.current_setting('{grantor_setting}', true);\n  \
  granted_by text := '';\n\
BEGIN\n  \
  IF grantor_role IS NOT NULL AND grantor_role <> '' THEN\n    \
    granted_by := pg_catalog.format(' GRANTED BY %I', grantor_role);\n  \
  END IF;\n  \
  IF grant_mode = 'self_row' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE {owner_q} FROM %I', current_user) || granted_by;\n  \
  ELSIF grant_mode = 'set' THEN\n    \
    EXECUTE pg_catalog.format('REVOKE SET OPTION FOR {owner_q} FROM %I', current_user) || granted_by;\n  \
  END IF;\n  \
  IF grant_mode IS NOT NULL AND grant_mode <> '' THEN\n    \
    PERFORM pg_catalog.set_config('{setting_name}', '', true);\n  \
  END IF;\n  \
  IF grantor_role IS NOT NULL AND grantor_role <> '' THEN\n    \
    PERFORM pg_catalog.set_config('{grantor_setting}', '', true);\n  \
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
///
/// When `value` contains backslashes (`\`), emits a PostgreSQL `E'...'` escape string with
/// doubled `\` and `'` (matching PostgreSQL's own `quote_literal()`) so the literal remains safe
/// even if a session runs with `standard_conforming_strings = off`.
pub fn quote_literal(value: &str) -> String {
    if value.contains('\\') {
        format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
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
/// When installing both the history schema (D2) and capture triggers (D3) together, prefer
/// [`crate::migrations::install_statements`] (for statement-by-statement transactional drivers)
/// or [`crate::migrations::render_install_sql`] (for `psql`) instead of manually stitching this
/// output with [`crate::triggers::generate`].
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
    let mut app_roles = options.app_roles.clone();
    app_roles.sort();
    app_roles.dedup();

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
        harden_ddl_acls_sql(&schema_lit, &app_roles),
        restore_prev_role_sql(),
    ];

    GeneratedDdl {
        setup,
        future_partitions: partitions_sql(&schema_lit, &owner_lit, 1, 3),
        role_cleanup: role_grant_cleanup_sql(&owner_q, "auditstream._granted_self"),
    }
}

/// Reconciles pre-existing ownership of the audit schema, `changes` (which cascades to owned
/// identity sequences), all attached child partitions, and `log_event()` as the caller role
/// *before* switching to `SET LOCAL ROLE auditstream_owner`.
#[allow(clippy::too_many_lines)]
fn reconcile_ddl_ownership_sql(schema_lit: &str, owner_lit: &str) -> String {
    do_block(&format!(
        "DECLARE
  caller name := current_user;
  obj_schema name;
  obj_name name;
  fn_oid oid;
  need_reconcile boolean := false;
  granted_schema_usage boolean := false;
BEGIN
  IF EXISTS (
    SELECT 1 FROM pg_catalog.pg_namespace
     WHERE nspname = {schema_lit}
       AND nspowner <> {owner_lit}::pg_catalog.regrole
  ) THEN
    EXECUTE pg_catalog.format('ALTER SCHEMA %I OWNER TO %I', {schema_lit}, {owner_lit});
  END IF;

  SELECT EXISTS (
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
    SELECT 1
      FROM tree
      JOIN pg_catalog.pg_class c ON c.oid = tree.relid
     WHERE c.relowner <> {owner_lit}::pg_catalog.regrole
  ) OR EXISTS (
    SELECT 1
      FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = {schema_lit}
       AND p.proname = 'log_event'
       AND p.prokind = 'f'
       AND p.pronargs = 3
       AND p.proargtypes = (ARRAY[
         'pg_catalog.text'::pg_catalog.regtype::pg_catalog.oid,
         'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid,
         'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid
       ])::pg_catalog.oidvector
       AND p.proowner <> {owner_lit}::pg_catalog.regrole
  ) INTO need_reconcile;

  IF NOT need_reconcile THEN
    RETURN;
  END IF;

  IF NOT pg_catalog.pg_has_role(caller, {owner_lit}, 'USAGE') THEN
    IF NOT EXISTS (
      SELECT 1 FROM pg_catalog.pg_namespace n
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(n.nspacl, pg_catalog.acldefault('n', n.nspowner))
      ) AS a
       WHERE n.nspname = {schema_lit}
         AND a.grantee <> 0
         AND pg_catalog.pg_get_userbyid(a.grantee)::text = caller::text
         AND a.privilege_type = 'USAGE'
    ) THEN
      EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});
      EXECUTE pg_catalog.format('GRANT USAGE ON SCHEMA %I TO %I', {schema_lit}, caller);
      SET LOCAL ROLE NONE;
      IF caller <> current_user THEN
        EXECUTE pg_catalog.format('SET LOCAL ROLE %I', caller);
      END IF;
      granted_schema_usage := true;
    END IF;
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

  SELECT p.oid INTO fn_oid
    FROM pg_catalog.pg_proc p
    JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = {schema_lit}
     AND p.proname = 'log_event'
     AND p.prokind = 'f'
     AND p.pronargs = 3
     AND p.proargtypes = (ARRAY[
       'pg_catalog.text'::pg_catalog.regtype::pg_catalog.oid,
       'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid,
       'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid
     ])::pg_catalog.oidvector;
  IF fn_oid IS NOT NULL AND EXISTS (
    SELECT 1 FROM pg_catalog.pg_proc
     WHERE oid = fn_oid AND proowner <> {owner_lit}::pg_catalog.regrole
  ) THEN
    EXECUTE pg_catalog.format('ALTER FUNCTION %I.log_event(text, jsonb, jsonb) OWNER TO %I', {schema_lit}, {owner_lit});
  END IF;

  IF granted_schema_usage THEN
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});
    EXECUTE pg_catalog.format('REVOKE USAGE ON SCHEMA %I FROM %I', {schema_lit}, caller);
    SET LOCAL ROLE NONE;
    IF caller <> current_user THEN
      EXECUTE pg_catalog.format('SET LOCAL ROLE %I', caller);
    END IF;
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
    ),
    child_grants AS (
      SELECT cn.nspname AS rel_schema, c.relname AS rel_name, a.grantee
        FROM tree
        JOIN pg_catalog.pg_class c ON c.oid = tree.relid
        JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace
        CROSS JOIN LATERAL pg_catalog.aclexplode(
          COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
        ) AS a
       WHERE a.grantee <> c.relowner
      UNION
      SELECT cn.nspname AS rel_schema, c.relname AS rel_name, a.grantee
        FROM tree
        JOIN pg_catalog.pg_class c ON c.oid = tree.relid
        JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace
        JOIN pg_catalog.pg_attribute att
          ON att.attrelid = c.oid
         AND att.attnum > 0
         AND NOT att.attisdropped
         AND att.attacl IS NOT NULL
        CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a
       WHERE a.grantee <> c.relowner
    )
    SELECT child_grants.rel_schema, child_grants.rel_name, child_grants.grantee
      FROM child_grants
     ORDER BY child_grants.rel_schema, child_grants.rel_name, child_grants.grantee
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
    SELECT parent_grants.grantee FROM (
      SELECT a.grantee
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        CROSS JOIN LATERAL pg_catalog.aclexplode(
          COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
        ) AS a
       WHERE n.nspname = {schema_lit}
         AND c.relname = 'changes'
         AND a.grantee <> c.relowner
         AND (a.grantee = 0 OR a.privilege_type <> 'SELECT' OR a.is_grantable)
      UNION
      SELECT a.grantee
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        JOIN pg_catalog.pg_attribute att
          ON att.attrelid = c.oid
         AND att.attnum > 0
         AND NOT att.attisdropped
         AND att.attacl IS NOT NULL
        CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a
       WHERE n.nspname = {schema_lit}
         AND c.relname = 'changes'
         AND a.grantee <> c.relowner
         AND (a.grantee = 0 OR a.privilege_type <> 'SELECT' OR a.is_grantable)
    ) AS parent_grants
     ORDER BY parent_grants.grantee
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
      JOIN pg_catalog.pg_depend d
        ON d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
       AND d.refobjid = c.oid
       AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
       AND d.deptype IN ('a', 'i')
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
    SELECT future_grants.rel_name, future_grants.grantee FROM (
      SELECT c.relname AS rel_name, a.grantee
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        CROSS JOIN LATERAL pg_catalog.aclexplode(
          COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))
        ) AS a
       WHERE n.nspname = {schema_lit}
         AND c.relname = ANY (managed_tables)
         AND a.grantee <> c.relowner
      UNION
      SELECT c.relname AS rel_name, a.grantee
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        JOIN pg_catalog.pg_attribute att
          ON att.attrelid = c.oid
         AND att.attnum > 0
         AND NOT att.attisdropped
         AND att.attacl IS NOT NULL
        CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a
       WHERE n.nspname = {schema_lit}
         AND c.relname = ANY (managed_tables)
         AND a.grantee <> c.relowner
    ) AS future_grants
     ORDER BY future_grants.rel_name, future_grants.grantee
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
         AND a.grantee <> 0
         AND pg_catalog.pg_get_userbyid(a.grantee)::text = target_role
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
         AND a.grantee <> 0
         AND pg_catalog.pg_get_userbyid(a.grantee)::text = target_role
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
         AND p.prokind = 'f'
         AND p.pronargs = 3
         AND p.proargtypes = (ARRAY[
           'pg_catalog.text'::pg_catalog.regtype::pg_catalog.oid,
           'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid,
           'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid
         ])::pg_catalog.oidvector
         AND a.grantee <> 0
         AND pg_catalog.pg_get_userbyid(a.grantee)::text = target_role
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
    SELECT extra_changes_grants.grantee FROM (
      SELECT a.grantee
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
      UNION
      SELECT a.grantee
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        JOIN pg_catalog.pg_attribute att
          ON att.attrelid = c.oid
         AND att.attnum > 0
         AND NOT att.attisdropped
         AND att.attacl IS NOT NULL
        CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a
       WHERE n.nspname = {schema_lit}
         AND c.relname = 'changes'
         AND a.grantee <> c.relowner
         AND a.grantee <> 0
         AND pg_catalog.pg_get_userbyid(a.grantee)::text <> ALL (allowed_roles)
    ) AS extra_changes_grants
     ORDER BY extra_changes_grants.grantee
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
       AND p.prokind = 'f'
       AND p.pronargs = 3
       AND p.proargtypes = (ARRAY[
         'pg_catalog.text'::pg_catalog.regtype::pg_catalog.oid,
         'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid,
         'pg_catalog.jsonb'::pg_catalog.regtype::pg_catalog.oid
       ])::pg_catalog.oidvector
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
    // A quoted schema identifier or role name can itself contain `$$` or any fixed tag.
    // Pick a collision-free dollar-quote delimiter dynamically so `DO` blocks can never
    // terminate early and stay immune to `standard_conforming_strings = off`.
    if !body.contains("$$") {
        return format!("DO $$\n{body}\n$$");
    }
    for candidate in ["$do$", "$audit_do$", "$auditstream_do$"] {
        if !body.contains(candidate) {
            return format!("DO {candidate}\n{body}\n{candidate}");
        }
    }
    let mut suffix: u64 = 1;
    loop {
        let candidate = format!("$auditstream_do_{suffix}$");
        if !body.contains(&candidate) {
            return format!("DO {candidate}\n{body}\n{candidate}");
        }
        suffix = suffix.saturating_add(1);
    }
}

fn log_event_sql(schema_q: &str) -> String {
    let body = format!(
        "DECLARE\n  \
  resolved record;\n  \
  v_event_id uuid;\n\
BEGIN\n  \
  IF action IS NULL\n     \
     OR pg_catalog.char_length(action) < 1\n     \
     OR pg_catalog.char_length(action) > 64\n     \
     OR pg_catalog.regexp_like(action COLLATE pg_catalog.\"C\", E'[\\\\x01-\\\\x1f\\\\x7f\\\\u0080-\\\\u009f]')\n     \
     OR pg_catalog.btrim(\n          \
          action,\n          \
          E' \\t\\n\\r\\f\\x0b\\u0085\\u00a0\\u1680\\u2000\\u2001\\u2002\\u2003\\u2004\\u2005\\u2006\\u2007\\u2008\\u2009\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000'\n        \
        ) COLLATE pg_catalog.\"C\" <> action COLLATE pg_catalog.\"C\"\n     \
     OR pg_catalog.translate(\n          \
          action,\n          \
          'abcdefghijklmnopqrstuvwxyz',\n          \
          'ABCDEFGHIJKLMNOPQRSTUVWXYZ'\n        \
        ) COLLATE pg_catalog.\"C\" IN ('INSERT', 'UPDATE', 'DELETE', 'TRUNCATE') THEN\n    \
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
END"
    );
    let tag = dollar_tag_avoiding(&[schema_q, &body]);
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
{body}\n\
{tag}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollar_tag_avoids_delimiter_collision_in_schema_name() {
        let stmts = generate(&DdlOptions::new("audit$fn$stream", vec![]));
        let log_event = stmts
            .iter()
            .find(|s| s.starts_with("CREATE OR REPLACE FUNCTION "))
            .expect("log_event emitted");
        assert!(
            log_event.contains("AS $auditfn$"),
            "expected fallback tag $auditfn$, got: {log_event}"
        );

        // Exhausting every fixed fallback tag plus `_1` exercises the dynamic retry loop (`_2`).
        let all_fixed_fn = "$fn$$auditfn$$auditstream_fn$$asfn$$auditstream_body$$auditstream_fn_body$$auditstream_fn_1$";
        let stmts_all = generate(&DdlOptions::new(all_fixed_fn, vec![]));
        let log_event_all = stmts_all
            .iter()
            .find(|s| s.starts_with("CREATE OR REPLACE FUNCTION "))
            .expect("log_event emitted");
        assert!(
            log_event_all.contains("AS $auditstream_fn_2$\n")
                && log_event_all.ends_with("\n$auditstream_fn_2$"),
            "expected dynamic tag $auditstream_fn_2$, got: {log_event_all}"
        );
        assert_eq!(
            log_event_all.matches("$auditstream_fn_2$").count(),
            2,
            "dollar tag must appear only as opening and closing delimiters"
        );
    }

    #[test]
    fn do_block_uses_collision_free_dollar_quotes_even_when_all_fixed_tags_appear() {
        let simple = do_block("BEGIN NULL; END");
        assert_eq!(simple, "DO $$\nBEGIN NULL; END\n$$");

        let with_empty_tag = do_block("BEGIN PERFORM '$$'; END");
        assert_eq!(with_empty_tag, "DO $do$\nBEGIN PERFORM '$$'; END\n$do$");

        let all_fixed_do =
            "BEGIN PERFORM '$$$do$$audit_do$$auditstream_do$$auditstream_do_1$'; END";
        let wrapped = do_block(all_fixed_do);
        assert!(
            wrapped.starts_with("DO $auditstream_do_2$\n")
                && wrapped.ends_with("\n$auditstream_do_2$"),
            "expected dynamic $auditstream_do_2$ tag, got: {wrapped}"
        );
        assert_eq!(wrapped.matches("$auditstream_do_2$").count(), 2);

        // Adversarial app_roles with `$$` and `$do$` cannot break out of harden_ddl_acls_sql.
        let stmts = generate(&DdlOptions::new(
            DEFAULT_SCHEMA,
            vec!["evil$$$do$$audit_do$$auditstream_do$role".to_owned()],
        ));
        let acl_stmt = stmts
            .iter()
            .find(|s| s.contains("allowed_roles text[]"))
            .expect("harden_ddl_acls_sql emitted");
        assert!(
            acl_stmt.starts_with("DO $auditstream_do_1$\n")
                && acl_stmt.ends_with("\n$auditstream_do_1$"),
            "expected dynamic tag on harden_ddl_acls_sql, got: {acl_stmt}"
        );
        assert_eq!(acl_stmt.matches("$auditstream_do_1$").count(), 2);
    }

    #[test]
    fn validate_schema_name_accepts_safe_identifiers_and_rejects_breakouts() {
        for valid in [
            "a",
            "pg",
            "auditstream",
            "custom_audit",
            "_audit_2",
            "AuditStream_V2",
            &"a".repeat(MAX_IDENTIFIER_BYTES),
        ] {
            assert!(
                validate_schema_name(valid).is_ok(),
                "expected valid schema {valid:?}"
            );
        }

        for invalid in [
            "",
            " ",
            "123audit",
            "audit-stream",
            "audit.stream",
            "audit$$stream",
            "audit$fn$stream",
            "audit\"stream",
            "audit'stream",
            "audit; DROP TABLE x;--",
            "audit\nstream",
            "audit\0stream",
            "pg_audit",
            "PG_audit",
            "pg_catalog",
            "information_schema",
            "INFORMATION_SCHEMA",
            &"a".repeat(MAX_IDENTIFIER_BYTES + 1),
        ] {
            assert!(
                validate_schema_name(invalid).is_err(),
                "expected invalid schema {invalid:?}"
            );
        }
    }

    #[test]
    fn quote_literal_escapes_backslashes_with_e_string_for_standard_conforming_strings_off() {
        assert_eq!(quote_literal("auditstream"), "'auditstream'");
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal(r"a\b"), r"E'a\\b'");
        assert_eq!(
            quote_literal(r"t\'); DROP TABLE x;--"),
            r"E't\\''); DROP TABLE x;--'"
        );
    }

    #[test]
    fn generate_canonicalizes_app_roles_order_and_duplicates() {
        let a = generate(&DdlOptions::new(
            DEFAULT_SCHEMA,
            vec!["z_app".into(), "a_app".into(), "z_app".into()],
        ));
        let b = generate(&DdlOptions::new(
            DEFAULT_SCHEMA,
            vec!["a_app".into(), "z_app".into()],
        ));
        assert_eq!(a, b);
    }
}
