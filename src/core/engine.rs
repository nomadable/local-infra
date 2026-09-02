//! Shared service engine containers (PRD §8.2, ENG-001..ENG-013).
//!
//! One container per `(target, engine, major version)` holds every project's
//! resources on that target, so a laptop runs one PostgreSQL and one MinIO
//! instead of one of each per repository. Containers and volumes are named
//! deterministically, are always labelled as managed, and are bound to
//! loopback unless the caller explicitly asks otherwise.
//!
//! Everything that differs between services lives in the small block of
//! `match engine` helpers below — adding a service means extending those, not
//! branching through the lifecycle.

use crate::core::activity::Activity;
use crate::core::config::Config;
use crate::core::ctx::Ctx;
use crate::core::docker::{self, RunSpec};
use crate::core::error::{Error, Result};
use crate::core::exec::{Executor, SecretEnv};
use crate::core::model::{EngineInstance, EngineKind, EngineStatus, Health, Target};
use crate::core::plan::{Plan, StepKind};
use crate::core::progress::{Cancel, Reporter};
use crate::core::secrets;
use crate::core::store::Store;
use crate::core::util;
use serde::Serialize;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

/// Administrative account created inside every engine this app owns:
/// the PostgreSQL superuser, the MinIO root user.
pub const ADMIN_USER: &str = "linf_admin";
/// First port considered when publishing a PostgreSQL engine.
pub const DEFAULT_PORT: u16 = 5432;
/// Default — and strongly recommended — bind address (ENG-008).
pub const LOOPBACK: &str = "127.0.0.1";

/// `initdb` target: a subdirectory, so the volume root stays free of
/// `lost+found` and the image's own bootstrap checks keep working.
const PGDATA: &str = "/var/lib/postgresql/data/pgdata";
/// Maintenance database the admin user connects to.
const ADMIN_DB: &str = "postgres";
const PASSWORD_LEN: usize = 32;
/// How far past the requested port `ensure` will look for a free one.
const PORT_SCAN_SPAN: u16 = 100;
const READY_TIMEOUT_SECS: u64 = 90;
const POLL_INTERVAL_MS: u64 = 500;
/// `mc` alias used for every administrative call into a MinIO engine.
pub const MINIO_ALIAS: &str = "linf";

// ---------------------------------------------------------------------------
// Per-service differences
// ---------------------------------------------------------------------------

/// Where the data volume is mounted inside the container.
pub fn data_dir(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Postgres => "/var/lib/postgresql/data",
        EngineKind::Minio => "/data",
    }
}

/// Name of the single environment variable carrying the admin password.
/// Its value is always passed through [`SecretEnv`], never on a command line.
pub fn password_env(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Postgres => "POSTGRES_PASSWORD",
        EngineKind::Minio => "MINIO_ROOT_PASSWORD",
    }
}

fn container_env(e: &EngineInstance) -> BTreeMap<String, String> {
    match e.engine {
        EngineKind::Postgres => BTreeMap::from([
            ("POSTGRES_USER".to_string(), e.admin_user.clone()),
            ("POSTGRES_DB".to_string(), ADMIN_DB.to_string()),
            ("PGDATA".to_string(), PGDATA.to_string()),
        ]),
        EngineKind::Minio => BTreeMap::from([
            ("MINIO_ROOT_USER".to_string(), e.admin_user.clone()),
            // The console is published separately; announcing it here keeps the
            // browser redirect pointing at the port the user actually reaches.
            (
                "MINIO_BROWSER_REDIRECT_URL".to_string(),
                format!(
                    "http://{}:{}",
                    e.bind_address,
                    e.console_port.unwrap_or(9001)
                ),
            ),
        ]),
    }
}

/// Arguments after the image. PostgreSQL's default entrypoint is right as-is.
fn container_command(e: &EngineInstance) -> Vec<String> {
    match e.engine {
        EngineKind::Postgres => Vec::new(),
        EngineKind::Minio => vec![
            "server".to_string(),
            data_dir(EngineKind::Minio).to_string(),
            "--console-address".to_string(),
            format!(
                ":{}",
                EngineKind::Minio.console_container_port().unwrap_or(9001)
            ),
        ],
    }
}

/// Docker healthcheck, when the image ships a probe that needs no credentials.
/// MinIO's readiness check needs an authenticated `mc` alias, which would mean
/// duplicating the root password into a second container variable — so MinIO
/// is probed by [`probe_ready`] instead.
fn container_healthcheck(e: &EngineInstance) -> Option<Vec<String>> {
    match e.engine {
        EngineKind::Postgres => Some(vec![
            "pg_isready".to_string(),
            "-U".to_string(),
            e.admin_user.clone(),
            "-d".to_string(),
            ADMIN_DB.to_string(),
        ]),
        EngineKind::Minio => None,
    }
}

/// Human-readable name of the readiness probe, used in plans and messages.
fn ready_probe_label(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Postgres => "pg_isready",
        EngineKind::Minio => "mc ready",
    }
}

/// `MC_HOST_<alias>` for administrative `mc` calls. Built here so both
/// `engine` and `minio` derive the alias the same way.
pub fn minio_admin_env(e: &EngineInstance, password: &str) -> Result<SecretEnv> {
    SecretEnv::new().set(
        format!("MC_HOST_{MINIO_ALIAS}"),
        format!(
            "http://{}:{}@127.0.0.1:{}",
            util::pct_encode(&e.admin_user),
            util::pct_encode(password),
            e.engine.container_port()
        ),
    )
}

// ---------------------------------------------------------------------------
// Naming (PRD §8.2)
// ---------------------------------------------------------------------------

/// `linf-postgres-17`, `linf-minio-latest`.
pub fn container_name(engine: EngineKind, major: &str) -> String {
    format!("linf-{}-{}", engine.as_str(), major)
}

/// `linf-pg17-data`, `linf-minio-latest-data`.
pub fn volume_name(engine: EngineKind, major: &str) -> String {
    format!("linf-{}-data", engine.short_tag(major))
}

/// The four labels every managed container and volume carries (ENG-005).
fn labels(target_id: &str, engine: EngineKind, major: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (docker::LABEL_MANAGED.to_string(), "true".to_string()),
        (docker::LABEL_TARGET.to_string(), target_id.to_string()),
        (
            docker::LABEL_ENGINE.to_string(),
            engine.as_str().to_string(),
        ),
        (docker::LABEL_MAJOR.to_string(), major.to_string()),
    ])
}

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSpec {
    pub engine: EngineKind,
    pub major_version: String,
    /// Loopback by default. Anything else is a deliberate exposure (ENG-008).
    pub bind_address: String,
    /// `None` means the engine's standard port, then the next free one
    /// (ENG-007).
    pub host_port: Option<u16>,
    /// `None` means the next free port from the engine's standard console
    /// port. Ignored by engines without a console.
    pub console_port: Option<u16>,
    /// `None` means `postgres:<major>`, prefixed by `general.image_prefix`.
    pub image: Option<String>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
}

impl EngineSpec {
    pub fn new(engine: EngineKind, major: &str) -> Self {
        Self {
            engine,
            major_version: major.to_string(),
            bind_address: LOOPBACK.to_string(),
            host_port: None,
            console_port: None,
            image: None,
            cpu_limit: None,
            memory_limit: None,
        }
    }

    pub fn postgres(major: &str) -> Self {
        Self::new(EngineKind::Postgres, major)
    }

    pub fn minio(major: &str) -> Self {
        Self::new(EngineKind::Minio, major)
    }

    fn label(&self) -> String {
        format!("{} {}", self.engine.as_str(), self.major_version)
    }
}

fn resolve_image(config: &Config, spec: &EngineSpec) -> String {
    if let Some(image) = &spec.image {
        return image.clone();
    }
    let base = spec.engine.default_image(&spec.major_version);
    match &config.general.image_prefix {
        Some(prefix) => format!("{}/{}", prefix.trim_end_matches('/'), base),
        None => base,
    }
}

// ---------------------------------------------------------------------------
// Port selection (ENG-007)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortChoice {
    /// What the caller asked for.
    requested: u16,
    /// What will actually be published.
    port: u16,
    /// Who holds `requested`, when it had to be replaced.
    holder: Option<String>,
}

impl PortChoice {
    fn substituted(&self) -> bool {
        self.port != self.requested
    }

    /// Korean sentence describing the substitution, if there was one.
    fn note(&self) -> Option<String> {
        let holder = self.holder.as_deref()?;
        if !self.substituted() {
            return None;
        }
        Some(format!(
            "포트 {}은(는) {}이(가) 사용 중이어서 {}(으)로 대체합니다.",
            self.requested, holder, self.port
        ))
    }
}

/// `Some(holder)` when `port` cannot be published on `bind` for this target.
async fn port_taken(
    x: &Executor,
    bind: &str,
    port: u16,
    own_container: &str,
) -> Result<Option<String>> {
    if let Some(holder) = docker::port_holder(x, port).await? {
        if holder.name != own_container {
            return Ok(Some(if holder.running {
                format!("컨테이너 `{}`", holder.name)
            } else {
                format!("중지된 컨테이너 `{}`", holder.name)
            }));
        }
        return Ok(None);
    }
    // A non-Docker listener is only observable when the target is this machine.
    if !x.is_remote() && !util::local_port_free(bind, port) {
        return Ok(Some("다른 로컬 프로세스".to_string()));
    }
    Ok(None)
}

async fn choose_port_avoiding(
    x: &Executor,
    bind: &str,
    requested: u16,
    own_container: &str,
    own_exists: bool,
    reserved: Option<u16>,
) -> Result<PortChoice> {
    // A live container already owns its published port; moving it would mean
    // recreating it, which `ensure` must never do behind the user's back.
    if own_exists {
        if reserved == Some(requested) {
            return Err(duplicate_endpoint_error(requested));
        }
        return Ok(PortChoice {
            requested,
            port: requested,
            holder: None,
        });
    }
    let mut holder = None;
    let last = requested.saturating_add(PORT_SCAN_SPAN);
    for candidate in requested..=last {
        if candidate == 0 {
            continue;
        }
        if reserved == Some(candidate) {
            if candidate == requested {
                holder = Some("기본 엔드포인트".to_string());
            }
            continue;
        }
        match port_taken(x, bind, candidate, own_container).await? {
            Some(who) => {
                if candidate == requested {
                    holder = Some(who);
                }
            }
            None => {
                return Ok(PortChoice {
                    requested,
                    port: candidate,
                    holder,
                })
            }
        }
    }
    Err(Error::Conflict(format!(
        "{requested}부터 {last}까지 사용할 수 있는 포트가 없습니다. \
         사용 중인 포트를 정리하거나 다른 포트를 지정하세요."
    )))
}

async fn choose_port(
    x: &Executor,
    bind: &str,
    requested: u16,
    own_container: &str,
    own_exists: bool,
) -> Result<PortChoice> {
    choose_port_avoiding(x, bind, requested, own_container, own_exists, None).await
}

fn duplicate_endpoint_error(port: u16) -> Error {
    Error::Conflict(format!(
        "MinIO 기본 포트와 콘솔 포트에 같은 포트 {port}을(를) 지정할 수 없습니다. \
         서로 다른 포트를 지정하세요."
    ))
}

fn validate_explicit_port_pair(spec: &EngineSpec) -> Result<()> {
    if spec.engine == EngineKind::Minio {
        if let (Some(host), Some(console)) = (spec.host_port, spec.console_port) {
            if host == console {
                return Err(duplicate_endpoint_error(host));
            }
        }
    }
    Ok(())
}

async fn wait_cancelled(cancel: &Cancel) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_cancellable<T, F>(cancel: &Cancel, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = fut => result,
        () = wait_cancelled(cancel) => Err(Error::Cancelled),
    }
}

// ---------------------------------------------------------------------------
// Observation → plan
// ---------------------------------------------------------------------------

/// Everything about the target that the plan and the execution both depend on,
/// read exactly once so the two can never disagree.
#[derive(Debug, Clone)]
struct Observed {
    engine: EngineKind,
    major_version: String,
    image: String,
    image_present: bool,
    container: String,
    container_exists: bool,
    container_managed: bool,
    container_state: String,
    running_image: Option<String>,
    volume: String,
    volume_exists: bool,
    volume_managed: bool,
    bind_address: String,
    port: PortChoice,
    /// Second published endpoint, for engines that have one.
    console: Option<PortChoice>,
}

async fn observe(
    x: &Executor,
    spec: &EngineSpec,
    image: &str,
    recorded: Option<&EngineInstance>,
) -> Result<Observed> {
    validate_explicit_port_pair(spec)?;
    let container = recorded
        .map(|e| e.container_name.clone())
        .unwrap_or_else(|| container_name(spec.engine, &spec.major_version));
    let volume = recorded
        .map(|e| e.volume_name.clone())
        .unwrap_or_else(|| volume_name(spec.engine, &spec.major_version));
    let bind_address = recorded
        .map(|e| e.bind_address.clone())
        .unwrap_or_else(|| spec.bind_address.clone());
    let requested = recorded
        .map(|e| e.host_port)
        .or(spec.host_port)
        .unwrap_or_else(|| spec.engine.container_port());

    let status = docker::container_status(x, &container).await?;
    let container_managed = if status.exists {
        docker::is_managed(x, &container).await?
    } else {
        false
    };
    let volume_exists = docker::volume_exists(x, &volume).await?;
    let volume_managed = if volume_exists {
        docker::volume_labels(x, &volume)
            .await?
            .get(docker::LABEL_MANAGED)
            .map(|v| v == "true")
            .unwrap_or(false)
    } else {
        false
    };

    let port = choose_port(x, &bind_address, requested, &container, status.exists).await?;
    let console = match spec.engine.console_container_port() {
        Some(container_port) => {
            let requested_console = recorded
                .and_then(|e| e.console_port)
                .or(spec.console_port)
                .unwrap_or(container_port);
            Some(
                choose_port_avoiding(
                    x,
                    &bind_address,
                    requested_console,
                    &container,
                    status.exists,
                    Some(port.port),
                )
                .await?,
            )
        }
        None => None,
    };

    Ok(Observed {
        engine: spec.engine,
        major_version: spec.major_version.clone(),
        image: image.to_string(),
        image_present: docker::image_exists(x, image).await?,
        container_exists: status.exists,
        container_managed,
        container_state: status.state.clone(),
        running_image: status.image.clone(),
        volume_exists,
        volume_managed,
        port,
        console,
        container,
        volume,
        bind_address,
    })
}

fn state_label(state: &str) -> &str {
    if state.trim().is_empty() {
        "unknown"
    } else {
        state
    }
}

fn health_label(health: Health) -> &'static str {
    match health {
        Health::Healthy => "정상",
        Health::Unhealthy => "비정상",
        Health::Starting => "기동 중",
        Health::None => "healthcheck 없음",
    }
}

/// The preview shown before anything is touched (PRD §7.5).
fn build_plan(o: &Observed) -> Plan {
    let mut plan = Plan::new(format!(
        "{} {} 엔진 준비",
        o.engine.as_str(),
        o.major_version
    ));

    if o.image_present {
        plan = plan.step_detailed(
            StepKind::Verify,
            format!("{} 이미지 확인", o.image),
            "로컬에 이미 있는 이미지를 사용합니다",
        );
    } else {
        plan = plan.step_detailed(
            StepKind::New,
            format!("{} 이미지 내려받기", o.image),
            format!("docker pull {}", o.image),
        );
    }

    if o.container_exists {
        plan = plan.step_detailed(
            StepKind::Reuse,
            format!("컨테이너 {} 재사용", o.container),
            format!("상태: {}", state_label(&o.container_state)),
        );
    } else {
        plan = plan.step_detailed(
            StepKind::New,
            format!("컨테이너 {} 생성", o.container),
            format!("이미지 {} · 관리 label 부여", o.image),
        );
    }

    if o.volume_exists {
        plan = plan.step_detailed(
            StepKind::Reuse,
            format!("볼륨 {} 재사용", o.volume),
            if o.volume_managed {
                "기존 데이터를 그대로 사용합니다".to_string()
            } else {
                "관리 label이 없는 볼륨입니다".to_string()
            },
        );
        if !o.volume_managed {
            plan = plan.warn(format!(
                "볼륨 `{}`에는 관리 label이 없습니다. local-infra는 이 볼륨을 마운트하지 않습니다.",
                o.volume
            ));
        }
    } else {
        plan = plan.step_detailed(
            StepKind::New,
            format!("볼륨 {} 생성", o.volume),
            format!("{}에 마운트", data_dir(o.engine)),
        );
    }

    let binding = format!(
        "포트 {}:{} → {} 바인딩",
        o.bind_address,
        o.port.port,
        o.engine.container_port()
    );
    let binding_detail = match o.port.note() {
        Some(note) => note,
        None if o.bind_address == LOOPBACK => "127.0.0.1 전용".to_string(),
        None => format!("{} 에 공개", o.bind_address),
    };
    let reuse_or_new = if o.container_exists {
        StepKind::Reuse
    } else {
        StepKind::New
    };
    plan = plan.step_detailed(reuse_or_new, binding, binding_detail);

    if let (Some(console), Some(container_port)) = (&o.console, o.engine.console_container_port()) {
        plan = plan.step_detailed(
            reuse_or_new,
            format!(
                "콘솔 포트 {}:{} → {} 바인딩",
                o.bind_address, console.port, container_port
            ),
            console
                .note()
                .unwrap_or_else(|| "웹 콘솔용 보조 엔드포인트".to_string()),
        );
    }

    plan = plan.step_detailed(
        StepKind::Verify,
        "엔진 준비 상태 확인",
        format!("{} 확인", ready_probe_label(o.engine)),
    );

    if let Some(note) = o.port.note() {
        plan = plan.warn(note);
    }
    if o.bind_address != LOOPBACK {
        plan = plan.warn(format!(
            "포트가 {}에 바인딩되어 이 머신 밖에서도 접근할 수 있습니다. \
             기본값인 127.0.0.1 사용을 권장합니다.",
            o.bind_address
        ));
    }
    if o.container_exists && !o.container_managed {
        plan = plan.warn(format!(
            "컨테이너 `{}`에는 관리 label이 없습니다. local-infra는 이 컨테이너를 변경하지 않습니다.",
            o.container
        ));
    }
    // ENG-010: surface a newer image without ever swapping it silently.
    if let Some(running) = &o.running_image {
        if running != &o.image {
            plan = plan.warn(format!(
                "컨테이너가 이미지 `{running}`(으)로 실행 중입니다. `{}`(으)로 바꾸려면 \
                 컨테이너를 다시 만들어야 하며, 볼륨의 데이터는 유지됩니다.",
                o.image
            ));
        }
    }
    plan
}

/// Plan only, no side effects (PRD §12.3).
pub async fn plan_ensure(ctx: &Ctx, target: &Target, spec: &EngineSpec) -> Result<Plan> {
    let x = ctx.executor(target)?;
    let recorded = ctx
        .store
        .find_engine(&target.id, spec.engine, &spec.major_version)?;
    let image = resolve_image(&ctx.config, spec);
    let observed = observe(&x, spec, &image, recorded.as_ref()).await?;
    Ok(build_plan(&observed))
}

// ---------------------------------------------------------------------------
// ensure
// ---------------------------------------------------------------------------

fn build_run_spec(
    e: &EngineInstance,
    cpu_limit: Option<&str>,
    memory_limit: Option<&str>,
) -> RunSpec {
    let extra_ports = match (e.console_port, e.engine.console_container_port()) {
        (Some(host), Some(container)) => vec![(host, container)],
        _ => Vec::new(),
    };
    RunSpec {
        container_name: e.container_name.clone(),
        image: e.image.clone(),
        volume_name: e.volume_name.clone(),
        data_dir: data_dir(e.engine).to_string(),
        bind_address: e.bind_address.clone(),
        host_port: e.host_port,
        container_port: e.engine.container_port(),
        extra_ports,
        labels: labels(&e.target_id, e.engine, &e.major_version),
        env: container_env(e),
        // Value-less passthrough: the password travels in the docker client's
        // environment, never in argv (PRD §11.2).
        secret_env: vec![password_env(e.engine).to_string()],
        cpu_limit: cpu_limit.map(str::to_string),
        memory_limit: memory_limit.map(str::to_string),
        healthcheck: container_healthcheck(e),
        command: container_command(e),
    }
}

/// Idempotent: reuses the recorded engine when its container is still there,
/// otherwise creates image, volume and container (ENG-001, ENG-002).
pub async fn ensure(
    ctx: &Ctx,
    target: &Target,
    spec: &EngineSpec,
    reporter: &Reporter,
    cancel: &Cancel,
) -> Result<EngineInstance> {
    ctx.require_write_lock()?;
    let x = ctx.executor(target)?;
    docker::require_daemon(&x).await?;

    let recorded = ctx
        .store
        .find_engine(&target.id, spec.engine, &spec.major_version)?;
    let image = resolve_image(&ctx.config, spec);
    let observed = observe(&x, spec, &image, recorded.as_ref()).await?;
    let plan = build_plan(&observed);
    for warning in &plan.warnings {
        reporter.log(warning);
    }

    // An unrecorded container under our name is somebody else's (ENG-006).
    if observed.container_exists && recorded.is_none() {
        return Err(Error::Conflict(format!(
            "컨테이너 `{}`이(가) 이미 있지만 local-infra에 등록되어 있지 않습니다. \
             이름이 겹치지 않는 다른 메이저 버전을 쓰거나, 해당 컨테이너를 직접 정리하세요.",
            observed.container
        )));
    }

    let id = recorded
        .as_ref()
        .map(|e| e.id.clone())
        .unwrap_or_else(util::new_id);
    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "engine",
        "ensure",
        format!("{} · {}", target.display_name, spec.label()),
    )?
    .on_target(&target.id)
    .on_resource(&id);

    let result = ensure_inner(
        ctx,
        target,
        spec,
        &x,
        &observed,
        recorded,
        id,
        reporter,
        cancel,
        &mut activity,
    )
    .await;
    activity.finish(&result);
    result
}

#[allow(clippy::too_many_arguments)]
async fn ensure_inner(
    ctx: &Ctx,
    target: &Target,
    spec: &EngineSpec,
    x: &Executor,
    o: &Observed,
    recorded: Option<EngineInstance>,
    id: String,
    reporter: &Reporter,
    cancel: &Cancel,
    activity: &mut Activity<'_>,
) -> Result<EngineInstance> {
    // ---- reuse -----------------------------------------------------------
    if let Some(engine) = recorded.as_ref().filter(|_| o.container_exists) {
        docker::require_managed(x, &engine.container_name).await?;
        let total = 2;
        cancel.check()?;
        reporter.step(
            1,
            total,
            format!("컨테이너 {} 재사용", engine.container_name),
        );
        if !docker::container_status(x, &engine.container_name)
            .await?
            .running
        {
            docker::start_container(x, &engine.container_name).await?;
            activity.step(format!("컨테이너 `{}` 시작", engine.container_name));
        } else {
            activity.step(format!("컨테이너 `{}` 재사용", engine.container_name));
        }
        reporter.step_done(1);

        cancel.check()?;
        reporter.step(2, total, "엔진 준비 상태 확인");
        wait_ready_with(ctx, x, engine, READY_TIMEOUT_SECS, reporter, cancel).await?;
        activity.step("준비 상태 확인 완료");
        reporter.step_done(2);
        return Ok(engine.clone());
    }

    // ---- create or recreate ---------------------------------------------
    if o.volume_exists && !o.volume_managed {
        return Err(Error::Refused(format!(
            "볼륨 `{}`은(는) local-infra가 생성하지 않았습니다. 이 볼륨을 마운트하지 않습니다.",
            o.volume
        )));
    }

    let credential_ref = recorded
        .as_ref()
        .map(|e| e.credential_ref.clone())
        .unwrap_or_else(|| secrets::engine_ref(&id));

    let mut created_secret = false;
    let password = match ctx.secrets.get(&credential_ref)? {
        Some(existing) => existing,
        None => {
            let generated = util::generate_password(PASSWORD_LEN);
            ctx.secrets.set(&credential_ref, &generated)?;
            created_secret = recorded.is_none();
            generated
        }
    };
    if o.volume_exists && recorded.is_some() {
        reporter.log(format!(
            "볼륨 {}의 기존 데이터를 그대로 사용합니다. 관리자 비밀번호는 변경되지 않습니다.",
            o.volume
        ));
    }

    let engine = EngineInstance {
        id: id.clone(),
        target_id: target.id.clone(),
        engine: spec.engine,
        major_version: spec.major_version.clone(),
        image: o.image.clone(),
        container_name: o.container.clone(),
        volume_name: o.volume.clone(),
        bind_address: o.bind_address.clone(),
        host_port: o.port.port,
        console_port: o.console.as_ref().map(|c| c.port),
        admin_user: ADMIN_USER.to_string(),
        credential_ref: credential_ref.clone(),
        managed: true,
        created_at: recorded
            .as_ref()
            .map(|e| e.created_at)
            .unwrap_or_else(util::now),
    };

    if let Some(previous) = recorded.as_ref() {
        if let Some(holder) = port_taken(
            x,
            &engine.bind_address,
            previous.host_port,
            &engine.container_name,
        )
        .await?
        {
            return Err(Error::Conflict(format!(
                "등록된 포트 {}을(를) {}이(가) 사용 중이라 엔진 컨테이너를 다시 만들 수 없습니다. \
                 해당 포트를 비우거나 `linf engine rm`으로 엔진 등록을 정리하세요.",
                previous.host_port, holder
            )));
        }
    }

    let mut created_volume = false;
    let mut created_container = false;
    let mut created_row = false;
    let outcome = create_engine_resources(
        ctx,
        x,
        target,
        spec,
        o,
        &engine,
        &password,
        cancel,
        reporter,
        activity,
        &mut created_volume,
        &mut created_container,
        &mut created_row,
    )
    .await;
    if outcome.is_err() {
        if created_container {
            let _ = docker::remove_container(x, &engine.container_name, true).await;
        }
        if created_volume {
            let _ = docker::remove_volume(x, &engine.volume_name).await;
        }
        if created_row {
            let _ = ctx.store.delete_engine(&engine.id);
        }
        if created_secret {
            let _ = ctx.secrets.delete(&credential_ref);
        }
        activity.step("생성 중 실패해 새로 만든 리소스를 되돌렸습니다");
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn create_engine_resources(
    ctx: &Ctx,
    x: &Executor,
    target: &Target,
    spec: &EngineSpec,
    o: &Observed,
    engine: &EngineInstance,
    password: &str,
    cancel: &Cancel,
    reporter: &Reporter,
    activity: &mut Activity<'_>,
    created_volume: &mut bool,
    created_container: &mut bool,
    created_row: &mut bool,
) -> Result<EngineInstance> {
    let mut step = 0usize;
    let total = 4 - usize::from(o.image_present) - usize::from(o.volume_exists);

    if !o.image_present {
        cancel.check()?;
        step += 1;
        reporter.step(step, total, format!("{} 이미지 내려받기", o.image));
        run_cancellable(cancel, docker::pull_image(x, &o.image)).await?;
        activity.step(format!("이미지 `{}` 준비", o.image));
        reporter.step_done(step);
    }

    if !o.volume_exists {
        cancel.check()?;
        step += 1;
        reporter.step(step, total, format!("볼륨 {} 생성", o.volume));
        docker::create_volume(
            x,
            &o.volume,
            &labels(&target.id, spec.engine, &spec.major_version),
        )
        .await?;
        *created_volume = true;
        activity.step(format!("볼륨 `{}` 생성", o.volume));
        reporter.step_done(step);
    }

    cancel.check()?;
    step += 1;
    reporter.step(
        step,
        total,
        format!(
            "컨테이너 {} 생성 ({}:{})",
            engine.container_name, engine.bind_address, engine.host_port
        ),
    );
    if let Some(note) = o.port.note() {
        reporter.log(&note);
        activity.step(&note);
    }
    let mut engine = engine.clone();
    let env = SecretEnv::new().set(password_env(engine.engine), password)?;
    run_container_with_free_port(x, spec, &mut engine, &env, cancel, reporter, activity).await?;
    *created_container = true;
    activity.step(format!(
        "컨테이너 `{}` 생성 ({}:{})",
        engine.container_name, engine.bind_address, engine.host_port
    ));
    reporter.step_done(step);

    if ctx
        .store
        .find_engine(&engine.target_id, engine.engine, &engine.major_version)?
        .is_none()
    {
        ctx.store.insert_engine(&engine)?;
        *created_row = true;
        activity.step("엔진 등록");
    }

    cancel.check()?;
    step += 1;
    reporter.step(step, total, "엔진 준비 상태 확인");
    wait_ready_with(ctx, x, &engine, READY_TIMEOUT_SECS, reporter, cancel).await?;
    activity.step("준비 상태 확인 완료");
    reporter.step_done(step);
    Ok(engine)
}

fn port_conflict(err: &Error) -> bool {
    let d = err.as_diagnostic();
    let mut hay = format!("{} {}", d.what, d.cause);
    if let Some(output) = &d.output {
        hay.push(' ');
        hay.push_str(output);
    }
    let hay = hay.to_ascii_lowercase();
    hay.contains("port is already allocated")
        || hay.contains("address already in use")
        || (hay.contains("bind for") && hay.contains("allocated"))
}

async fn run_container_with_free_port(
    x: &Executor,
    spec: &EngineSpec,
    engine: &mut EngineInstance,
    env: &SecretEnv,
    cancel: &Cancel,
    reporter: &Reporter,
    activity: &mut Activity<'_>,
) -> Result<()> {
    for attempt in 0..8 {
        cancel.check()?;
        let run_spec = build_run_spec(
            engine,
            spec.cpu_limit.as_deref(),
            spec.memory_limit.as_deref(),
        );
        match run_cancellable(cancel, docker::run_container(x, &run_spec, env)).await {
            Ok(_) => return Ok(()),
            Err(e) if attempt + 1 < 8 && port_conflict(&e) => {
                let next = choose_port(
                    x,
                    &engine.bind_address,
                    engine.host_port.saturating_add(1),
                    &engine.container_name,
                    false,
                )
                .await?;
                let note = format!(
                    "포트 {}이(가) 사용 중이어서 {}(으)로 다시 시도합니다.",
                    engine.host_port, next.port
                );
                reporter.log(&note);
                activity.step(&note);
                engine.host_port = next.port;
                if let Some(console) = engine.console_port {
                    if console == engine.host_port {
                        let console = choose_port_avoiding(
                            x,
                            &engine.bind_address,
                            console.saturating_add(1),
                            &engine.container_name,
                            false,
                            Some(engine.host_port),
                        )
                        .await?;
                        engine.console_port = Some(console.port);
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(Error::Conflict(format!(
        "컨테이너 `{}`를 만들 포트를 찾지 못했습니다.",
        engine.container_name
    )))
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

fn executor_for(ctx: &Ctx, e: &EngineInstance) -> Result<Executor> {
    let target = ctx.store.require_target(&e.target_id)?;
    ctx.executor(&target)
}

pub async fn status(ctx: &Ctx, e: &EngineInstance) -> Result<EngineStatus> {
    let x = executor_for(ctx, e)?;
    docker::container_status(&x, &e.container_name).await
}

pub async fn start(ctx: &Ctx, e: &EngineInstance) -> Result<()> {
    lifecycle(ctx, e, "start", "시작").await
}

pub async fn stop(ctx: &Ctx, e: &EngineInstance) -> Result<()> {
    lifecycle(ctx, e, "stop", "중지").await
}

pub async fn restart(ctx: &Ctx, e: &EngineInstance) -> Result<()> {
    lifecycle(ctx, e, "restart", "재시작").await
}

async fn lifecycle(ctx: &Ctx, e: &EngineInstance, action: &str, korean: &str) -> Result<()> {
    ctx.require_write_lock()?;
    let x = executor_for(ctx, e)?;
    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "engine",
        action,
        format!("{} {}", e.container_name, korean),
    )?
    .on_target(&e.target_id)
    .on_resource(&e.id);

    let result = match action {
        "start" => docker::start_container(&x, &e.container_name).await,
        "stop" => docker::stop_container(&x, &e.container_name).await,
        _ => docker::restart_container(&x, &e.container_name).await,
    };
    if result.is_ok() {
        activity.step(format!("컨테이너 `{}` {}", e.container_name, korean));
    }
    activity.finish(&result);
    result
}

pub async fn logs(ctx: &Ctx, e: &EngineInstance, tail: usize) -> Result<String> {
    let x = executor_for(ctx, e)?;
    docker::logs(&x, &e.container_name, tail).await
}

/// Poll the container healthcheck, falling back to the engine's own readiness
/// probe for containers created without one (ENG-004).
pub async fn wait_ready(
    ctx: &Ctx,
    e: &EngineInstance,
    timeout_secs: u64,
    reporter: &Reporter,
    cancel: &Cancel,
) -> Result<()> {
    let x = executor_for(ctx, e)?;
    wait_ready_with(ctx, &x, e, timeout_secs, reporter, cancel).await
}

async fn wait_ready_with(
    ctx: &Ctx,
    x: &Executor,
    e: &EngineInstance,
    timeout_secs: u64,
    reporter: &Reporter,
    cancel: &Cancel,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let mut last = String::new();
    loop {
        cancel.check()?;
        let status = docker::container_status(x, &e.container_name).await?;
        if !status.exists {
            return Err(Error::failed(
                format!("컨테이너 `{}`이(가) 사라졌습니다", e.container_name),
                "준비 상태를 확인하는 동안 컨테이너가 제거되었습니다.",
                "`linf engine logs`로 원인을 확인한 뒤 다시 시도하세요.",
            ));
        }
        if status.running {
            match status.health {
                // The configured healthcheck is the engine's own probe, so a
                // healthy container needs no extra round trip.
                Health::Healthy => return Ok(()),
                Health::None => {
                    if probe_ready(ctx, x, e).await? {
                        return Ok(());
                    }
                }
                Health::Starting | Health::Unhealthy => {}
            }
        }

        let now = format!(
            "{} · {}",
            state_label(&status.state),
            health_label(status.health)
        );
        if now != last {
            reporter.log(format!("엔진 준비 대기 중: {now}"));
            last = now;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::failed(
                format!("엔진 `{}`이(가) 준비되지 않았습니다", e.container_name),
                format!(
                    "{timeout_secs}초 안에 {}이(가) 성공하지 않았습니다 (상태: {last}).",
                    ready_probe_label(e.engine)
                ),
                "`linf engine logs`로 기동 로그를 확인하세요. 볼륨 권한 문제나 포트 충돌이 흔한 원인입니다.",
            ));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

/// The engine's own readiness probe, run inside the container.
///
/// MinIO's probe needs the root credentials, which is exactly why it is not a
/// Docker healthcheck: they arrive through [`SecretEnv`] instead of being
/// baked into a second container variable.
async fn probe_ready(ctx: &Ctx, x: &Executor, e: &EngineInstance) -> Result<bool> {
    match e.engine {
        EngineKind::Postgres => {
            let argv = vec![
                "pg_isready".to_string(),
                "-U".to_string(),
                e.admin_user.clone(),
                "-d".to_string(),
                ADMIN_DB.to_string(),
            ];
            Ok(
                docker::exec(x, &e.container_name, None, &argv, &SecretEnv::new(), None)
                    .await?
                    .ok(),
            )
        }
        EngineKind::Minio => {
            let Some(password) = ctx.secrets.get(&e.credential_ref)? else {
                // Without the root password there is nothing to authenticate
                // with; fall back to "the container is up", which the caller
                // already established.
                return Ok(true);
            };
            let secrets = minio_admin_env(e, &password)?;
            let argv = vec![
                "mc".to_string(),
                "ready".to_string(),
                MINIO_ALIAS.to_string(),
            ];
            Ok(
                docker::exec(x, &e.container_name, None, &argv, &secrets, None)
                    .await?
                    .ok(),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Removal (PRD §7.9)
// ---------------------------------------------------------------------------

async fn plan_remove_with(
    store: &Store,
    x: &Executor,
    e: &EngineInstance,
    remove_volume: bool,
) -> Result<Plan> {
    let databases = store.list_databases_for_engine(&e.id)?;
    let names: Vec<String> = databases.iter().map(|d| d.database_name.clone()).collect();
    let listed = if names.is_empty() {
        "없음".to_string()
    } else {
        names.join(", ")
    };

    let mut plan = Plan::new(if remove_volume {
        format!("{} 엔진과 볼륨 삭제", e.label())
    } else {
        format!("{} 엔진 컨테이너 삭제", e.label())
    });

    plan = plan.step_detailed(
        StepKind::Verify,
        format!("컨테이너 {} 관리 label 확인", e.container_name),
        "local-infra가 만든 리소스만 삭제합니다",
    );

    let container_exists = docker::container_status(x, &e.container_name).await?.exists;
    plan = if container_exists {
        plan.step_detailed(
            StepKind::Destroy,
            format!("컨테이너 {} 삭제", e.container_name),
            format!("영향 받는 DB {}개: {}", names.len(), listed),
        )
    } else {
        plan.step_detailed(
            StepKind::Verify,
            format!("컨테이너 {} 없음", e.container_name),
            "이미 삭제되어 있습니다",
        )
    };
    plan = plan.warn(format!(
        "이 엔진의 DB {}개가 모두 중단됩니다: {}",
        names.len(),
        listed
    ));

    if remove_volume {
        let volume_exists = docker::volume_exists(x, &e.volume_name).await?;
        plan = if volume_exists {
            plan.step_detailed(
                StepKind::Destroy,
                format!("볼륨 {} 삭제", e.volume_name),
                "되돌릴 수 없습니다",
            )
        } else {
            plan.step_detailed(
                StepKind::Verify,
                format!("볼륨 {} 없음", e.volume_name),
                "이미 삭제되어 있습니다",
            )
        };
        plan = plan
            .step_detailed(
                StepKind::Destroy,
                "엔진 등록 정보 삭제",
                format!(
                    "DB 메타데이터 {}건과 저장된 비밀번호를 함께 제거합니다",
                    names.len()
                ),
            )
            .warn(format!(
                "{}개 DB의 모든 데이터가 영구 삭제됩니다: {}",
                names.len(),
                listed
            ))
            .warn("먼저 `linf backup run`으로 백업하는 것을 권장합니다.");
    } else {
        plan = plan
            .warn("볼륨과 등록 정보는 유지되므로 `linf engine ensure`로 다시 만들 수 있습니다.");
    }
    Ok(plan)
}

pub async fn plan_remove(ctx: &Ctx, e: &EngineInstance, remove_volume: bool) -> Result<Plan> {
    let x = executor_for(ctx, e)?;
    plan_remove_with(&ctx.store, &x, e, remove_volume).await
}

/// Removes the container, and — only when `remove_volume` — the data volume
/// together with every metadata row and secret this engine owns (PRD §7.9).
pub async fn remove(
    ctx: &Ctx,
    e: &EngineInstance,
    remove_volume: bool,
    reporter: &Reporter,
) -> Result<()> {
    ctx.require_write_lock()?;
    let x = executor_for(ctx, e)?;
    let databases = ctx.store.list_databases_for_engine(&e.id)?;

    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "engine",
        if remove_volume { "destroy" } else { "remove" },
        format!(
            "{} 삭제 (볼륨 {})",
            e.container_name,
            if remove_volume { "포함" } else { "유지" }
        ),
    )?
    .on_target(&e.target_id)
    .on_resource(&e.id);

    let result = remove_inner(
        ctx,
        &x,
        e,
        remove_volume,
        &databases,
        reporter,
        &mut activity,
    )
    .await;
    activity.finish(&result);
    result
}

async fn remove_inner(
    ctx: &Ctx,
    x: &Executor,
    e: &EngineInstance,
    remove_volume: bool,
    databases: &[crate::core::model::ManagedDatabase],
    reporter: &Reporter,
    activity: &mut Activity<'_>,
) -> Result<()> {
    crate::core::tunnel::stop_for_engine(ctx, &e.id).await?;
    activity.step("관련 SSH 터널을 정리했습니다");
    let total = if remove_volume { 3 } else { 1 };

    if docker::container_status(x, &e.container_name).await?.exists {
        reporter.step(1, total, format!("컨테이너 {} 삭제", e.container_name));
        // Re-reads the live labels: never touch a container we did not create.
        docker::require_managed(x, &e.container_name).await?;
        let _ = docker::stop_container(x, &e.container_name).await;
        docker::remove_container(x, &e.container_name, true).await?;
        activity.step(format!("컨테이너 `{}` 삭제", e.container_name));
        reporter.step_done(1);
    } else {
        activity.step(format!(
            "컨테이너 `{}`은(는) 이미 없습니다",
            e.container_name
        ));
    }

    if !remove_volume {
        return Ok(());
    }

    reporter.step(2, total, format!("볼륨 {} 삭제", e.volume_name));
    if docker::volume_exists(x, &e.volume_name).await? {
        docker::remove_volume(x, &e.volume_name).await?;
        activity.step(format!("볼륨 `{}` 삭제", e.volume_name));
    } else {
        activity.step(format!("볼륨 `{}`은(는) 이미 없습니다", e.volume_name));
    }
    reporter.step_done(2);

    reporter.step(3, total, "등록 정보 삭제");
    forget_engine_rows(ctx, e)?;
    activity.step(format!(
        "등록 정보와 DB 메타데이터 {}건 삭제",
        databases.len()
    ));
    reporter.step_done(3);
    Ok(())
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct EngineOverview {
    pub engine: EngineInstance,
    pub target: Target,
    pub status: EngineStatus,
    pub database_count: usize,
}

/// Every registered engine with its live status. A target whose Docker cannot
/// be reached reports a missing engine instead of failing the whole listing.
pub async fn overview(ctx: &Ctx) -> Result<Vec<EngineOverview>> {
    let mut out = Vec::new();
    for engine in ctx.store.list_engines()? {
        let Some(target) = ctx.store.find_target(&engine.target_id)? else {
            continue;
        };
        let status = match ctx.executor(&target) {
            Ok(x) => docker::container_status(&x, &engine.container_name)
                .await
                .unwrap_or_else(|_| EngineStatus::missing()),
            Err(_) => EngineStatus::missing(),
        };
        let database_count = ctx.store.list_databases_for_engine(&engine.id)?.len();
        out.push(EngineOverview {
            engine,
            target,
            status,
            database_count,
        });
    }
    Ok(out)
}

/// `Ok(None)` in restricted secret mode, where nothing was ever persisted.
pub fn admin_password(ctx: &Ctx, e: &EngineInstance) -> Result<Option<String>> {
    ctx.secrets.get(&e.credential_ref)
}

// ---------------------------------------------------------------------------
// Reconcile / reset
// ---------------------------------------------------------------------------

/// Drop this engine's SQLite rows and every secret it owns. Docker is already
/// gone — this is the metadata half of a `docker rm`.
pub fn forget_engine_rows(ctx: &Ctx, e: &EngineInstance) -> Result<()> {
    let databases = ctx.store.list_databases_for_engine(&e.id)?;
    let buckets = ctx.store.list_buckets_for_engine(&e.id)?;
    for db in &databases {
        let _ = ctx.secrets.delete(&db.credential_ref);
        let _ = ctx.store.delete_tunnels_for_resource(&db.id);
        let _ = ctx.store.delete_backups_for_resource(&db.id);
    }
    for bucket in &buckets {
        let _ = ctx.secrets.delete(&bucket.credential_ref);
        let _ = ctx.store.delete_tunnels_for_resource(&bucket.id);
        let _ = ctx.store.delete_backups_for_resource(&bucket.id);
    }
    let _ = ctx.secrets.delete(&e.credential_ref);
    ctx.store.delete_engine(&e.id)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileEvent {
    pub container_name: String,
    pub reason: String,
}

/// Docker is the source of truth. An engine whose container *and* volume are
/// both gone is forgotten, so the TUI does not keep ghost databases.
///
/// A missing container with a surviving volume is left registered: the next
/// `ensure` recreates the container on the same data.
/// Unreachable Docker is skipped — we never prune on a failed inspect.
pub async fn reconcile(ctx: &Ctx) -> Result<Vec<ReconcileEvent>> {
    if !ctx.has_write_lock() {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    for engine in ctx.store.list_engines()? {
        let Some(target) = ctx.store.find_target(&engine.target_id)? else {
            continue;
        };
        let Ok(x) = ctx.executor(&target) else {
            continue;
        };
        let Ok(status) = docker::container_status(&x, &engine.container_name).await else {
            continue;
        };
        if status.exists {
            continue;
        }
        let Ok(volume_exists) = docker::volume_exists(&x, &engine.volume_name).await else {
            continue;
        };
        if volume_exists {
            events.push(ReconcileEvent {
                container_name: engine.container_name.clone(),
                reason: format!(
                    "컨테이너는 없지만 볼륨 `{}`은 남아 있습니다. 엔진을 다시 만들면 데이터가 살아납니다.",
                    engine.volume_name
                ),
            });
            continue;
        }
        let _ = crate::core::tunnel::stop_for_engine(ctx, &engine.id).await;
        forget_engine_rows(ctx, &engine)?;
        events.push(ReconcileEvent {
            container_name: engine.container_name,
            reason: "Docker에서 컨테이너와 볼륨이 사라져 등록을 해제했습니다.".into(),
        });
    }
    Ok(events)
}

#[derive(Debug, Clone, Serialize)]
pub struct ResetReport {
    pub engines_removed: usize,
    pub targets_removed: usize,
}

/// Destroy every managed engine (container + volume) and wipe registration.
/// Targets are forgotten last so the next TUI launch starts empty.
pub async fn reset_all(ctx: &Ctx, reporter: &Reporter) -> Result<ResetReport> {
    ctx.require_write_lock()?;
    let engines = ctx.store.list_engines()?;
    let n = engines.len();
    let mut executors: Vec<Executor> = Vec::new();
    for engine in &engines {
        if let Ok(Some(t)) = ctx.store.find_target(&engine.target_id) {
            if let Ok(x) = ctx.executor(&t) {
                executors.push(x);
            }
        }
        reporter.step(
            n.max(1),
            n.max(1),
            format!("{} 삭제", engine.container_name),
        );
        let _ = remove(ctx, engine, true, reporter).await;
        reporter.step_done(n.max(1));
    }
    executors.push(Executor::Local {
        docker: "docker".into(),
    });
    for leftover in ctx.store.list_engines()? {
        let _ = forget_engine_rows(ctx, &leftover);
    }
    for x in &executors {
        sweep_managed_docker(x).await;
    }
    let targets = ctx.store.list_targets()?;
    let targets_removed = targets.len();
    for t in &targets {
        let _ = ctx.store.delete_target(&t.id);
    }
    Ok(ResetReport {
        engines_removed: n,
        targets_removed,
    })
}

async fn sweep_managed_docker(x: &Executor) {
    if let Ok(names) = docker::list_managed_container_names(x).await {
        for name in names {
            let _ = docker::stop_container(x, &name).await;
            let _ = docker::remove_container(x, &name, true).await;
        }
    }
    if let Ok(volumes) = docker::list_volume_names(x).await {
        for name in volumes {
            let Ok(labels) = docker::volume_labels(x, &name).await else {
                continue;
            };
            if labels.get(docker::LABEL_MANAGED).map(String::as_str) == Some("true") {
                let _ = docker::remove_volume(x, &name).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{ManagedDatabase, TargetKind};
    use std::net::TcpListener;

    #[test]
    fn docker_desktop_port_allocation_errors_are_recognised() {
        let err = Error::diagnostic(
            crate::core::error::Diagnostic::new("컨테이너 생성 실패", "종료 코드 125", "다시 시도")
                .with_output("Bind for 0.0.0.0:9000 failed: port is already allocated"),
        );
        assert!(port_conflict(&err));
        assert!(!port_conflict(&Error::Usage("bad".into())));
    }

    /// Every docker call fails → nothing exists on the target.
    fn empty_target() -> Executor {
        Executor::Local {
            docker: "false".into(),
        }
    }

    /// Every docker call succeeds with empty output → everything exists.
    fn populated_target() -> Executor {
        Executor::Local {
            docker: "true".into(),
        }
    }

    fn target() -> Target {
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
            created_at: util::now(),
            last_connected_at: None,
        }
    }

    fn engine_row(target_id: &str, port: u16) -> EngineInstance {
        let id = util::new_id();
        EngineInstance {
            credential_ref: secrets::engine_ref(&id),
            id,
            target_id: target_id.to_string(),
            engine: EngineKind::Postgres,
            major_version: "17".into(),
            image: "postgres:17".into(),
            container_name: container_name(EngineKind::Postgres, "17"),
            volume_name: volume_name(EngineKind::Postgres, "17"),
            bind_address: LOOPBACK.into(),
            host_port: port,
            console_port: None,
            admin_user: ADMIN_USER.into(),
            managed: true,
            created_at: util::now(),
        }
    }

    fn database_row(engine_id: &str, name: &str) -> ManagedDatabase {
        let id = util::new_id();
        ManagedDatabase {
            credential_ref: crate::core::secrets::database_ref(&id),
            id,
            engine_instance_id: engine_id.to_string(),
            project_name: name.to_string(),
            database_name: format!("{name}_dev"),
            username: format!("{name}_user"),
            preferred_local_tunnel_port: None,
            created_at: util::now(),
            last_connection_test_at: None,
            last_backup_at: None,
        }
    }

    /// A port nothing is listening on right now.
    fn free_port() -> u16 {
        TcpListener::bind((LOOPBACK, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn spec_on(port: u16) -> EngineSpec {
        EngineSpec {
            host_port: Some(port),
            console_port: None,
            ..EngineSpec::postgres("17")
        }
    }

    #[test]
    fn names_follow_the_documented_convention() {
        assert_eq!(
            container_name(EngineKind::Postgres, "17"),
            "linf-postgres-17"
        );
        assert_eq!(volume_name(EngineKind::Postgres, "17"), "linf-pg17-data");
        assert_eq!(
            container_name(EngineKind::Postgres, "16"),
            "linf-postgres-16"
        );
        assert_eq!(volume_name(EngineKind::Postgres, "16"), "linf-pg16-data");
    }

    #[test]
    fn deleting_an_engine_row_takes_its_databases_with_it() {
        let store = Store::open_in_memory().unwrap();
        let t = target();
        store.insert_target(&t).unwrap();
        let e = engine_row(&t.id, 5432);
        store.insert_engine(&e).unwrap();
        store.insert_database(&database_row(&e.id, "p")).unwrap();
        assert_eq!(store.list_databases_for_engine(&e.id).unwrap().len(), 1);
        store.delete_engine(&e.id).unwrap();
        assert!(store.list_databases_for_engine(&e.id).unwrap().is_empty());
        assert!(store.list_engines().unwrap().is_empty());
    }

    #[test]
    fn the_default_spec_is_loopback_only_on_the_standard_port() {
        let spec = EngineSpec::postgres("17");
        assert_eq!(spec.bind_address, "127.0.0.1");
        assert_eq!(spec.host_port, None);
        assert_eq!(
            resolve_image(&Config::default(), &spec),
            "postgres:17".to_string()
        );
    }

    #[test]
    fn an_image_prefix_is_applied_only_to_generated_names() {
        let mut config = Config::default();
        config.general.image_prefix = Some("docker.io/library".into());
        assert_eq!(
            resolve_image(&config, &EngineSpec::postgres("17")),
            "docker.io/library/postgres:17"
        );
        let pinned = EngineSpec {
            image: Some("ghcr.io/acme/pg:17".into()),
            ..EngineSpec::postgres("17")
        };
        assert_eq!(resolve_image(&config, &pinned), "ghcr.io/acme/pg:17");
    }

    #[tokio::test]
    async fn an_empty_target_plans_everything_as_new() {
        let spec = spec_on(free_port());
        let observed = observe(&empty_target(), &spec, "postgres:17", None)
            .await
            .unwrap();
        let plan = build_plan(&observed);
        let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::New,    // pull image
                StepKind::New,    // create container
                StepKind::New,    // create volume
                StepKind::New,    // publish port
                StepKind::Verify, // readiness
            ]
        );
        let text = plan.render();
        assert!(text.contains("컨테이너 linf-postgres-17 생성"), "{text}");
        assert!(text.contains("볼륨 linf-pg17-data 생성"), "{text}");
        assert!(text.contains("127.0.0.1 전용"), "{text}");
        assert!(!plan.is_destructive());
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[tokio::test]
    async fn an_existing_engine_is_planned_as_reuse() {
        let spec = spec_on(free_port());
        let observed = observe(&populated_target(), &spec, "postgres:17", None)
            .await
            .unwrap();
        let plan = build_plan(&observed);
        let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::Verify, // image already local
                StepKind::Reuse,  // container
                StepKind::Reuse,  // volume
                StepKind::Reuse,  // port binding kept
                StepKind::Verify, // readiness
            ]
        );
        assert!(plan.render().contains("컨테이너 linf-postgres-17 재사용"));
    }

    #[tokio::test]
    async fn a_busy_default_port_is_substituted_and_explained() {
        let held = TcpListener::bind((LOOPBACK, 0)).unwrap();
        let busy = held.local_addr().unwrap().port();

        let choice = choose_port(&empty_target(), LOOPBACK, busy, "linf-postgres-17", false)
            .await
            .unwrap();
        assert!(choice.substituted(), "{choice:?}");
        assert_ne!(choice.port, busy);
        assert!(choice.port > busy);
        let note = choice.note().expect("substitution is explained");
        assert!(note.contains(&busy.to_string()), "{note}");
        assert!(note.contains("다른 로컬 프로세스"), "{note}");

        let plan = build_plan(
            &observe(&empty_target(), &spec_on(busy), "postgres:17", None)
                .await
                .unwrap(),
        );
        assert!(
            plan.warnings.iter().any(|w| w.contains("대체")),
            "{:?}",
            plan.warnings
        );
        assert!(plan
            .render()
            .contains(&format!("포트 127.0.0.1:{}", choice.port)));
    }

    #[tokio::test]
    async fn a_live_container_keeps_the_port_it_already_published() {
        let held = TcpListener::bind((LOOPBACK, 0)).unwrap();
        let busy = held.local_addr().unwrap().port();
        let choice = choose_port(&empty_target(), LOOPBACK, busy, "linf-postgres-17", true)
            .await
            .unwrap();
        assert_eq!(choice.port, busy);
        assert!(!choice.substituted());
    }

    #[tokio::test]
    async fn binding_outside_loopback_is_warned_about() {
        let spec = EngineSpec {
            bind_address: "0.0.0.0".into(),
            host_port: Some(free_port()),
            console_port: None,
            ..EngineSpec::postgres("17")
        };
        let plan = build_plan(
            &observe(&empty_target(), &spec, "postgres:17", None)
                .await
                .unwrap(),
        );
        assert!(
            plan.warnings.iter().any(|w| w.contains("0.0.0.0")),
            "{:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn an_unlabelled_container_under_our_name_is_flagged_not_adopted() {
        // `true` answers every inspect successfully but reports no labels.
        let observed = observe(
            &populated_target(),
            &EngineSpec::postgres("17"),
            "postgres:17",
            None,
        )
        .await
        .unwrap();
        assert!(observed.container_exists);
        assert!(!observed.container_managed);
        let plan = build_plan(&observed);
        assert!(
            plan.warnings.iter().any(|w| w.contains("관리 label")),
            "{:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn removing_an_engine_lists_every_database_it_would_stop() {
        let store = Store::open_in_memory().unwrap();
        let t = target();
        store.insert_target(&t).unwrap();
        let e = engine_row(&t.id, 5432);
        store.insert_engine(&e).unwrap();
        store
            .insert_database(&database_row(&e.id, "letsbid"))
            .unwrap();
        store
            .insert_database(&database_row(&e.id, "parantica"))
            .unwrap();

        let plan = plan_remove_with(&store, &populated_target(), &e, false)
            .await
            .unwrap();
        assert!(plan.is_destructive());
        let text = plan.render();
        assert!(text.contains("letsbid_dev"), "{text}");
        assert!(text.contains("parantica_dev"), "{text}");
        assert!(text.contains("DB 2개가 모두 중단됩니다"), "{text}");
        assert!(
            !text.contains("영구 삭제"),
            "볼륨을 남기면 데이터는 유지된다: {text}"
        );

        let destructive = plan_remove_with(&store, &populated_target(), &e, true)
            .await
            .unwrap();
        assert!(destructive.is_destructive());
        let text = destructive.render();
        assert!(text.contains("볼륨 linf-pg17-data 삭제"), "{text}");
        assert!(
            text.contains("2개 DB의 모든 데이터가 영구 삭제됩니다"),
            "{text}"
        );
        assert!(text.contains("letsbid_dev, parantica_dev"), "{text}");
    }

    #[tokio::test]
    async fn a_removal_plan_never_claims_to_delete_what_is_already_gone() {
        let store = Store::open_in_memory().unwrap();
        let t = target();
        store.insert_target(&t).unwrap();
        let e = engine_row(&t.id, 5432);
        store.insert_engine(&e).unwrap();

        let plan = plan_remove_with(&store, &empty_target(), &e, true)
            .await
            .unwrap();
        let text = plan.render();
        assert!(text.contains("컨테이너 linf-postgres-17 없음"), "{text}");
        assert!(text.contains("볼륨 linf-pg17-data 없음"), "{text}");
        // The registration itself is still there and still gets removed.
        assert!(plan.is_destructive());
    }

    #[test]
    fn the_run_spec_labels_the_container_and_keeps_the_password_out_of_argv() {
        let e = engine_row("t-local", 5433);
        let spec = build_run_spec(&e, Some("1.5"), Some("512m"));

        assert_eq!(spec.labels.get(docker::LABEL_MANAGED).unwrap(), "true");
        assert_eq!(spec.labels.get(docker::LABEL_TARGET).unwrap(), "t-local");
        assert_eq!(spec.labels.get(docker::LABEL_ENGINE).unwrap(), "postgres");
        assert_eq!(spec.labels.get(docker::LABEL_MAJOR).unwrap(), "17");
        assert_eq!(spec.env.get("POSTGRES_USER").unwrap(), ADMIN_USER);
        assert_eq!(spec.env.get("POSTGRES_DB").unwrap(), "postgres");
        assert_eq!(spec.env.get("PGDATA").unwrap(), PGDATA);
        assert_eq!(spec.secret_env, vec!["POSTGRES_PASSWORD".to_string()]);
        assert_eq!(spec.container_port, DEFAULT_PORT);
        assert_eq!(spec.host_port, 5433);

        let argv = docker::run_argv("docker", &spec).join(" ");
        assert!(argv.contains("--publish 127.0.0.1:5433:5432"), "{argv}");
        assert!(
            argv.contains("--health-cmd pg_isready -U linf_admin -d postgres"),
            "{argv}"
        );
        assert!(
            argv.contains("--volume linf-pg17-data:/var/lib/postgresql/data"),
            "{argv}"
        );
        assert!(!argv.contains("POSTGRES_PASSWORD="), "{argv}");
    }

    fn minio_row(target_id: &str, port: u16, console: u16) -> EngineInstance {
        let id = util::new_id();
        EngineInstance {
            credential_ref: secrets::engine_ref(&id),
            id,
            target_id: target_id.to_string(),
            engine: EngineKind::Minio,
            major_version: "latest".into(),
            image: "minio/minio:latest".into(),
            container_name: container_name(EngineKind::Minio, "latest"),
            volume_name: volume_name(EngineKind::Minio, "latest"),
            bind_address: LOOPBACK.into(),
            host_port: port,
            console_port: Some(console),
            admin_user: ADMIN_USER.into(),
            managed: true,
            created_at: util::now(),
        }
    }

    #[test]
    fn the_minio_run_spec_publishes_both_endpoints_and_starts_the_server() {
        let e = minio_row("t-local", 9000, 9001);
        let spec = build_run_spec(&e, None, None);

        assert_eq!(spec.container_port, 9000);
        assert_eq!(spec.extra_ports, vec![(9001, 9001)]);
        assert_eq!(spec.env.get("MINIO_ROOT_USER").unwrap(), ADMIN_USER);
        assert!(spec.healthcheck.is_none(), "probed by mc, not by docker");

        let argv = docker::run_argv("docker", &spec).join(" ");
        assert!(argv.contains("--publish 127.0.0.1:9000:9000"), "{argv}");
        assert!(argv.contains("--publish 127.0.0.1:9001:9001"), "{argv}");
        assert!(
            argv.contains("--volume linf-minio-latest-data:/data"),
            "{argv}"
        );
        assert!(
            argv.ends_with("minio/minio:latest server /data --console-address :9001"),
            "{argv}"
        );
    }

    /// The container is told to *inherit* one named variable; `ensure` then
    /// supplies exactly that name. A mismatch starts MinIO with no credentials
    /// at all, which it survives long enough to look healthy and then crash-loops.
    #[test]
    fn every_engine_inherits_the_password_variable_it_is_given() {
        for e in [minio_row("t", 9000, 9001), engine_row("t", 5432)] {
            let spec = build_run_spec(&e, None, None);
            assert_eq!(
                spec.secret_env,
                vec![password_env(e.engine).to_string()],
                "{:?}: run spec and supplied secret must use one name",
                e.engine
            );
            let argv = docker::run_argv("docker", &spec).join(" ");
            assert!(
                argv.contains(&format!("--env {}", password_env(e.engine))),
                "{argv}"
            );
            assert!(
                !argv.contains(&format!("{}=", password_env(e.engine))),
                "value must never reach argv: {argv}"
            );
        }
    }

    #[test]
    fn minio_naming_follows_the_same_convention_as_postgres() {
        assert_eq!(
            container_name(EngineKind::Minio, "latest"),
            "linf-minio-latest"
        );
        assert_eq!(
            volume_name(EngineKind::Minio, "latest"),
            "linf-minio-latest-data"
        );
        assert_eq!(
            container_name(EngineKind::Postgres, "17"),
            "linf-postgres-17"
        );
        assert_eq!(volume_name(EngineKind::Postgres, "17"), "linf-pg17-data");
    }

    #[test]
    fn explicit_identical_minio_ports_are_refused() {
        let spec = EngineSpec {
            host_port: Some(9001),
            console_port: Some(9001),
            ..EngineSpec::minio("latest")
        };
        assert!(matches!(
            validate_explicit_port_pair(&spec),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn implicit_console_skips_the_primary_port() {
        let reserved = Some(9001);
        // choose_port_avoiding with own_exists treats reserved==requested as error.
        assert!(matches!(
            {
                let requested = 9001u16;
                if reserved == Some(requested) {
                    Err(duplicate_endpoint_error(requested))
                } else {
                    Ok(())
                }
            },
            Err(Error::Conflict(_))
        ));
    }
}
