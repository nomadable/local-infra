//! Persistent domain model (PRD §13) plus the runtime view structs that both
//! `cli` and `tui` render.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Local,
    Ssh,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Local => "local",
            TargetKind::Ssh => "ssh",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(TargetKind::Local),
            "ssh" => Some(TargetKind::Ssh),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    /// Delegate to a running `ssh-agent`.
    Agent,
    /// Explicit private key path passed as `ssh -i`.
    Key,
}

impl AuthType {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthType::Agent => "agent",
            AuthType::Key => "key",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent" => Some(AuthType::Agent),
            "key" => Some(AuthType::Key),
            _ => None,
        }
    }
}

/// A place where Docker can be driven: the local machine or an SSH host.
///
/// Note: SSH private key *contents* and passwords are never stored here
/// (PRD §11.1) — only an on-disk key path or a reference to `ssh-agent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub kind: TargetKind,
    pub display_name: String,
    /// Hostname, IP or Tailscale MagicDNS name. `None` for local.
    pub host: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_username: Option<String>,
    pub auth_type: Option<AuthType>,
    pub identity_path: Option<String>,
    /// Docker executable on the target, default `docker`.
    pub docker_command: String,
    /// Approved host key fingerprint (`SHA256:…`). Required before an SSH
    /// target may be used (TAR-005).
    pub host_key_fingerprint: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
}

impl Target {
    pub fn is_remote(&self) -> bool {
        self.kind == TargetKind::Ssh
    }

    /// `user@host` form used on the ssh command line.
    pub fn ssh_destination(&self) -> Option<String> {
        let host = self.host.as_ref()?;
        Some(match &self.ssh_username {
            Some(u) => format!("{u}@{host}"),
            None => host.clone(),
        })
    }

    /// Short location label for tables: `local` or `host · ssh`.
    pub fn location(&self) -> String {
        match self.kind {
            TargetKind::Local => "local machine".to_string(),
            TargetKind::Ssh => match (&self.host, self.ssh_port) {
                (Some(h), Some(p)) if p != 22 => format!("{h}:{p} · ssh"),
                (Some(h), _) => format!("{h} · ssh"),
                _ => "ssh".to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Engine — a shared service container
// ---------------------------------------------------------------------------

/// A service this app runs as one shared container per target and major
/// version, carving project-scoped resources out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    /// PostgreSQL: projects get a database plus a login role.
    Postgres,
    /// MinIO object storage: projects get a bucket plus a scoped access key.
    Minio,
}

impl EngineKind {
    pub const ALL: [EngineKind; 2] = [EngineKind::Postgres, EngineKind::Minio];

    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::Postgres => "postgres",
            EngineKind::Minio => "minio",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Some(EngineKind::Postgres),
            "minio" | "s3" | "storage" => Some(EngineKind::Minio),
            _ => None,
        }
    }

    /// What a project gets out of this engine.
    pub fn resource_kind(self) -> ResourceKind {
        match self {
            EngineKind::Postgres => ResourceKind::Database,
            EngineKind::Minio => ResourceKind::Bucket,
        }
    }

    /// Port the service listens on inside its container.
    pub fn container_port(self) -> u16 {
        match self {
            EngineKind::Postgres => 5432,
            EngineKind::Minio => 9000,
        }
    }

    /// Secondary container port, when the service has a second endpoint.
    /// MinIO's web console; PostgreSQL has none.
    pub fn console_container_port(self) -> Option<u16> {
        match self {
            EngineKind::Postgres => None,
            EngineKind::Minio => Some(9001),
        }
    }

    /// Version label used when the caller does not pin one.
    pub fn default_major_version(self) -> &'static str {
        match self {
            EngineKind::Postgres => "17",
            EngineKind::Minio => "latest",
        }
    }

    /// Image for a major version, before any registry prefix is applied.
    pub fn default_image(self, major_version: &str) -> String {
        match self {
            EngineKind::Postgres => format!("postgres:{major_version}"),
            EngineKind::Minio => format!("minio/minio:{major_version}"),
        }
    }

    /// Short token used in volume names: `pg17`, `minio-latest`.
    pub fn short_tag(self, major_version: &str) -> String {
        match self {
            EngineKind::Postgres => format!("pg{major_version}"),
            EngineKind::Minio => format!("minio-{major_version}"),
        }
    }
}

/// What a project-scoped resource is, for the tables that hold both kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Database,
    Bucket,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Database => "database",
            ResourceKind::Bucket => "bucket",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "database" => Some(ResourceKind::Database),
            "bucket" => Some(ResourceKind::Bucket),
            _ => None,
        }
    }
}

/// One shared service container on one target (PRD §8.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineInstance {
    pub id: String,
    pub target_id: String,
    pub engine: EngineKind,
    /// Major version only; minor updates reuse the same container/volume.
    pub major_version: String,
    /// Fully qualified image reference actually used, e.g. `postgres:17`.
    pub image: String,
    pub container_name: String,
    pub volume_name: String,
    /// Always a loopback address by default (ENG-008).
    pub bind_address: String,
    /// Published port for the service's primary endpoint.
    pub host_port: u16,
    /// Published port for the secondary endpoint, e.g. MinIO's console.
    pub console_port: Option<u16>,
    pub admin_user: String,
    /// Key into the secret store holding the admin password.
    pub credential_ref: String,
    /// `true` when this app created the container (ENG-005/006).
    pub managed: bool,
    pub created_at: DateTime<Utc>,
}

impl EngineInstance {
    pub fn label(&self) -> String {
        format!("{} {}", self.engine.as_str(), self.major_version)
    }

    /// Port a project resource is reached through on the target itself.
    pub fn remote_service_port(&self) -> u16 {
        self.engine.container_port()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Unhealthy,
    Starting,
    /// Container has no healthcheck configured.
    None,
}

/// Live container state, never persisted — always read from Docker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub exists: bool,
    pub running: bool,
    /// Raw docker state string (`running`, `exited`, …).
    pub state: String,
    pub health: Health,
    pub image: Option<String>,
    pub started_at: Option<String>,
}

impl EngineStatus {
    pub fn missing() -> Self {
        Self {
            exists: false,
            running: false,
            state: "missing".into(),
            health: Health::None,
            image: None,
            started_at: None,
        }
    }

    /// `●` running · `○` stopped · `!` error — symbol first so colour is never
    /// the only carrier of meaning (PRD §12.4).
    pub fn symbol(&self) -> &'static str {
        if !self.exists {
            "·"
        } else if self.health == Health::Unhealthy {
            "!"
        } else if self.running {
            "●"
        } else {
            "○"
        }
    }
}

/// A container on the target that this app did *not* create (MIG-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub ports: String,
    /// Engine guessed from the image name, when recognisable.
    pub guessed_engine: Option<String>,
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

/// One project's database plus its dedicated login role (PRD §8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedDatabase {
    pub id: String,
    pub engine_instance_id: String,
    pub project_name: String,
    pub database_name: String,
    pub username: String,
    pub credential_ref: String,
    /// Stable local port reserved for this database's tunnel (TUN-009).
    pub preferred_local_tunnel_port: Option<u16>,
    pub created_at: DateTime<Utc>,
    pub last_connection_test_at: Option<DateTime<Utc>>,
    pub last_backup_at: Option<DateTime<Utc>>,
}

/// Runtime statistics read from the engine, not persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseStats {
    pub size_bytes: Option<i64>,
    pub connections: Option<i64>,
}

/// Everything a list row or detail pane needs, assembled by `core`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseView {
    pub database: ManagedDatabase,
    pub engine: EngineInstance,
    pub target: Target,
    pub stats: DatabaseStats,
    pub tunnel: Option<TunnelSession>,
}

impl DatabaseView {
    /// Address a client application should connect to: the tunnel endpoint for
    /// remote targets, the engine's bound port for local ones.
    pub fn client_endpoint(&self) -> Option<(String, u16)> {
        if self.target.is_remote() {
            self.tunnel
                .as_ref()
                .filter(|t| t.status == TunnelStatus::Active)
                .map(|t| (t.local_host.clone(), t.local_port))
        } else {
            Some((self.engine.bind_address.clone(), self.engine.host_port))
        }
    }
}

/// Resolved connection parameters. `password` is `None` when the secret store
/// is in no-store mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password: Option<String>,
}

impl ConnectionInfo {
    /// `postgresql://user:pass@host:port/db` with percent-encoded credentials.
    pub fn url(&self) -> String {
        let pw = self.password.as_deref().unwrap_or("");
        format!(
            "postgresql://{}:{}@{}:{}/{}",
            crate::core::util::pct_encode(&self.username),
            crate::core::util::pct_encode(pw),
            self.host,
            self.port,
            self.database
        )
    }

    /// Same URL with the password replaced by `****` — the only form allowed in
    /// logs, activity records and non-focused UI (PRD §11.1).
    pub fn redacted_url(&self) -> String {
        format!(
            "postgresql://{}:****@{}:{}/{}",
            crate::core::util::pct_encode(&self.username),
            self.host,
            self.port,
            self.database
        )
    }

    /// `.env` block with both the URL and split variables (DB-006).
    pub fn env_block(&self) -> String {
        let pw = self.password.as_deref().unwrap_or("");
        format!(
            "DATABASE_URL={}\nPGHOST={}\nPGPORT={}\nPGDATABASE={}\nPGUSER={}\nPGPASSWORD={}\n",
            self.url(),
            self.host,
            self.port,
            self.database,
            self.username,
            pw
        )
    }
}

// ---------------------------------------------------------------------------
// Bucket — the object-storage counterpart of a database
// ---------------------------------------------------------------------------

/// One project's bucket plus its dedicated, bucket-scoped access key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedBucket {
    pub id: String,
    pub engine_instance_id: String,
    pub project_name: String,
    pub bucket_name: String,
    /// S3 access key id of the per-project user.
    pub access_key: String,
    /// Key into the secret store holding the secret access key.
    pub credential_ref: String,
    pub preferred_local_tunnel_port: Option<u16>,
    pub created_at: DateTime<Utc>,
    pub last_connection_test_at: Option<DateTime<Utc>>,
    pub last_backup_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketStats {
    pub objects: Option<u64>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketView {
    pub bucket: ManagedBucket,
    pub engine: EngineInstance,
    pub target: Target,
    pub stats: BucketStats,
    pub tunnel: Option<TunnelSession>,
}

impl BucketView {
    /// Where a client should point its S3 endpoint: the tunnel for remote
    /// targets, the engine's published port for local ones.
    pub fn client_endpoint(&self) -> Option<(String, u16)> {
        if self.target.is_remote() {
            self.tunnel
                .as_ref()
                .filter(|t| t.status == TunnelStatus::Active)
                .map(|t| (t.local_host.clone(), t.local_port))
        } else {
            Some((self.engine.bind_address.clone(), self.engine.host_port))
        }
    }
}

/// Resolved S3 connection parameters. `secret_key` is `None` in restricted
/// secret mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3ConnectionInfo {
    pub host: String,
    pub port: u16,
    pub bucket: String,
    pub access_key: String,
    #[serde(skip_serializing)]
    pub secret_key: Option<String>,
    pub region: String,
    /// MinIO is served over plain HTTP behind loopback or an SSH tunnel; TLS
    /// would add nothing a tunnel does not already provide (PRD §6.4).
    pub secure: bool,
}

impl S3ConnectionInfo {
    pub fn scheme(&self) -> &'static str {
        if self.secure {
            "https"
        } else {
            "http"
        }
    }

    /// `http://127.0.0.1:9000` — what every SDK calls the endpoint.
    pub fn endpoint(&self) -> String {
        format!("{}://{}:{}", self.scheme(), self.host, self.port)
    }

    /// Credential-bearing connection string, the S3 counterpart of a database
    /// URL. Understood by `mc alias import`, rclone and friends.
    pub fn url(&self) -> String {
        let secret = self.secret_key.as_deref().unwrap_or("");
        format!(
            "s3://{}:{}@{}:{}/{}",
            crate::core::util::pct_encode(&self.access_key),
            crate::core::util::pct_encode(secret),
            self.host,
            self.port,
            self.bucket
        )
    }

    pub fn redacted_url(&self) -> String {
        format!(
            "s3://{}:****@{}:{}/{}",
            crate::core::util::pct_encode(&self.access_key),
            self.host,
            self.port,
            self.bucket
        )
    }

    /// `.env` block in both the generic `S3_*` and the AWS SDK `AWS_*` shapes,
    /// so it drops into either kind of project unchanged.
    pub fn env_block(&self) -> String {
        let secret = self.secret_key.as_deref().unwrap_or("");
        let endpoint = self.endpoint();
        format!(
            "S3_ENDPOINT={endpoint}\n\
             S3_BUCKET={bucket}\n\
             S3_REGION={region}\n\
             S3_ACCESS_KEY_ID={access}\n\
             S3_SECRET_ACCESS_KEY={secret}\n\
             S3_FORCE_PATH_STYLE=true\n\
             AWS_ENDPOINT_URL_S3={endpoint}\n\
             AWS_REGION={region}\n\
             AWS_ACCESS_KEY_ID={access}\n\
             AWS_SECRET_ACCESS_KEY={secret}\n",
            bucket = self.bucket,
            region = self.region,
            access = self.access_key,
        )
    }
}

// ---------------------------------------------------------------------------
// Tunnel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelStatus {
    Active,
    Stopped,
    Failed,
}

impl TunnelStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TunnelStatus::Active => "active",
            TunnelStatus::Stopped => "stopped",
            TunnelStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(TunnelStatus::Active),
            "stopped" => Some(TunnelStatus::Stopped),
            "failed" => Some(TunnelStatus::Failed),
            _ => None,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            TunnelStatus::Active => "●",
            TunnelStatus::Stopped => "○",
            TunnelStatus::Failed => "!",
        }
    }
}

/// A detached `ssh -N -L` process owned by this app (PRD §6.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSession {
    pub id: String,
    /// The project resource this tunnel serves — a database or a bucket.
    pub resource_id: String,
    pub resource_kind: ResourceKind,
    pub local_host: String,
    pub local_port: u16,
    /// Address of the engine as seen *from the remote host* — always loopback.
    pub remote_host: String,
    pub remote_port: u16,
    pub pid: Option<i32>,
    pub pid_file_path: String,
    pub status: TunnelStatus,
    pub started_at: DateTime<Utc>,
    pub stopped_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupFormat {
    /// `pg_dump -Fc` — restorable with `pg_restore`, supports selective restore.
    Custom,
    /// Plain SQL text.
    Plain,
    /// Object-storage archive: a manifest line followed by the raw object
    /// bytes, produced and consumed with `mc`.
    Objects,
}

impl BackupFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            BackupFormat::Custom => "custom",
            BackupFormat::Plain => "plain",
            BackupFormat::Objects => "objects",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "custom" | "c" | "dump" => Some(BackupFormat::Custom),
            "plain" | "p" | "sql" => Some(BackupFormat::Plain),
            "objects" | "o" | "bucket" => Some(BackupFormat::Objects),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            BackupFormat::Custom => "dump",
            BackupFormat::Plain => "sql",
            BackupFormat::Objects => "objects",
        }
    }

    /// Which engine this format belongs to.
    pub fn resource_kind(self) -> ResourceKind {
        match self {
            BackupFormat::Custom | BackupFormat::Plain => ResourceKind::Database,
            BackupFormat::Objects => ResourceKind::Bucket,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupStatus {
    Running,
    Ok,
    Failed,
}

impl BackupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BackupStatus::Running => "running",
            BackupStatus::Ok => "ok",
            BackupStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(BackupStatus::Running),
            "ok" => Some(BackupStatus::Ok),
            "failed" => Some(BackupStatus::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRecord {
    pub id: String,
    /// Database id or bucket id, depending on `resource_kind`.
    pub resource_id: String,
    pub resource_kind: ResourceKind,
    /// Absolute directory the dump lives in, always local (decision §19.6).
    pub storage_location: String,
    pub file_name: String,
    pub format: BackupFormat,
    pub size: u64,
    /// SHA-256 of the dump file, used by BAK-009 verification.
    pub checksum: String,
    pub status: BackupStatus,
    pub created_at: DateTime<Utc>,
}

impl BackupRecord {
    pub fn path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.storage_location).join(&self.file_name)
    }
}

// ---------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Tui,
    Cli,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Tui => "tui",
            Origin::Cli => "cli",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tui" => Some(Origin::Tui),
            "cli" => Some(Origin::Cli),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Started,
    Ok,
    Failed,
    RolledBack,
}

impl ActivityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ActivityStatus::Started => "started",
            ActivityStatus::Ok => "ok",
            ActivityStatus::Failed => "failed",
            ActivityStatus::RolledBack => "rolled_back",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "started" => Some(ActivityStatus::Started),
            "ok" => Some(ActivityStatus::Ok),
            "failed" => Some(ActivityStatus::Failed),
            "rolled_back" => Some(ActivityStatus::RolledBack),
            _ => None,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            ActivityStatus::Started => "…",
            ActivityStatus::Ok => "✓",
            ActivityStatus::Failed => "!",
            ActivityStatus::RolledBack => "↩",
        }
    }
}

/// One audited operation. `redacted_summary` and `steps` never contain secrets
/// (PRD §7.8, §11.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRecord {
    pub id: String,
    pub target_id: Option<String>,
    pub resource_type: String,
    pub resource_id: Option<String>,
    pub action: String,
    pub origin: Origin,
    pub status: ActivityStatus,
    pub redacted_summary: String,
    /// Step-by-step trace, so partial failures show what ran (PRD §10).
    pub steps: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn conn() -> ConnectionInfo {
        ConnectionInfo {
            host: "127.0.0.1".into(),
            port: 15432,
            database: "letsbid_dev".into(),
            username: "letsbid_user".into(),
            password: Some("p@ss w/ord".into()),
        }
    }

    #[test]
    fn url_percent_encodes_credentials() {
        assert_eq!(
            conn().url(),
            "postgresql://letsbid_user:p%40ss%20w%2Ford@127.0.0.1:15432/letsbid_dev"
        );
    }

    #[test]
    fn redacted_url_never_contains_the_password() {
        let c = conn();
        let r = c.redacted_url();
        assert!(!r.contains("p%40ss"));
        assert!(r.contains("****"));
    }

    #[test]
    fn env_block_exposes_url_and_split_variables() {
        let block = conn().env_block();
        assert!(block.contains("DATABASE_URL=postgresql://"));
        assert!(block.contains("PGDATABASE=letsbid_dev"));
        assert!(block.contains("PGPASSWORD=p@ss w/ord"));
    }

    #[test]
    fn remote_view_has_no_endpoint_until_the_tunnel_is_active() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let target = Target {
            id: "t".into(),
            kind: TargetKind::Ssh,
            display_name: "dev-vps".into(),
            host: Some("vps.ts.net".into()),
            ssh_port: Some(22),
            ssh_username: Some("dev".into()),
            auth_type: Some(AuthType::Agent),
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: Some("SHA256:x".into()),
            created_at: now,
            last_connected_at: None,
        };
        let engine = EngineInstance {
            id: "e".into(),
            target_id: "t".into(),
            engine: EngineKind::Postgres,
            major_version: "17".into(),
            image: "postgres:17".into(),
            container_name: "linf-postgres-17".into(),
            volume_name: "linf-pg17-data".into(),
            bind_address: "127.0.0.1".into(),
            host_port: 5432,
            console_port: None,
            admin_user: "linf_admin".into(),
            credential_ref: "engine:e".into(),
            managed: true,
            created_at: now,
        };
        let database = ManagedDatabase {
            id: "d".into(),
            engine_instance_id: "e".into(),
            project_name: "Parantica".into(),
            database_name: "parantica_dev".into(),
            username: "parantica_user".into(),
            credential_ref: "database:d".into(),
            preferred_local_tunnel_port: None,
            created_at: now,
            last_connection_test_at: None,
            last_backup_at: None,
        };
        let mut view = DatabaseView {
            database,
            engine,
            target,
            stats: DatabaseStats::default(),
            tunnel: None,
        };
        assert_eq!(view.client_endpoint(), None);

        view.tunnel = Some(TunnelSession {
            id: "s".into(),
            resource_id: "d".into(),
            resource_kind: ResourceKind::Database,
            local_host: "127.0.0.1".into(),
            local_port: 15432,
            remote_host: "127.0.0.1".into(),
            remote_port: 5432,
            pid: Some(1),
            pid_file_path: "/tmp/x.pid".into(),
            status: TunnelStatus::Stopped,
            started_at: now,
            stopped_at: None,
        });
        assert_eq!(view.client_endpoint(), None, "stopped tunnel is not usable");

        view.tunnel.as_mut().unwrap().status = TunnelStatus::Active;
        assert_eq!(
            view.client_endpoint(),
            Some(("127.0.0.1".to_string(), 15432))
        );
    }
}
