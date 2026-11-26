//! Export sinks (JSON Lines, CSV), closed sink configuration, and batch export pipeline.

pub mod csv;
pub mod error;
pub mod jsonl;
pub mod pipeline;

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use auditstream_core::{Batch, Sink, SinkError};
use serde::{Deserialize, Serialize};
use tokio as _;

pub(crate) fn open_append_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let file_type = file.metadata()?.file_type();
        if file_type.is_dir() || file_type.is_fifo() || file_type.is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "refusing to write export to non-file target {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(file)
}

macro_rules! impl_opened_sink {
    ($ty:ty) => {
        impl ::auditstream_core::Sink for $ty {
            async fn write(
                &mut self,
                batch: &::auditstream_core::Batch,
            ) -> Result<(), ::auditstream_core::SinkError> {
                match self {
                    Self::Stdout(sink) => sink.write(batch).await,
                    Self::File(sink) => sink.write(batch).await,
                }
            }

            async fn flush(&mut self) -> Result<(), ::auditstream_core::SinkError> {
                match self {
                    Self::Stdout(sink) => sink.flush().await,
                    Self::File(sink) => {
                        sink.flush().await?;
                        sink.get_mut().sync_data().await?;
                        Ok(())
                    }
                }
            }
        }
    };
}
pub(crate) use impl_opened_sink;

pub use crate::csv::{CSV_HEADERS, CsvSink, OpenedCsvSink};
pub use crate::error::ExportError;
pub use crate::jsonl::{JsonLinesSink, OpenedJsonLinesSink};
pub use crate::pipeline::{DEFAULT_BATCH_SIZE, Options, Report, run};

/// Crate identifier retained for workspace smoke checks.
pub const CRATE_NAME: &str = "auditstream-export";

/// Re-verify dependency wiring to `auditstream-core`.
pub fn core_crate_name() -> &'static str {
    auditstream_core::CRATE_NAME
}

/// Closed configuration enum selecting one built-in export sink.
///
/// Deserialized with `#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]`
/// so unknown sink types and unknown fields fail fast with a clear error message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SinkConfig {
    /// JSON Lines (`jsonl`) sink (`spec/export.md`). Writes to stdout when `path` is absent.
    Jsonl {
        /// Optional destination file path; writes to stdout when `None`.
        path: Option<PathBuf>,
    },
    /// RFC 4180 CSV (`csv`) sink (`spec/export.md`). Writes to stdout when `path` is absent.
    Csv {
        /// Optional destination file path; writes to stdout when `None`.
        path: Option<PathBuf>,
    },
}

/// Closed runtime sink dispatcher implementing [`Sink`] via exhaustive `match` without boxing.
#[derive(Debug)]
pub enum AnySink {
    /// JSON Lines sink.
    Jsonl(OpenedJsonLinesSink),
    /// RFC 4180 CSV sink.
    Csv(OpenedCsvSink),
}

impl AnySink {
    /// Constructs an [`AnySink`] from a [`SinkConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`SinkError`] if opening the underlying sink fails.
    pub fn open(config: &SinkConfig) -> Result<Self, SinkError> {
        match config {
            SinkConfig::Jsonl { path } => {
                Ok(Self::Jsonl(OpenedJsonLinesSink::open(path.as_deref())?))
            }
            SinkConfig::Csv { path } => Ok(Self::Csv(OpenedCsvSink::open(path.as_deref())?)),
        }
    }
}

impl Sink for AnySink {
    async fn write(&mut self, batch: &Batch) -> Result<(), SinkError> {
        match self {
            Self::Jsonl(sink) => sink.write(batch).await,
            Self::Csv(sink) => sink.write(batch).await,
        }
    }

    async fn flush(&mut self) -> Result<(), SinkError> {
        match self {
            Self::Jsonl(sink) => sink.flush().await,
            Self::Csv(sink) => sink.flush().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::PathBuf;

    use auditstream_core::{Batch, Sink};

    use super::{AnySink, SinkConfig};

    fn assert_send<F: Future + Send>(_: &F) {}

    #[test]
    fn deserializes_jsonl_and_csv_variants_from_toml() {
        let jsonl_stdout: SinkConfig = toml::from_str("type = \"jsonl\"\n").expect("parse jsonl");
        assert_eq!(jsonl_stdout, SinkConfig::Jsonl { path: None });

        let jsonl_file: SinkConfig =
            toml::from_str("type = \"jsonl\"\npath = \"/tmp/audit.jsonl\"\n")
                .expect("parse jsonl with path");
        assert_eq!(
            jsonl_file,
            SinkConfig::Jsonl {
                path: Some(PathBuf::from("/tmp/audit.jsonl"))
            }
        );

        let csv_stdout: SinkConfig = toml::from_str("type = \"csv\"\n").expect("parse csv");
        assert_eq!(csv_stdout, SinkConfig::Csv { path: None });

        let csv_file: SinkConfig = toml::from_str("type = \"csv\"\npath = \"/tmp/audit.csv\"\n")
            .expect("parse csv with path");
        assert_eq!(
            csv_file,
            SinkConfig::Csv {
                path: Some(PathBuf::from("/tmp/audit.csv"))
            }
        );
    }

    #[test]
    fn rejects_unknown_type_and_unknown_field_in_toml() {
        let bad_type = toml::from_str::<SinkConfig>("type = \"parquet\"\n")
            .expect_err("unknown sink type must fail");
        assert!(
            bad_type.to_string().contains("parquet"),
            "error must name unknown type: {bad_type}"
        );

        let bad_field = toml::from_str::<SinkConfig>("type = \"jsonl\"\ncompression = \"gzip\"\n")
            .expect_err("unknown field must fail");
        assert!(
            bad_field.to_string().contains("compression"),
            "error must name unknown field: {bad_field}"
        );
    }

    #[tokio::test]
    async fn any_sink_dispatches_to_jsonl_and_csv() {
        let batch = Batch::default();

        let mut jsonl = AnySink::open(&SinkConfig::Jsonl { path: None }).expect("open jsonl");
        let write_fut = jsonl.write(&batch);
        assert_send(&write_fut);
        write_fut.await.expect("empty jsonl batch write succeeds");
        jsonl.flush().await.expect("jsonl stdout flush succeeds");

        let mut csv = AnySink::open(&SinkConfig::Csv { path: None }).expect("open csv");
        let write_fut = csv.write(&batch);
        assert_send(&write_fut);
        write_fut.await.expect("empty csv batch write succeeds");
        csv.flush().await.expect("csv stdout flush succeeds");
    }

    #[cfg(unix)]
    #[test]
    fn open_append_file_creates_export_files_with_0600_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        for filename in ["events.jsonl", "events.csv"] {
            let path = dir.path().join(filename);
            let _file = super::open_append_file(&path).expect("open append file");
            let mode = std::fs::metadata(&path)
                .expect("stat file")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o600,
                "export file {filename} must be created with mode 0600, got {mode:o}"
            );
        }

        // Symlinked output path must be rejected via O_NOFOLLOW without modifying the target.
        let victim = dir.path().join("victim.jsonl");
        std::fs::write(&victim, "KEEP\n").expect("write victim");
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&victim, &link).expect("create symlink");
        let err = super::open_append_file(&link).expect_err("symlink output must be rejected");
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ELOOP),
            "expected ELOOP from O_NOFOLLOW, got {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("read victim"),
            "KEEP\n"
        );
    }
}
