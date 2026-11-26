//! Export committed events and checkpoint each batch after its destination flushes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use auditstream_core::{
    Batch, Cursor, EventSource, Redaction, Sink, SinkError, Source, SourceError,
};
use auditstream_export::{AnySink, ExportError, Options, SinkConfig};
use auditstream_pg::reader::{self, DatabaseIdentity, ExportFilter, PgEventSource};
use clap::{Args as ClapArgs, ValueEnum};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::commands::common::{connect_database, require_database_url, run_async};
use crate::config::Config;
use crate::exit::{Exit, ExitCodeExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    Jsonl,
    Csv,
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
    database: Database,
    schema: String,
    filters: Filters,
    sink: SinkConfig,
    output_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Redaction::is_empty")]
    redaction: Redaction,
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
}

impl From<DatabaseIdentity> for Database {
    fn from(id: DatabaseIdentity) -> Self {
        Self {
            system_identifier: id.system_identifier,
            name: id.name,
            oid: id.oid,
            history_table_oid: id.history_table_oid,
        }
    }
}

fn sink_path(sink: &SinkConfig) -> Option<&Path> {
    match sink {
        SinkConfig::Jsonl { path } | SinkConfig::Csv { path } => path.as_deref(),
    }
}

fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    if !matches!(
        path.components().next_back(),
        Some(std::path::Component::Normal(_))
    ) {
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
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to follow symlink at {}", path.display()),
            ));
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(name))
}

fn open_nofollow(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(options, libc::O_NOFOLLOW);
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
            (self.database != request.database, "database"),
            (
                std::mem::discriminant(&self.sink) != std::mem::discriminant(&request.sink),
                "--format",
            ),
            (
                sink_path(&self.sink) != sink_path(&request.sink),
                "--output",
            ),
            (self.redaction_differs(request), "export.redaction"),
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
        self.output_len = match sink_path(&self.sink) {
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
        let (Some(path), Some(length)) = (sink_path(&self.sink), self.output_len) else {
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

struct ExportSource<S> {
    reader: S,
    file: Option<CursorFile>,
    checkpoint: Checkpoint,
    // Record the starting length of a regular output file before batch 1 writes, only when no
    // prior checkpoint exists on disk. Replays (`--from-start`) keep the previous checkpoint
    // intact until the first batch flushes and reaches `ack`.
    save_initial: bool,
}

impl<S> ExportSource<S> {
    fn save(&mut self, files_only: bool) -> io::Result<()> {
        if let Some(file) = &self.file {
            self.checkpoint.measure_output()?;
            if !files_only || self.checkpoint.output_len.is_some() {
                file.save(&self.checkpoint)?;
            }
        }
        Ok(())
    }
}

impl<S: EventSource> EventSource for ExportSource<S> {
    async fn next_batch(&mut self, max: usize) -> Result<Option<Batch>, SourceError> {
        let batch = self.reader.next_batch(max).await?;
        if std::mem::take(&mut self.save_initial) && batch.is_some() {
            self.save(true)
                .map_err(|err| SourceError::backend("initializing export cursor", err))?;
        }
        Ok(batch)
    }

    async fn ack(&mut self, cursor: &Cursor) -> Result<(), SourceError> {
        self.reader.ack(cursor).await?;
        self.checkpoint.cursor = *cursor;
        self.save(false)
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
        if batch.events.is_empty() {
            return Ok(());
        }
        self.ensure_open()?.write(batch).await
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
    had_saved: bool,
}

fn open_checkpoint(
    args: &Args,
    config: &Config,
    database: Database,
) -> anyhow::Result<OpenedCheckpoint> {
    let mut requested = Checkpoint {
        cursor: Cursor::new(0, 0),
        database,
        schema: config.schema.clone(),
        filters: args.filters.clone(),
        sink: config.export.sink.clone(),
        output_len: None,
        redaction: config.export.redaction.clone(),
    };
    let (SinkConfig::Jsonl { path: output } | SinkConfig::Csv { path: output }) =
        &mut requested.sink;
    if let Some(output) = output {
        *output = resolve_path(output)
            .context("resolving export output")
            .exit_code(Exit::Io)?;
    }
    let Some(path) = &config.export.cursor_file else {
        return Ok(OpenedCheckpoint {
            file: None,
            checkpoint: requested,
            had_saved: false,
        });
    };
    let file = CursorFile::lock(path)
        .with_context(|| format!("opening cursor {}", path.display()))
        .exit_code(Exit::Io)?;
    if let Some(output) = sink_path(&requested.sink)
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
        let saved = file.read().ok().flatten();
        let had_saved = saved.is_some();
        if let Some(saved) = saved
            && requested.sink == saved.sink
        {
            requested.output_len = saved.output_len;
        }
        return Ok(OpenedCheckpoint {
            file: Some(file),
            checkpoint: requested,
            had_saved,
        });
    }
    if let Some(saved) = file
        .read()
        .with_context(|| format!("reading cursor {}", path.display()))
        .exit_code(Exit::Io)?
    {
        saved.check_resume(&requested)?;
        return Ok(OpenedCheckpoint {
            file: Some(file),
            checkpoint: saved,
            had_saved: true,
        });
    }
    Ok(OpenedCheckpoint {
        file: Some(file),
        checkpoint: requested,
        had_saved: false,
    })
}

async fn run_pipeline<S: EventSource, K: Sink>(
    source: &mut ExportSource<S>,
    sink: &mut K,
    options: &Options,
) -> anyhow::Result<Option<auditstream_export::Report>> {
    match auditstream_export::run(source, sink, options).await {
        Ok(report) => Ok(Some(report)),
        Err(ExportError::Sink(SinkError::Io(err)))
            if source.file.is_none()
                && sink_path(&source.checkpoint.sink).is_none()
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

async fn export_with_pool(
    args: &Args,
    config: &Config,
    pool: auditstream_pg::PgPool,
) -> anyhow::Result<()> {
    let database = reader::database_identity(&pool, &config.schema)
        .await
        .context("identifying PostgreSQL database")
        .exit_code(Exit::Database)?
        .into();
    let opened = open_checkpoint(args, config, database)?;
    opened
        .checkpoint
        .restore_output(args.from_start)
        .context("recovering export output")
        .exit_code(Exit::Io)?;
    if let Some(path) = sink_path(&opened.checkpoint.sink) {
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
    }

    let had_saved = opened.had_saved;
    let reader = PgEventSource::new(
        pool,
        &config.schema,
        opened.checkpoint.cursor,
        args.filters.reader_filter(),
    );
    let mut sink = LazySink::new(opened.checkpoint.sink.clone(), opened.file.is_some());
    let mut source = ExportSource {
        reader,
        file: opened.file,
        checkpoint: opened.checkpoint,
        save_initial: !had_saved,
    };
    let options = Options::with_batch_size(config.export.batch_size)
        .with_redaction(config.export.compiled_redaction.clone());
    let Some(report) = run_pipeline(&mut source, &mut sink, &options).await? else {
        return Ok(());
    };
    // Now that the PostgreSQL query succeeded, validate/create any configured output file even
    // when zero events matched, and persist the initial or `--from-start` checkpoint if 0 batches ran.
    sink.ensure_open()
        .context("opening export output")
        .exit_code(Exit::Io)?;
    if report.batches == 0 && (args.from_start || !had_saved) {
        source
            .save(false)
            .context("saving export cursor")
            .exit_code(Exit::Io)?;
    }
    if !config.quiet {
        let cursor = source.checkpoint.cursor;
        eprintln!(
            "events={} batches={} cursor=({}, {})",
            report.events, report.batches, cursor.xact_id, cursor.id
        );
    }
    Ok(())
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
    use auditstream_export::{Options, SinkConfig};
    use auditstream_pg::reader::ExportFilter;
    use tempfile::tempdir;

    use super::{Checkpoint, CursorFile, Database, ExportSource, Filters, LazySink, run_pipeline};
    use crate::exit::{Exit, exit_code_for};

    struct StaticSource {
        batches: std::collections::VecDeque<Result<Batch, SourceError>>,
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

    fn sample_checkpoint(sink: SinkConfig, cursor: Cursor, output_len: Option<u64>) -> Checkpoint {
        Checkpoint {
            cursor,
            database: sample_database(),
            schema: "auditstream".to_owned(),
            filters: Filters::default(),
            sink,
            output_len,
            redaction: Redaction::default(),
        }
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
            SinkConfig::Jsonl {
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
            checkpoint: sample_checkpoint(sink_cfg, Cursor::new(0, 0), None),
            save_initial: false,
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
            SinkConfig::Jsonl {
                path: Some(PathBuf::from("/tmp/a.jsonl")),
            },
            Cursor::new(10, 1),
            Some(128),
        );

        let diff_output = sample_checkpoint(
            SinkConfig::Jsonl {
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
            SinkConfig::Csv {
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
        let initial = sample_checkpoint(sink_cfg.clone(), Cursor::new(42, 7), Some(20));
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
            checkpoint: sample_checkpoint(sink_cfg, Cursor::new(0, 0), Some(20)),
            save_initial: false,
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
            checkpoint: sample_checkpoint(sink_cfg.clone(), Cursor::new(0, 0), None),
            save_initial: false,
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
            checkpoint: sample_checkpoint(sink_cfg, Cursor::new(0, 0), None),
            save_initial: false,
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
            checkpoint: sample_checkpoint(sink_cfg.clone(), Cursor::new(0, 0), None),
            save_initial: false,
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

        // restore_output refuses to truncate a symlink target via O_NOFOLLOW.
        let big_victim = dir.path().join("big_victim.jsonl");
        fs::write(&big_victim, b"0123456789abcdef").expect("write big_victim");
        let out_link = dir.path().join("out_link.jsonl");
        symlink(&big_victim, &out_link).expect("create out symlink");
        let cp = sample_checkpoint(
            SinkConfig::Jsonl {
                path: Some(out_link),
            },
            Cursor::new(1, 1),
            Some(4),
        );
        assert!(cp.restore_output(false).is_err());
        assert_eq!(fs::read(&big_victim).unwrap(), b"0123456789abcdef");
    }
}
