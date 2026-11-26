use sha2::{Digest, Sha256};

/// Hashes all deterministic generated DDL, ACL, function, and trigger statements so any engine
/// security or capture change produces a refresh migration on the next `init` run. Only
/// selection-history-dependent `DROP TRIGGER IF EXISTS` statements are skipped so removing a
/// table once does not invalidate subsequent no-op checks.
pub(super) fn from_statements(statements: &[String]) -> String {
    let mut digest = Sha256::new();
    for sql in statements {
        if !sql.starts_with("DROP TRIGGER IF EXISTS ") {
            digest.update(sql.as_bytes());
            // NUL separates statements and cannot occur in a PostgreSQL statement.
            digest.update([0]);
        }
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::super::install_statements;
    use super::*;
    use crate::{ddl, triggers};

    fn digest_after_edit(statements: &[String], prefix: &str, from: &str, to: &str) -> String {
        let mut edited = statements.to_vec();
        let sql = edited
            .iter_mut()
            .find(|sql| sql.starts_with(prefix))
            .expect(prefix);
        *sql = sql.replacen(from, to, 1);
        from_statements(&edited)
    }

    #[test]
    fn changing_any_generated_function_or_trigger_changes_the_digest() {
        let table = triggers::TableTriggerSpec::new("public", "invoice", vec!["id".into()], vec![]);
        let statements =
            install_statements(&ddl::DdlOptions::default(), &[table]).expect("install statements");
        let original = from_statements(&statements);

        for function in ["resolve_actor", "capture", "log_event"] {
            let prefix = format!(r#"CREATE OR REPLACE FUNCTION "auditstream".{function}("#);
            let revised = digest_after_edit(&statements, &prefix, "AS $fn$", "AS $fn$\n-- revised");
            assert_ne!(original, revised, "{function}");
        }
        let trigger = "CREATE OR REPLACE TRIGGER auditstream_update";
        let when = "FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)";
        let revised = digest_after_edit(&statements, trigger, "FOR EACH ROW", when);
        assert_ne!(original, revised, "{trigger}");

        // Editing a DDL/ACL statement must change the digest so security upgrades refresh.
        let ddl_revised = digest_after_edit(
            &statements,
            "CREATE SCHEMA IF NOT EXISTS ",
            "AUTHORIZATION",
            "AUTHORIZATION /* changed */",
        );
        assert_ne!(original, ddl_revised, "DDL/ACL edit must change digest");

        // Selection-history-dependent DROP TRIGGER IF EXISTS statements are excluded.
        let mut with_drop = statements.clone();
        with_drop.push("DROP TRIGGER IF EXISTS auditstream_insert ON \"public\".\"old\"".into());
        assert_eq!(
            original,
            from_statements(&with_drop),
            "DROP TRIGGER IF EXISTS must not perturb digest"
        );
    }
}
