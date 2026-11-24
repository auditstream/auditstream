//! Execution context attached to an [`Actor`](crate::Actor).

use std::fmt;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::CoreError;

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
    /// Validates that every present context field is a non-empty string.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::EmptyContextField`] if any present field is empty.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.request_id.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::EmptyContextField { field: "requestId" });
        }
        if self.source.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::EmptyContextField { field: "source" });
        }
        if self.ip.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::EmptyContextField { field: "ip" });
        }
        if self.user_agent.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::EmptyContextField { field: "userAgent" });
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
                    match key.as_str() {
                        "requestId" => {
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom(
                                    "context.requestId must not be empty",
                                ));
                            }
                            request_id = Some(value);
                        }
                        "source" => {
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom("context.source must not be empty"));
                            }
                            source = Some(value);
                        }
                        "ip" => {
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom("context.ip must not be empty"));
                            }
                            ip = Some(value);
                        }
                        "userAgent" => {
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom(
                                    "context.userAgent must not be empty",
                                ));
                            }
                            user_agent = Some(value);
                        }
                        // Readers MUST ignore unknown keys for forward compatibility (spec/actor.md).
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
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
