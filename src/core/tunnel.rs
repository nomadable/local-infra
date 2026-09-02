//! Detached SSH local port forwarding (PRD §6.5, §8.5, decision §19.4).
//!
//! A tunnel is not a child of the UI. `ssh -N -L` is spawned into its own
//! session with `setsid(2)`, its pid is written to a file this app owns, and
//! the session row in SQLite is the record of what *should* be running. The two
//! are reconciled at startup (TUN-007), so quitting the TUI never breaks a
//! development loop and a crashed tunnel is never reported as healthy.
//!
//! Nothing here reaches for `ssh -f`: that forks away the pid we would need in
//! order to reconcile, and there is no daemon to ask instead.

use crate::core::activity::Activity;
use crate::core::config::harden_file;
use crate::core::ctx::Ctx;
use crate::core::error::{Diagnostic, Error, Result};
use crate::core::exec::{Executor, SshConfig};
use crate::core::model::{
    EngineInstance, ManagedBucket, ManagedDatabase, ResourceKind, Target, TunnelSession,
    TunnelStatus,
};
use crate::core::util::{local_port_free, new_id, now, redact, shell_join};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Tunnels only ever listen on loopback (§6.4).
const LOCAL_HOST: &str = "127.0.0.1";
/// How long a forward may take to start accepting connections (TUN-006).
const READY_TIMEOUT_SECS: u64 = 20;
const READY_POLL_MS: u64 = 100;
/// Grace period between `SIGTERM` and `SIGKILL`.
const STOP_GRACE_MS: u64 = 3_000;
const KILL_GRACE_MS: u64 = 1_000;
const CONNECT_TIMEOUT_MS: u64 = 300;

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

/// The exact `ssh` invocation used for a forward.
///
/// `ExitOnForwardFailure=yes` is what makes readiness meaningful: without it
/// `ssh` happily stays up after failing to bind and the port never answers.
/// The keepalive pair turns a silently dead TCP session into a process exit,
/// which is what [`reconcile`] detects (TUN-005).
pub fn ssh_tunnel_argv(
    cfg: &SshConfig,
    local_host: &str,
    local_port: u16,
    remote_host: &str,
    remote_port: u16,
) -> Vec<String> {
    let mut argv = vec!["ssh".to_string()];
    argv.extend(cfg.base_options());
    argv.push("-N".into());
    argv.push("-L".into());
    argv.push(format!(
        "{local_host}:{local_port}:{remote_host}:{remote_port}"
    ));
    argv.push("-o".into());
    argv.push("ExitOnForwardFailure=yes".into());
    argv.push("-o".into());
    argv.push("ServerAliveInterval=15".into());
    argv.push("-o".into());
    argv.push("ServerAliveCountMax=3".into());
    argv.push(cfg.destination());
    argv
}

/// Where the forward lands *on the remote host*. The engine is bound to
/// loopback there (ENG-008), and a wildcard bind is still reached over
/// loopback from inside the box.
pub(crate) fn remote_endpoint(engine: &EngineInstance) -> (String, u16) {
    let host = match engine.bind_address.trim() {
        "" | "0.0.0.0" | "::" | "[::]" | "*" => LOCAL_HOST,
        other => other,
    };
    (host.to_string(), engine.host_port)
}

// ---------------------------------------------------------------------------
// Port selection
// ---------------------------------------------------------------------------

/// The project resource a forward serves. Keeping the tunnel module blind to
/// what is on the other end is what lets one implementation serve PostgreSQL
/// databases and MinIO buckets alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelTarget {
    pub resource_id: String,
    pub resource_kind: ResourceKind,
    /// Name shown in messages and in the tunnels table.
    pub label: String,
    pub engine_instance_id: String,
    pub preferred_local_port: Option<u16>,
}

impl TunnelTarget {
    pub fn database(db: &ManagedDatabase) -> Self {
        Self {
            resource_id: db.id.clone(),
            resource_kind: ResourceKind::Database,
            label: db.database_name.clone(),
            engine_instance_id: db.engine_instance_id.clone(),
            preferred_local_port: db.preferred_local_tunnel_port,
        }
    }

    pub fn bucket(bucket: &ManagedBucket) -> Self {
        Self {
            resource_id: bucket.id.clone(),
            resource_kind: ResourceKind::Bucket,
            label: bucket.bucket_name.clone(),
            engine_instance_id: bucket.engine_instance_id.clone(),
            preferred_local_port: bucket.preferred_local_tunnel_port,
        }
    }
}

/// Stable port for this resource when possible, otherwise the first free port
/// in the configured range (TUN-002/009).
pub fn choose_local_port(ctx: &Ctx, resource: &TunnelTarget) -> Result<u16> {
    let reserved: Vec<u16> = ctx
        .store
        .reserved_tunnel_ports()?
        .into_iter()
        .filter(|p| Some(*p) != resource.preferred_local_port)
        .collect();
    let start = ctx.config.tunnel.port_range_start;
    let span = ctx.config.tunnel.port_range_span;

    select_port(
        resource.preferred_local_port,
        &reserved,
        start,
        span,
        &|port| local_port_free(LOCAL_HOST, port),
    )
    .ok_or_else(|| {
        Error::Conflict(format!(
            "사용 가능한 로컬 포트를 찾지 못했습니다 ({start}–{}). \
             사용 중인 터널을 정리하거나 설정의 `tunnel.port_range_start`를 조정하세요.",
            start.saturating_add(span)
        ))
    })
}

/// Pure half of [`choose_local_port`]: `reserved` holds ports other resources
/// have claimed, `is_free` answers whether the OS will let us bind.
pub(crate) fn select_port(
    preferred: Option<u16>,
    reserved: &[u16],
    start: u16,
    span: u16,
    is_free: &dyn Fn(u16) -> bool,
) -> Option<u16> {
    if let Some(port) = preferred {
        if port != 0 && !reserved.contains(&port) && is_free(port) {
            return Some(port);
        }
    }
    (start..=start.saturating_add(span))
        .find(|port| *port != 0 && !reserved.contains(port) && is_free(*port))
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Open a forward for a project resource, detached from this process
/// (TUN-001/004).
pub async fn start(
    ctx: &Ctx,
    resource: &TunnelTarget,
    engine: &EngineInstance,
    target: &Target,
) -> Result<TunnelSession> {
    ctx.require_write_lock()?;
    if !target.is_remote() {
        return Err(Error::Usage(format!(
            "Target `{}`은(는) 로컬이므로 SSH 터널이 필요하지 않습니다. 엔진 포트로 바로 접속하세요.",
            target.display_name
        )));
    }
    if engine.target_id != target.id {
        return Err(Error::Usage(
            "엔진이 해당 Target에 속해 있지 않습니다.".into(),
        ));
    }
    if resource.engine_instance_id != engine.id {
        return Err(Error::Usage(format!(
            "`{}`이(가) 해당 엔진에 속해 있지 않습니다.",
            resource.label
        )));
    }

    // Already up and answering: reuse it instead of leaking a second process.
    if let Some(existing) = ctx.store.latest_tunnel(&resource.resource_id)? {
        if existing.status == TunnelStatus::Active && session_live(&existing) {
            return Ok(existing);
        }
    }

    let executor = Executor::for_target(target)?;
    let cfg = executor.ssh().cloned().ok_or_else(|| {
        Error::Usage(format!(
            "Target `{}`의 SSH 설정을 구성할 수 없습니다.",
            target.display_name
        ))
    })?;

    let local_port = choose_local_port(ctx, resource)?;
    let (remote_host, remote_port) = remote_endpoint(engine);
    let id = new_id();
    let session = TunnelSession {
        pid_file_path: ctx.pid_file(&id).display().to_string(),
        id,
        resource_id: resource.resource_id.clone(),
        resource_kind: resource.resource_kind,
        local_host: LOCAL_HOST.to_string(),
        local_port,
        remote_host: remote_host.clone(),
        remote_port,
        pid: None,
        status: TunnelStatus::Active,
        started_at: now(),
        stopped_at: None,
    };
    let argv = ssh_tunnel_argv(&cfg, LOCAL_HOST, local_port, &remote_host, remote_port);

    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "tunnel",
        "start",
        format!(
            "`{}` 터널 시작 {LOCAL_HOST}:{local_port} → {} {remote_host}:{remote_port}",
            resource.label, target.display_name
        ),
    )?
    .on_target(&target.id)
    .on_resource(&resource.resource_id);

    let result = start_steps(ctx, session, &argv, &mut activity).await;
    activity.finish(&result);
    result
}

async fn start_steps(
    ctx: &Ctx,
    mut session: TunnelSession,
    argv: &[String],
    activity: &mut Activity<'_>,
) -> Result<TunnelSession> {
    let mut child = spawn_detached(argv)?;
    let pid = child.id() as i32;
    session.pid = Some(pid);
    let signature = process_signature(pid);

    if let Err(e) = write_pid_file(Path::new(&session.pid_file_path), pid, signature.as_deref()) {
        kill_group(pid, libc::SIGKILL);
        let _ = child.wait();
        return Err(e);
    }
    if let Err(e) = ctx.store.upsert_tunnel(&session) {
        kill_group(pid, libc::SIGKILL);
        let _ = child.wait();
        let _ = std::fs::remove_file(&session.pid_file_path);
        return Err(e);
    }
    activity.step(format!(
        "ssh 프로세스를 분리 실행했습니다 (pid {pid}, 로컬 포트 {}).",
        session.local_port
    ));

    match wait_until_ready(&mut child, &session).await {
        Ok(()) => {
            activity.step("로컬 포트가 연결을 수락합니다.");
            Ok(session)
        }
        Err(cause) => {
            kill_group(pid, libc::SIGKILL);
            let _ = child.wait();
            let _ = std::fs::remove_file(&session.pid_file_path);
            session.status = TunnelStatus::Failed;
            session.stopped_at = Some(now());
            session.pid = None;
            let _ = ctx.store.upsert_tunnel(&session);
            Err(Error::diagnostic(
                Diagnostic::new(
                    format!(
                        "SSH 터널을 열지 못했습니다 (로컬 포트 {})",
                        session.local_port
                    ),
                    cause,
                    "`linf target test`로 SSH 연결과 Docker 권한을 각각 확인하고, \
                     원격 엔진이 실행 중인지 점검한 뒤 다시 시도하세요.",
                )
                .with_command(redact(&shell_join(argv))),
            ))
        }
    }
}

/// Stop a tunnel this app started.
///
/// A pid alone is not proof of ownership — pids are reused. The pid file must
/// still name the same process before any signal is sent.
pub async fn stop(ctx: &Ctx, s: &TunnelSession) -> Result<()> {
    ctx.require_write_lock()?;
    let mut session = s.clone();
    let path = PathBuf::from(&session.pid_file_path);

    let pid = match session.pid {
        Some(pid) if pid > 1 => pid,
        // Nothing to signal; only the record is out of date.
        _ => return mark_stopped(ctx, &mut session),
    };

    match check_pid_file(std::fs::read_to_string(&path).ok().as_deref(), pid) {
        PidFileCheck::Matches => {}
        PidFileCheck::Mismatch(other) => {
            return Err(Error::Refused(format!(
                "PID 파일 `{}`에 기록된 프로세스({other})가 이 터널 세션의 pid({pid})와 다릅니다. \
                 엉뚱한 프로세스를 종료할 위험이 있어 신호를 보내지 않았습니다. \
                 `linf tunnel status`로 상태를 정리한 뒤 다시 시도하세요.",
                path.display()
            )));
        }
        PidFileCheck::Missing => {
            if is_alive(pid) {
                return Err(Error::Refused(format!(
                    "PID 파일 `{}`이(가) 없어 pid {pid}가 이 앱이 만든 터널인지 확인할 수 없습니다. \
                     엉뚱한 프로세스를 종료할 위험이 있어 신호를 보내지 않았습니다. \
                     해당 프로세스를 직접 확인해 종료한 뒤 `linf tunnel status`를 실행하세요.",
                    path.display()
                )));
            }
            return mark_stopped(ctx, &mut session);
        }
    }

    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "tunnel",
        "stop",
        format!("터널 중지 (로컬 포트 {}, pid {pid})", session.local_port),
    )?
    .on_resource(&session.resource_id);

    let result = stop_steps(ctx, &mut session, pid, &path, &mut activity).await;
    activity.finish(&result);
    result
}

/// Stop and forget every tunnel recorded for a project resource.
pub async fn stop_for_resource(ctx: &Ctx, resource_id: &str) -> Result<()> {
    let sessions: Vec<TunnelSession> = ctx
        .store
        .list_tunnels()?
        .into_iter()
        .filter(|s| s.resource_id == resource_id)
        .collect();
    let mut first_err = None;
    for session in sessions {
        if session.status == TunnelStatus::Active {
            if let Err(e) = stop(ctx, &session).await {
                first_err = first_err.or(Some(e));
            }
        }
        let _ = std::fs::remove_file(&session.pid_file_path);
        if let Err(e) = ctx.store.delete_tunnel(&session.id) {
            first_err = first_err.or(Some(e));
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Stop tunnels of every database and bucket on an engine before the engine
/// row (and its children) disappear.
pub async fn stop_for_engine(ctx: &Ctx, engine_id: &str) -> Result<()> {
    let mut ids: Vec<String> = ctx
        .store
        .list_databases_for_engine(engine_id)?
        .into_iter()
        .map(|d| d.id)
        .collect();
    ids.extend(
        ctx.store
            .list_buckets_for_engine(engine_id)?
            .into_iter()
            .map(|b| b.id),
    );
    let mut first_err = None;
    for id in ids {
        if let Err(e) = stop_for_resource(ctx, &id).await {
            first_err = first_err.or(Some(e));
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn stop_steps(
    ctx: &Ctx,
    session: &mut TunnelSession,
    pid: i32,
    path: &Path,
    activity: &mut Activity<'_>,
) -> Result<()> {
    kill_group(pid, libc::SIGTERM);
    activity.step(format!("프로세스 그룹 {pid}에 SIGTERM을 보냈습니다."));

    if !wait_until_gone(pid, Duration::from_millis(STOP_GRACE_MS)).await {
        kill_group(pid, libc::SIGKILL);
        activity.step("응답이 없어 SIGKILL로 종료했습니다.");
        if !wait_until_gone(pid, Duration::from_millis(KILL_GRACE_MS)).await {
            return Err(Error::failed(
                format!("터널 프로세스(pid {pid})를 종료하지 못했습니다"),
                "SIGTERM과 SIGKILL을 보냈지만 프로세스가 남아 있습니다.",
                format!("`kill -9 {pid}`로 직접 종료한 뒤 `linf tunnel status`를 실행하세요."),
            ));
        }
    }

    let _ = std::fs::remove_file(path);
    session.status = TunnelStatus::Stopped;
    session.stopped_at = Some(now());
    session.pid = None;
    ctx.store.upsert_tunnel(session)?;
    activity.step("터널을 stopped로 기록하고 PID 파일을 정리했습니다.");
    Ok(())
}

pub async fn restart(
    ctx: &Ctx,
    resource: &TunnelTarget,
    engine: &EngineInstance,
    target: &Target,
) -> Result<TunnelSession> {
    if let Some(existing) = ctx.store.latest_tunnel(&resource.resource_id)? {
        if existing.status == TunnelStatus::Active {
            stop(ctx, &existing).await?;
        }
    }
    start(ctx, resource, engine, target).await
}

/// Does a process with this pid exist?
///
/// Finished children are reaped first: a zombie still answers `kill(pid, 0)`
/// and would otherwise be reported as a live tunnel forever.
pub fn is_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    reap(pid);
    // SAFETY: signal 0 performs the existence and permission check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Compare the recorded sessions with reality (TUN-007).
///
/// Returns every previously-active session with its corrected status, so the
/// caller can show what changed while the app was not running.
pub async fn reconcile(ctx: &Ctx) -> Result<Vec<TunnelSession>> {
    let writable = ctx.has_write_lock();
    let mut corrected = Vec::new();

    for mut session in ctx.store.active_tunnels()? {
        let pid = session.pid.unwrap_or(0);
        let alive = is_alive(pid);
        let bound = !local_port_free(&session.local_host, session.local_port);
        let owned = check_pid_file(
            std::fs::read_to_string(&session.pid_file_path)
                .ok()
                .as_deref(),
            pid,
        ) == PidFileCheck::Matches;
        if alive && bound && owned {
            corrected.push(session);
            continue;
        }

        // A live process that is no longer forwarding is ours to clean up —
        // but only once the pid file proves it is ours.
        if alive && owned {
            kill_group(pid, libc::SIGTERM);
        }
        if owned {
            let _ = std::fs::remove_file(&session.pid_file_path);
        }

        session.status = TunnelStatus::Failed;
        session.stopped_at = Some(now());
        session.pid = None;
        if writable {
            let _ = ctx.store.upsert_tunnel(&session);
        }
        corrected.push(session);
    }
    Ok(corrected)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TunnelView {
    pub session: TunnelSession,
    /// Database or bucket name.
    pub resource_name: String,
    pub resource_kind: ResourceKind,
    pub target_name: String,
}

/// Current session per project resource, newest first (TUN-003).
pub async fn status(ctx: &Ctx) -> Result<Vec<TunnelView>> {
    let _ = reconcile(ctx).await?;
    let target_names: HashMap<String, String> = ctx
        .store
        .list_targets()?
        .into_iter()
        .map(|t| (t.id, t.display_name))
        .collect();
    let engine_targets: HashMap<String, String> = ctx
        .store
        .list_engines()?
        .into_iter()
        .map(|e| (e.id, e.target_id))
        .collect();

    let mut resources: Vec<TunnelTarget> = ctx
        .store
        .list_databases()?
        .iter()
        .map(TunnelTarget::database)
        .collect();
    resources.extend(ctx.store.list_buckets()?.iter().map(TunnelTarget::bucket));

    let mut views = Vec::new();
    for resource in resources {
        let Some(session) = ctx.store.latest_tunnel(&resource.resource_id)? else {
            continue;
        };
        let target_name = engine_targets
            .get(&resource.engine_instance_id)
            .and_then(|target_id| target_names.get(target_id))
            .cloned()
            .unwrap_or_else(|| "(알 수 없음)".to_string());
        views.push(TunnelView {
            session,
            resource_name: resource.label,
            resource_kind: resource.resource_kind,
            target_name,
        });
    }
    views.sort_by_key(|v| std::cmp::Reverse(v.session.started_at));
    Ok(views)
}

/// Honours `tunnel.keep_alive_on_exit` (decision §19.7): the default is to
/// leave the forwards running so quitting the TUI does not break the dev loop.
pub async fn shutdown_for_exit(ctx: &Ctx) -> Result<()> {
    if ctx.config.tunnel.keep_alive_on_exit {
        return Ok(());
    }
    for session in ctx.store.active_tunnels()? {
        // Best effort: an exit path must not fail because one tunnel is stuck.
        let _ = stop(ctx, &session).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

/// `ssh` in its own session, with no stdio, so it outlives this process.
fn spawn_detached(argv: &[String]) -> Result<Child> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and is the only work done between
    // fork and exec, so it is legal in the child half of a multi-threaded
    // parent. It detaches the tunnel from this process's controlling terminal
    // and makes it a process-group leader, so `kill(-pid, …)` reaches exactly
    // this tunnel and nothing else.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| {
        Error::diagnostic(
            Diagnostic::new(
                "SSH 터널 프로세스를 실행할 수 없습니다",
                e.to_string(),
                "`ssh` 명령이 설치되어 있고 PATH에 있는지 확인하세요.",
            )
            .with_command(redact(&shell_join(argv))),
        )
    })
}

fn write_pid_file(path: &Path, pid: i32, signature: Option<&str>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut body = format!("{pid}\n");
    if let Some(sig) = signature {
        body.push_str(sig);
        body.push('\n');
    }
    std::fs::write(path, body)?;
    harden_file(path)?;
    Ok(())
}

/// `ps -p PID -o lstart=` — a pid number is reused; the start time is not.
fn process_signature(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidFileCheck {
    /// The file names exactly the process we recorded.
    Matches,
    /// The file names a different process — do not signal it.
    Mismatch(i32),
    /// No file, or unreadable content, or a pid-only legacy file.
    Missing,
}

pub(crate) fn parse_pid_file(content: &str) -> Option<(i32, Option<String>)> {
    let mut lines = content.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    if pid <= 1 {
        return None;
    }
    let sig = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((pid, sig))
}

pub(crate) fn check_pid_file(content: Option<&str>, pid: i32) -> PidFileCheck {
    check_pid_file_with(content, pid, process_signature)
}

fn check_pid_file_with(
    content: Option<&str>,
    pid: i32,
    signature_of: impl Fn(i32) -> Option<String>,
) -> PidFileCheck {
    let Some(raw) = content else {
        return PidFileCheck::Missing;
    };
    let Some((recorded, sig)) = parse_pid_file(raw) else {
        return PidFileCheck::Missing;
    };
    if recorded != pid {
        return PidFileCheck::Mismatch(recorded);
    }
    match sig {
        Some(expected) => match signature_of(pid) {
            Some(actual) if actual == expected => PidFileCheck::Matches,
            Some(_) => PidFileCheck::Mismatch(pid),
            None => PidFileCheck::Missing,
        },
        // A pid-only file cannot prove identity after reuse. Refuse to signal.
        None => PidFileCheck::Missing,
    }
}

/// Signal the whole process group. `setsid` made pgid == pid; if the group is
/// already gone, fall back to the pid so a reparented straggler still dies.
fn kill_group(pid: i32, signal: i32) {
    if pid <= 1 {
        return;
    }
    // SAFETY: both calls are plain `kill(2)`; failures are expected and ignored.
    unsafe {
        if libc::kill(-pid, signal) == -1 {
            libc::kill(pid, signal);
        }
    }
}

/// Collect a finished child so it does not linger as a zombie. `ECHILD` for a
/// pid that is not ours is the expected, harmless case.
fn reap(pid: i32) {
    let mut status: libc::c_int = 0;
    // SAFETY: `waitpid` with `WNOHANG` never blocks and reports an error rather
    // than touching a process we do not own.
    unsafe {
        libc::waitpid(pid, &mut status, libc::WNOHANG);
    }
}

fn mark_stopped(ctx: &Ctx, session: &mut TunnelSession) -> Result<()> {
    session.status = TunnelStatus::Stopped;
    session.stopped_at = Some(now());
    session.pid = None;
    ctx.store.upsert_tunnel(session)
}

fn session_live(session: &TunnelSession) -> bool {
    session.pid.map(is_alive).unwrap_or(false)
        && !local_port_free(&session.local_host, session.local_port)
}

/// TUN-006: the tunnel is only "active" once the local port answers.
async fn wait_until_ready(
    child: &mut Child,
    session: &TunnelSession,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(READY_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "ssh 프로세스가 곧바로 종료되었습니다 (종료 코드 {}). \
                     로컬 포트 {}이(가) 이미 사용 중이거나, SSH 인증 또는 호스트 키 검증에 실패했을 수 있습니다.",
                    status.code().unwrap_or(-1),
                    session.local_port
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("ssh 프로세스 상태를 확인할 수 없습니다: {e}")),
        }
        if port_accepts(&session.local_host, session.local_port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{READY_TIMEOUT_SECS}초 안에 로컬 포트 {}이(가) 연결을 수락하지 않았습니다.",
                session.local_port
            ));
        }
        tokio::time::sleep(Duration::from_millis(READY_POLL_MS)).await;
    }
}

async fn wait_until_gone(pid: i32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if !is_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn port_accepts(host: &str, port: u16) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};

    let Some(addr) = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
    else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(CONNECT_TIMEOUT_MS)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::EngineKind;
    use std::path::PathBuf;

    fn cfg(identity: Option<&str>) -> SshConfig {
        SshConfig {
            host: "vps.example.com".into(),
            port: 2222,
            user: Some("devdb".into()),
            identity: identity.map(PathBuf::from),
        }
    }

    #[test]
    fn tunnel_argv_is_exact() {
        let argv = ssh_tunnel_argv(&cfg(None), "127.0.0.1", 15432, "127.0.0.1", 5432);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-N",
                "-L",
                "127.0.0.1:15432:127.0.0.1:5432",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "devdb@vps.example.com",
            ]
        );
    }

    #[test]
    fn tunnel_argv_pins_an_explicit_identity() {
        let argv = ssh_tunnel_argv(
            &cfg(Some("/home/dev/.ssh/id_ed25519")),
            "127.0.0.1",
            15433,
            "127.0.0.1",
            5432,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("-i /home/dev/.ssh/id_ed25519"), "{joined}");
        assert!(joined.contains("-o IdentitiesOnly=yes"), "{joined}");
        // The forward spec still comes after the base options.
        assert_eq!(argv.last().unwrap(), "devdb@vps.example.com");
    }

    #[test]
    fn host_key_checking_is_never_weakened() {
        for identity in [None, Some("/home/dev/.ssh/id_ed25519")] {
            let argv = ssh_tunnel_argv(&cfg(identity), "127.0.0.1", 15432, "127.0.0.1", 5432);
            let joined = argv.join(" ");
            assert!(
                joined.contains("StrictHostKeyChecking=yes"),
                "host key checking must stay on: {joined}"
            );
            assert!(
                !joined.contains("StrictHostKeyChecking=no"),
                "PRD §11.3 forbids disabling host key checking: {joined}"
            );
            assert!(
                !joined.contains("UserKnownHostsFile=/dev/null"),
                "known_hosts must not be bypassed: {joined}"
            );
            assert!(
                joined.contains("ExitOnForwardFailure=yes"),
                "a failed bind must exit, not linger: {joined}"
            );
        }
    }

    #[test]
    fn preferred_port_wins_when_free_and_unclaimed() {
        let all_free = |_: u16| true;
        assert_eq!(
            select_port(Some(15500), &[], 15432, 200, &all_free),
            Some(15500)
        );
    }

    #[test]
    fn ports_reserved_by_other_databases_are_skipped() {
        let all_free = |_: u16| true;

        // Another project already reserved this database's preferred port:
        // fall back into the range and skip everything else reserved.
        assert_eq!(
            select_port(Some(15432), &[15432, 15433, 15434], 15432, 200, &all_free),
            Some(15435),
            "a port claimed by another database is never handed out twice"
        );

        // No preference at all: still skips reservations.
        assert_eq!(
            select_port(None, &[15432], 15432, 200, &all_free),
            Some(15433)
        );

        // Reserved *and* busy are independent reasons to skip.
        let busy_15433 = |port: u16| port != 15433;
        assert_eq!(
            select_port(None, &[15432], 15432, 200, &busy_15433),
            Some(15434)
        );
    }

    #[test]
    fn a_busy_preferred_port_falls_back_to_the_range() {
        let busy_preferred = |port: u16| port != 15500;
        assert_eq!(
            select_port(Some(15500), &[], 15432, 200, &busy_preferred),
            Some(15432)
        );
    }

    #[test]
    fn an_exhausted_range_yields_nothing() {
        let none_free = |_: u16| false;
        assert_eq!(select_port(Some(15500), &[], 15432, 200, &none_free), None);

        let all_free = |_: u16| true;
        let reserved: Vec<u16> = (15432..=15434).collect();
        assert_eq!(
            select_port(None, &reserved, 15432, 2, &all_free),
            None,
            "a fully reserved range has no candidate"
        );
    }

    #[test]
    fn wildcard_binds_are_reached_over_loopback_from_inside_the_host() {
        let engine = |bind: &str| EngineInstance {
            id: "e-1".into(),
            target_id: "t-1".into(),
            engine: EngineKind::Postgres,
            major_version: "17".into(),
            image: "postgres:17".into(),
            container_name: "linf-postgres-17".into(),
            volume_name: "linf-pg17-data".into(),
            bind_address: bind.into(),
            host_port: 5432,
            console_port: None,
            admin_user: "linf_admin".into(),
            credential_ref: "engine:e-1".into(),
            managed: true,
            created_at: now(),
        };
        assert_eq!(
            remote_endpoint(&engine("127.0.0.1")),
            ("127.0.0.1".to_string(), 5432)
        );
        assert_eq!(
            remote_endpoint(&engine("0.0.0.0")),
            ("127.0.0.1".to_string(), 5432)
        );
        assert_eq!(
            remote_endpoint(&engine("")),
            ("127.0.0.1".to_string(), 5432)
        );
        assert_eq!(
            remote_endpoint(&engine("10.0.0.5")),
            ("10.0.0.5".to_string(), 5432)
        );
    }

    #[test]
    fn a_pid_is_only_signalled_when_the_pid_file_still_names_it() {
        let known = |pid: i32| (pid == 4242).then(|| "Wed Sep 1 12:00:00 2026".to_string());
        assert_eq!(
            check_pid_file_with(Some("4242\nWed Sep 1 12:00:00 2026\n"), 4242, known),
            PidFileCheck::Matches
        );
        assert_eq!(
            check_pid_file_with(Some("4242\nThu Jan 1 00:00:00 1970\n"), 4242, known),
            PidFileCheck::Mismatch(4242),
            "a reused pid with a different start time is not ours"
        );
        assert_eq!(
            check_pid_file_with(Some("9999\nWed Sep 1 12:00:00 2026\n"), 4242, known),
            PidFileCheck::Mismatch(9999)
        );
        assert_eq!(
            check_pid_file_with(Some("4242\n"), 4242, known),
            PidFileCheck::Missing,
            "a pid-only file cannot prove identity"
        );
        assert_eq!(
            check_pid_file_with(None, 4242, known),
            PidFileCheck::Missing
        );
        assert_eq!(
            check_pid_file_with(Some("not a pid"), 4242, known),
            PidFileCheck::Missing
        );
    }

    #[test]
    fn liveness_uses_signal_zero_and_rejects_impossible_pids() {
        assert!(is_alive(std::process::id() as i32), "this process is alive");
        assert!(!is_alive(0), "pid 0 is the process group, never a tunnel");
        assert!(!is_alive(1), "pid 1 is init, never a tunnel we spawned");
        assert!(!is_alive(-1), "a negative pid would signal a whole group");
    }
}
