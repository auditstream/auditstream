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

/// Parses a single `<schema>.<table>` specifier using `label` in error messages.
pub(crate) fn parse_table_ref(raw: &str, label: &str) -> anyhow::Result<(String, String)> {
    let mut parts = raw.split('.');
    let (Some(schema), Some(table), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(anyhow!(
            "invalid {label} `{raw}`: expected `<schema>.<table>`"
        ))
        .exit_code(Exit::Usage);
    };
    if schema.is_empty() || table.is_empty() {
        return Err(anyhow!(
            "invalid {label} `{raw}`: expected `<schema>.<table>`"
        ))
        .exit_code(Exit::Usage);
    }
    Ok((schema.to_owned(), table.to_owned()))
}

/// Parses a list of `<schema>.<table>` specifiers.
pub(crate) fn parse_table_refs(
    raw: &[String],
    in_config: bool,
) -> anyhow::Result<Vec<(String, String)>> {
    let suffix = if in_config { " in config" } else { "" };
    let mut parsed = Vec::with_capacity(raw.len());
    for entry in raw {
        let mut parts = entry.split('.');
        let (Some(schema), Some(table), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(anyhow!(
                "invalid table specifier `{entry}`{suffix}: expected `<schema>.<table>`"
            ))
            .exit_code(Exit::Usage);
        };
        if schema.is_empty() || table.is_empty() {
            return Err(anyhow!(
                "invalid table specifier `{entry}`{suffix}: expected `<schema>.<table>`"
            ))
            .exit_code(Exit::Usage);
        }
        parsed.push((schema.to_owned(), table.to_owned()));
    }
    Ok(parsed)
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
