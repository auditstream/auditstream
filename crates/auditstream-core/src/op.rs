//! Operation, event source, and actor resolution source enums from spec 0.1.0.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::CoreError;

/// Maximum character length of an application event action (`spec/history-table.md`).
pub const MAX_ACTION_CHARS: usize = 64;

/// Origin of an audit event row (`auditstream.changes.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Captured automatically by a database trigger.
    Db,
    /// Recorded explicitly via `auditstream.log_event()`.
    App,
}

/// Mechanism that produced the resolved actor (`auditstream.changes.actor_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorSource {
    /// Resolved from transaction-local `set_config('auditstream.actor', ..., true)`.
    SetConfig,
    /// Resolved from a trailing `/*auditstream='...'*/` SQL comment.
    Comment,
    /// Resolved from `request.jwt.claims` `sub`.
    Jwt,
    /// Present input was malformed, oversized, or failed decoding.
    Invalid,
}

/// Recorded operation (`auditstream.changes.op`).
///
/// Database DML operations map to [`Op::Insert`], [`Op::Update`], [`Op::Delete`], and
/// [`Op::Truncate`]; any other string represents an application event action recorded via
/// `auditstream.log_event()`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Op {
    /// Database `INSERT`.
    Insert,
    /// Database `UPDATE`.
    Update,
    /// Database `DELETE`.
    Delete,
    /// Database `TRUNCATE`.
    Truncate,
    /// Application event action name (for example `invoice.exported`).
    App(String),
}

impl Op {
    /// Constructs a validated application event action [`Op::App`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if `action` is blank, exceeds 64 Unicode scalar values,
    /// or collides (case-insensitively) with `INSERT`, `UPDATE`, `DELETE`, or `TRUNCATE`.
    pub fn app(action: impl Into<String>) -> Result<Self, CoreError> {
        let action = action.into();
        if action.trim().is_empty() {
            return Err(CoreError::EmptyAction);
        }
        let char_len = action.chars().count();
        if char_len > MAX_ACTION_CHARS {
            return Err(CoreError::ActionTooLong {
                len: char_len,
                max: MAX_ACTION_CHARS,
            });
        }
        if matches!(
            action.trim().to_ascii_uppercase().as_str(),
            "INSERT" | "UPDATE" | "DELETE" | "TRUNCATE"
        ) {
            return Err(CoreError::ReservedAction { action });
        }
        Ok(Self::App(action))
    }

    /// Returns the wire string representation of this operation.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
            Self::App(action) => action.as_str(),
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Op {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "INSERT" => Ok(Self::Insert),
            "UPDATE" => Ok(Self::Update),
            "DELETE" => Ok(Self::Delete),
            "TRUNCATE" => Ok(Self::Truncate),
            other => Self::app(other),
        }
    }
}

impl Serialize for Op {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Op {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OpVisitor;

        impl Visitor<'_> for OpVisitor {
            type Value = Op;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a non-empty operation string (1..=64 characters)")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Op::from_str(v).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(OpVisitor)
    }
}
