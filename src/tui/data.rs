//! The data every screen renders, and the one async call that collects it.
//!
//! Screens never touch `core` directly: they read a [`Snapshot`], which is
//! plain owned data. That is what lets the row builders in
//! [`crate::tui::rows`] be pure functions and be unit-tested without Docker.

use crate::core::model::{
    ActivityRecord, BackupRecord, BackupStatus, BucketView, DatabaseView, EngineInstance,
    ForeignContainer, ManagedBucket, ManagedDatabase, ResourceKind, Target, TunnelSession,
    TunnelStatus,
};
use crate::core::{
    backup, bucket, database, discovery, doctor, engine, target, tunnel, Ctx, Result,
};
use chrono::{DateTime, Utc};

/// One project-scoped resource, whichever service it was carved out of.
///
/// The resources screen lists databases and buckets in one table, so the rest
/// of the UI works against this instead of branching on two view types
/// everywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Database(DatabaseView),
    Bucket(BucketView),
}

impl Resource {
    pub fn kind(&self) -> ResourceKind {
        match self {
            Resource::Database(_) => ResourceKind::Database,
            Resource::Bucket(_) => ResourceKind::Bucket,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Resource::Database(v) => &v.database.id,
            Resource::Bucket(v) => &v.bucket.id,
        }
    }

    /// Database name or bucket name — what the user calls this thing.
    pub fn name(&self) -> &str {
        match self {
            Resource::Database(v) => &v.database.database_name,
            Resource::Bucket(v) => &v.bucket.bucket_name,
        }
    }

    pub fn project(&self) -> &str {
        match self {
            Resource::Database(v) => &v.database.project_name,
            Resource::Bucket(v) => &v.bucket.project_name,
        }
    }

    /// Login role or access key: the identity a client authenticates as.
    pub fn principal(&self) -> &str {
        match self {
            Resource::Database(v) => &v.database.username,
            Resource::Bucket(v) => &v.bucket.access_key,
        }
    }

    pub fn target(&self) -> &Target {
        match self {
            Resource::Database(v) => &v.target,
            Resource::Bucket(v) => &v.target,
        }
    }

    pub fn engine(&self) -> &EngineInstance {
        match self {
            Resource::Database(v) => &v.engine,
            Resource::Bucket(v) => &v.engine,
        }
    }

    pub fn tunnel(&self) -> Option<&TunnelSession> {
        match self {
            Resource::Database(v) => v.tunnel.as_ref(),
            Resource::Bucket(v) => v.tunnel.as_ref(),
        }
    }

    pub fn size_bytes(&self) -> Option<u64> {
        match self {
            Resource::Database(v) => v.stats.size_bytes.map(|b| b.max(0) as u64),
            Resource::Bucket(v) => v.stats.size_bytes,
        }
    }

    /// Live connections for a database, stored objects for a bucket.
    pub fn usage(&self) -> Option<u64> {
        match self {
            Resource::Database(v) => v.stats.connections.map(|c| c.max(0) as u64),
            Resource::Bucket(v) => v.stats.objects,
        }
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        match self {
            Resource::Database(v) => v.database.created_at,
            Resource::Bucket(v) => v.bucket.created_at,
        }
    }

    pub fn last_backup_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Resource::Database(v) => v.database.last_backup_at,
            Resource::Bucket(v) => v.bucket.last_backup_at,
        }
    }

    pub fn preferred_tunnel_port(&self) -> Option<u16> {
        match self {
            Resource::Database(v) => v.database.preferred_local_tunnel_port,
            Resource::Bucket(v) => v.bucket.preferred_local_tunnel_port,
        }
    }

    /// Endpoint a local client would use, when one exists.
    pub fn client_endpoint(&self) -> Option<(String, u16)> {
        match self {
            Resource::Database(v) => v.client_endpoint(),
            Resource::Bucket(v) => v.client_endpoint(),
        }
    }

    pub fn as_database(&self) -> Option<&DatabaseView> {
        match self {
            Resource::Database(v) => Some(v),
            Resource::Bucket(_) => None,
        }
    }

    pub fn as_bucket(&self) -> Option<&BucketView> {
        match self {
            Resource::Bucket(v) => Some(v),
            Resource::Database(_) => None,
        }
    }

    pub fn managed_database(&self) -> Option<&ManagedDatabase> {
        self.as_database().map(|v| &v.database)
    }

    pub fn managed_bucket(&self) -> Option<&ManagedBucket> {
        self.as_bucket().map(|v| &v.bucket)
    }

    /// Sort key: target first, then name, so the table groups by host the way
    /// the PRD mock does.
    pub(crate) fn order(&self) -> (String, String) {
        (
            self.target().display_name.clone(),
            self.name().to_ascii_lowercase(),
        )
    }
}

/// A non-fatal condition worth showing on the dashboard (PRD §7.3 ALERTS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub headline: String,
    pub subject: String,
    pub at: Option<DateTime<Utc>>,
}

impl Alert {
    pub fn new(headline: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            headline: headline.into(),
            subject: subject.into(),
            at: None,
        }
    }

    pub fn at(mut self, at: DateTime<Utc>) -> Self {
        self.at = Some(at);
        self
    }
}

/// Everything the seven screens read, collected in one pass.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub targets: Vec<target::TargetOverview>,
    pub engines: Vec<engine::EngineOverview>,
    pub resources: Vec<Resource>,
    pub tunnels: Vec<tunnel::TunnelView>,
    pub backups: Vec<BackupRecord>,
    pub activity: Vec<ActivityRecord>,
    pub checks: Vec<doctor::Check>,
    /// Containers on a target this app did not create (MIG-001), by target id.
    pub foreign: Vec<(String, ForeignContainer)>,
    pub at: Option<DateTime<Utc>>,
}

impl Snapshot {
    pub fn empty() -> Self {
        Self {
            targets: Vec::new(),
            engines: Vec::new(),
            resources: Vec::new(),
            tunnels: Vec::new(),
            backups: Vec::new(),
            activity: Vec::new(),
            checks: Vec::new(),
            foreign: Vec::new(),
            at: None,
        }
    }

    pub fn loaded(&self) -> bool {
        self.at.is_some()
    }

    pub fn docker_state(&self) -> &'static str {
        if self.targets.is_empty() {
            return match self.checks.iter().find(|c| c.name == "Docker CLI") {
                Some(c) if c.ok => "ok",
                Some(_) => "down",
                None => "none",
            };
        }
        if self.targets.iter().all(|t| t.reachable) {
            "ok"
        } else if self.targets.iter().any(|t| t.reachable) {
            "degraded"
        } else {
            "down"
        }
    }

    pub fn active_tunnels(&self) -> usize {
        self.tunnels
            .iter()
            .filter(|t| t.session.status == TunnelStatus::Active)
            .count()
    }

    pub fn engines_for_target(&self, target_id: &str) -> Vec<&engine::EngineOverview> {
        self.engines
            .iter()
            .filter(|e| e.engine.target_id == target_id)
            .collect()
    }

    /// Project resources living on one engine, counted per service.
    pub fn resource_count(&self, engine_id: &str) -> usize {
        self.resources
            .iter()
            .filter(|r| r.engine().id == engine_id)
            .count()
    }

    pub fn tunnel_count(&self, target_id: &str) -> usize {
        self.resources
            .iter()
            .filter(|r| {
                r.target().id == target_id
                    && r.tunnel().is_some_and(|t| t.status == TunnelStatus::Active)
            })
            .count()
    }

    pub fn resource_name(&self, resource_id: &str) -> Option<&str> {
        self.resources
            .iter()
            .find(|r| r.id() == resource_id)
            .map(|r| r.name())
    }

    pub fn find_resource(&self, resource_id: &str) -> Option<&Resource> {
        self.resources.iter().find(|r| r.id() == resource_id)
    }

    /// Failed backups, unreachable targets and broken tunnels, newest first.
    /// Pure, so the dashboard's alert block is testable.
    pub fn alerts(&self) -> Vec<Alert> {
        let mut alerts = Vec::new();
        for record in &self.backups {
            if record.status != BackupStatus::Failed {
                continue;
            }
            let subject = self
                .resource_name(&record.resource_id)
                .unwrap_or(&record.resource_id)
                .to_string();
            alerts.push(Alert::new("backup failed", subject).at(record.created_at));
        }
        for overview in &self.targets {
            if !overview.reachable {
                alerts.push(Alert::new(
                    "target unreachable",
                    format!("{} · {}", overview.target.display_name, overview.detail),
                ));
            }
        }
        for view in &self.tunnels {
            if view.session.status == TunnelStatus::Failed {
                alerts.push(
                    Alert::new("tunnel failed", view.resource_name.clone())
                        .at(view.session.started_at),
                );
            }
        }
        for overview in &self.engines {
            if !overview.status.exists {
                alerts.push(Alert::new(
                    "engine missing",
                    format!(
                        "{} · Docker에서 컨테이너가 없습니다",
                        overview.engine.container_name
                    ),
                ));
            }
        }

        for check in &self.checks {
            if !check.ok {
                alerts.push(Alert::new(
                    "doctor",
                    format!("{} · {}", check.name, check.detail),
                ));
            }
        }
        alerts
    }
}

/// Group by target, then by name — the order the PRD §7.6 mock shows. Shared
/// by [`load`] and the test fixture so both see the same table.
pub fn sort_resources(resources: &mut [Resource]) {
    resources.sort_by_key(Resource::order);
}

/// Collect everything, tolerating unreachable targets: a snapshot with holes
/// beats a screen with an error box (PRD §7.6).
///
/// `with_stats` is what makes this call expensive — the caller passes `false`
/// for the very first load so the first frame is fast (PRD §12.2).
pub async fn load(ctx: &Ctx, with_stats: bool) -> Result<Snapshot> {
    let _ = engine::reconcile(ctx).await?;
    let targets = target::overview(ctx).await?;
    let engines = engine::overview(ctx).await?;

    let mut resources: Vec<Resource> = Vec::new();
    for view in database::views(ctx, with_stats).await? {
        resources.push(Resource::Database(view));
    }
    for view in bucket::views(ctx, with_stats).await? {
        resources.push(Resource::Bucket(view));
    }
    sort_resources(&mut resources);

    let mut foreign = Vec::new();
    for overview in &targets {
        if !overview.reachable {
            continue;
        }
        if let Ok(found) = discovery::foreign_containers(ctx, &overview.target).await {
            for container in found {
                foreign.push((overview.target.id.clone(), container));
            }
        }
    }

    Ok(Snapshot {
        targets,
        engines,
        resources,
        tunnels: tunnel::status(ctx).await?,
        backups: backup::list(ctx, None)?,
        activity: ctx.store.list_activity(ACTIVITY_LIMIT)?,
        checks: doctor::run(ctx).await.unwrap_or_default(),
        foreign,
        at: Some(crate::core::util::now()),
    })
}

/// Enough history to scroll through a working day without paging.
const ACTIVITY_LIMIT: usize = 200;

#[cfg(test)]
pub(crate) mod fixture {
    //! Hand-built views, so the row builders and renderers can be tested
    //! without a store, Docker or a network.

    use super::*;
    use crate::core::model::{
        BucketStats, DatabaseStats, EngineKind, EngineStatus, Health, Origin, TargetKind,
    };
    use chrono::TimeZone;

    pub fn when(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, day, hour, minute, 0).unwrap()
    }

    pub fn local_target() -> Target {
        Target {
            id: "t-local".into(),
            kind: TargetKind::Local,
            display_name: "local".into(),
            host: None,
            ssh_port: None,
            ssh_username: None,
            auth_type: None,
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: None,
            created_at: when(1, 9, 0),
            last_connected_at: None,
        }
    }

    pub fn remote_target() -> Target {
        Target {
            id: "t-vps".into(),
            kind: TargetKind::Ssh,
            display_name: "dev-vps".into(),
            host: Some("vps.ts.net".into()),
            ssh_port: Some(22),
            ssh_username: Some("dev".into()),
            auth_type: None,
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: Some("SHA256:abc".into()),
            created_at: when(1, 9, 0),
            last_connected_at: Some(when(1, 20, 0)),
        }
    }

    pub fn engine_instance(target: &Target, kind: EngineKind, port: u16) -> EngineInstance {
        let major = kind.default_major_version().to_string();
        EngineInstance {
            id: format!("e-{}-{}", target.id, kind.as_str()),
            target_id: target.id.clone(),
            engine: kind,
            major_version: major.clone(),
            image: kind.default_image(&major),
            container_name: format!("linf-{}-{}", kind.as_str(), major),
            volume_name: format!("linf-{}-data", kind.short_tag(&major)),
            bind_address: "127.0.0.1".into(),
            host_port: port,
            console_port: kind.console_container_port().map(|_| 9001),
            admin_user: "linf_admin".into(),
            credential_ref: "engine:e1".into(),
            managed: true,
            created_at: when(1, 9, 0),
        }
    }

    pub fn running() -> EngineStatus {
        EngineStatus {
            exists: true,
            running: true,
            state: "running".into(),
            health: Health::Healthy,
            image: Some("postgres:17".into()),
            started_at: Some("2026-09-01T09:00:00Z".into()),
        }
    }

    pub fn database(target: &Target, name: &str, tunnel: Option<TunnelSession>) -> Resource {
        let engine = engine_instance(target, EngineKind::Postgres, 5432);
        Resource::Database(DatabaseView {
            database: ManagedDatabase {
                id: format!("db-{name}"),
                engine_instance_id: engine.id.clone(),
                project_name: name.trim_end_matches("_dev").to_string(),
                database_name: name.to_string(),
                username: format!("{}_user", name.trim_end_matches("_dev")),
                credential_ref: format!("database:db-{name}"),
                preferred_local_tunnel_port: Some(15432),
                created_at: when(1, 9, 0),
                last_connection_test_at: None,
                last_backup_at: Some(when(1, 3, 0)),
            },
            engine,
            target: target.clone(),
            stats: DatabaseStats {
                size_bytes: Some(88_080_384),
                connections: Some(2),
            },
            tunnel,
        })
    }

    pub fn bucket_resource(target: &Target, name: &str) -> Resource {
        let engine = engine_instance(target, EngineKind::Minio, 9000);
        Resource::Bucket(BucketView {
            bucket: ManagedBucket {
                id: format!("bk-{name}"),
                engine_instance_id: engine.id.clone(),
                project_name: name.trim_end_matches("-dev").to_string(),
                bucket_name: name.to_string(),
                access_key: "AKIALINF0000000EXAMPLE".into(),
                credential_ref: format!("bucket:bk-{name}"),
                preferred_local_tunnel_port: Some(19000),
                created_at: when(2, 11, 0),
                last_connection_test_at: None,
                last_backup_at: None,
            },
            engine,
            target: target.clone(),
            stats: BucketStats {
                objects: Some(142),
                size_bytes: Some(12_582_912),
            },
            tunnel: None,
        })
    }

    pub fn session(resource_id: &str, kind: ResourceKind, port: u16) -> TunnelSession {
        TunnelSession {
            id: format!("tn-{resource_id}"),
            resource_id: resource_id.to_string(),
            resource_kind: kind,
            local_host: "127.0.0.1".into(),
            local_port: port,
            remote_host: "127.0.0.1".into(),
            remote_port: 5432,
            pid: Some(48122),
            pid_file_path: "/tmp/tunnel.pid".into(),
            status: TunnelStatus::Active,
            started_at: when(1, 21, 4),
            stopped_at: None,
        }
    }

    pub fn activity(action: &str, status: crate::core::model::ActivityStatus) -> ActivityRecord {
        ActivityRecord {
            id: format!("ac-{action}"),
            target_id: Some("t-local".into()),
            resource_type: "database".into(),
            resource_id: Some("db-letsbid_dev".into()),
            action: action.to_string(),
            origin: Origin::Tui,
            status,
            redacted_summary: "`letsbid_dev` DB와 `letsbid_user` 계정을 local에 생성".into(),
            steps: vec!["엔진 확인".into(), "DB 생성".into(), "접속 테스트".into()],
            started_at: when(1, 21, 4),
            completed_at: Some(when(1, 21, 5)),
        }
    }

    pub fn backup_record(resource_id: &str, status: BackupStatus) -> BackupRecord {
        use crate::core::model::BackupFormat;
        BackupRecord {
            id: format!("bp-{resource_id}"),
            resource_id: resource_id.to_string(),
            resource_kind: ResourceKind::Database,
            storage_location: "/home/dev/backups".into(),
            file_name: "letsbid_dev-20260901-030000.dump".into(),
            format: BackupFormat::Custom,
            size: 4_194_304,
            checksum: "0".repeat(64),
            status,
            created_at: when(1, 3, 0),
        }
    }

    /// A populated snapshot roughly matching the PRD §7.3/§7.6 mock-ups.
    pub fn snapshot() -> Snapshot {
        let local = local_target();
        let vps = remote_target();
        let pg_local = engine_instance(&local, EngineKind::Postgres, 5432);
        let minio_local = engine_instance(&local, EngineKind::Minio, 9000);
        let pg_vps = engine_instance(&vps, EngineKind::Postgres, 5432);
        let tunnel = session("db-parantica_dev", ResourceKind::Database, 15432);

        let mut snapshot = Snapshot {
            targets: vec![
                target::TargetOverview {
                    target: local.clone(),
                    reachable: true,
                    docker: Some("27.1.1".into()),
                    detail: "connected".into(),
                },
                target::TargetOverview {
                    target: vps.clone(),
                    reachable: false,
                    docker: None,
                    detail: "ssh timeout".into(),
                },
            ],
            engines: vec![
                engine::EngineOverview {
                    engine: pg_local,
                    target: local.clone(),
                    status: running(),
                    database_count: 2,
                },
                engine::EngineOverview {
                    engine: minio_local,
                    target: local.clone(),
                    status: running(),
                    database_count: 1,
                },
                engine::EngineOverview {
                    engine: pg_vps,
                    target: vps.clone(),
                    status: EngineStatus::missing(),
                    database_count: 1,
                },
            ],
            resources: vec![
                database(&local, "letsbid_dev", None),
                bucket_resource(&local, "letsbid-dev-assets"),
                database(&vps, "parantica_dev", Some(tunnel.clone())),
            ],
            tunnels: vec![tunnel::TunnelView {
                session: tunnel,
                resource_name: "parantica_dev".into(),
                resource_kind: ResourceKind::Database,
                target_name: "dev-vps".into(),
            }],
            backups: vec![
                backup_record("db-letsbid_dev", BackupStatus::Ok),
                backup_record("db-parantica_dev", BackupStatus::Failed),
            ],
            activity: vec![
                activity("create", crate::core::model::ActivityStatus::Ok),
                activity("drop", crate::core::model::ActivityStatus::Failed),
            ],
            checks: vec![doctor::Check {
                name: "docker".into(),
                ok: true,
                detail: "docker 27.1.1".into(),
                remedy: None,
            }],
            foreign: Vec::new(),
            at: Some(when(1, 21, 4)),
        };
        // `load` sorts; the fixture must match or the tests would assert an
        // order the app never shows.
        super::sort_resources(&mut snapshot.resources);
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::fixture;
    use crate::core::model::TunnelStatus;

    #[test]
    fn a_partially_reachable_fleet_reports_degraded_docker() {
        let snap = fixture::snapshot();
        assert_eq!(snap.docker_state(), "degraded");
        assert_eq!(snap.active_tunnels(), 1);
    }

    #[test]
    fn alerts_name_the_failed_backup_by_resource_not_by_id() {
        let snap = fixture::snapshot();
        let alerts = snap.alerts();
        let backup = alerts
            .iter()
            .find(|a| a.headline == "backup failed")
            .expect("failed backup surfaces as an alert");
        assert_eq!(backup.subject, "parantica_dev");
        assert!(backup.at.is_some());
        assert!(alerts.iter().any(|a| a.headline == "target unreachable"));
    }

    #[test]
    fn resources_are_grouped_by_target_then_name() {
        let snap = fixture::snapshot();
        let names: Vec<&str> = snap.resources.iter().map(|r| r.name()).collect();
        assert_eq!(
            names,
            vec!["parantica_dev", "letsbid-dev-assets", "letsbid_dev"]
        );
    }

    #[test]
    fn buckets_report_objects_where_databases_report_connections() {
        let local = fixture::local_target();
        let db = fixture::database(&local, "letsbid_dev", None);
        let bucket = fixture::bucket_resource(&local, "letsbid-dev-assets");
        assert_eq!(db.usage(), Some(2));
        assert_eq!(bucket.usage(), Some(142));
        assert_eq!(bucket.size_bytes(), Some(12_582_912));
    }

    #[test]
    fn a_remote_resource_without_a_live_tunnel_has_no_client_endpoint() {
        let vps = fixture::remote_target();
        let without = fixture::database(&vps, "parantica_dev", None);
        assert_eq!(without.client_endpoint(), None);

        let mut live = fixture::session("db-parantica_dev", super::ResourceKind::Database, 15432);
        live.status = TunnelStatus::Active;
        let with = fixture::database(&vps, "parantica_dev", Some(live));
        assert_eq!(
            with.client_endpoint(),
            Some(("127.0.0.1".to_string(), 15432))
        );
    }
}
