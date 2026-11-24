//! Configuration file loading and precedence resolution.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::ValueEnum;
use serde::Deserialize;

use crate::cli::{Cli, Command};
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

fn redact_url_password(raw: &str) -> String {
    let Some(scheme_end) = raw.find("://") else {
        return raw.to_owned();
    };
    let rest = &raw[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at_pos) = authority.rfind('@') else {
        return raw.to_owned();
    };
    let userinfo = &authority[..at_pos];
    let Some(colon_pos) = userinfo.find(':') else {
        return raw.to_owned();
    };

    let prefix = &raw[..=(scheme_end + 3 + colon_pos)];
    let suffix = &rest[at_pos..];
    format!("{prefix}***{suffix}")
}

/// Resolved CLI configuration (public contract: `docs/reference/config.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    /// Resolved configuration file path (`--config` / `AUDITSTREAM_CONFIG` or `./auditstream.toml`).
    pub(crate) file_path: PathBuf,
    /// Whether `-q` / `--quiet` was passed on the command line.
    #[allow(dead_code)]
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
    /// Sink format name (placeholder until C2's `SinkConfig` lands in F5).
    pub(crate) format: Option<String>,
    /// Output path (defaults to stdout when `None`).
    pub(crate) output: Option<PathBuf>,
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
    sink: Option<toml::Value>,
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
        let mut export = ExportConfig {
            cursor_file: file.export.cursor_file,
            batch_size: file.export.batch_size.unwrap_or(DEFAULT_BATCH_SIZE),
            format: file
                .export
                .sink
                .as_ref()
                .and_then(|val| val.get("type"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            output: None,
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
                if let Some(format) = &args.format {
                    export.format = Some(format.clone());
                }
                if let Some(output) = &args.output {
                    export.output = Some(output.clone());
                }
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
        .map_err(|err| anyhow!("invalid config in {}: {err}", path.display()))
        .exit_code(Exit::Usage)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

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
}
