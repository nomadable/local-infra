//! Endpoint resolution, TLS setup, and PostgreSQL session lifetime.

use crate::core::error::{Diagnostic, Error, Result};
use crate::core::model::TunnelStatus;
use crate::core::progress::Reporter;
use crate::core::sql::profile::{
    AccessMode, ProfileSpec, ResolvedSqlConnection, SqlConnectionSource, SqlEndpoint, TlsMode,
    DEFAULT_CELL_BYTES, DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_PREVIEW_ROWS, DEFAULT_QUERY_TIMEOUT_MS,
};
use crate::core::{database, tunnel, Ctx};
use rustls::{ClientConfig, RootCertStore};
use std::fs::File;
use std::io::BufReader;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

pub struct ResolveOptions<'a> {
    pub writable: bool,
    pub confirmation: Option<&'a str>,
    pub password: Option<String>,
    pub start_tunnel: bool,
}

impl Default for ResolveOptions<'_> {
    fn default() -> Self {
        Self {
            writable: false,
            confirmation: None,
            password: None,
            start_tunnel: true,
        }
    }
}

pub async fn resolve(
    ctx: &Ctx,
    key: &str,
    options: ResolveOptions<'_>,
) -> Result<ResolvedSqlConnection> {
    let database = ctx.store.find_database(key)?;
    let profile = ctx.store.find_sql_profile(key)?;
    match (database, profile) {
        (Some(_), Some(_)) => Err(Error::Conflict(format!(
            "`{key}` 이름이 managed DB와 SQL connection에 모두 존재합니다. id를 사용하세요."
        ))),
        (Some(database), None) => resolve_managed(ctx, &database.id, options.start_tunnel).await,
        (None, Some(profile)) => {
            let access = external_access(&profile.name, profile.access_mode, &options)?;
            let password = match options.password {
                Some(password) => Some(password),
                None => match &profile.credential_ref {
                    Some(reference) => ctx.secrets.get(reference)?,
                    None => None,
                },
            };
            Ok(ResolvedSqlConnection::new(
                SqlConnectionSource::ExternalProfile {
                    profile_id: profile.id.clone(),
                },
                SqlEndpoint {
                    label: profile.name,
                    host: profile.host,
                    port: profile.port,
                    database: profile.database,
                    username: profile.username,
                    tls_mode: profile.tls_mode,
                    access,
                    external: true,
                    root_ca_path: profile.root_ca_path,
                    connect_timeout_ms: profile.connect_timeout_ms,
                    query_timeout_ms: profile.query_timeout_ms,
                    application_name: profile.application_name,
                    preview_rows: profile.preview_rows,
                    history_text: profile.history_text,
                },
                password,
            ))
        }
        (None, None) => Err(Error::NotFound(format!(
            "DB 또는 SQL connection `{key}`을(를) 찾을 수 없습니다."
        ))),
    }
}

pub fn resolve_external_spec(
    spec: &ProfileSpec,
    password: Option<String>,
) -> Result<ResolvedSqlConnection> {
    let profile = spec.clone().into_profile("connection-test".into(), None);
    profile.validate()?;
    Ok(ResolvedSqlConnection::new(
        SqlConnectionSource::ExternalProfile {
            profile_id: profile.id,
        },
        SqlEndpoint {
            label: profile.name,
            host: profile.host,
            port: profile.port,
            database: profile.database,
            username: profile.username,
            tls_mode: profile.tls_mode,
            access: AccessMode::ReadOnly,
            external: true,
            root_ca_path: profile.root_ca_path,
            connect_timeout_ms: profile.connect_timeout_ms,
            query_timeout_ms: profile.query_timeout_ms,
            application_name: profile.application_name,
            preview_rows: profile.preview_rows,
            history_text: false,
        },
        password,
    ))
}

fn external_access(
    profile_name: &str,
    allowed: AccessMode,
    options: &ResolveOptions<'_>,
) -> Result<AccessMode> {
    if !options.writable {
        return Ok(AccessMode::ReadOnly);
    }
    if allowed != AccessMode::ReadWrite {
        return Err(Error::Refused(format!(
            "SQL connection `{profile_name}`은(는) read-only로 등록되어 있습니다."
        )));
    }
    if options.confirmation != Some(profile_name) {
        return Err(Error::Refused(format!(
            "writable session을 열려면 connection 이름 `{profile_name}`을(를) 다시 입력해야 합니다."
        )));
    }
    Ok(AccessMode::ReadWrite)
}

async fn resolve_managed(
    ctx: &Ctx,
    database_id: &str,
    start_tunnel: bool,
) -> Result<ResolvedSqlConnection> {
    let mut view = database::view(ctx, database_id).await?;
    if view.target.is_remote()
        && !view
            .tunnel
            .as_ref()
            .is_some_and(|session| session.status == TunnelStatus::Active)
    {
        if !start_tunnel {
            return Err(Error::Refused(format!(
                "원격 DB `{}`의 SSH tunnel이 실행 중이 아닙니다. `linf tunnel start {}`를 먼저 실행하세요.",
                view.database.database_name, view.database.database_name
            )));
        }
        let resource = tunnel::TunnelTarget::database(&view.database);
        tunnel::start(ctx, &resource, &view.engine, &view.target).await?;
        view = database::view(ctx, database_id).await?;
    }
    let connection = database::connection_info(ctx, &view)?;
    Ok(ResolvedSqlConnection::new(
        SqlConnectionSource::ManagedDatabase {
            database_id: view.database.id,
        },
        SqlEndpoint {
            label: view.database.database_name,
            host: connection.host,
            port: connection.port,
            database: connection.database,
            username: connection.username,
            tls_mode: TlsMode::Disable,
            access: AccessMode::ReadWrite,
            external: false,
            root_ca_path: None,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            query_timeout_ms: DEFAULT_QUERY_TIMEOUT_MS,
            application_name: "linf-sql".into(),
            preview_rows: DEFAULT_PREVIEW_ROWS,
            history_text: true,
        },
        connection.password,
    ))
}

#[derive(Clone)]
enum CancelTransport {
    Plain,
    Rustls(MakeRustlsConnect),
}

impl CancelTransport {
    async fn cancel(&self, token: tokio_postgres::CancelToken) -> Result<()> {
        let outcome = match self {
            Self::Plain => token.cancel_query(NoTls).await,
            Self::Rustls(tls) => token.cancel_query(tls.clone()).await,
        };
        outcome.map_err(|error| postgres_failure("쿼리를 취소할 수 없습니다", &error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    Idle,
    InTransaction,
    Failed,
}

pub struct SqlSession {
    client: Client,
    transport: CancelTransport,
    connection_task: JoinHandle<()>,
    pub source: SqlConnectionSource,
    pub endpoint: SqlEndpoint,
    pub transaction: TransactionState,
    pub generation: u64,
}

impl SqlSession {
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn cancel_token(&self) -> tokio_postgres::CancelToken {
        self.client.cancel_token()
    }

    pub async fn cancel(&self, token: tokio_postgres::CancelToken) -> Result<()> {
        self.transport.cancel(token).await
    }

    pub fn closed(&self) -> bool {
        self.client.is_closed() || self.connection_task.is_finished()
    }
}

impl Drop for SqlSession {
    fn drop(&mut self) {
        self.connection_task.abort();
    }
}
pub async fn connect(resolved: ResolvedSqlConnection) -> Result<SqlSession> {
    if !resolved.source.external() && resolved.password().is_none() {
        return Err(Error::failed(
            "PostgreSQL 자격 증명을 사용할 수 없습니다",
            format!(
                "`{}` managed database 비밀번호가 secret store에 없습니다.",
                resolved.endpoint.label
            ),
            "secret store 설정을 복구하거나 managed DB 비밀번호를 교체하세요.",
        ));
    }
    let mut config = Config::new();
    config
        .host(&resolved.endpoint.host)
        .port(resolved.endpoint.port)
        .dbname(&resolved.endpoint.database)
        .user(&resolved.endpoint.username)
        .application_name(&resolved.endpoint.application_name)
        .connect_timeout(Duration::from_millis(resolved.endpoint.connect_timeout_ms));
    if let Some(password) = resolved.password() {
        config.password(password);
    }

    let (client, task, transport) = match resolved.endpoint.tls_mode {
        TlsMode::Disable => {
            config.ssl_mode(SslMode::Disable);
            let (client, connection) = config
                .connect(NoTls)
                .await
                .map_err(|error| postgres_failure("PostgreSQL 연결에 실패했습니다", &error))?;
            let task = tokio::spawn(async move {
                let _ = connection.await;
            });
            (client, task, CancelTransport::Plain)
        }
        TlsMode::VerifyFull => {
            config.ssl_mode(SslMode::Require);
            let tls = tls_connector(resolved.endpoint.root_ca_path.as_deref())?;
            let (client, connection) = config
                .connect(tls.clone())
                .await
                .map_err(|error| postgres_failure("PostgreSQL TLS 연결에 실패했습니다", &error))?;
            let task = tokio::spawn(async move {
                let _ = connection.await;
            });
            (client, task, CancelTransport::Rustls(tls))
        }
    };

    Ok(SqlSession {
        client,
        transport,
        connection_task: task,
        source: resolved.source,
        endpoint: resolved.endpoint,
        transaction: TransactionState::Idle,
        generation: 1,
    })
}

fn tls_connector(root_ca_path: Option<&std::path::Path>) -> Result<MakeRustlsConnect> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let native_count = native.certs.len();
    for certificate in native.certs {
        let _ = roots.add(certificate);
    }
    if let Some(path) = root_ca_path {
        let file = File::open(path).map_err(|error| {
            Error::failed(
                "TLS root CA를 읽을 수 없습니다",
                format!("{}: {error}", path.display()),
                "connection의 root CA 경로와 파일 권한을 확인하세요.",
            )
        })?;
        let mut reader = BufReader::new(file);
        let certificates =
            rustls_pemfile::certs(&mut reader).collect::<std::result::Result<Vec<_>, _>>()?;
        if certificates.is_empty() {
            return Err(Error::Usage(format!(
                "TLS root CA 파일 `{}`에 인증서가 없습니다.",
                path.display()
            )));
        }
        for certificate in certificates {
            roots.add(certificate).map_err(|error| {
                Error::failed(
                    "TLS root CA를 추가할 수 없습니다",
                    error.to_string(),
                    "PEM 인증서 파일을 확인하세요.",
                )
            })?;
        }
    } else if native_count == 0 {
        return Err(Error::failed(
            "운영체제 TLS root 인증서를 찾을 수 없습니다",
            native
                .errors
                .first()
                .map(ToString::to_string)
                .unwrap_or_else(|| "root certificate store가 비어 있습니다.".into()),
            "운영체제 CA bundle을 설치하거나 connection에 root CA 경로를 지정하세요.",
        ));
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

pub async fn test(resolved: ResolvedSqlConnection) -> Result<()> {
    let session = connect(resolved).await?;
    session
        .client()
        .simple_query("SELECT 1")
        .await
        .map_err(|error| postgres_failure("PostgreSQL connection test에 실패했습니다", &error))?;
    Ok(())
}

pub fn postgres_failure(what: &str, error: &tokio_postgres::Error) -> Error {
    let cause = if let Some(db) = error.as_db_error() {
        format!("{} [{}]: {}", db.severity(), db.code().code(), db.message())
    } else {
        crate::core::util::redact(&error.to_string())
    };
    Error::diagnostic(Diagnostic::new(
        what,
        cause,
        "endpoint, TLS 설정, PostgreSQL 인증 정보와 서버 상태를 확인하세요.",
    ))
}

pub fn limits(endpoint: &SqlEndpoint) -> (usize, usize, usize) {
    (
        endpoint.preview_rows,
        DEFAULT_CELL_BYTES,
        crate::core::sql::profile::DEFAULT_RESULT_BYTES,
    )
}

#[allow(dead_code)]
fn _keep_reporter_contract(_: &Reporter) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_sessions_require_allowed_access_and_exact_confirmation() {
        let defaults = ResolveOptions::default();
        assert_eq!(
            external_access("production", AccessMode::ReadWrite, &defaults).unwrap(),
            AccessMode::ReadOnly
        );

        let missing_confirmation = ResolveOptions {
            writable: true,
            confirmation: None,
            ..ResolveOptions::default()
        };
        assert!(
            external_access("production", AccessMode::ReadWrite, &missing_confirmation).is_err()
        );

        let wrong_confirmation = ResolveOptions {
            writable: true,
            confirmation: Some("Production"),
            ..ResolveOptions::default()
        };
        assert!(external_access("production", AccessMode::ReadWrite, &wrong_confirmation).is_err());

        let confirmed = ResolveOptions {
            writable: true,
            confirmation: Some("production"),
            ..ResolveOptions::default()
        };
        assert!(
            external_access("production", AccessMode::ReadOnly, &confirmed).is_err(),
            "a read-only profile cannot be escalated"
        );
        assert_eq!(
            external_access("production", AccessMode::ReadWrite, &confirmed).unwrap(),
            AccessMode::ReadWrite
        );
    }
}
