//! Execution context attached to an [`Actor`](crate::Actor).

use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::CoreError;
use crate::actor::NoNulIgnoredAny;

/// Optional request or execution context carried alongside an actor payload.
///
/// Field order follows the canonical order defined in `spec/transport.md`:
/// `requestId`, `source`, `ip`, `userAgent`.
///
/// `ip` and `user_agent` are opt-in only per `spec/actor.md` and should not be
/// populated unless explicitly enabled by the application.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Context {
    /// Non-empty correlation identifier for the originating request or job.
    #[serde(rename = "requestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,

    /// Non-empty label for the entry surface (for example `web`, `api`, `cli`, `worker`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Opt-in client IP address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,

    /// Opt-in client user-agent string.
    #[serde(rename = "userAgent", skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

impl Context {
    /// Validates that every present context field is a non-empty string without NUL characters.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::EmptyContextField`] if any present field is empty or
    /// [`CoreError::NullByteInField`] if any present field contains `U+0000`.
    pub fn validate(&self) -> Result<(), CoreError> {
        for (value, field, path) in [
            (self.request_id.as_deref(), "requestId", "context.requestId"),
            (self.source.as_deref(), "source", "context.source"),
            (self.ip.as_deref(), "ip", "context.ip"),
            (self.user_agent.as_deref(), "userAgent", "context.userAgent"),
        ] {
            if let Some(val) = value {
                if val.is_empty() {
                    return Err(CoreError::EmptyContextField { field });
                }
                if val.contains('\0') {
                    return Err(CoreError::NullByteInField { field: path });
                }
            }
        }
        Ok(())
    }

    /// Returns `true` if no context fields are populated.
    pub fn is_empty(&self) -> bool {
        self.request_id.is_none()
            && self.source.is_none()
            && self.ip.is_none()
            && self.user_agent.is_none()
    }
}

impl<'de> Deserialize<'de> for Context {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ContextVisitor;

        impl<'de> Visitor<'de> for ContextVisitor {
            type Value = Context;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an Auditstream context JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut request_id: Option<String> = None;
                let mut source: Option<String> = None;
                let mut ip: Option<String> = None;
                let mut user_agent: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    if key.contains('\0') {
                        return Err(de::Error::custom(
                            "context object keys must not contain NUL characters",
                        ));
                    }
                    match key.as_str() {
                        "requestId" => parse_unique_context_field(
                            &mut request_id,
                            "requestId",
                            "context.requestId must not be empty",
                            "context.requestId must not contain NUL characters",
                            &mut map,
                        )?,
                        "source" => parse_unique_context_field(
                            &mut source,
                            "source",
                            "context.source must not be empty",
                            "context.source must not contain NUL characters",
                            &mut map,
                        )?,
                        "ip" => parse_unique_context_field(
                            &mut ip,
                            "ip",
                            "context.ip must not be empty",
                            "context.ip must not contain NUL characters",
                            &mut map,
                        )?,
                        "userAgent" => parse_unique_context_field(
                            &mut user_agent,
                            "userAgent",
                            "context.userAgent must not be empty",
                            "context.userAgent must not contain NUL characters",
                            &mut map,
                        )?,
                        // Readers MUST ignore unknown keys for forward compatibility (spec/actor.md),
                        // while still rejecting NUL characters inside nested strings and keys.
                        _ => {
                            let _: NoNulIgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(Context {
                    request_id,
                    source,
                    ip,
                    user_agent,
                })
            }
        }

        deserializer.deserialize_map(ContextVisitor)
    }
}

fn parse_unique_context_field<'de, A>(
    slot: &mut Option<String>,
    field: &'static str,
    empty_msg: &'static str,
    nul_msg: &'static str,
    map: &mut A,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
{
    if slot.is_some() {
        return Err(de::Error::duplicate_field(field));
    }
    let value: String = map.next_value()?;
    if value.is_empty() {
        return Err(de::Error::custom(empty_msg));
    }
    if value.contains('\0') {
        return Err(de::Error::custom(nul_msg));
    }
    *slot = Some(value);
    Ok(())
}
