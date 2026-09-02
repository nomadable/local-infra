//! `linf doctor` — environment diagnosis (PRD §8.9, §10).
//!
//! Every problem the app can hit before it does any real work is checked here,
//! and every failing check carries the command or setting that fixes it. A
//! failed check is *data*: [`run`] only returns `Err` if it cannot produce the
//! report at all, never because the environment is broken — that is the whole
//! point of running it.

use crate::core::config::{Paths, SecretMode};
use crate::core::ctx::Ctx;
use crate::core::docker;
use crate::core::error::Result;
use crate::core::exec::Executor;
use crate::core::model::{Target, TargetKind};
use crate::core::secrets::SecretStore;
use crate::core::store::Store;
use serde::Serialize;
use std::io::IsTerminal;
use std::path::Path;

/// Minimum usable terminal (PRD §12.1, TUI-002).
pub const MIN_TERMINAL_WIDTH: u16 = 80;
pub const MIN_TERMINAL_HEIGHT: u16 = 24;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// What to do about it. Always present when `ok` is false.
    pub remedy: Option<String>,
}

impl Check {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: false,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// Run every check. Failures are reported, not raised.
pub async fn run(ctx: &Ctx) -> Result<Vec<Check>> {
    let mut checks = vec![
        state_dir_check(&ctx.paths),
        store_check(&ctx.store),
        secrets_check(&ctx.secrets),
    ];

    let targets = ctx.store.list_targets().unwrap_or_default();
    let local: Vec<&Target> = targets
        .iter()
        .filter(|t| t.kind == TargetKind::Local)
        .collect();
    let remote: Vec<&Target> = targets
        .iter()
        .filter(|t| t.kind == TargetKind::Ssh)
        .collect();

    // Diagnose the CLI with the binary the user actually configured.
    let cli_executor = match local.first() {
        Some(t) => Executor::Local {
            docker: t.docker_command.clone(),
        },
        None => Executor::local(),
    };
    checks.push(docker_cli_check(&cli_executor).await);

    if targets.is_empty() {
        checks.push(Check::fail(
            "등록된 Target",
            "아직 Target이 하나도 등록되어 있지 않습니다.",
            "TUI에서 Enter를 누르거나 `linf target add-local`로 이 컴퓨터를 등록하세요.",
        ));
    }

    for target in &local {
        let x = Executor::Local {
            docker: target.docker_command.clone(),
        };
        checks.push(docker_daemon_check(&x, &target.display_name).await);
    }

    if !remote.is_empty() {
        checks.push(ssh_binary_check().await);
        for target in &remote {
            checks.push(host_key_check(target));
        }
    }

    checks.push(terminal_size_check(terminal_size()));
    checks.push(terminal_capability_check(
        std::env::var("TERM").ok().as_deref(),
        ctx.config.unicode_enabled(),
    ));
    Ok(checks)
}

/// True when every check passed — the CLI turns this into the exit code.
pub fn all_ok(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.ok)
}

// ---------------------------------------------------------------------------
// Local state
// ---------------------------------------------------------------------------

fn state_dir_check(paths: &Paths) -> Check {
    let dir = &paths.state_dir;
    let name = "상태 디렉터리";
    let shown = dir.display().to_string();

    if !dir.is_dir() {
        return Check::fail(
            name,
            format!("{shown} 디렉터리가 없습니다."),
            format!(
                "`mkdir -p {shown}` 후 다시 실행하거나 `LINF_STATE_DIR`로 다른 위치를 지정하세요."
            ),
        );
    }
    if let Err(e) = probe_writable(dir) {
        return Check::fail(
            name,
            format!("{shown}에 쓸 수 없습니다 ({e})."),
            format!("`chmod u+rwx {shown}`로 권한을 복구하세요."),
        );
    }
    match dir_mode(dir) {
        Some(mode) if mode != 0o700 => Check::fail(
            name,
            format!(
                "{shown}의 권한이 {mode:o}입니다. 상태 파일이 다른 사용자에게 노출될 수 있습니다."
            ),
            format!("`chmod 700 {shown}`을 실행하세요."),
        ),
        Some(_) => Check::pass(name, format!("{shown} (0700)")),
        None => Check::pass(name, shown),
    }
}

#[cfg(unix)]
fn dir_mode(dir: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(std::fs::metadata(dir).ok()?.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn dir_mode(_dir: &Path) -> Option<u32> {
    None
}

/// Actually write, because a permission bit is not a guarantee (read-only
/// mounts, full disks and immutable flags all pass a `metadata` check).
fn probe_writable(dir: &Path) -> std::io::Result<()> {
    let probe = dir.join(format!(".linf-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"ok")?;
    std::fs::remove_file(&probe)
}

fn store_check(store: &Store) -> Check {
    let name = "상태 데이터베이스";
    match store.list_targets() {
        Ok(targets) => Check::pass(name, format!("정상 · Target {}개", targets.len())),
        Err(e) => Check::fail(
            name,
            format!("state.db를 읽을 수 없습니다 ({}).", e.as_diagnostic().cause),
            "상태 디렉터리 권한을 확인하고, 손상된 경우 `state.db`를 백업 후 삭제하면 새로 생성됩니다.",
        ),
    }
}

fn secrets_check(secrets: &SecretStore) -> Check {
    let name = "비밀번호 저장소";
    match secrets.mode() {
        SecretMode::Keyring => Check::pass(
            name,
            "OS 키체인 · 다음 실행에서도 비밀번호를 복구할 수 있습니다",
        ),
        SecretMode::File => Check::pass(
            name,
            "암호화 파일(AES-256-GCM) · 다음 실행에서도 비밀번호를 복구할 수 있습니다",
        ),
        SecretMode::None => Check::fail(
            name,
            "미저장 모드입니다. 생성 직후 한 번만 표시되고 이후에는 복구할 수 없습니다.",
            "OS 키체인을 사용할 수 있는 환경에서 실행하거나, `config.toml`에 \
             `[secrets] mode = \"file\"`을 설정하고 `LINF_PASSPHRASE`를 제공하세요.",
        ),
    }
}

// ---------------------------------------------------------------------------
// Docker and SSH
// ---------------------------------------------------------------------------

async fn docker_cli_check(x: &Executor) -> Check {
    let name = "Docker CLI";
    let bin = x.docker_bin().to_string();
    match docker::info(x).await {
        Ok(info) => match info.client_version {
            Some(version) => Check::pass(name, format!("{bin} {version}")),
            None => Check::fail(
                name,
                format!("`{bin}`이(가) 버전을 보고하지 않습니다."),
                "Docker Desktop 또는 Docker Engine을 설치하고 `docker version`이 동작하는지 확인하세요.",
            ),
        },
        Err(_) => Check::fail(
            name,
            format!("`{bin}` 명령을 실행할 수 없습니다."),
            "Docker를 설치하거나, Target의 `docker_command` 설정을 실제 경로로 지정하세요.",
        ),
    }
}

async fn docker_daemon_check(x: &Executor, target_name: &str) -> Check {
    let name = format!("Docker 데몬 ({target_name})");
    match docker::info(x).await {
        Ok(info) if info.reachable => Check::pass(
            name,
            format!(
                "server {}",
                info.server_version.as_deref().unwrap_or("unknown")
            ),
        ),
        Ok(_) => Check::fail(
            name,
            "Docker 데몬에 연결할 수 없습니다.",
            "Docker Desktop 또는 `systemctl start docker`로 데몬을 시작한 뒤 다시 실행하세요.",
        ),
        Err(_) => Check::fail(
            name,
            format!("`{}` 명령을 실행할 수 없습니다.", x.docker_bin()),
            "Docker CLI를 설치하거나 PATH를 확인하세요.",
        ),
    }
}

async fn ssh_binary_check() -> Check {
    let name = "ssh 클라이언트";
    let argv = vec!["ssh".to_string(), "-V".to_string()];
    match Executor::local().run(&argv).await {
        // `ssh -V` prints its banner on stderr.
        Ok(out) if out.ok() => {
            let version = out.stderr_str();
            let version = if version.is_empty() {
                out.stdout_str()
            } else {
                version
            };
            Check::pass(name, version)
        }
        Ok(out) => Check::fail(
            name,
            format!("`ssh -V`가 종료 코드 {}로 실패했습니다.", out.code),
            "OpenSSH 클라이언트를 설치하세요 (macOS는 기본 제공, Debian 계열은 `apt install openssh-client`).",
        ),
        Err(_) => Check::fail(
            name,
            "`ssh` 명령을 찾을 수 없습니다.",
            "OpenSSH 클라이언트를 설치하세요. 원격 Target과 터널 기능에 필요합니다.",
        ),
    }
}

fn host_key_check(target: &Target) -> Check {
    let name = format!("SSH 호스트 키 ({})", target.display_name);
    match &target.host_key_fingerprint {
        Some(fingerprint) => Check::pass(name, format!("승인됨 · {fingerprint}")),
        None => Check::fail(
            name,
            format!(
                "{}의 호스트 키 지문이 승인되지 않았습니다.",
                target.host.as_deref().unwrap_or(&target.display_name)
            ),
            format!(
                "`linf target verify {}`로 지문을 확인하고 승인하세요. 승인 전에는 접속을 시도하지 않습니다.",
                target.display_name
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// `None` when stdout is not a terminal, so the check can be skipped instead of
/// failing in CI and pipes (CLI-006).
fn terminal_size() -> Option<(u16, u16)> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size().ok()
}

fn terminal_size_check(size: Option<(u16, u16)>) -> Check {
    let name = "터미널 크기";
    match size {
        None => Check::pass(name, "TTY가 아니므로 확인을 건너뜁니다 (CLI 전용 모드)"),
        Some((cols, rows)) if cols < MIN_TERMINAL_WIDTH || rows < MIN_TERMINAL_HEIGHT => {
            Check::fail(
                name,
                format!("{cols}×{rows} — TUI는 최소 {MIN_TERMINAL_WIDTH}×{MIN_TERMINAL_HEIGHT}가 필요합니다."),
                format!("터미널 창을 {MIN_TERMINAL_WIDTH}×{MIN_TERMINAL_HEIGHT} 이상으로 늘리거나 `linf` 서브커맨드를 사용하세요."),
            )
        }
        Some((cols, rows)) => Check::pass(name, format!("{cols}×{rows}")),
    }
}

fn terminal_capability_check(term: Option<&str>, unicode: bool) -> Check {
    let name = "터미널 기능";
    let glyphs = if unicode {
        "유니코드 기호 사용"
    } else {
        "ASCII 대체 문자 사용"
    };
    match term {
        None | Some("") => Check::fail(
            name,
            "`TERM`이 설정되어 있지 않습니다.",
            "`TERM=xterm-256color`처럼 터미널 종류를 설정하세요. TUI 렌더링에 필요합니다.",
        ),
        Some("dumb") => Check::fail(
            name,
            "`TERM=dumb`은 커서 제어를 지원하지 않습니다.",
            "`TERM=xterm-256color`로 설정하거나 TUI 대신 `linf` 서브커맨드를 사용하세요.",
        ),
        Some(term) => Check::pass(name, format!("TERM={term} · {glyphs}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::util::now;

    fn ssh_target(fingerprint: Option<&str>) -> Target {
        Target {
            id: "t-vps".into(),
            kind: TargetKind::Ssh,
            display_name: "dev-vps".into(),
            host: Some("vps.ts.net".into()),
            ssh_port: Some(22),
            ssh_username: Some("deploy".into()),
            auth_type: None,
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: fingerprint.map(str::to_string),
            created_at: now(),
            last_connected_at: None,
        }
    }

    /// Every failing check must be actionable — that is the contract of the
    /// whole module, so it is asserted on every failure produced below.
    fn assert_actionable(check: &Check) {
        assert!(!check.ok);
        let remedy = check.remedy.as_deref().unwrap_or_default();
        assert!(!remedy.trim().is_empty(), "{check:?} has no remedy");
    }

    /// Built directly instead of through `Paths::resolve`, so these tests never
    /// touch the process-wide `LINF_STATE_DIR` and can run in parallel.
    fn paths_at(root: &Path) -> Paths {
        Paths {
            state_dir: root.to_path_buf(),
            config_dir: root.to_path_buf(),
            run_dir: root.join("run"),
            db_path: root.join("state.db"),
            lock_path: root.join("instance.lock"),
            config_path: root.join("config.toml"),
            default_backup_dir: root.join("backups"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_docker_is_a_failed_check_not_an_error() {
        // `false` stands in for a docker binary that never succeeds.
        let x = Executor::Local {
            docker: "false".into(),
        };
        let check = docker_daemon_check(&x, "local").await;
        assert_eq!(check.name, "Docker 데몬 (local)");
        assert_actionable(&check);
        assert!(check.remedy.as_deref().unwrap().contains("Docker"));
    }

    #[tokio::test]
    async fn a_missing_docker_binary_is_reported_against_the_configured_command() {
        let x = Executor::Local {
            docker: "/nonexistent/bin/docker".into(),
        };
        let check = docker_cli_check(&x).await;
        assert_actionable(&check);
        assert!(
            check.detail.contains("/nonexistent/bin/docker"),
            "{check:?}"
        );
    }

    #[test]
    fn the_state_directory_must_be_writable_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_at(dir.path());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let ok = state_dir_check(&paths);
            assert!(ok.ok, "{ok:?}");
            assert!(ok.detail.contains("0700"));

            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            let loose = state_dir_check(&paths);
            assert_actionable(&loose);
            assert!(loose.remedy.as_deref().unwrap().contains("chmod 700"));
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        #[cfg(not(unix))]
        assert!(state_dir_check(&paths).ok);
    }

    #[test]
    fn a_missing_state_directory_is_reported_rather_than_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let check = state_dir_check(&paths_at(&missing));
        assert_actionable(&check);
        assert!(!missing.exists(), "doctor never mutates the environment");
    }

    #[test]
    fn a_reachable_store_reports_how_many_targets_it_holds() {
        let store = Store::open_in_memory().unwrap();
        let check = store_check(&store);
        assert!(check.ok, "{check:?}");
        assert!(check.detail.contains("Target 0개"), "{check:?}");
    }

    #[test]
    fn restricted_secret_mode_is_flagged_with_the_way_to_persist() {
        let check = secrets_check(&SecretStore::restricted());
        assert_actionable(&check);
        let remedy = check.remedy.unwrap();
        assert!(remedy.contains("mode = \"file\""), "{remedy}");
    }

    #[test]
    fn an_unapproved_ssh_host_key_blocks_the_target() {
        let missing = host_key_check(&ssh_target(None));
        assert_actionable(&missing);
        assert!(missing
            .remedy
            .as_deref()
            .unwrap()
            .contains("target verify dev-vps"));

        let approved = host_key_check(&ssh_target(Some("SHA256:abc")));
        assert!(approved.ok);
        assert!(approved.detail.contains("SHA256:abc"));
    }

    #[test]
    fn a_terminal_smaller_than_80x24_is_reported_but_a_pipe_is_not() {
        let small = terminal_size_check(Some((70, 20)));
        assert_actionable(&small);
        assert!(small.detail.contains("70×20"), "{small:?}");

        assert!(terminal_size_check(Some((80, 24))).ok);
        assert!(terminal_size_check(Some((200, 60))).ok);
        assert!(terminal_size_check(None).ok, "not a TTY is not a failure");
    }

    #[test]
    fn term_must_be_set_to_something_that_can_render() {
        assert_actionable(&terminal_capability_check(None, true));
        assert_actionable(&terminal_capability_check(Some(""), true));
        assert_actionable(&terminal_capability_check(Some("dumb"), true));

        let ok = terminal_capability_check(Some("xterm-256color"), true);
        assert!(ok.ok);
        assert!(ok.detail.contains("유니코드"));

        let ascii = terminal_capability_check(Some("screen"), false);
        assert!(ascii.ok, "ASCII fallback is supported, not broken");
        assert!(ascii.detail.contains("ASCII"));
    }
}
