//! Actor types and 512-byte payload validation from `spec/actor.md`.

use std::fmt;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{Context, CoreError};

/// Maximum UTF-8 byte length of the compact serialized actor + optional context JSON payload.
pub const MAX_PAYLOAD_BYTES: usize = 512;

/// Allowed actor categories defined by Auditstream Specification 0.1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    /// Human end user.
    User,
    /// API key or personal access token.
    ApiKey,
    /// Autonomous or assisted AI agent.
    AiAgent,
    /// Internal service or background worker.
    Service,
    /// Automated system process (such as migrations or scheduled jobs).
    System,
}

/// Nested actor inside [`Actor::on_behalf_of`].
///
/// Nesting is strictly one level deep per `spec/actor.md`: deserializing a nested
/// actor that carries its own `onBehalfOf` key fails with an error, while other
/// unknown keys are ignored for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct NestedActor {
    /// Actor category.
    #[serde(rename = "type")]
    pub actor_type: ActorType,

    /// Non-empty opaque identifier for the nested actor.
    pub id: String,

    /// Optional non-empty opaque tenant or organization identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

impl NestedActor {
    /// Creates a new [`NestedActor`] with the required category and identifier.
    pub fn new(actor_type: ActorType, id: impl Into<String>) -> Self {
        Self {
            actor_type,
            id: id.into(),
            tenant: None,
        }
    }

    /// Validates field invariants on this nested actor.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::EmptyActorId`] or [`CoreError::EmptyTenant`] if invalid.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.id.is_empty() {
            return Err(CoreError::EmptyActorId);
        }
        if self.tenant.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::EmptyTenant);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for NestedActor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NestedActorVisitor;

        impl<'de> Visitor<'de> for NestedActorVisitor {
            type Value = NestedActor;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a nested Auditstream actor JSON object without onBehalfOf")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut actor_type: Option<ActorType> = None;
                let mut id: Option<String> = None;
                let mut tenant: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => {
                            if actor_type.is_some() {
                                return Err(de::Error::duplicate_field("type"));
                            }
                            actor_type = Some(map.next_value()?);
                        }
                        "id" => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom("onBehalfOf.id must not be empty"));
                            }
                            id = Some(value);
                        }
                        "tenant" => {
                            if tenant.is_some() {
                                return Err(de::Error::duplicate_field("tenant"));
                            }
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom(
                                    "onBehalfOf.tenant must not be empty",
                                ));
                            }
                            tenant = Some(value);
                        }
                        "onBehalfOf" => {
                            let _: IgnoredAny = map.next_value()?;
                            return Err(de::Error::custom(
                                "onBehalfOf cannot be nested more than one level deep",
                            ));
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let actor_type = actor_type.ok_or_else(|| de::Error::missing_field("type"))?;
                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;

                Ok(NestedActor {
                    actor_type,
                    id,
                    tenant,
                })
            }
        }

        deserializer.deserialize_map(NestedActorVisitor)
    }
}

/// Primary actor payload defined by Auditstream Specification 0.1.0.
///
/// Serializes keys in the canonical order of `spec/transport.md`:
/// `type`, `id`, `onBehalfOf`, `tenant`.
///
/// # Examples
///
/// ```
/// use auditstream_core::{Actor, ActorType, validate_actor_payload};
///
/// let actor = Actor::new(ActorType::User, "usr_42");
/// let json = validate_actor_payload(&actor, None).expect("valid payload");
/// assert_eq!(json, r#"{"type":"user","id":"usr_42"}"#);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Actor {
    /// Actor category (`user`, `api_key`, `ai_agent`, `service`, or `system`).
    #[serde(rename = "type")]
    pub actor_type: ActorType,

    /// Non-empty opaque identifier for the actor.
    pub id: String,

    /// Optional nested actor on whose behalf this actor acted (max depth 1).
    #[serde(rename = "onBehalfOf", skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<NestedActor>,

    /// Optional non-empty opaque tenant or organization identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

impl Actor {
    /// Creates a new [`Actor`] with the required category and identifier.
    pub fn new(actor_type: ActorType, id: impl Into<String>) -> Self {
        Self {
            actor_type,
            id: id.into(),
            on_behalf_of: None,
            tenant: None,
        }
    }

    /// Validates all field invariants on this actor (excluding the 512-byte serialized size limit).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if `id` is empty, `tenant` is empty, or `on_behalf_of` is invalid.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.id.is_empty() {
            return Err(CoreError::EmptyActorId);
        }
        if self.tenant.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::EmptyTenant);
        }
        if let Some(nested) = &self.on_behalf_of {
            nested.validate()?;
        }
        Ok(())
    }

    /// Validates this actor together with optional [`Context`] and checks the 512-byte UTF-8 JSON limit.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if any field is invalid or the compact UTF-8 JSON exceeds 512 bytes.
    pub fn validate_with_context(&self, context: Option<&Context>) -> Result<String, CoreError> {
        validate_actor_payload(self, context)
    }
}

impl<'de> Deserialize<'de> for Actor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ActorVisitor;

        impl<'de> Visitor<'de> for ActorVisitor {
            type Value = Actor;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an Auditstream actor JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut actor_type: Option<ActorType> = None;
                let mut id: Option<String> = None;
                let mut on_behalf_of: Option<NestedActor> = None;
                let mut tenant: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => {
                            if actor_type.is_some() {
                                return Err(de::Error::duplicate_field("type"));
                            }
                            actor_type = Some(map.next_value()?);
                        }
                        "id" => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom("actor.id must not be empty"));
                            }
                            id = Some(value);
                        }
                        "onBehalfOf" => {
                            if on_behalf_of.is_some() {
                                return Err(de::Error::duplicate_field("onBehalfOf"));
                            }
                            on_behalf_of = Some(map.next_value()?);
                        }
                        "tenant" => {
                            if tenant.is_some() {
                                return Err(de::Error::duplicate_field("tenant"));
                            }
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom("actor.tenant must not be empty"));
                            }
                            tenant = Some(value);
                        }
                        // Unknown keys (including sibling "context") are ignored on Actor itself.
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let actor_type = actor_type.ok_or_else(|| de::Error::missing_field("type"))?;
                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;

                Ok(Actor {
                    actor_type,
                    id,
                    on_behalf_of,
                    tenant,
                })
            }
        }

        deserializer.deserialize_map(ActorVisitor)
    }
}

/// Complete wire payload combining an [`Actor`] and optional [`Context`] in canonical key order.
///
/// Serializes without `#[serde(flatten)]` so it works cleanly with `serde_json`'s
/// `arbitrary_precision` feature enabled across the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ActorPayload {
    /// Actor category.
    #[serde(rename = "type")]
    pub actor_type: ActorType,

    /// Non-empty opaque identifier for the actor.
    pub id: String,

    /// Optional nested actor on whose behalf this actor acted (max depth 1).
    #[serde(rename = "onBehalfOf", skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<NestedActor>,

    /// Optional non-empty opaque tenant or organization identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// Optional execution context carried under the `context` key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Context>,
}

impl ActorPayload {
    /// Builds an [`ActorPayload`] from an [`Actor`] and optional [`Context`].
    pub fn new(actor: Actor, context: Option<Context>) -> Self {
        Self {
            actor_type: actor.actor_type,
            id: actor.id,
            on_behalf_of: actor.on_behalf_of,
            tenant: actor.tenant,
            context,
        }
    }

    /// Splits this payload into its [`Actor`] and optional [`Context`].
    pub fn into_parts(self) -> (Actor, Option<Context>) {
        (
            Actor {
                actor_type: self.actor_type,
                id: self.id,
                on_behalf_of: self.on_behalf_of,
                tenant: self.tenant,
            },
            self.context,
        )
    }

    /// Validates all fields and the 512-byte UTF-8 compact JSON size limit.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if any field is invalid or the serialized JSON exceeds 512 bytes.
    pub fn validate(&self) -> Result<String, CoreError> {
        let actor = Actor {
            actor_type: self.actor_type,
            id: self.id.clone(),
            on_behalf_of: self.on_behalf_of.clone(),
            tenant: self.tenant.clone(),
        };
        validate_actor_payload(&actor, self.context.as_ref())
    }
}

impl<'de> Deserialize<'de> for ActorPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ActorPayloadVisitor;

        impl<'de> Visitor<'de> for ActorPayloadVisitor {
            type Value = ActorPayload;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an Auditstream actor and optional context JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut actor_type: Option<ActorType> = None;
                let mut id: Option<String> = None;
                let mut on_behalf_of: Option<NestedActor> = None;
                let mut tenant: Option<String> = None;
                let mut context: Option<Context> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => {
                            if actor_type.is_some() {
                                return Err(de::Error::duplicate_field("type"));
                            }
                            actor_type = Some(map.next_value()?);
                        }
                        "id" => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom("actor.id must not be empty"));
                            }
                            id = Some(value);
                        }
                        "onBehalfOf" => {
                            if on_behalf_of.is_some() {
                                return Err(de::Error::duplicate_field("onBehalfOf"));
                            }
                            on_behalf_of = Some(map.next_value()?);
                        }
                        "tenant" => {
                            if tenant.is_some() {
                                return Err(de::Error::duplicate_field("tenant"));
                            }
                            let value: String = map.next_value()?;
                            if value.is_empty() {
                                return Err(de::Error::custom("actor.tenant must not be empty"));
                            }
                            tenant = Some(value);
                        }
                        "context" => {
                            if context.is_some() {
                                return Err(de::Error::duplicate_field("context"));
                            }
                            context = Some(map.next_value()?);
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let actor_type = actor_type.ok_or_else(|| de::Error::missing_field("type"))?;
                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;

                Ok(ActorPayload {
                    actor_type,
                    id,
                    on_behalf_of,
                    tenant,
                    context,
                })
            }
        }

        deserializer.deserialize_map(ActorPayloadVisitor)
    }
}

#[derive(Serialize)]
struct CanonicalPayloadRef<'a> {
    #[serde(rename = "type")]
    actor_type: ActorType,
    id: &'a str,
    #[serde(rename = "onBehalfOf", skip_serializing_if = "Option::is_none")]
    on_behalf_of: Option<&'a NestedActor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a Context>,
}

/// Validates an [`Actor`] and optional [`Context`] against `spec/actor.md` and returns
/// its canonical compact UTF-8 JSON representation if its byte length is at most 512 bytes.
///
/// # Errors
///
/// Returns [`CoreError`] if any actor/context field is invalid or if the compact UTF-8
/// JSON representation exceeds [`MAX_PAYLOAD_BYTES`] (512 bytes).
pub fn validate_actor_payload(
    actor: &Actor,
    context: Option<&Context>,
) -> Result<String, CoreError> {
    actor.validate()?;
    if let Some(ctx) = context {
        ctx.validate()?;
    }

    let wire = CanonicalPayloadRef {
        actor_type: actor.actor_type,
        id: &actor.id,
        on_behalf_of: actor.on_behalf_of.as_ref(),
        tenant: actor.tenant.as_deref(),
        context,
    };

    let json = serde_json::to_string(&wire)?;
    if json.len() > MAX_PAYLOAD_BYTES {
        return Err(CoreError::PayloadTooLarge {
            size: json.len(),
            limit: MAX_PAYLOAD_BYTES,
        });
    }
    Ok(json)
}
