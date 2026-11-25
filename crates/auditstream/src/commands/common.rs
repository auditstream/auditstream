//! Shared runtime, database connection, specifier parsing, and trigger-spec helpers
//! reused across CLI subcommands.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use anyhow::anyhow;
use auditstream_pg::PgPool;
use auditstream_pg::introspect::{TableInfo, TableKind};
use auditstream_pg::triggers::TableTriggerSpec;
use auditstream_pg::verify::InstalledTableCapture;

use crate::config::{Config, RedactedUrl};
use crate::exit::{Exit, ExitCodeExt};

/// Runs an async command body on a single-threaded Tokio runtime.
pub(crate) fn run_async<F, T>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| anyhow!("failed to start async runtime: {err}"))
        .exit_code(Exit::Internal)?;

    runtime.block_on(fut)
}

/// Returns the configured database URL or a usage error (exit 2) when missing.
pub(crate) fn require_database_url(config: &Config) -> anyhow::Result<&RedactedUrl> {
    config.database_url.as_ref().ok_or_else(|| {
        crate::exit::tag_error(
            anyhow!(
                "missing database URL: pass --database-url or set AUDITSTREAM_DATABASE_URL / DATABASE_URL"
            ),
            Exit::Usage,
        )
    })
}

/// Connects to PostgreSQL using `db_url`, redacting any embedded password on error.
pub(crate) async fn connect_database(db_url: &RedactedUrl) -> anyhow::Result<PgPool> {
    auditstream_pg::connect(db_url.expose())
        .await
        .map_err(|err| anyhow!("could not connect to database {}: {err}", db_url.redacted()))
        .exit_code(Exit::Database)
}

/// Default PostgreSQL schema assumed for unqualified CLI `--table <name>` flags.
const DEFAULT_TABLE_SCHEMA: &str = "public";

fn parse_single_table(raw: &str, allow_default_schema: bool) -> Option<(String, String)> {
    let mut parts = raw.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(table), None, None) if allow_default_schema && !table.is_empty() => {
            Some((DEFAULT_TABLE_SCHEMA.to_owned(), table.to_owned()))
        }
        (Some(schema), Some(table), None) if !schema.is_empty() && !table.is_empty() => {
            Some((schema.to_owned(), table.to_owned()))
        }
        _ => None,
    }
}

/// Parses a single CLI `[schema.]table` specifier (defaulting unqualified names to `public`).
pub(crate) fn parse_table_ref(raw: &str, label: &str) -> anyhow::Result<(String, String)> {
    parse_single_table(raw, true).ok_or_else(|| {
        crate::exit::tag_error(
            anyhow!("invalid {label} `{raw}`: expected `<table>` or `<schema>.<table>`"),
            Exit::Usage,
        )
    })
}

/// Parses a list of table specifiers.
///
/// CLI entries (`in_config == false`) default unqualified `<table>` to `public.<table>`, while
/// configuration file entries (`in_config == true`) require explicit `<schema>.<table>`.
pub(crate) fn parse_table_refs(
    raw: &[String],
    in_config: bool,
) -> anyhow::Result<Vec<(String, String)>> {
    let allow_default_schema = !in_config;
    let mut parsed = Vec::with_capacity(raw.len());
    for entry in raw {
        let Some(pair) = parse_single_table(entry, allow_default_schema) else {
            let msg = if in_config {
                format!("invalid table specifier `{entry}` in config: expected `<schema>.<table>`")
            } else {
                format!(
                    "invalid table specifier `{entry}`: expected `<table>` or `<schema>.<table>`"
                )
            };
            return Err(anyhow!(msg)).exit_code(Exit::Usage);
        };
        parsed.push(pair);
    }
    Ok(parsed)
}

const fn is_bidi_control(ch: char) -> bool {
    matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn skip_csi_body(chars: &mut std::str::Chars<'_>) {
    while matches!(chars.as_str().chars().next(), Some('\x30'..='\x3F')) {
        let _ = chars.next();
    }
    while matches!(chars.as_str().chars().next(), Some('\x20'..='\x2F')) {
        let _ = chars.next();
    }
    if matches!(chars.as_str().chars().next(), Some('\x40'..='\x7E')) {
        let _ = chars.next();
    }
}

fn skip_string_sequence(chars: &mut std::str::Chars<'_>) {
    while let Some(c) = chars.as_str().chars().next() {
        match c {
            '\x07' | '\u{009C}' => {
                let _ = chars.next();
                break;
            }
            '\x1b' => {
                if chars.as_str().starts_with("\x1b\\") {
                    let _ = chars.next();
                    let _ = chars.next();
                }
                break;
            }
            _ => {
                let _ = chars.next();
            }
        }
    }
}

fn skip_esc_sequence(chars: &mut std::str::Chars<'_>) {
    let Some(next) = chars.as_str().chars().next() else {
        return;
    };
    match next {
        '[' => {
            let _ = chars.next();
            skip_csi_body(chars);
        }
        ']' | 'P' | 'X' | '^' | '_' => {
            let _ = chars.next();
            skip_string_sequence(chars);
        }
        '\x20'..='\x2F' => {
            while matches!(chars.as_str().chars().next(), Some('\x20'..='\x2F')) {
                let _ = chars.next();
            }
            if matches!(chars.as_str().chars().next(), Some('\x30'..='\x7E')) {
                let _ = chars.next();
            }
        }
        '\x30'..='\x7E' => {
            let _ = chars.next();
        }
        _ => {}
    }
}

/// Neutralizes untrusted text for safe terminal rendering.
///
/// Strips ANSI CSI/OSC/DCS/SOS/PM/APC/ESC escape sequences (both 7-bit and 8-bit C1 forms),
/// strips all other C0/C1 control characters and Unicode bidirectional override/isolate controls
/// (`U+202A..=U+202E`, `U+2066..=U+2069`), and maps `\n`, `\r`, and `\t` to spaces.
pub(crate) fn sanitize_terminal(raw: &str) -> String {
    if !raw.chars().any(|ch| ch.is_control() || is_bidi_control(ch)) {
        return raw.to_owned();
    }

    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => skip_esc_sequence(&mut chars),
            '\u{009B}' => skip_csi_body(&mut chars),
            '\u{0090}' | '\u{0098}' | '\u{009D}' | '\u{009E}' | '\u{009F}' => {
                skip_string_sequence(&mut chars);
            }
            '\n' | '\r' | '\t' => out.push(' '),
            other if other.is_control() || is_bidi_control(other) => {}
            other => out.push(other),
        }
    }

    out
}

/// Parses a list of `<schema>.<table>.<column>` excluded column specifiers.
pub(crate) fn parse_exclude_refs(
    raw: &[String],
    in_config: bool,
) -> anyhow::Result<Vec<(String, String, String)>> {
    let suffix = if in_config { " in config" } else { "" };
    let mut parsed = Vec::with_capacity(raw.len());
    for entry in raw {
        let mut parts = entry.split('.');
        let (Some(schema), Some(table), Some(column), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(anyhow!(
                "invalid excluded column specifier `{entry}`{suffix}: expected `<schema>.<table>.<column>`"
            ))
            .exit_code(Exit::Usage);
        };
        if schema.is_empty() || table.is_empty() || column.is_empty() {
            return Err(anyhow!(
                "invalid excluded column specifier `{entry}`{suffix}: expected `<schema>.<table>.<column>`"
            ))
            .exit_code(Exit::Usage);
        }
        parsed.push((schema.to_owned(), table.to_owned(), column.to_owned()));
    }
    Ok(parsed)
}

/// Groups `(schema, table, column)` tuples into a sorted per-table exclusion map.
pub(crate) fn group_excludes_by_table(
    excludes: &[(String, String, String)],
) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut grouped: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (schema, table, col) in excludes {
        grouped
            .entry((schema.clone(), table.clone()))
            .or_default()
            .insert(col.clone());
    }
    grouped
}

/// Builds a [`TableTriggerSpec`], marking it partitioned when `is_partitioned` is `true`.
pub(crate) fn table_trigger_spec(
    schema: String,
    table: String,
    primary_key: Vec<String>,
    excluded_columns: Vec<String>,
    is_partitioned: bool,
) -> TableTriggerSpec {
    let mut spec = TableTriggerSpec::new(schema, table, primary_key, excluded_columns);
    if is_partitioned {
        spec = spec.partitioned();
    }
    spec
}

/// Builds expected [`TableTriggerSpec`]s for installed captures using live catalog metadata
/// when available and falling back to the installed trigger arguments otherwise.
pub(crate) fn build_installed_verify_specs(
    installed: &[InstalledTableCapture],
    introspected_map: &BTreeMap<(String, String), &TableInfo>,
) -> Vec<TableTriggerSpec> {
    installed
        .iter()
        .map(|cap| {
            let key = (cap.schema.clone(), cap.table.clone());
            let (pk, is_part) = introspected_map.get(&key).map_or_else(
                || (cap.primary_key.clone(), cap.is_partitioned),
                |info| {
                    (
                        info.primary_key.clone(),
                        info.kind == TableKind::Partitioned,
                    )
                },
            );
            table_trigger_spec(
                cap.schema.clone(),
                cap.table.clone(),
                pk,
                cap.excluded_columns.clone(),
                is_part,
            )
        })
        .collect()
}

/// Builds expected [`TableTriggerSpec`]s for configured tables using live catalog metadata.
pub(crate) fn build_configured_verify_specs(
    configured_set: &BTreeSet<(String, String)>,
    configured_excludes: &BTreeMap<(String, String), BTreeSet<String>>,
    introspected_map: &BTreeMap<(String, String), &TableInfo>,
) -> Vec<TableTriggerSpec> {
    configured_set
        .iter()
        .map(|key| {
            let excl: Vec<String> = configured_excludes
                .get(key)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default();
            let (pk, is_part) = introspected_map.get(key).map_or_else(
                || (Vec::new(), false),
                |info| {
                    (
                        info.primary_key.clone(),
                        info.kind == TableKind::Partitioned,
                    )
                },
            );
            table_trigger_spec(key.0.clone(), key.1.clone(), pk, excl, is_part)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_exclude_refs, parse_table_ref, parse_table_refs, sanitize_terminal};
    use crate::exit::{Exit, exit_code_for};

    #[test]
    fn sanitize_terminal_strips_escapes_controls_and_bidi_while_preserving_safe_text() {
        assert_eq!(
            sanitize_terminal("plain José — €10.50 ✓"),
            "plain José — €10.50 ✓"
        );
        assert_eq!(sanitize_terminal("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize_terminal("\x1b[2J\x1b[Hhello"), "hello");
        assert_eq!(sanitize_terminal("\u{009B}31mcsi8\u{009B}0m"), "csi8");
        assert_eq!(sanitize_terminal("\x1b]52;c;YWJj\x07safe"), "safe");
        assert_eq!(
            sanitize_terminal("\x1b]8;;https://evil.example\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(sanitize_terminal("\u{009D}0;title\u{009C}ok"), "ok");
        assert_eq!(sanitize_terminal("prefix\x1b]52;c;unterminated"), "prefix");
        assert_eq!(sanitize_terminal("\x1bP1$r\x1b\\dcs"), "dcs");
        assert_eq!(sanitize_terminal("\x1bcreset\x1b(Bdone\x1b"), "resetdone");
        assert_eq!(sanitize_terminal("a\rb\nc\td"), "a b c d");
        assert_eq!(sanitize_terminal("ok\x08\x07\x00\x7f\u{0085}!"), "ok!");
        assert_eq!(
            sanitize_terminal(
                "user\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}name\u{2066}\u{2067}\u{2068}\u{2069}"
            ),
            "username"
        );
        // Deeply repeated unterminated OSC sequences must not overflow the stack.
        let nested = format!("{}tail", "\x1b]".repeat(20_000));
        assert_eq!(sanitize_terminal(&nested), "");
    }

    #[test]
    fn cli_table_refs_default_unqualified_names_to_public_while_config_stays_strict() {
        assert_eq!(
            parse_table_ref("invoice", "--table").unwrap(),
            ("public".to_owned(), "invoice".to_owned())
        );
        assert_eq!(
            parse_table_ref("billing.invoice", "--table").unwrap(),
            ("billing".to_owned(), "invoice".to_owned())
        );
        assert_eq!(
            parse_table_refs(&["invoice".to_owned(), "billing.order".to_owned()], false).unwrap(),
            vec![
                ("public".to_owned(), "invoice".to_owned()),
                ("billing".to_owned(), "order".to_owned()),
            ]
        );

        // Config entries remain explicit `<schema>.<table>`.
        let cfg_err = parse_table_refs(&["invoice".to_owned()], true).unwrap_err();
        assert_eq!(exit_code_for(&cfg_err), Exit::Usage);

        // Invalid syntax is always rejected.
        for bad in ["", ".", ".invoice", "public.", "a.b.c"] {
            assert_eq!(
                exit_code_for(&parse_table_ref(bad, "--table").unwrap_err()),
                Exit::Usage,
                "bad={bad:?}"
            );
            assert_eq!(
                exit_code_for(&parse_table_refs(&[bad.to_owned()], false).unwrap_err()),
                Exit::Usage,
                "bad={bad:?}"
            );
        }

        // Excluded columns stay strict `<schema>.<table>.<column>`.
        for bad_excl in ["invoice.secret", "", "public.invoice.", "a.b.c.d"] {
            assert_eq!(
                exit_code_for(&parse_exclude_refs(&[bad_excl.to_owned()], false).unwrap_err()),
                Exit::Usage,
                "bad_excl={bad_excl:?}"
            );
        }
    }
}
