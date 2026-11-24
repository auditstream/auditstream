//! Actor transport codec for trailing SQL comments and transaction-local `set_config`.
//!
//! Implements Auditstream Specification `0.1.0` (`spec/transport.md`).

pub mod comment;
pub mod set_config;

use serde_json::{Map, Value};

use crate::ActorSource;

pub use crate::transport::comment::{
    DecodedActor, MAX_ENCODED_COMMENT_CHARS, decode, encode, encode_with_context, tag,
    tag_with_context,
};
pub use crate::transport::set_config::{
    SET_CONFIG_SQL, SetConfigStatement, set_config, set_config_with_context,
};

/// Result of applying the full `spec/transport.md` precedence order across
/// `set_config`, trailing SQL comment, and `request.jwt.claims`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedActor {
    /// Decoded actor JSON object, or `None` when absent or malformed.
    pub actor: Option<Value>,
    /// Decoded context JSON object, or `None` when absent or malformed.
    pub context: Option<Value>,
    /// Resolution source (`set_config`, `comment`, `jwt`, `invalid`, or `None`).
    pub actor_source: Option<ActorSource>,
}

/// Resolves an actor following the strict precedence rules in `spec/transport.md`:
///
/// 1. `set_config` (`auditstream.actor`, empty string treated as unset)
/// 2. Trailing SQL comment (`/*auditstream='...'*/`)
/// 3. JWT claims (`request.jwt.claims` `sub`)
/// 4. None
///
/// Malformed `set_config` or malformed `auditstream` comments yield `ActorSource::Invalid`
/// and NEVER fall through to lower-priority sources.
pub fn resolve_precedence(
    sql: &str,
    set_config_raw: Option<&str>,
    jwt_claims_raw: Option<&str>,
) -> ResolvedActor {
    if let Some(setting) = set_config_raw.filter(|s| !s.is_empty()) {
        return match comment::parse_trigger_actor_json(setting) {
            DecodedActor::Valid { actor, context } => ResolvedActor {
                actor: Some(actor),
                context,
                actor_source: Some(ActorSource::SetConfig),
            },
            DecodedActor::Invalid | DecodedActor::Absent => ResolvedActor {
                actor: None,
                context: None,
                actor_source: Some(ActorSource::Invalid),
            },
        };
    }

    match decode(sql) {
        DecodedActor::Valid { actor, context } => {
            return ResolvedActor {
                actor: Some(actor),
                context,
                actor_source: Some(ActorSource::Comment),
            };
        }
        DecodedActor::Invalid => {
            return ResolvedActor {
                actor: None,
                context: None,
                actor_source: Some(ActorSource::Invalid),
            };
        }
        DecodedActor::Absent => {}
    }

    if let Some(claims_str) = jwt_claims_raw.filter(|s| !s.is_empty()) {
        let Ok(Value::Object(claims)) = serde_json::from_str::<Value>(claims_str) else {
            return ResolvedActor {
                actor: None,
                context: None,
                actor_source: Some(ActorSource::Invalid),
            };
        };
        if let Some(sub) = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let mut actor = Map::new();
            actor.insert("type".to_owned(), Value::String("user".to_owned()));
            actor.insert("id".to_owned(), Value::String(sub.to_owned()));
            return ResolvedActor {
                actor: Some(Value::Object(actor)),
                context: None,
                actor_source: Some(ActorSource::Jwt),
            };
        }
    }

    ResolvedActor {
        actor: None,
        context: None,
        actor_source: None,
    }
}
