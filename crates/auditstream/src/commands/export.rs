//! Export committed events and checkpoint each batch after its destination flushes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, anyhow};
use auditstream_core::{
    Batch, Cursor, EventSource, Redaction, Sink, SinkError, Source, SourceError,
};
use auditstream_export::{AnySink, ExportError, Options, SinkConfig, SinkFingerprint};
use auditstream_pg::reader::{
    self, BoundaryAnchor, DatabaseIdentity, ExportFilter, ExportLag, PgEventSource,
    ResumeVerification,
};
use clap::{Args as ClapArgs, ValueEnum};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::cli::LogFormat;
use crate::commands::common::{connect_database, require_database_url, run_async};
use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    Jsonl,
    Csv,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum StatsFormat {
    /// Human-readable single-line summary on stderr (default).
    #[default]
    Text,
    /// Structured single-line JSON telemetry summary on stderr.
    Json,
}

#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Destination encoding (default: jsonl).
    #[arg(long, value_enum)]
    pub(crate) format: Option<Format>,
    /// Append to this file (default: stdout).
    #[arg(long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,
    /// Persist the last acknowledged position and resume on the next run.
    #[arg(long, value_name = "PATH")]
    pub(crate) cursor_file: Option<PathBuf>,
    /// Maximum events per batch (default: 1000).
    #[arg(long, value_name = "N")]
    pub(crate) batch_size: Option<usize>,
    /// Replay from the beginning and replace the stored cursor.
    #[arg(long)]
    from_start: bool,
    /// Allow exporting when Row-Level Security filters rows on `<schema>.changes` (for intentional per-tenant exports).
    #[arg(long)]
    allow_rls_filtered: bool,
    /// Format for the stderr summary (`text` or `json`; defaults to `json` with `--log-format json`, else `text`).
    #[arg(long, value_enum)]
    pub(crate) stats_format: Option<StatsFormat>,
    #[command(flatten)]
    filters: Filters,
}

#[derive(Debug, Clone, Default, ClapArgs, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    /// Include events at or after this RFC 3339 timestamp or UTC date.
    #[arg(long, value_name = "TIME", value_parser = parse_time)]
    #[serde(default, with = "time::serde::rfc3339::option")]
    since: Option<OffsetDateTime>,
    /// Include events strictly before this RFC 3339 timestamp or UTC date.
    #[arg(long, value_name = "TIME", value_parser = parse_time)]
    #[serde(default, with = "time::serde::rfc3339::option")]
    until: Option<OffsetDateTime>,
    /// Match an actor ID directly or through onBehalfOf.
    #[arg(long, value_name = "ID")]
    actor: Option<String>,
    /// Match a tenant ID directly or through onBehalfOf.
    #[arg(long, value_name = "ID")]
    #[serde(default)]
    tenant: Option<String>,
    /// Select database changes or application events.
    #[arg(long, value_name = "db|app", value_parser = parse_source)]
    source: Option<Source>,
}

fn parse_time(raw: &str) -> Result<OffsetDateTime, &'static str> {
    let timestamp = if raw.len() == 10 {
        format!("{raw}T00:00:00Z")
    } else {
        raw.to_owned()
    };
    OffsetDateTime::parse(&timestamp, &Rfc3339)
        .map(|time| time.to_offset(UtcOffset::UTC))
        .map_err(|_| "expected an RFC 3339 timestamp or YYYY-MM-DD")
}

fn parse_source(raw: &str) -> Result<Source, &'static str> {
    match raw {
        "db" => Ok(Source::Db),
        "app" => Ok(Source::App),
        _ => Err("expected db or app"),
    }
}

impl Filters {
    fn reader_filter(&self) -> ExportFilter {
        ExportFilter {
            from: self.since,
            until: self.until,
            actor_or_on_behalf_of: self.actor.clone(),
            tenant_or_on_behalf_of: self.tenant.clone(),
            source: self.source,
            ..ExportFilter::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    cursor: Cursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<BoundaryAnchor>,
    database: Database,
    schema: String,
    filters: Filters,
    sink: SinkFingerprint,
    output_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Redaction::is_empty")]
    redaction: Redaction,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    allow_rls_filtered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Database {
    #[serde(default)]
    system_identifier: Option<String>,
    name: String,
    oid: u32,
    #[serde(default)]
    history_table_oid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline_id: Option<u32>,
}

impl Database {
    /// Checks cluster, database, and history-table identity independent of `timeline_id`.
    ///
    /// Timeline transitions (such as planned zero-loss HA failovers or upgrading a legacy
    /// checkpoint that predates `timeline_id`) are validated separately via
    /// [`reader::verify_resume_state`].
    fn same_identity(&self, other: &Self) -> bool {
        self.system_identifier == other.system_identifier
            && self.name == other.name
            && self.oid == other.oid
            && self.history_table_oid == other.history_table_oid
    }
}

impl From<DatabaseIdentity> for Database {
    fn from(id: DatabaseIdentity) -> Self {
        Self {
            system_identifier: id.system_identifier,
            name: id.name,
            oid: id.oid,
            history_table_oid: id.history_table_oid,
            timeline_id: id.timeline_id,
        }
    }
}

fn sink_path(sink: &SinkConfig) -> Option<&Path> {
    sink.output_path()
}

fn has_trailing_dir_syntax(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.ends_with(b"/")
        || bytes.ends_with(b"/.")
        || (cfg!(windows) && (bytes.ends_with(b"\\") || bytes.ends_with(b"\\.")))
}

fn check_not_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to follow symlink at {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    if has_trailing_dir_syntax(path)
        || !matches!(
            path.components().next_back(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path must name a file: {}", path.display()),
        ));
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path must name a file: {}", path.display()),
        )
    })?;
    check_not_symlink(path)?;
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolved = parent.canonicalize()?.join(name);
    check_not_symlink(&resolved)?;
    Ok(resolved)
}

fn open_nofollow(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(options, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options.open(path)
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(unix)]
    if let (Ok(a), Ok(b)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        use std::os::unix::fs::MetadataExt;
        return a.dev() == b.dev() && a.ino() == b.ino();
    }
    false
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()
}

impl Checkpoint {
    fn redaction_differs(&self, request: &Self) -> bool {
        match (self.redaction.compile(), request.redaction.compile()) {
            (Ok(saved), Ok(requested)) => saved != requested,
            _ => self.redaction != request.redaction,
        }
    }

    fn check_resume(&self, request: &Self) -> anyhow::Result<()> {
        for (changed, flag) in [
            (self.filters.since != request.filters.since, "--since"),
            (self.filters.until != request.filters.until, "--until"),
            (self.filters.actor != request.filters.actor, "--actor"),
            (self.filters.tenant != request.filters.tenant, "--tenant"),
            (self.filters.source != request.filters.source, "--source"),
            (self.schema != request.schema, "--schema"),
            (!self.database.same_identity(&request.database), "database"),
            (self.sink.kind != request.sink.kind, "--format"),
            (
                self.sink.path != request.sink.path || self.sink.target != request.sink.target,
                "--output",
            ),
            (self.redaction_differs(request), "export.redaction"),
            (
                self.allow_rls_filtered != request.allow_rls_filtered,
                "--allow-rls-filtered",
            ),
        ] {
            if changed {
                return Err(anyhow!(
                    "{flag} differs from the saved cursor. Use --from-start to replay"
                ))
                .exit_code(Exit::Usage);
            }
        }
        Ok(())
    }

    fn measure_output(&mut self) -> io::Result<()> {
        self.output_len = match self.sink.path.as_deref() {
            Some(path) => match fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("refusing to follow symlink at {}", path.display()),
                    ));
                }
                Ok(meta) => meta.is_file().then_some(meta.len()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Some(0),
                Err(err) => return Err(err),
            },
            None => None,
        };
        Ok(())
    }

    fn restore_output(&self, from_start: bool) -> io::Result<()> {
        let (Some(path), Some(length)) = (self.sink.path.as_deref(), self.output_len) else {
            return Ok(());
        };
        let mut opts = OpenOptions::new();
        opts.write(true);
        let file = match open_nofollow(&mut opts, path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound && (length == 0 || from_start) => {
                return Ok(());
            }
            other => other?,
        };
        let meta = file.metadata()?;
        let fresh_file = from_start && meta.len() == 0;
        if !meta.is_file() || (meta.len() < length && !fresh_file) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "output is shorter than the acknowledged length or is no longer a regular file",
            ));
        }
        if meta.len() > length {
            file.set_len(length)?;
            file.sync_data()?;
        }
        Ok(())
    }
}

struct CursorFile {
    path: PathBuf,
    // The cursor inode changes at each rename. Keep the stable sidecar locked until export ends.
    _lock: File,
}

impl CursorFile {
    fn lock(path: &Path) -> anyhow::Result<Self> {
        let path = resolve_path(path)?;
        let lock_path = path.with_added_extension("lock");
        if lock_path.is_symlink() {
            return Err(anyhow!(
                "refusing to follow symlink at cursor lock {}",
                lock_path.display()
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let lock = open_nofollow(&mut options, &lock_path)?;
        let meta = lock.metadata()?;
        if !meta.file_type().is_file() {
            return Err(anyhow!(
                "cursor lock {} is not a regular file",
                lock_path.display()
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if meta.mode() & 0o077 != 0 {
                lock.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
        }
        lock.try_lock()
            .with_context(|| format!("cannot lock cursor {}", path.display()))?;
        Ok(Self { path, _lock: lock })
    }

    fn read(&self) -> io::Result<Option<Checkpoint>> {
        let mut options = OpenOptions::new();
        options.read(true);
        match open_nofollow(&mut options, &self.path) {
            Ok(file) => {
                if !file.metadata()?.file_type().is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("cursor {} is not a regular file", self.path.display()),
                    ));
                }
                Ok(Some(serde_json::from_reader(file)?))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn save(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        let bytes = serde_json::to_vec(checkpoint)?;
        // Exclusive creation with random entropy prevents following symlinks, predictable-path
        // collisions, or overwriting files left by another process.
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut attempt = 0_u32;
        let (temp, mut file) = loop {
            let temp = self
                .path
                .with_added_extension(format!("tmp.{}", Uuid::new_v4().simple()));
            match options.open(&temp) {
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempt < 64 => {
                    attempt += 1;
                }
                result => break (temp, result?),
            }
        };
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        #[cfg(unix)]
        sync_parent(&self.path)?;
        Ok(())
    }
}

trait AnchorReader {
    fn acked_anchor(&self) -> Option<BoundaryAnchor>;
}

impl AnchorReader for PgEventSource {
    fn acked_anchor(&self) -> Option<BoundaryAnchor> {
        Self::acked_anchor(self)
    }
}

struct ExportSource<S> {
    reader: S,
    file: Option<CursorFile>,
    checkpoint: Checkpoint,
    // Record the starting length of a regular output file before batch 1 writes, only when no
    // prior checkpoint exists on disk. Replays (`--from-start`) keep the previous checkpoint
    // intact until the first batch flushes and reaches `ack`.
    save_initial: bool,
    created_initial_cursor: bool,
}

impl<S> ExportSource<S> {
    fn save(&mut self, files_only: bool) -> io::Result<bool> {
        if let Some(file) = &self.file {
            self.checkpoint.measure_output()?;
            if !files_only || self.checkpoint.output_len.is_some() {
                file.save(&self.checkpoint)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn cleanup_unused_initial_cursor(&mut self) {
        if self.created_initial_cursor
            && self.checkpoint.cursor == Cursor::new(0, 0)
            && let Some(file) = &self.file
        {
            let _ = fs::remove_file(&file.path);
            self.created_initial_cursor = false;
        }
    }
}

impl<S: EventSource + AnchorReader> EventSource for ExportSource<S> {
    async fn next_batch(&mut self, max: usize) -> Result<Option<Batch>, SourceError> {
        let batch = self.reader.next_batch(max).await?;
        if std::mem::take(&mut self.save_initial) && batch.is_some() {
            let wrote = self
                .save(true)
                .map_err(|err| SourceError::backend("initializing export cursor", err))?;
            self.created_initial_cursor = wrote;
        }
        Ok(batch)
    }

    async fn ack(&mut self, cursor: &Cursor) -> Result<(), SourceError> {
        self.reader.ack(cursor).await?;
        self.checkpoint.cursor = *cursor;
        if let Some(anchor) = self.reader.acked_anchor() {
            self.checkpoint.anchor = Some(anchor);
        }
        self.save(false)
            .map(|_| ())
            .map_err(|err| SourceError::backend("saving export cursor", err))
    }
}

enum LazySink {
    Pending { config: SinkConfig, sync_dir: bool },
    Open(AnySink),
}

impl LazySink {
    fn new(config: SinkConfig, sync_dir: bool) -> Self {
        Self::Pending { config, sync_dir }
    }

    fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    fn ensure_open(&mut self) -> Result<&mut AnySink, SinkError> {
        if let Self::Pending { config, sync_dir } = self {
            let sink = AnySink::open(config)?;
            #[cfg(unix)]
            if *sync_dir
                && let Some(path) = sink_path(config)
                && fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
            {
                sync_parent(path)?;
            }
            #[cfg(not(unix))]
            let _ = sync_dir;
            *self = Self::Open(sink);
        }
        match self {
            Self::Open(sink) => Ok(sink),
            Self::Pending { .. } => unreachable!("sink opened immediately above"),
        }
    }
}

impl Sink for LazySink {
    async fn write(&mut self, batch: &Batch) -> Result<(), SinkError> {
        let sink = self.ensure_open()?;
        if batch.events.is_empty() {
            return Ok(());
        }
        sink.write(batch).await
    }

    async fn flush(&mut self) -> Result<(), SinkError> {
        match self {
            Self::Open(sink) => sink.flush().await,
            Self::Pending { .. } => Ok(()),
        }
    }
}

pub(crate) fn run(args: &Args, config: &Config) -> anyhow::Result<()> {
    if config.export.batch_size == 0 {
        return Err(anyhow!("--batch-size must be greater than zero")).exit_code(Exit::Usage);
    }
    if args
        .filters
        .since
        .zip(args.filters.until)
        .is_some_and(|(since, until)| since >= until)
    {
        return Err(anyhow!("--until must be strictly after --since")).exit_code(Exit::Usage);
    }
    if args.filters.actor.as_ref().is_some_and(String::is_empty) {
        return Err(anyhow!("--actor must not be empty")).exit_code(Exit::Usage);
    }
    if args.filters.tenant.as_ref().is_some_and(String::is_empty) {
        return Err(anyhow!("--tenant must not be empty")).exit_code(Exit::Usage);
    }
    run_async(export(args, config))
}

struct OpenedCheckpoint {
    file: Option<CursorFile>,
    checkpoint: Checkpoint,
    runtime_sink: SinkConfig,
    had_saved: bool,
    saved_timeline_id: Option<u32>,
}

fn open_checkpoint(
    args: &Args,
    config: &Config,
    database: Database,
) -> anyhow::Result<OpenedCheckpoint> {
    let mut runtime_sink = config.export.sink.clone();
    let (SinkConfig::Jsonl { path: output } | SinkConfig::Csv { path: output }) = &mut runtime_sink;
    if let Some(output) = output {
        *output = resolve_path(output)
            .context("resolving export output")
            .exit_code(Exit::Io)?;
    }
    let mut requested = Checkpoint {
        cursor: Cursor::new(0, 0),
        anchor: None,
        database,
        schema: config.schema.clone(),
        filters: args.filters.clone(),
        sink: runtime_sink.fingerprint(),
        output_len: None,
        redaction: config.export.redaction.clone(),
        allow_rls_filtered: args.allow_rls_filtered,
    };
    let Some(path) = &config.export.cursor_file else {
        return Ok(OpenedCheckpoint {
            file: None,
            checkpoint: requested,
            runtime_sink,
            had_saved: false,
            saved_timeline_id: None,
        });
    };
    let file = CursorFile::lock(path)
        .with_context(|| format!("opening cursor {}", path.display()))
        .exit_code(Exit::Io)?;
    if let Some(output) = sink_path(&runtime_sink)
        && (same_file(output, &file.path)
            || same_file(output, &file.path.with_added_extension("lock")))
    {
        return Err(anyhow!(
            "--output must differ from the cursor and its lock file"
        ))
        .exit_code(Exit::Usage);
    }
    if args.from_start {
        // Replay starts at the beginning, but a partial batch in this destination still needs repair.
        // Corrupt/newer-schema cursor JSON is ignored for output_len recovery under `--from-start`,
        // while keeping `had_saved = true` so pre-existing files on disk are not overwritten or
        // deleted until batch 1 flushes and reaches `ack`. Symlink/non-file/I/O errors fail closed.
        let (saved, had_saved) = match file.read() {
            Ok(Some(saved)) => (Some(saved), true),
            Ok(None) => (None, false),
            Err(err) if matches!(err.get_ref(), Some(inner) if inner.is::<serde_json::Error>()) => {
                (None, true)
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading cursor {}", path.display()))
                    .exit_code(Exit::Io);
            }
        };
        if let Some(saved) = saved
            && requested.sink == saved.sink
        {
            requested.output_len = saved.output_len;
        }
        return Ok(OpenedCheckpoint {
            file: Some(file),
            checkpoint: requested,
            runtime_sink,
            had_saved,
            saved_timeline_id: None,
        });
    }
    if let Some(saved) = file
        .read()
        .with_context(|| format!("reading cursor {}", path.display()))
        .exit_code(Exit::Io)?
    {
        saved.check_resume(&requested)?;
        let saved_timeline_id = saved.database.timeline_id;
        return Ok(OpenedCheckpoint {
            file: Some(file),
            checkpoint: saved,
            runtime_sink,
            had_saved: true,
            saved_timeline_id,
        });
    }
    Ok(OpenedCheckpoint {
        file: Some(file),
        checkpoint: requested,
        runtime_sink,
        had_saved: false,
        saved_timeline_id: None,
    })
}

#[derive(Debug, Serialize)]
struct ExportStats {
    events: usize,
    batches: usize,
    duration_ms: u64,
    cursor: Cursor,
    snapshot_xmin: u64,
    oldest_unexported_age_secs: Option<f64>,
    stalled: bool,
}

fn format_stats_summary(
    stats_format: StatsFormat,
    report: &auditstream_export::Report,
    duration_ms: u64,
    cursor: Cursor,
    lag: ExportLag,
) -> String {
    match stats_format {
        StatsFormat::Text => {
            if let Some(age) = lag.oldest_unexported_age_secs {
                format!(
                    "events={} batches={} cursor=({}, {}) stalled=true oldest_unexported_age_secs={age:.1}",
                    report.events, report.batches, cursor.xact_id, cursor.id
                )
            } else {
                format!(
                    "events={} batches={} cursor=({}, {})",
                    report.events, report.batches, cursor.xact_id, cursor.id
                )
            }
        }
        StatsFormat::Json => {
            let stats = ExportStats {
                events: report.events,
                batches: report.batches,
                duration_ms,
                cursor,
                snapshot_xmin: lag.snapshot_xmin,
                oldest_unexported_age_secs: lag.oldest_unexported_age_secs,
                stalled: lag.stalled,
            };
            serde_json::to_string(&stats)
                .unwrap_or_else(|_| String::from(r#"{"error":"failed to serialize export stats"}"#))
        }
    }
}

async fn run_pipeline<S: EventSource + AnchorReader, K: Sink>(
    source: &mut ExportSource<S>,
    sink: &mut K,
    options: &Options,
) -> anyhow::Result<Option<auditstream_export::Report>> {
    match auditstream_export::run(source, sink, options).await {
        Ok(report) => Ok(Some(report)),
        Err(ExportError::Sink(SinkError::Io(err)))
            if source.file.is_none()
                && source.checkpoint.sink.is_stdout()
                && err.kind() == io::ErrorKind::BrokenPipe =>
        {
            Ok(None)
        }
        Err(err) => {
            let exit = match &err {
                ExportError::Source(SourceError::Backend { source, .. })
                    if source.is::<io::Error>() =>
                {
                    Exit::Io
                }
                ExportError::Source(_) => Exit::Database,
                ExportError::Ack(_) | ExportError::Sink(_) => Exit::Io,
                ExportError::InvalidOptions(_) => Exit::Usage,
                // Fail-closed redaction (`ExportError::Redact`) and future non-exhaustive variants exit Internal (5).
                _ => Exit::Internal,
            };
            Err(err).exit_code(exit)
        }
    }
}

async fn export(args: &Args, config: &Config) -> anyhow::Result<()> {
    let url = require_database_url(config)?;
    let pool = connect_database(url).await?;
    let result = export_with_pool(args, config, pool.clone()).await;
    pool.close().await;
    result
}

async fn ensure_output_not_dir(sink: &SinkConfig) -> anyhow::Result<()> {
    let Some(path) = sink_path(sink) else {
        return Ok(());
    };
    let check_path = path.to_path_buf();
    let is_dir = tokio::task::spawn_blocking(move || {
        fs::symlink_metadata(&check_path).is_ok_and(|meta| meta.file_type().is_dir())
    })
    .await
    .context("checking export output path")
    .exit_code(Exit::Io)?;
    if is_dir {
        return Err(anyhow!(
            "opening export output: {} is a directory",
            path.display()
        ))
        .exit_code(Exit::Io);
    }
    Ok(())
}

fn effective_stats_format(explicit: Option<StatsFormat>, log_format: LogFormat) -> StatsFormat {
    explicit.unwrap_or(match log_format {
        LogFormat::Text => StatsFormat::Text,
        LogFormat::Json => StatsFormat::Json,
    })
}

async fn emit_export_telemetry(
    pool: &auditstream_pg::PgPool,
    args: &Args,
    config: &Config,
    report: &auditstream_export::Report,
    started_at: Instant,
    cursor: Cursor,
) -> anyhow::Result<()> {
    let stats_format = effective_stats_format(args.stats_format, config.log_format);
    let want_stall_warn = (config.log_format == LogFormat::Json
        || stats_format == StatsFormat::Text)
        && tracing::enabled!(tracing::Level::WARN);
    if config.quiet && !want_stall_warn {
        return Ok(());
    }

    let lag = reader::fetch_export_lag(pool, &config.schema, cursor)
        .await
        .context("reading export lag telemetry")
        .exit_code(Exit::Database)?;
    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);

    if lag.stalled && want_stall_warn {
        tracing::warn!(
            snapshot_xmin = lag.snapshot_xmin,
            oldest_unexported_age_secs = lag.oldest_unexported_age_secs,
            cursor_xact_id = cursor.xact_id,
            cursor_id = cursor.id,
            "export snapshot window is stalled behind an open PostgreSQL transaction"
        );
    }

    if !config.quiet {
        eprintln!(
            "{}",
            format_stats_summary(stats_format, report, duration_ms, cursor, lag)
        );
    }
    Ok(())
}

fn update_checkpoint_timeline(
    opened: &mut OpenedCheckpoint,
    current_timeline_id: Option<u32>,
) -> bool {
    let Some(tl) = current_timeline_id else {
        return false;
    };
    let updated = opened.checkpoint.database.timeline_id != Some(tl);
    if let Some(prev_tl) = opened.saved_timeline_id
        && prev_tl != tl
    {
        let msg = if opened.checkpoint.anchor.is_some() {
            "verified export boundary anchor after PostgreSQL timeline change"
        } else {
            "advancing zero-cursor export checkpoint after PostgreSQL timeline change"
        };
        tracing::info!(previous_timeline_id = prev_tl, timeline_id = tl, "{msg}");
    }
    opened.checkpoint.database.timeline_id = Some(tl);
    updated
}

async fn export_with_pool(
    args: &Args,
    config: &Config,
    pool: auditstream_pg::PgPool,
) -> anyhow::Result<()> {
    let started_at = Instant::now();
    let database: Database = reader::database_identity(&pool, &config.schema)
        .await
        .context("identifying PostgreSQL database")
        .exit_code(Exit::Database)?
        .into();
    if !args.allow_rls_filtered {
        reader::ensure_export_rls_safe(&pool, &config.schema)
            .await
            .context("checking Row-Level Security posture before export")
            .exit_code(Exit::Database)?;
    }
    let current_timeline_id = database.timeline_id;
    let mut opened = open_checkpoint(args, config, database)?;

    let had_saved = opened.had_saved;
    if had_saved && !args.from_start {
        reader::verify_resume_state(
            &pool,
            &config.schema,
            ResumeVerification {
                cursor: opened.checkpoint.cursor,
                anchor: opened.checkpoint.anchor,
                saved_timeline_id: opened.saved_timeline_id,
                current_timeline_id,
            },
        )
        .await
        .context("verifying resumable export continuity")
        .exit_code(Exit::Database)?;
    }

    opened
        .checkpoint
        .restore_output(args.from_start)
        .context("recovering export output")
        .exit_code(Exit::Io)?;
    ensure_output_not_dir(&opened.runtime_sink).await?;

    let timeline_updated = update_checkpoint_timeline(&mut opened, current_timeline_id);

    let reader = PgEventSource::new(
        pool.clone(),
        &config.schema,
        opened.checkpoint.cursor,
        args.filters.reader_filter(),
    )
    .allow_rls_filtered(args.allow_rls_filtered);
    let mut sink = LazySink::new(opened.runtime_sink, opened.file.is_some());
    let mut source = ExportSource {
        reader,
        file: opened.file,
        checkpoint: opened.checkpoint,
        save_initial: !had_saved,
        created_initial_cursor: false,
    };
    let options = Options::with_batch_size(config.export.batch_size)
        .with_redaction(config.export.compiled_redaction.clone());
    let report = match run_pipeline(&mut source, &mut sink, &options).await {
        Ok(Some(report)) => report,
        Ok(None) => return Ok(()),
        Err(err) => {
            if sink.is_pending() {
                source.cleanup_unused_initial_cursor();
            }
            return Err(err);
        }
    };
    // Now that the PostgreSQL query succeeded, validate/create any configured output file even
    // when zero events matched, and persist the initial, `--from-start`, or timeline-updated checkpoint.
    sink.ensure_open()
        .context("opening export output")
        .exit_code(Exit::Io)?;
    if report.batches == 0 && (args.from_start || !had_saved || timeline_updated) {
        source
            .save(false)
            .context("saving export cursor")
            .exit_code(Exit::Io)?;
    }

    emit_export_telemetry(
        &pool,
        args,
        config,
        &report,
        started_at,
        source.checkpoint.cursor,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::future::Future;
    use std::io;
    use std::path::PathBuf;

    use auditstream_core::{
        Batch, CompiledRedaction, Cursor, Event, EventSource, Redaction, RedactionMode,
        RedactionRule, Sink, SinkError, SourceError,
    };
    use auditstream_export::{Options, Report, SinkConfig};
    use auditstream_pg::reader::{BoundaryAnchor, ExportFilter, ExportLag};
    use serde_json::Value;
    use tempfile::tempdir;

    use super::{
        AnchorReader, Checkpoint, CursorFile, Database, ExportSource, Filters, LazySink,
        StatsFormat, format_stats_summary, run_pipeline,
    };
    use crate::exit::{Exit, exit_code_for};

    struct StaticSource {
        batches: std::collections::VecDeque<Result<Batch, SourceError>>,
    }

    impl AnchorReader for StaticSource {
        fn acked_anchor(&self) -> Option<BoundaryAnchor> {
            None
        }
    }

    impl EventSource for StaticSource {
        fn next_batch(
            &mut self,
            _max: usize,
        ) -> impl Future<Output = Result<Option<Batch>, SourceError>> + Send {
            let next = self.batches.pop_front().transpose();
            std::future::ready(next)
        }

        fn ack(&mut self, _upto: &Cursor) -> impl Future<Output = Result<(), SourceError>> + Send {
            std::future::ready(Ok(()))
        }
    }

    struct FailingSink {
        kind: io::ErrorKind,
    }

    impl Sink for FailingSink {
        fn write(&mut self, _batch: &Batch) -> impl Future<Output = Result<(), SinkError>> + Send {
            std::future::ready(Err(SinkError::Io(io::Error::new(
                self.kind,
                "simulated sink error",
            ))))
        }

        fn flush(&mut self) -> impl Future<Output = Result<(), SinkError>> + Send {
            std::future::ready(Ok(()))
        }
    }

    fn sample_database() -> Database {
        Database {
            system_identifier: Some("7123456789012345678".to_owned()),
            name: "audit_test".to_owned(),
            oid: 16384,
            history_table_oid: Some(24576),
            timeline_id: Some(1),
        }
    }

    fn sample_event() -> Event {
        serde_json::from_str(
            r#"{
                "event_id": "00000000-0000-4000-8000-000000000001",
                "occurred_at": "1970-01-01T00:00:00.000000Z",
                "source": "app",
                "schema_name": null,
                "table_name": null,
                "op": "test.event",
                "record_id": null,
                "before": null,
                "after": null,
                "changed": null,
                "actor": null,
                "context": null,
                "actor_source": null,
                "db_user": "postgres",
                "schema_version": 1
            }"#,
        )
        .expect("valid sample event JSON")
    }

    fn sample_checkpoint(sink: &SinkConfig, cursor: Cursor, output_len: Option<u64>) -> Checkpoint {
        Checkpoint {
            cursor,
            anchor: None,
            database: sample_database(),
            schema: "auditstream".to_owned(),
            filters: Filters::default(),
            sink: sink.fingerprint(),
            output_len,
            redaction: Redaction::default(),
            allow_rls_filtered: false,
        }
    }

    #[test]
    fn database_same_identity_allows_timeline_transition_and_rejects_identity_drift() {
        let base = sample_database();
        let mut failover = base.clone();
        failover.timeline_id = Some(2);
        assert!(base.same_identity(&failover));

        let mut legacy = base.clone();
        legacy.timeline_id = None;
        assert!(legacy.same_identity(&base));

        let mut other_oid = base.clone();
        other_oid.oid = 99999;
        assert!(!base.same_identity(&other_oid));

        let mut other_table = base.clone();
        other_table.history_table_oid = Some(99999);
        assert!(!base.same_identity(&other_table));

        let mut other_cluster = base.clone();
        other_cluster.system_identifier = Some("9999999999999999999".to_owned());
        assert!(!base.same_identity(&other_cluster));
    }

    #[test]
    fn format_stats_summary_emits_text_and_json_telemetry() {
        let report = Report {
            batches: 2,
            events: 5,
            last_cursor: Some(Cursor::new(42, 0)),
        };
        let clean_lag = ExportLag {
            snapshot_xmin: 42,
            oldest_unexported_age_secs: None,
            stalled: false,
        };
        assert_eq!(
            format_stats_summary(
                StatsFormat::Text,
                &report,
                15,
                Cursor::new(42, 0),
                clean_lag
            ),
            "events=5 batches=2 cursor=(42, 0)"
        );

        let clean_json: Value = serde_json::from_str(&format_stats_summary(
            StatsFormat::Json,
            &report,
            15,
            Cursor::new(42, 0),
            clean_lag,
        ))
        .expect("parse clean stats json");
        assert_eq!(clean_json["events"], 5);
        assert_eq!(clean_json["batches"], 2);
        assert_eq!(clean_json["duration_ms"], 15);
        assert_eq!(clean_json["cursor"]["xact_id"], 42);
        assert_eq!(clean_json["cursor"]["id"], 0);
        assert_eq!(clean_json["snapshot_xmin"], 42);
        assert!(clean_json["oldest_unexported_age_secs"].is_null());
        assert_eq!(clean_json["stalled"], false);

        let stalled_lag = ExportLag {
            snapshot_xmin: 42,
            oldest_unexported_age_secs: Some(18.75),
            stalled: true,
        };
        let stalled_text = format_stats_summary(
            StatsFormat::Text,
            &Report::default(),
            9,
            Cursor::new(42, 0),
            stalled_lag,
        );
        assert_eq!(
            stalled_text,
            "events=0 batches=0 cursor=(42, 0) stalled=true oldest_unexported_age_secs=18.8"
        );
        let stalled_json: Value = serde_json::from_str(&format_stats_summary(
            StatsFormat::Json,
            &Report::default(),
            9,
            Cursor::new(42, 0),
            stalled_lag,
        ))
        .expect("parse stalled stats json");
        assert_eq!(stalled_json["stalled"], true);
        assert_eq!(stalled_json["oldest_unexported_age_secs"], 18.75);
        assert_eq!(stalled_json["snapshot_xmin"], 42);

        assert_eq!(
            super::effective_stats_format(None, super::LogFormat::Text),
            StatsFormat::Text
        );
        assert_eq!(
            super::effective_stats_format(None, super::LogFormat::Json),
            StatsFormat::Json
        );
        assert_eq!(
            super::effective_stats_format(Some(StatsFormat::Text), super::LogFormat::Json),
            StatsFormat::Text
        );
        assert_eq!(
            super::effective_stats_format(Some(StatsFormat::Json), super::LogFormat::Text),
            StatsFormat::Json
        );
    }

    #[test]
    fn v0_1_checkpoint_without_redaction_field_deserializes_and_checks_drift() {
        let legacy_json = r#"{
            "cursor": {"xact_id": 10, "id": 1},
            "database": {"system_identifier": "7123456789012345678", "name": "audit_test", "oid": 16384, "history_table_oid": 24576},
            "schema": "auditstream",
            "filters": {"since": null, "until": null, "actor": null, "source": null},
            "sink": {"type": "jsonl", "path": "/tmp/a.jsonl"},
            "output_len": 128
        }"#;
        let saved: Checkpoint =
            serde_json::from_str(legacy_json).expect("v0.1 cursor without redaction must parse");
        assert!(saved.redaction.is_empty());

        let unredacted = sample_checkpoint(
            &SinkConfig::Jsonl {
                path: Some(PathBuf::from("/tmp/a.jsonl")),
            },
            Cursor::new(0, 0),
            None,
        );
        saved
            .check_resume(&unredacted)
            .expect("unredacted resume matches legacy checkpoint");

        let mut with_rules = unredacted.clone();
        with_rules.redaction = Redaction::new(vec![RedactionRule::for_table(
            "invoice",
            ["ssn"],
            RedactionMode::Mask,
        )]);
        let err = saved
            .check_resume(&with_rules)
            .expect_err("adding redaction rules must invalidate legacy checkpoint");
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("export.redaction"), "{err}");
        assert!(err.to_string().contains("--from-start"), "{err}");

        // Equivalent rules in a different order compile identically and resume cleanly.
        let mut reordered = unredacted;
        reordered.redaction = Redaction::new(vec![RedactionRule::for_table(
            "invoice",
            ["tax_id", "ssn"],
            RedactionMode::Mask,
        )]);
        let mut saved_two = reordered.clone();
        saved_two.redaction = Redaction::new(vec![
            RedactionRule::for_table("invoice", ["ssn"], RedactionMode::Mask),
            RedactionRule::for_table("invoice", ["tax_id"], RedactionMode::Mask),
        ]);
        saved_two
            .check_resume(&reordered)
            .expect("equivalent compiled redaction policy resumes cleanly");
    }

    #[tokio::test]
    async fn run_pipeline_maps_fail_closed_redaction_error_to_internal_without_advancing_cursor() {
        let dir = tempdir().expect("tempdir");
        let cursor_path = dir.path().join("cursor.json");
        let sink_cfg = SinkConfig::Jsonl { path: None };
        let cursor_file = CursorFile::lock(&cursor_path).expect("lock");

        let mut bad_event = sample_event();
        bad_event.after = Some(
            serde_json::value::RawValue::from_string(r#"[{"ssn":"123"}]"#.to_owned())
                .expect("array payload"),
        );

        let mut source = ExportSource {
            reader: StaticSource {
                batches: [Ok(Batch::new(vec![bad_event], Some(Cursor::new(50, 1))))]
                    .into_iter()
                    .collect(),
            },
            file: Some(cursor_file),
            checkpoint: sample_checkpoint(&sink_cfg, Cursor::new(0, 0), None),
            save_initial: false,
            created_initial_cursor: false,
        };
        let mut sink = FailingSink {
            kind: io::ErrorKind::Other,
        };
        let compiled =
            CompiledRedaction::compile(&Redaction::new(vec![RedactionRule::for_app_action(
                "test.event",
                ["ssn"],
                RedactionMode::Mask,
            )]))
            .expect("compile");

        let err = run_pipeline(
            &mut source,
            &mut sink,
            &Options::with_batch_size(1).with_redaction(compiled),
        )
        .await
        .expect_err("non-object app payload must fail closed");
        assert_eq!(exit_code_for(&err), Exit::Internal);
        assert_eq!(source.checkpoint.cursor, Cursor::new(0, 0));
        assert!(!cursor_path.exists());
    }

    #[test]
    fn missing_checkpoint_filters_mean_no_restrictions() {
        let filters: Filters = serde_json::from_str("{}").expect("omitted filters are optional");
        assert_eq!(filters.reader_filter(), ExportFilter::default());
    }

    #[test]
    fn database_checkpoint_rejects_unknown_fields() {
        let raw = r#"{"name":"db","oid":1,"host":"localhost"}"#;
        let err = serde_json::from_str::<Database>(raw).expect_err("unknown field must fail");
        assert!(err.to_string().contains("host"), "{err}");
    }

    #[test]
    fn check_resume_exits_usage_for_mismatched_output_and_format() {
        let saved = sample_checkpoint(
            &SinkConfig::Jsonl {
                path: Some(PathBuf::from("/tmp/a.jsonl")),
            },
            Cursor::new(10, 1),
            Some(128),
        );

        let diff_output = sample_checkpoint(
            &SinkConfig::Jsonl {
                path: Some(PathBuf::from("/tmp/b.jsonl")),
            },
            Cursor::new(0, 0),
            None,
        );
        let err = saved.check_resume(&diff_output).expect_err("must reject");
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("--output"), "{err}");
        assert!(err.to_string().contains("--from-start"), "{err}");

        let diff_format = sample_checkpoint(
            &SinkConfig::Csv {
                path: Some(PathBuf::from("/tmp/a.jsonl")),
            },
            Cursor::new(0, 0),
            None,
        );
        let err = saved.check_resume(&diff_format).expect_err("must reject");
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("--format"), "{err}");

        // Legacy v0.1 checkpoint missing `tenant` deserializes with `tenant: None`
        // and rejects resuming with `--tenant`.
        let legacy_filters: Filters =
            serde_json::from_str(r#"{"since":null,"until":null,"actor":null,"source":null}"#)
                .expect("legacy filters without tenant");
        assert_eq!(legacy_filters.tenant, None);

        let mut with_tenant = saved.clone();
        with_tenant.filters.tenant = Some("org_acme".to_owned());
        let err = saved.check_resume(&with_tenant).expect_err("must reject");
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("--tenant"), "{err}");
        assert!(err.to_string().contains("--from-start"), "{err}");

        // Switching between unfiltered and `--allow-rls-filtered` requires `--from-start`.
        let mut rls_filtered = saved.clone();
        rls_filtered.allow_rls_filtered = true;
        let err = saved.check_resume(&rls_filtered).expect_err("must reject");
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("--allow-rls-filtered"), "{err}");
        assert!(err.to_string().contains("--from-start"), "{err}");
        let err = rls_filtered.check_resume(&saved).expect_err("must reject");
        assert_eq!(exit_code_for(&err), Exit::Usage);
        assert!(err.to_string().contains("--allow-rls-filtered"), "{err}");

        // Unfiltered checkpoints omit `allow_rls_filtered`; filtered checkpoints persist it.
        let serialized_unfiltered = serde_json::to_string(&saved).expect("serialize unfiltered");
        assert!(!serialized_unfiltered.contains("allow_rls_filtered"));
        let serialized_filtered = serde_json::to_string(&rls_filtered).expect("serialize filtered");
        assert!(serialized_filtered.contains("\"allow_rls_filtered\":true"));
    }

    #[tokio::test]
    async fn from_start_preserves_existing_cursor_when_first_batch_write_fails() {
        let dir = tempdir().expect("tempdir");
        let cursor_path = dir.path().join("cursor.json");
        let output_path = dir.path().join("events.jsonl");
        fs::write(&output_path, b"existing-acked-line\n").expect("seed output");

        let sink_cfg = SinkConfig::Jsonl {
            path: Some(output_path.clone()),
        };
        let initial = sample_checkpoint(&sink_cfg, Cursor::new(42, 7), Some(20));
        let cursor_file = CursorFile::lock(&cursor_path).expect("lock cursor");
        cursor_file.save(&initial).expect("write prior checkpoint");
        let before_bytes = fs::read(&cursor_path).expect("read prior checkpoint");

        let mut source = ExportSource {
            reader: StaticSource {
                batches: [Ok(Batch::new(
                    vec![sample_event()],
                    Some(Cursor::new(99, 1)),
                ))]
                .into_iter()
                .collect(),
            },
            file: Some(cursor_file),
            checkpoint: sample_checkpoint(&sink_cfg, Cursor::new(0, 0), Some(20)),
            save_initial: false,
            created_initial_cursor: false,
        };
        let mut sink = FailingSink {
            kind: io::ErrorKind::StorageFull,
        };

        let err = run_pipeline(&mut source, &mut sink, &Options::with_batch_size(1))
            .await
            .expect_err("storage full must fail");
        assert_eq!(exit_code_for(&err), Exit::Io);
        assert_eq!(
            fs::read(&cursor_path).expect("read cursor after failure"),
            before_bytes,
            "failed --from-start replay must leave the prior cursor untouched"
        );
    }

    #[tokio::test]
    async fn broken_pipe_on_stdout_succeeds_statelessly_and_fails_with_cursor_file() {
        let dir = tempdir().expect("tempdir");
        let sink_cfg = SinkConfig::Jsonl { path: None };

        // 1. Stateless stdout BrokenPipe -> Ok(None) (exit 0)
        let mut stateless = ExportSource {
            reader: StaticSource {
                batches: [Ok(Batch::new(
                    vec![sample_event()],
                    Some(Cursor::new(10, 1)),
                ))]
                .into_iter()
                .collect(),
            },
            file: None,
            checkpoint: sample_checkpoint(&sink_cfg, Cursor::new(0, 0), None),
            save_initial: false,
            created_initial_cursor: false,
        };
        let mut sink = FailingSink {
            kind: io::ErrorKind::BrokenPipe,
        };
        let outcome = run_pipeline(&mut stateless, &mut sink, &Options::with_batch_size(1))
            .await
            .expect("stateless broken pipe is quiet success");
        assert!(outcome.is_none());

        // 2. Stateful stdout (--cursor-file) BrokenPipe -> Exit::Io (4)
        let cursor_file = CursorFile::lock(&dir.path().join("cursor.json")).expect("lock");
        let mut stateful = ExportSource {
            reader: StaticSource {
                batches: [Ok(Batch::new(
                    vec![sample_event()],
                    Some(Cursor::new(10, 1)),
                ))]
                .into_iter()
                .collect(),
            },
            file: Some(cursor_file),
            checkpoint: sample_checkpoint(&sink_cfg, Cursor::new(0, 0), None),
            save_initial: false,
            created_initial_cursor: false,
        };
        let err = run_pipeline(&mut stateful, &mut sink, &Options::with_batch_size(1))
            .await
            .expect_err("stateful broken pipe must fail with Exit::Io");
        assert_eq!(exit_code_for(&err), Exit::Io);
    }

    #[tokio::test]
    async fn lazy_sink_does_not_create_output_file_when_source_read_fails() {
        let dir = tempdir().expect("tempdir");
        let output_path = dir.path().join("should-not-exist.jsonl");
        let sink_cfg = SinkConfig::Jsonl {
            path: Some(output_path.clone()),
        };

        let mut source = ExportSource {
            reader: StaticSource {
                batches: [Err(SourceError::backend(
                    "simulated db failure",
                    io::Error::other("relation does not exist"),
                ))]
                .into_iter()
                .collect(),
            },
            file: None,
            checkpoint: sample_checkpoint(&sink_cfg, Cursor::new(0, 0), None),
            save_initial: false,
            created_initial_cursor: false,
        };
        let mut sink = LazySink::new(sink_cfg, false);

        let _ = run_pipeline(&mut source, &mut sink, &Options::with_batch_size(1))
            .await
            .expect_err("source failure must propagate");
        assert!(
            !output_path.exists(),
            "output file must not be created when initial source read fails"
        );
    }

    #[tokio::test]
    async fn empty_filtered_batch_opens_lazy_sink_before_ack_and_cleans_up_unused_initial_cursor() {
        let dir = tempdir().expect("tempdir");
        let cursor_path = dir.path().join("cursor.json");
        // A non-existent parent directory causes `AnySink::open` to fail cleanly on all platforms.
        let unopenable_output = dir.path().join("missing-dir").join("events.jsonl");
        let sink_cfg = SinkConfig::Jsonl {
            path: Some(unopenable_output),
        };
        let cursor_file = CursorFile::lock(&cursor_path).expect("lock cursor");

        let mut source = ExportSource {
            reader: StaticSource {
                batches: [Ok(Batch::new(Vec::new(), Some(Cursor::new(42, 0))))]
                    .into_iter()
                    .collect(),
            },
            file: Some(cursor_file),
            checkpoint: sample_checkpoint(&sink_cfg, Cursor::new(0, 0), None),
            save_initial: true,
            created_initial_cursor: false,
        };
        let mut sink = LazySink::new(sink_cfg, false);

        let err = run_pipeline(&mut source, &mut sink, &Options::with_batch_size(10))
            .await
            .expect_err("unopenable output must fail even on an empty filtered batch");
        assert_eq!(exit_code_for(&err), Exit::Io);
        assert!(sink.is_pending());
        assert_eq!(source.checkpoint.cursor, Cursor::new(0, 0));
        source.cleanup_unused_initial_cursor();
        assert!(
            !cursor_path.exists(),
            "stray initial cursor must be cleaned up when LazySink never opened"
        );
    }

    #[test]
    fn open_checkpoint_from_start_marks_corrupt_cursor_as_had_saved() {
        use clap::Parser;

        let dir = tempdir().expect("tempdir");
        let cursor_path = dir.path().join("cursor.json");
        let output_path = dir.path().join("events.jsonl");
        let corrupt = br#"{"unexpected_future_field": true}"#;
        fs::write(&cursor_path, corrupt).expect("seed corrupt cursor");

        let cli = crate::cli::Cli::try_parse_from([
            "auditstream",
            "export",
            "--from-start",
            "--cursor-file",
            cursor_path.to_str().expect("utf8 cursor path"),
            "--output",
            output_path.to_str().expect("utf8 output path"),
        ])
        .expect("parse export cli");
        let config = crate::config::Config::load(&cli, dir.path()).expect("load config");
        let crate::cli::Command::Export(args) = &cli.command else {
            unreachable!("parsed export subcommand");
        };

        let opened = super::open_checkpoint(args, &config, sample_database())
            .expect("--from-start ignores corrupt cursor JSON");
        assert!(
            opened.had_saved,
            "pre-existing corrupt cursor must keep had_saved=true so save_initial never clobbers it"
        );
        assert_eq!(fs::read(&cursor_path).unwrap(), corrupt);
    }

    #[cfg(unix)]
    #[test]
    fn cursor_lock_uses_0600_mode_and_refuses_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempdir().expect("tempdir");
        let cursor_path = dir.path().join("cursor.json");
        let lock_path = dir.path().join("cursor.json.lock");
        let locked = CursorFile::lock(&cursor_path).expect("lock new cursor");
        let mode = fs::metadata(&lock_path)
            .expect("lock meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(locked);

        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").expect("victim");
        let sym_cursor = dir.path().join("sym.cursor");
        symlink(&victim, dir.path().join("sym.cursor.lock")).expect("create symlink");
        let err = CursorFile::lock(&sym_cursor)
            .err()
            .expect("symlink lock must be rejected");
        assert!(err.to_string().contains("symlink"), "{err}");

        // Pre-existing permissive `.lock` file is tightened to 0600.
        let loose_cursor = dir.path().join("loose.cursor");
        let loose_lock = dir.path().join("loose.cursor.lock");
        fs::write(&loose_lock, b"").expect("create loose lock");
        fs::set_permissions(&loose_lock, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
        let locked = CursorFile::lock(&loose_cursor).expect("lock loose cursor");
        let tightened = fs::metadata(&loose_lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(tightened, 0o600, "loose lock must be tightened to 0600");
        drop(locked);

        // resolve_path rejects live & dangling symlinks on final component, preserves symlinked parent dir.
        let live_link = dir.path().join("live-link.json");
        symlink(&victim, &live_link).expect("create live symlink");
        assert!(super::resolve_path(&live_link).is_err());

        let dangling_link = dir.path().join("dangling-link.json");
        symlink(dir.path().join("missing.json"), &dangling_link).expect("create dangling symlink");
        assert!(super::resolve_path(&dangling_link).is_err());

        let real_subdir = dir.path().join("real_sub");
        fs::create_dir(&real_subdir).expect("create real_sub");
        let parent_link = dir.path().join("sym_sub");
        symlink(&real_subdir, &parent_link).expect("create parent symlink");
        let resolved_child = super::resolve_path(&parent_link.join("events.jsonl"))
            .expect("symlinked parent dir allowed");
        assert_eq!(
            resolved_child,
            real_subdir.canonicalize().unwrap().join("events.jsonl")
        );

        // Trailing `/` or `/.` on a symlinked directory must NOT bypass symlink/file validation.
        let sym_trailing_slash = PathBuf::from(format!("{}/", parent_link.display()));
        let sym_trailing_dot = PathBuf::from(format!("{}/.", parent_link.display()));
        assert!(
            super::resolve_path(&sym_trailing_slash).is_err(),
            "resolve_path must reject trailing slash on symlinked dir"
        );
        assert!(
            super::resolve_path(&sym_trailing_dot).is_err(),
            "resolve_path must reject trailing /. on symlinked dir"
        );

        // Named pipe (FIFO) must fail immediately in CursorFile::read without hanging.
        let fifo_cursor = dir.path().join("fifo.cursor");
        if std::process::Command::new("mkfifo")
            .arg(&fifo_cursor)
            .status()
            .is_ok_and(|s| s.success())
        {
            let locked_fifo = CursorFile::lock(&fifo_cursor).expect("lock sidecar");
            assert!(
                locked_fifo.read().is_err(),
                "FIFO cursor file must fail immediately without hanging"
            );
            drop(locked_fifo);
        }

        // restore_output refuses to truncate a symlink target via O_NOFOLLOW.
        let big_victim = dir.path().join("big_victim.jsonl");
        fs::write(&big_victim, b"0123456789abcdef").expect("write big_victim");
        let out_link = dir.path().join("out_link.jsonl");
        symlink(&big_victim, &out_link).expect("create out symlink");
        let cp = sample_checkpoint(
            &SinkConfig::Jsonl {
                path: Some(out_link),
            },
            Cursor::new(1, 1),
            Some(4),
        );
        assert!(cp.restore_output(false).is_err());
        assert_eq!(fs::read(&big_victim).unwrap(), b"0123456789abcdef");
    }
}
