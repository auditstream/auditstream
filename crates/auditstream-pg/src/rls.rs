//! Shared PostgreSQL Row-Level Security (RLS) catalog inspection for
//! `{audit_schema}.changes`.
//!
//! Used by [`crate::verify`], [`crate::reader`], and [`crate::partitions`] so RLS visibility
//! rules are evaluated identically everywhere: always inspecting `pg_catalog.row_security_active`
//! on the **parent** `{audit_schema}.changes` table (never `changes_default`) and checking both
//! `PERMISSIVE` and `RESTRICTIVE` `SELECT`/`ALL` policies applicable to `current_user` (including
//! `PUBLIC` and inherited role memberships via `pg_has_role(..., 'USAGE')`).

use auditstream_core::SourceError;
use sqlx::FromRow;
use sqlx::postgres::PgConnection;

/// Row-Level Security visibility assessment for the active PostgreSQL role on
/// `{audit_schema}.changes`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)]
pub struct ExportRlsStatus {
    /// Whether `{audit_schema}.changes` exists in `pg_catalog.pg_class`.
    pub table_exists: bool,
    /// Whether `pg_catalog.row_security_active(changes.oid)` is `true` on the parent table.
    pub row_security_active: bool,
    /// Whether at least one applicable `PERMISSIVE` `SELECT`/`ALL` policy has `USING (true)`.
    pub has_unrestricted_permissive_select: bool,
    /// Whether any applicable `PERMISSIVE` `SELECT`/`ALL` policy has a non-`'true'` `USING`
    /// expression (which PostgreSQL still rewrites/plans alongside `USING (true)`).
    pub has_nontrivial_applicable_permissive_select: bool,
    /// Number of applicable `PERMISSIVE` `SELECT`/`ALL` policies with a non-null `USING` clause.
    pub applicable_permissive_select_policies: usize,
    /// Names of applicable `RESTRICTIVE` `SELECT`/`ALL` policies whose `USING` expression is not
    /// `'true'` (including `NULL` `USING` expressions, failing closed).
    pub blocking_restrictive_policies: Vec<String>,
    /// Effective PostgreSQL role (`current_user`) evaluated by the catalog check.
    pub current_role: String,
}

impl ExportRlsStatus {
    /// Returns `true` when the active role can read every row in `{audit_schema}.changes`
    /// without silent RLS filtering.
    ///
    /// Specifically, returns `true` when `row_security_active` is `false` (RLS disabled,
    /// superuser, `BYPASSRLS`, or table owner without `FORCE ROW LEVEL SECURITY`), or when at
    /// least one applicable `PERMISSIVE` policy covering `SELECT` (`polcmd IN ('*', 'r')`) has
    /// `USING (true)` **and** zero applicable `RESTRICTIVE` policies covering `SELECT` have a
    /// `USING` expression other than `'true'`.
    #[must_use]
    pub fn allows_unfiltered_export(&self) -> bool {
        !self.row_security_active
            || (self.has_unrestricted_permissive_select
                && self.blocking_restrictive_policies.is_empty())
    }

    /// Returns `true` when probing `{audit_schema}.changes` (for example `changes_default` row
    /// inspection in `verify`, `history_stats`, or `coverage`) is guaranteed both unfiltered and
    /// free of non-trivial applicable `PERMISSIVE` expressions (such as hostile `1 / 0 = 1` or
    /// cross-table `EXISTS (...)` policies targeting `PUBLIC` that PostgreSQL still rewrites
    /// alongside a `PERMISSIVE USING (true)` policy).
    #[must_use]
    pub fn safe_for_catalog_probe(&self) -> bool {
        !self.row_security_active
            || (self.has_unrestricted_permissive_select
                && !self.has_nontrivial_applicable_permissive_select
                && self.blocking_restrictive_policies.is_empty())
    }
}

/// Complete parent-table RLS catalog snapshot shared by verification, partition stats, and export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParentRlsInspection {
    pub(crate) status: ExportRlsStatus,
    pub(crate) has_any_permissive_select_policy: bool,
}

#[derive(Debug, FromRow)]
struct ParentRlsRow {
    oid: i64,
    relrowsecurity: bool,
    row_security_active: bool,
    current_role: String,
}

#[derive(Debug, Clone, FromRow)]
struct SelectPolicyRow {
    polname: String,
    polpermissive: bool,
    qual_expr: Option<String>,
    applies_to_current_user: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PolicySummary {
    has_any_permissive_select_policy: bool,
    has_unrestricted_permissive_select: bool,
    has_nontrivial_applicable_permissive_select: bool,
    applicable_permissive_select_policies: usize,
    blocking_restrictive_policies: Vec<String>,
}

fn record_select_policy(summary: &mut PolicySummary, row: &SelectPolicyRow) {
    let qual = row.qual_expr.as_deref();
    if !row.polpermissive {
        if row.applies_to_current_user && qual != Some("true") {
            summary
                .blocking_restrictive_policies
                .push(row.polname.clone());
        }
        return;
    }

    let Some(expr) = qual else {
        return;
    };
    summary.has_any_permissive_select_policy = true;
    if !row.applies_to_current_user {
        return;
    }
    summary.applicable_permissive_select_policies += 1;
    if expr == "true" {
        summary.has_unrestricted_permissive_select = true;
    } else {
        summary.has_nontrivial_applicable_permissive_select = true;
    }
}

fn classify_select_policies(rows: &[SelectPolicyRow]) -> PolicySummary {
    let mut summary = PolicySummary::default();
    for row in rows {
        record_select_policy(&mut summary, row);
    }
    summary
}

/// Inspects Row-Level Security posture on the parent `{audit_schema}.changes` table on an
/// existing PostgreSQL connection or transaction.
///
/// Always evaluates `pg_catalog.row_security_active(c.oid)` against the **parent** `changes`
/// relation (never a child partition) and checks `pg_catalog.pg_policy` applicability for
/// `current_user` using `0::oid` (`PUBLIC`) and `pg_catalog.pg_has_role(role_oid, 'USAGE')`
/// (which accurately reflects `INHERIT`/`NOINHERIT` and `SET ROLE` across PostgreSQL 15–19).
pub(crate) async fn inspect_parent_rls_on_conn(
    conn: &mut PgConnection,
    audit_schema: &str,
) -> Result<ParentRlsInspection, sqlx::Error> {
    let parent: Option<ParentRlsRow> = sqlx::query_as(
        "SELECT \
           c.oid::bigint AS oid, \
           c.relrowsecurity, \
           pg_catalog.row_security_active(c.oid) AS row_security_active, \
           current_user::text AS current_role \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
        WHERE n.nspname = $1 \
          AND c.relname = 'changes' \
          AND c.relkind IN ('r', 'p')",
    )
    .bind(audit_schema)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(parent) = parent else {
        return Ok(ParentRlsInspection {
            status: ExportRlsStatus {
                table_exists: false,
                row_security_active: false,
                has_unrestricted_permissive_select: false,
                has_nontrivial_applicable_permissive_select: false,
                applicable_permissive_select_policies: 0,
                blocking_restrictive_policies: Vec::new(),
                current_role: String::new(),
            },
            has_any_permissive_select_policy: false,
        });
    };

    let summary = if parent.relrowsecurity || parent.row_security_active {
        let rows: Vec<SelectPolicyRow> = sqlx::query_as(
            "SELECT \
               pol.polname::text AS polname, \
               pol.polpermissive, \
               pg_catalog.pg_get_expr(pol.polqual, pol.polrelid) AS qual_expr, \
               EXISTS ( \
                 SELECT 1 \
                   FROM pg_catalog.unnest(pol.polroles) AS r(role_oid) \
                  WHERE r.role_oid = 0::pg_catalog.oid \
                     OR pg_catalog.pg_has_role(r.role_oid, 'USAGE') \
               ) AS applies_to_current_user \
             FROM pg_catalog.pg_policy pol \
            WHERE pol.polrelid = $1::pg_catalog.oid \
              AND pol.polcmd IN ('*', 'r') \
            ORDER BY pol.polname ASC",
        )
        .bind(parent.oid)
        .fetch_all(&mut *conn)
        .await?;
        classify_select_policies(&rows)
    } else {
        PolicySummary::default()
    };

    Ok(ParentRlsInspection {
        status: ExportRlsStatus {
            table_exists: true,
            row_security_active: parent.row_security_active,
            has_unrestricted_permissive_select: summary.has_unrestricted_permissive_select,
            has_nontrivial_applicable_permissive_select: summary
                .has_nontrivial_applicable_permissive_select,
            applicable_permissive_select_policies: summary.applicable_permissive_select_policies,
            blocking_restrictive_policies: summary.blocking_restrictive_policies,
            current_role: parent.current_role,
        },
        has_any_permissive_select_policy: summary.has_any_permissive_select_policy,
    })
}

/// Formats an actionable fail-closed error message explaining why export aborted under RLS and
/// how to configure a dedicated `auditstream_exporter` role (compatible with managed Postgres
/// where non-superuser `BYPASSRLS` is disallowed) or opt into `--allow-rls-filtered`.
pub(crate) fn format_rls_blocked_message(audit_schema: &str, status: &ExportRlsStatus) -> String {
    let role = &status.current_role;
    let reason = if !status.blocking_restrictive_policies.is_empty() {
        let names = status.blocking_restrictive_policies.join(", ");
        if status.has_unrestricted_permissive_select {
            format!(
                "Row-Level Security is active on `{audit_schema}.changes` for role `{role}` and \
                 applicable RESTRICTIVE policy/policies ({names}) can filter rows even though a \
                 PERMISSIVE USING (true) SELECT policy exists"
            )
        } else {
            format!(
                "Row-Level Security is active on `{audit_schema}.changes` for role `{role}` \
                 without an applicable PERMISSIVE USING (true) SELECT policy and with blocking \
                 RESTRICTIVE policy/policies ({names})"
            )
        }
    } else if status.applicable_permissive_select_policies == 0 {
        format!(
            "Row-Level Security is active on `{audit_schema}.changes` for role `{role}` with no \
             applicable PERMISSIVE SELECT policy (reads would silently return 0 rows)"
        )
    } else {
        format!(
            "Row-Level Security is active on `{audit_schema}.changes` for role `{role}` without \
             an unobstructed PERMISSIVE USING (true) SELECT policy (reads may silently omit rows)"
        )
    };

    format!(
        "{reason}; refusing to export and advance the cursor to prevent silent audit event loss. \
         Create a dedicated `auditstream_exporter` role with a PERMISSIVE SELECT policy \
         `USING (true)` (managed PostgreSQL services such as RDS, Neon, and Supabase do not \
         allow non-superuser `BYPASSRLS`) and ensure no applicable RESTRICTIVE policy filters \
         that role, or pass `--allow-rls-filtered` if a per-tenant filtered export is intentional"
    )
}

/// Enforces the fail-closed RLS export guard on an existing PostgreSQL connection.
pub(crate) async fn ensure_export_rls_safe_on_conn(
    conn: &mut PgConnection,
    audit_schema: &str,
) -> Result<(), SourceError> {
    let inspection = inspect_parent_rls_on_conn(conn, audit_schema)
        .await
        .map_err(|err| SourceError::backend("failed to inspect Row-Level Security posture", err))?;

    if !inspection.status.allows_unfiltered_export() {
        return Err(SourceError::RlsFiltered(format_rls_blocked_message(
            audit_schema,
            &inspection.status,
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ExportRlsStatus, PolicySummary, SelectPolicyRow, classify_select_policies,
        format_rls_blocked_message,
    };

    fn policy(name: &str, permissive: bool, qual: Option<&str>, applies: bool) -> SelectPolicyRow {
        SelectPolicyRow {
            polname: name.to_owned(),
            polpermissive: permissive,
            qual_expr: qual.map(str::to_owned),
            applies_to_current_user: applies,
        }
    }

    #[test]
    fn classify_select_policies_handles_zero_filtered_restrictive_and_unrestricted_cases() {
        // 1. Zero policies -> neither any permissive nor unrestricted.
        assert_eq!(classify_select_policies(&[]), PolicySummary::default());

        // 2. Permissive policy with NULL USING (e.g. FOR ALL WITH CHECK only) does NOT cover SELECT.
        assert_eq!(
            classify_select_policies(&[policy("wc_only", true, None, true)]),
            PolicySummary::default()
        );

        // 3. Tenant-filtered permissive policy covers SELECT globally but is not unrestricted.
        let tenant_only = classify_select_policies(&[policy(
            "tenant_read",
            true,
            Some("((actor ->> 'tenant'::text) = current_setting('app.tenant'::text, true))"),
            true,
        )]);
        assert!(tenant_only.has_any_permissive_select_policy);
        assert!(!tenant_only.has_unrestricted_permissive_select);
        assert!(tenant_only.has_nontrivial_applicable_permissive_select);
        assert_eq!(tenant_only.applicable_permissive_select_policies, 1);
        assert_eq!(
            tenant_only.blocking_restrictive_policies,
            Vec::<String>::new()
        );

        // 4. Unrestricted permissive + non-true restrictive -> blocked by restrictive policy.
        let perm_plus_rest = classify_select_policies(&[
            policy("exporter_all", true, Some("true"), true),
            policy(
                "other_role_tenant",
                true,
                Some("(actor IS NOT NULL)"),
                false,
            ),
            policy("rest_other_role", false, Some("false"), false),
            policy("rest_true", false, Some("true"), true),
            policy("rest_tenant", false, Some("(actor IS NOT NULL)"), true),
            policy("rest_null_qual", false, None, true),
        ]);
        assert!(perm_plus_rest.has_any_permissive_select_policy);
        assert!(perm_plus_rest.has_unrestricted_permissive_select);
        assert!(!perm_plus_rest.has_nontrivial_applicable_permissive_select);
        assert_eq!(perm_plus_rest.applicable_permissive_select_policies, 1);
        assert_eq!(
            perm_plus_rest.blocking_restrictive_policies,
            vec!["rest_tenant".to_owned(), "rest_null_qual".to_owned()]
        );

        // 5. Unrestricted permissive + sibling non-trivial applicable permissive (e.g. TO PUBLIC)
        //    -> allows_unfiltered_export stays true, but safe_for_catalog_probe flags it!
        let sibling_perm = classify_select_policies(&[
            policy("exporter_all", true, Some("true"), true),
            policy("public_trap", true, Some("((1 / 0) = 1)"), true),
        ]);
        assert!(sibling_perm.has_unrestricted_permissive_select);
        assert!(sibling_perm.has_nontrivial_applicable_permissive_select);
        assert_eq!(sibling_perm.applicable_permissive_select_policies, 2);

        // 6. Unrestricted permissive + restrictive USING (true) + non-applicable restrictive -> clean.
        let clean = classify_select_policies(&[
            policy("exporter_all", true, Some("true"), true),
            policy("rest_true", false, Some("true"), true),
            policy("rest_other_role", false, Some("false"), false),
        ]);
        assert!(clean.has_unrestricted_permissive_select);
        assert!(!clean.has_nontrivial_applicable_permissive_select);
        assert_eq!(clean.blocking_restrictive_policies, Vec::<String>::new());
    }

    #[test]
    fn allows_unfiltered_export_probe_safety_and_error_messages_cover_all_guard_states() {
        let inactive = ExportRlsStatus {
            table_exists: true,
            row_security_active: false,
            has_unrestricted_permissive_select: false,
            has_nontrivial_applicable_permissive_select: true,
            applicable_permissive_select_policies: 0,
            blocking_restrictive_policies: vec!["rest_x".to_owned()],
            current_role: "postgres".to_owned(),
        };
        assert!(inactive.allows_unfiltered_export());
        assert!(inactive.safe_for_catalog_probe());

        let allowed = ExportRlsStatus {
            table_exists: true,
            row_security_active: true,
            has_unrestricted_permissive_select: true,
            has_nontrivial_applicable_permissive_select: false,
            applicable_permissive_select_policies: 1,
            blocking_restrictive_policies: Vec::new(),
            current_role: "auditstream_exporter".to_owned(),
        };
        assert!(allowed.allows_unfiltered_export());
        assert!(allowed.safe_for_catalog_probe());

        let sibling_nontrivial = ExportRlsStatus {
            has_nontrivial_applicable_permissive_select: true,
            applicable_permissive_select_policies: 2,
            ..allowed
        };
        assert!(sibling_nontrivial.allows_unfiltered_export());
        assert!(!sibling_nontrivial.safe_for_catalog_probe());

        let zero_pols = ExportRlsStatus {
            table_exists: true,
            row_security_active: true,
            has_unrestricted_permissive_select: false,
            has_nontrivial_applicable_permissive_select: false,
            applicable_permissive_select_policies: 0,
            blocking_restrictive_policies: Vec::new(),
            current_role: "app_user".to_owned(),
        };
        assert!(!zero_pols.allows_unfiltered_export());
        assert!(!zero_pols.safe_for_catalog_probe());
        let msg_zero = format_rls_blocked_message("auditstream", &zero_pols);
        assert!(msg_zero.contains("no applicable PERMISSIVE SELECT policy"));
        assert!(msg_zero.contains("auditstream_exporter"));
        assert!(msg_zero.contains("BYPASSRLS"));
        assert!(msg_zero.contains("--allow-rls-filtered"));

        let filtered = ExportRlsStatus {
            applicable_permissive_select_policies: 1,
            has_nontrivial_applicable_permissive_select: true,
            ..zero_pols.clone()
        };
        assert!(!filtered.allows_unfiltered_export());
        let msg_filtered = format_rls_blocked_message("auditstream", &filtered);
        assert!(msg_filtered.contains("without an unobstructed PERMISSIVE USING (true)"));

        let perm_plus_rest = ExportRlsStatus {
            has_unrestricted_permissive_select: true,
            applicable_permissive_select_policies: 1,
            blocking_restrictive_policies: vec!["tenant_guard".to_owned()],
            ..zero_pols.clone()
        };
        assert!(!perm_plus_rest.allows_unfiltered_export());
        let msg_perm_rest = format_rls_blocked_message("auditstream", &perm_plus_rest);
        assert!(msg_perm_rest.contains("tenant_guard"));
        assert!(msg_perm_rest.contains("even though a PERMISSIVE USING (true)"));

        let rest_without_perm = ExportRlsStatus {
            has_unrestricted_permissive_select: false,
            applicable_permissive_select_policies: 0,
            blocking_restrictive_policies: vec!["tenant_guard".to_owned()],
            ..zero_pols
        };
        let msg_rest_no_perm = format_rls_blocked_message("auditstream", &rest_without_perm);
        assert!(msg_rest_no_perm.contains("without an applicable PERMISSIVE USING (true)"));
        assert!(msg_rest_no_perm.contains("tenant_guard"));
    }
}
