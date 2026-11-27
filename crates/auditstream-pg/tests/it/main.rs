//! Integration test entrypoint for `auditstream-pg`.

mod capture;
mod ddl;
mod introspect;
mod migrations;
mod partitions;
mod reader;
mod tls;
mod triggers;
mod verify;

/// Returns whether the connected PostgreSQL server supports disabling
/// `standard_conforming_strings` (removed in PostgreSQL 19 with SQLSTATE `0A000`).
pub(crate) async fn supports_non_standard_strings(conn: &mut sqlx::postgres::PgConnection) -> bool {
    let version_num: i32 = sqlx::query_scalar::<_, String>("SHOW server_version_num")
        .fetch_one(&mut *conn)
        .await
        .expect("read server_version_num")
        .parse()
        .expect("parse server_version_num");
    match sqlx::Executor::execute(&mut *conn, "SET standard_conforming_strings = off").await {
        Ok(_) => {
            sqlx::Executor::execute(&mut *conn, "RESET standard_conforming_strings")
                .await
                .expect("reset standard_conforming_strings");
            true
        }
        Err(err)
            if version_num >= 190_000
                && err
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::code)
                    .as_deref()
                    == Some("0A000") =>
        {
            false
        }
        Err(err) => panic!(
            "disable standard_conforming_strings failed unexpectedly on PG {version_num}: {err}"
        ),
    }
}
