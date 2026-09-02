//! Project database use cases (PRD §8.3, §9.1, §9.2).
//!
//! One project owns exactly one database and one login role on a *shared*
//! engine. Everything here is written so that a half-finished creation leaves
//! nothing behind: the server-side objects are rolled back and the attempt is
//! recorded as `rolled_back` in the activity log.

use crate::core::activity::Activity;
use crate::core::ctx::Ctx;
use crate::core::docker;
use crate::core::engine::{self, EngineSpec};
use crate::core::error::{Error, Result};
use crate::core::exec::Executor;
use crate::core::model::{
    ConnectionInfo, DatabaseStats, DatabaseView, EngineInstance, ManagedDatabase, Target,
};
use crate::core::pg::{self, DbSpec};
use crate::core::plan::{Plan, PlanStep, StepKind};
use crate::core::progress::{Cancel, Reporter};
use crate::core::secrets;
use crate::core::util;
use std::collections::HashMap;

/// Length of a generated project password.
const PASSWORD_LEN: usize = 24;

/// Seconds to wait for the engine to accept connections before giving up.
const READY_TIMEOUT_SECS: u64 = 60;

/// Databases every PostgreSQL cluster already owns.
const RESERVED_DATABASES: &[&str] = &["postgres", "template0", "template1"];

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// Everything the create form collects (PRD §7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSpec {
    pub project_name: String,
    pub database_name: String,
    pub username: String,
    /// `None` generates one; the CLI never accepts a password as an argument
    /// (PRD §11.2).
    pub password: Option<String>,
    pub encoding: String,
    pub locale: String,
    pub preferred_local_tunnel_port: Option<u16>,
}

impl CreateSpec {
    pub fn for_project(project: &str) -> Self {
        let (database_name, username) = util::suggest_names(project);
        Self {
            project_name: project.to_string(),
            database_name,
            username,
            password: None,
            encoding: "UTF8".into(),
            locale: "C".into(),
            preferred_local_tunnel_port: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub database: ManagedDatabase,
    pub engine: EngineInstance,
    pub connection: ConnectionInfo,
}

/// DB-003/DB-004 input rules, applied identically by the TUI form (live) and
/// the CLI (at parse time).
pub fn validate_new_names(db: &str, user: &str) -> Result<()> {
    util::validate_pg_identifier("DB명", db)?;
    util::validate_pg_identifier("사용자명", user)?;
    if RESERVED_DATABASES.contains(&db) {
        return Err(Error::Conflict(format!(
            "`{db}`은(는) PostgreSQL이 이미 사용하는 DB명입니다. 다른 이름을 사용하세요."
        )));
    }
    if user == engine::ADMIN_USER {
        return Err(Error::Conflict(format!(
            "`{user}`은(는) 엔진 관리자 계정 이름입니다. 다른 이름을 사용하세요."
        )));
    }
    Ok(())
}

/// The DB-side steps appended to the engine plan. Kept pure so the preview in
/// PRD §7.5 can be asserted without touching Docker.
fn create_steps(s: &CreateSpec, remote: bool) -> Vec<PlanStep> {
    let mut steps = vec![
        PlanStep::new(
            StepKind::New,
            format!("DB {} 및 계정 {} 생성", s.database_name, s.username),
        )
        .with_detail(format!(
            "소유자 {} · 인코딩 {} · 로케일 {} · PUBLIC 권한 회수",
            s.username, s.encoding, s.locale
        )),
        PlanStep::new(StepKind::Verify, "접속 테스트").with_detail(format!(
            "컨테이너 안에서 {}(으)로 실제 로그인합니다.",
            s.username
        )),
    ];
    if remote {
        steps.push(
            PlanStep::new(StepKind::Verify, "로컬 접속에는 SSH 터널이 필요합니다").with_detail(
                "생성 후 `linf tunnel start`로 터널을 시작하면 접속 URL이 활성화됩니다.",
            ),
        );
    }
    steps
}

/// Full preview: the engine plan (new or reused) followed by the DB steps, so
/// what the user reads matches what `create` does end to end.
pub async fn plan_create(ctx: &Ctx, t: &Target, es: &EngineSpec, s: &CreateSpec) -> Result<Plan> {
    validate_new_names(&s.database_name, &s.username)?;
    let mut plan = engine::plan_ensure(ctx, t, es).await?;
    plan.title = format!("`{}` DB 생성", s.database_name);
    for step in create_steps(s, t.is_remote()) {
        plan.push(step);
    }
    Ok(plan)
}

/// PRD §9.1/§9.2. Idempotent on the engine, strictly non-idempotent on the
/// database: a name that already exists anywhere is a conflict, never a
/// silent reuse (DB-004).
pub async fn create(
    ctx: &Ctx,
    t: &Target,
    es: &EngineSpec,
    s: &CreateSpec,
    r: &Reporter,
    c: &Cancel,
) -> Result<Created> {
    ctx.require_write_lock()?;
    validate_new_names(&s.database_name, &s.username)?;
    let password = resolve_password(s.password.as_deref())?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "database",
        "create",
        format!(
            "`{}` DB와 `{}` 계정을 {}에 생성",
            s.database_name, s.username, t.display_name
        ),
    )?
    .on_target(&t.id);

    let mut rollback: Option<String> = None;
    let result = create_inner(ctx, t, es, s, &password, r, c, &mut act, &mut rollback).await;
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
    password: &str,
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

    reject_duplicates(ctx, &x, &engine_instance, &s.database_name, &s.username).await?;
    act.step("이름 중복 확인");
    c.check()?;

    let id = util::new_id();
    let credential_ref = secrets::database_ref(&id);
    ctx.secrets.set(&credential_ref, password)?;

    let spec = DbSpec {
        database: s.database_name.clone(),
        owner: s.username.clone(),
        encoding: s.encoding.clone(),
        locale: s.locale.clone(),
    };

    r.step(1, 3, format!("DB {} 및 계정 생성", s.database_name));
    if let Err(e) = pg::create_database_and_role(&x, &engine_instance, &spec, password).await {
        *rollback = Some(
            undo(
                ctx,
                &x,
                &engine_instance,
                &spec.database,
                &spec.owner,
                &credential_ref,
            )
            .await,
        );
        return Err(e);
    }
    act.step(format!("DB {} 및 계정 {} 생성", spec.database, spec.owner));
    r.step_done(1);

    r.step(2, 3, "접속 테스트");
    if let Err(e) =
        pg::verify_login(&x, &engine_instance, &spec.database, &spec.owner, password).await
    {
        *rollback = Some(
            undo(
                ctx,
                &x,
                &engine_instance,
                &spec.database,
                &spec.owner,
                &credential_ref,
            )
            .await,
        );
        return Err(e);
    }
    act.step("접속 테스트 성공");
    r.step_done(2);

    r.step(3, 3, "메타데이터 저장");
    let row = ManagedDatabase {
        id,
        engine_instance_id: engine_instance.id.clone(),
        project_name: s.project_name.clone(),
        database_name: spec.database.clone(),
        username: spec.owner.clone(),
        credential_ref: credential_ref.clone(),
        preferred_local_tunnel_port: s.preferred_local_tunnel_port,
        created_at: util::now(),
        last_connection_test_at: Some(util::now()),
        last_backup_at: None,
    };
    if let Err(e) = ctx.store.insert_database(&row) {
        *rollback = Some(
            undo(
                ctx,
                &x,
                &engine_instance,
                &spec.database,
                &spec.owner,
                &credential_ref,
            )
            .await,
        );
        return Err(e);
    }
    r.step_done(3);

    let view = DatabaseView {
        database: row.clone(),
        engine: engine_instance.clone(),
        target: t.clone(),
        stats: DatabaseStats::default(),
        tunnel: None,
    };
    Ok(Created {
        connection: connection_with(ctx, &view, Some(password.to_string())),
        database: row,
        engine: engine_instance,
    })
}

fn resolve_password(given: Option<&str>) -> Result<String> {
    match given {
        None => Ok(util::generate_password(PASSWORD_LEN)),
        Some(p) if p.trim().is_empty() => Err(Error::Usage(
            "비밀번호가 비어 있습니다. 값을 입력하거나 자동 생성을 사용하세요.".into(),
        )),
        Some(p) if p.contains('\n') || p.contains('\r') => Err(Error::Usage(
            "비밀번호에는 줄바꿈을 포함할 수 없습니다.".into(),
        )),
        Some(p) => Ok(p.to_string()),
    }
}

/// DB-004 both ways: the local registry *and* the live server must be free of
/// the name, because the engine may be shared with databases this app never
/// created.
async fn reject_duplicates(
    ctx: &Ctx,
    x: &Executor,
    e: &EngineInstance,
    database: &str,
    username: &str,
) -> Result<()> {
    if ctx
        .store
        .find_database_on_engine(&e.id, database)?
        .is_some()
    {
        return Err(Error::Conflict(format!(
            "`{database}` DB는 이미 이 엔진에 등록되어 있습니다. 다른 이름을 사용하세요."
        )));
    }
    if ctx
        .store
        .list_databases_for_engine(&e.id)?
        .iter()
        .any(|d| d.username == username)
    {
        return Err(Error::Conflict(format!(
            "`{username}` 계정은 이미 이 엔진에 등록되어 있습니다. 다른 이름을 사용하세요."
        )));
    }
    if pg::database_exists(x, e, database).await? {
        return Err(Error::Conflict(format!(
            "엔진 `{}`에 이미 `{database}` DB가 있습니다. 다른 이름을 사용하거나 \
             기존 DB를 `linf backup run`으로 옮긴 뒤 다시 시도하세요.",
            e.container_name
        )));
    }
    if pg::role_exists(x, e, username).await? {
        return Err(Error::Conflict(format!(
            "엔진 `{}`에 이미 `{username}` 계정이 있습니다. 다른 이름을 사용하세요.",
            e.container_name
        )));
    }
    Ok(())
}

/// Remove whatever the failed attempt managed to create. Best effort by
/// design: the original error is what the user needs to see, so a failure to
/// clean up is folded into the returned reason instead of replacing it.
async fn undo(
    ctx: &Ctx,
    x: &Executor,
    e: &EngineInstance,
    database: &str,
    role: &str,
    credential_ref: &str,
) -> String {
    let _ = ctx.secrets.delete(credential_ref);
    match pg::drop_database_and_role(x, e, database, role).await {
        Ok(()) => format!("생성 실패로 DB `{database}`와 계정 `{role}`을(를) 정리했습니다"),
        Err(e) => format!(
            "생성 실패 후 정리도 실패했습니다: {}. `{database}`/`{role}`이 남아 있는지 확인하세요",
            e.as_diagnostic().what
        ),
    }
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Every registered database with its engine, target and latest tunnel.
/// `with_stats` adds size and connection counts; an unreachable target yields
/// empty statistics rather than an error (PRD §7.6).
pub async fn views(ctx: &Ctx, with_stats: bool) -> Result<Vec<DatabaseView>> {
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

    // One status probe per engine, not per database.
    let mut reachable: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    for database in ctx.store.list_databases()? {
        let Some(engine_row) = engines.get(&database.engine_instance_id) else {
            continue;
        };
        let Some(target) = targets.get(&engine_row.target_id) else {
            continue;
        };
        let tunnel = ctx.store.latest_tunnel(&database.id)?;
        let mut view = DatabaseView {
            database,
            engine: engine_row.clone(),
            target: target.clone(),
            stats: DatabaseStats::default(),
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

/// One database by id or by name, with statistics filled in when the engine is
/// reachable.
pub async fn view(ctx: &Ctx, key: &str) -> Result<DatabaseView> {
    let database = ctx.store.require_database(key)?;
    let engine_row = ctx.store.get_engine(&database.engine_instance_id)?;
    let target = ctx.store.require_target(&engine_row.target_id)?;
    let tunnel = ctx.store.latest_tunnel(&database.id)?;
    let mut view = DatabaseView {
        database,
        engine: engine_row,
        target,
        stats: DatabaseStats::default(),
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

async fn read_stats(ctx: &Ctx, v: &DatabaseView) -> DatabaseStats {
    let Ok(x) = ctx.executor(&v.target) else {
        return DatabaseStats::default();
    };
    DatabaseStats {
        size_bytes: pg::database_size_bytes(&x, &v.engine, &v.database.database_name)
            .await
            .unwrap_or(None),
        connections: pg::connection_count(&x, &v.engine, &v.database.database_name)
            .await
            .unwrap_or(None),
    }
}

/// DB-006. For a remote target this is the *tunnel* endpoint; without a live
/// tunnel there is no address a local client could use, so the call is refused
/// with the action that fixes it (TUN-006).
pub fn connection_info(ctx: &Ctx, v: &DatabaseView) -> Result<ConnectionInfo> {
    connection_parts(v, ctx.secrets.get(&v.database.credential_ref)?)
}

/// Same as [`connection_info`] but never reads a secret — for painting the UI.
pub fn connection_preview(v: &DatabaseView) -> Result<ConnectionInfo> {
    connection_parts(v, None)
}

fn connection_parts(v: &DatabaseView, password: Option<String>) -> Result<ConnectionInfo> {
    let (host, port) = v.client_endpoint().ok_or_else(|| {
        Error::Refused(format!(
            "`{}`은(는) 원격 Target `{}`에 있어 SSH 터널이 필요합니다. \
             `linf tunnel start {}`로 터널을 먼저 시작하세요.",
            v.database.database_name, v.target.display_name, v.database.database_name
        ))
    })?;
    Ok(ConnectionInfo {
        host,
        port,
        database: v.database.database_name.clone(),
        username: v.database.username.clone(),
        password,
    })
}

/// Endpoint used when reporting the *result* of a mutation. A remote database
/// has no live tunnel the instant it is created, so the reserved local port is
/// reported instead of refusing to answer.
fn connection_with(ctx: &Ctx, v: &DatabaseView, password: Option<String>) -> ConnectionInfo {
    let (host, port) = v.client_endpoint().unwrap_or_else(|| {
        (
            "127.0.0.1".to_string(),
            v.database
                .preferred_local_tunnel_port
                .unwrap_or(v.engine.host_port),
        )
    });
    ConnectionInfo {
        host,
        port,
        database: v.database.database_name.clone(),
        username: v.database.username.clone(),
        password: password.or_else(|| ctx.secrets.get(&v.database.credential_ref).ok().flatten()),
    }
}

/// DB-005 on demand: a real login as the project role, then a timestamp.
pub async fn test_connection(ctx: &Ctx, v: &DatabaseView) -> Result<()> {
    let password = require_password(ctx, v)?;
    let x = ctx.executor(&v.target)?;
    pg::verify_login(
        &x,
        &v.engine,
        &v.database.database_name,
        &v.database.username,
        &password,
    )
    .await?;
    let mut row = v.database.clone();
    row.last_connection_test_at = Some(util::now());
    ctx.store.update_database(&row)?;
    Ok(())
}

fn require_password(ctx: &Ctx, v: &DatabaseView) -> Result<String> {
    ctx.secrets.get(&v.database.credential_ref)?.ok_or_else(|| {
        Error::Refused(format!(
            "`{}`의 비밀번호가 저장되어 있지 않습니다. \
                 `linf db rotate-password {}`로 새 비밀번호를 발급하세요.",
            v.database.database_name, v.database.database_name
        ))
    })
}

// ---------------------------------------------------------------------------
// Drop / forget
// ---------------------------------------------------------------------------

/// DB-008: this plan never contains the engine or the volume.
fn drop_plan(v: &DatabaseView, backups: usize) -> Plan {
    let mut plan = Plan::new(format!("`{}` DB 삭제", v.database.database_name))
        .step_detailed(
            StepKind::Destroy,
            format!("DB {} 삭제", v.database.database_name),
            format!(
                "Target {} · 엔진 {}",
                v.target.display_name, v.engine.container_name
            ),
        )
        .step(
            StepKind::Destroy,
            format!("계정 {} 삭제", v.database.username),
        )
        .step(StepKind::Destroy, "저장된 비밀번호 삭제")
        .step_detailed(
            StepKind::Reuse,
            format!("엔진 {} 유지", v.engine.container_name),
            "같은 엔진의 다른 DB는 영향을 받지 않습니다.",
        )
        .warn("이 작업은 되돌릴 수 없습니다.");
    if let Some(size) = v.stats.size_bytes {
        plan = plan.warn(format!(
            "삭제되는 데이터 크기: 약 {}",
            util::human_bytes(size.max(0) as u64)
        ));
    }
    if let Some(connections) = v.stats.connections {
        if connections > 0 {
            plan = plan.warn(format!(
                "현재 {connections}개의 연결이 열려 있습니다. 삭제 시 강제로 종료됩니다."
            ));
        }
    }
    if v.tunnel.is_some() {
        plan = plan.warn("이 DB의 터널 기록도 함께 삭제됩니다. 실행 중이면 먼저 중지하세요.");
    }
    plan = plan.warn(match backups {
        0 => "백업 기록이 없습니다. 먼저 `linf backup run`을 실행하는 것을 권장합니다.".to_string(),
        n => format!("백업 기록 {n}건이 목록에서 제거됩니다(파일 자체는 남습니다)."),
    });
    plan
}

pub async fn plan_drop(ctx: &Ctx, v: &DatabaseView) -> Result<Plan> {
    let backups = ctx.store.list_backups(Some(&v.database.id))?.len();
    Ok(drop_plan(v, backups))
}

/// DB-008. Removes one project's database and role, nothing else.
pub async fn drop(ctx: &Ctx, v: &DatabaseView, r: &Reporter) -> Result<()> {
    ctx.require_write_lock()?;
    let x = ctx.executor(&v.target)?;
    docker::require_managed(&x, &v.engine.container_name).await?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "database",
        "drop",
        format!(
            "`{}` DB와 `{}` 계정을 삭제",
            v.database.database_name, v.database.username
        ),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.database.id);

    let result = drop_inner(ctx, &x, v, r, &mut act).await;
    act.finish(&result);
    result
}

async fn drop_inner(
    ctx: &Ctx,
    x: &Executor,
    v: &DatabaseView,
    r: &Reporter,
    act: &mut Activity<'_>,
) -> Result<()> {
    crate::core::tunnel::stop_for_resource(ctx, &v.database.id).await?;
    act.step("관련 SSH 터널을 정리했습니다");
    r.step(1, 3, format!("DB {} 삭제", v.database.database_name));
    pg::drop_database_and_role(
        x,
        &v.engine,
        &v.database.database_name,
        &v.database.username,
    )
    .await?;
    act.step(format!(
        "DB {} 및 계정 {} 삭제",
        v.database.database_name, v.database.username
    ));
    r.step_done(1);

    r.step(2, 3, "메타데이터 삭제");
    ctx.store.delete_database(&v.database.id)?;
    act.step("등록 정보 삭제");
    r.step_done(2);

    r.step(3, 3, "비밀번호 삭제");
    ctx.secrets.delete(&v.database.credential_ref)?;
    r.step_done(3);
    Ok(())
}

/// DB-007: unregister only. The server keeps the database and the role exactly
/// as they are.
pub fn forget(ctx: &Ctx, v: &DatabaseView) -> Result<()> {
    ctx.require_write_lock()?;
    let act = Activity::start(
        &ctx.store,
        ctx.origin,
        "database",
        "forget",
        format!(
            "`{}` 등록 해제 (서버의 DB는 유지)",
            v.database.database_name
        ),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.database.id);

    let result = (|| -> Result<()> {
        ctx.store.delete_database(&v.database.id)?;
        ctx.secrets.delete(&v.database.credential_ref)?;
        Ok(())
    })();
    act.finish(&result);
    result
}

// ---------------------------------------------------------------------------
// Rotate / duplicate
// ---------------------------------------------------------------------------

/// DB-009. The server is changed first: a stored secret that the server does
/// not accept is worse than a rotation that failed outright.
pub async fn rotate_password(ctx: &Ctx, v: &DatabaseView) -> Result<ConnectionInfo> {
    ctx.require_write_lock()?;
    let x = ctx.executor(&v.target)?;
    docker::require_managed(&x, &v.engine.container_name).await?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "database",
        "rotate-password",
        format!("`{}` 계정 비밀번호 교체", v.database.username),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.database.id);

    let previous = ctx.secrets.get(&v.database.credential_ref).unwrap_or(None);
    let password = util::generate_password(PASSWORD_LEN);
    let mut rollback: Option<String> = None;
    let result = rotate_inner(
        ctx,
        &x,
        v,
        &password,
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

async fn rotate_inner(
    ctx: &Ctx,
    x: &Executor,
    v: &DatabaseView,
    password: &str,
    previous: Option<&str>,
    act: &mut Activity<'_>,
    rollback: &mut Option<String>,
) -> Result<ConnectionInfo> {
    pg::set_role_password(x, &v.engine, &v.database.username, password).await?;
    act.step("서버 비밀번호 변경");

    if let Err(e) = ctx.secrets.set(&v.database.credential_ref, password) {
        // The new password exists only on the server and nowhere else; put the
        // old one back so the user is not locked out.
        *rollback = match previous {
            Some(old) => match pg::set_role_password(x, &v.engine, &v.database.username, old).await
            {
                Ok(()) => Some("비밀번호 저장에 실패해 이전 비밀번호로 되돌렸습니다".into()),
                Err(_) => Some(
                    "비밀번호 저장과 되돌리기가 모두 실패했습니다. \
                     `linf db rotate-password`를 다시 실행하세요"
                        .into(),
                ),
            },
            None => {
                Some("비밀번호 저장에 실패했습니다. 저장 모드를 확인한 뒤 다시 실행하세요".into())
            }
        };
        return Err(e);
    }
    act.step("새 비밀번호 저장");

    pg::verify_login(
        x,
        &v.engine,
        &v.database.database_name,
        &v.database.username,
        password,
    )
    .await?;
    act.step("접속 테스트 성공");
    Ok(connection_with(ctx, v, Some(password.to_string())))
}

/// `letsbid_copy_dev` → `letsbid_copy_user`. Called only after the database
/// name itself has been validated, so byte slicing is safe.
fn derive_username(database: &str) -> String {
    let stem = database.strip_suffix("_dev").unwrap_or(database);
    let mut stem = stem.to_string();
    // `_user` is 5 bytes and an identifier is capped at 63.
    stem.truncate(58);
    while stem.ends_with('_') {
        stem.pop();
    }
    format!("{stem}_user")
}

/// DB-010. `CREATE DATABASE … TEMPLATE` is a physical copy, so the copy gets a
/// role of its own and every copied object is re-homed onto it — otherwise the
/// two projects would share an owner.
pub async fn duplicate(
    ctx: &Ctx,
    v: &DatabaseView,
    new_name: &str,
    r: &Reporter,
) -> Result<Created> {
    ctx.require_write_lock()?;
    util::validate_pg_identifier("DB명", new_name)?;
    let username = derive_username(new_name);
    validate_new_names(new_name, &username)?;

    let x = ctx.executor(&v.target)?;
    docker::require_managed(&x, &v.engine.container_name).await?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "database",
        "duplicate",
        format!(
            "`{}`을(를) `{new_name}`(으)로 복제",
            v.database.database_name
        ),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.database.id);

    let mut rollback: Option<String> = None;
    let result = duplicate_inner(ctx, &x, v, new_name, &username, r, &mut act, &mut rollback).await;
    match (&result, rollback) {
        (Err(_), Some(reason)) => act.rolled_back(reason),
        _ => act.finish(&result),
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn duplicate_inner(
    ctx: &Ctx,
    x: &Executor,
    v: &DatabaseView,
    new_name: &str,
    username: &str,
    r: &Reporter,
    act: &mut Activity<'_>,
    rollback: &mut Option<String>,
) -> Result<Created> {
    reject_duplicates(ctx, x, &v.engine, new_name, username).await?;

    let id = util::new_id();
    let credential_ref = secrets::database_ref(&id);
    let password = util::generate_password(PASSWORD_LEN);
    ctx.secrets.set(&credential_ref, &password)?;

    r.step(1, 4, format!("계정 {username} 생성"));
    if let Err(e) = pg::create_role(x, &v.engine, username, &password).await {
        *rollback = Some(undo(ctx, x, &v.engine, new_name, username, &credential_ref).await);
        return Err(e);
    }
    r.step_done(1);

    r.step(2, 4, format!("DB {new_name} 복제"));
    if let Err(e) = pg::create_database_from_template(
        x,
        &v.engine,
        new_name,
        username,
        &v.database.database_name,
    )
    .await
    {
        *rollback = Some(undo(ctx, x, &v.engine, new_name, username, &credential_ref).await);
        return Err(e);
    }
    act.step(format!("{} → {new_name} 복제", v.database.database_name));
    r.step_done(2);

    r.step(3, 4, "소유권 이전");
    if let Err(e) = pg::take_ownership(x, &v.engine, new_name, username).await {
        *rollback = Some(undo(ctx, x, &v.engine, new_name, username, &credential_ref).await);
        return Err(e);
    }
    r.step_done(3);

    r.step(4, 4, "접속 테스트");
    if let Err(e) = pg::verify_login(x, &v.engine, new_name, username, &password).await {
        *rollback = Some(undo(ctx, x, &v.engine, new_name, username, &credential_ref).await);
        return Err(e);
    }
    r.step_done(4);

    let row = ManagedDatabase {
        id,
        engine_instance_id: v.engine.id.clone(),
        project_name: format!("{} (복제)", v.database.project_name),
        database_name: new_name.to_string(),
        username: username.to_string(),
        credential_ref: credential_ref.clone(),
        preferred_local_tunnel_port: None,
        created_at: util::now(),
        last_connection_test_at: Some(util::now()),
        last_backup_at: None,
    };
    if let Err(e) = ctx.store.insert_database(&row) {
        *rollback = Some(undo(ctx, x, &v.engine, new_name, username, &credential_ref).await);
        return Err(e);
    }

    let copy_view = DatabaseView {
        database: row.clone(),
        engine: v.engine.clone(),
        target: v.target.clone(),
        stats: DatabaseStats::default(),
        tunnel: None,
    };
    Ok(Created {
        connection: connection_with(ctx, &copy_view, Some(password)),
        database: row,
        engine: v.engine.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::ResourceKind;
    use crate::core::model::{AuthType, EngineKind, TargetKind, TunnelSession, TunnelStatus};
    use chrono::Utc;

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
            id: "eng-1".into(),
            target_id: "tgt-1".into(),
            engine: EngineKind::Postgres,
            major_version: "17".into(),
            image: "postgres:17".into(),
            container_name: "linf-postgres-17".into(),
            volume_name: "linf-pg17-data".into(),
            bind_address: "127.0.0.1".into(),
            host_port: 5432,
            console_port: None,
            admin_user: "linf_admin".into(),
            credential_ref: "engine:eng-1".into(),
            managed: true,
            created_at: Utc::now(),
        }
    }

    fn database_row() -> ManagedDatabase {
        ManagedDatabase {
            id: "db-1".into(),
            engine_instance_id: "eng-1".into(),
            project_name: "Letsbid".into(),
            database_name: "letsbid_dev".into(),
            username: "letsbid_user".into(),
            credential_ref: "database:db-1".into(),
            preferred_local_tunnel_port: Some(15432),
            created_at: Utc::now(),
            last_connection_test_at: None,
            last_backup_at: None,
        }
    }

    fn view_of(remote: bool) -> DatabaseView {
        DatabaseView {
            database: database_row(),
            engine: engine_row(),
            target: target(remote),
            stats: DatabaseStats::default(),
            tunnel: None,
        }
    }

    fn tunnel(status: TunnelStatus) -> TunnelSession {
        TunnelSession {
            id: "tun-1".into(),
            resource_id: "db-1".into(),
            resource_kind: ResourceKind::Database,
            local_host: "127.0.0.1".into(),
            local_port: 15432,
            remote_host: "127.0.0.1".into(),
            remote_port: 5432,
            pid: Some(4242),
            pid_file_path: "/tmp/tunnel-tun-1.pid".into(),
            status,
            started_at: Utc::now(),
            stopped_at: None,
        }
    }

    #[test]
    fn for_project_suggests_names() {
        let s = CreateSpec::for_project("Letsbid");
        assert_eq!(s.database_name, "letsbid_dev");
        assert_eq!(s.username, "letsbid_user");
        assert_eq!(s.encoding, "UTF8");
        assert_eq!(s.locale, "C");
        assert!(s.password.is_none(), "비밀번호는 생성 시점에 만든다");
    }

    #[test]
    fn validate_rejects_reserved_and_admin_names() {
        assert!(validate_new_names("letsbid_dev", "letsbid_user").is_ok());
        assert!(matches!(
            validate_new_names("postgres", "letsbid_user"),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            validate_new_names("template1", "letsbid_user"),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            validate_new_names("letsbid_dev", engine::ADMIN_USER),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn validate_rejects_illegal_identifiers() {
        assert!(matches!(
            validate_new_names("Letsbid-Dev", "letsbid_user"),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            validate_new_names("letsbid_dev", "pg_user"),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            validate_new_names("select", "letsbid_user"),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn create_plan_steps_match_the_mockup() {
        let spec = CreateSpec::for_project("Letsbid");
        let steps = create_steps(&spec, false);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, StepKind::New);
        assert_eq!(steps[0].title, "DB letsbid_dev 및 계정 letsbid_user 생성");
        assert!(steps[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("PUBLIC 권한 회수"));
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
        v.stats = DatabaseStats {
            size_bytes: Some(88_080_384),
            connections: Some(2),
        };
        let plan = drop_plan(&v, 0);
        assert!(plan.is_destructive());
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(plan.steps[3].kind, StepKind::Reuse);
        assert!(plan.steps[3].title.contains("linf-postgres-17"));
        assert!(plan.steps.iter().all(|s| !s.title.contains("볼륨")));
        let rendered = plan.render();
        assert!(rendered.contains("되돌릴 수 없습니다"), "{rendered}");
        assert!(rendered.contains("84.0 MB"), "{rendered}");
        assert!(rendered.contains("2개의 연결"), "{rendered}");
        assert!(rendered.contains("백업 기록이 없습니다"), "{rendered}");
    }

    #[test]
    fn drop_plan_mentions_existing_backups() {
        let plan = drop_plan(&view_of(false), 3);
        assert!(plan.render().contains("백업 기록 3건"));
    }

    #[test]
    fn local_endpoint_is_the_engine_port() {
        let v = view_of(false);
        assert_eq!(v.client_endpoint(), Some(("127.0.0.1".to_string(), 5432)));
    }

    #[test]
    fn remote_endpoint_requires_an_active_tunnel() {
        let mut v = view_of(true);
        assert_eq!(v.client_endpoint(), None);
        v.tunnel = Some(tunnel(TunnelStatus::Stopped));
        assert_eq!(v.client_endpoint(), None);
        v.tunnel = Some(tunnel(TunnelStatus::Active));
        assert_eq!(v.client_endpoint(), Some(("127.0.0.1".to_string(), 15432)));
    }

    #[test]
    fn derives_a_username_from_the_copy_name() {
        assert_eq!(derive_username("letsbid_copy_dev"), "letsbid_copy_user");
        assert_eq!(derive_username("staging"), "staging_user");
        let long = format!("{}_dev", "a".repeat(60));
        let derived = derive_username(&long);
        assert!(derived.len() <= 63, "{derived}");
        assert!(derived.ends_with("_user"));
    }

    #[test]
    fn password_resolution_generates_or_validates() {
        let generated = resolve_password(None).unwrap();
        assert_eq!(generated.len(), PASSWORD_LEN);
        assert_eq!(resolve_password(Some("given")).unwrap(), "given");
        assert!(matches!(resolve_password(Some("  ")), Err(Error::Usage(_))));
        assert!(matches!(
            resolve_password(Some("two\nlines")),
            Err(Error::Usage(_))
        ));
    }
}
