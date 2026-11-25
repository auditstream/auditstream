use sha2::{Digest, Sha256};

/// Hashes the generated function and trigger definitions, which `verify` compares with the
/// catalog, so changing either produces a refresh migration. Other statements are skipped:
/// they are idempotent or depend on the previous selection (`DROP TRIGGER` for removed tables).
pub(super) fn from_statements(statements: &[String]) -> String {
    let mut digest = Sha256::new();
    for sql in statements {
        if sql.starts_with("CREATE OR REPLACE FUNCTION ")
            || sql.starts_with("CREATE OR REPLACE TRIGGER ")
        {
            digest.update(sql.as_bytes());
            // NUL separates definitions and cannot occur in a PostgreSQL statement.
            digest.update([0]);
        }
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
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
        let mut statements = ddl::generate(&ddl::DdlOptions::default());
        statements.extend(triggers::generate(&triggers::TriggerOptions::new(
            ddl::DEFAULT_SCHEMA,
            vec![table],
        )));
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
    }
}
