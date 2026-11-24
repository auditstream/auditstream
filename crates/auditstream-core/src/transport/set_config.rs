//! Transaction-local `set_config` actor transport (`spec/transport.md`).

use crate::{Actor, Context, CoreError, validate_actor_payload};

/// Parameterized SQL statement setting the transaction-local `auditstream.actor` GUC.
///
/// The actor JSON MUST ALWAYS be passed as the bound parameter `$1` and never interpolated
/// into SQL text. The third argument (`true`) scopes the setting strictly to the current
/// transaction so poolers never leak actors across requests.
pub const SET_CONFIG_SQL: &str = "SELECT set_config('auditstream.actor', $1, true)";

/// Prepared `set_config` statement and its bound JSON parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetConfigStatement {
    /// Fixed SQL text (`SELECT set_config('auditstream.actor', $1, true)`).
    pub sql: &'static str,
    /// Validated canonical compact UTF-8 JSON bound as `$1` (`<= 512` UTF-8 bytes).
    pub param: String,
}

/// Builds a parameterized `SELECT set_config('auditstream.actor', $1, true)` statement for `actor`.
///
/// # Errors
///
/// Returns [`CoreError`] if `actor` is invalid or exceeds 512 UTF-8 bytes.
pub fn set_config(actor: &Actor) -> Result<SetConfigStatement, CoreError> {
    set_config_with_context(actor, None)
}

/// Builds a parameterized `SELECT set_config('auditstream.actor', $1, true)` statement
/// for `actor` and optional `context`.
///
/// # Errors
///
/// Returns [`CoreError`] if the payload is invalid or exceeds 512 UTF-8 bytes.
pub fn set_config_with_context(
    actor: &Actor,
    context: Option<&Context>,
) -> Result<SetConfigStatement, CoreError> {
    let param = validate_actor_payload(actor, context)?;
    Ok(SetConfigStatement {
        sql: SET_CONFIG_SQL,
        param,
    })
}
