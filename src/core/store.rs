//! SQLite-backed metadata store (PRD §13).
//!
//! The store is the app's memory, never its source of truth: Docker and the
//! running tunnels are reconciled against it at startup (§12.3).

use crate::core::config::harden_file;
use crate::core::error::{Error, Result};
use crate::core::model::*;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        harden_file(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        f(&guard)
    }

    fn migrate(&self) -> Result<()> {
        self.with(|c| {
            c.execute_batch(
                r#"
CREATE TABLE IF NOT EXISTS targets (
    id                   TEXT PRIMARY KEY,
    kind                 TEXT NOT NULL,
    display_name         TEXT NOT NULL UNIQUE,
    host                 TEXT,
    ssh_port             INTEGER,
    ssh_username         TEXT,
    auth_type            TEXT,
    identity_path        TEXT,
    docker_command       TEXT NOT NULL DEFAULT 'docker',
    host_key_fingerprint TEXT,
    created_at           TEXT NOT NULL,
    last_connected_at    TEXT
);

CREATE TABLE IF NOT EXISTS engines (
    id             TEXT PRIMARY KEY,
    target_id      TEXT NOT NULL REFERENCES targets(id) ON DELETE CASCADE,
    engine         TEXT NOT NULL,
    major_version  TEXT NOT NULL,
    image          TEXT NOT NULL,
    container_name TEXT NOT NULL,
    volume_name    TEXT NOT NULL,
    bind_address   TEXT NOT NULL,
    host_port      INTEGER NOT NULL,
    console_port   INTEGER,
    admin_user     TEXT NOT NULL,
    credential_ref TEXT NOT NULL,
    managed        INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    UNIQUE (target_id, engine, major_version)
);

CREATE TABLE IF NOT EXISTS databases (
    id                           TEXT PRIMARY KEY,
    engine_instance_id           TEXT NOT NULL REFERENCES engines(id) ON DELETE CASCADE,
    project_name                 TEXT NOT NULL,
    database_name                TEXT NOT NULL,
    username                     TEXT NOT NULL,
    credential_ref               TEXT NOT NULL,
    preferred_local_tunnel_port  INTEGER,
    created_at                   TEXT NOT NULL,
    last_connection_test_at      TEXT,
    last_backup_at               TEXT,
    UNIQUE (engine_instance_id, database_name),
    UNIQUE (engine_instance_id, username)
);

CREATE TABLE IF NOT EXISTS buckets (
    id                           TEXT PRIMARY KEY,
    engine_instance_id           TEXT NOT NULL REFERENCES engines(id) ON DELETE CASCADE,
    project_name                 TEXT NOT NULL,
    bucket_name                  TEXT NOT NULL,
    access_key                   TEXT NOT NULL,
    credential_ref               TEXT NOT NULL,
    preferred_local_tunnel_port  INTEGER,
    created_at                   TEXT NOT NULL,
    last_connection_test_at      TEXT,
    last_backup_at               TEXT,
    UNIQUE (engine_instance_id, bucket_name),
    UNIQUE (engine_instance_id, access_key)
);

-- Tunnels and backups belong to a project resource, which is a database on a
-- PostgreSQL engine and a bucket on a MinIO engine. The two resource tables
-- have separate primary keys, so referential integrity is enforced by the
-- deleting use case rather than by a foreign key.
CREATE TABLE IF NOT EXISTS tunnels (
    id            TEXT PRIMARY KEY,
    resource_id   TEXT NOT NULL,
    resource_kind TEXT NOT NULL,
    local_host    TEXT NOT NULL,
    local_port    INTEGER NOT NULL,
    remote_host   TEXT NOT NULL,
    remote_port   INTEGER NOT NULL,
    pid           INTEGER,
    pid_file_path TEXT NOT NULL,
    status        TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    stopped_at    TEXT
);
CREATE INDEX IF NOT EXISTS tunnels_by_resource ON tunnels(resource_id);

CREATE TABLE IF NOT EXISTS backups (
    id               TEXT PRIMARY KEY,
    resource_id      TEXT NOT NULL,
    resource_kind    TEXT NOT NULL,
    storage_location TEXT NOT NULL,
    file_name        TEXT NOT NULL,
    format           TEXT NOT NULL,
    size             INTEGER NOT NULL,
    checksum         TEXT NOT NULL,
    status           TEXT NOT NULL,
    created_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS backups_by_resource ON backups(resource_id, created_at DESC);

CREATE TABLE IF NOT EXISTS activity (
    id               TEXT PRIMARY KEY,
    target_id        TEXT,
    resource_type    TEXT NOT NULL,
    resource_id      TEXT,
    action           TEXT NOT NULL,
    origin           TEXT NOT NULL,
    status           TEXT NOT NULL,
    redacted_summary TEXT NOT NULL,
    steps            TEXT NOT NULL DEFAULT '[]',
    started_at       TEXT NOT NULL,
    completed_at     TEXT
);
CREATE INDEX IF NOT EXISTS activity_recent ON activity(started_at DESC);
"#,
            )?;
            Ok(())
        })
    }

    // -- targets ------------------------------------------------------------

    pub fn insert_target(&self, t: &Target) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO targets (id, kind, display_name, host, ssh_port, ssh_username,
                                      auth_type, identity_path, docker_command,
                                      host_key_fingerprint, created_at, last_connected_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    t.id,
                    t.kind.as_str(),
                    t.display_name,
                    t.host,
                    t.ssh_port,
                    t.ssh_username,
                    t.auth_type.map(|a| a.as_str()),
                    t.identity_path,
                    t.docker_command,
                    t.host_key_fingerprint,
                    ts(&t.created_at),
                    t.last_connected_at.as_ref().map(ts),
                ],
            )
            .map_err(|e| {
                unique_conflict(
                    e,
                    &format!("Target 이름 `{}`은(는) 이미 사용 중입니다.", t.display_name),
                )
            })?;
            Ok(())
        })
    }

    pub fn update_target(&self, t: &Target) -> Result<()> {
        self.with(|c| {
            let n = c
                .execute(
                    "UPDATE targets SET kind=?2, display_name=?3, host=?4, ssh_port=?5,
                        ssh_username=?6, auth_type=?7, identity_path=?8, docker_command=?9,
                        host_key_fingerprint=?10, last_connected_at=?11
                 WHERE id=?1",
                    params![
                        t.id,
                        t.kind.as_str(),
                        t.display_name,
                        t.host,
                        t.ssh_port,
                        t.ssh_username,
                        t.auth_type.map(|a| a.as_str()),
                        t.identity_path,
                        t.docker_command,
                        t.host_key_fingerprint,
                        t.last_connected_at.as_ref().map(ts),
                    ],
                )
                .map_err(|e| {
                    unique_conflict(
                        e,
                        &format!("Target 이름 `{}`은(는) 이미 사용 중입니다.", t.display_name),
                    )
                })?;
            if n == 0 {
                return Err(Error::NotFound(format!(
                    "Target `{}`을(를) 찾을 수 없습니다.",
                    t.id
                )));
            }
            Ok(())
        })
    }

    pub fn delete_target(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM targets WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn list_targets(&self) -> Result<Vec<Target>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT * FROM targets ORDER BY kind, display_name")?;
            let rows = stmt.query_map([], row_target)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Resolve by id first, then by display name — every CLI subcommand accepts
    /// either form.
    pub fn find_target(&self, key: &str) -> Result<Option<Target>> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM targets WHERE id=?1 OR display_name=?1 LIMIT 1")?;
            Ok(stmt.query_row(params![key], row_target).optional()?)
        })
    }

    pub fn require_target(&self, key: &str) -> Result<Target> {
        self.find_target(key)?
            .ok_or_else(|| Error::NotFound(format!("Target `{key}`을(를) 찾을 수 없습니다.")))
    }

    // -- engines ------------------------------------------------------------

    pub fn insert_engine(&self, e: &EngineInstance) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO engines (id, target_id, engine, major_version, image, container_name,
                                      volume_name, bind_address, host_port, console_port,
                                      admin_user, credential_ref, managed, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    e.id,
                    e.target_id,
                    e.engine.as_str(),
                    e.major_version,
                    e.image,
                    e.container_name,
                    e.volume_name,
                    e.bind_address,
                    e.host_port,
                    e.console_port,
                    e.admin_user,
                    e.credential_ref,
                    e.managed as i32,
                    ts(&e.created_at),
                ],
            )
            .map_err(|err| {
                unique_conflict(
                    err,
                    &format!(
                        "해당 Target에는 이미 {} {} 엔진이 등록되어 있습니다.",
                        e.engine.as_str(),
                        e.major_version
                    ),
                )
            })?;
            Ok(())
        })
    }

    pub fn delete_engine(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM engines WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn list_engines(&self) -> Result<Vec<EngineInstance>> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM engines ORDER BY target_id, engine, major_version")?;
            let rows = stmt.query_map([], row_engine)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn list_engines_for_target(&self, target_id: &str) -> Result<Vec<EngineInstance>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM engines WHERE target_id=?1 ORDER BY engine, major_version",
            )?;
            let rows = stmt.query_map(params![target_id], row_engine)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn find_engine(
        &self,
        target_id: &str,
        engine: EngineKind,
        major_version: &str,
    ) -> Result<Option<EngineInstance>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM engines WHERE target_id=?1 AND engine=?2 AND major_version=?3",
            )?;
            Ok(stmt
                .query_row(
                    params![target_id, engine.as_str(), major_version],
                    row_engine,
                )
                .optional()?)
        })
    }

    pub fn get_engine(&self, id: &str) -> Result<EngineInstance> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT * FROM engines WHERE id=?1")?;
            stmt.query_row(params![id], row_engine)
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("엔진 `{id}`을(를) 찾을 수 없습니다.")))
        })
    }

    // -- databases ----------------------------------------------------------

    pub fn insert_database(&self, d: &ManagedDatabase) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO databases (id, engine_instance_id, project_name, database_name,
                                        username, credential_ref, preferred_local_tunnel_port,
                                        created_at, last_connection_test_at, last_backup_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    d.id,
                    d.engine_instance_id,
                    d.project_name,
                    d.database_name,
                    d.username,
                    d.credential_ref,
                    d.preferred_local_tunnel_port,
                    ts(&d.created_at),
                    d.last_connection_test_at.as_ref().map(ts),
                    d.last_backup_at.as_ref().map(ts),
                ],
            )
            .map_err(|e| {
                unique_conflict(
                    e,
                    &format!(
                        "이 엔진에는 이미 `{}` DB 또는 `{}` 계정이 있습니다.",
                        d.database_name, d.username
                    ),
                )
            })?;
            Ok(())
        })
    }

    pub fn update_database(&self, d: &ManagedDatabase) -> Result<()> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE databases SET project_name=?2, database_name=?3, username=?4,
                        credential_ref=?5, preferred_local_tunnel_port=?6,
                        last_connection_test_at=?7, last_backup_at=?8
                 WHERE id=?1",
                params![
                    d.id,
                    d.project_name,
                    d.database_name,
                    d.username,
                    d.credential_ref,
                    d.preferred_local_tunnel_port,
                    d.last_connection_test_at.as_ref().map(ts),
                    d.last_backup_at.as_ref().map(ts),
                ],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!(
                    "DB `{}`을(를) 찾을 수 없습니다.",
                    d.id
                )));
            }
            Ok(())
        })
    }

    pub fn delete_database(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM databases WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn list_databases(&self) -> Result<Vec<ManagedDatabase>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT * FROM databases ORDER BY database_name")?;
            let rows = stmt.query_map([], row_database)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn list_databases_for_engine(&self, engine_id: &str) -> Result<Vec<ManagedDatabase>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM databases WHERE engine_instance_id=?1 ORDER BY database_name",
            )?;
            let rows = stmt.query_map(params![engine_id], row_database)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// By id, or by database name when unambiguous. A name that exists on more
    /// than one target is a `Conflict`, never a silent pick.
    pub fn find_database(&self, key: &str) -> Result<Option<ManagedDatabase>> {
        self.with(|c| {
            let mut by_id = c.prepare("SELECT * FROM databases WHERE id=?1")?;
            if let Some(d) = by_id.query_row(params![key], row_database).optional()? {
                return Ok(Some(d));
            }
            let mut stmt = c.prepare("SELECT * FROM databases WHERE database_name=?1")?;
            let matches = stmt
                .query_map(params![key], row_database)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match matches.len() {
                0 => Ok(None),
                1 => Ok(matches.into_iter().next()),
                _ => Err(Error::Conflict(format!(
                    "`{key}` DB가 여러 Target에 있습니다. `--target`으로 대상을 지정하세요."
                ))),
            }
        })
    }

    pub fn find_database_on_engine(
        &self,
        engine_id: &str,
        name: &str,
    ) -> Result<Option<ManagedDatabase>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM databases WHERE engine_instance_id=?1 AND database_name=?2",
            )?;
            Ok(stmt
                .query_row(params![engine_id, name], row_database)
                .optional()?)
        })
    }

    pub fn require_database(&self, key: &str) -> Result<ManagedDatabase> {
        self.find_database(key)?
            .ok_or_else(|| Error::NotFound(format!("DB `{key}`을(를) 찾을 수 없습니다.")))
    }

    /// Ports already reserved by another project resource, so a new
    /// reservation cannot collide inside the app itself (TUN-009).
    pub fn reserved_tunnel_ports(&self) -> Result<Vec<u16>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT preferred_local_tunnel_port FROM databases
                 WHERE preferred_local_tunnel_port IS NOT NULL
                 UNION
                 SELECT preferred_local_tunnel_port FROM buckets
                 WHERE preferred_local_tunnel_port IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, u16>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // -- buckets ------------------------------------------------------------

    pub fn insert_bucket(&self, b: &ManagedBucket) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO buckets (id, engine_instance_id, project_name, bucket_name,
                                      access_key, credential_ref, preferred_local_tunnel_port,
                                      created_at, last_connection_test_at, last_backup_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    b.id,
                    b.engine_instance_id,
                    b.project_name,
                    b.bucket_name,
                    b.access_key,
                    b.credential_ref,
                    b.preferred_local_tunnel_port,
                    ts(&b.created_at),
                    b.last_connection_test_at.as_ref().map(ts),
                    b.last_backup_at.as_ref().map(ts),
                ],
            )
            .map_err(|e| {
                unique_conflict(
                    e,
                    &format!(
                        "이 엔진에는 이미 `{}` 버킷 또는 `{}` 액세스 키가 있습니다.",
                        b.bucket_name, b.access_key
                    ),
                )
            })?;
            Ok(())
        })
    }

    pub fn update_bucket(&self, b: &ManagedBucket) -> Result<()> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE buckets SET project_name=?2, bucket_name=?3, access_key=?4,
                        credential_ref=?5, preferred_local_tunnel_port=?6,
                        last_connection_test_at=?7, last_backup_at=?8
                 WHERE id=?1",
                params![
                    b.id,
                    b.project_name,
                    b.bucket_name,
                    b.access_key,
                    b.credential_ref,
                    b.preferred_local_tunnel_port,
                    b.last_connection_test_at.as_ref().map(ts),
                    b.last_backup_at.as_ref().map(ts),
                ],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!(
                    "버킷 `{}`을(를) 찾을 수 없습니다.",
                    b.id
                )));
            }
            Ok(())
        })
    }

    pub fn delete_bucket(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM buckets WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn list_buckets(&self) -> Result<Vec<ManagedBucket>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT * FROM buckets ORDER BY bucket_name")?;
            let rows = stmt.query_map([], row_bucket)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn list_buckets_for_engine(&self, engine_id: &str) -> Result<Vec<ManagedBucket>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM buckets WHERE engine_instance_id=?1 ORDER BY bucket_name",
            )?;
            let rows = stmt.query_map(params![engine_id], row_bucket)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// By id, or by bucket name when unambiguous.
    pub fn find_bucket(&self, key: &str) -> Result<Option<ManagedBucket>> {
        self.with(|c| {
            let mut by_id = c.prepare("SELECT * FROM buckets WHERE id=?1")?;
            if let Some(b) = by_id.query_row(params![key], row_bucket).optional()? {
                return Ok(Some(b));
            }
            let mut stmt = c.prepare("SELECT * FROM buckets WHERE bucket_name=?1")?;
            let matches = stmt
                .query_map(params![key], row_bucket)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match matches.len() {
                0 => Ok(None),
                1 => Ok(matches.into_iter().next()),
                _ => Err(Error::Conflict(format!(
                    "`{key}` 버킷이 여러 Target에 있습니다. `--target`으로 대상을 지정하세요."
                ))),
            }
        })
    }

    pub fn find_bucket_on_engine(
        &self,
        engine_id: &str,
        name: &str,
    ) -> Result<Option<ManagedBucket>> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM buckets WHERE engine_instance_id=?1 AND bucket_name=?2")?;
            Ok(stmt
                .query_row(params![engine_id, name], row_bucket)
                .optional()?)
        })
    }

    pub fn require_bucket(&self, key: &str) -> Result<ManagedBucket> {
        self.find_bucket(key)?
            .ok_or_else(|| Error::NotFound(format!("버킷 `{key}`을(를) 찾을 수 없습니다.")))
    }

    // -- tunnels ------------------------------------------------------------

    pub fn upsert_tunnel(&self, t: &TunnelSession) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO tunnels (id, resource_id, resource_kind, local_host, local_port,
                                      remote_host, remote_port, pid, pid_file_path, status,
                                      started_at, stopped_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET
                    local_host=excluded.local_host, local_port=excluded.local_port,
                    remote_host=excluded.remote_host, remote_port=excluded.remote_port,
                    pid=excluded.pid, pid_file_path=excluded.pid_file_path,
                    status=excluded.status, started_at=excluded.started_at,
                    stopped_at=excluded.stopped_at",
                params![
                    t.id,
                    t.resource_id,
                    t.resource_kind.as_str(),
                    t.local_host,
                    t.local_port,
                    t.remote_host,
                    t.remote_port,
                    t.pid,
                    t.pid_file_path,
                    t.status.as_str(),
                    ts(&t.started_at),
                    t.stopped_at.as_ref().map(ts),
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_tunnels(&self) -> Result<Vec<TunnelSession>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT * FROM tunnels ORDER BY started_at DESC")?;
            let rows = stmt.query_map([], row_tunnel)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Most recent session for a project resource, whatever its status.
    pub fn latest_tunnel(&self, resource_id: &str) -> Result<Option<TunnelSession>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM tunnels WHERE resource_id=?1 ORDER BY started_at DESC LIMIT 1",
            )?;
            Ok(stmt
                .query_row(params![resource_id], row_tunnel)
                .optional()?)
        })
    }

    pub fn active_tunnels(&self) -> Result<Vec<TunnelSession>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT * FROM tunnels WHERE status='active'")?;
            let rows = stmt.query_map([], row_tunnel)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn delete_tunnel(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM tunnels WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn delete_tunnels_for_resource(&self, resource_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM tunnels WHERE resource_id=?1",
                params![resource_id],
            )?;
            Ok(())
        })
    }

    // -- backups ------------------------------------------------------------

    pub fn insert_backup(&self, b: &BackupRecord) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO backups (id, resource_id, resource_kind, storage_location, file_name,
                                      format, size, checksum, status, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(id) DO UPDATE SET
                    size=excluded.size, checksum=excluded.checksum, status=excluded.status",
                params![
                    b.id,
                    b.resource_id,
                    b.resource_kind.as_str(),
                    b.storage_location,
                    b.file_name,
                    b.format.as_str(),
                    b.size,
                    b.checksum,
                    b.status.as_str(),
                    ts(&b.created_at),
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_backups(&self, resource_id: Option<&str>) -> Result<Vec<BackupRecord>> {
        self.with(|c| match resource_id {
            Some(id) => {
                let mut stmt = c.prepare(
                    "SELECT * FROM backups WHERE resource_id=?1 ORDER BY created_at DESC",
                )?;
                let rows = stmt.query_map(params![id], row_backup)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
            None => {
                let mut stmt = c.prepare("SELECT * FROM backups ORDER BY created_at DESC")?;
                let rows = stmt.query_map([], row_backup)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
        })
    }

    pub fn find_backup(&self, id: &str) -> Result<Option<BackupRecord>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT * FROM backups WHERE id=?1")?;
            Ok(stmt.query_row(params![id], row_backup).optional()?)
        })
    }

    pub fn delete_backups_for_resource(&self, resource_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM backups WHERE resource_id=?1",
                params![resource_id],
            )?;
            Ok(())
        })
    }

    // -- activity -----------------------------------------------------------

    pub fn upsert_activity(&self, a: &ActivityRecord) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO activity (id, target_id, resource_type, resource_id, action, origin,
                                       status, redacted_summary, steps, started_at, completed_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(id) DO UPDATE SET
                    status=excluded.status, redacted_summary=excluded.redacted_summary,
                    steps=excluded.steps, completed_at=excluded.completed_at",
                params![
                    a.id,
                    a.target_id,
                    a.resource_type,
                    a.resource_id,
                    a.action,
                    a.origin.as_str(),
                    a.status.as_str(),
                    a.redacted_summary,
                    serde_json::to_string(&a.steps)?,
                    ts(&a.started_at),
                    a.completed_at.as_ref().map(ts),
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_activity(&self, limit: usize) -> Result<Vec<ActivityRecord>> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM activity ORDER BY started_at DESC, rowid DESC LIMIT ?1")?;
            let rows = stmt.query_map(params![limit as i64], row_activity)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

fn ts(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339()
}

fn parse_ts(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn opt_ts(raw: Option<String>) -> Option<DateTime<Utc>> {
    raw.as_deref().map(parse_ts)
}

/// Turn a UNIQUE violation into a `Conflict` with a domain message instead of
/// leaking SQL text to the user.
fn unique_conflict(e: rusqlite::Error, message: &str) -> Error {
    let is_unique = matches!(
        &e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation
    );
    if is_unique {
        Error::Conflict(message.to_string())
    } else {
        e.into()
    }
}

fn row_target(r: &Row<'_>) -> rusqlite::Result<Target> {
    Ok(Target {
        id: r.get("id")?,
        kind: TargetKind::parse(&r.get::<_, String>("kind")?).unwrap_or(TargetKind::Local),
        display_name: r.get("display_name")?,
        host: r.get("host")?,
        ssh_port: r.get("ssh_port")?,
        ssh_username: r.get("ssh_username")?,
        auth_type: r
            .get::<_, Option<String>>("auth_type")?
            .and_then(|v| AuthType::parse(&v)),
        identity_path: r.get("identity_path")?,
        docker_command: r.get("docker_command")?,
        host_key_fingerprint: r.get("host_key_fingerprint")?,
        created_at: parse_ts(&r.get::<_, String>("created_at")?),
        last_connected_at: opt_ts(r.get("last_connected_at")?),
    })
}

fn row_engine(r: &Row<'_>) -> rusqlite::Result<EngineInstance> {
    Ok(EngineInstance {
        id: r.get("id")?,
        target_id: r.get("target_id")?,
        engine: EngineKind::parse(&r.get::<_, String>("engine")?).unwrap_or(EngineKind::Postgres),
        major_version: r.get("major_version")?,
        image: r.get("image")?,
        container_name: r.get("container_name")?,
        volume_name: r.get("volume_name")?,
        bind_address: r.get("bind_address")?,
        host_port: r.get("host_port")?,
        console_port: r.get("console_port")?,
        admin_user: r.get("admin_user")?,
        credential_ref: r.get("credential_ref")?,
        managed: r.get::<_, i32>("managed")? != 0,
        created_at: parse_ts(&r.get::<_, String>("created_at")?),
    })
}

fn row_database(r: &Row<'_>) -> rusqlite::Result<ManagedDatabase> {
    Ok(ManagedDatabase {
        id: r.get("id")?,
        engine_instance_id: r.get("engine_instance_id")?,
        project_name: r.get("project_name")?,
        database_name: r.get("database_name")?,
        username: r.get("username")?,
        credential_ref: r.get("credential_ref")?,
        preferred_local_tunnel_port: r.get("preferred_local_tunnel_port")?,
        created_at: parse_ts(&r.get::<_, String>("created_at")?),
        last_connection_test_at: opt_ts(r.get("last_connection_test_at")?),
        last_backup_at: opt_ts(r.get("last_backup_at")?),
    })
}

fn row_bucket(r: &Row<'_>) -> rusqlite::Result<ManagedBucket> {
    Ok(ManagedBucket {
        id: r.get("id")?,
        engine_instance_id: r.get("engine_instance_id")?,
        project_name: r.get("project_name")?,
        bucket_name: r.get("bucket_name")?,
        access_key: r.get("access_key")?,
        credential_ref: r.get("credential_ref")?,
        preferred_local_tunnel_port: r.get("preferred_local_tunnel_port")?,
        created_at: parse_ts(&r.get::<_, String>("created_at")?),
        last_connection_test_at: opt_ts(r.get("last_connection_test_at")?),
        last_backup_at: opt_ts(r.get("last_backup_at")?),
    })
}

fn row_tunnel(r: &Row<'_>) -> rusqlite::Result<TunnelSession> {
    Ok(TunnelSession {
        id: r.get("id")?,
        resource_id: r.get("resource_id")?,
        resource_kind: ResourceKind::parse(&r.get::<_, String>("resource_kind")?)
            .unwrap_or(ResourceKind::Database),
        local_host: r.get("local_host")?,
        local_port: r.get("local_port")?,
        remote_host: r.get("remote_host")?,
        remote_port: r.get("remote_port")?,
        pid: r.get("pid")?,
        pid_file_path: r.get("pid_file_path")?,
        status: TunnelStatus::parse(&r.get::<_, String>("status")?).unwrap_or(TunnelStatus::Failed),
        started_at: parse_ts(&r.get::<_, String>("started_at")?),
        stopped_at: opt_ts(r.get("stopped_at")?),
    })
}

fn row_backup(r: &Row<'_>) -> rusqlite::Result<BackupRecord> {
    Ok(BackupRecord {
        id: r.get("id")?,
        resource_id: r.get("resource_id")?,
        resource_kind: ResourceKind::parse(&r.get::<_, String>("resource_kind")?)
            .unwrap_or(ResourceKind::Database),
        storage_location: r.get("storage_location")?,
        file_name: r.get("file_name")?,
        format: BackupFormat::parse(&r.get::<_, String>("format")?).unwrap_or(BackupFormat::Custom),
        size: r.get::<_, i64>("size")?.max(0) as u64,
        checksum: r.get("checksum")?,
        status: BackupStatus::parse(&r.get::<_, String>("status")?).unwrap_or(BackupStatus::Failed),
        created_at: parse_ts(&r.get::<_, String>("created_at")?),
    })
}

fn row_activity(r: &Row<'_>) -> rusqlite::Result<ActivityRecord> {
    Ok(ActivityRecord {
        id: r.get("id")?,
        target_id: r.get("target_id")?,
        resource_type: r.get("resource_type")?,
        resource_id: r.get("resource_id")?,
        action: r.get("action")?,
        origin: Origin::parse(&r.get::<_, String>("origin")?).unwrap_or(Origin::Cli),
        status: ActivityStatus::parse(&r.get::<_, String>("status")?)
            .unwrap_or(ActivityStatus::Failed),
        redacted_summary: r.get("redacted_summary")?,
        steps: serde_json::from_str(&r.get::<_, String>("steps")?).unwrap_or_default(),
        started_at: parse_ts(&r.get::<_, String>("started_at")?),
        completed_at: opt_ts(r.get("completed_at")?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::util::{new_id, now};

    fn local_target(name: &str) -> Target {
        Target {
            id: new_id(),
            kind: TargetKind::Local,
            display_name: name.into(),
            host: None,
            ssh_port: None,
            ssh_username: None,
            auth_type: None,
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: None,
            created_at: now(),
            last_connected_at: None,
        }
    }

    fn engine(target_id: &str) -> EngineInstance {
        let id = new_id();
        EngineInstance {
            credential_ref: crate::core::secrets::engine_ref(&id),
            id,
            target_id: target_id.into(),
            engine: EngineKind::Postgres,
            major_version: "17".into(),
            image: "postgres:17".into(),
            container_name: "linf-postgres-17".into(),
            volume_name: "linf-pg17-data".into(),
            bind_address: "127.0.0.1".into(),
            host_port: 5432,
            console_port: None,
            admin_user: "linf_admin".into(),
            managed: true,
            created_at: now(),
        }
    }

    fn database(engine_id: &str, name: &str) -> ManagedDatabase {
        let id = new_id();
        ManagedDatabase {
            credential_ref: crate::core::secrets::database_ref(&id),
            id,
            engine_instance_id: engine_id.into(),
            project_name: name.into(),
            database_name: format!("{name}_dev"),
            username: format!("{name}_user"),
            preferred_local_tunnel_port: None,
            created_at: now(),
            last_connection_test_at: None,
            last_backup_at: None,
        }
    }

    #[test]
    fn targets_round_trip_and_resolve_by_id_or_name() {
        let s = Store::open_in_memory().unwrap();
        let t = local_target("local");
        s.insert_target(&t).unwrap();
        assert_eq!(s.find_target(&t.id).unwrap().unwrap(), t);
        assert_eq!(s.find_target("local").unwrap().unwrap(), t);
        assert!(s.find_target("nope").unwrap().is_none());
        assert!(s.require_target("nope").is_err());
    }

    #[test]
    fn duplicate_target_name_is_a_conflict_not_a_sql_error() {
        let s = Store::open_in_memory().unwrap();
        s.insert_target(&local_target("local")).unwrap();
        let err = s.insert_target(&local_target("local")).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(err, Error::Conflict(_)));
    }

    #[test]
    fn duplicate_database_name_on_one_engine_is_rejected() {
        let s = Store::open_in_memory().unwrap();
        let t = local_target("local");
        s.insert_target(&t).unwrap();
        let e = engine(&t.id);
        s.insert_engine(&e).unwrap();
        s.insert_database(&database(&e.id, "letsbid")).unwrap();
        let err = s.insert_database(&database(&e.id, "letsbid")).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    }

    #[test]
    fn ambiguous_database_name_across_targets_is_a_conflict() {
        let s = Store::open_in_memory().unwrap();
        let a = local_target("local");
        let b = local_target("dev-vps");
        s.insert_target(&a).unwrap();
        s.insert_target(&b).unwrap();
        let ea = engine(&a.id);
        let eb = engine(&b.id);
        s.insert_engine(&ea).unwrap();
        s.insert_engine(&eb).unwrap();
        s.insert_database(&database(&ea.id, "shared")).unwrap();
        s.insert_database(&database(&eb.id, "shared")).unwrap();
        assert!(matches!(
            s.find_database("shared_dev").unwrap_err(),
            Error::Conflict(_)
        ));
    }

    #[test]
    fn deleting_an_engine_cascades_to_its_databases_only() {
        let s = Store::open_in_memory().unwrap();
        let t = local_target("local");
        s.insert_target(&t).unwrap();
        let e17 = engine(&t.id);
        let mut e16 = engine(&t.id);
        e16.major_version = "16".into();
        e16.container_name = "linf-postgres-16".into();
        s.insert_engine(&e17).unwrap();
        s.insert_engine(&e16).unwrap();
        s.insert_database(&database(&e17.id, "a")).unwrap();
        s.insert_database(&database(&e16.id, "b")).unwrap();

        s.delete_engine(&e17.id).unwrap();
        let remaining = s.list_databases().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].database_name, "b_dev");
    }

    #[test]
    fn one_engine_per_target_and_major_version() {
        let s = Store::open_in_memory().unwrap();
        let t = local_target("local");
        s.insert_target(&t).unwrap();
        s.insert_engine(&engine(&t.id)).unwrap();
        assert!(matches!(
            s.insert_engine(&engine(&t.id)).unwrap_err(),
            Error::Conflict(_)
        ));
    }

    #[test]
    fn activity_steps_survive_the_json_round_trip() {
        let s = Store::open_in_memory().unwrap();
        let record = ActivityRecord {
            id: new_id(),
            target_id: None,
            resource_type: "database".into(),
            resource_id: None,
            action: "create".into(),
            origin: Origin::Cli,
            status: ActivityStatus::Started,
            redacted_summary: "postgresql://u:****@h:1/d".into(),
            steps: vec!["1. 이미지 확인".into(), "2. 컨테이너 생성".into()],
            started_at: now(),
            completed_at: None,
        };
        s.upsert_activity(&record).unwrap();
        let mut done = record.clone();
        done.status = ActivityStatus::Ok;
        done.steps.push("3. 접속 테스트".into());
        done.completed_at = Some(now());
        s.upsert_activity(&done).unwrap();

        let all = s.list_activity(10).unwrap();
        assert_eq!(all.len(), 1, "upsert must not duplicate");
        assert_eq!(all[0].steps.len(), 3);
        assert_eq!(all[0].status, ActivityStatus::Ok);
    }
}
