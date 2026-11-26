//! Trailing SQL comment transport encoding, tagging, and decoding (`spec/transport.md`).

use serde_json::Value;

use crate::{Actor, Context, CoreError, MAX_PAYLOAD_BYTES, validate_actor_payload};

/// Maximum unpadded base64url character count (`ceil(512 * 4 / 3)`).
pub const MAX_ENCODED_COMMENT_CHARS: usize = 683;

/// Number of trailing Unicode scalar values scanned by the SQL resolver (`right(current_query(), 4096)`).
const TAIL_WINDOW_CHARS: usize = 4096;

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Outcome of decoding a trailing `/*auditstream='...'*/` SQL comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedActor {
    /// No trailing comment or no `auditstream` key was present.
    Absent,
    /// An `auditstream` key was present, but failed alphabet, length, UTF-8, JSON, or field checks.
    Invalid,
    /// A valid actor payload (and optional context object) was extracted.
    Valid {
        /// Decoded actor JSON object (without the `context` key).
        actor: Value,
        /// Optional decoded `context` JSON object extracted from the payload.
        context: Option<Value>,
    },
}

impl DecodedActor {
    /// Parses the decoded actor JSON into a typed [`Actor`], returning `None` when not [`DecodedActor::Valid`].
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if the stored JSON does not satisfy the full [`Actor`] schema.
    pub fn into_actor(self) -> Result<Option<Actor>, serde_json::Error> {
        match self {
            Self::Absent | Self::Invalid => Ok(None),
            Self::Valid { actor, .. } => serde_json::from_value(actor).map(Some),
        }
    }
}

/// Encodes `actor` (with no context) into the unpadded base64url string defined in `spec/transport.md`.
///
/// # Errors
///
/// Returns [`CoreError`] if the actor is invalid or exceeds [`MAX_PAYLOAD_BYTES`] (512 UTF-8 bytes).
pub fn encode(actor: &Actor) -> Result<String, CoreError> {
    encode_with_context(actor, None)
}

/// Encodes `actor` and optional `context` into the unpadded base64url string defined in `spec/transport.md`.
///
/// # Errors
///
/// Returns [`CoreError`] if the payload is invalid or exceeds [`MAX_PAYLOAD_BYTES`] (512 UTF-8 bytes).
pub fn encode_with_context(actor: &Actor, context: Option<&Context>) -> Result<String, CoreError> {
    let canonical = validate_actor_payload(actor, context)?;
    Ok(encode_base64url_unpadded(canonical.as_bytes()))
}

/// Appends or merges a trailing `/*auditstream='<base64url>'*/` comment onto `sql`.
///
/// - Never tags a statement that already ends with an `auditstream` key (returns `sql` unchanged).
/// - If `sql` ends with a valid sqlcommenter comment (such as `/*traceparent='00-...'*/`),
///   merges `auditstream='...'` into that comment with keys sorted lexicographically and
///   preserves trailing whitespace/semicolons.
/// - Otherwise appends ` /*auditstream='...'*/` to the end of `sql`.
///
/// # Errors
///
/// Returns [`CoreError`] if `actor` is invalid or exceeds [`MAX_PAYLOAD_BYTES`] (512 UTF-8 bytes).
pub fn tag(sql: &str, actor: &Actor) -> Result<String, CoreError> {
    tag_with_context(sql, actor, None)
}

/// Appends or merges a trailing `/*auditstream='<base64url>'*/` comment with optional `context`.
///
/// # Errors
///
/// Returns [`CoreError`] if the payload is invalid or exceeds [`MAX_PAYLOAD_BYTES`] (512 UTF-8 bytes).
pub fn tag_with_context(
    sql: &str,
    actor: &Actor,
    context: Option<&Context>,
) -> Result<String, CoreError> {
    let encoded = encode_with_context(actor, context)?;

    if let Some((comment_start, body, suffix)) = find_trailing_comment(sql) {
        if extract_auditstream_value(body).is_some() {
            return Ok(sql.to_owned());
        }
        if let Some(mut pairs) = parse_sqlcommenter_pairs(body) {
            pairs.push(("auditstream", encoded.as_str()));
            pairs.sort_unstable_by(|a, b| a.0.cmp(b.0));

            let mut out = String::with_capacity(sql.len() + encoded.len() + 20);
            out.push_str(&sql[..comment_start]);
            out.push_str("/*");
            for (idx, (k, v)) in pairs.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(k);
                out.push_str("='");
                out.push_str(v);
                out.push('\'');
            }
            out.push_str("*/");
            out.push_str(suffix);
            return Ok(out);
        }
    }

    Ok(format!("{sql} /*auditstream='{encoded}'*/"))
}

/// Decodes a trailing `/*auditstream='...'*/` comment from `sql` following `spec/transport.md`.
///
/// Scans at most the last 4,096 Unicode scalar values (`right(current_query(), 4096)`),
/// never panics on multi-byte UTF-8 boundaries, and returns [`DecodedActor::Absent`] or
/// [`DecodedActor::Invalid`] rather than failing on malformed payloads.
pub fn decode(sql: &str) -> DecodedActor {
    let tail = tail_chars(sql, TAIL_WINDOW_CHARS);
    let Some((_, body, _)) = find_trailing_comment(tail) else {
        return DecodedActor::Absent;
    };

    let Some(raw_value) = extract_auditstream_value(body) else {
        return DecodedActor::Absent;
    };

    let Some(bytes) = decode_base64url_unpadded(raw_value) else {
        return DecodedActor::Invalid;
    };
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return DecodedActor::Invalid;
    }

    let Ok(text) = std::str::from_utf8(&bytes) else {
        return DecodedActor::Invalid;
    };

    parse_trigger_actor_json(text)
}

const ALLOWED_ACTOR_TYPES: &[&str] = &["user", "api_key", "ai_agent", "service", "system"];
const KNOWN_CONTEXT_KEYS: &[&str] = &["requestId", "source", "ip", "userAgent"];

fn is_valid_actor_type(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| ALLOWED_ACTOR_TYPES.contains(&s))
}

fn is_non_empty_json_string(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.is_empty())
}

fn is_valid_optional_non_empty_string(map: &serde_json::Map<String, Value>, key: &str) -> bool {
    map.get(key).is_none_or(is_non_empty_json_string)
}

fn is_valid_nested_actor(value: &Value) -> bool {
    let Value::Object(obo) = value else {
        return false;
    };
    is_valid_actor_type(obo.get("type"))
        && obo.get("id").is_some_and(is_non_empty_json_string)
        && !obo.contains_key("onBehalfOf")
        && is_valid_optional_non_empty_string(obo, "tenant")
}

fn is_valid_context_object(value: &Value) -> bool {
    let Value::Object(ctx) = value else {
        return false;
    };
    KNOWN_CONTEXT_KEYS
        .iter()
        .all(|key| is_valid_optional_non_empty_string(ctx, key))
}

/// Returns `true` if a syntactically valid JSON text `raw` contains a raw NUL byte (`0x00`)
/// or an unescaped `\u0000` escape inside any JSON string literal (object key or string value).
///
/// Scanning the raw JSON text rather than only the deduplicated `serde_json::Value` tree catches
/// `\u0000` even inside overwritten duplicate object keys, matching PostgreSQL's `::jsonb` lexer
/// (`SQLSTATE 22P05`) without heap allocations. Escaped backslashes (`\\u0000`) are skipped cleanly.
pub(crate) fn raw_json_contains_nul(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.contains(&0) {
        return true;
    }
    let len = bytes.len();
    let mut i = 0;
    let mut in_string = false;

    while i < len {
        match bytes[i] {
            0 => return true,
            b'"' => {
                in_string = !in_string;
                i += 1;
            }
            b'\\' if in_string => {
                if bytes.get(i + 1..i + 6) == Some(b"u0000") {
                    return true;
                }
                // Advance past the escaped byte so `\\u0000` is treated as literal `\` + `u0000`.
                i = (i + 2).min(len);
            }
            _ => i += 1,
        }
    }
    false
}

/// Validates and splits a JSON string using the full `spec/actor.md` and `spec/transport.md` rules:
/// JSON object with valid enum `type`, non-empty string `id`, optional valid `tenant` and
/// single-level `onBehalfOf`, optional `context` object with non-empty known fields, and no
/// NUL characters (`U+0000`) anywhere in the payload.
pub(crate) fn parse_trigger_actor_json(raw_json: &str) -> DecodedActor {
    if raw_json.len() > MAX_PAYLOAD_BYTES || raw_json_contains_nul(raw_json) {
        return DecodedActor::Invalid;
    }

    let Ok(val) = serde_json::from_str::<Value>(raw_json) else {
        return DecodedActor::Invalid;
    };
    let Value::Object(mut map) = val else {
        return DecodedActor::Invalid;
    };

    if !is_valid_actor_type(map.get("type"))
        || !map.get("id").is_some_and(is_non_empty_json_string)
        || !is_valid_optional_non_empty_string(&map, "tenant")
        || !map.get("onBehalfOf").is_none_or(is_valid_nested_actor)
    {
        return DecodedActor::Invalid;
    }

    let context = match map.remove("context") {
        None => None,
        Some(ctx) if is_valid_context_object(&ctx) => Some(ctx),
        Some(_) => return DecodedActor::Invalid,
    };

    DecodedActor::Valid {
        actor: Value::Object(map),
        context,
    }
}

fn tail_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().rev().nth(max_chars.saturating_sub(1)) {
        Some((idx, _)) => &s[idx..],
        None => s,
    }
}

/// Matches `/\*([^/]*)\*/[\s;]*$` at the end of `s`, returning `(comment_start_byte, body, suffix)`.
fn find_trailing_comment(s: &str) -> Option<(usize, &str, &str)> {
    let trimmed = s.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    let suffix = &s[trimmed.len()..];
    let before_close = trimmed.strip_suffix("*/")?;

    // Find the last `/*` before `*/` whose body contains no `/`.
    let open_idx = before_close.rfind("/*")?;
    let body = &before_close[open_idx + 2..];
    if body.contains('/') {
        return None;
    }

    Some((open_idx, body, suffix))
}

/// Matches `(?:^|,)auditstream='([^']*)'` inside a comment body.
fn extract_auditstream_value(body: &str) -> Option<&str> {
    let prefix = "auditstream='";
    let mut rest = body;
    let mut at_boundary = true;

    while !rest.is_empty() {
        if at_boundary && let Some(after_prefix) = rest.strip_prefix(prefix) {
            let end = after_prefix.find('\'')?;
            return Some(&after_prefix[..end]);
        }
        if let Some(comma_idx) = rest.find(',') {
            rest = &rest[comma_idx + 1..];
            at_boundary = true;
        } else {
            break;
        }
    }

    None
}

/// Parses a strict sqlcommenter body `k1='v1',k2='v2'` where keys are `[A-Za-z0-9_-]+`
/// and values contain no `'` or `/`.
fn parse_sqlcommenter_pairs(body: &str) -> Option<Vec<(&str, &str)>> {
    if body.is_empty() {
        return None;
    }
    let mut pairs = Vec::new();
    for part in body.split(',') {
        let eq = part.find('=')?;
        let key = &part[..eq];
        let val_quoted = &part[eq + 1..];
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return None;
        }
        let val = val_quoted.strip_prefix('\'')?.strip_suffix('\'')?;
        if val.contains('\'') || val.contains('/') {
            return None;
        }
        pairs.push((key, val));
    }
    Some(pairs)
}

fn encode_base64url_unpadded(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let (chunks, rem) = bytes.as_chunks::<3>();

    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[(n & 0x3f) as usize] as char);
    }

    match rem.len() {
        1 => {
            let b0 = u32::from(rem[0]);
            out.push(B64_ALPHABET[((b0 >> 2) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((b0 << 4) & 0x3f) as usize] as char);
        }
        2 => {
            let n = (u32::from(rem[0]) << 8) | u32::from(rem[1]);
            out.push(B64_ALPHABET[((n >> 10) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n >> 4) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n << 2) & 0x3f) as usize] as char);
        }
        _ => {}
    }

    out
}

fn decode_b64_char(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn decode_base64url_unpadded(encoded: &str) -> Option<Vec<u8>> {
    let bytes = encoded.as_bytes();
    let len = bytes.len();
    if len == 0 || len > MAX_ENCODED_COMMENT_CHARS || len % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity((len * 3) / 4);
    let mut i = 0;

    while i < len {
        let c0 = decode_b64_char(bytes[i])?;
        let c1 = decode_b64_char(bytes[i + 1])?;
        let rem = len - i;
        if rem == 2 {
            out.push((c0 << 2) | (c1 >> 4));
            break;
        }
        let c2 = decode_b64_char(bytes[i + 2])?;
        if rem == 3 {
            out.push((c0 << 2) | (c1 >> 4));
            out.push(((c1 & 0x0f) << 4) | (c2 >> 2));
            break;
        }
        let c3 = decode_b64_char(bytes[i + 3])?;
        out.push((c0 << 2) | (c1 >> 4));
        out.push(((c1 & 0x0f) << 4) | (c2 >> 2));
        out.push(((c2 & 0x03) << 6) | c3);
        i += 4;
    }

    Some(out)
}
