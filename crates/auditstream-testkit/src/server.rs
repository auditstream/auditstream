use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt, ReuseDirective};
use tokio::sync::Mutex;
use url::Url;

use crate::{DEFAULT_PG_MAJOR, Error};

const PG_PORT: u16 = 5432;
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

static CONTAINER_URLS: LazyLock<Mutex<HashMap<u16, Url>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Resolves the `postgres` maintenance database URL for the target server.
///
/// Uses `DATABASE_URL` when set and non-empty; otherwise starts or reuses a
/// named `postgres:<major>` container where `<major>` comes from
/// `AUDITSTREAM_TEST_PG` (defaulting to [`DEFAULT_PG_MAJOR`]).
pub(crate) async fn resolve_admin_url() -> Result<Url, Error> {
    if let Ok(raw) = env::var("DATABASE_URL") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return admin_url_from_database_url(trimmed);
        }
    }

    let major = parse_pg_major(env::var("AUDITSTREAM_TEST_PG").ok().as_deref())?;

    if let Some(url) = CONTAINER_URLS.lock().await.get(&major).cloned() {
        return Ok(url);
    }

    let url = ensure_reusable_container(major).await?;
    CONTAINER_URLS.lock().await.insert(major, url.clone());
    Ok(url)
}

pub(crate) fn parse_pg_major(raw: Option<&str>) -> Result<u16, Error> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_PG_MAJOR);
    };

    let major: u16 = raw
        .parse()
        .map_err(|_| Error::InvalidPgMajor(raw.to_owned()))?;
    if major == 0 {
        return Err(Error::InvalidPgMajor(raw.to_owned()));
    }
    Ok(major)
}

pub(crate) fn admin_url_from_database_url(raw: &str) -> Result<Url, Error> {
    let mut url = Url::parse(raw).map_err(|source| Error::InvalidDatabaseUrl {
        url: raw.to_owned(),
        source,
    })?;
    // Always run admin DDL and advisory locks against the `postgres`
    // maintenance database, never `template1` or an application database.
    url.set_path("/postgres");
    Ok(url)
}

pub(crate) fn database_url_for(
    admin_url: &Url,
    db_name: &str,
    credentials: Option<(&str, &str)>,
) -> Result<String, Error> {
    let mut url = admin_url.clone();
    url.set_path(&format!("/{db_name}"));
    if let Some((user, password)) = credentials {
        url.set_username(user)
            .map_err(|()| Error::UrlCredentials(user.to_owned()))?;
        url.set_password(Some(password))
            .map_err(|()| Error::UrlCredentials(user.to_owned()))?;
    }
    Ok(url.into())
}

struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

async fn acquire_container_lock(major: u16) -> Result<FileLock, Error> {
    let path = env::temp_dir().join(format!("auditstream-testkit-pg{major}.lock"));
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| Error::LockFile {
                path: path.clone(),
                source,
            })?;
        file.lock()
            .map_err(|source| Error::LockFile { path, source })?;
        Ok(FileLock(file))
    })
    .await
    .map_err(Error::Join)?
}

async fn ensure_reusable_container(major: u16) -> Result<Url, Error> {
    // Serialize container creation/start across concurrent nextest processes
    // so only one process creates the named container while peers wait.
    let _file_lock = acquire_container_lock(major).await?;

    let container_name = format!("auditstream-testkit-pg{major}");
    let tag = major.to_string();
    let container = GenericImage::new("postgres", tag.as_str())
        .with_exposed_port(PG_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_DB", "postgres")
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_cmd([
            "-c",
            "fsync=off",
            "-c",
            "synchronous_commit=off",
            "-c",
            "max_connections=300",
        ])
        .with_container_name(&container_name)
        .with_reuse(ReuseDirective::Always)
        .start()
        .await
        .map_err(|source| Error::Container(source.to_string()))?;

    let host = container
        .get_host()
        .await
        .map_err(|source| Error::Container(source.to_string()))?;
    let port = container
        .get_host_port_ipv4(PG_PORT)
        .await
        .map_err(|source| Error::Container(source.to_string()))?;

    let raw_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let admin_url = admin_url_from_database_url(&raw_url)?;
    wait_until_ready(admin_url.as_str()).await?;
    Ok(admin_url)
}

async fn wait_until_ready(admin_url: &str) -> Result<(), Error> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last_err: Option<sqlx::Error> = None;

    while Instant::now() < deadline {
        match PgConnection::connect(admin_url).await {
            Ok(mut conn) => match conn.execute("SELECT 1").await {
                Ok(_) => {
                    let _ = conn.close().await;
                    return Ok(());
                }
                Err(err) => {
                    let _ = conn.close().await;
                    last_err = Some(err);
                }
            },
            Err(err) => {
                last_err = Some(err);
            }
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }

    Err(Error::ContainerReadyTimeout {
        timeout: READY_TIMEOUT,
        last_error: last_err.map_or_else(
            || "no connection attempts recorded".to_owned(),
            |e| e.to_string(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pg_major_defaults_and_validates() {
        assert_eq!(parse_pg_major(None).ok(), Some(DEFAULT_PG_MAJOR));
        assert_eq!(parse_pg_major(Some("")).ok(), Some(DEFAULT_PG_MAJOR));
        assert_eq!(parse_pg_major(Some("  ")).ok(), Some(DEFAULT_PG_MAJOR));
        assert_eq!(parse_pg_major(Some("15")).ok(), Some(15));
        assert_eq!(parse_pg_major(Some(" 18 ")).ok(), Some(18));
        assert!(parse_pg_major(Some("0")).is_err());
        assert!(parse_pg_major(Some("latest")).is_err());
    }

    #[test]
    fn admin_url_always_targets_postgres_maintenance_db() {
        let url = admin_url_from_database_url("postgres://u:p@localhost:5432/template1")
            .expect("valid database url");
        assert_eq!(url.as_str(), "postgres://u:p@localhost:5432/postgres");

        let url = admin_url_from_database_url("postgres://u:p@127.0.0.1:5433/app?sslmode=disable")
            .expect("valid database url");
        assert_eq!(
            url.as_str(),
            "postgres://u:p@127.0.0.1:5433/postgres?sslmode=disable"
        );
    }

    #[test]
    fn database_url_for_rewrites_database_and_optional_credentials() {
        let admin =
            admin_url_from_database_url("postgres://postgres:secret@127.0.0.1:5432/postgres")
                .expect("valid admin url");
        let db_url =
            database_url_for(&admin, "auditstream_test_123", None).expect("db url without creds");
        assert_eq!(
            db_url,
            "postgres://postgres:secret@127.0.0.1:5432/auditstream_test_123"
        );

        let non_super_url = database_url_for(
            &admin,
            "auditstream_test_123",
            Some(("auditstream_role_123", "pw_123")),
        )
        .expect("db url with non-superuser creds");
        assert_eq!(
            non_super_url,
            "postgres://auditstream_role_123:pw_123@127.0.0.1:5432/auditstream_test_123"
        );
    }
}
