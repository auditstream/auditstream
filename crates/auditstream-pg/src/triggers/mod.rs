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
//!   `SET search_path = pg_catalog, pg_temp`, inlining actor resolution from the same Rust
//!   template as `resolve_actor()`, recording statement-level `INSERT` / `DELETE`
//!   (via transition tables, resolving the actor once per statement), row-level `UPDATE`
//!   (skipping no-op and excluded-column-only updates and computing `changed[]` inside a
//!   single-SPI `INSERT ... VALUES`), and statement-level `TRUNCATE`. Decodes unified
//!   `TG_ARGV[0]`/`TG_ARGV[1]` formats (v0.1.x JSON string arrays `'["id"]'`/`'[]'`,
//!   PostgreSQL `text[]` literals `'{a,b}'`/`'{}'`, and plain single column names `'id'`/`''`)
//!   while generated triggers keep emitting v0.1.x JSON arrays so function upgrades require
//!   zero `CREATE OR REPLACE TRIGGER` locks on audited tables.
//! - Per-table `CREATE OR REPLACE TRIGGER` definitions (`auditstream_insert`,
//!   `auditstream_update`, `auditstream_delete`, `auditstream_truncate`).
//!
//! Actor comment framing uses SQL string builtins with trailing-character and single-`/` fast
//! paths, while base64url and sqlcommenter key alphabet validation uses DFA-cached
//! `pg_catalog.textregexeq(... COLLATE pg_catalog."C", ...)` character classes; length bounds
//! remain explicit outside regexes so PostgreSQL's 255 quantifier cap is never reached.

use crate::ddl::{
    DEFAULT_SCHEMA, OWNER_ROLE, do_block, dollar_tag_avoiding, enter_owner_role_sql, quote_ident,
    quote_literal, restore_prev_role_sql, role_grant_cleanup_sql, role_grant_setup_sql,
    tx_begin_guard_sql,
};

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

    /// Returns any primary key column names also listed in [`Self::excluded_columns`],
    /// sorted and deduplicated for deterministic diagnostics.
    #[must_use]
    pub fn excluded_primary_key_columns(&self) -> Vec<&str> {
        let mut bad: Vec<&str> = self
            .excluded_columns
            .iter()
            .filter(|col| self.primary_key.iter().any(|pk| pk == *col))
            .map(String::as_str)
            .collect();
        bad.sort_unstable();
        bad.dedup();
        bad
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
/// Callers MUST execute the returned statements sequentially inside a single transaction
/// (`BEGIN; ... COMMIT;`) so any temporary `auditstream_owner` self-grant acquired for a
/// non-superuser migrator is atomic and cleanly revoked before commit.
///
/// When installing both the history schema (D2) and capture triggers (D3) together, prefer
/// [`crate::migrations::install_statements`] or [`crate::migrations::render_install_sql`]
/// instead of manually combining [`crate::ddl::generate`] with this function.
pub fn generate(options: &TriggerOptions) -> Vec<String> {
    let owner_q = quote_ident(OWNER_ROLE);
    let owner_lit = quote_literal(OWNER_ROLE);

    let body = generate_body(options);
    let mut stmts = Vec::with_capacity(body.len() + 3);
    stmts.push(tx_begin_guard_sql());
    stmts.push(role_grant_setup_sql(
        &owner_q,
        &owner_lit,
        "auditstream._triggers_granted_self",
        "Auditstream trigger statements",
        false,
    ));
    stmts.extend(body);
    stmts.push(role_grant_cleanup_sql(
        &owner_q,
        "auditstream._triggers_granted_self",
    ));
    stmts
}

/// Generates trigger functions (created and ACL-sanitized under `auditstream_owner`) and
/// per-table trigger definitions without transaction-marker or role self-grant wrapper blocks
/// (used by migration composers that already manage a transaction-scoped `auditstream_owner`
/// self-grant).
pub(crate) fn generate_body(options: &TriggerOptions) -> Vec<String> {
    let schema_q = quote_ident(&options.audit_schema);
    let schema_lit = quote_literal(&options.audit_schema);
    let owner_lit = quote_literal(OWNER_ROLE);

    let mut stmts = Vec::with_capacity(8 + options.tables.len() * 4);
    stmts.push(reconcile_trigger_functions_ownership_sql(
        &schema_lit,
        &owner_lit,
    ));
    stmts.push(enter_owner_role_sql(&owner_lit));
    stmts.push(resolve_actor_sql(&schema_q));
    stmts.push(capture_function_sql(&schema_q));
    stmts.push(harden_trigger_functions_acls_sql(&schema_lit));
    stmts.push(restore_prev_role_sql());

    if !options.tables.is_empty() {
        stmts.push(grant_temp_trigger_exec_if_noinherit_sql(
            &schema_lit,
            &owner_lit,
        ));
        for table in &options.tables {
            append_table_triggers(&mut stmts, &schema_q, table);
        }
        stmts.push(revoke_temp_trigger_exec_if_noinherit_sql(
            &schema_lit,
            &owner_lit,
        ));
    }

    stmts
}

fn append_table_triggers(stmts: &mut Vec<String>, schema_q: &str, table: &TableTriggerSpec) {
    let table_fq = format!(
        "{}.{}",
        quote_ident(&table.schema),
        quote_ident(&table.table)
    );
    let pk_arg = quote_literal(&encode_json_string_array(&table.primary_key));
    let mut excluded_sorted = table.excluded_columns.clone();
    excluded_sorted.sort();
    excluded_sorted.dedup();
    let ex_arg = quote_literal(&encode_json_string_array(&excluded_sorted));
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

pub(crate) fn encode_json_string_array(items: &[String]) -> String {
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

/// Reconciles pre-existing ownership of `resolve_actor()` and `capture()` as the caller role
/// *before* entering `SET LOCAL ROLE auditstream_owner` for `CREATE OR REPLACE FUNCTION`.
fn reconcile_trigger_functions_ownership_sql(schema_lit: &str, owner_lit: &str) -> String {
    do_block(&format!(
        "DECLARE
  caller name := current_user;
  fn_name name;
  need_reconcile boolean := false;
  granted_schema_usage boolean := false;
BEGIN
  SELECT EXISTS (
    SELECT 1 FROM pg_catalog.pg_proc p
    JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = {schema_lit}
       AND p.proname IN ('resolve_actor', 'capture')
       AND p.prokind = 'f'
       AND p.pronargs = 0
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

  FOREACH fn_name IN ARRAY ARRAY['resolve_actor', 'capture']::name[] LOOP
    IF EXISTS (
      SELECT 1 FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
       WHERE n.nspname = {schema_lit}
         AND p.proname = fn_name
         AND p.prokind = 'f'
         AND p.pronargs = 0
         AND p.proowner <> {owner_lit}::pg_catalog.regrole
    ) THEN
      EXECUTE pg_catalog.format('ALTER FUNCTION %I.%I() OWNER TO %I', {schema_lit}, fn_name, {owner_lit});
    END IF;
  END LOOP;

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

/// Revokes `PUBLIC` and every non-owner grant on `resolve_actor()` and `capture()` while running
/// as `auditstream_owner` (neutralizing `ALTER DEFAULT PRIVILEGES ... ON FUNCTIONS`).
fn harden_trigger_functions_acls_sql(schema_lit: &str) -> String {
    do_block(&format!(
        "DECLARE
  fn_name name;
  grantee_oid oid;
BEGIN
  FOR fn_name, grantee_oid IN
    SELECT DISTINCT p.proname, a.grantee
      FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
      CROSS JOIN LATERAL pg_catalog.aclexplode(
        COALESCE(p.proacl, pg_catalog.acldefault('f', p.proowner))
      ) AS a
     WHERE n.nspname = {schema_lit}
       AND p.proname IN ('resolve_actor', 'capture')
       AND p.prokind = 'f'
       AND p.pronargs = 0
       AND a.grantee <> p.proowner
     ORDER BY p.proname, a.grantee
  LOOP
    IF grantee_oid = 0 THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON FUNCTION %I.%I() FROM PUBLIC CASCADE', {schema_lit}, fn_name);
    ELSE
      EXECUTE pg_catalog.format(
        'REVOKE ALL ON FUNCTION %I.%I() FROM %I CASCADE',
        {schema_lit},
        fn_name,
        pg_catalog.pg_get_userbyid(grantee_oid)
      );
    END IF;
  END LOOP;
END"
    ))
}

/// On PostgreSQL 15 with a `NOINHERIT` migrator role and PostgreSQL 16+ with a `SET`-only
/// (`INHERIT FALSE, SET TRUE`) migrator role, temporarily grants `USAGE` on the audit schema and
/// `EXECUTE` on `capture()` to `current_user` for the duration of `CREATE OR REPLACE TRIGGER` and
/// revokes both immediately afterward inside the same transaction. No-op when `current_user`
/// already has `USAGE` on `auditstream_owner` (superusers and `INHERIT` members).
fn grant_temp_trigger_exec_if_noinherit_sql(schema_lit: &str, owner_lit: &str) -> String {
    do_block(&format!(
        "DECLARE
  caller name := current_user;
  granted_usage boolean := false;
  granted_exec boolean := false;
BEGIN
  IF NOT pg_catalog.pg_has_role(caller, {owner_lit}, 'USAGE') THEN
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});
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
      EXECUTE pg_catalog.format('GRANT USAGE ON SCHEMA %I TO %I', {schema_lit}, caller);
      granted_usage := true;
    END IF;
    EXECUTE pg_catalog.format('GRANT EXECUTE ON FUNCTION %I.capture() TO %I', {schema_lit}, caller);
    granted_exec := true;
    SET LOCAL ROLE NONE;
    IF caller <> current_user THEN
      EXECUTE pg_catalog.format('SET LOCAL ROLE %I', caller);
    END IF;
  END IF;
  PERFORM pg_catalog.set_config(
    'auditstream._temp_trigger_privs',
    CASE
      WHEN granted_usage AND granted_exec THEN 'both'
      WHEN granted_exec THEN 'exec'
      ELSE 'none'
    END,
    true
  );
END"
    ))
}

fn revoke_temp_trigger_exec_if_noinherit_sql(schema_lit: &str, owner_lit: &str) -> String {
    do_block(&format!(
        "DECLARE
  caller name := current_user;
  priv_mode text := pg_catalog.current_setting('auditstream._temp_trigger_privs', true);
BEGIN
  IF priv_mode IN ('both', 'exec') THEN
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', {owner_lit});
    EXECUTE pg_catalog.format('REVOKE EXECUTE ON FUNCTION %I.capture() FROM %I CASCADE', {schema_lit}, caller);
    IF priv_mode = 'both' THEN
      EXECUTE pg_catalog.format('REVOKE USAGE ON SCHEMA %I FROM %I CASCADE', {schema_lit}, caller);
    END IF;
    SET LOCAL ROLE NONE;
    IF caller <> current_user THEN
      EXECUTE pg_catalog.format('SET LOCAL ROLE %I', caller);
    END IF;
    PERFORM pg_catalog.set_config('auditstream._temp_trigger_privs', '', true);
  END IF;
END"
    ))
}

struct ActorResolutionTargets<'a> {
    actor_var: &'a str,
    context_var: &'a str,
    source_var: &'a str,
    exit_stmt: &'a str,
    indent: &'a str,
}

const ACTOR_RESOLUTION_DECLS: &str = r"  v_raw_setting text;
  v_q_tail text;
  v_last_ch text;
  v_q_len integer;
  v_prefix text;
  v_p1 integer;
  v_open_pos integer;
  v_comment_body text;
  v_body_len integer;
  v_allow_escapes boolean;
  v_pos integer;
  v_eq_pos integer;
  v_pair_key text;
  v_key_check text;
  v_val_start integer;
  v_val_end integer;
  v_scan_pos integer;
  v_quote_rel integer;
  v_q_idx integer;
  v_bs_prefix text;
  v_found_count integer;
  v_pair_ok boolean;
  v_has_tag boolean := false;
  v_raw_b64 text;
  v_b64_len integer;
  v_decoded_bytes bytea;
  v_payload jsonb;
  v_source text;
  v_type text;
  v_val jsonb;
  v_obo jsonb;
  v_ctx jsonb;
  v_claims jsonb;";

const ACTOR_RESOLUTION_BODY_TEMPLATE: &str = r#"  v_raw_setting := NULLIF(pg_catalog.current_setting('auditstream.actor', true), '');
  IF v_raw_setting IS NOT NULL THEN
    IF pg_catalog.octet_length(v_raw_setting) > 512 THEN
      __INVALID_EXIT__
    END IF;
    BEGIN
      v_payload := v_raw_setting::jsonb;
    EXCEPTION WHEN others THEN
      __INVALID_EXIT__
    END;
    v_source := 'set_config';
  ELSE
    v_q_tail := pg_catalog.right(pg_catalog.current_query(), 4096);
    v_last_ch := pg_catalog.right(v_q_tail, 1);
    IF v_last_ch = '/'
       OR (v_last_ch <> '' AND pg_catalog.strpos(E' \t\n\r\f\x0b;', v_last_ch) > 0) THEN
      IF v_last_ch <> '/' THEN
        v_q_tail := pg_catalog.rtrim(v_q_tail, E' \t\n\r\f\x0b;');
      END IF;
      v_q_len := pg_catalog.length(v_q_tail);
      IF v_q_len >= 4
         AND pg_catalog.right(v_q_tail, 2) = '*/'
         AND pg_catalog.strpos(v_q_tail, 'auditstream=''') > 0 THEN
        v_prefix := pg_catalog.left(v_q_tail, v_q_len - 2);
        v_p1 := pg_catalog.strpos(v_prefix, '/');
        IF v_p1 > 0 THEN
          IF pg_catalog.strpos(pg_catalog.substr(v_prefix, v_p1 + 1), '/') = 0 THEN
            v_open_pos := v_p1;
          ELSE
            v_open_pos := v_q_len - 1 - pg_catalog.strpos(pg_catalog.reverse(v_prefix), '/');
          END IF;
          IF pg_catalog.substr(v_prefix, v_open_pos, 2) = '/*'
             AND (CASE
               WHEN v_open_pos > 1 THEN pg_catalog.substr(v_prefix, v_open_pos - 1, 1) <> '*'
               ELSE pg_catalog.length(pg_catalog.current_query()) <= 4096
                    OR pg_catalog.left(pg_catalog.right(pg_catalog.current_query(), 4097), 1) <> '*'
             END) THEN
            v_comment_body := pg_catalog.substr(v_prefix, v_open_pos + 2);
            v_body_len := pg_catalog.length(v_comment_body);
            IF v_body_len >= 14
               AND pg_catalog.left(v_comment_body, 13) = 'auditstream='''
               AND pg_catalog.right(v_comment_body, 1) = '''' THEN
              v_raw_b64 := pg_catalog.substr(v_comment_body, 14, v_body_len - 14);
              IF pg_catalog.strpos(v_raw_b64, '''') = 0 THEN
                v_has_tag := true;
              END IF;
            END IF;
            IF NOT v_has_tag AND pg_catalog.strpos(v_comment_body, 'auditstream=''') > 0 THEN
              FOR pass_idx IN 0..1 LOOP
                v_allow_escapes := (pass_idx = 1);
                IF v_allow_escapes AND pg_catalog.strpos(v_comment_body, E'\\''') = 0 THEN
                  EXIT;
                END IF;
                v_pos := 1;
                v_found_count := 0;
                v_pair_ok := true;
                v_raw_b64 := NULL;
                WHILE v_pos <= v_body_len LOOP
                  v_eq_pos := pg_catalog.strpos(pg_catalog.substr(v_comment_body, v_pos), '=''');
                  IF v_eq_pos <= 1 THEN
                    v_pair_ok := false;
                    EXIT;
                  END IF;
                  v_pair_key := pg_catalog.substr(v_comment_body, v_pos, v_eq_pos - 1);
                  v_key_check := CASE
                    WHEN v_allow_escapes THEN pg_catalog.replace(v_pair_key, E'\\''', '')
                    ELSE v_pair_key
                  END;
                  IF pg_catalog.textregexeq(v_key_check COLLATE pg_catalog."C", '[^A-Za-z0-9_.*~()!%-]') THEN
                    v_pair_ok := false;
                    EXIT;
                  END IF;
                  v_val_start := v_pos + v_eq_pos + 1;
                  v_val_end := 0;
                  IF v_allow_escapes AND v_pair_key <> 'auditstream' THEN
                    v_scan_pos := v_val_start;
                    WHILE v_scan_pos <= v_body_len LOOP
                      v_quote_rel := pg_catalog.strpos(pg_catalog.substr(v_comment_body, v_scan_pos), '''');
                      IF v_quote_rel = 0 THEN
                        EXIT;
                      END IF;
                      v_q_idx := v_scan_pos + v_quote_rel - 1;
                      IF v_quote_rel = 1 OR pg_catalog.substr(v_comment_body, v_q_idx - 1, 1) <> E'\\' THEN
                        v_val_end := v_q_idx;
                        EXIT;
                      END IF;
                      v_bs_prefix := pg_catalog.substr(v_comment_body, v_scan_pos, v_quote_rel - 1);
                      IF (pg_catalog.length(v_bs_prefix) - pg_catalog.length(pg_catalog.rtrim(v_bs_prefix, E'\\'))) % 2 = 0 THEN
                        v_val_end := v_q_idx;
                        EXIT;
                      END IF;
                      v_scan_pos := v_q_idx + 1;
                    END LOOP;
                  ELSE
                    v_quote_rel := pg_catalog.strpos(pg_catalog.substr(v_comment_body, v_val_start), '''');
                    IF v_quote_rel > 0 THEN
                      v_val_end := v_val_start + v_quote_rel - 1;
                    END IF;
                  END IF;
                  IF v_val_end = 0 THEN
                    v_pair_ok := false;
                    EXIT;
                  END IF;
                  IF v_pair_key = 'auditstream' THEN
                    v_found_count := v_found_count + 1;
                    IF v_found_count = 1 THEN
                      v_raw_b64 := pg_catalog.substr(v_comment_body, v_val_start, v_val_end - v_val_start);
                    END IF;
                  END IF;
                  v_pos := v_val_end + 1;
                  IF v_pos <= v_body_len THEN
                    IF pg_catalog.substr(v_comment_body, v_pos, 1) <> ',' OR v_pos = v_body_len THEN
                      v_pair_ok := false;
                      EXIT;
                    END IF;
                    v_pos := v_pos + 1;
                  END IF;
                END LOOP;
                IF v_pair_ok THEN
                  IF v_found_count > 1 THEN
                    __INVALID_EXIT__
                  END IF;
                  IF v_found_count = 1 THEN
                    v_has_tag := true;
                    EXIT;
                  END IF;
                END IF;
              END LOOP;
            END IF;
          END IF;
        END IF;
      END IF;
    END IF;
    IF v_has_tag THEN
      v_b64_len := pg_catalog.length(v_raw_b64);
      IF v_b64_len < 1
         OR v_b64_len > 683
         OR v_b64_len % 4 = 1
         OR pg_catalog.textregexeq(v_raw_b64 COLLATE pg_catalog."C", '[^A-Za-z0-9_-]') THEN
        __INVALID_EXIT__
      END IF;
      BEGIN
        v_decoded_bytes := pg_catalog.decode(
          pg_catalog.translate(v_raw_b64, '-_', '+/')
            || pg_catalog.repeat('=', (4 - v_b64_len % 4) % 4),
          'base64'
        );
        IF pg_catalog.octet_length(v_decoded_bytes) > 512 THEN
          __INVALID_EXIT__
        END IF;
        v_payload := pg_catalog.convert_from(v_decoded_bytes, 'UTF8')::jsonb;
      EXCEPTION WHEN others THEN
        __INVALID_EXIT__
      END;
      v_source := 'comment';
    END IF;
  END IF;

  IF v_source IS NOT NULL THEN
    IF pg_catalog.jsonb_typeof(v_payload) IS DISTINCT FROM 'object' THEN
      __INVALID_EXIT__
    END IF;
    v_type := v_payload->>'type';
    v_val := v_payload->'id';
    IF v_type IS NULL
       OR pg_catalog.jsonb_typeof(v_payload->'type') IS DISTINCT FROM 'string'
       OR v_type NOT IN ('user', 'api_key', 'ai_agent', 'service', 'system')
       OR pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string'
       OR v_val = '""'::jsonb THEN
      __INVALID_EXIT__
    END IF;
    v_val := v_payload->'tenant';
    IF v_val IS NOT NULL
       AND (pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string' OR v_val = '""'::jsonb) THEN
      __INVALID_EXIT__
    END IF;
    v_obo := v_payload->'onBehalfOf';
    IF v_obo IS NOT NULL THEN
      IF pg_catalog.jsonb_typeof(v_obo) IS DISTINCT FROM 'object' THEN
        __INVALID_EXIT__
      END IF;
      v_type := v_obo->>'type';
      v_val := v_obo->'id';
      IF v_type IS NULL
         OR pg_catalog.jsonb_typeof(v_obo->'type') IS DISTINCT FROM 'string'
         OR v_type NOT IN ('user', 'api_key', 'ai_agent', 'service', 'system')
         OR pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string'
         OR v_val = '""'::jsonb
         OR v_obo->'onBehalfOf' IS NOT NULL THEN
        __INVALID_EXIT__
      END IF;
      v_val := v_obo->'tenant';
      IF v_val IS NOT NULL
         AND (pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string' OR v_val = '""'::jsonb) THEN
        __INVALID_EXIT__
      END IF;
    END IF;
    v_ctx := v_payload->'context';
    IF v_ctx IS NOT NULL THEN
      IF pg_catalog.jsonb_typeof(v_ctx) IS DISTINCT FROM 'object' THEN
        __INVALID_EXIT__
      END IF;
      v_val := v_ctx->'requestId';
      IF v_val IS NOT NULL
         AND (pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string' OR v_val = '""'::jsonb) THEN
        __INVALID_EXIT__
      END IF;
      v_val := v_ctx->'source';
      IF v_val IS NOT NULL
         AND (pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string' OR v_val = '""'::jsonb) THEN
        __INVALID_EXIT__
      END IF;
      v_val := v_ctx->'ip';
      IF v_val IS NOT NULL
         AND (pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string' OR v_val = '""'::jsonb) THEN
        __INVALID_EXIT__
      END IF;
      v_val := v_ctx->'userAgent';
      IF v_val IS NOT NULL
         AND (pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string' OR v_val = '""'::jsonb) THEN
        __INVALID_EXIT__
      END IF;
      __CONTEXT__ := v_ctx;
      __ACTOR__ := v_payload - 'context';
    ELSE
      __CONTEXT__ := NULL;
      __ACTOR__ := v_payload;
    END IF;
    __SOURCE__ := v_source;
    __EXIT__
  END IF;

  v_raw_setting := NULLIF(pg_catalog.current_setting('request.jwt.claims', true), '');
  IF v_raw_setting IS NOT NULL THEN
    BEGIN
      v_claims := v_raw_setting::jsonb;
    EXCEPTION WHEN others THEN
      __INVALID_EXIT__
    END;
    IF pg_catalog.jsonb_typeof(v_claims) IS DISTINCT FROM 'object' THEN
      __INVALID_EXIT__
    END IF;
    v_val := v_claims->'sub';
    IF v_val IS NOT NULL
       AND pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'null' THEN
      IF pg_catalog.jsonb_typeof(v_val) IS DISTINCT FROM 'string'
         OR v_val = '""'::jsonb
         OR 21 + pg_catalog.octet_length(v_val::text) > 512 THEN
        __INVALID_EXIT__
      END IF;
      __ACTOR__ := pg_catalog.jsonb_build_object('type', 'user', 'id', v_val);
      __CONTEXT__ := NULL;
      __SOURCE__ := 'jwt';
      __EXIT__
    END IF;
  END IF;

  __ACTOR__ := NULL;
  __CONTEXT__ := NULL;
  __SOURCE__ := NULL;"#;

fn indent_lines(block: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return block.to_owned();
    }
    block
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_actor_resolution_body(targets: &ActorResolutionTargets<'_>) -> String {
    let invalid_exit = format!(
        "{} := NULL; {} := NULL; {} := 'invalid'; {}",
        targets.actor_var, targets.context_var, targets.source_var, targets.exit_stmt
    );
    let rendered = ACTOR_RESOLUTION_BODY_TEMPLATE
        .replace("__INVALID_EXIT__", &invalid_exit)
        .replace("__ACTOR__", targets.actor_var)
        .replace("__CONTEXT__", targets.context_var)
        .replace("__SOURCE__", targets.source_var)
        .replace("__EXIT__", targets.exit_stmt);
    indent_lines(&rendered, targets.indent)
}

fn resolve_actor_sql(schema_q: &str) -> String {
    let resolved_body = render_actor_resolution_body(&ActorResolutionTargets {
        actor_var: "actor",
        context_var: "context",
        source_var: "actor_source",
        exit_stmt: "RETURN;",
        indent: "",
    });
    let body = format!("DECLARE\n{ACTOR_RESOLUTION_DECLS}\nBEGIN\n{resolved_body}\nEND");
    let tag = dollar_tag_avoiding(&[schema_q, &body]);
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
AS {tag}\n\
{body}\n\
{tag}"
    )
}

/// Generates `{schema}.capture()` with inlined actor resolution from
/// [`render_actor_resolution_body`], single-SPI `UPDATE` `changed[]` computation, and unified
/// `TG_ARGV[0]`/`TG_ARGV[1]` decoding (JSON arrays, PostgreSQL `text[]`, and plain single column
/// names) while `append_table_triggers()` keeps emitting v0.1.x JSON arrays for lock-free
/// function-only upgrades.
#[allow(clippy::too_many_lines)]
fn capture_function_sql(schema_q: &str) -> String {
    let inlined_actor = render_actor_resolution_body(&ActorResolutionTargets {
        actor_var: "v_actor",
        context_var: "v_context",
        source_var: "v_actor_source",
        exit_stmt: "EXIT resolve_actor;",
        indent: "  ",
    });
    let body = format!(
        "DECLARE\n  \
  v_actor jsonb;\n  \
  v_context jsonb;\n  \
  v_actor_source text;\n\
{ACTOR_RESOLUTION_DECLS}\n  \
  v_arg text;\n  \
  pk_json jsonb;\n  \
  pk_len integer;\n  \
  pk_single text;\n  \
  pk_cols text[] := '{{}}';\n  \
  ex_json jsonb;\n  \
  ex_len integer;\n  \
  ex_cols text[] := '{{}}';\n  \
  has_ex boolean := false;\n  \
  v_schema text;\n  \
  v_table text;\n  \
  v_relid oid;\n  \
  v_before jsonb;\n  \
  v_after jsonb;\n  \
  v_record_id jsonb;\n\
BEGIN\n  \
  IF TG_NARGS >= 4 THEN\n    \
    v_schema := TG_ARGV[2];\n    \
    v_table := TG_ARGV[3];\n    \
    SELECT c.oid\n      \
      INTO v_relid\n      \
      FROM pg_catalog.pg_class c\n      \
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n     \
     WHERE n.nspname = v_schema\n       \
       AND c.relname = v_table;\n    \
    IF v_relid IS NULL OR (\n      \
      v_relid <> TG_RELID\n      \
      AND NOT EXISTS (\n        \
        SELECT 1\n          \
          FROM pg_catalog.pg_partition_ancestors(TG_RELID) AS a(relid)\n         \
         WHERE a.relid = v_relid\n      \
      )\n    \
    ) THEN\n      \
      RAISE EXCEPTION 'auditstream.capture: trigger relation % is not a partition of %.%',\n        \
        TG_RELID::pg_catalog.regclass, v_schema, v_table\n        \
        USING ERRCODE = '42501';\n    \
    END IF;\n  \
  ELSE\n    \
    v_schema := TG_TABLE_SCHEMA;\n    \
    v_table := TG_TABLE_NAME;\n    \
    v_relid := TG_RELID;\n  \
  END IF;\n\n  \
  IF TG_OP <> 'TRUNCATE' THEN\n    \
    IF TG_NARGS >= 2 THEN\n      \
      v_arg := TG_ARGV[1];\n      \
      IF v_arg <> '[]' AND v_arg <> '{{}}' AND v_arg <> '' THEN\n        \
        IF pg_catalog.left(v_arg, 1) = '[' THEN\n          \
          ex_json := v_arg::jsonb;\n          \
          ex_len := pg_catalog.jsonb_array_length(ex_json);\n          \
          IF ex_len = 1 THEN\n            \
            ex_cols := ARRAY[ex_json->>0];\n            \
            has_ex := true;\n          \
          ELSIF ex_len > 1 THEN\n            \
            ex_cols := ARRAY(SELECT pg_catalog.jsonb_array_elements_text(ex_json));\n            \
            has_ex := true;\n          \
          END IF;\n        \
        ELSIF pg_catalog.left(v_arg, 1) = '{{' THEN\n          \
          ex_cols := v_arg::text[];\n          \
          has_ex := (pg_catalog.cardinality(ex_cols) > 0);\n        \
        ELSE\n          \
          ex_cols := ARRAY[v_arg];\n          \
          has_ex := true;\n        \
        END IF;\n      \
      END IF;\n    \
    END IF;\n\n    \
    IF TG_OP = 'UPDATE' THEN\n      \
      IF has_ex THEN\n        \
        v_before := pg_catalog.to_jsonb(OLD) - ex_cols;\n        \
        v_after := pg_catalog.to_jsonb(NEW) - ex_cols;\n      \
      ELSE\n        \
        v_before := pg_catalog.to_jsonb(OLD);\n        \
        v_after := pg_catalog.to_jsonb(NEW);\n      \
      END IF;\n      \
      IF v_after = v_before THEN\n        \
        RETURN NULL;\n      \
      END IF;\n    \
    END IF;\n  \
  END IF;\n\n  \
  <<resolve_actor>>\n  \
  BEGIN\n\
{inlined_actor}\n  \
  END resolve_actor;\n\n  \
  IF TG_OP = 'TRUNCATE' THEN\n    \
    INSERT INTO {schema_q}.changes (\n      \
      source, schema_name, table_name, table_oid, op,\n      \
      actor, context, actor_source, db_user, schema_version\n    \
    )\n    \
    VALUES (\n      \
      'db', v_schema, v_table, v_relid, 'TRUNCATE',\n      \
      v_actor, v_context, v_actor_source,\n      \
      session_user, 1\n    \
    );\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  IF TG_NARGS >= 1 THEN\n    \
    v_arg := TG_ARGV[0];\n    \
    IF v_arg <> '[]' AND v_arg <> '{{}}' AND v_arg <> '' THEN\n      \
      IF pg_catalog.left(v_arg, 1) = '[' THEN\n        \
        pk_json := v_arg::jsonb;\n        \
        pk_len := pg_catalog.jsonb_array_length(pk_json);\n        \
        IF pk_len = 1 THEN\n          \
          pk_single := pk_json->>0;\n        \
        ELSIF pk_len > 1 THEN\n          \
          pk_cols := ARRAY(SELECT pg_catalog.jsonb_array_elements_text(pk_json));\n        \
        END IF;\n      \
      ELSIF pg_catalog.left(v_arg, 1) = '{{' THEN\n        \
        pk_cols := v_arg::text[];\n        \
        pk_len := pg_catalog.cardinality(pk_cols);\n        \
        IF pk_len = 1 THEN\n          \
          pk_single := pk_cols[1];\n          \
          pk_cols := '{{}}';\n        \
        END IF;\n      \
      ELSE\n        \
        pk_single := v_arg;\n      \
      END IF;\n    \
    END IF;\n  \
  END IF;\n\n  \
  IF TG_OP = 'UPDATE' THEN\n    \
    IF pk_single IS NOT NULL THEN\n      \
      v_record_id := pg_catalog.jsonb_build_object(pk_single, v_after->pk_single);\n    \
    ELSIF pg_catalog.cardinality(pk_cols) > 0 THEN\n      \
      SELECT pg_catalog.jsonb_object_agg(k, v_after->k)\n        \
        INTO v_record_id\n        \
        FROM pg_catalog.unnest(pk_cols) AS k;\n    \
    END IF;\n    \
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
      ARRAY(\n        \
        SELECT e.k\n          \
          FROM pg_catalog.jsonb_each(v_after) AS e(k, v)\n         \
         WHERE v_before->e.k IS DISTINCT FROM e.v\n      \
      ),\n      \
      v_actor,\n      \
      v_context,\n      \
      v_actor_source,\n      \
      session_user,\n      \
      1\n    \
    );\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  IF TG_OP = 'INSERT' THEN\n    \
    IF pk_single IS NOT NULL AND NOT has_ex THEN\n      \
      INSERT INTO {schema_q}.changes (\n        \
        source, schema_name, table_name, table_oid, op,\n        \
        record_id, before, after, changed,\n        \
        actor, context, actor_source, db_user, schema_version\n      \
      )\n      \
      SELECT\n        \
        'db',\n        \
        v_schema,\n        \
        v_table,\n        \
        v_relid,\n        \
        'INSERT',\n        \
        pg_catalog.jsonb_build_object(pk_single, s.row_after->pk_single),\n        \
        NULL,\n        \
        s.row_after,\n        \
        NULL,\n        \
        v_actor,\n        \
        v_context,\n        \
        v_actor_source,\n        \
        session_user,\n        \
        1\n      \
      FROM (\n        \
        SELECT pg_catalog.to_jsonb(r) AS row_after\n          \
          FROM new_rows AS r\n        \
        OFFSET 0\n      \
      ) AS s;\n    \
    ELSE\n      \
      INSERT INTO {schema_q}.changes (\n        \
        source, schema_name, table_name, table_oid, op,\n        \
        record_id, before, after, changed,\n        \
        actor, context, actor_source, db_user, schema_version\n      \
      )\n      \
      SELECT\n        \
        'db',\n        \
        v_schema,\n        \
        v_table,\n        \
        v_relid,\n        \
        'INSERT',\n        \
        CASE\n          \
          WHEN pk_single IS NOT NULL THEN pg_catalog.jsonb_build_object(pk_single, s.row_after->pk_single)\n          \
          WHEN pg_catalog.cardinality(pk_cols) > 0 THEN (\n            \
            SELECT pg_catalog.jsonb_object_agg(k, s.row_after->k)\n              \
              FROM pg_catalog.unnest(pk_cols) AS k\n          \
          )\n        \
        END,\n        \
        NULL,\n        \
        s.row_after,\n        \
        NULL,\n        \
        v_actor,\n        \
        v_context,\n        \
        v_actor_source,\n        \
        session_user,\n        \
        1\n      \
      FROM (\n        \
        SELECT CASE WHEN has_ex THEN pg_catalog.to_jsonb(r) - ex_cols ELSE pg_catalog.to_jsonb(r) END AS row_after\n          \
          FROM new_rows AS r\n        \
        OFFSET 0\n      \
      ) AS s;\n    \
    END IF;\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  IF TG_OP = 'DELETE' THEN\n    \
    IF pk_single IS NOT NULL AND NOT has_ex THEN\n      \
      INSERT INTO {schema_q}.changes (\n        \
        source, schema_name, table_name, table_oid, op,\n        \
        record_id, before, after, changed,\n        \
        actor, context, actor_source, db_user, schema_version\n      \
      )\n      \
      SELECT\n        \
        'db',\n        \
        v_schema,\n        \
        v_table,\n        \
        v_relid,\n        \
        'DELETE',\n        \
        pg_catalog.jsonb_build_object(pk_single, s.row_before->pk_single),\n        \
        s.row_before,\n        \
        NULL,\n        \
        NULL,\n        \
        v_actor,\n        \
        v_context,\n        \
        v_actor_source,\n        \
        session_user,\n        \
        1\n      \
      FROM (\n        \
        SELECT pg_catalog.to_jsonb(r) AS row_before\n          \
          FROM old_rows AS r\n        \
        OFFSET 0\n      \
      ) AS s;\n    \
    ELSE\n      \
      INSERT INTO {schema_q}.changes (\n        \
        source, schema_name, table_name, table_oid, op,\n        \
        record_id, before, after, changed,\n        \
        actor, context, actor_source, db_user, schema_version\n      \
      )\n      \
      SELECT\n        \
        'db',\n        \
        v_schema,\n        \
        v_table,\n        \
        v_relid,\n        \
        'DELETE',\n        \
        CASE\n          \
          WHEN pk_single IS NOT NULL THEN pg_catalog.jsonb_build_object(pk_single, s.row_before->pk_single)\n          \
          WHEN pg_catalog.cardinality(pk_cols) > 0 THEN (\n            \
            SELECT pg_catalog.jsonb_object_agg(k, s.row_before->k)\n              \
              FROM pg_catalog.unnest(pk_cols) AS k\n          \
          )\n        \
        END,\n        \
        s.row_before,\n        \
        NULL,\n        \
        NULL,\n        \
        v_actor,\n        \
        v_context,\n        \
        v_actor_source,\n        \
        session_user,\n        \
        1\n      \
      FROM (\n        \
        SELECT CASE WHEN has_ex THEN pg_catalog.to_jsonb(r) - ex_cols ELSE pg_catalog.to_jsonb(r) END AS row_before\n          \
          FROM old_rows AS r\n        \
        OFFSET 0\n      \
      ) AS s;\n    \
    END IF;\n    \
    RETURN NULL;\n  \
  END IF;\n\n  \
  RETURN NULL;\n\
END"
    );
    let tag = dollar_tag_avoiding(&[schema_q, &body]);
    format!(
        "CREATE OR REPLACE FUNCTION {schema_q}.capture()\n\
RETURNS trigger\n\
LANGUAGE plpgsql\n\
SECURITY DEFINER\n\
SET search_path = pg_catalog, pg_temp\n\
AS {tag}\n\
{body}\n\
{tag}"
    )
}
