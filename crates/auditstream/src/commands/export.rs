//! Export committed events and checkpoint each batch after its destination flushes.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use auditstream_core::{Batch, Cursor, EventSource, SinkError, Source, SourceError};
use auditstream_export::{AnySink, ExportError, Options, SinkConfig};
use auditstream_pg::reader::{ExportFilter, PgEventSource};
use clap::{Args as ClapArgs, ValueEnum};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

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

#[derive(Debug, Clone, ClapArgs, Serialize, Deserialize)]
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
            source: self.source,
            ..ExportFilter::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    cursor: Cursor,
    database: Database,
    schema: String,
    filters: Filters,
    sink: SinkConfig,
    output_len: Option<u64>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct Database {
    host: PathBuf,
    port: u16,
    name: String,
}

impl Database {
    fn from_pool(pool: &auditstream_pg::PgPool) -> Self {
        let options = pool.connect_options();
        Self {
            host: options
                .get_socket()
                .cloned()
                .unwrap_or_else(|| PathBuf::from(options.get_host())),
            port: options.get_port(),
            name: options
                .get_database()
                .unwrap_or_else(|| options.get_username())
                .to_owned(),
        }
    }
}

fn sink_path(sink: &SinkConfig) -> Option<&Path> {
    match sink {
        SinkConfig::Jsonl { path } | SinkConfig::Csv { path } => path.as_deref(),
    }
}

fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    match path.canonicalize() {
        Err(err) if err.kind() == io::ErrorKind::NotFound && !path.is_symlink() => {
            let name = path.file_name().ok_or(err)?;
            let parent = path
                .parent()
                .filter(|dir| !dir.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(parent.canonicalize()?.join(name))
        }
        other => other,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(unix)]
    if let (Ok(a), Ok(b)) = (a.metadata(), b.metadata()) {
        use std::os::unix::fs::MetadataExt;
        return a.dev() == b.dev() && a.ino() == b.ino();
    }
    false
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

impl Checkpoint {
    fn check_resume(&self, request: &Self) -> anyhow::Result<()> {
        for (changed, flag) in [
            (self.filters.since != request.filters.since, "--since"),
            (self.filters.until != request.filters.until, "--until"),
            (self.filters.actor != request.filters.actor, "--actor"),
            (self.filters.source != request.filters.source, "--source"),
            (self.schema != request.schema, "--schema"),
            (self.database != request.database, "database"),
            (
                std::mem::discriminant(&self.sink) != std::mem::discriminant(&request.sink),
                "--format",
            ),
        ] {
            if changed {
                return Err(anyhow!(
                    "{flag} differs from the saved cursor. Use --from-start to replay"
                ))
                .exit_code(Exit::Usage);
            }
        }
        if sink_path(&self.sink) != sink_path(&request.sink) {
            return Err(anyhow!("output path differs from the saved cursor")).exit_code(Exit::Io);
        }
        Ok(())
    }

    fn measure_output(&mut self) -> io::Result<()> {
        self.output_len = match sink_path(&self.sink) {
            Some(path) => {
                let meta = path.metadata()?;
                meta.is_file().then_some(meta.len())
            }
            None => None,
        };
        Ok(())
    }

    fn restore_output(&self, from_start: bool) -> io::Result<()> {
        let (Some(path), Some(length)) = (sink_path(&self.sink), self.output_len) else {
            return Ok(());
        };
        let file = match File::options().write(true).open(path) {
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
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_added_extension("lock"))?;
        lock.try_lock()
            .with_context(|| format!("cannot lock cursor {}", path.display()))?;
        Ok(Self { path, _lock: lock })
    }

    fn read(&self) -> io::Result<Option<Checkpoint>> {
        match File::open(&self.path) {
            Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn save(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        // Exclusive creation prevents following symlinks or overwriting files left by another process.
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut attempt = 0_u64;
        let (temp, file) = loop {
            let temp = self
                .path
                .with_added_extension(format!("tmp.{}.{attempt}", std::process::id()));
            match options.open(&temp) {
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => attempt += 1,
                result => break (temp, result?),
            }
        };
        let result = (|| {
            serde_json::to_writer(&file, checkpoint)?;
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

struct ExportSource {
    reader: PgEventSource,
    file: Option<CursorFile>,
    checkpoint: Checkpoint,
    first_read: bool,
}

impl ExportSource {
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

impl EventSource for ExportSource {
    async fn next_batch(&mut self, max: usize) -> Result<Option<Batch>, SourceError> {
        let batch = self.reader.next_batch(max).await?;
        if std::mem::take(&mut self.first_read) {
            // On a new file export, preserve its original length before writing the first batch.
            // A failed PostgreSQL read must leave any previous checkpoint intact.
            self.save(batch.is_some())
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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(export(args, config))
}

fn open_checkpoint(
    args: &Args,
    config: &Config,
    pool: &auditstream_pg::PgPool,
) -> anyhow::Result<(Option<CursorFile>, Checkpoint, bool)> {
    let mut requested = Checkpoint {
        cursor: Cursor::new(0, 0),
        database: Database::from_pool(pool),
        schema: config.schema.clone(),
        filters: args.filters.clone(),
        sink: config.export.sink.clone(),
        output_len: None,
    };
    let Some(path) = &config.export.cursor_file else {
        return Ok((None, requested, false));
    };
    let file = CursorFile::lock(path)
        .with_context(|| format!("opening cursor {}", path.display()))
        .exit_code(Exit::Io)?;
    let (SinkConfig::Jsonl { path: output } | SinkConfig::Csv { path: output }) =
        &mut requested.sink;
    if let Some(output) = output {
        *output = resolve_path(output)
            .context("resolving export output")
            .exit_code(Exit::Io)?;
        if same_file(output, &file.path)
            || same_file(output, &file.path.with_added_extension("lock"))
        {
            return Err(anyhow!(
                "--output must differ from the cursor and its lock file"
            ))
            .exit_code(Exit::Usage);
        }
    }
    if args.from_start {
        // Replay starts at the beginning, but a partial batch in this destination still needs repair.
        if let Ok(Some(saved)) = file.read()
            && requested.sink == saved.sink
        {
            requested.output_len = saved.output_len;
        }
    } else if let Some(saved) = file
        .read()
        .with_context(|| format!("reading cursor {}", path.display()))
        .exit_code(Exit::Io)?
    {
        saved.check_resume(&requested)?;
        return Ok((Some(file), saved, true));
    }
    Ok((Some(file), requested, false))
}

async fn export(args: &Args, config: &Config) -> anyhow::Result<()> {
    let url = config.database_url.as_ref().context("missing database URL: pass --database-url or set AUDITSTREAM_DATABASE_URL / DATABASE_URL").exit_code(Exit::Usage)?;
    let pool = auditstream_pg::connect(url.expose())
        .await
        .with_context(|| format!("could not connect to database {}", url.redacted()))
        .exit_code(Exit::Database)?;
    let (file, checkpoint, resuming) = open_checkpoint(args, config, &pool)?;
    checkpoint
        .restore_output(args.from_start)
        .context("recovering export output")
        .exit_code(Exit::Io)?;
    let reader = PgEventSource::new(
        pool,
        &config.schema,
        checkpoint.cursor,
        args.filters.reader_filter(),
    );
    let mut sink = AnySink::open(&checkpoint.sink)
        .context("opening export output")
        .exit_code(Exit::Io)?;
    #[cfg(unix)]
    if file.is_some()
        && let Some(path) = sink_path(&checkpoint.sink)
        && path.is_file()
    {
        // The output directory entry must survive a crash before a durable cursor can refer to it.
        sync_parent(path)
            .context("syncing export output directory")
            .exit_code(Exit::Io)?;
    }
    let mut source = ExportSource {
        reader,
        file,
        checkpoint,
        first_read: !resuming,
    };
    let options = Options::with_batch_size(config.export.batch_size);
    let report = match auditstream_export::run(&mut source, &mut sink, &options).await {
        Ok(report) => report,
        Err(ExportError::Sink(SinkError::Io(err)))
            if sink_path(&source.checkpoint.sink).is_none()
                && err.kind() == io::ErrorKind::BrokenPipe =>
        {
            return Ok(());
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
                _ => Exit::Internal,
            };
            return Err(err).exit_code(exit);
        }
    };
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
    use super::{ExportFilter, Filters};

    #[test]
    fn missing_checkpoint_filters_mean_no_restrictions() {
        let filters: Filters = serde_json::from_str("{}").expect("omitted filters are optional");
        assert_eq!(filters.reader_filter(), ExportFilter::default());
    }
}
