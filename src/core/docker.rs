//! Docker CLI adapter (PRD §6.1 — the app shells out to `docker` rather than
//! talking to the API, so remote control is plain SSH and no daemon socket is
//! ever exposed, §6.4).
//!
//! Ownership rule (ENG-005/006, §11.4): this app only ever mutates containers
//! and volumes carrying [`LABEL_MANAGED`]. [`require_managed`] is the guard and
//! every destructive helper calls it.

use crate::core::error::{Diagnostic, Error, Result};
use crate::core::exec::{Executor, Output, SecretEnv};
use crate::core::model::{EngineStatus, ForeignContainer, Health};
use std::collections::BTreeMap;

pub const LABEL_MANAGED: &str = "local-infra.managed";
pub const LABEL_TARGET: &str = "local-infra.target-id";
pub const LABEL_ENGINE: &str = "local-infra.engine";
pub const LABEL_MAJOR: &str = "local-infra.major-version";

// ---------------------------------------------------------------------------
// Engine reachability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerInfo {
    pub client_version: Option<String>,
    pub server_version: Option<String>,
    pub reachable: bool,
}

/// Diagnose the Docker CLI and daemon (TAR-002). Never fails for an
/// unreachable daemon — that is a *result*, not an error.
pub async fn info(x: &Executor) -> Result<DockerInfo> {
    let argv = vec![
        x.docker_bin().to_string(),
        "version".into(),
        "--format".into(),
        // Not a tab: the docker CLI pipes `--format` output through a
        // tabwriter, which turns tabs into alignment spaces and would leave
        // the two versions unsplittable.
        "{{.Client.Version}}|{{.Server.Version}}".into(),
    ];
    let out = x.run(&argv).await?;
    if out.ok() {
        let text = out.stdout_str();
        let mut parts = text.splitn(2, '|');
        let client = parts.next().unwrap_or("").trim().to_string();
        let server = parts.next().unwrap_or("").trim().to_string();
        return Ok(DockerInfo {
            client_version: (!client.is_empty()).then_some(client),
            server_version: (!server.is_empty()).then_some(server.clone()),
            reachable: !server.is_empty(),
        });
    }
    // A stopped daemon still lets `docker version` print the client half.
    let client = x
        .run(&[
            x.docker_bin().to_string(),
            "version".into(),
            "--format".into(),
            "{{.Client.Version}}".into(),
        ])
        .await
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout_str())
        .filter(|s| !s.is_empty());
    Ok(DockerInfo {
        client_version: client,
        server_version: None,
        reachable: false,
    })
}

/// Fail with an actionable diagnostic when the daemon cannot be reached.
pub async fn require_daemon(x: &Executor) -> Result<DockerInfo> {
    let info = info(x).await?;
    if info.reachable {
        return Ok(info);
    }
    let next = if x.is_remote() {
        "원격 호스트에서 Docker 데몬이 실행 중인지, 해당 SSH 사용자가 Docker를 실행할 권한이 있는지 확인하세요."
    } else {
        "Docker Desktop 또는 Docker Engine을 시작한 뒤 다시 시도하세요."
    };
    let cause = match &info.client_version {
        Some(v) => format!("Docker CLI {v}은(는) 있지만 데몬에 연결할 수 없습니다."),
        None => "Docker CLI를 찾을 수 없습니다.".to_string(),
    };
    Err(Error::diagnostic(
        Diagnostic::new("Docker에 연결할 수 없습니다", cause, next)
            .with_command(x.describe(&[x.docker_bin().to_string(), "version".into()])),
    ))
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

/// Live state of one container. A missing container is `EngineStatus::missing`,
/// not an error (ENG-004).
pub async fn container_status(x: &Executor, name: &str) -> Result<EngineStatus> {
    let argv = vec![
        x.docker_bin().to_string(),
        "inspect".into(),
        "--type".into(),
        "container".into(),
        "--format".into(),
        "{{.State.Status}}\t{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}\t{{.Config.Image}}\t{{.State.StartedAt}}".into(),
        name.to_string(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(EngineStatus::missing());
    }
    let text = out.stdout_str();
    let mut parts = text.split('\t');
    let state = parts.next().unwrap_or("unknown").trim().to_string();
    let health = match parts.next().unwrap_or("none").trim() {
        "healthy" => Health::Healthy,
        "unhealthy" => Health::Unhealthy,
        "starting" => Health::Starting,
        _ => Health::None,
    };
    let image = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let started_at = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(EngineStatus {
        exists: true,
        running: state == "running",
        state,
        health,
        image,
        started_at,
    })
}

pub async fn container_labels(x: &Executor, name: &str) -> Result<BTreeMap<String, String>> {
    let argv = vec![
        x.docker_bin().to_string(),
        "inspect".into(),
        "--type".into(),
        "container".into(),
        "--format".into(),
        "{{range $k, $v := .Config.Labels}}{{$k}}={{$v}}\n{{end}}".into(),
        name.to_string(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(BTreeMap::new());
    }
    Ok(parse_labels(&out.stdout_str()))
}

fn parse_labels(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

pub async fn is_managed(x: &Executor, name: &str) -> Result<bool> {
    Ok(container_labels(x, name)
        .await?
        .get(LABEL_MANAGED)
        .map(|v| v == "true")
        .unwrap_or(false))
}

/// The guard behind every destructive container operation. Re-reads the live
/// labels rather than trusting the local database (ENG-006).
pub async fn require_managed(x: &Executor, name: &str) -> Result<()> {
    let status = container_status(x, name).await?;
    if !status.exists {
        return Err(Error::NotFound(format!(
            "컨테이너 `{name}`을(를) 찾을 수 없습니다."
        )));
    }
    if is_managed(x, name).await? {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "컨테이너 `{name}`은(는) local-infra가 생성하지 않았습니다. \
         이 앱은 `{LABEL_MANAGED}=true` label이 있는 리소스만 변경합니다."
    )))
}

/// Everything needed to create an engine container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    pub container_name: String,
    pub image: String,
    pub volume_name: String,
    /// Mount point inside the container.
    pub data_dir: String,
    pub bind_address: String,
    pub host_port: u16,
    pub container_port: u16,
    /// Extra `(host, container)` publications, e.g. MinIO's web console.
    pub extra_ports: Vec<(u16, u16)>,
    pub labels: BTreeMap<String, String>,
    /// Non-secret environment, e.g. `POSTGRES_USER`, `POSTGRES_DB`.
    pub env: BTreeMap<String, String>,
    /// Names of secret variables to pass through from the environment.
    /// Their *values* travel via [`SecretEnv`], never argv.
    pub secret_env: Vec<String>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    pub healthcheck: Option<Vec<String>>,
    /// Arguments placed after the image, replacing its default command.
    pub command: Vec<String>,
}

/// Build the `docker run` argv. Pure, so the exact invocation is testable.
pub fn run_argv(docker_bin: &str, spec: &RunSpec) -> Vec<String> {
    let mut argv = vec![
        docker_bin.to_string(),
        "run".into(),
        "--detach".into(),
        "--name".into(),
        spec.container_name.clone(),
        "--restart".into(),
        "unless-stopped".into(),
        "--publish".into(),
        format!(
            "{}:{}:{}",
            spec.bind_address, spec.host_port, spec.container_port
        ),
        "--volume".into(),
        format!("{}:{}", spec.volume_name, spec.data_dir),
    ];
    for (host, container) in &spec.extra_ports {
        argv.push("--publish".into());
        argv.push(format!("{}:{host}:{container}", spec.bind_address));
    }
    for (k, v) in &spec.labels {
        argv.push("--label".into());
        argv.push(format!("{k}={v}"));
    }
    for (k, v) in &spec.env {
        argv.push("--env".into());
        argv.push(format!("{k}={v}"));
    }
    for name in &spec.secret_env {
        // Value-less `--env NAME` makes the docker client read it from its own
        // environment, so the secret never appears on a command line.
        argv.push("--env".into());
        argv.push(name.clone());
    }
    if let Some(cpus) = &spec.cpu_limit {
        argv.push("--cpus".into());
        argv.push(cpus.clone());
    }
    if let Some(mem) = &spec.memory_limit {
        argv.push("--memory".into());
        argv.push(mem.clone());
    }
    if let Some(test) = &spec.healthcheck {
        argv.push("--health-cmd".into());
        argv.push(test.join(" "));
        argv.push("--health-interval".into());
        argv.push("5s".into());
        argv.push("--health-retries".into());
        argv.push("12".into());
        argv.push("--health-start-period".into());
        argv.push("5s".into());
    }
    argv.push(spec.image.clone());
    argv.extend(spec.command.iter().cloned());
    argv
}

pub async fn run_container(x: &Executor, spec: &RunSpec, secrets: &SecretEnv) -> Result<String> {
    let argv = run_argv(x.docker_bin(), spec);
    let out = x.run_secret(&argv, secrets, None).await?;
    if !out.ok() {
        return Err(x.failure(
            &argv,
            &out,
            &format!("컨테이너 `{}` 생성에 실패했습니다", spec.container_name),
            "포트 충돌과 이미지 이름을 확인한 뒤 다시 시도하세요.",
        ));
    }
    Ok(out.stdout_str())
}

pub async fn start_container(x: &Executor, name: &str) -> Result<()> {
    require_managed(x, name).await?;
    let argv = vec![x.docker_bin().to_string(), "start".into(), name.to_string()];
    x.run_checked(
        &argv,
        &format!("컨테이너 `{name}` 시작에 실패했습니다"),
        "`linf engine logs`로 원인을 확인하세요.",
    )
    .await?;
    Ok(())
}

pub async fn stop_container(x: &Executor, name: &str) -> Result<()> {
    require_managed(x, name).await?;
    let argv = vec![x.docker_bin().to_string(), "stop".into(), name.to_string()];
    x.run_checked(
        &argv,
        &format!("컨테이너 `{name}` 중지에 실패했습니다"),
        "Docker 상태를 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

pub async fn restart_container(x: &Executor, name: &str) -> Result<()> {
    require_managed(x, name).await?;
    let argv = vec![
        x.docker_bin().to_string(),
        "restart".into(),
        name.to_string(),
    ];
    x.run_checked(
        &argv,
        &format!("컨테이너 `{name}` 재시작에 실패했습니다"),
        "`linf engine logs`로 원인을 확인하세요.",
    )
    .await?;
    Ok(())
}

pub async fn remove_container(x: &Executor, name: &str, force: bool) -> Result<()> {
    require_managed(x, name).await?;
    let mut argv = vec![x.docker_bin().to_string(), "rm".into()];
    if force {
        argv.push("--force".into());
    }
    argv.push(name.to_string());
    x.run_checked(
        &argv,
        &format!("컨테이너 `{name}` 삭제에 실패했습니다"),
        "컨테이너를 먼저 중지한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

pub async fn logs(x: &Executor, name: &str, tail: usize) -> Result<String> {
    let argv = vec![
        x.docker_bin().to_string(),
        "logs".into(),
        "--tail".into(),
        tail.to_string(),
        name.to_string(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Err(x.failure(
            &argv,
            &out,
            &format!("컨테이너 `{name}`의 로그를 읽을 수 없습니다"),
            "컨테이너가 존재하는지 확인하세요.",
        ));
    }
    Ok(
        crate::core::util::redact(&format!("{}\n{}", out.stderr_str(), out.stdout_str()))
            .trim()
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

/// Build a `docker exec` argv.
///
/// `env_passthrough` uses the value-less `--env NAME` form so secrets are read
/// from the docker client's environment instead of appearing in argv.
pub fn exec_argv(
    docker_bin: &str,
    container: &str,
    as_user: Option<&str>,
    env_passthrough: &[&str],
    argv: &[String],
) -> Vec<String> {
    let mut out = vec![
        docker_bin.to_string(),
        "exec".into(),
        "--interactive".into(),
    ];
    if let Some(user) = as_user {
        out.push("--user".into());
        out.push(user.to_string());
    }
    for name in env_passthrough {
        out.push("--env".into());
        out.push((*name).to_string());
    }
    out.push(container.to_string());
    out.extend(argv.iter().cloned());
    out
}

/// Run a command inside a container.
pub async fn exec(
    x: &Executor,
    container: &str,
    as_user: Option<&str>,
    argv: &[String],
    secrets: &SecretEnv,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let names: Vec<&str> = secrets.names();
    let full = exec_argv(x.docker_bin(), container, as_user, &names, argv);
    x.run_secret(&full, secrets, stdin).await
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

pub async fn volume_exists(x: &Executor, name: &str) -> Result<bool> {
    let argv = vec![
        x.docker_bin().to_string(),
        "volume".into(),
        "inspect".into(),
        name.to_string(),
    ];
    Ok(x.run(&argv).await?.ok())
}

/// Names of every volume on the target. Used by reset to catch leftovers
/// that SQLite no longer knows about.
pub async fn list_volume_names(x: &Executor) -> Result<Vec<String>> {
    let argv = vec![
        x.docker_bin().to_string(),
        "volume".into(),
        "ls".into(),
        "--format".into(),
        "{{.Name}}".into(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout_str()
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect())
}

pub async fn create_volume(
    x: &Executor,
    name: &str,
    labels: &BTreeMap<String, String>,
) -> Result<()> {
    let mut argv = vec![x.docker_bin().to_string(), "volume".into(), "create".into()];
    for (k, v) in labels {
        argv.push("--label".into());
        argv.push(format!("{k}={v}"));
    }
    argv.push(name.to_string());
    x.run_checked(
        &argv,
        &format!("볼륨 `{name}` 생성에 실패했습니다"),
        "Docker 상태와 디스크 공간을 확인하세요.",
    )
    .await?;
    Ok(())
}

pub async fn volume_labels(x: &Executor, name: &str) -> Result<BTreeMap<String, String>> {
    let argv = vec![
        x.docker_bin().to_string(),
        "volume".into(),
        "inspect".into(),
        "--format".into(),
        "{{range $k, $v := .Labels}}{{$k}}={{$v}}\n{{end}}".into(),
        name.to_string(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(BTreeMap::new());
    }
    Ok(parse_labels(&out.stdout_str()))
}

/// Volume deletion is irreversible, so the managed check is on the volume
/// itself, not on the engine that references it (PRD §11.4).
pub async fn remove_volume(x: &Executor, name: &str) -> Result<()> {
    if !volume_exists(x, name).await? {
        return Err(Error::NotFound(format!(
            "볼륨 `{name}`을(를) 찾을 수 없습니다."
        )));
    }
    let managed = volume_labels(x, name)
        .await?
        .get(LABEL_MANAGED)
        .map(|v| v == "true")
        .unwrap_or(false);
    if !managed {
        return Err(Error::Refused(format!(
            "볼륨 `{name}`은(는) local-infra가 생성하지 않았습니다. 삭제하지 않습니다."
        )));
    }
    let argv = vec![
        x.docker_bin().to_string(),
        "volume".into(),
        "rm".into(),
        name.to_string(),
    ];
    x.run_checked(
        &argv,
        &format!("볼륨 `{name}` 삭제에 실패했습니다"),
        "볼륨을 사용 중인 컨테이너를 먼저 제거하세요.",
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

pub async fn image_exists(x: &Executor, image: &str) -> Result<bool> {
    let argv = vec![
        x.docker_bin().to_string(),
        "image".into(),
        "inspect".into(),
        image.to_string(),
    ];
    Ok(x.run(&argv).await?.ok())
}

pub async fn pull_image(x: &Executor, image: &str) -> Result<()> {
    let argv = vec![x.docker_bin().to_string(), "pull".into(), image.to_string()];
    x.run_checked(
        &argv,
        &format!("이미지 `{image}`를 가져오지 못했습니다"),
        "이미지 이름과 네트워크 연결을 확인하세요.",
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ports and inventory
// ---------------------------------------------------------------------------

/// The container that publishes `port`, if any (ENG-007).
///
/// Stopped containers count. Their port is not bound right now, but taking it
/// would break the user's stack the next time they start it, and this app is
/// not entitled to do that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHolder {
    pub name: String,
    pub running: bool,
}

pub async fn port_holder(x: &Executor, port: u16) -> Result<Option<PortHolder>> {
    let argv = vec![
        x.docker_bin().to_string(),
        "ps".into(),
        "--all".into(),
        "--format".into(),
        "{{.Names}}\t{{.State}}\t{{.Ports}}".into(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(None);
    }
    Ok(find_port_holder(&out.stdout_str(), port))
}

/// Pure half of [`port_holder`]. A running holder wins over a stopped one, so
/// the message names the process that is actually in the way.
fn find_port_holder(text: &str, port: u16) -> Option<PortHolder> {
    let mut stopped = None;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(name), Some(state), Some(ports)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !publishes_host_port(ports, port) {
            continue;
        }
        let holder = PortHolder {
            name: name.trim().to_string(),
            running: state.trim() == "running",
        };
        if holder.running {
            return Some(holder);
        }
        stopped.get_or_insert(holder);
    }
    stopped
}

/// Host-side publication, including ranges (`9000-9001->…`) and IPv6.
pub fn publishes_host_port(ports: &str, port: u16) -> bool {
    for chunk in ports.split(',') {
        let host_side = chunk.split("->").next().unwrap_or(chunk).trim();
        let Some(colon) = host_side.rfind(':') else {
            continue;
        };
        let spec = &host_side[colon + 1..];
        if let Some((lo, hi)) = spec.split_once('-') {
            if let (Ok(lo), Ok(hi)) = (lo.parse::<u16>(), hi.parse::<u16>()) {
                if port >= lo.min(hi) && port <= lo.max(hi) {
                    return true;
                }
            }
        } else if spec.parse::<u16>() == Ok(port) {
            return true;
        }
    }
    false
}

/// Every container on the target, managed or not (MIG-001).
pub async fn list_containers(x: &Executor) -> Result<Vec<ForeignContainer>> {
    let argv = vec![
        x.docker_bin().to_string(),
        "ps".into(),
        "--all".into(),
        "--format".into(),
        "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Ports}}\t{{.Labels}}".into(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Err(x.failure(
            &argv,
            &out,
            "컨테이너 목록을 읽을 수 없습니다",
            "Docker 데몬 상태를 확인하세요.",
        ));
    }
    Ok(out.stdout_str().lines().filter_map(parse_ps_line).collect())
}

pub async fn list_managed_container_names(x: &Executor) -> Result<Vec<String>> {
    let argv = vec![
        x.docker_bin().to_string(),
        "ps".into(),
        "--all".into(),
        "--filter".into(),
        format!("label={LABEL_MANAGED}=true"),
        "--format".into(),
        "{{.Names}}".into(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout_str()
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect())
}

/// `id\tname\timage\tstate\tports\tlabels` → `(ForeignContainer, managed)`.
fn parse_ps_line(line: &str) -> Option<ForeignContainer> {
    let mut parts = line.split('\t');
    let id = parts.next()?.trim().to_string();
    let name = parts.next()?.trim().to_string();
    let image = parts.next()?.trim().to_string();
    let state = parts.next().unwrap_or("").trim().to_string();
    let ports = parts.next().unwrap_or("").trim().to_string();
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(ForeignContainer {
        id,
        name,
        guessed_engine: guess_engine(&image),
        image,
        state,
        ports,
    })
}

/// `{{.Labels}}` renders as `k=v,k2=v2`.
pub fn labels_field_contains_managed(labels_field: &str) -> bool {
    labels_field
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .any(|(k, v)| k.trim() == LABEL_MANAGED && v.trim() == "true")
}

fn guess_engine(image: &str) -> Option<String> {
    let lower = image.to_ascii_lowercase();
    for (needle, engine) in [
        ("postgres", "postgres"),
        ("postgis", "postgres"),
        ("mysql", "mysql"),
        ("mariadb", "mariadb"),
        ("mongo", "mongodb"),
        ("redis", "redis"),
    ] {
        if lower.contains(needle) {
            return Some(engine.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct ContainerStats {
    pub name: String,
    pub cpu_percent: String,
    pub memory_usage: String,
}

/// `docker stats --no-stream` for the named containers. Polled only when the
/// relevant screen is visible (PRD §12.2).
pub async fn stats(x: &Executor, names: &[String]) -> Result<Vec<ContainerStats>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut argv = vec![
        x.docker_bin().to_string(),
        "stats".into(),
        "--no-stream".into(),
        "--format".into(),
        "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}".into(),
    ];
    argv.extend(names.iter().cloned());
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout_str()
        .lines()
        .filter_map(|line| {
            let mut p = line.split('\t');
            Some(ContainerStats {
                name: p.next()?.trim().to_string(),
                cpu_percent: p.next().unwrap_or("-").trim().to_string(),
                memory_usage: p.next().unwrap_or("-").trim().to_string(),
            })
        })
        .collect())
}

/// Total reclaimable/used disk for images, containers and volumes (TAR-010).
pub async fn disk_usage(x: &Executor) -> Result<Vec<(String, String)>> {
    let argv = vec![
        x.docker_bin().to_string(),
        "system".into(),
        "df".into(),
        "--format".into(),
        "{{.Type}}\t{{.Size}}".into(),
    ];
    let out = x.run(&argv).await?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout_str()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec {
            container_name: "linf-postgres-17".into(),
            image: "postgres:17".into(),
            volume_name: "linf-pg17-data".into(),
            data_dir: "/var/lib/postgresql/data".into(),
            bind_address: "127.0.0.1".into(),
            host_port: 5432,
            container_port: 5432,
            extra_ports: Vec::new(),
            labels: BTreeMap::from([
                (LABEL_MANAGED.to_string(), "true".to_string()),
                (LABEL_ENGINE.to_string(), "postgres".to_string()),
            ]),
            env: BTreeMap::from([("POSTGRES_USER".to_string(), "linf_admin".to_string())]),
            secret_env: vec!["POSTGRES_PASSWORD".into()],
            cpu_limit: None,
            memory_limit: None,
            healthcheck: Some(vec!["pg_isready".into(), "-U".into(), "linf_admin".into()]),
            command: Vec::new(),
        }
    }

    #[test]
    fn run_argv_binds_to_loopback_and_labels_the_container() {
        let argv = run_argv("docker", &spec());
        let joined = argv.join(" ");
        assert!(joined.contains("--publish 127.0.0.1:5432:5432"), "{joined}");
        assert!(joined.contains("--label local-infra.managed=true"));
        assert!(joined.contains("--volume linf-pg17-data:/var/lib/postgresql/data"));
        assert!(joined.ends_with("postgres:17"), "image goes last");
    }

    #[test]
    fn run_argv_passes_secret_env_by_name_only() {
        let argv = run_argv("docker", &spec());
        let joined = argv.join(" ");
        assert!(joined.contains("--env POSTGRES_USER=linf_admin"));
        assert!(
            joined.contains("--env POSTGRES_PASSWORD ")
                || joined.contains("--env POSTGRES_PASSWORD\n"),
            "value-less form keeps the secret out of argv: {joined}"
        );
        assert!(!joined.contains("POSTGRES_PASSWORD="));
    }

    #[test]
    fn run_argv_includes_limits_and_healthcheck_only_when_asked() {
        let mut s = spec();
        s.cpu_limit = Some("1.5".into());
        s.memory_limit = Some("512m".into());
        let joined = run_argv("docker", &s).join(" ");
        assert!(joined.contains("--cpus 1.5"));
        assert!(joined.contains("--memory 512m"));
        assert!(joined.contains("--health-cmd pg_isready -U linf_admin"));

        let mut bare = spec();
        bare.healthcheck = None;
        let joined = run_argv("docker", &bare).join(" ");
        assert!(!joined.contains("--cpus"));
        assert!(!joined.contains("--health-cmd"));
    }

    #[test]
    fn exec_argv_is_interactive_and_passes_env_by_name() {
        let argv = exec_argv(
            "docker",
            "linf-postgres-17",
            Some("postgres"),
            &["PGPASSWORD"],
            &["psql".to_string(), "-f".to_string(), "-".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "docker",
                "exec",
                "--interactive",
                "--user",
                "postgres",
                "--env",
                "PGPASSWORD",
                "linf-postgres-17",
                "psql",
                "-f",
                "-"
            ]
        );
    }

    #[test]
    fn labels_are_parsed_from_the_inspect_format() {
        let labels = parse_labels("local-infra.managed=true\nlocal-infra.engine=postgres\n");
        assert_eq!(labels.get(LABEL_MANAGED).map(String::as_str), Some("true"));
        assert_eq!(
            labels.get(LABEL_ENGINE).map(String::as_str),
            Some("postgres")
        );
    }

    #[test]
    fn ps_lines_become_foreign_containers_with_a_guessed_engine() {
        let c = parse_ps_line(
            "abc123\tmy-pg\tpostgres:16-alpine\trunning\t0.0.0.0:5433->5432/tcp\tfoo=bar",
        )
        .unwrap();
        assert_eq!(c.name, "my-pg");
        assert_eq!(c.guessed_engine.as_deref(), Some("postgres"));
        assert_eq!(c.state, "running");
        assert!(parse_ps_line("").is_none());
    }

    #[test]
    fn a_stopped_container_still_reserves_its_published_port() {
        let text = "old-pg\texited\t0.0.0.0:5432->5432/tcp\nweb\trunning\t0.0.0.0:80->80/tcp";
        assert_eq!(
            find_port_holder(text, 5432),
            Some(PortHolder {
                name: "old-pg".into(),
                running: false
            })
        );
        assert_eq!(find_port_holder(text, 5433), None);
    }

    #[test]
    fn a_running_holder_wins_over_a_stopped_one() {
        let text =
            "old-pg\texited\t0.0.0.0:5432->5432/tcp\nlive-pg\trunning\t0.0.0.0:5432->5432/tcp";
        let holder = find_port_holder(text, 5432).unwrap();
        assert_eq!(holder.name, "live-pg");
        assert!(holder.running);
        assert!(holder.running);
    }

    #[test]
    fn published_port_ranges_and_ipv6_count_as_taken() {
        assert!(publishes_host_port("0.0.0.0:9000->9000/tcp", 9000));
        assert!(publishes_host_port("127.0.0.1:9000->9000/tcp", 9000));
        assert!(publishes_host_port("[::]:9000->9000/tcp", 9000));
        assert!(publishes_host_port(
            "0.0.0.0:9000-9001->9000-9001/tcp",
            9000
        ));
        assert!(publishes_host_port(
            "0.0.0.0:9000-9001->9000-9001/tcp",
            9001
        ));
        assert!(!publishes_host_port(
            "0.0.0.0:9000-9001->9000-9001/tcp",
            9002
        ));
        assert!(!publishes_host_port("0.0.0.0:5432->5432/tcp", 9000));
    }

    #[test]
    fn managed_label_is_detected_in_the_ps_labels_field() {
        assert!(labels_field_contains_managed(
            "com.docker.compose=1,local-infra.managed=true"
        ));
        assert!(!labels_field_contains_managed("local-infra.managed=false"));
        assert!(!labels_field_contains_managed(""));
    }

    #[tokio::test]
    async fn unreachable_daemon_is_a_result_not_an_error() {
        // `false` stands in for a docker binary that always fails.
        let x = Executor::Local {
            docker: "false".into(),
        };
        let info = info(&x).await.unwrap();
        assert!(!info.reachable);
        let err = require_daemon(&x).await.unwrap_err();
        assert!(err
            .as_diagnostic()
            .what
            .contains("Docker에 연결할 수 없습니다"));
    }
}
