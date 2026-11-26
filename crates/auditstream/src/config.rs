//! Configuration file loading and precedence resolution.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use auditstream_core::{CompiledRedaction, Redaction};
use auditstream_export::SinkConfig;
use clap::ValueEnum;
use serde::Deserialize;

use crate::cli::{Cli, Command};
use crate::commands::export::Format;
use crate::exit::{Exit, ExitCodeExt};

/// Default Postgres schema name for Auditstream objects.
pub(crate) const DEFAULT_SCHEMA: &str = "auditstream";

/// Default number of rows read per export batch.
pub(crate) const DEFAULT_BATCH_SIZE: usize = 1000;

/// Default config filename looked up in the working directory when `--config` is omitted.
pub(crate) const DEFAULT_CONFIG_FILE: &str = "auditstream.toml";

/// Supported migration output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MigrationFormat {
    /// Prisma SQL migration directory.
    Prisma,
    /// Drizzle SQL migration directory.
    Drizzle,
    /// Plain SQL migration files.
    Sql,
}

/// Wrapper around a database URL that masks embedded passwords in `Debug` and `Display`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RedactedUrl(String);

impl RedactedUrl {
    /// Wraps a raw database connection URL.
    pub(crate) fn new(raw: String) -> Self {
        Self(raw)
    }

    /// Returns a redacted representation with any URL password replaced by `***`.
    pub(crate) fn redacted(&self) -> String {
        redact_url_password(&self.0)
    }

    /// Returns the raw connection string for passing directly to [`auditstream_pg::connect`].
    ///
    /// Never log or include this value in user-facing error messages.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RedactedUrl")
            .field(&self.redacted())
            .finish()
    }
}

impl fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

fn is_secret_pg_param(key: &str) -> bool {
    let cleaned: String = key
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    cleaned.eq_ignore_ascii_case("password")
        || cleaned.eq_ignore_ascii_case("sslpassword")
        || cleaned.eq_ignore_ascii_case("ssl-password")
        || cleaned.eq_ignore_ascii_case("oauth_client_secret")
}

fn is_valid_uri_scheme(prefix: &str) -> bool {
    let mut bytes = prefix.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode_form_key(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                decoded.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    decoded.push((hi << 4) | lo);
                    i += 3;
                } else {
                    decoded.push(bytes[i]);
                    i += 1;
                }
            }
            byte => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn strip_whatwg_ascii_tabs_newlines(trimmed: &str) -> std::borrow::Cow<'_, str> {
    if trimmed.contains(['\t', '\n', '\r']) {
        std::borrow::Cow::Owned(
            trimmed
                .chars()
                .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    }
}

const fn is_libpq_whitespace(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == b'\x0b'
}

const fn is_libpq_ws_char(ch: char) -> bool {
    ch.is_ascii_whitespace() || ch == '\x0b'
}

fn redact_url_password(raw: &str) -> String {
    // Run libpq KV redaction against the raw whitespace-preserving input first so single-token
    // (`password=...`), vertical-tab (`\x0b`), and mixed whitespace-separated tokens
    // (`foo:bar\tpassword=...`, `postgresql://db/prod\tpassword=...`,
    // `postgresql: host=localhost\tpassword=...`) are always masked before WHATWG stripping.
    let kv_redacted = redact_libpq_kv_password(raw);
    let trimmed = kv_redacted.trim_matches(|c: char| c <= ' ');
    let normalized = strip_whatwg_ascii_tabs_newlines(trimmed);
    if let Some(scheme_end) = normalized.find("://")
        && is_valid_uri_scheme(&normalized[..scheme_end])
    {
        return redact_uri_password(&normalized, scheme_end, 3);
    }

    if let Some(scheme_end) = normalized.find(':') {
        let scheme = &normalized[..scheme_end];
        if (scheme.eq_ignore_ascii_case("postgres") || scheme.eq_ignore_ascii_case("postgresql"))
            && normalized[scheme_end + 1..].contains(['?', '@', ':'])
        {
            return redact_uri_password(&normalized, scheme_end, 1);
        }
    }
    kv_redacted
}

fn is_valid_port(port: &str) -> bool {
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok()
}

fn is_valid_ipv6_literal(addr: &str) -> bool {
    let (ip_part, zone_ok) = match addr.split_once('%') {
        Some((ip, zone)) => (
            ip,
            !zone.is_empty()
                && zone
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'%' | b'.' | b'-' | b'_')),
        ),
        None => (addr, true),
    };
    zone_ok && ip_part.parse::<std::net::Ipv6Addr>().is_ok()
}

fn is_host_port_authority(authority: &str) -> bool {
    authority.split(',').all(|segment| {
        if segment
            .bytes()
            .any(|b| is_libpq_whitespace(b) || matches!(b, b'=' | b'&'))
        {
            return false;
        }
        if let Some(rest) = segment.strip_prefix('[') {
            match rest.split_once(']') {
                Some((addr, "")) => is_valid_ipv6_literal(addr),
                Some((addr, port_part)) => {
                    is_valid_ipv6_literal(addr)
                        && port_part.strip_prefix(':').is_some_and(is_valid_port)
                }
                None => false,
            }
        } else {
            match segment.split_once(':') {
                None => true,
                Some((_, port)) => is_valid_port(port),
            }
        }
    })
}

fn is_strong_uri_host(host: &str, after_delim: Option<u8>) -> bool {
    after_delim == Some(b'/')
        || host.contains([':', '.', ','])
        || host.eq_ignore_ascii_case("localhost")
}

#[derive(Clone, Copy)]
enum AuthorityShape {
    Malformed,
    HasPassword,
    BareHostPort,
    Clean,
}

#[derive(Clone, Copy)]
enum TailRegion {
    Path { seen_at: bool },
    Query { pair_has_eq: bool },
    Fragment,
}

struct TailAtScan {
    best_authority_at: Option<usize>,
    fallback_non_query_at: Option<usize>,
}

struct TailAtScanner {
    authority_shape: AuthorityShape,
    has_nonempty_path: bool,
    path_has_colon: bool,
    region: TailRegion,
    best_authority_at: Option<usize>,
    fallback_non_query_at: Option<usize>,
}

impl TailAtScanner {
    fn new(authority: &str, tail: &str, malformed_authority: bool) -> Self {
        let authority_at = authority.rfind('@');
        let authority_shape = if malformed_authority {
            AuthorityShape::Malformed
        } else if authority_at.is_some_and(|at| authority[..at].contains(':')) {
            AuthorityShape::HasPassword
        } else if authority_at.is_none() && authority.contains(':') {
            AuthorityShape::BareHostPort
        } else {
            AuthorityShape::Clean
        };
        Self {
            authority_shape,
            has_nonempty_path: tail.starts_with('/'),
            path_has_colon: false,
            region: TailRegion::Path { seen_at: false },
            best_authority_at: None,
            fallback_non_query_at: None,
        }
    }

    fn qualifies(&self, host: &str, after_delim: Option<u8>) -> bool {
        let malformed_or_path_colon =
            matches!(self.authority_shape, AuthorityShape::Malformed) || self.path_has_colon;
        if !malformed_or_path_colon && matches!(self.authority_shape, AuthorityShape::Clean) {
            return false;
        }
        let has_path_or_colon = after_delim == Some(b'/') || host.contains(':');
        let allow_query_or_frag = malformed_or_path_colon || !self.has_nonempty_path;
        match self.region {
            TailRegion::Path { .. } => {
                malformed_or_path_colon
                    || matches!(self.authority_shape, AuthorityShape::HasPassword)
                    || is_strong_uri_host(host, after_delim)
            }
            TailRegion::Query { pair_has_eq: false } => {
                allow_query_or_frag || is_strong_uri_host(host, after_delim)
            }
            TailRegion::Query { pair_has_eq: true } => allow_query_or_frag && has_path_or_colon,
            TailRegion::Fragment => {
                allow_query_or_frag && (has_path_or_colon || self.best_authority_at.is_none())
            }
        }
    }

    fn inspect_at(&mut self, tail: &str, authority_end: usize, idx: usize) {
        if !matches!(self.region, TailRegion::Query { pair_has_eq: true }) {
            self.fallback_non_query_at = Some(authority_end + idx);
        }
        let bytes = tail.as_bytes();
        let after = &tail[idx + 1..];
        // Stopping at `@` keeps all scanned host slices pairwise disjoint (strictly O(n)).
        let rel_end = after
            .bytes()
            .position(|c| matches!(c, b'/' | b'?' | b'#' | b'@'))
            .unwrap_or(after.len());
        let host_end = idx + 1 + rel_end;
        let has_following_at = host_end < bytes.len() && bytes[host_end] == b'@';
        let host = &after[..rel_end];
        let after_delim = bytes.get(host_end).copied();
        let valid_host = !has_following_at && !host.is_empty() && is_host_port_authority(host);

        if valid_host && self.qualifies(host, after_delim) {
            self.best_authority_at = Some(authority_end + idx);
        }
        if let TailRegion::Path { seen_at } = &mut self.region {
            *seen_at = true;
        }
    }
}

fn scan_tail_at_candidates(
    authority: &str,
    authority_end: usize,
    tail: &str,
    malformed_authority: bool,
) -> TailAtScan {
    let mut scanner = TailAtScanner::new(authority, tail, malformed_authority);
    for (idx, &byte) in tail.as_bytes().iter().enumerate() {
        match byte {
            b'#' => scanner.region = TailRegion::Fragment,
            b'?' if matches!(scanner.region, TailRegion::Path { .. }) => {
                scanner.region = TailRegion::Query { pair_has_eq: false };
            }
            b'&' if matches!(scanner.region, TailRegion::Query { .. }) => {
                scanner.region = TailRegion::Query { pair_has_eq: false };
            }
            b'=' if matches!(scanner.region, TailRegion::Query { .. }) => {
                scanner.region = TailRegion::Query { pair_has_eq: true };
            }
            b':' if matches!(scanner.region, TailRegion::Path { seen_at: false }) => {
                scanner.path_has_colon = true;
            }
            b'@' => scanner.inspect_at(tail, authority_end, idx),
            _ => {}
        }
    }
    TailAtScan {
        best_authority_at: scanner.best_authority_at,
        fallback_non_query_at: scanner.fallback_non_query_at,
    }
}

fn append_redacted_query_pair(out: &mut String, mut pair: &str) {
    loop {
        let hash_pos = pair.find('#');
        let key_limit = hash_pos.unwrap_or(pair.len());
        if let Some(eq_pos) = pair[..key_limit].find('=') {
            let key = &pair[..eq_pos];
            let decoded_key = percent_decode_form_key(key);
            if is_secret_pg_param(&decoded_key) {
                out.push_str(key);
                out.push_str("=***");
                return;
            }
        }
        if let Some(h) = hash_pos
            && let Some(rel_q) = pair[h + 1..].find('?')
        {
            let split = h + 1 + rel_q + 1;
            out.push_str(&pair[..split]);
            pair = &pair[split..];
            continue;
        }
        out.push_str(pair);
        return;
    }
}

fn append_redacted_uri_tail(out: &mut String, tail: &str) {
    // Do not stop at `#` before `?` or inside a query value: PostgreSQL URLs do not use
    // fragments, so an unencoded `#` in a password or earlier value must never disable masking.
    let Some(q_pos) = tail.find('?') else {
        out.push_str(tail);
        return;
    };

    out.push_str(&tail[..=q_pos]);
    for (idx, pair) in tail[q_pos + 1..].split('&').enumerate() {
        if idx > 0 {
            out.push('&');
        }
        append_redacted_query_pair(out, pair);
    }
}

fn redact_uri_password(raw: &str, scheme_end: usize, sep_len: usize) -> String {
    let rest = &raw[scheme_end + sep_len..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let mut authority = &rest[..authority_end];
    let mut tail = &rest[authority_end..];

    // If an unencoded `/`, `?`, or `#` appeared inside an authority password (for example
    // `postgresql://app:abc/def@host/db`, `postgresql://app:1234/secret@host`, or
    // `postgresql://app:p1@p2/secret@host/db?application_name=ops@corp.com`), re-slice at the
    // rightmost authority `@` outside query values (or fail closed) in O(n) time so raw
    // credentials are never echoed while legitimate `@` in paths, queries, or fragments remain
    // intact.
    let authority_host = authority
        .rfind('@')
        .map_or(authority, |at| &authority[at + 1..]);
    let malformed_authority = !is_host_port_authority(authority_host);
    let scan = scan_tail_at_candidates(authority, authority_end, tail, malformed_authority);
    if malformed_authority || scan.best_authority_at.is_some() {
        if let Some(later_at) = scan.best_authority_at.or(scan.fallback_non_query_at) {
            let after_at = &rest[later_at + 1..];
            let host_end = after_at.find(['/', '?', '#']).unwrap_or(after_at.len());
            let new_end = later_at + 1 + host_end;
            authority = &rest[..new_end];
            tail = &rest[new_end..];
        } else {
            return format!(
                "{}<redacted malformed database URL>",
                &raw[..scheme_end + sep_len]
            );
        }
    }

    let mut out = String::with_capacity(raw.len());
    out.push_str(&raw[..scheme_end + sep_len]);

    if let Some(at_pos) = authority.rfind('@')
        && let Some(colon_pos) = authority[..at_pos].find(':')
    {
        out.push_str(&authority[..=colon_pos]);
        out.push_str("***");
        out.push_str(&authority[at_pos..]);
    } else {
        out.push_str(authority);
    }

    append_redacted_uri_tail(&mut out, tail);
    out
}

fn advance_escaped(raw: &str, backslash_idx: usize) -> usize {
    let after_slash = backslash_idx + 1;
    raw[after_slash..]
        .chars()
        .next()
        .map_or(raw.len(), |ch| after_slash + ch.len_utf8())
}

fn starts_with_secret_kv(rest: &str) -> bool {
    // Longest secret parameter name (`oauth_client_secret`) is 19 ASCII bytes.
    let Some(sep_pos) = rest
        .bytes()
        .take(20)
        .position(|b| is_libpq_whitespace(b) || b == b'=')
    else {
        return false;
    };
    let key = &rest[..sep_pos];
    if !is_secret_pg_param(key) {
        return false;
    }
    rest.as_bytes()[sep_pos] == b'='
        || rest[sep_pos..]
            .trim_start_matches(is_libpq_ws_char)
            .starts_with('=')
}

fn skip_secret_quoted_trailing(raw: &str, mut i: usize) -> usize {
    let bytes = raw.as_bytes();
    if starts_with_secret_kv(&raw[i..]) {
        return i;
    }
    while i < bytes.len() && !is_libpq_whitespace(bytes[i]) {
        let is_quote = bytes[i] == b'\'';
        i += 1;
        if is_quote && starts_with_secret_kv(&raw[i..]) {
            break;
        }
    }
    i
}

fn redact_libpq_kv_password(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut out = String::with_capacity(len);

    while i < len {
        let ws_start = i;
        while i < len && is_libpq_whitespace(bytes[i]) {
            i += 1;
        }
        out.push_str(&raw[ws_start..i]);
        if i >= len {
            break;
        }

        let key_start = i;
        while i < len && !is_libpq_whitespace(bytes[i]) && !matches!(bytes[i], b'=' | b'\'') {
            i += 1;
        }
        if i < len && bytes[i] == b'\'' {
            i += 1;
            out.push_str(&raw[key_start..i]);
            continue;
        }
        let key = &raw[key_start..i];
        out.push_str(key);

        let pre_eq = i;
        while i < len && is_libpq_whitespace(bytes[i]) {
            i += 1;
        }
        if i >= len || bytes[i] != b'=' {
            out.push_str(&raw[pre_eq..i]);
            continue;
        }
        i += 1; // consume '='
        let after_eq = i;
        while i < len && is_libpq_whitespace(bytes[i]) {
            i += 1;
        }
        let had_ws_after_eq = i > after_eq;
        out.push_str(&raw[pre_eq..i]);

        let val_start = i;
        if i < len && bytes[i] == b'\'' {
            i += 1;
            let mut closed = false;
            while i < len {
                if bytes[i] == b'\\' {
                    i = advance_escaped(raw, i);
                } else if bytes[i] == b'\'' {
                    i += 1;
                    closed = true;
                    break;
                } else {
                    i += 1;
                }
            }
            if is_secret_pg_param(key) {
                // Consume any malformed trailing non-whitespace suffix (e.g. `password='pa'ss'`),
                // stopping before an adjacent secret parameter immediately after `'` (e.g.
                // `password='a'sslpassword = b` or `password='a'dbname='prod'sslpassword = b`).
                if closed {
                    i = skip_secret_quoted_trailing(raw, i);
                }
                out.push_str("***");
            } else if closed && (i >= len || is_libpq_whitespace(bytes[i])) {
                out.push_str(&raw[val_start..i]);
            } else if closed && starts_with_secret_kv(&raw[i..]) {
                // Adjacent secret parameter immediately follows a closing quote (e.g.
                // `host='localhost'password=SECRET` or a misaligned quote right before
                // `password=...`); redact the inner span (which has no unescaped `'`, so
                // recursion depth is at most 1) and keep `i` positioned at the secret key.
                out.push('\'');
                out.push_str(&redact_libpq_kv_password(&raw[val_start + 1..i - 1]));
                out.push('\'');
            } else if closed {
                // Misaligned quote (e.g. `application_name='broken password='SECRET'` or
                // `application_name='broken password='s3cr3t=='`) must not swallow a following
                // secret parameter.
                out.push('\'');
                i = val_start + 1;
            } else {
                // Unclosed quote in a non-secret value must not swallow a following `password=...`.
                out.push('\'');
                out.push_str(&redact_libpq_kv_password(&raw[val_start + 1..i]));
            }
        } else {
            while i < len && !is_libpq_whitespace(bytes[i]) {
                if bytes[i] == b'\\' {
                    i = advance_escaped(raw, i);
                } else {
                    i += 1;
                }
            }
            let val = &raw[val_start..i];
            if had_ws_after_eq && starts_with_secret_kv(&raw[val_start..]) {
                // Previous key had an empty unquoted value (e.g. `sslrootcert= password=SECRET`,
                // `sslrootcert= password = SECRET`, or `password= sslpassword = SECRET`); rewind
                // so the following secret pair is parsed and masked on the next iteration.
                i = val_start;
            } else if is_secret_pg_param(key) {
                out.push_str("***");
            } else {
                out.push_str(val);
            }
        }
    }

    out
}

/// Resolved CLI configuration (public contract: `docs/reference/config.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    /// Resolved configuration file path (`--config` / `AUDITSTREAM_CONFIG` or `./auditstream.toml`).
    pub(crate) file_path: PathBuf,
    /// Whether `-q` / `--quiet` was passed on the command line.
    pub(crate) quiet: bool,
    /// Postgres connection URL (password redacted in debug/logs).
    pub(crate) database_url: Option<RedactedUrl>,
    /// Audit schema name.
    pub(crate) schema: String,
    /// Capture settings.
    pub(crate) capture: CaptureConfig,
    /// Migration writer settings.
    pub(crate) migrations: MigrationsConfig,
    /// Export settings.
    pub(crate) export: ExportConfig,
}

/// Resolved `[capture]` configuration section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CaptureConfig {
    /// Audited tables (`schema.table`).
    pub(crate) tables: Vec<String>,
    /// Excluded columns (`schema.table.column`).
    pub(crate) exclude: Vec<String>,
}

/// Resolved `[migrations]` configuration section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MigrationsConfig {
    /// Target migration format.
    pub(crate) format: Option<MigrationFormat>,
    /// Target migration directory.
    pub(crate) dir: Option<PathBuf>,
}

/// Resolved `[export]` configuration section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExportConfig {
    /// Optional cursor persistence file path.
    pub(crate) cursor_file: Option<PathBuf>,
    /// Maximum number of events per batch.
    pub(crate) batch_size: usize,
    /// Built-in sink format and optional output path (defaults to JSON Lines on stdout).
    pub(crate) sink: SinkConfig,
    /// Raw export redaction policy from `auditstream.toml` (bound into export checkpoints).
    pub(crate) redaction: Redaction,
    /// Pre-validated, pre-compiled redaction rules applied before `sink.write`.
    pub(crate) compiled_redaction: CompiledRedaction,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    database_url: Option<String>,
    schema: Option<String>,
    #[serde(default)]
    capture: CaptureFileConfig,
    #[serde(default)]
    migrations: MigrationsFileConfig,
    #[serde(default)]
    export: ExportFileConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureFileConfig {
    #[serde(default)]
    tables: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationsFileConfig {
    format: Option<MigrationFormat>,
    dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportFileConfig {
    cursor_file: Option<PathBuf>,
    batch_size: Option<usize>,
    sink: Option<SinkConfig>,
    #[serde(default)]
    redaction: Redaction,
}

impl Config {
    /// Loads configuration with precedence: flags > environment > config file > defaults.
    pub(crate) fn load(cli: &Cli, cwd: &Path) -> anyhow::Result<Self> {
        let fallback_database_url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|val| !val.is_empty());
        Self::load_with_fallback_env(cli, cwd, fallback_database_url)
    }

    /// Loads configuration with an injectable `DATABASE_URL` fallback for deterministic tests.
    pub(crate) fn load_with_fallback_env(
        cli: &Cli,
        cwd: &Path,
        fallback_database_url: Option<String>,
    ) -> anyhow::Result<Self> {
        let file = load_file_config(cli.config.as_deref(), cwd)?;

        let database_url = cli
            .database_url
            .clone()
            .filter(|val| !val.is_empty())
            .or(fallback_database_url.filter(|val| !val.is_empty()))
            .or(file.database_url.filter(|val| !val.is_empty()))
            .map(RedactedUrl::new);

        let schema = cli
            .schema
            .clone()
            .filter(|val| !val.is_empty())
            .or(file.schema.filter(|val| !val.is_empty()))
            .unwrap_or_else(|| DEFAULT_SCHEMA.to_owned());

        let mut capture = CaptureConfig {
            tables: file.capture.tables,
            exclude: file.capture.exclude,
        };
        let mut migrations = MigrationsConfig {
            format: file.migrations.format,
            dir: file.migrations.dir,
        };
        let compiled_redaction = file
            .export
            .redaction
            .compile()
            .map_err(|err| anyhow!("invalid export.redaction config: {err}"))
            .exit_code(Exit::Usage)?;

        let mut export = ExportConfig {
            cursor_file: file.export.cursor_file,
            batch_size: file.export.batch_size.unwrap_or(DEFAULT_BATCH_SIZE),
            sink: file.export.sink.unwrap_or(SinkConfig::Jsonl { path: None }),
            redaction: file.export.redaction,
            compiled_redaction,
        };

        match &cli.command {
            Command::Init(args) => {
                if !args.table.is_empty() {
                    capture.tables.clone_from(&args.table);
                }
                if !args.exclude.is_empty() {
                    capture.exclude.clone_from(&args.exclude);
                }
                if let Some(format) = args.migrations {
                    migrations.format = Some(format);
                }
                if let Some(dir) = &args.out {
                    migrations.dir = Some(dir.clone());
                }
            }
            Command::Export(args) => {
                if let Some(cursor_file) = &args.cursor_file {
                    export.cursor_file = Some(cursor_file.clone());
                }
                if let Some(batch_size) = args.batch_size {
                    export.batch_size = batch_size;
                }
                let (format, path) = match export.sink {
                    SinkConfig::Jsonl { path } => (Format::Jsonl, path),
                    SinkConfig::Csv { path } => (Format::Csv, path),
                };
                let path = args.output.clone().or(path);
                export.sink = match args.format.unwrap_or(format) {
                    Format::Jsonl => SinkConfig::Jsonl { path },
                    Format::Csv => SinkConfig::Csv { path },
                };
            }
            Command::Status(_) | Command::Verify(_) | Command::History(_) => {}
        }

        let file_path = cli
            .config
            .clone()
            .unwrap_or_else(|| cwd.join(DEFAULT_CONFIG_FILE));

        Ok(Self {
            file_path,
            quiet: cli.quiet,
            database_url,
            schema,
            capture,
            migrations,
            export,
        })
    }
}

fn load_file_config(explicit_path: Option<&Path>, cwd: &Path) -> anyhow::Result<FileConfig> {
    let path = if let Some(explicit) = explicit_path {
        if !explicit.exists() {
            return Err(anyhow!("config file not found: {}", explicit.display()))
                .exit_code(Exit::Usage);
        }
        explicit.to_path_buf()
    } else {
        let default_path = cwd.join(DEFAULT_CONFIG_FILE);
        if !default_path.exists() {
            return Ok(FileConfig::default());
        }
        default_path
    };

    let raw = fs::read_to_string(&path)
        .map_err(|err| anyhow!("failed to read config file {}: {err}", path.display()))
        .exit_code(Exit::Usage)?;

    toml::from_str::<FileConfig>(&raw)
        .map_err(|mut err| {
            // Never echo raw config source lines that may contain `database_url` credentials.
            err.set_input(None);
            anyhow!("invalid config in {}: {err}", path.display())
        })
        .exit_code(Exit::Usage)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use auditstream_export::SinkConfig;
    use clap::Parser;
    use tempfile::tempdir;

    use super::{Config, DEFAULT_BATCH_SIZE, DEFAULT_SCHEMA, MigrationFormat, RedactedUrl};
    use crate::cli::Cli;
    use crate::exit::{Exit, exit_code_for};

    #[test]
    fn defaults_apply_without_config_file_or_env() {
        let dir = tempdir().unwrap();
        let cli = Cli::try_parse_from(["auditstream", "status"]).unwrap();
        let cfg = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap();

        assert_eq!(cfg.database_url, None);
        assert_eq!(cfg.schema, DEFAULT_SCHEMA);
        assert_eq!(cfg.capture.tables, Vec::<String>::new());
        assert_eq!(cfg.capture.exclude, Vec::<String>::new());
        assert_eq!(cfg.migrations.format, None);
        assert_eq!(cfg.migrations.dir, None);
        assert_eq!(cfg.export.cursor_file, None);
        assert_eq!(cfg.export.batch_size, DEFAULT_BATCH_SIZE);
        assert_eq!(cfg.export.sink, SinkConfig::Jsonl { path: None });
    }

    #[test]
    fn file_layer_overrides_defaults() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auditstream.toml"),
            r#"
database_url = "postgres://file_user@localhost/file_db"
schema = "custom_audit"

[capture]
tables = ["public.invoice"]
exclude = ["public.invoice.password_hash"]

[migrations]
format = "prisma"
dir = "prisma/migrations"

[export]
cursor_file = ".auditstream.cursor"
batch_size = 250
"#,
        )
        .unwrap();

        let cli = Cli::try_parse_from(["auditstream", "status"]).unwrap();
        let cfg = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap();

        assert_eq!(
            cfg.database_url,
            Some(RedactedUrl::new(
                "postgres://file_user@localhost/file_db".to_owned()
            ))
        );
        assert_eq!(cfg.schema, "custom_audit");
        assert_eq!(cfg.capture.tables, vec!["public.invoice"]);
        assert_eq!(cfg.capture.exclude, vec!["public.invoice.password_hash"]);
        assert_eq!(cfg.migrations.format, Some(MigrationFormat::Prisma));
        assert_eq!(cfg.migrations.dir, Some(PathBuf::from("prisma/migrations")));
        assert_eq!(
            cfg.export.cursor_file,
            Some(PathBuf::from(".auditstream.cursor"))
        );
        assert_eq!(cfg.export.batch_size, 250);
    }

    #[test]
    fn fallback_database_url_env_overrides_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auditstream.toml"),
            "database_url = \"postgres://file@localhost/db\"\n",
        )
        .unwrap();

        let cli = Cli::try_parse_from(["auditstream", "status"]).unwrap();
        let cfg = Config::load_with_fallback_env(
            &cli,
            dir.path(),
            Some("postgres://env@localhost/db".to_owned()),
        )
        .unwrap();

        assert_eq!(
            cfg.database_url,
            Some(RedactedUrl::new("postgres://env@localhost/db".to_owned()))
        );
    }

    #[test]
    fn cli_flags_override_env_and_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auditstream.toml"),
            "database_url = \"postgres://file@localhost/db\"\nschema = \"file_schema\"\n",
        )
        .unwrap();

        let cli = Cli::try_parse_from([
            "auditstream",
            "--database-url",
            "postgres://flag@localhost/db",
            "--schema",
            "flag_schema",
            "export",
            "--batch-size",
            "42",
        ])
        .unwrap();
        let cfg = Config::load_with_fallback_env(
            &cli,
            dir.path(),
            Some("postgres://env@localhost/db".to_owned()),
        )
        .unwrap();

        assert_eq!(
            cfg.database_url,
            Some(RedactedUrl::new("postgres://flag@localhost/db".to_owned()))
        );
        assert_eq!(cfg.schema, "flag_schema");
        assert_eq!(cfg.export.batch_size, 42);
    }

    #[test]
    fn unknown_key_wrong_type_and_missing_config_exit_usage() {
        let dir = tempdir().unwrap();
        let unknown_path = dir.path().join("unknown.toml");
        fs::write(&unknown_path, "not_a_key = 123\n").unwrap();
        let cli = Cli::try_parse_from([
            "auditstream",
            "--config",
            unknown_path.to_str().unwrap(),
            "status",
        ])
        .unwrap();
        let err = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap_err();
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("not_a_key"));

        let bad_type_path = dir.path().join("bad_type.toml");
        fs::write(&bad_type_path, "[export]\nbatch_size = \"lots\"\n").unwrap();
        let cli = Cli::try_parse_from([
            "auditstream",
            "--config",
            bad_type_path.to_str().unwrap(),
            "status",
        ])
        .unwrap();
        let err = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap_err();
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("batch_size"));

        let cli =
            Cli::try_parse_from(["auditstream", "--config", "missing.toml", "status"]).unwrap();
        let err = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap_err();
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("missing.toml"));
    }

    #[test]
    fn export_sink_overrides_preserve_other_fields() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auditstream.toml"),
            "[export.sink]\ntype = \"csv\"\npath = \"events.csv\"\n",
        )
        .unwrap();
        for (args, expected) in [
            (
                ["--format", "jsonl"],
                SinkConfig::Jsonl {
                    path: Some("events.csv".into()),
                },
            ),
            (
                ["--output", "override.csv"],
                SinkConfig::Csv {
                    path: Some("override.csv".into()),
                },
            ),
        ] {
            let cli =
                Cli::try_parse_from(["auditstream", "export"].into_iter().chain(args)).unwrap();
            let cfg = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap();
            assert_eq!(cfg.export.sink, expected);
        }
    }

    #[test]
    fn invalid_export_sink_is_a_usage_error() {
        let dir = tempdir().unwrap();
        for (sink, invalid) in [
            ("type = \"parquet\"", "parquet"),
            ("type = \"jsonl\"\ncompression = \"gzip\"", "compression"),
            ("type = \"csv\"\npath = 42", "path"),
        ] {
            fs::write(
                dir.path().join("auditstream.toml"),
                format!("[export.sink]\n{sink}\n"),
            )
            .unwrap();
            let cli = Cli::try_parse_from(["auditstream", "export"]).unwrap();
            let err = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap_err();
            assert_eq!(exit_code_for(&err), Exit::Usage);
            assert!(err.to_string().contains(invalid), "{err}");
        }
    }

    #[test]
    fn redacted_url_masks_password_in_debug_and_display() {
        let secret = ["dummy", "p@ss", "token"].join("_");
        let raw =
            format!("postgresql://app_user:{secret}@db.example.com:5432/prod?sslmode=require");
        let url = RedactedUrl::new(raw);
        let display = format!("{url}");
        let debug = format!("{url:?}");
        assert!(!display.contains(&secret));
        assert!(!debug.contains(&secret));
        assert!(display.contains("postgresql://app_user:***@db.example.com:5432/prod"));
    }

    #[test]
    fn redacted_url_masks_uri_query_params_and_libpq_key_value_passwords() {
        let pw1 = "s3cr3t_alpha";
        let pw2 = "s3cr3t_beta";
        let pw3 = "s3cr3t_gamma";

        // 1. URI with authority password + query password + sslpassword + trailing `#...`
        // (fails closed on `#...` inside a secret query value so unencoded `#` in passwords
        // never leaks, while preserving `#...` after non-secret query parameters).
        let uri_all = format!(
            "postgresql://app:{pw1}@db.example.com:5432/prod?sslmode=verify-full&password={pw2}&sslpassword={pw3}#frag"
        );
        let red_uri = RedactedUrl::new(uri_all).redacted();
        for s in [pw1, pw2, pw3] {
            assert!(!red_uri.contains(s), "URI leaked {s}: {red_uri}");
        }
        assert_eq!(
            red_uri,
            "postgresql://app:***@db.example.com:5432/prod?sslmode=verify-full&password=***&sslpassword=***"
        );
        assert_eq!(
            RedactedUrl::new(format!(
                "postgresql://app:{pw1}@db.example.com:5432/prod?password={pw2}&sslmode=verify-full#frag"
            ))
            .redacted(),
            "postgresql://app:***@db.example.com:5432/prod?password=***&sslmode=verify-full#frag"
        );

        // 2. URI without authority password but with uppercase/mixed-case query secrets.
        let uri_q = format!("postgres://app@db.example.com/prod?PASSWORD={pw1}&SslPassword={pw2}");
        let red_q = RedactedUrl::new(uri_q).redacted();
        assert!(!red_q.contains(pw1) && !red_q.contains(pw2), "{red_q}");
        assert_eq!(
            red_q,
            "postgres://app@db.example.com/prod?PASSWORD=***&SslPassword=***"
        );

        // 3. Libpq key-value unquoted & escaped whitespace values.
        let kv_plain = format!(
            "host=db.example.com port=5432 user=app password={pw1}\\ tail sslpassword={pw2} dbname=prod"
        );
        let red_kv = RedactedUrl::new(kv_plain).redacted();
        assert!(!red_kv.contains(pw1) && !red_kv.contains(pw2), "{red_kv}");
        assert!(
            !red_kv.contains("tail"),
            "escaped space tail leaked: {red_kv}"
        );
        assert_eq!(
            red_kv,
            "host=db.example.com port=5432 user=app password=*** sslpassword=*** dbname=prod"
        );

        // 4. Libpq single-quoted values with spaces, escaped quotes, and whitespace around `=`.
        let kv_quoted = format!(
            "host = db.example.com password = 'my \\'{pw1}\\' pass' sslpassword='x\\\\{pw2}' dbname=prod"
        );
        let red_quoted = RedactedUrl::new(kv_quoted).redacted();
        assert!(
            !red_quoted.contains(pw1) && !red_quoted.contains(pw2),
            "{red_quoted}"
        );
        assert_eq!(
            red_quoted,
            "host = db.example.com password = *** sslpassword=*** dbname=prod"
        );

        // 5. Unterminated single-quoted password redacts to end of string safely.
        let kv_dangling = format!("host=db.example.com password='{pw1} dangling");
        let red_dangling = RedactedUrl::new(kv_dangling).redacted();
        assert!(!red_dangling.contains(pw1), "{red_dangling}");
        assert_eq!(red_dangling, "host=db.example.com password=***");

        // 6. Multi-byte UTF-8 after backslash escapes never panics on char boundaries.
        let kv_utf8 = format!("application_name='a\\é' password='b\\é{pw1}' dbname=prod");
        let red_utf8 = RedactedUrl::new(kv_utf8).redacted();
        assert!(!red_utf8.contains(pw1), "{red_utf8}");
        assert_eq!(red_utf8, "application_name='a\\é' password=*** dbname=prod");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn redacted_url_handles_adversarial_bypasses_and_toml_parse_errors_hide_secrets() {
        let pw1 = "s3cr3t_alpha";
        let pw2 = "s3cr3t_beta";

        // Percent-encoded URI query keys (`%70assword` decoded by sqlx) are masked.
        let uri_pct = format!(
            "postgresql://app@db.example.com:5432/prod?%70assword={pw1}&%73slpassword={pw2}"
        );
        let red_pct = RedactedUrl::new(uri_pct).redacted();
        assert!(
            !red_pct.contains(pw1) && !red_pct.contains(pw2),
            "{red_pct}"
        );

        // 8. Libpq KV strings containing `://` in values or passwords are NOT misclassified as URIs.
        let kv_with_url = format!(
            "host=db.example.com application_name=https://svc.internal password={pw1} dbname=prod"
        );
        let red_kv_url = RedactedUrl::new(kv_with_url).redacted();
        assert!(!red_kv_url.contains(pw1), "{red_kv_url}");
        assert!(red_kv_url.contains("password=***"), "{red_kv_url}");

        let kv_pw_url = format!("host=db.example.com password=foo://{pw1} dbname=prod");
        let red_kv_pw_url = RedactedUrl::new(kv_pw_url).redacted();
        assert!(!red_kv_pw_url.contains(pw1), "{red_kv_pw_url}");
        assert_eq!(
            red_kv_pw_url,
            "host=db.example.com password=*** dbname=prod"
        );

        // 9. Unencoded `/`, `?`, `#`, numeric prefixes, empty prefixes, multi-`@`, leading
        // whitespace, and embedded tabs/newlines never leak credentials, while legitimate `@` in
        // query values stays intact.
        let scheme = "postgresql://";
        let prefix = "part1";
        for bad_uri in [
            format!("{scheme}app:{prefix}/{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:{prefix}?{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:{prefix}#{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:/{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:?{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:#{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:1234/{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:1234?{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:1234#{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:p1@p2/{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:p1/p2@p3/{pw1}@db.example.com:5432/prod"),
            format!(
                "{scheme}app:1234/{pw1}@db.example.com:5432/prod?application_name=ops@corp.com"
            ),
            format!(
                "{scheme}app:p1@p2/{pw1}@db.example.com:5432/prod?application_name=ops@corp.com"
            ),
            format!(
                "{scheme}app:abc/{pw1}@db.example.com/prod?application_name=ops@corp.com&password={pw2}"
            ),
            format!("{scheme}app:abc/{pw1}?application_name=ops@corp.com&password={pw2}"),
            format!("{scheme}app:{pw1}/no-at-sign"),
            format!(" \t{scheme}app:{pw1}@db.example.com:5432/prod "),
            format!("postgresql:\t//app:{pw1}@db.example.com:5432/prod"),
            format!("postgresql:/\n/app:{pw1}@db.example.com:5432/prod"),
            format!(
                "{scheme}app@db.example.com:5432/prod?%7\t0assword={pw1}&%\n73slpassword={pw2}"
            ),
            format!("postgresql:?host=localhost&password={pw1}&sslpassword={pw2}"),
            format!("postgresql:?%7\t0assword={pw1}"),
            format!("postgres:prod?oauth_client_secret={pw1}"),
            format!("postgresql:app:{pw1}@db.example.com:5432/prod"),
            format!("postgresql:/app:{pw1}@db.example.com:5432/prod"),
            format!("postgresql:///app:{pw1}@db.example.com:5432/prod"),
            format!("{scheme}org/app:{pw1}@db.example.com:5432/prod"),
            format!("postgresql:app:1234/{pw1}@db.example.com:5432/prod?sslpassword={pw2}"),
            format!("postgresql:app:1234#{pw1}@db.example.com:5432/prod"),
            format!("postgresql:app:1234?{pw1}@db.example.com:5432/prod"),
            format!("postgresql:app:1234#frag?k={pw1}@db.example.com:5432/prod"),
            format!("postgresql:app:p1@p2#frag?k={pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:1234#frag?k={pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:p1@p2#frag?k={pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:1234?k={pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:p1@p2?k={pw1}@db.example.com:5432/prod"),
            format!("postgresql:///app:{pw1}?k=v@db.example.com:5432/prod"),
            format!("{scheme}app:1234?a=1&{pw1}@db.example.com:5432/prod"),
            format!("{scheme}app:1234/x{pw1}@db.example.com:5432/prod#ops@corp.com"),
            format!("{scheme}app:1234?k={pw1}@db.example.com:5432/prod#ops@corp.com"),
            format!("{scheme}app:1234?k={pw1}@[::1]"),
            format!("{scheme}app:1234/{pw1}@db.example.com"),
            format!("{scheme}app:1234/{pw1}@localhost"),
            format!("{scheme}app:1234/{pw1}@host1,host2"),
            format!("{scheme}app:p1@p2/{pw1}@db.example.com"),
            format!("{scheme}app:1234?{pw1}@db.example.com"),
            format!("{scheme}app:1234#{pw1}@db.example.com"),
            format!("{scheme}app:1234/{pw1}@db.example.com#frag"),
            format!("postgresql:///app:{pw1}@db.example.com"),
            format!("{scheme}org/app:{pw1}@db.example.com"),
            format!("{scheme}app@db.example.com:5432/prod?password=p1#{pw1}"),
            format!("{scheme}app@db.example.com:5432/prod?application_name=svc#1&password={pw1}"),
            format!("{scheme}app@db.example.com:5432/my#db?password={pw1}"),
            format!("{scheme}app@db.example.com:5432/prod?a=1#frag?password={pw1}"),
            format!("{scheme} host=localhost password={pw1}"),
            format!("{scheme}db.example.com password={pw1}"),
            format!("{scheme}db.example.com/prod password={pw1}"),
            format!("{scheme}app:+1234/no-at-sign"),
            format!("{scheme}[app:{pw1}]/no-at-sign"),
            format!("{scheme}[::1%{pw1}:x]/no-at-sign"),
            format!("foo:bar password={pw1}"),
            format!("foo:bar\tpassword={pw1}"),
            format!("db.example.com:5432 password={pw1}"),
            format!("db.example.com:5432\npassword={pw1}"),
            format!("postgresql: host=localhost password={pw1}"),
            format!("postgresql: host=localhost\tpassword={pw1}"),
            format!("foo:bar?baz=1 password={pw1}"),
            format!("foo:bar?baz=1\tpassword={pw1}"),
            format!(
                "{scheme}app@db.example.com:5432/prod?ssl\tpassword={pw1}&ssl-password={pw2}&oauth_client_secret={pw1}"
            ),
        ] {
            let red = RedactedUrl::new(bad_uri).redacted();
            assert!(!red.contains(pw1) && !red.contains(pw2), "leaked: {red}");
        }
        assert_eq!(
            RedactedUrl::new(format!(
                "{scheme}app:1234/{pw1}@db.example.com:5432/prod?application_name=ops@corp.com"
            ))
            .redacted(),
            "postgresql://app:***@db.example.com:5432/prod?application_name=ops@corp.com"
        );
        assert_eq!(
            RedactedUrl::new(format!(
                "{scheme}app:abc/{pw1}@db.example.com/prod?application_name=ops@corp.com&password={pw2}"
            ))
            .redacted(),
            "postgresql://app:***@db.example.com/prod?application_name=ops@corp.com&password=***"
        );
        for safe_uri in [
            format!("{scheme}db.example.com:5432/prod?application_name=ops@corp.com"),
            format!("{scheme}db.example.com:5432?application_name=ops@corp.com"),
            format!("{scheme}db.example.com:5432/prod?sslmode=verify-full#ops@corp.com"),
            format!("{scheme}app@db.example.com:5432/prod#ops@corp.com"),
            format!("{scheme}db.example.com/prod?application_name=svc:1#ops@corp.com"),
            format!("{scheme}db.example.com:5432/prod@v2"),
            format!("{scheme}db.example.com:5432/prod@v2?sslmode=verify-full"),
            format!("{scheme}app@db.example.com:5432/prod@v2?sslmode=verify-full"),
            format!("{scheme}db.example.com:5432/prod?application_name=worker@node1:8080"),
            format!("{scheme}db.example.com:5432/prod?application_name=ops@corp.com/team"),
            format!("{scheme}db.example.com:5432/prod#ops@corp.com/team"),
            format!("{scheme}[::1]:5432/prod?sslmode=verify-full"),
            format!("{scheme}:5432/prod"),
            "postgresql:///prod?host=/var/run/postgresql&application_name=ops@corp.com".to_owned(),
            "postgresql:///prod?host=/var/run/postgresql&application_name=svc:1&options=-c%20app.email=ops@corp.com/team".to_owned(),
        ] {
            assert_eq!(RedactedUrl::new(safe_uri.clone()).redacted(), safe_uri);
        }
        assert_eq!(
            RedactedUrl::new(format!(
                "{scheme}app:{pw1}@db.example.com:5432/prod#ops@corp.com"
            ))
            .redacted(),
            "postgresql://app:***@db.example.com:5432/prod#ops@corp.com"
        );
        assert_eq!(
            RedactedUrl::new(format!(
                "{scheme}app:{pw1}@db.example.com:5432/prod?application_name=worker@node1:8080"
            ))
            .redacted(),
            "postgresql://app:***@db.example.com:5432/prod?application_name=worker@node1:8080"
        );

        // Linear-time behavior on adversarial repeated-delimiter inputs.
        for huge in [
            format!("{scheme}host:5432/db?k={}", "@".repeat(50_000)),
            format!("{scheme}host/db/{}", "@".repeat(50_000)),
            format!("{scheme}a:b/{}", "@".repeat(50_000)),
            format!("{scheme}host/db?{}", "#?".repeat(25_000)),
        ] {
            let red = RedactedUrl::new(huge).redacted();
            assert_ne!(red, "");
        }

        // 10. Libpq KV empty values, vertical tabs (`\x0b`), adjacent quoted secrets, malformed
        // quotes, and unclosed/misaligned quotes before `password=` never leak.
        for kv_case in [
            format!("password={pw1}"),
            format!(" password={pw1} "),
            format!("sslpassword={pw1}"),
            format!("ssl-password={pw1}"),
            format!("oauth_client_secret={pw1}"),
            format!("password='{pw1}'"),
            format!("password=foo://{pw1}"),
            format!("host=db.example.com\x0bpassword={pw1}"),
            format!("host=db.example.com password\x0b={pw1}"),
            format!("host=db.example.com password=\x0bsslpassword\x0b=\x0b{pw2}"),
            format!("host=db.example.com sslrootcert= password={pw1} dbname=prod"),
            format!("host=db.example.com sslrootcert= password = {pw1} dbname=prod"),
            format!("host=db.example.com password= sslpassword = {pw2} dbname=prod"),
            format!("host=db.example.com sslpassword= password = {pw1} dbname=prod"),
            format!("host=db.example.com password= sslpassword='first {pw2}' dbname=prod"),
            format!("host=db.example.com password='{pw1}'sslpassword = {pw2} dbname=prod"),
            format!("host=db.example.com password='{pw1}'sslpassword='first {pw2}' dbname=prod"),
            format!("host=db.example.com password='{pw1}'dbname='prod'sslpassword = {pw2}"),
            format!("host=db.example.com password='{pw1}'dbname='prod'sslpassword='first {pw2}'"),
            format!("host=db.example.com password='{pw1}'dbname='my db'sslpassword={pw2}"),
            format!("host=db.example.com password='pa'{pw1}' dbname=prod"),
            format!("host='localhost'password={pw1} dbname=prod"),
            format!("host='localhost'password = {pw1} dbname=prod"),
            format!("application_name='broken password={pw1} dbname=prod"),
            format!("application_name='broken password='{pw1}' dbname=prod"),
            format!("application_name='broken password='{pw1}==' dbname=prod"),
            format!("application_name='broken password={pw1} dbname='prod'"),
            format!("application_name='broken password={pw1} dbname='password={pw2}'"),
        ] {
            let red = RedactedUrl::new(kv_case).redacted();
            assert!(
                !red.contains(pw1) && !red.contains(pw2),
                "leaked in libpq KV: {red}"
            );
        }

        // 11. Non-secret libpq parameters and URIs remain untouched.
        let safe_kv = "host=localhost port=5432 user=postgres dbname=app sslmode=verify-full";
        assert_eq!(RedactedUrl::new(safe_kv.to_owned()).redacted(), safe_kv);

        // 12. TOML syntax/unknown-field errors never echo `database_url` credentials.
        let dir = tempdir().unwrap();
        let toml_secret = "TOML_CONFIG_SECRET_999";
        fs::write(
            dir.path().join("auditstream.toml"),
            format!("databse_url = \"postgresql://app:{toml_secret}@db.example.com/prod\"\n"),
        )
        .unwrap();
        let cli = Cli::try_parse_from(["auditstream", "status"]).unwrap();
        let err = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap_err();
        assert!(
            !err.to_string().contains(toml_secret),
            "TOML parse error leaked secret: {err}"
        );
    }

    #[test]
    fn loads_and_validates_export_redaction_rules() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auditstream.toml"),
            r#"
[[export.redaction.rules]]
table = "public.invoice"
columns = ["ssn", "tax_id"]
mode = "mask"

[[export.redaction.rules]]
action = "invoice.exported"
columns = ["recipient_email"]
mode = "remove"
"#,
        )
        .unwrap();

        let cli = Cli::try_parse_from(["auditstream", "export"]).unwrap();
        let cfg = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap();
        assert_eq!(cfg.export.redaction.rules.len(), 2);
        assert!(!cfg.export.compiled_redaction.is_empty());

        for (toml_snippet, expected_msg) in [
            (
                "[[export.redaction.rules]]\ntable = \"invoice\"\ncolumns = []\n",
                "at least one column",
            ),
            (
                "[[export.redaction.rules]]\ntable = \"invoice\"\ncolumns = [\"metadata.ssn\"]\n",
                "nested JSON path",
            ),
            (
                "[[export.redaction.rules]]\ntable = \"invoice\"\naction = \"invoice.sent\"\ncolumns = [\"ssn\"]\n",
                "both `table` and `action`",
            ),
            (
                "[[export.redaction.rules]]\ncolumns = [\"ssn\"]\n",
                "must set either `table`",
            ),
            (
                "[[export.redaction.rules]]\ntable = \"invoice\"\ncolumns = [\"ssn\"]\nunknown_field = true\n",
                "unknown_field",
            ),
        ] {
            fs::write(dir.path().join("auditstream.toml"), toml_snippet).unwrap();
            let cli = Cli::try_parse_from(["auditstream", "export"]).unwrap();
            let err = Config::load_with_fallback_env(&cli, dir.path(), None).unwrap_err();
            assert_eq!(exit_code_for(&err), Exit::Usage, "{toml_snippet}");
            assert!(
                err.to_string().contains(expected_msg),
                "expected {expected_msg:?} in {err}"
            );
        }
    }
}
