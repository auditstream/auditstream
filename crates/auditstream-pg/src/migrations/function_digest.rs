use sha2::{Digest, Sha256};

pub(super) fn from_statements(statements: &[String]) -> String {
    let mut digest = Sha256::new();
    for sql in statements {
        if sql.starts_with("CREATE OR REPLACE FUNCTION ") {
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

    #[test]
    fn changing_any_generated_function_body_changes_the_digest() {
        let mut statements = ddl::generate(&ddl::DdlOptions::default());
        statements.extend(triggers::generate(&triggers::TriggerOptions::default()));
        let original = from_statements(&statements);

        for function in ["resolve_actor", "capture", "log_event"] {
            let mut changed = statements.clone();
            let signature = format!(r#"CREATE OR REPLACE FUNCTION "auditstream".{function}("#);
            let sql = changed
                .iter_mut()
                .find(|sql| sql.starts_with(&signature))
                .expect("generated function");
            *sql = sql.replacen("AS $fn$", "AS $fn$\n-- revised function body", 1);
            assert_ne!(original, from_statements(&changed), "{function}");
        }
    }
}
