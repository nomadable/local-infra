//! PostgreSQL connection profiles and resolved endpoint contracts.

use crate::core::error::{Error, Result};
use crate::core::util::{new_id, now};
use crate::core::{secrets, Ctx};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_QUERY_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_PREVIEW_ROWS: usize = 5_000;
pub const DEFAULT_CELL_BYTES: usize = 16 * 1024;
pub const DEFAULT_RESULT_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_HISTORY_ENTRIES: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "kebab-case")]
pub enum AccessMode {
    ReadOnly,
    ReadWrite,
}

impl AccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "read_only" => Ok(Self::ReadOnly),
            "read_write" => Ok(Self::ReadWrite),
            other => Err(Error::Usage(format!(
                "알 수 없는 SQL access mode `{other}`"
            ))),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ ONLY",
            Self::ReadWrite => "WRITABLE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "kebab-case")]
pub enum TlsMode {
    VerifyFull,
    Disable,
}

impl TlsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifyFull => "verify_full",
            Self::Disable => "disable",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "verify_full" => Ok(Self::VerifyFull),
            "disable" => Ok(Self::Disable),
            other => Err(Error::Usage(format!("알 수 없는 SQL TLS mode `{other}`"))),
        }
    }

    pub fn warning(self) -> Option<&'static str> {
        (self == Self::Disable)
            .then_some("TLS가 꺼져 있습니다. 네트워크에서 자격 증명과 쿼리가 노출될 수 있습니다.")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlConnectionProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub tls_mode: TlsMode,
    pub access_mode: AccessMode,
    pub credential_ref: Option<String>,
    pub root_ca_path: Option<PathBuf>,
    pub client_certificate_path: Option<PathBuf>,
    pub client_key_path: Option<PathBuf>,
    pub connect_timeout_ms: u64,
    pub query_timeout_ms: u64,
    pub application_name: String,
    pub history_text: bool,
    pub preview_rows: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SqlConnectionProfile {
    pub fn endpoint_label(&self) -> String {
        format!(
            "{}@{}:{}/{}",
            self.username, self.host, self.port, self.database
        )
    }

    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.host.trim().is_empty() {
            return Err(Error::Usage("SQL host를 입력하세요.".into()));
        }
        if self.database.trim().is_empty() {
            return Err(Error::Usage("SQL database 이름을 입력하세요.".into()));
        }
        if self.username.trim().is_empty() {
            return Err(Error::Usage("SQL username을 입력하세요.".into()));
        }
        if self.port == 0 {
            return Err(Error::Usage("SQL port는 1 이상이어야 합니다.".into()));
        }
        if self.connect_timeout_ms == 0 || self.query_timeout_ms == 0 {
            return Err(Error::Usage("SQL timeout은 1ms 이상이어야 합니다.".into()));
        }
        if self.preview_rows == 0 {
            return Err(Error::Usage(
                "preview row limit은 1 이상이어야 합니다.".into(),
            ));
        }
        if self.client_certificate_path.is_some() || self.client_key_path.is_some() {
            return Err(Error::Refused(
                "첫 릴리스에서는 client certificate 인증을 저장하거나 사용하지 않습니다.".into(),
            ));
        }
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Usage("SQL connection 이름을 입력하세요.".into()));
    }
    if name.len() > 64 {
        return Err(Error::Usage(
            "SQL connection 이름은 64 bytes 이하여야 합니다.".into(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::Usage(
            "SQL connection 이름에는 제어 문자를 사용할 수 없습니다.".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SqlConnectionSource {
    ManagedDatabase { database_id: String },
    ExternalProfile { profile_id: String },
}

impl SqlConnectionSource {
    pub fn id(&self) -> &str {
        match self {
            Self::ManagedDatabase { database_id } => database_id,
            Self::ExternalProfile { profile_id } => profile_id,
        }
    }

    pub fn external(&self) -> bool {
        matches!(self, Self::ExternalProfile { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlEndpoint {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub tls_mode: TlsMode,
    pub access: AccessMode,
    pub external: bool,
    pub root_ca_path: Option<PathBuf>,
    pub connect_timeout_ms: u64,
    pub query_timeout_ms: u64,
    pub application_name: String,
    pub preview_rows: usize,
    pub history_text: bool,
}

impl SqlEndpoint {
    pub fn redacted_url(&self) -> String {
        format!(
            "postgresql://{}:****@{}:{}/{}",
            crate::core::util::pct_encode(&self.username),
            self.host,
            self.port,
            self.database
        )
    }
}

pub struct ResolvedSqlConnection {
    pub source: SqlConnectionSource,
    pub endpoint: SqlEndpoint,
    password: Option<String>,
}

impl ResolvedSqlConnection {
    pub fn new(
        source: SqlConnectionSource,
        endpoint: SqlEndpoint,
        password: Option<String>,
    ) -> Self {
        Self {
            source,
            endpoint,
            password,
        }
    }

    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    pub fn set_password(&mut self, password: Option<String>) {
        self.password = password;
    }
}

impl fmt::Debug for ResolvedSqlConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedSqlConnection")
            .field("source", &self.source)
            .field("endpoint", &self.endpoint)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryHistoryEntry {
    pub id: String,
    pub profile_id: Option<String>,
    pub profile_label: String,
    pub executed_at: DateTime<Utc>,
    pub success: bool,
    pub elapsed_ms: u64,
    pub row_count: u64,
    pub query_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSpec {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub tls_mode: TlsMode,
    pub access_mode: AccessMode,
    pub root_ca_path: Option<PathBuf>,
    pub connect_timeout_ms: u64,
    pub query_timeout_ms: u64,
    pub application_name: String,
    pub history_text: bool,
    pub preview_rows: usize,
}

impl ProfileSpec {
    pub fn into_profile(self, id: String, credential_ref: Option<String>) -> SqlConnectionProfile {
        let timestamp = now();
        SqlConnectionProfile {
            id,
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            port: self.port,
            database: self.database.trim().to_string(),
            username: self.username.trim().to_string(),
            tls_mode: self.tls_mode,
            access_mode: self.access_mode,
            credential_ref,
            root_ca_path: self.root_ca_path,
            client_certificate_path: None,
            client_key_path: None,
            connect_timeout_ms: self.connect_timeout_ms,
            query_timeout_ms: self.query_timeout_ms,
            application_name: self.application_name.trim().to_string(),
            history_text: self.history_text,
            preview_rows: self.preview_rows,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }
}

pub fn create(
    ctx: &Ctx,
    spec: ProfileSpec,
    password: Option<&str>,
    store_password: bool,
) -> Result<SqlConnectionProfile> {
    ctx.require_write_lock()?;
    if store_password && !ctx.secrets.persistent() {
        return Err(Error::Refused(
            "현재 secret store가 비밀번호 미저장 모드라 외부 DB 비밀번호를 저장할 수 없습니다."
                .into(),
        ));
    }
    let id = new_id();
    let credential_ref =
        (store_password && password.is_some()).then(|| secrets::sql_profile_ref(&id));
    let profile = spec.into_profile(id, credential_ref.clone());
    profile.validate()?;
    if let (Some(reference), Some(secret)) = (&credential_ref, password) {
        if secret.is_empty() {
            return Err(Error::Usage("빈 SQL 비밀번호는 저장할 수 없습니다.".into()));
        }
        ctx.secrets.set(reference, secret)?;
    }
    if let Err(error) = ctx.store.insert_sql_profile(&profile) {
        if let Some(reference) = &credential_ref {
            let _ = ctx.secrets.delete(reference);
        }
        return Err(error);
    }
    Ok(profile)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PasswordUpdate<'a> {
    Keep,
    Remove,
    Replace(&'a str),
}

impl fmt::Debug for PasswordUpdate<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => formatter.write_str("Keep"),
            Self::Remove => formatter.write_str("Remove"),
            Self::Replace(_) => formatter.write_str("Replace([REDACTED])"),
        }
    }
}

pub fn update(
    ctx: &Ctx,
    key: &str,
    spec: ProfileSpec,
    password: PasswordUpdate<'_>,
) -> Result<SqlConnectionProfile> {
    ctx.require_write_lock()?;
    let mut profile = require(ctx, key)?;
    let previous_ref = profile.credential_ref.clone();
    let replaced_secret = if matches!(password, PasswordUpdate::Replace(_)) {
        match &previous_ref {
            Some(reference) => ctx.secrets.get(reference)?,
            None => None,
        }
    } else {
        None
    };
    let credential_ref = match password {
        PasswordUpdate::Keep => previous_ref.clone(),
        PasswordUpdate::Remove => None,
        PasswordUpdate::Replace(secret) => {
            if !ctx.secrets.persistent() {
                return Err(Error::Refused(
                    "현재 secret store가 비밀번호 미저장 모드라 외부 DB 비밀번호를 저장할 수 없습니다."
                        .into(),
                ));
            }
            if secret.is_empty() {
                return Err(Error::Usage("빈 SQL 비밀번호는 저장할 수 없습니다.".into()));
            }
            Some(secrets::sql_profile_ref(&profile.id))
        }
    };
    let created_at = profile.created_at;
    profile = spec.into_profile(profile.id, credential_ref.clone());
    profile.created_at = created_at;
    profile.updated_at = now();
    profile.validate()?;

    if let (PasswordUpdate::Replace(secret), Some(reference)) = (password, &credential_ref) {
        ctx.secrets.set(reference, secret)?;
    }
    if let Err(error) = ctx.store.update_sql_profile(&profile) {
        if let (PasswordUpdate::Replace(_), Some(reference)) = (password, &credential_ref) {
            match replaced_secret {
                Some(secret) => {
                    let _ = ctx.secrets.set(reference, &secret);
                }
                None => {
                    let _ = ctx.secrets.delete(reference);
                }
            }
        }
        return Err(error);
    }
    if matches!(password, PasswordUpdate::Remove) {
        if let Some(reference) = previous_ref {
            ctx.secrets.delete(&reference)?;
        }
    }
    Ok(profile)
}

pub fn list(ctx: &Ctx) -> Result<Vec<SqlConnectionProfile>> {
    ctx.store.list_sql_profiles()
}

pub fn find(ctx: &Ctx, key: &str) -> Result<Option<SqlConnectionProfile>> {
    ctx.store.find_sql_profile(key)
}

pub fn require(ctx: &Ctx, key: &str) -> Result<SqlConnectionProfile> {
    find(ctx, key)?
        .ok_or_else(|| Error::NotFound(format!("SQL connection `{key}`을(를) 찾을 수 없습니다.")))
}

pub fn forget(ctx: &Ctx, key: &str) -> Result<SqlConnectionProfile> {
    ctx.require_write_lock()?;
    let profile = require(ctx, key)?;
    ctx.store.delete_sql_profile(&profile.id)?;
    if let Some(reference) = &profile.credential_ref {
        ctx.secrets.delete(reference)?;
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_update_debug_is_redacted() {
        let rendered = format!("{:?}", PasswordUpdate::Replace("never-print-this"));
        assert_eq!(rendered, "Replace([REDACTED])");
        assert!(!rendered.contains("never-print-this"));
    }
}
