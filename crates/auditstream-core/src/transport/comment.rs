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
/// - Never tags a statement that already ends with a valid, lexically parsed `auditstream`
///   tag inside PostgreSQL's 4,096-character tail window (returns `sql` unchanged).
/// - If `sql` ends with a strict sqlcommenter block comment (such as `/*traceparent='00-...'*/`)
///   outside strings, identifiers, and `--` line comments, merges `auditstream='...'` into that
///   comment with keys sorted lexicographically and preserves trailing ASCII whitespace/semicolons,
///   provided the merged comment + suffix fits within the 4,096-character tail window and introduces
///   no `,auditstream='` collision with an earlier pair.
/// - When `sql` ends inside an unterminated `--` single-line comment, appends
///   `\n/*auditstream='...'*/` so PostgreSQL does not treat the tag as part of the line comment.
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

    if !ends_with_block_comment_close(sql) && !has_line_comment_candidate(sql) {
        return Ok(format!("{sql} /*auditstream='{encoded}'*/"));
    }

    let tail = scan_sql_tail(sql);
    if let Some(comment) = tail.trailing_block_comment {
        if matches!(
            decode_comment_body(comment.body),
            DecodedActor::Valid { .. }
        ) {
            return Ok(sql.to_owned());
        }
        if let Some(merged) = try_merge_trailing_comment(sql, &comment, &encoded) {
            return Ok(merged);
        }
    }

    if tail.ends_in_line_comment {
        Ok(format!("{sql}\n/*auditstream='{encoded}'*/"))
    } else {
        Ok(format!("{sql} /*auditstream='{encoded}'*/"))
    }
}

/// Decodes a trailing `/*auditstream='...'*/` comment from `sql` following `spec/transport.md`.
///
/// Scans at most the last 4,096 Unicode scalar values (`right(current_query(), 4096)`),
/// lexically ignores comments inside strings, identifiers, nested block comments, and `--`
/// line comments, enforces strict sqlcommenter key-value pair boundaries (with Drizzle/Google
/// sqlcommenter `\'` escape fallback), never panics on multi-byte UTF-8 boundaries, and returns
/// [`DecodedActor::Absent`] or [`DecodedActor::Invalid`] rather than failing on malformed payloads.
pub fn decode(sql: &str) -> DecodedActor {
    if !ends_with_block_comment_close(sql)
        || !tail_chars(sql, TAIL_WINDOW_CHARS).contains("auditstream='")
    {
        return DecodedActor::Absent;
    }

    let Some(comment) = scan_sql_tail(sql).trailing_block_comment else {
        return DecodedActor::Absent;
    };

    decode_comment_body(comment.body)
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

struct TrailingBlockComment<'a> {
    start: usize,
    body: &'a str,
    suffix: &'a str,
}

struct SqlTailInfo<'a> {
    ends_in_line_comment: bool,
    trailing_block_comment: Option<TrailingBlockComment<'a>>,
}

enum ParsedCommentTag<'a> {
    Absent,
    Invalid,
    Found(&'a str),
}

fn tail_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().rev().nth(max_chars.saturating_sub(1)) {
        Some((idx, _)) => &s[idx..],
        None => s,
    }
}

/// Matches PostgreSQL `scan.l` `space` (`[ \t\n\r\f\v]`) and `resolve_actor()`'s
/// `rtrim(..., E' \t\n\r\f\x0b;')`. Non-ASCII whitespace (such as `\u{00A0}`) is lexed as an
/// identifier byte by PostgreSQL, not whitespace.
const fn is_sql_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C | 0x0B)
}

const fn is_newline(b: u8) -> bool {
    matches!(b, b'\n' | b'\r')
}

const fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

const fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'$'
}

const fn is_dollar_tag_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

/// Matches `spec/transport.md` sqlcommenter keys plus unescaped `encodeURIComponent` characters
/// emitted by Drizzle 1.x and Prisma 7 (`A-Za-z0-9-_.!~*()%`).
const fn is_sqlcommenter_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'_' | b'-' | b'.' | b'%' | b'!' | b'~' | b'*' | b'(' | b')'
        )
}

fn ends_with_block_comment_close(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b';' || is_sql_whitespace(bytes[end - 1])) {
        end -= 1;
    }
    end >= 4 && bytes[end - 2] == b'*' && bytes[end - 1] == b'/'
}

fn has_line_comment_candidate(sql: &str) -> bool {
    let last_break = sql.rfind(['\n', '\r']).map_or(0, |idx| idx + 1);
    sql[last_break..].contains("--")
}

/// Matches PostgreSQL `pg_catalog.right(current_query(), 4096)` (Unicode scalar values).
fn is_within_tail_window(sql: &str, start_byte: usize) -> bool {
    let byte_len = sql.len() - start_byte;
    if byte_len <= TAIL_WINDOW_CHARS {
        return true;
    }
    if byte_len > TAIL_WINDOW_CHARS * 4 {
        return false;
    }
    sql[start_byte..].chars().nth(TAIL_WINDOW_CHARS).is_none()
}

fn skip_line_comment(bytes: &[u8], start: usize) -> (usize, bool) {
    let mut i = start + 2;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if is_newline(b) {
            return (i, true);
        }
    }
    (bytes.len(), false)
}

fn skip_quoted_token(bytes: &[u8], start: usize, quote: u8) -> usize {
    let len = bytes.len();
    let mut i = start + 1;
    while i < len {
        if bytes[i] != quote {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) == Some(&quote) {
            i += 2;
        } else {
            return i + 1;
        }
    }
    len
}

/// Matches PostgreSQL `scan.l` `<xe>{quotecontinue}` (`{whitespace_with_newline}'`) so
/// multi-line `E'...'` continuations keep backslash escaping active across chunks.
fn match_quote_continue(bytes: &[u8], after_quote: usize) -> Option<usize> {
    let len = bytes.len();
    let mut j = after_quote;
    let mut seen_newline = false;

    while j < len {
        let b = bytes[j];
        if is_sql_whitespace(b) {
            seen_newline |= is_newline(b);
            j += 1;
        } else if b == b'-' && bytes.get(j + 1) == Some(&b'-') {
            let (next_idx, terminated) = skip_line_comment(bytes, j);
            if !terminated {
                return None;
            }
            seen_newline = true;
            j = next_idx;
        } else {
            return (b == b'\'' && seen_newline).then_some(j + 1);
        }
    }
    None
}

fn skip_escape_string(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    let mut i = start + 2;
    while i < len {
        match bytes[i] {
            b'\\' => i = (i + 2).min(len),
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
            b'\'' => match match_quote_continue(bytes, i + 1) {
                Some(cont) => i = cont,
                None => return i + 1,
            },
            _ => i += 1,
        }
    }
    len
}

fn is_escape_string_start(bytes: &[u8], index: usize) -> bool {
    matches!(bytes[index], b'E' | b'e')
        && bytes.get(index + 1) == Some(&b'\'')
        && (index == 0 || !is_ident_cont(bytes[index - 1]))
}

fn match_dollar_quote_tag(sql: &str, start: usize) -> Option<&str> {
    let bytes = sql.as_bytes();
    if start > 0 && is_ident_cont(bytes[start - 1]) {
        return None;
    }
    let next = *bytes.get(start + 1)?;
    if next == b'$' {
        return Some(&sql[start..start + 2]);
    }
    if !is_ident_start(next) {
        return None;
    }
    let mut j = start + 2;
    while j < bytes.len() && is_dollar_tag_cont(bytes[j]) {
        j += 1;
    }
    (bytes.get(j) == Some(&b'$')).then_some(&sql[start..=j])
}

fn scan_block_comment(bytes: &[u8], start: usize) -> (usize, bool, bool) {
    let len = bytes.len();
    let mut i = start + 2;
    let mut depth: usize = 1;
    let mut has_slash_in_body = false;

    while i < len && depth > 0 {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if b == b'/' && next == Some(b'*') {
            depth += 1;
            has_slash_in_body = true;
            i += 2;
        } else if b == b'*' && next == Some(b'/') {
            depth -= 1;
            has_slash_in_body |= depth > 0;
            i += 2;
        } else {
            has_slash_in_body |= b == b'/';
            i += 1;
        }
    }

    (i, depth == 0, has_slash_in_body)
}

fn skip_non_comment_token(sql: &str, index: usize) -> usize {
    let bytes = sql.as_bytes();
    let b = bytes[index];
    if is_escape_string_start(bytes, index) {
        return skip_escape_string(bytes, index);
    }
    if b == b'\'' || b == b'"' {
        return skip_quoted_token(bytes, index, b);
    }
    if b == b'$'
        && let Some(tag) = match_dollar_quote_tag(sql, index)
    {
        let after_open = index + tag.len();
        return sql[after_open..]
            .find(tag)
            .map_or(sql.len(), |rel| after_open + rel + tag.len());
    }
    index + 1
}

const fn is_valid_trailing_block_comment(
    bytes: &[u8],
    start: usize,
    closed: bool,
    has_slash_in_body: bool,
) -> bool {
    closed && !has_slash_in_body && (start == 0 || bytes[start - 1] != b'*')
}

/// Single-pass PostgreSQL lexical scanner determining whether `sql` ends inside an unterminated
/// `--` line comment and extracting the final trailing `/*...*/` block comment (if closed,
/// containing no `/`, not immediately following `*`, outside strings/identifiers/`--` comments,
/// and within the 4,096-char tail).
fn scan_sql_tail(sql: &str) -> SqlTailInfo<'_> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut last_comment: Option<(usize, usize)> = None;
    let mut i = 0;

    while i < len {
        let b = bytes[i];
        if b == b';' || is_sql_whitespace(b) {
            i += 1;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            last_comment = None;
            let (next_idx, terminated) = skip_line_comment(bytes, i);
            if !terminated {
                return SqlTailInfo {
                    ends_in_line_comment: true,
                    trailing_block_comment: None,
                };
            }
            i = next_idx;
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let (next_idx, closed, has_slash_in_body) = scan_block_comment(bytes, i);
            last_comment = is_valid_trailing_block_comment(bytes, i, closed, has_slash_in_body)
                .then_some((i, next_idx));
            i = next_idx;
        } else {
            last_comment = None;
            i = skip_non_comment_token(sql, i);
        }
    }

    let trailing_block_comment = last_comment
        .filter(|&(start, _)| is_within_tail_window(sql, start))
        .map(|(start, end)| TrailingBlockComment {
            start,
            body: &sql[start + 2..end - 2],
            suffix: &sql[end..],
        });
    SqlTailInfo {
        ends_in_line_comment: false,
        trailing_block_comment,
    }
}

fn read_sqlcommenter_key_end(bytes: &[u8], start: usize, allow_escapes: bool) -> usize {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if is_sqlcommenter_key_byte(b) {
            i += 1;
        } else if allow_escapes && b == b'\\' && bytes.get(i + 1) == Some(&b'\'') {
            i += 2;
        } else {
            break;
        }
    }
    i
}

fn read_sqlcommenter_value(body: &str, start: usize, allow_escapes: bool) -> Option<(&str, usize)> {
    let bytes = body.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if allow_escapes && b == b'\\' && matches!(bytes.get(i + 1), Some(b'\'' | b'\\')) {
            i += 2;
            continue;
        }
        if b == b'\'' {
            return Some((&body[start..i], i + 1));
        }
        i += 1;
    }
    None
}

/// Parses a sqlcommenter body `k1='v1',k2='v2'` respecting single-quoted value boundaries.
///
/// Callers pass bodies already validated by [`scan_sql_tail`] to contain no `/`.
/// When `allow_escapes` is `false`, enforces strict `spec/transport.md` ABNF (no `'` inside
/// values). When `allow_escapes` is `true`, additionally permits Drizzle/Google sqlcommenter
/// `\'` in non-`auditstream` keys and `\'` / `\\` in non-`auditstream` values.
fn parse_sqlcommenter_pairs_with_mode(
    body: &str,
    allow_escapes: bool,
) -> Option<Vec<(&str, &str)>> {
    if body.is_empty() {
        return None;
    }
    let bytes = body.as_bytes();
    let mut pairs = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let key_end = read_sqlcommenter_key_end(bytes, i, allow_escapes);
        if key_end == i || bytes.get(key_end..key_end + 2) != Some(b"='") {
            return None;
        }
        let key = &body[i..key_end];
        let (val, next) =
            read_sqlcommenter_value(body, key_end + 2, allow_escapes && key != "auditstream")?;
        pairs.push((key, val));

        if next == bytes.len() {
            break;
        }
        if bytes[next] != b',' || next + 1 >= bytes.len() {
            return None;
        }
        i = next + 1;
    }

    Some(pairs)
}

fn extract_tag_from_pairs<'a>(pairs: &[(&'a str, &'a str)]) -> ParsedCommentTag<'a> {
    let mut found: Option<&'a str> = None;
    for &(key, val) in pairs {
        if key == "auditstream" {
            if found.is_some() {
                return ParsedCommentTag::Invalid;
            }
            found = Some(val);
        }
    }
    found.map_or(ParsedCommentTag::Absent, ParsedCommentTag::Found)
}

fn parse_auditstream_from_body(body: &str) -> ParsedCommentTag<'_> {
    if body.is_empty() {
        return ParsedCommentTag::Absent;
    }
    // Prefer strict unescaped ABNF (matching spec/transport.md and PG resolve_actor()),
    // falling back to Drizzle/Google sqlcommenter `\'` escaping only when strict parsing yields
    // no tag and `\'` is present.
    if let Some(pairs) = parse_sqlcommenter_pairs_with_mode(body, false) {
        let tag = extract_tag_from_pairs(&pairs);
        if !matches!(tag, ParsedCommentTag::Absent) || !body.contains("\\'") {
            return tag;
        }
    } else if !body.contains("\\'") {
        return ParsedCommentTag::Absent;
    }
    parse_sqlcommenter_pairs_with_mode(body, true).map_or(ParsedCommentTag::Absent, |pairs| {
        extract_tag_from_pairs(&pairs)
    })
}

fn decode_comment_body(body: &str) -> DecodedActor {
    let raw_value = match parse_auditstream_from_body(body) {
        ParsedCommentTag::Absent => return DecodedActor::Absent,
        ParsedCommentTag::Invalid => return DecodedActor::Invalid,
        ParsedCommentTag::Found(val) => val,
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

fn try_merge_trailing_comment(
    sql: &str,
    comment: &TrailingBlockComment<'_>,
    encoded: &str,
) -> Option<String> {
    let mut pairs = parse_sqlcommenter_pairs_with_mode(comment.body, false)?;
    if pairs.iter().any(|(k, _)| *k == "auditstream") {
        return None;
    }
    pairs.push(("auditstream", encoded));
    pairs.sort_by(|a, b| a.0.cmp(b.0));

    let mut merged_body = String::with_capacity(comment.body.len() + encoded.len() + 16);
    let mut auditstream_start = 0;
    for (idx, (k, v)) in pairs.iter().enumerate() {
        if idx > 0 {
            merged_body.push(',');
        }
        if *k == "auditstream" {
            auditstream_start = merged_body.len();
        }
        merged_body.push_str(k);
        merged_body.push_str("='");
        merged_body.push_str(v);
        merged_body.push('\'');
    }

    // Avoid emitting `,auditstream='` inside an earlier key's value before our key so even
    // legacy substring-based `resolve_actor()` installations and older adapters decode cleanly.
    if auditstream_start > 0 && merged_body.find(",auditstream='") != Some(auditstream_start - 1) {
        return None;
    }

    // Ensure the entire merged `/*...*/` plus trailing whitespace/semicolons stays inside
    // PostgreSQL's 4,096-character `right(current_query(), 4096)` window.
    let merged_tail_chars = 4 + merged_body.chars().count() + comment.suffix.chars().count();
    if merged_tail_chars > TAIL_WINDOW_CHARS {
        return None;
    }

    let mut out =
        String::with_capacity(comment.start + 4 + merged_body.len() + comment.suffix.len());
    out.push_str(&sql[..comment.start]);
    out.push_str("/*");
    out.push_str(&merged_body);
    out.push_str("*/");
    out.push_str(comment.suffix);
    Some(out)
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
