//! SSH host key approval, connectivity probes and `~/.ssh/config` import
//! (PRD §11.3, TAR-005/006/011).
//!
//! Host key checking is never disabled and there is no option to disable it.
//! The only way an SSH target becomes usable is that the user is shown the
//! fingerprints the host actually offers and approves one of them; [`trust`]
//! then appends that exact key line to `~/.ssh/known_hosts`. A host that
//! already has a *different* key on file is refused, never overwritten.

use crate::core::config::{expand_tilde, harden_file};
use crate::core::error::{Diagnostic, Error, Result};
use crate::core::exec::{Executor, Output, SshConfig};
use std::path::PathBuf;

/// Seconds `ssh-keyscan` waits for a banner before giving up.
const KEYSCAN_TIMEOUT: u32 = 5;

/// One key a host offered, in the form the user compares by eye.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    /// `ssh-ed25519`, `ecdsa-sha2-nistp256`, `ssh-rsa`, …
    pub key_type: String,
    /// `SHA256:…`, the form OpenSSH prints.
    pub fingerprint: String,
}

// ---------------------------------------------------------------------------
// Host keys
// ---------------------------------------------------------------------------

/// Every key the host currently offers (TAR-005).
///
/// A host that offers nothing is a failure, not an empty list: silently
/// returning `Ok(vec![])` would let a caller "approve" an empty set.
pub async fn scan_host_keys(host: &str, port: u16) -> Result<Vec<HostKey>> {
    let x = Executor::local();
    let (lines, out, argv) = scan_lines(&x, host, port).await?;

    let mut keys = Vec::new();
    for line in &lines {
        let key_type = line
            .split_whitespace()
            .nth(1)
            .unwrap_or("unknown")
            .to_string();
        if let Some(fingerprint) = fingerprint_of(&x, line).await? {
            keys.push(HostKey {
                host: host.to_string(),
                port,
                key_type,
                fingerprint,
            });
        }
    }

    if keys.is_empty() {
        return Err(Error::diagnostic(
            Diagnostic::new(
                format!("`{host}:{port}`의 SSH 호스트 키를 가져오지 못했습니다"),
                "호스트가 응답하지 않았거나, 해당 포트에서 SSH 서비스가 동작하지 않습니다.",
                "호스트 주소와 포트를 확인하고, 방화벽 또는 Tailscale 연결 상태를 점검한 뒤 다시 시도하세요.",
            )
            .with_command(x.describe(&argv))
            .with_output(out.message()),
        ));
    }
    Ok(keys)
}

/// `~/.ssh/known_hosts`.
pub fn known_hosts_path() -> PathBuf {
    expand_tilde("~/.ssh/known_hosts")
}

/// Fingerprints already trusted for this host, so the UI can tell a brand new
/// host from one whose key changed.
pub async fn known_fingerprints(host: &str, port: u16) -> Result<Vec<String>> {
    let path = known_hosts_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let x = Executor::local();
    let argv = vec![
        "ssh-keygen".to_string(),
        "-F".into(),
        host_spec(host, port),
        "-f".into(),
        path.display().to_string(),
    ];
    let out = x.run(&argv).await?;
    // Exit 1 simply means "no entry for that host".
    if !out.ok() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for line in key_lines(&out.stdout_str()) {
        if let Some(fp) = fingerprint_of(&x, &line).await? {
            if !found.contains(&fp) {
                found.push(fp);
            }
        }
    }
    Ok(found)
}

/// Append the *approved* key line to `known_hosts`, creating the file `0600`.
///
/// Only ever called after the user approved a fingerprint. The host is
/// re-scanned; any key whose fingerprint is not the approved one is ignored,
/// and if the approved fingerprint is no longer offered nothing is written.
/// When a *different* key is already on file this refuses instead of
/// overwriting, because that is exactly what a man-in-the-middle looks like.
pub async fn trust(host: &str, port: u16, approved_fingerprint: &str) -> Result<()> {
    let x = Executor::local();
    let (lines, _, _) = scan_lines(&x, host, port).await?;

    let mut scanned = Vec::new();
    for line in &lines {
        if let Some(fp) = fingerprint_of(&x, line).await? {
            scanned.push((line.clone(), fp));
        }
    }
    if scanned.is_empty() {
        return Err(Error::failed(
            format!("`{host}:{port}`의 SSH 호스트 키를 확인하지 못했습니다"),
            "ssh-keyscan이 키를 반환하지 않았습니다.",
            "호스트가 SSH 요청에 응답하는지 확인한 뒤 다시 시도하세요.",
        ));
    }

    let chosen = choose_trusted_line(&scanned, approved_fingerprint)?;
    let want = approved_fingerprint.trim();
    let existing = known_fingerprints(host, port).await?;
    if existing.iter().any(|fp| fp == want) {
        return Ok(());
    }
    if !existing.is_empty() {
        return Err(Error::Refused(format!(
            "`{}`에 이미 다른 SSH 호스트 키가 등록되어 있습니다. \
             등록된 지문: {} / 이번에 승인한 지문: {}. \
             호스트 키가 교체되었거나 중간자 공격일 수 있으므로 known_hosts를 덮어쓰지 않았습니다. \
             서버 관리자에게 키 교체 여부를 확인한 뒤, 직접 `ssh-keygen -R {}`로 기존 항목을 지우고 다시 등록하세요.",
            host_spec(host, port),
            existing.join(", "),
            want,
            host_spec(host, port),
        )));
    }

    append_known_hosts(&[chosen])
}

/// Pure half of [`trust`]: pick the single scanned line whose fingerprint the
/// user approved. An unapproved sibling key is never returned, even when it
/// was offered in the same scan.
pub(crate) fn choose_trusted_line(scanned: &[(String, String)], approved: &str) -> Result<String> {
    let want = approved.trim();
    if want.is_empty() {
        return Err(Error::Usage(
            "승인된 호스트 키 지문이 비어 있습니다.".into(),
        ));
    }
    let offered: Vec<&str> = scanned.iter().map(|(_, fp)| fp.as_str()).collect();
    match scanned
        .iter()
        .find(|(_, fp)| fp == want)
        .map(|(line, _)| line.clone())
    {
        Some(line) => Ok(line),
        None => Err(Error::Refused(format!(
            "승인한 지문 `{want}`이(가) 호스트가 지금 제시하는 키에 없습니다. \
             제시된 지문: {}. 승인 직후 키가 바뀌었을 수 있으므로 known_hosts에 쓰지 않았습니다.",
            if offered.is_empty() {
                "없음".to_string()
            } else {
                offered.join(", ")
            }
        ))),
    }
}

/// `host` for port 22, `[host]:port` otherwise — the form `known_hosts` uses.
pub(crate) fn host_spec(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Raw `ssh-keyscan` key lines plus the output they came from, so the caller
/// can build a diagnostic without running the scan twice.
async fn scan_lines(
    x: &Executor,
    host: &str,
    port: u16,
) -> Result<(Vec<String>, Output, Vec<String>)> {
    let argv = vec![
        "ssh-keyscan".to_string(),
        "-p".into(),
        port.to_string(),
        "-T".into(),
        KEYSCAN_TIMEOUT.to_string(),
        host.to_string(),
    ];
    let out = x.run(&argv).await?;
    let lines = key_lines(&out.stdout_str());
    Ok((lines, out, argv))
}

/// Drop comments and blanks from `ssh-keyscan` / `ssh-keygen -F` output.
pub(crate) fn key_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Fingerprint one `known_hosts`-style key line via `ssh-keygen -lf -`.
async fn fingerprint_of(x: &Executor, key_line: &str) -> Result<Option<String>> {
    let argv = vec!["ssh-keygen".to_string(), "-lf".into(), "-".into()];
    let out = x
        .run_with_stdin(&argv, format!("{key_line}\n").as_bytes())
        .await?;
    if !out.ok() {
        return Ok(None);
    }
    Ok(parse_fingerprint(&out.stdout_str()))
}

/// `256 SHA256:abc… host (ED25519)` → `SHA256:abc…`.
pub(crate) fn parse_fingerprint(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| token.starts_with("SHA256:"))
        .map(str::to_string)
}

fn append_known_hosts(lines: &[String]) -> Result<()> {
    use std::io::Write;

    let path = known_hosts_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(dir)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(dir, perms)?;
        }
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    for line in lines {
        if existing.lines().any(|l| l.trim() == line) {
            continue;
        }
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    drop(file);
    harden_file(&path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Connectivity
// ---------------------------------------------------------------------------

/// `ssh <base options> dest -- true`: does the transport work at all?
pub async fn test_connection(cfg: &SshConfig) -> Result<()> {
    let x = Executor::local();
    let mut argv = vec!["ssh".to_string()];
    argv.extend(cfg.base_options());
    argv.push(cfg.destination());
    argv.push("--".into());
    argv.push("true".into());

    let out = x.run(&argv).await?;
    if out.ok() {
        return Ok(());
    }
    let (cause, next) = ssh_failure_text(classify_ssh_stderr(&out.message()), cfg);
    Err(Error::diagnostic(
        Diagnostic::new(
            format!("`{}` SSH 연결에 실패했습니다", cfg.destination()),
            cause,
            next,
        )
        .with_command(x.describe(&argv))
        .with_output(out.message()),
    ))
}

/// `docker version --format {{.Server.Version}}` over SSH: is the *Docker*
/// half usable by this account (TAR-004)?
pub async fn test_docker(cfg: &SshConfig, docker_bin: &str) -> Result<()> {
    let x = Executor::Ssh {
        ssh: cfg.clone(),
        docker: docker_bin.to_string(),
    };
    let argv = vec![
        docker_bin.to_string(),
        "version".into(),
        "--format".into(),
        "{{.Server.Version}}".into(),
    ];
    let out = x.run(&argv).await?;
    if out.ok() && !out.stdout_str().is_empty() {
        return Ok(());
    }

    let text = out.message();
    let failure = if out.ok() {
        DockerFailure::Daemon
    } else {
        classify_docker_stderr(&text)
    };
    let (what, cause, next) = docker_failure_text(failure, cfg, docker_bin);
    Err(Error::diagnostic(
        Diagnostic::new(what, cause, next)
            .with_command(x.describe(&argv))
            .with_output(text),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshFailure {
    /// The stored key no longer matches the one the host presents.
    HostKeyChanged,
    /// The host is not in `known_hosts` at all.
    HostKeyUnknown,
    /// Transport is fine, credentials are not.
    Auth,
    /// The name does not resolve.
    Resolve,
    /// Refused, timed out or unreachable.
    Network,
    Unknown,
}

pub(crate) fn classify_ssh_stderr(text: &str) -> SshFailure {
    let t = text.to_ascii_lowercase();
    if t.contains("remote host identification has changed")
        || t.contains("host key for") && t.contains("changed")
    {
        return SshFailure::HostKeyChanged;
    }
    if t.contains("host key verification failed") || t.contains("no matching host key") {
        return SshFailure::HostKeyUnknown;
    }
    if t.contains("permission denied") || t.contains("too many authentication failures") {
        return SshFailure::Auth;
    }
    if t.contains("could not resolve hostname")
        || t.contains("name or service not known")
        || t.contains("nodename nor servname")
    {
        return SshFailure::Resolve;
    }
    if t.contains("connection refused")
        || t.contains("connection timed out")
        || t.contains("operation timed out")
        || t.contains("no route to host")
        || t.contains("network is unreachable")
    {
        return SshFailure::Network;
    }
    SshFailure::Unknown
}

fn ssh_failure_text(failure: SshFailure, cfg: &SshConfig) -> (String, String) {
    let dest = cfg.destination();
    match failure {
        SshFailure::HostKeyChanged => (
            "저장된 SSH 호스트 키와 서버가 제시한 키가 다릅니다.".to_string(),
            format!(
                "서버를 재설치했다면 `ssh-keygen -R {}`로 기존 항목을 지우고 지문을 다시 승인하세요. \
                 그렇지 않다면 중간자 공격일 수 있으므로 접속하지 마세요.",
                host_spec(&cfg.host, cfg.port)
            ),
        ),
        SshFailure::HostKeyUnknown => (
            "호스트 키가 known_hosts에 없어 검증에 실패했습니다.".to_string(),
            "`linf target verify`로 호스트 키 지문을 확인하고 승인하세요.".to_string(),
        ),
        SshFailure::Auth => (
            format!("`{dest}` 계정 인증에 실패했습니다."),
            "ssh-agent에 키가 등록되어 있는지, 또는 개인키 경로가 올바른지 확인하세요. \
             비밀번호 인증만 허용하는 서버는 지원하지 않습니다(BatchMode)."
                .to_string(),
        ),
        SshFailure::Resolve => (
            format!("호스트 이름 `{}`을(를) 확인할 수 없습니다.", cfg.host),
            "호스트 주소 철자, DNS 설정, Tailscale MagicDNS 연결 상태를 확인하세요.".to_string(),
        ),
        SshFailure::Network => (
            format!("`{}:{}`에 연결할 수 없습니다.", cfg.host, cfg.port),
            "서버가 켜져 있는지, SSH 포트가 열려 있는지, 방화벽 규칙을 확인하세요.".to_string(),
        ),
        SshFailure::Unknown => (
            "SSH 명령이 실패했습니다.".to_string(),
            "아래 출력 내용을 확인한 뒤 다시 시도하세요.".to_string(),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockerFailure {
    /// The SSH transport itself failed; Docker was never reached.
    Transport(SshFailure),
    /// SSH worked, but the account may not use the Docker socket (PRD §10).
    Permission,
    /// The `docker` executable is not on the remote PATH.
    Missing,
    /// Docker CLI runs, the daemon does not answer.
    Daemon,
    Unknown,
}

pub(crate) fn classify_docker_stderr(text: &str) -> DockerFailure {
    let t = text.to_ascii_lowercase();
    if docker_permission_denied(&t) {
        return DockerFailure::Permission;
    }
    // Docker-specific wording is checked before the generic SSH patterns:
    // `dial unix …: connect: connection refused` is the daemon being down, not
    // an SSH transport problem.
    if t.contains("cannot connect to the docker daemon")
        || t.contains("is the docker daemon running")
        || t.contains("docker daemon socket")
    {
        return DockerFailure::Daemon;
    }
    if t.contains("command not found")
        || t.contains("executable file not found")
        || t.contains(": not found")
    {
        return DockerFailure::Missing;
    }
    match classify_ssh_stderr(&t) {
        SshFailure::Unknown => {}
        other => return DockerFailure::Transport(other),
    }
    DockerFailure::Unknown
}

/// The PRD §10 case: SSH succeeded but the account cannot use the socket.
pub(crate) fn docker_permission_denied(lowercased: &str) -> bool {
    if !lowercased.contains("permission denied") {
        return false;
    }
    lowercased.contains("docker.sock")
        || lowercased.contains("docker daemon socket")
        || lowercased.contains("while trying to connect to the docker")
}

fn docker_failure_text(
    failure: DockerFailure,
    cfg: &SshConfig,
    docker_bin: &str,
) -> (String, String, String) {
    let user = cfg.user.clone().unwrap_or_else(|| "해당".to_string());
    match failure {
        DockerFailure::Permission => (
            "원격 Docker 접근 실패".to_string(),
            format!(
                "SSH 연결에는 성공했지만 {user} 사용자가 Docker 명령을 실행할 권한이 없습니다."
            ),
            format!(
                "서버에서 해당 사용자에게 Docker 실행 권한을 부여한 뒤 다시 시도하세요 \
                 (`sudo usermod -aG docker {user}` 후 재로그인). \
                 docker 그룹 권한은 사실상 서버 관리자 권한임에 유의하세요."
            ),
        ),
        DockerFailure::Missing => (
            "원격 Docker 접근 실패".to_string(),
            format!(
                "SSH 연결에는 성공했지만 원격 호스트에서 `{docker_bin}` 명령을 찾을 수 없습니다."
            ),
            "서버에 Docker를 설치하거나, Target 설정에서 Docker 실행 파일 경로를 지정하세요."
                .to_string(),
        ),
        DockerFailure::Daemon => (
            "원격 Docker 데몬 연결 실패".to_string(),
            "SSH 연결과 Docker CLI는 정상이지만 Docker 데몬이 응답하지 않습니다.".to_string(),
            "서버에서 `sudo systemctl start docker`로 데몬을 시작한 뒤 다시 시도하세요."
                .to_string(),
        ),
        DockerFailure::Transport(inner) => {
            let (cause, next) = ssh_failure_text(inner, cfg);
            ("원격 Docker 접근 실패".to_string(), cause, next)
        }
        DockerFailure::Unknown => (
            "원격 Docker 접근 실패".to_string(),
            "원격 Docker 명령이 실패했습니다.".to_string(),
            "아래 출력 내용을 확인한 뒤 다시 시도하세요.".to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// ~/.ssh/config import (TAR-011)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SshConfigHost {
    pub alias: String,
    pub host_name: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
}

/// Concrete hosts declared in `~/.ssh/config`. A missing file is not an error.
pub fn config_hosts() -> Result<Vec<SshConfigHost>> {
    let path = expand_tilde("~/.ssh/config");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(parse_ssh_config(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Parse `Host` blocks, keeping only the four fields a Target needs.
///
/// Wildcard patterns (`*`, `?`, `!prefix`) and `Match` blocks are skipped:
/// they are defaults, not hosts a user can pick from a list.
pub(crate) fn parse_ssh_config(text: &str) -> Vec<SshConfigHost> {
    let mut hosts: Vec<SshConfigHost> = Vec::new();
    // Where the block currently being parsed starts; an empty range means the
    // block declared nothing importable and its settings are dropped.
    let mut block = 0usize;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = split_keyword(line) else {
            continue;
        };
        let key = key.to_ascii_lowercase();
        match key.as_str() {
            "host" => {
                block = hosts.len();
                for pattern in value.split_whitespace() {
                    let alias = unquote(pattern);
                    if alias.is_empty()
                        || alias.starts_with('!')
                        || alias.contains('*')
                        || alias.contains('?')
                    {
                        continue;
                    }
                    hosts.push(SshConfigHost {
                        alias,
                        host_name: None,
                        user: None,
                        port: None,
                        identity_file: None,
                    });
                }
            }
            // A `Match` block's settings never belong to the previous `Host`.
            "match" => block = hosts.len(),
            "hostname" | "user" | "port" | "identityfile" => {
                let value = unquote(value);
                for host in hosts[block..].iter_mut() {
                    // OpenSSH keeps the *first* value it sees for a keyword.
                    match key.as_str() {
                        "hostname" if host.host_name.is_none() => {
                            host.host_name = Some(value.clone())
                        }
                        "user" if host.user.is_none() => host.user = Some(value.clone()),
                        "port" if host.port.is_none() => host.port = value.parse().ok(),
                        "identityfile" if host.identity_file.is_none() => {
                            host.identity_file = Some(value.clone())
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    hosts
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

/// `Key value`, `Key=value` and `Key = value` are all legal in ssh_config.
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let key = &line[..idx];
    let value = line[idx..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
        .trim();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some((key, value))
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return trimmed[1..trimmed.len() - 1].to_string();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_spec_brackets_non_default_ports() {
        assert_eq!(host_spec("vps.example.com", 22), "vps.example.com");
        assert_eq!(host_spec("vps.example.com", 2222), "[vps.example.com]:2222");
        assert_eq!(host_spec("100.64.0.1", 22), "100.64.0.1");
    }

    #[test]
    fn fingerprint_is_read_out_of_ssh_keygen_output() {
        let line =
            "256 SHA256:9kFPq2m0Rq6mV5T0e2sJm/xLd0Wl5S8sN9v3aC0uZ1o vps.example.com (ED25519)";
        assert_eq!(
            parse_fingerprint(line).as_deref(),
            Some("SHA256:9kFPq2m0Rq6mV5T0e2sJm/xLd0Wl5S8sN9v3aC0uZ1o")
        );
        assert_eq!(parse_fingerprint("no key here"), None);
    }

    #[test]
    fn comments_and_blanks_are_not_key_lines() {
        let text = "# vps.example.com:22 SSH-2.0-OpenSSH_9.6\n\
                    vps.example.com ssh-ed25519 AAAAC3Nz\n\
                    \n\
                    # comment\n\
                    vps.example.com ssh-rsa AAAAB3Nz\n";
        assert_eq!(
            key_lines(text),
            vec![
                "vps.example.com ssh-ed25519 AAAAC3Nz".to_string(),
                "vps.example.com ssh-rsa AAAAB3Nz".to_string(),
            ]
        );
    }

    #[test]
    fn docker_socket_permission_denied_is_recognised() {
        // The exact shape PRD §10 renders in its modal.
        assert_eq!(
            classify_docker_stderr("permission denied on /var/run/docker.sock"),
            DockerFailure::Permission
        );
        assert_eq!(
            classify_docker_stderr(
                "Got permission denied while trying to connect to the Docker daemon socket at \
                 unix:///var/run/docker.sock"
            ),
            DockerFailure::Permission
        );
        // An SSH-level "Permission denied (publickey)" is *not* a Docker problem.
        assert_eq!(
            classify_docker_stderr("dev@vps: Permission denied (publickey)."),
            DockerFailure::Transport(SshFailure::Auth)
        );
        assert_eq!(
            classify_docker_stderr("bash: line 1: docker: command not found"),
            DockerFailure::Missing
        );
        assert_eq!(
            classify_docker_stderr(
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock."
            ),
            DockerFailure::Daemon
        );
        // Docker's own transport wording must not be read as an SSH failure.
        assert_eq!(
            classify_docker_stderr(
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
                 dial unix /var/run/docker.sock: connect: connection refused"
            ),
            DockerFailure::Daemon
        );
        assert_eq!(
            classify_docker_stderr("error during connect: Is the docker daemon running?"),
            DockerFailure::Daemon
        );
    }

    #[test]
    fn ssh_stderr_is_classified_by_root_cause() {
        assert_eq!(
            classify_ssh_stderr("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @"),
            SshFailure::HostKeyChanged
        );
        assert_eq!(
            classify_ssh_stderr("Host key verification failed."),
            SshFailure::HostKeyUnknown
        );
        assert_eq!(
            classify_ssh_stderr(
                "ssh: Could not resolve hostname nope: nodename nor servname provided"
            ),
            SshFailure::Resolve
        );
        assert_eq!(
            classify_ssh_stderr("ssh: connect to host vps port 22: Connection refused"),
            SshFailure::Network
        );
        assert_eq!(classify_ssh_stderr(""), SshFailure::Unknown);
    }

    #[test]
    fn ssh_config_is_parsed_and_wildcards_skipped() {
        let text = "\
# personal defaults
Host *
    ServerAliveInterval 60
    User fallback
    IdentityFile ~/.ssh/id_default

Host dev-vps
    HostName 203.0.113.10
    User devdb
    Port 2222
    IdentityFile ~/.ssh/id_dev_vps

Host tailnet   # inline comment
  HostName=vps.tail-scale.ts.net
  User = ops

Host build-* !build-old
    HostName build.example.com

Host box-a box-b
    HostName shared.example.com
    Port 22

Host quoted
    IdentityFile \"~/.ssh/key with space\"
";
        let hosts = parse_ssh_config(text);
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(
            aliases,
            vec!["dev-vps", "tailnet", "box-a", "box-b", "quoted"],
            "wildcard and negated patterns are not importable hosts"
        );

        let dev = &hosts[0];
        assert_eq!(dev.host_name.as_deref(), Some("203.0.113.10"));
        assert_eq!(dev.user.as_deref(), Some("devdb"));
        assert_eq!(dev.port, Some(2222));
        assert_eq!(dev.identity_file.as_deref(), Some("~/.ssh/id_dev_vps"));

        let tailnet = &hosts[1];
        assert_eq!(tailnet.host_name.as_deref(), Some("vps.tail-scale.ts.net"));
        assert_eq!(tailnet.user.as_deref(), Some("ops"));
        assert_eq!(tailnet.port, None);
        assert_eq!(
            tailnet.identity_file, None,
            "`Host *` defaults must not leak into a concrete host"
        );

        // One block, two aliases, shared settings.
        assert_eq!(hosts[2].host_name.as_deref(), Some("shared.example.com"));
        assert_eq!(hosts[3].host_name.as_deref(), Some("shared.example.com"));
        assert_eq!(hosts[3].port, Some(22));

        assert_eq!(
            hosts[4].identity_file.as_deref(),
            Some("~/.ssh/key with space")
        );
    }

    #[test]
    fn empty_config_yields_no_hosts() {
        assert!(parse_ssh_config("").is_empty());
        assert!(parse_ssh_config("# only a comment\n\n").is_empty());
        assert!(
            parse_ssh_config("Host *\n  User nobody\n").is_empty(),
            "a wildcard-only config has nothing to import"
        );
    }

    #[test]
    fn trust_selects_only_the_approved_key_from_a_multi_key_scan() {
        let scanned = vec![
            (
                "vps ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAAaaa".into(),
                "SHA256:AAAA".into(),
            ),
            (
                "vps ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQbbbb".into(),
                "SHA256:BBBB".into(),
            ),
        ];
        assert_eq!(
            choose_trusted_line(&scanned, "SHA256:AAAA").unwrap(),
            "vps ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAAaaa"
        );
        assert_eq!(
            choose_trusted_line(&scanned, "  SHA256:BBBB\n").unwrap(),
            "vps ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQbbbb"
        );
        let refused = choose_trusted_line(&scanned, "SHA256:CCCC").unwrap_err();
        assert!(matches!(refused, Error::Refused(_)), "{refused:?}");
        assert!(
            !refused.to_string().contains("AAAAC3NzaC1lZDI1NTE5AAAAAaaa"),
            "the unapproved key line itself must never be returned"
        );
    }

    #[test]
    fn a_scan_that_no_longer_offers_the_approved_key_writes_nothing() {
        let scanned = vec![(
            "vps ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAAnew".into(),
            "SHA256:NEW".into(),
        )];
        let err = choose_trusted_line(&scanned, "SHA256:OLD").unwrap_err();
        assert!(matches!(err, Error::Refused(_)));
        assert!(err.to_string().contains("SHA256:NEW"));
    }

    #[test]
    fn an_empty_approved_fingerprint_is_usage_not_a_write() {
        assert!(matches!(
            choose_trusted_line(&[], "  ").unwrap_err(),
            Error::Usage(_)
        ));
    }
}
