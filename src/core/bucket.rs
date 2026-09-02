//! Project bucket use cases (PRD §8.4, §9.1, §9.2).
//!
//! One project owns exactly one bucket and one bucket-scoped access key on a
//! *shared* MinIO engine — the object-storage counterpart of
//! [`crate::core::database`]. Everything here is written so that a
//! half-finished creation leaves nothing behind: the bucket, the policy and the
//! user are rolled back and the attempt is recorded as `rolled_back` in the
//! activity log.
//!
//! Backup and restore live here rather than in [`crate::core::backup`] because
//! an object archive has nothing in common with a `pg_dump`: the MinIO image
//! ships no `tar`, so the archive is a manifest line followed by the raw object
//! bytes, streamed one object at a time through `mc cat` and `mc pipe`.

use crate::core::activity::Activity;
use crate::core::config::harden_file;
use crate::core::ctx::Ctx;
use crate::core::docker;
use crate::core::engine::{self, EngineSpec};
use crate::core::error::{Error, Result};
use crate::core::exec::Executor;
use crate::core::minio::{self, ObjectEntry};
use crate::core::model::{
    BackupFormat, BackupRecord, BackupStatus, BucketStats, BucketView, EngineInstance,
    ManagedBucket, ResourceKind, S3ConnectionInfo, Target,
};
use crate::core::plan::{Plan, PlanStep, StepKind};
use crate::core::progress::{Cancel, Reporter};
use crate::core::secrets;
use crate::core::util;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Length of a generated secret access key.
const SECRET_KEY_LEN: usize = 40;

/// MinIO refuses a secret key shorter than this.
const SECRET_KEY_MIN: usize = 8;

/// Seconds to wait for the engine to accept requests before giving up.
const READY_TIMEOUT_SECS: u64 = 60;

/// MinIO ignores the region for addressing but every SDK insists on one.
const DEFAULT_REGION: &str = "us-east-1";

/// Names MinIO keeps for itself.
const RESERVED_BUCKETS: &[&str] = &["minio"];

/// 63 (the DNS label limit) minus the `-dev` suffix.
const BUCKET_STEM_MAX: usize = 59;

const CHUNK: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// Everything the create form collects (PRD §7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSpec {
    pub project_name: String,
    pub bucket_name: String,
    /// `None` generates one, the way an S3 console would.
    pub access_key: Option<String>,
    /// `None` generates one; the CLI never accepts a secret as an argument
    /// (PRD §11.2).
    pub secret_key: Option<String>,
    pub region: String,
    pub preferred_local_tunnel_port: Option<u16>,
}

impl CreateSpec {
    pub fn for_project(project: &str) -> Self {
        Self {
            project_name: project.to_string(),
            bucket_name: suggest_bucket_name(project),
            access_key: None,
            secret_key: None,
            region: DEFAULT_REGION.to_string(),
            preferred_local_tunnel_port: None,
        }
    }
}

/// `Letsbid` → `letsbid-dev`. A bucket is a DNS label, so the identifier slug
/// is re-separated with `-`, and anything that cannot begin or end a label is
/// trimmed away. A name that slugifies to nothing still has to be legal, which
/// is what the bare `dev` fallback is for.
fn suggest_bucket_name(project: &str) -> String {
    let slug = util::slugify(project).replace('_', "-");
    let mut stem: String = slug
        .trim_matches('-')
        .chars()
        .take(BUCKET_STEM_MAX)
        .collect();
    while stem.ends_with('-') {
        stem.pop();
    }
    if stem.is_empty() {
        "dev".to_string()
    } else {
        format!("{stem}-dev")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub bucket: ManagedBucket,
    pub engine: EngineInstance,
    pub connection: S3ConnectionInfo,
}

/// DB-003/DB-004 input rules for object storage, applied identically by the
/// TUI form (live) and the CLI (at parse time).
pub fn validate_new_names(bucket: &str, access_key: &str) -> Result<()> {
    validate_bucket(bucket)?;
    validate_key(access_key)
}

fn validate_bucket(bucket: &str) -> Result<()> {
    minio::validate_bucket_name(bucket)?;
    if RESERVED_BUCKETS.contains(&bucket) {
        return Err(Error::Conflict(format!(
            "`{bucket}`은(는) MinIO가 이미 사용하는 이름입니다. 다른 이름을 사용하세요."
        )));
    }
    Ok(())
}

fn validate_key(access_key: &str) -> Result<()> {
    minio::validate_access_key(access_key)?;
    if access_key == engine::ADMIN_USER {
        return Err(Error::Conflict(format!(
            "`{access_key}`은(는) 엔진 관리자 계정 이름입니다. 다른 액세스 키를 사용하세요."
        )));
    }
    Ok(())
}

/// Whatever the spec has actually decided. A generated access key is checked
/// when it is generated, so its absence is not an error here.
fn validate_spec(s: &CreateSpec) -> Result<()> {
    validate_bucket(&s.bucket_name)?;
    match s.access_key.as_deref() {
        Some(key) => validate_key(key),
        None => Ok(()),
    }
}

/// The bucket-side steps appended to the engine plan. Kept pure so the preview
/// in PRD §7.5 can be asserted without touching Docker.
fn create_steps(s: &CreateSpec, remote: bool) -> Vec<PlanStep> {
    let key = match s.access_key.as_deref() {
        Some(key) => key.to_string(),
        None => "자동 생성".to_string(),
    };
    let mut steps = vec![
        PlanStep::new(
            StepKind::New,
            format!("버킷 {} 및 액세스 키 {} 생성", s.bucket_name, key),
        )
        .with_detail(format!(
            "정책 {} · 이 버킷에만 접근 허용 · 리전 {}",
            minio::policy_name(&s.bucket_name),
            s.region
        )),
        PlanStep::new(StepKind::Verify, "접속 테스트")
            .with_detail("컨테이너 안에서 프로젝트 액세스 키로 버킷을 조회합니다.".to_string()),
    ];
    if remote {
        steps.push(
            PlanStep::new(StepKind::Verify, "로컬 접속에는 SSH 터널이 필요합니다").with_detail(
                "생성 후 `linf tunnel start`로 터널을 시작하면 S3 엔드포인트가 활성화됩니다.",
            ),
        );
    }
    steps
}

/// Full preview: the engine plan (new or reused) followed by the bucket steps,
/// so what the user reads matches what [`create`] does end to end.
pub async fn plan_create(ctx: &Ctx, t: &Target, es: &EngineSpec, s: &CreateSpec) -> Result<Plan> {
    validate_spec(s)?;
    let mut plan = engine::plan_ensure(ctx, t, es).await?;
    plan.title = format!("`{}` 버킷 생성", s.bucket_name);
    for step in create_steps(s, t.is_remote()) {
        plan.push(step);
    }
    Ok(plan)
}

/// PRD §9.1/§9.2. Idempotent on the engine, strictly non-idempotent on the
/// bucket: a name that already exists anywhere is a conflict, never a silent
/// reuse (DB-004 analogue).
pub async fn create(
    ctx: &Ctx,
    t: &Target,
    es: &EngineSpec,
    s: &CreateSpec,
    r: &Reporter,
    c: &Cancel,
) -> Result<Created> {
    ctx.require_write_lock()?;
    validate_spec(s)?;
    let access_key = resolve_access_key(s.access_key.as_deref())?;
    let secret_key = resolve_secret_key(s.secret_key.as_deref())?;
    validate_new_names(&s.bucket_name, &access_key)?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "bucket",
        "create",
        format!(
            "`{}` 버킷과 액세스 키 `{}`을(를) {}에 생성",
            s.bucket_name, access_key, t.display_name
        ),
    )?
    .on_target(&t.id);

    let mut rollback: Option<String> = None;
    let result = create_inner(
        ctx,
        t,
        es,
        s,
        &access_key,
        &secret_key,
        r,
        c,
        &mut act,
        &mut rollback,
    )
    .await;
    match (&result, rollback) {
        (Err(_), Some(reason)) => act.rolled_back(reason),
        _ => act.finish(&result),
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn create_inner(
    ctx: &Ctx,
    t: &Target,
    es: &EngineSpec,
    s: &CreateSpec,
    access_key: &str,
    secret_key: &str,
    r: &Reporter,
    c: &Cancel,
    act: &mut Activity<'_>,
    rollback: &mut Option<String>,
) -> Result<Created> {
    c.check()?;
    let engine_instance = engine::ensure(ctx, t, es, r, c).await?;
    act.step(format!("엔진 {} 준비", engine_instance.container_name));

    let x = ctx.executor(t)?;
    docker::require_managed(&x, &engine_instance.container_name).await?;
    engine::wait_ready(ctx, &engine_instance, READY_TIMEOUT_SECS, r, c).await?;
    c.check()?;

    let admin = require_admin_password(ctx, &engine_instance)?;
    reject_duplicates(
        ctx,
        &x,
        &engine_instance,
        &admin,
        &s.bucket_name,
        access_key,
    )
    .await?;
    act.step("이름 중복 확인");
    c.check()?;

    let id = util::new_id();
    let credential_ref = secrets::bucket_ref(&id);
    ctx.secrets.set(&credential_ref, secret_key)?;

    r.step(1, 4, format!("버킷 {} 생성", s.bucket_name));
    if let Err(e) = minio::create_bucket(&x, &engine_instance, &admin, &s.bucket_name).await {
        *rollback = Some(
            undo(
                ctx,
                &x,
                &engine_instance,
                &admin,
                &s.bucket_name,
                access_key,
                &credential_ref,
            )
            .await,
        );
        return Err(e);
    }
    act.step(format!("버킷 {} 생성", s.bucket_name));
    r.step_done(1);
    c.check()?;

    r.step(2, 4, "전용 액세스 키 및 정책 생성");
    if let Err(e) = minio::create_scoped_user(
        &x,
        &engine_instance,
        &admin,
        &s.bucket_name,
        access_key,
        secret_key,
    )
    .await
    {
        *rollback = Some(
            undo(
                ctx,
                &x,
                &engine_instance,
                &admin,
                &s.bucket_name,
                access_key,
                &credential_ref,
            )
            .await,
        );
        return Err(e);
    }
    act.step(format!(
        "액세스 키 {access_key} 및 정책 {} 생성",
        minio::policy_name(&s.bucket_name)
    ));
    r.step_done(2);
    c.check()?;

    r.step(3, 4, "접속 테스트");
    if let Err(e) =
        minio::verify_access(&x, &engine_instance, &s.bucket_name, access_key, secret_key).await
    {
        *rollback = Some(
            undo(
                ctx,
                &x,
                &engine_instance,
                &admin,
                &s.bucket_name,
                access_key,
                &credential_ref,
            )
            .await,
        );
        return Err(e);
    }
    act.step("접속 테스트 성공");
    r.step_done(3);

    r.step(4, 4, "메타데이터 저장");
    let row = ManagedBucket {
        id,
        engine_instance_id: engine_instance.id.clone(),
        project_name: s.project_name.clone(),
        bucket_name: s.bucket_name.clone(),
        access_key: access_key.to_string(),
        credential_ref: credential_ref.clone(),
        preferred_local_tunnel_port: s.preferred_local_tunnel_port,
        created_at: util::now(),
        last_connection_test_at: Some(util::now()),
        last_backup_at: None,
    };
    if let Err(e) = ctx.store.insert_bucket(&row) {
        *rollback = Some(
            undo(
                ctx,
                &x,
                &engine_instance,
                &admin,
                &s.bucket_name,
                access_key,
                &credential_ref,
            )
            .await,
        );
        return Err(e);
    }
    r.step_done(4);

    let view = BucketView {
        bucket: row.clone(),
        engine: engine_instance.clone(),
        target: t.clone(),
        stats: BucketStats::default(),
        tunnel: None,
    };
    Ok(Created {
        connection: connection_with(ctx, &view, Some(secret_key.to_string()), &s.region),
        bucket: row,
        engine: engine_instance,
    })
}

fn resolve_access_key(given: Option<&str>) -> Result<String> {
    match given {
        None => Ok(minio::generate_access_key()),
        Some(key) => {
            validate_key(key)?;
            Ok(key.to_string())
        }
    }
}

fn resolve_secret_key(given: Option<&str>) -> Result<String> {
    match given {
        None => Ok(util::generate_password(SECRET_KEY_LEN)),
        Some(k) if k.trim().is_empty() => Err(Error::Usage(
            "시크릿 키가 비어 있습니다. 값을 입력하거나 자동 생성을 사용하세요.".into(),
        )),
        Some(k) if k.contains('\n') || k.contains('\r') => Err(Error::Usage(
            "시크릿 키에는 줄바꿈을 포함할 수 없습니다.".into(),
        )),
        Some(k) if k.chars().count() < SECRET_KEY_MIN => Err(Error::Usage(format!(
            "시크릿 키는 {SECRET_KEY_MIN}자 이상이어야 합니다 (현재 {}자).",
            k.chars().count()
        ))),
        Some(k) => Ok(k.to_string()),
    }
}

/// Administering MinIO means authenticating as its root user. In restricted
/// secret mode nothing was ever stored, so say so instead of failing with an
/// `mc` error the user cannot act on.
fn require_admin_password(ctx: &Ctx, e: &EngineInstance) -> Result<String> {
    engine::admin_password(ctx, e)?.ok_or_else(|| {
        Error::Refused(format!(
            "엔진 `{}`의 관리자 비밀번호가 저장되어 있지 않아 버킷을 관리할 수 없습니다. \
             `secrets.mode`를 확인하거나 엔진을 다시 생성하세요.",
            e.container_name
        ))
    })
}

/// DB-004 both ways: the local registry *and* the live server must be free of
/// the name, because the engine may be shared with buckets this app never
/// created.
async fn reject_duplicates(
    ctx: &Ctx,
    x: &Executor,
    e: &EngineInstance,
    admin: &str,
    bucket: &str,
    access_key: &str,
) -> Result<()> {
    if ctx.store.find_bucket_on_engine(&e.id, bucket)?.is_some() {
        return Err(Error::Conflict(format!(
            "`{bucket}` 버킷은 이미 이 엔진에 등록되어 있습니다. 다른 이름을 사용하세요."
        )));
    }
    if ctx
        .store
        .list_buckets_for_engine(&e.id)?
        .iter()
        .any(|b| b.access_key == access_key)
    {
        return Err(Error::Conflict(format!(
            "`{access_key}` 액세스 키는 이미 이 엔진에 등록되어 있습니다. 다른 키를 사용하세요."
        )));
    }
    if minio::bucket_exists(x, e, admin, bucket).await? {
        return Err(Error::Conflict(format!(
            "엔진 `{}`에 이미 `{bucket}` 버킷이 있습니다. 다른 이름을 사용하세요.",
            e.container_name
        )));
    }
    if minio::user_exists(x, e, admin, access_key).await? {
        return Err(Error::Conflict(format!(
            "엔진 `{}`에 이미 `{access_key}` 액세스 키가 있습니다. 다른 키를 사용하세요.",
            e.container_name
        )));
    }
    Ok(())
}

/// Remove whatever the failed attempt managed to create. Best effort by
/// design: the original error is what the user needs to see, so a failure to
/// clean up is folded into the returned reason instead of replacing it.
///
/// Existence is re-checked rather than tracked, so this is correct however far
/// the attempt got — and never leaves an orphan bucket, policy or user.
async fn undo(
    ctx: &Ctx,
    x: &Executor,
    e: &EngineInstance,
    admin: &str,
    bucket: &str,
    access_key: &str,
    credential_ref: &str,
) -> String {
    let _ = ctx.secrets.delete(credential_ref);
    let mut problems: Vec<String> = Vec::new();

    if let Err(err) = minio::remove_scoped_user(x, e, admin, bucket, access_key).await {
        problems.push(err.as_diagnostic().what);
    }
    match minio::bucket_exists(x, e, admin, bucket).await {
        Ok(true) => {
            if let Err(err) = minio::remove_bucket(x, e, admin, bucket).await {
                problems.push(err.as_diagnostic().what);
            }
        }
        Ok(false) => {}
        Err(err) => problems.push(err.as_diagnostic().what),
    }

    if problems.is_empty() {
        format!("생성 실패로 버킷 `{bucket}`과 액세스 키 `{access_key}`을(를) 정리했습니다")
    } else {
        format!(
            "생성 실패 후 정리도 실패했습니다: {}. `{bucket}`/`{access_key}`이(가) 남아 있는지 확인하세요",
            problems.join("; ")
        )
    }
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Every registered bucket with its engine, target and latest tunnel.
/// `with_stats` adds object counts and sizes; an unreachable target yields
/// empty statistics rather than an error (PRD §7.6).
pub async fn views(ctx: &Ctx, with_stats: bool) -> Result<Vec<BucketView>> {
    let targets: HashMap<String, Target> = ctx
        .store
        .list_targets()?
        .into_iter()
        .map(|t| (t.id.clone(), t))
        .collect();
    let engines: HashMap<String, EngineInstance> = ctx
        .store
        .list_engines()?
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();

    // One status probe per engine, not per bucket.
    let mut reachable: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    for bucket in ctx.store.list_buckets()? {
        let Some(engine_row) = engines.get(&bucket.engine_instance_id) else {
            continue;
        };
        let Some(target) = targets.get(&engine_row.target_id) else {
            continue;
        };
        let tunnel = ctx.store.latest_tunnel(&bucket.id)?;
        let mut view = BucketView {
            bucket,
            engine: engine_row.clone(),
            target: target.clone(),
            stats: BucketStats::default(),
            tunnel,
        };
        if with_stats {
            let live = match reachable.get(&engine_row.id) {
                Some(v) => *v,
                None => {
                    let v = engine_running(ctx, target, engine_row).await;
                    reachable.insert(engine_row.id.clone(), v);
                    v
                }
            };
            if live {
                view.stats = read_stats(ctx, &view).await;
            }
        }
        out.push(view);
    }
    Ok(out)
}

/// One bucket by id or by name, with statistics filled in when the engine is
/// reachable.
pub async fn view(ctx: &Ctx, key: &str) -> Result<BucketView> {
    let bucket = ctx.store.require_bucket(key)?;
    let engine_row = ctx.store.get_engine(&bucket.engine_instance_id)?;
    let target = ctx.store.require_target(&engine_row.target_id)?;
    let tunnel = ctx.store.latest_tunnel(&bucket.id)?;
    let mut view = BucketView {
        bucket,
        engine: engine_row,
        target,
        stats: BucketStats::default(),
        tunnel,
    };
    if engine_running(ctx, &view.target, &view.engine).await {
        view.stats = read_stats(ctx, &view).await;
    }
    Ok(view)
}

async fn engine_running(ctx: &Ctx, target: &Target, e: &EngineInstance) -> bool {
    let Ok(x) = ctx.executor(target) else {
        return false;
    };
    matches!(
        docker::container_status(&x, &e.container_name).await,
        Ok(status) if status.running
    )
}

async fn read_stats(ctx: &Ctx, v: &BucketView) -> BucketStats {
    let Ok(x) = ctx.executor(&v.target) else {
        return BucketStats::default();
    };
    let Ok(Some(admin)) = engine::admin_password(ctx, &v.engine) else {
        return BucketStats::default();
    };
    minio::bucket_usage(&x, &v.engine, &admin, &v.bucket.bucket_name)
        .await
        .unwrap_or_default()
}

/// DB-006 for object storage. For a remote target this is the *tunnel*
/// endpoint; without a live tunnel there is no address a local client could
/// use, so the call is refused with the action that fixes it (TUN-006).
pub fn connection_info(ctx: &Ctx, v: &BucketView) -> Result<S3ConnectionInfo> {
    connection_parts(v, ctx.secrets.get(&v.bucket.credential_ref)?)
}

/// Same as [`connection_info`] but never reads a secret — for painting the UI.
pub fn connection_preview(v: &BucketView) -> Result<S3ConnectionInfo> {
    connection_parts(v, None)
}

fn connection_parts(v: &BucketView, secret_key: Option<String>) -> Result<S3ConnectionInfo> {
    let (host, port) = v.client_endpoint().ok_or_else(|| {
        Error::Refused(format!(
            "`{}`은(는) 원격 Target `{}`에 있어 SSH 터널이 필요합니다. \
             `linf tunnel start {}`로 터널을 먼저 시작하세요.",
            v.bucket.bucket_name, v.target.display_name, v.bucket.bucket_name
        ))
    })?;
    Ok(S3ConnectionInfo {
        host,
        port,
        bucket: v.bucket.bucket_name.clone(),
        access_key: v.bucket.access_key.clone(),
        secret_key,
        region: DEFAULT_REGION.to_string(),
        secure: false,
    })
}

/// Endpoint used when reporting the *result* of a mutation. A remote bucket has
/// no live tunnel the instant it is created, so the reserved local port is
/// reported instead of refusing to answer.
fn connection_with(
    ctx: &Ctx,
    v: &BucketView,
    secret_key: Option<String>,
    region: &str,
) -> S3ConnectionInfo {
    let (host, port) = v.client_endpoint().unwrap_or_else(|| {
        (
            "127.0.0.1".to_string(),
            v.bucket
                .preferred_local_tunnel_port
                .unwrap_or(v.engine.host_port),
        )
    });
    let region = match region.trim() {
        "" => DEFAULT_REGION,
        r => r,
    };
    S3ConnectionInfo {
        host,
        port,
        bucket: v.bucket.bucket_name.clone(),
        access_key: v.bucket.access_key.clone(),
        secret_key: secret_key.or_else(|| ctx.secrets.get(&v.bucket.credential_ref).ok().flatten()),
        region: region.to_string(),
        secure: false,
    }
}

/// DB-005 on demand: a real request with the *project* key, then a timestamp.
pub async fn test_connection(ctx: &Ctx, v: &BucketView) -> Result<()> {
    let secret_key = require_secret_key(ctx, v)?;
    let x = ctx.executor(&v.target)?;
    minio::verify_access(
        &x,
        &v.engine,
        &v.bucket.bucket_name,
        &v.bucket.access_key,
        &secret_key,
    )
    .await?;
    let mut row = v.bucket.clone();
    row.last_connection_test_at = Some(util::now());
    ctx.store.update_bucket(&row)?;
    Ok(())
}

fn require_secret_key(ctx: &Ctx, v: &BucketView) -> Result<String> {
    ctx.secrets.get(&v.bucket.credential_ref)?.ok_or_else(|| {
        Error::Refused(format!(
            "`{}`의 시크릿 키가 저장되어 있지 않습니다. \
                 `linf bucket rotate-key {}`로 새 키를 발급하세요.",
            v.bucket.bucket_name, v.bucket.bucket_name
        ))
    })
}

// ---------------------------------------------------------------------------
// Drop / forget
// ---------------------------------------------------------------------------

/// DB-008 analogue: this plan never contains the engine or the volume.
fn drop_plan(v: &BucketView, backups: usize) -> Plan {
    let mut plan = Plan::new(format!("`{}` 버킷 삭제", v.bucket.bucket_name))
        .step_detailed(
            StepKind::Destroy,
            format!("버킷 {} 및 모든 객체 삭제", v.bucket.bucket_name),
            format!(
                "Target {} · 엔진 {}",
                v.target.display_name, v.engine.container_name
            ),
        )
        .step_detailed(
            StepKind::Destroy,
            format!("액세스 키 {} 삭제", v.bucket.access_key),
            format!(
                "정책 {} 도 함께 삭제됩니다.",
                minio::policy_name(&v.bucket.bucket_name)
            ),
        )
        .step(StepKind::Destroy, "저장된 시크릿 키 삭제")
        .step_detailed(
            StepKind::Reuse,
            format!("엔진 {} 유지", v.engine.container_name),
            "같은 엔진의 다른 버킷은 영향을 받지 않습니다.",
        )
        .warn("이 작업은 되돌릴 수 없습니다.");
    if let Some(objects) = v.stats.objects {
        plan = plan.warn(format!("삭제되는 객체 수: {objects}개"));
    }
    if let Some(size) = v.stats.size_bytes {
        plan = plan.warn(format!(
            "삭제되는 데이터 크기: 약 {}",
            util::human_bytes(size)
        ));
    }
    if v.tunnel.is_some() {
        plan = plan.warn("이 버킷의 터널 기록도 함께 삭제됩니다. 실행 중이면 먼저 중지하세요.");
    }
    plan = plan.warn(match backups {
        0 => "백업 기록이 없습니다. 먼저 `linf backup run`을 실행하는 것을 권장합니다.".to_string(),
        n => format!("백업 기록 {n}건이 목록에서 제거됩니다(파일 자체는 남습니다)."),
    });
    plan
}

pub async fn plan_drop(ctx: &Ctx, v: &BucketView) -> Result<Plan> {
    let backups = ctx.store.list_backups(Some(&v.bucket.id))?.len();
    Ok(drop_plan(v, backups))
}

/// DB-008 analogue. Removes one project's bucket, its objects, its access key
/// and its policy — nothing else.
pub async fn drop(ctx: &Ctx, v: &BucketView, r: &Reporter) -> Result<()> {
    ctx.require_write_lock()?;
    let x = ctx.executor(&v.target)?;
    docker::require_managed(&x, &v.engine.container_name).await?;
    let admin = require_admin_password(ctx, &v.engine)?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "bucket",
        "drop",
        format!(
            "`{}` 버킷과 액세스 키 `{}`을(를) 삭제",
            v.bucket.bucket_name, v.bucket.access_key
        ),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.bucket.id);

    let result = drop_inner(ctx, &x, v, &admin, r, &mut act).await;
    act.finish(&result);
    result
}

async fn drop_inner(
    ctx: &Ctx,
    x: &Executor,
    v: &BucketView,
    admin: &str,
    r: &Reporter,
    act: &mut Activity<'_>,
) -> Result<()> {
    crate::core::tunnel::stop_for_resource(ctx, &v.bucket.id).await?;
    act.step("관련 SSH 터널을 정리했습니다");
    r.step(1, 4, format!("버킷 {} 및 객체 삭제", v.bucket.bucket_name));
    minio::remove_bucket(x, &v.engine, admin, &v.bucket.bucket_name).await?;
    act.step(format!("버킷 {} 삭제", v.bucket.bucket_name));
    r.step_done(1);

    r.step(2, 4, "액세스 키 및 정책 삭제");
    minio::remove_scoped_user(
        x,
        &v.engine,
        admin,
        &v.bucket.bucket_name,
        &v.bucket.access_key,
    )
    .await?;
    act.step(format!(
        "액세스 키 {} 및 정책 {} 삭제",
        v.bucket.access_key,
        minio::policy_name(&v.bucket.bucket_name)
    ));
    r.step_done(2);

    r.step(3, 4, "메타데이터 삭제");
    ctx.store.delete_bucket(&v.bucket.id)?;
    act.step("등록 정보 삭제");
    r.step_done(3);

    r.step(4, 4, "시크릿 키 삭제");
    ctx.secrets.delete(&v.bucket.credential_ref)?;
    r.step_done(4);
    Ok(())
}

/// DB-007 analogue: unregister only. The server keeps the bucket, the key and
/// the policy exactly as they are.
pub fn forget(ctx: &Ctx, v: &BucketView) -> Result<()> {
    ctx.require_write_lock()?;
    let act = Activity::start(
        &ctx.store,
        ctx.origin,
        "bucket",
        "forget",
        format!("`{}` 등록 해제 (서버의 버킷은 유지)", v.bucket.bucket_name),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.bucket.id);

    let result = (|| -> Result<()> {
        ctx.store.delete_bucket(&v.bucket.id)?;
        ctx.secrets.delete(&v.bucket.credential_ref)?;
        Ok(())
    })();
    act.finish(&result);
    result
}

// ---------------------------------------------------------------------------
// Rotate
// ---------------------------------------------------------------------------

/// DB-009 analogue. The server is changed first: a stored secret the server
/// does not accept is worse than a rotation that failed outright.
pub async fn rotate_key(ctx: &Ctx, v: &BucketView) -> Result<S3ConnectionInfo> {
    ctx.require_write_lock()?;
    let x = ctx.executor(&v.target)?;
    docker::require_managed(&x, &v.engine.container_name).await?;
    let admin = require_admin_password(ctx, &v.engine)?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "bucket",
        "rotate-key",
        format!("`{}` 액세스 키의 시크릿 교체", v.bucket.access_key),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.bucket.id);

    let previous = ctx.secrets.get(&v.bucket.credential_ref).unwrap_or(None);
    let secret_key = util::generate_password(SECRET_KEY_LEN);
    let mut rollback: Option<String> = None;
    let result = rotate_inner(
        ctx,
        &x,
        v,
        &admin,
        &secret_key,
        previous.as_deref(),
        &mut act,
        &mut rollback,
    )
    .await;
    match (&result, rollback) {
        (Err(_), Some(reason)) => act.rolled_back(reason),
        _ => act.finish(&result),
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn rotate_inner(
    ctx: &Ctx,
    x: &Executor,
    v: &BucketView,
    admin: &str,
    secret_key: &str,
    previous: Option<&str>,
    act: &mut Activity<'_>,
    rollback: &mut Option<String>,
) -> Result<S3ConnectionInfo> {
    minio::set_user_secret(x, &v.engine, admin, &v.bucket.access_key, secret_key).await?;
    act.step("서버 시크릿 키 변경");

    if let Err(e) = ctx.secrets.set(&v.bucket.credential_ref, secret_key) {
        // The new secret exists only on the server and nowhere else; put the
        // old one back so the project is not locked out.
        *rollback = match previous {
            Some(old) => {
                match minio::set_user_secret(x, &v.engine, admin, &v.bucket.access_key, old).await {
                    Ok(()) => Some("시크릿 키 저장에 실패해 이전 키로 되돌렸습니다".into()),
                    Err(_) => Some(
                        "시크릿 키 저장과 되돌리기가 모두 실패했습니다. \
                         `linf bucket rotate-key`를 다시 실행하세요"
                            .into(),
                    ),
                }
            }
            None => {
                Some("시크릿 키 저장에 실패했습니다. 저장 모드를 확인한 뒤 다시 실행하세요".into())
            }
        };
        return Err(e);
    }
    act.step("새 시크릿 키 저장");

    minio::verify_access(
        x,
        &v.engine,
        &v.bucket.bucket_name,
        &v.bucket.access_key,
        secret_key,
    )
    .await?;
    act.step("접속 테스트 성공");
    Ok(connection_with(
        ctx,
        v,
        Some(secret_key.to_string()),
        DEFAULT_REGION,
    ))
}

// ---------------------------------------------------------------------------
// Archive format
// ---------------------------------------------------------------------------

/// First line of a bucket archive. The version is in the magic, so a future
/// format change is a different token rather than a field to negotiate.
const ARCHIVE_MAGIC: &str = "LINFBKT1";

/// What an archive says it contains. The bytes of every object follow the
/// manifest line, back to back, in `objects` order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub bucket: String,
    pub created_at: DateTime<Utc>,
    pub objects: Vec<ObjectEntry>,
}

impl Manifest {
    /// Bytes the object section must contain.
    pub fn body_bytes(&self) -> u64 {
        self.objects.iter().map(|o| o.size).sum()
    }
}

/// The exact prefix of an archive file: the magic line, then the manifest as
/// one line of JSON. Pure counterpart of [`parse_manifest`].
pub fn write_manifest(m: &Manifest) -> Result<Vec<u8>> {
    // `to_string` never emits a newline, which is what keeps the manifest on
    // one line and the body at a computable offset.
    let json = serde_json::to_string(m)?;
    Ok(format!("{ARCHIVE_MAGIC}\n{json}\n").into_bytes())
}

/// Parse the archive prefix: the magic line, then the manifest line. Anything
/// after the manifest line is ignored, so a whole archive file may be handed
/// in — the object bytes that follow need not be UTF-8, and the two lines are
/// found and validated at the byte level for exactly that reason.
///
/// A prefix that stops early is an error, never a manifest with fewer objects:
/// the difference decides whether a restore is complete.
pub fn parse_manifest(prefix: &[u8]) -> Result<Manifest> {
    let Some(first) = prefix.iter().position(|b| *b == b'\n') else {
        return Err(truncated(
            "백업 파일이 헤더에서 끊겼습니다",
            "첫 줄이 끝나지 않았습니다.",
        ));
    };
    let magic = std::str::from_utf8(&prefix[..first]).unwrap_or_default();
    if magic.trim_end_matches('\r') != ARCHIVE_MAGIC {
        return Err(Error::failed(
            "객체 백업 파일이 아닙니다",
            format!("첫 줄이 `{ARCHIVE_MAGIC}`이 아닙니다."),
            "`.objects` 확장자의 버킷 백업 파일을 지정하세요.",
        ));
    }
    let rest = &prefix[first + 1..];
    let Some(second) = rest.iter().position(|b| *b == b'\n') else {
        return Err(truncated(
            "백업 파일이 매니페스트에서 끊겼습니다",
            "매니페스트 줄이 끝나지 않았습니다.",
        ));
    };
    let line = std::str::from_utf8(&rest[..second]).map_err(|_| {
        Error::failed(
            "백업 파일의 매니페스트를 읽을 수 없습니다",
            "매니페스트가 UTF-8이 아닙니다.",
            "파일이 전송 중 손상되지 않았는지 확인한 뒤 다시 시도하세요.",
        )
    })?;
    serde_json::from_str(line.trim()).map_err(|e| {
        Error::failed(
            "백업 파일의 매니페스트를 읽을 수 없습니다",
            format!("JSON 파싱에 실패했습니다: {e}"),
            "파일이 전송 중 손상되지 않았는지 확인한 뒤 다시 시도하세요.",
        )
    })
}

fn truncated(what: &str, cause: &str) -> Error {
    Error::failed(
        what.to_string(),
        cause.to_string(),
        "백업 파일을 다시 받은 뒤 복원하세요. 잘린 파일로는 복원하지 않습니다.",
    )
}

/// A truncated archive is refused before a single object is written: half a
/// restore is worse than none.
fn check_body_length(m: &Manifest, prefix_len: u64, file_len: u64) -> Result<()> {
    let expected = prefix_len.saturating_add(m.body_bytes());
    if file_len < expected {
        return Err(truncated(
            "백업 파일이 잘려 있습니다",
            &format!(
                "매니페스트는 객체 {}개, 총 {}를 요구하지만 파일은 {}입니다.",
                m.objects.len(),
                util::human_bytes(expected),
                util::human_bytes(file_len)
            ),
        ));
    }
    Ok(())
}

/// Read the archive prefix, leaving `reader` positioned at the first object
/// byte. Returns the manifest and the number of bytes the prefix occupied.
async fn read_prefix<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<(Manifest, u64)> {
    let mut prefix = Vec::new();
    let mut consumed = 0u64;
    for _ in 0..2 {
        let n = reader.read_until(b'\n', &mut prefix).await?;
        if n == 0 {
            break;
        }
        consumed += n as u64;
    }
    Ok((parse_manifest(&prefix)?, consumed))
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/// `letsbid-dev-20260831-030000.objects`. The stamp is UTC so it matches
/// [`BackupRecord::created_at`] and never repeats across a DST change.
fn backup_file_name(bucket: &str, at: DateTime<Utc>) -> String {
    format!(
        "{bucket}-{}.{}",
        at.format("%Y%m%d-%H%M%S"),
        BackupFormat::Objects.extension()
    )
}

/// Never overwrite an existing archive: two backups inside the same second get
/// a counter rather than one clobbering the other.
fn unique_path(dir: &Path, file_name: &str) -> Result<PathBuf> {
    let first = dir.join(file_name);
    if !first.exists() {
        return Ok(first);
    }
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((s, e)) => (s, Some(e)),
        None => (file_name, None),
    };
    for n in 2..1000 {
        let candidate = match ext {
            Some(e) => dir.join(format!("{stem}-{n}.{e}")),
            None => dir.join(format!("{stem}-{n}")),
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::failed(
        "백업 파일 이름을 정할 수 없습니다",
        format!("`{}`에 같은 이름의 파일이 너무 많습니다.", dir.display()),
        "다른 저장 폴더를 지정하거나 오래된 백업을 정리하세요.",
    ))
}

/// An archive holds every byte of a project's objects, so it is never
/// world-readable, not even for an instant (PRD §11.2).
fn create_private(path: &Path) -> Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    harden_file(path)?;
    Ok(file)
}

fn backup_plan(v: &BucketView, path: &Path, previous: usize) -> Plan {
    let mut plan = Plan::new(format!("`{}` 백업", v.bucket.bucket_name))
        .step_detailed(
            StepKind::Verify,
            format!("엔진 {} 상태 확인", v.engine.container_name),
            format!("Target {}", v.target.display_name),
        )
        .step_detailed(
            StepKind::New,
            "객체 목록 조회",
            "컨테이너 안에서 `mc ls --recursive`로 매니페스트를 만듭니다.",
        )
        .step_detailed(
            StepKind::New,
            "객체를 로컬 파일로 스트리밍",
            format!("{} (0600)", path.display()),
        )
        .step(StepKind::Verify, "SHA-256 체크섬 계산");
    if v.target.is_remote() {
        plan = plan.warn(
            "원격 백업은 SSH 스트림으로 곧바로 로컬 파일에 저장됩니다. VPS에는 아무것도 남지 않습니다.",
        );
    }
    if let Some(objects) = v.stats.objects {
        plan = plan.warn(format!("대상 버킷의 객체 수는 약 {objects}개입니다."));
    }
    if let Some(size) = v.stats.size_bytes {
        plan = plan.warn(format!(
            "대상 버킷 크기는 약 {}입니다. 저장 폴더의 여유 공간을 확인하세요.",
            util::human_bytes(size)
        ));
    }
    if previous > 0 {
        plan = plan.warn(format!(
            "이 버킷의 기존 백업 기록 {previous}건은 그대로 유지됩니다."
        ));
    }
    plan
}

pub async fn plan_backup(ctx: &Ctx, v: &BucketView, out_dir: &Path) -> Result<Plan> {
    let previous = ctx.store.list_backups(Some(&v.bucket.id))?.len();
    let path = out_dir.join(backup_file_name(&v.bucket.bucket_name, util::now()));
    Ok(backup_plan(v, &path, previous))
}

/// BAK-001/002/003/004/007 for object storage. Streams every object into one
/// local `0600` archive, reports transferred bytes, and can be cancelled
/// between objects and between chunks. A cancelled or failed run deletes the
/// partial file and leaves a `failed` record behind so the attempt stays
/// visible.
pub async fn backup(
    ctx: &Ctx,
    v: &BucketView,
    out_dir: &Path,
    r: &Reporter,
    c: &Cancel,
) -> Result<BackupRecord> {
    ctx.require_write_lock()?;
    let x = ctx.executor(&v.target)?;
    let admin = require_admin_password(ctx, &v.engine)?;
    std::fs::create_dir_all(out_dir)?;
    let directory = std::fs::canonicalize(out_dir).unwrap_or_else(|_| out_dir.to_path_buf());

    let created_at = util::now();
    let path = unique_path(
        &directory,
        &backup_file_name(&v.bucket.bucket_name, created_at),
    )?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("backup")
        .to_string();

    let mut record = BackupRecord {
        id: util::new_id(),
        resource_id: v.bucket.id.clone(),
        resource_kind: ResourceKind::Bucket,
        storage_location: directory.display().to_string(),
        file_name,
        format: BackupFormat::Objects,
        size: 0,
        checksum: String::new(),
        status: BackupStatus::Running,
        created_at,
    };
    ctx.store.insert_backup(&record)?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "bucket",
        "backup",
        format!("`{}` 버킷을 객체 아카이브로 백업", v.bucket.bucket_name),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.bucket.id);

    let result = backup_inner(
        ctx,
        &x,
        v,
        &admin,
        &path,
        created_at,
        &mut record,
        r,
        c,
        &mut act,
    )
    .await;
    match &result {
        Ok(()) => record.status = BackupStatus::Ok,
        Err(_) => {
            record.status = BackupStatus::Failed;
            record.size = 0;
            record.checksum.clear();
            let _ = std::fs::remove_file(&path);
        }
    }
    let _ = ctx.store.insert_backup(&record);
    act.finish(&result);
    result.map(|()| record)
}

#[allow(clippy::too_many_arguments)]
async fn backup_inner(
    ctx: &Ctx,
    x: &Executor,
    v: &BucketView,
    admin: &str,
    path: &Path,
    created_at: DateTime<Utc>,
    record: &mut BackupRecord,
    r: &Reporter,
    c: &Cancel,
    act: &mut Activity<'_>,
) -> Result<()> {
    c.check()?;
    let bucket = &v.bucket.bucket_name;

    r.step(1, 4, "객체 목록 조회");
    let objects = minio::list_objects(x, &v.engine, admin, bucket).await?;
    let manifest = Manifest {
        bucket: bucket.clone(),
        created_at,
        objects,
    };
    act.step(format!(
        "객체 {}개 · {} 확인",
        manifest.objects.len(),
        util::human_bytes(manifest.body_bytes())
    ));
    r.step_done(1);
    c.check()?;

    r.step(2, 4, format!("객체 {}개 스트리밍", manifest.objects.len()));
    let secrets = engine::minio_admin_env(&v.engine, admin)?;
    let written = {
        let mut sink = tokio::fs::File::from_std(create_private(path)?);
        sink.write_all(&write_manifest(&manifest)?).await?;
        let mut written = 0u64;
        for object in &manifest.objects {
            c.check()?;
            let argv = minio::cat_argv(x.docker_bin(), &v.engine, bucket, &object.key);
            let (out, bytes) = x.stream_out(&argv, &secrets, &mut sink, c, r).await?;
            if !out.ok() {
                return Err(x.failure(
                    &argv,
                    &out,
                    &format!("객체 `{}`을(를) 읽지 못했습니다", object.key),
                    "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
                ));
            }
            if bytes != object.size {
                // The manifest is what a restore trusts, so an archive whose
                // body disagrees with it must never be written out.
                return Err(Error::failed(
                    "백업 중 버킷이 변경되었습니다",
                    format!(
                        "객체 `{}`은(는) {}바이트로 기록되었지만 {}바이트가 전송되었습니다.",
                        object.key, object.size, bytes
                    ),
                    "쓰기가 멈춘 뒤 `linf backup run`을 다시 실행하세요.",
                ));
            }
            written += bytes;
        }
        sink.flush().await?;
        sink.sync_all().await?;
        written
    };
    act.step(format!("{} 스트리밍 완료", util::human_bytes(written)));
    r.step_done(2);

    r.step(3, 4, "체크섬 계산");
    record.size = std::fs::metadata(path)?.len();
    record.checksum = sha256_file(path).await?;
    r.step_done(3);

    r.step(4, 4, "기록 갱신");
    let mut row = v.bucket.clone();
    row.last_backup_at = Some(util::now());
    ctx.store.update_bucket(&row)?;
    act.step(format!("{} 저장", path.display()));
    r.step_done(4);
    Ok(())
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

fn restore_plan(
    file: &Path,
    size: u64,
    m: &Manifest,
    v: &BucketView,
    overwrite: bool,
    existing: usize,
) -> Plan {
    let mut plan = Plan::new(format!("`{}`(으)로 복원", v.bucket.bucket_name)).step_detailed(
        StepKind::Verify,
        "백업 파일 확인",
        format!(
            "{} · {} · 객체 {}개 (원본 버킷 {})",
            file.display(),
            util::human_bytes(size),
            m.objects.len(),
            m.bucket
        ),
    );
    if existing > 0 {
        if overwrite {
            plan = plan
                .step_detailed(
                    StepKind::Destroy,
                    format!("`{}`의 기존 객체를 덮어씁니다", v.bucket.bucket_name),
                    format!("현재 객체 {existing}개 중 백업과 키가 같은 객체는 교체됩니다."),
                )
                .warn("덮어쓰기: 이 작업은 되돌릴 수 없습니다. 먼저 현재 상태를 백업하세요.");
        } else {
            plan = plan.warn(format!(
                "대상 버킷 `{}`에 이미 객체 {existing}개가 있어 복원이 거부됩니다. \
                 덮어쓰려면 overwrite를 지정하세요.",
                v.bucket.bucket_name
            ));
        }
    } else {
        plan = plan.step_detailed(
            StepKind::Reuse,
            format!("`{}`은(는) 비어 있습니다", v.bucket.bucket_name),
            "덮어써지는 객체가 없습니다.",
        );
    }
    plan.step_detailed(
        StepKind::New,
        format!("mc pipe로 객체 {}개 복원", m.objects.len()),
        format!(
            "파일을 컨테이너 stdin으로 흘려보냅니다 (Target {}).",
            v.target.display_name
        ),
    )
}

/// BAK-006: the preview always says, in words, whether data will be lost.
pub async fn plan_restore(ctx: &Ctx, file: &Path, v: &BucketView, overwrite: bool) -> Result<Plan> {
    let meta = std::fs::metadata(file).map_err(|_| {
        Error::NotFound(format!(
            "백업 파일 `{}`을(를) 찾을 수 없습니다.",
            file.display()
        ))
    })?;
    let mut source = BufReader::new(tokio::fs::File::open(file).await?);
    let (manifest, prefix_len) = read_prefix(&mut source).await?;
    check_body_length(&manifest, prefix_len, meta.len())?;

    let existing = match (
        ctx.executor(&v.target),
        engine::admin_password(ctx, &v.engine),
    ) {
        (Ok(x), Ok(Some(admin))) => {
            minio::list_objects(&x, &v.engine, &admin, &v.bucket.bucket_name)
                .await
                .map(|o| o.len())
                .unwrap_or(0)
        }
        _ => 0,
    };
    Ok(restore_plan(
        file,
        meta.len(),
        &manifest,
        v,
        overwrite,
        existing,
    ))
}

/// BAK-005/006. Refuses to touch a bucket that already holds objects unless
/// `overwrite` was granted explicitly, and refuses a truncated archive before
/// writing anything at all.
pub async fn restore(
    ctx: &Ctx,
    file: &Path,
    v: &BucketView,
    overwrite: bool,
    r: &Reporter,
    c: &Cancel,
) -> Result<()> {
    ctx.require_write_lock()?;
    if !file.is_file() {
        return Err(Error::NotFound(format!(
            "백업 파일 `{}`을(를) 찾을 수 없습니다.",
            file.display()
        )));
    }
    let x = ctx.executor(&v.target)?;
    docker::require_managed(&x, &v.engine.container_name).await?;
    let admin = require_admin_password(ctx, &v.engine)?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "bucket",
        "restore",
        format!(
            "`{}`을(를) `{}`(으)로 복원{}",
            file.display(),
            v.bucket.bucket_name,
            if overwrite { " (덮어쓰기)" } else { "" }
        ),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.bucket.id);

    let result = restore_inner(&x, file, v, &admin, overwrite, r, c, &mut act).await;
    act.finish(&result);
    result
}

#[allow(clippy::too_many_arguments)]
async fn restore_inner(
    x: &Executor,
    file: &Path,
    v: &BucketView,
    admin: &str,
    overwrite: bool,
    r: &Reporter,
    c: &Cancel,
    act: &mut Activity<'_>,
) -> Result<()> {
    c.check()?;
    let bucket = &v.bucket.bucket_name;

    r.step(1, 3, "백업 파일 확인");
    let file_len = std::fs::metadata(file)?.len();
    let mut source = BufReader::new(tokio::fs::File::open(file).await?);
    let (manifest, prefix_len) = read_prefix(&mut source).await?;
    check_body_length(&manifest, prefix_len, file_len)?;
    act.step(format!(
        "매니페스트 확인: 객체 {}개 · {}",
        manifest.objects.len(),
        util::human_bytes(manifest.body_bytes())
    ));
    r.step_done(1);
    c.check()?;

    r.step(2, 3, "대상 버킷 확인");
    if minio::bucket_exists(x, &v.engine, admin, bucket).await? {
        let existing = minio::list_objects(x, &v.engine, admin, bucket).await?;
        if !existing.is_empty() && !overwrite {
            return Err(Error::Refused(format!(
                "대상 버킷 `{bucket}`에 이미 객체 {}개가 있습니다. \
                 덮어쓰려면 overwrite를 지정하고 다시 실행하세요.",
                existing.len()
            )));
        }
    } else {
        minio::create_bucket(x, &v.engine, admin, bucket).await?;
        act.step(format!("대상 버킷 {bucket} 생성"));
    }
    r.step_done(2);
    c.check()?;

    r.step(3, 3, format!("객체 {}개 복원", manifest.objects.len()));
    let secrets = engine::minio_admin_env(&v.engine, admin)?;
    let mut restored = 0u64;
    for object in &manifest.objects {
        c.check()?;
        let argv = minio::pipe_argv(
            x.docker_bin(),
            &v.engine,
            bucket,
            &object.key,
            object.content_type.as_deref(),
        );
        let mut chunk = (&mut source).take(object.size);
        let (out, bytes) = x.stream_in(&argv, &secrets, &mut chunk, c, r).await?;
        if bytes != object.size {
            return Err(truncated(
                "백업 파일이 잘려 있습니다",
                &format!(
                    "객체 `{}`에 {}바이트가 필요하지만 {}바이트만 남아 있습니다.",
                    object.key, object.size, bytes
                ),
            ));
        }
        if !out.ok() {
            return Err(x.failure(
                &argv,
                &out,
                &format!("객체 `{}` 복원에 실패했습니다", object.key),
                "엔진이 실행 중인지, 버킷에 쓸 수 있는지 확인한 뒤 다시 시도하세요.",
            ));
        }
        restored += bytes;
    }
    act.step(format!(
        "객체 {}개 · {} 복원",
        manifest.objects.len(),
        util::human_bytes(restored)
    ));
    r.step_done(3);
    Ok(())
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// BAK-009: re-hash the archive on disk and compare it with what was recorded
/// when it was written.
pub async fn verify(ctx: &Ctx, b: &BackupRecord) -> Result<bool> {
    let path = b.path();
    if !path.is_file() {
        return Err(Error::NotFound(format!(
            "백업 파일 `{}`을(를) 찾을 수 없습니다.",
            path.display()
        )));
    }
    let actual = sha256_file(&path).await?;
    let matches = !b.checksum.is_empty() && actual.eq_ignore_ascii_case(&b.checksum);

    let act = Activity::start(
        &ctx.store,
        ctx.origin,
        "bucket",
        "verify",
        format!(
            "`{}` 무결성 검증: {}",
            b.file_name,
            if matches { "일치" } else { "불일치" }
        ),
    )?
    .on_resource(&b.resource_id);
    act.ok();
    Ok(matches)
}

async fn sha256_file(path: &Path) -> Result<String> {
    let mut handle = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = handle.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{AuthType, EngineKind, TargetKind, TunnelSession, TunnelStatus};

    fn target(remote: bool) -> Target {
        Target {
            id: "tgt-1".into(),
            kind: if remote {
                TargetKind::Ssh
            } else {
                TargetKind::Local
            },
            display_name: if remote {
                "dev-vps".into()
            } else {
                "local".into()
            },
            host: remote.then(|| "dev-vps".to_string()),
            ssh_port: remote.then_some(22),
            ssh_username: remote.then(|| "deploy".to_string()),
            auth_type: remote.then_some(AuthType::Agent),
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: remote.then(|| "SHA256:abc".to_string()),
            created_at: Utc::now(),
            last_connected_at: None,
        }
    }

    fn engine_row() -> EngineInstance {
        EngineInstance {
            id: "eng-2".into(),
            target_id: "tgt-1".into(),
            engine: EngineKind::Minio,
            major_version: "latest".into(),
            image: "minio/minio:latest".into(),
            container_name: "linf-minio-latest".into(),
            volume_name: "linf-minio-latest-data".into(),
            bind_address: "127.0.0.1".into(),
            host_port: 9000,
            console_port: Some(9001),
            admin_user: "linf_admin".into(),
            credential_ref: "engine:eng-2".into(),
            managed: true,
            created_at: Utc::now(),
        }
    }

    fn bucket_row() -> ManagedBucket {
        ManagedBucket {
            id: "bkt-1".into(),
            engine_instance_id: "eng-2".into(),
            project_name: "Letsbid".into(),
            bucket_name: "letsbid-dev".into(),
            access_key: "AKIALETSBIDDEV000001".into(),
            credential_ref: "bucket:bkt-1".into(),
            preferred_local_tunnel_port: Some(19000),
            created_at: Utc::now(),
            last_connection_test_at: None,
            last_backup_at: None,
        }
    }

    fn view_of(remote: bool) -> BucketView {
        BucketView {
            bucket: bucket_row(),
            engine: engine_row(),
            target: target(remote),
            stats: BucketStats::default(),
            tunnel: None,
        }
    }

    fn tunnel(status: TunnelStatus) -> TunnelSession {
        TunnelSession {
            id: "tun-2".into(),
            resource_id: "bkt-1".into(),
            resource_kind: ResourceKind::Bucket,
            local_host: "127.0.0.1".into(),
            local_port: 19000,
            remote_host: "127.0.0.1".into(),
            remote_port: 9000,
            pid: Some(4242),
            pid_file_path: "/tmp/tunnel-tun-2.pid".into(),
            status,
            started_at: Utc::now(),
            stopped_at: None,
        }
    }

    fn manifest_of(objects: Vec<ObjectEntry>) -> Manifest {
        Manifest {
            bucket: "letsbid-dev".into(),
            created_at: DateTime::parse_from_rfc3339("2026-08-31T03:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            objects,
        }
    }

    fn object(key: &str, size: u64) -> ObjectEntry {
        ObjectEntry {
            key: key.into(),
            size,
            etag: Some("d41d8cd98f00b204e9800998ecf8427e".into()),
            content_type: None,
        }
    }

    // -- names --------------------------------------------------------------

    #[test]
    fn for_project_suggests_a_legal_bucket_name() {
        let s = CreateSpec::for_project("Letsbid");
        assert_eq!(s.bucket_name, "letsbid-dev");
        assert_eq!(s.region, "us-east-1");
        assert!(s.access_key.is_none(), "액세스 키는 생성 시점에 만든다");
        assert!(s.secret_key.is_none(), "시크릿 키는 생성 시점에 만든다");
    }

    #[test]
    fn messy_project_names_still_produce_legal_bucket_names() {
        for project in [
            "Letsbid",
            "Dalbit Editor",
            "my_project_name",
            "2024 Project",
            "달빛 에디터",
            "  ---  ",
            "",
            "A",
            &"z".repeat(120),
            "Ünïcødé Ñame",
        ] {
            let name = CreateSpec::for_project(project).bucket_name;
            assert!(
                minio::validate_bucket_name(&name).is_ok(),
                "`{project}` → `{name}` 은 적법한 버킷명이어야 한다"
            );
            assert!(!name.contains('_'), "`{name}`에 밑줄이 남아 있다");
        }
        assert_eq!(
            CreateSpec::for_project("Dalbit Editor").bucket_name,
            "dalbit-editor-dev"
        );
        assert_eq!(
            CreateSpec::for_project("my_project_name").bucket_name,
            "my-project-name-dev"
        );
        assert_eq!(
            CreateSpec::for_project("2024 Project").bucket_name,
            "2024-project-dev"
        );
        assert_eq!(CreateSpec::for_project("달빛 에디터").bucket_name, "dev");
    }

    #[test]
    fn validate_rejects_reserved_and_admin_names() {
        assert!(validate_new_names("letsbid-dev", "AKIALETSBID").is_ok());
        assert!(matches!(
            validate_new_names("minio", "AKIALETSBID"),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            validate_new_names("letsbid-dev", engine::ADMIN_USER),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn validate_rejects_illegal_names() {
        assert!(matches!(
            validate_new_names("Letsbid_Dev", "AKIALETSBID"),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            validate_new_names("letsbid-dev", "key"),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn secret_key_resolution_generates_or_validates() {
        let generated = resolve_secret_key(None).unwrap();
        assert_eq!(generated.len(), SECRET_KEY_LEN);
        assert_eq!(
            resolve_secret_key(Some("longenough")).unwrap(),
            "longenough"
        );
        assert!(matches!(
            resolve_secret_key(Some("   ")),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            resolve_secret_key(Some("short")),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            resolve_secret_key(Some("two\nlines-long")),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn access_key_resolution_generates_or_validates() {
        let generated = resolve_access_key(None).unwrap();
        assert!(minio::validate_access_key(&generated).is_ok());
        assert_eq!(
            resolve_access_key(Some("AKIALETSBID")).unwrap(),
            "AKIALETSBID"
        );
        assert!(matches!(
            resolve_access_key(Some("bad key")),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            resolve_access_key(Some(engine::ADMIN_USER)),
            Err(Error::Conflict(_))
        ));
    }

    // -- plans --------------------------------------------------------------

    #[test]
    fn create_plan_steps_match_the_mockup() {
        let spec = CreateSpec::for_project("Letsbid");
        let steps = create_steps(&spec, false);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, StepKind::New);
        assert_eq!(
            steps[0].title,
            "버킷 letsbid-dev 및 액세스 키 자동 생성 생성"
        );
        let detail = steps[0].detail.as_deref().unwrap();
        assert!(detail.contains("linf-letsbid-dev"), "{detail}");
        assert!(detail.contains("이 버킷에만 접근 허용"), "{detail}");
        assert_eq!(steps[1].kind, StepKind::Verify);
        assert_eq!(steps[1].title, "접속 테스트");
    }

    #[test]
    fn create_plan_warns_a_remote_target_needs_a_tunnel() {
        let spec = CreateSpec::for_project("Letsbid");
        let steps = create_steps(&spec, true);
        assert_eq!(steps.len(), 3);
        assert!(steps[2].title.contains("SSH 터널"));
    }

    #[test]
    fn drop_plan_keeps_the_engine_and_is_destructive() {
        let mut v = view_of(false);
        v.stats = BucketStats {
            objects: Some(42),
            size_bytes: Some(88_080_384),
        };
        let plan = drop_plan(&v, 0);
        assert!(plan.is_destructive());
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(plan.steps[3].kind, StepKind::Reuse);
        assert!(plan.steps[3].title.contains("linf-minio-latest"));
        assert!(plan.steps.iter().all(|s| !s.title.contains("볼륨")));
        let rendered = plan.render();
        assert!(rendered.contains("되돌릴 수 없습니다"), "{rendered}");
        assert!(rendered.contains("42개"), "{rendered}");
        assert!(rendered.contains("84.0 MB"), "{rendered}");
        assert!(rendered.contains("백업 기록이 없습니다"), "{rendered}");
        assert!(rendered.contains("linf-letsbid-dev"), "{rendered}");
    }

    #[test]
    fn drop_plan_mentions_existing_backups() {
        let plan = drop_plan(&view_of(false), 3);
        assert!(plan.render().contains("백업 기록 3건"));
    }

    #[test]
    fn backup_plan_names_the_archive_and_the_stream() {
        let v = view_of(true);
        let path = Path::new("/tmp/letsbid-dev-20260831-030000.objects");
        let plan = backup_plan(&v, path, 2);
        assert!(!plan.is_destructive());
        assert_eq!(plan.steps.len(), 4);
        let rendered = plan.render();
        assert!(rendered.contains("mc ls --recursive"), "{rendered}");
        assert!(rendered.contains("0600"), "{rendered}");
        assert!(
            rendered.contains("VPS에는 아무것도 남지 않습니다"),
            "{rendered}"
        );
        assert!(rendered.contains("기존 백업 기록 2건"), "{rendered}");
    }

    #[test]
    fn restore_plan_says_whether_data_is_lost() {
        let v = view_of(false);
        let file = Path::new("/tmp/letsbid-dev-20260831-030000.objects");
        let m = manifest_of(vec![object("a.png", 11), object("b.png", 3)]);

        let empty = restore_plan(file, 200, &m, &v, false, 0);
        assert!(!empty.is_destructive());
        assert!(empty.render().contains("비어 있습니다"));

        let refused = restore_plan(file, 200, &m, &v, false, 5);
        assert!(!refused.is_destructive());
        assert!(refused.render().contains("복원이 거부됩니다"));

        let overwriting = restore_plan(file, 200, &m, &v, true, 5);
        assert!(overwriting.is_destructive());
        let rendered = overwriting.render();
        assert!(rendered.contains("덮어쓰기"), "{rendered}");
        assert!(rendered.contains("객체 2개"), "{rendered}");
    }

    #[test]
    fn backup_file_names_are_stamped_and_extended() {
        let at = DateTime::parse_from_rfc3339("2026-08-31T03:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            backup_file_name("letsbid-dev", at),
            "letsbid-dev-20260831-030000.objects"
        );
    }

    // -- endpoints ----------------------------------------------------------

    #[test]
    fn local_endpoint_is_the_engine_port() {
        let v = view_of(false);
        assert_eq!(v.client_endpoint(), Some(("127.0.0.1".to_string(), 9000)));
    }

    #[test]
    fn remote_endpoint_requires_an_active_tunnel() {
        let mut v = view_of(true);
        assert_eq!(v.client_endpoint(), None);
        v.tunnel = Some(tunnel(TunnelStatus::Stopped));
        assert_eq!(v.client_endpoint(), None);
        v.tunnel = Some(tunnel(TunnelStatus::Active));
        assert_eq!(v.client_endpoint(), Some(("127.0.0.1".to_string(), 19000)));
    }

    // -- archive ------------------------------------------------------------

    #[test]
    fn manifest_round_trips() {
        let m = manifest_of(vec![object("uploads/a.png", 11), object("b.bin", 0)]);
        let bytes = write_manifest(&m).unwrap();
        assert!(bytes.starts_with(b"LINFBKT1\n"), "매직이 첫 줄이어야 한다");
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 2);
        assert_eq!(parse_manifest(&bytes).unwrap(), m);
        assert_eq!(m.body_bytes(), 11);
    }

    #[test]
    fn an_empty_bucket_round_trips_too() {
        let m = manifest_of(Vec::new());
        let bytes = write_manifest(&m).unwrap();
        let parsed = parse_manifest(&bytes).unwrap();
        assert_eq!(parsed, m);
        assert!(parsed.objects.is_empty());
        assert_eq!(parsed.body_bytes(), 0);
        assert!(check_body_length(&parsed, bytes.len() as u64, bytes.len() as u64).is_ok());
    }

    #[test]
    fn a_truncated_prefix_fails_loudly() {
        let m = manifest_of(vec![object("uploads/a.png", 11)]);
        let bytes = write_manifest(&m).unwrap();

        // Cut inside the manifest line: the JSON no longer parses.
        let cut = &bytes[..bytes.len() - 20];
        assert!(parse_manifest(cut).is_err());

        // Cut inside the magic line: there is no first newline at all.
        assert!(parse_manifest(b"LINFB").is_err());

        // Nothing at all.
        assert!(parse_manifest(b"").is_err());

        // A complete magic line but no manifest line terminator.
        assert!(parse_manifest(b"LINFBKT1\n{\"bucket\":\"x\"").is_err());
    }

    #[test]
    fn a_foreign_file_is_not_mistaken_for_an_archive() {
        let err = parse_manifest(b"PGDMP\nrest of a dump\n").unwrap_err();
        assert!(
            err.as_diagnostic()
                .what
                .contains("객체 백업 파일이 아닙니다"),
            "{err}"
        );
    }

    #[test]
    fn a_whole_archive_can_be_parsed_even_though_its_body_is_binary() {
        let m = manifest_of(vec![object("blob.bin", 4)]);
        let mut archive = write_manifest(&m).unwrap();
        // Bytes that are not valid UTF-8 and contain a newline of their own.
        archive.extend_from_slice(&[0xff, 0x0a, 0xfe, 0x00]);
        assert_eq!(parse_manifest(&archive).unwrap(), m);
    }

    #[test]
    fn a_binary_first_line_is_not_an_archive_either() {
        let err = parse_manifest(&[0xff, 0xfe, b'\n', b'{', b'}', b'\n']).unwrap_err();
        assert!(
            err.as_diagnostic()
                .what
                .contains("객체 백업 파일이 아닙니다"),
            "{err}"
        );
    }

    #[test]
    fn a_truncated_body_is_refused_before_anything_is_written() {
        let m = manifest_of(vec![object("a.png", 11), object("b.png", 3)]);
        let prefix = write_manifest(&m).unwrap().len() as u64;

        assert!(check_body_length(&m, prefix, prefix + 14).is_ok());
        // Longer than needed is fine; shorter is not.
        assert!(check_body_length(&m, prefix, prefix + 99).is_ok());
        let err = check_body_length(&m, prefix, prefix + 13).unwrap_err();
        let d = err.as_diagnostic();
        assert!(d.what.contains("잘려 있습니다"), "{d}");
        assert!(d.cause.contains("객체 2개"), "{d}");
        assert!(d.next.contains("복원하지 않습니다"), "{d}");
    }

    #[tokio::test]
    async fn read_prefix_leaves_the_reader_on_the_first_object_byte() {
        let m = manifest_of(vec![object("a.txt", 5), object("b.txt", 3)]);
        let mut archive = write_manifest(&m).unwrap();
        let prefix_len = archive.len() as u64;
        archive.extend_from_slice(b"helloabc");

        let mut reader = BufReader::new(std::io::Cursor::new(archive.clone()));
        let (parsed, consumed) = read_prefix(&mut reader).await.unwrap();
        assert_eq!(parsed, m);
        assert_eq!(consumed, prefix_len);
        assert!(check_body_length(&parsed, consumed, archive.len() as u64).is_ok());

        // Each object is read by size, in manifest order.
        let mut first = String::new();
        (&mut reader)
            .take(5)
            .read_to_string(&mut first)
            .await
            .unwrap();
        assert_eq!(first, "hello");
        let mut second = String::new();
        (&mut reader)
            .take(3)
            .read_to_string(&mut second)
            .await
            .unwrap();
        assert_eq!(second, "abc");
    }

    #[tokio::test]
    async fn read_prefix_rejects_an_archive_cut_mid_manifest() {
        let m = manifest_of(vec![object("a.txt", 5)]);
        let archive = write_manifest(&m).unwrap();
        let cut = archive[..archive.len() - 10].to_vec();
        let mut reader = BufReader::new(std::io::Cursor::new(cut));
        assert!(read_prefix(&mut reader).await.is_err());
    }
}
