//! Target registration and diagnosis (PRD §8.1).
//!
//! A target is only ever *metadata*: where Docker lives and how to reach it.
//! Registering one creates nothing on the host and forgetting one destroys
//! nothing (§7.9 row 1). The single hard gate is TAR-005 — an SSH target may
//! not be stored unless the fingerprint the user approved is one the host is
//! actually offering right now.

use crate::core::activity::Activity;
use crate::core::ctx::Ctx;
use crate::core::docker::{self, DockerInfo};
use crate::core::error::{Error, Result};
use crate::core::exec::Executor;
use crate::core::model::{AuthType, EngineInstance, Target, TargetKind};
use crate::core::ssh;
use crate::core::util::{display_cols, new_id, now};

/// Longest display name that still fits the target column of a narrow table.
const MAX_NAME_COLS: usize = 48;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSpec {
    pub display_name: String,
    /// `docker`, or an absolute path when the CLI is not on `PATH`.
    pub docker_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSpec {
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub auth: AuthType,
    /// Path only — key *contents* are never copied into the app (§11.1).
    pub identity_path: Option<String>,
    pub docker_command: String,
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the local Docker host (TAR-001) and diagnose it (TAR-002).
///
/// A stopped daemon is reported, not fatal: the user may well be registering
/// the target before starting Docker Desktop.
pub async fn add_local(ctx: &Ctx, s: &LocalSpec) -> Result<Target> {
    ctx.require_write_lock()?;
    validate_display_name(&s.display_name)?;

    let target = Target {
        id: new_id(),
        kind: TargetKind::Local,
        display_name: s.display_name.clone(),
        host: None,
        ssh_port: None,
        ssh_username: None,
        auth_type: None,
        identity_path: None,
        docker_command: normalise_docker_command(&s.docker_command),
        host_key_fingerprint: None,
        created_at: now(),
        last_connected_at: None,
    };

    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "target",
        "add",
        format!("로컬 Target `{}` 등록", target.display_name),
    )?
    .on_target(&target.id)
    .on_resource(&target.id);

    let result = add_local_steps(ctx, target, &mut activity).await;
    activity.finish(&result);
    result
}

async fn add_local_steps(ctx: &Ctx, target: Target, activity: &mut Activity<'_>) -> Result<Target> {
    ctx.store.insert_target(&target)?;
    activity.step(format!("`{}`을(를) 등록했습니다.", target.display_name));
    activity.step(format!("Docker 진단: {}", diagnose(&target).await));
    Ok(target)
}

/// Register an SSH host (TAR-003/006).
///
/// `approved_fingerprint` must be the value the user explicitly approved after
/// being shown the scan result. It is re-scanned and compared here so approval
/// cannot go stale between the prompt and the write (TAR-005).
pub async fn add_ssh(ctx: &Ctx, s: &SshSpec, approved_fingerprint: &str) -> Result<Target> {
    ctx.require_write_lock()?;
    validate_ssh_spec(s)?;

    let id = new_id();
    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "target",
        "add",
        format!(
            "SSH Target `{}` 등록 ({}:{})",
            s.display_name, s.host, s.port
        ),
    )?
    .on_target(&id)
    .on_resource(&id);

    let result = add_ssh_steps(ctx, s, approved_fingerprint, id, &mut activity).await;
    activity.finish(&result);
    result
}

async fn add_ssh_steps(
    ctx: &Ctx,
    s: &SshSpec,
    approved_fingerprint: &str,
    id: String,
    activity: &mut Activity<'_>,
) -> Result<Target> {
    let offered = ssh::scan_host_keys(&s.host, s.port).await?;
    activity.step(format!(
        "호스트 키 {}개를 확인했습니다: {}",
        offered.len(),
        offered
            .iter()
            .map(|k| k.key_type.clone())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    match_fingerprint(&offered, approved_fingerprint)?;
    activity.step("승인된 지문이 호스트가 제시한 키와 일치합니다.");

    ssh::trust(&s.host, s.port, approved_fingerprint).await?;

    // `trust` re-scans and writes only the approved line. Confirm what landed.
    let trusted = ssh::known_fingerprints(&s.host, s.port).await?;
    if !trusted.iter().any(|fp| fp == approved_fingerprint.trim()) {
        return Err(Error::Refused(format!(
            "known_hosts에 승인한 지문 `{}`이(가) 등록되지 않았습니다. 등록된 지문: {}. \
             승인 직후 호스트 키가 바뀌었을 수 있으므로 Target을 등록하지 않았습니다.",
            approved_fingerprint.trim(),
            if trusted.is_empty() {
                "없음".to_string()
            } else {
                trusted.join(", ")
            }
        )));
    }
    activity.step("known_hosts에 승인된 호스트 키가 등록되어 있음을 확인했습니다.");

    let target = Target {
        id,
        kind: TargetKind::Ssh,
        display_name: s.display_name.clone(),
        host: Some(s.host.clone()),
        ssh_port: Some(s.port),
        ssh_username: s.username.clone(),
        auth_type: Some(s.auth),
        identity_path: match s.auth {
            // Agent auth must not pin a key file.
            AuthType::Agent => None,
            AuthType::Key => s.identity_path.clone(),
        },
        docker_command: normalise_docker_command(&s.docker_command),
        host_key_fingerprint: Some(approved_fingerprint.trim().to_string()),
        created_at: now(),
        last_connected_at: None,
    };
    ctx.store.insert_target(&target)?;
    activity.step(format!("`{}`을(를) 등록했습니다.", target.display_name));
    Ok(target)
}

/// TAR-005 enforcement point: the approved fingerprint must be one the host is
/// offering. Anything else is refused rather than stored.
pub(crate) fn match_fingerprint(offered: &[ssh::HostKey], approved: &str) -> Result<()> {
    let want = approved.trim();
    if want.is_empty() {
        return Err(Error::Usage(
            "승인할 SSH 호스트 키 지문이 비어 있습니다. 표시된 지문 중 하나를 선택하세요.".into(),
        ));
    }
    if offered.iter().any(|key| key.fingerprint == want) {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "승인한 지문 `{want}`이(가) 호스트가 제시한 키와 일치하지 않습니다. \
         현재 제시된 지문: {}. 호스트 키가 교체되었거나 중간자 공격일 수 있으므로 등록하지 않았습니다.",
        offered
            .iter()
            .map(|key| format!("{} {}", key.key_type, key.fingerprint))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

// ---------------------------------------------------------------------------
// Reads and edits
// ---------------------------------------------------------------------------

pub fn list(ctx: &Ctx) -> Result<Vec<Target>> {
    ctx.store.list_targets()
}

/// Accepts either the id or the display name.
pub fn get(ctx: &Ctx, key: &str) -> Result<Target> {
    ctx.store.require_target(key)
}

pub fn update(ctx: &Ctx, t: &Target) -> Result<()> {
    ctx.require_write_lock()?;
    validate_display_name(&t.display_name)?;
    if t.kind == TargetKind::Ssh && t.host.as_deref().unwrap_or("").trim().is_empty() {
        return Err(Error::Usage(
            "SSH Target에는 호스트 주소가 필요합니다.".into(),
        ));
    }

    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "target",
        "update",
        format!("Target `{}` 수정", t.display_name),
    )?
    .on_target(&t.id)
    .on_resource(&t.id);

    let result = ctx.store.update_target(t);
    if result.is_ok() {
        activity.step("Target 메타데이터를 갱신했습니다.");
    }
    activity.finish(&result);
    result
}

/// Unregister only — no container, volume or database is touched (§7.9 row 1).
///
/// Engines registered on the target would be orphaned by the delete cascade,
/// so this refuses while any remain (TAR-008).
pub fn forget(ctx: &Ctx, t: &Target) -> Result<()> {
    ctx.require_write_lock()?;
    let engines = ctx.store.list_engines_for_target(&t.id)?;
    refuse_if_engines_remain(t, &engines)?;

    let mut activity = Activity::start(
        &ctx.store,
        ctx.origin,
        "target",
        "forget",
        format!("Target `{}` 등록 해제", t.display_name),
    )?
    .on_target(&t.id)
    .on_resource(&t.id);

    let result = ctx.store.delete_target(&t.id);
    if result.is_ok() {
        activity.step("메타데이터에서만 제거했습니다. 원격 리소스는 그대로 유지됩니다.");
    }
    activity.finish(&result);
    result
}

pub(crate) fn refuse_if_engines_remain(t: &Target, engines: &[EngineInstance]) -> Result<()> {
    if engines.is_empty() {
        return Ok(());
    }
    let names = engines
        .iter()
        .map(|e| e.container_name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    Err(Error::Conflict(format!(
        "Target `{}`에 아직 엔진 {}개가 등록되어 있습니다: {names}. \
         먼저 해당 엔진의 등록을 해제하거나 삭제한 뒤 다시 시도하세요.",
        t.display_name,
        engines.len()
    )))
}

// ---------------------------------------------------------------------------
// Diagnosis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TargetCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub remedy: Option<String>,
}

/// SSH reachability and Docker permission are reported as separate checks so a
/// working SSH login with a broken Docker group is unambiguous (TAR-004).
pub async fn test(ctx: &Ctx, t: &Target) -> Result<Vec<TargetCheck>> {
    let checks = match t.kind {
        TargetKind::Local => test_local(t).await,
        TargetKind::Ssh => test_ssh(t).await,
    };
    if checks.iter().all(|c| c.ok) {
        touch_last_connected(ctx, t);
    }
    Ok(checks)
}

async fn test_local(t: &Target) -> Vec<TargetCheck> {
    match probe_docker(t).await {
        Ok(info) => vec![
            TargetCheck {
                name: "Docker CLI".into(),
                ok: info.client_version.is_some(),
                detail: match &info.client_version {
                    Some(v) => format!("Docker CLI {v}"),
                    None => "Docker CLI 버전을 확인할 수 없습니다.".into(),
                },
                remedy: info.client_version.is_none().then(|| {
                    "Docker Desktop 또는 Docker Engine을 설치하고 PATH를 확인하세요.".to_string()
                }),
            },
            TargetCheck {
                name: "Docker 데몬".into(),
                ok: info.reachable,
                detail: match &info.server_version {
                    Some(v) => format!("Docker 데몬 {v} 연결됨"),
                    None => "Docker 데몬에 연결할 수 없습니다.".into(),
                },
                remedy: (!info.reachable).then(|| {
                    "Docker Desktop 또는 Docker Engine을 시작한 뒤 다시 시도하세요.".to_string()
                }),
            },
        ],
        Err(e) => {
            let d = e.as_diagnostic();
            vec![
                TargetCheck {
                    name: "Docker CLI".into(),
                    ok: false,
                    detail: d.cause.clone(),
                    remedy: Some(d.next.clone()),
                },
                unknown_check(
                    "Docker 데몬",
                    "Docker CLI를 실행할 수 없어 확인하지 못했습니다.",
                ),
            ]
        }
    }
}

async fn test_ssh(t: &Target) -> Vec<TargetCheck> {
    let mut checks = Vec::new();
    let approved = t.host_key_fingerprint.clone();
    checks.push(TargetCheck {
        name: "SSH 호스트 키".into(),
        ok: approved.is_some(),
        detail: match &approved {
            Some(fp) => format!("승인된 지문 {fp}"),
            None => "승인된 호스트 키 지문이 없습니다.".into(),
        },
        remedy: approved.is_none().then(|| {
            format!(
                "`linf target verify {}`로 지문을 확인하고 승인하세요.",
                t.display_name
            )
        }),
    });

    let executor = match Executor::for_target(t) {
        Ok(x) => x,
        Err(e) => {
            let d = e.as_diagnostic();
            checks.push(TargetCheck {
                name: "SSH 연결".into(),
                ok: false,
                detail: d.what,
                remedy: Some(d.next),
            });
            checks.push(unknown_check(
                "Docker 실행 권한",
                "SSH 연결을 확인하지 못해 건너뛰었습니다.",
            ));
            return checks;
        }
    };
    let cfg = match executor.ssh() {
        Some(cfg) => cfg.clone(),
        None => {
            checks.push(unknown_check("SSH 연결", "SSH 설정을 구성할 수 없습니다."));
            checks.push(unknown_check(
                "Docker 실행 권한",
                "SSH 연결을 확인하지 못해 건너뛰었습니다.",
            ));
            return checks;
        }
    };

    match ssh::test_connection(&cfg).await {
        Ok(()) => checks.push(TargetCheck {
            name: "SSH 연결".into(),
            ok: true,
            detail: format!("`{}`에 접속했습니다.", cfg.destination()),
            remedy: None,
        }),
        Err(e) => {
            let d = e.as_diagnostic();
            checks.push(TargetCheck {
                name: "SSH 연결".into(),
                ok: false,
                detail: d.cause,
                remedy: Some(d.next),
            });
            checks.push(unknown_check(
                "Docker 실행 권한",
                "SSH 연결에 실패해 확인하지 못했습니다.",
            ));
            return checks;
        }
    }

    match ssh::test_docker(&cfg, &t.docker_command).await {
        Ok(()) => checks.push(TargetCheck {
            name: "Docker 실행 권한".into(),
            ok: true,
            detail: "원격 Docker 데몬에 접근할 수 있습니다.".into(),
            remedy: None,
        }),
        Err(e) => {
            let d = e.as_diagnostic();
            checks.push(TargetCheck {
                name: "Docker 실행 권한".into(),
                ok: false,
                detail: d.cause,
                remedy: Some(d.next),
            });
        }
    }
    checks
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TargetOverview {
    pub target: Target,
    pub reachable: bool,
    /// Docker *server* version when the daemon answered.
    pub docker: Option<String>,
    pub detail: String,
}

/// Dashboard row per target. An unreachable host is a row, never an error.
pub async fn overview(ctx: &Ctx) -> Result<Vec<TargetOverview>> {
    let mut rows = Vec::new();
    for target in ctx.store.list_targets()? {
        let (reachable, docker, detail) = match probe_docker(&target).await {
            Ok(info) => (
                info.reachable,
                info.server_version.clone(),
                describe_docker(&target, &info),
            ),
            Err(e) => (false, None, e.as_diagnostic().cause),
        };
        rows.push(TargetOverview {
            target,
            reachable,
            docker,
            detail,
        });
    }
    Ok(rows)
}

async fn probe_docker(t: &Target) -> Result<DockerInfo> {
    let executor = Executor::for_target(t)?;
    docker::info(&executor).await
}

/// One-line docker verdict, used by both the activity log and the dashboard.
async fn diagnose(t: &Target) -> String {
    match probe_docker(t).await {
        Ok(info) => describe_docker(t, &info),
        Err(e) => e.as_diagnostic().cause,
    }
}

fn describe_docker(t: &Target, info: &DockerInfo) -> String {
    if info.reachable {
        return match &info.server_version {
            Some(v) => format!("Docker 데몬 {v} 연결됨"),
            None => "Docker 데몬 연결됨".to_string(),
        };
    }
    if t.is_remote() {
        return "SSH 또는 원격 Docker 데몬에 연결할 수 없습니다.".to_string();
    }
    match &info.client_version {
        Some(v) => format!("Docker CLI {v}은(는) 있지만 데몬에 연결할 수 없습니다."),
        None => "Docker CLI를 찾을 수 없습니다.".to_string(),
    }
}

fn unknown_check(name: &str, detail: &str) -> TargetCheck {
    TargetCheck {
        name: name.to_string(),
        ok: false,
        detail: detail.to_string(),
        remedy: None,
    }
}

/// Best effort: a read-only second instance must not fail a test just because
/// it cannot record the timestamp.
fn touch_last_connected(ctx: &Ctx, t: &Target) {
    if !ctx.has_write_lock() {
        return;
    }
    let mut updated = t.clone();
    updated.last_connected_at = Some(now());
    let _ = ctx.store.update_target(&updated);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

pub(crate) fn validate_display_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::Usage("Target 이름을 입력하세요.".into()));
    }
    if name.trim() != name {
        return Err(Error::Usage("Target 이름의 앞뒤 공백을 제거하세요.".into()));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::Usage(
            "Target 이름에는 제어 문자를 사용할 수 없습니다.".into(),
        ));
    }
    if display_cols(name) > MAX_NAME_COLS {
        return Err(Error::Usage(format!(
            "Target 이름은 화면 {MAX_NAME_COLS}칸을 넘을 수 없습니다."
        )));
    }
    Ok(())
}

pub(crate) fn validate_ssh_spec(s: &SshSpec) -> Result<()> {
    validate_display_name(&s.display_name)?;
    if s.host.trim().is_empty() {
        return Err(Error::Usage("SSH 호스트 주소를 입력하세요.".into()));
    }
    if s.host.trim() != s.host || s.host.chars().any(char::is_whitespace) {
        return Err(Error::Usage(
            "SSH 호스트 주소에는 공백을 포함할 수 없습니다.".into(),
        ));
    }
    if s.port == 0 {
        return Err(Error::Usage("SSH 포트는 1 이상이어야 합니다.".into()));
    }
    if s.auth == AuthType::Key && s.identity_path.as_deref().unwrap_or("").trim().is_empty() {
        return Err(Error::Usage(
            "개인키 인증을 선택하면 개인키 경로가 필요합니다. ssh-agent를 쓰려면 인증 방식을 agent로 바꾸세요."
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn normalise_docker_command(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "docker".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::EngineKind;
    use crate::core::store::Store;

    fn host_key(key_type: &str, fingerprint: &str) -> ssh::HostKey {
        ssh::HostKey {
            host: "vps.example.com".into(),
            port: 22,
            key_type: key_type.into(),
            fingerprint: fingerprint.into(),
        }
    }

    fn a_target(id: &str, name: &str) -> Target {
        Target {
            id: id.into(),
            kind: TargetKind::Ssh,
            display_name: name.into(),
            host: Some("vps.example.com".into()),
            ssh_port: Some(22),
            ssh_username: Some("devdb".into()),
            auth_type: Some(AuthType::Agent),
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: Some("SHA256:aaa".into()),
            created_at: now(),
            last_connected_at: None,
        }
    }

    #[test]
    fn approved_fingerprint_must_be_one_the_host_offers() {
        let offered = vec![
            host_key(
                "ssh-ed25519",
                "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
            host_key(
                "ssh-rsa",
                "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            ),
        ];

        assert!(match_fingerprint(
            &offered,
            "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
        )
        .is_ok());
        // Surrounding whitespace from a paste is tolerated, the value is not.
        assert!(match_fingerprint(
            &offered,
            "  SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n"
        )
        .is_ok());

        let mismatch = match_fingerprint(
            &offered,
            "SHA256:CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
        )
        .unwrap_err();
        assert!(matches!(mismatch, Error::Refused(_)), "got {mismatch:?}");
        assert_eq!(mismatch.exit_code(), 2);
        let text = mismatch.to_string();
        assert!(
            text.contains("ssh-ed25519"),
            "lists what the host offered: {text}"
        );
        assert!(text.contains("ssh-rsa"), "lists every offered key: {text}");

        // A truncated fingerprint must not be accepted as a prefix match.
        assert!(matches!(
            match_fingerprint(&offered, "SHA256:AAAA").unwrap_err(),
            Error::Refused(_)
        ));
        assert!(matches!(
            match_fingerprint(&offered, "   ").unwrap_err(),
            Error::Usage(_)
        ));
        assert!(matches!(
            match_fingerprint(&[], "SHA256:AAAA").unwrap_err(),
            Error::Refused(_)
        ));
    }

    #[test]
    fn forget_refuses_while_engines_are_still_registered() {
        let store = Store::open_in_memory().unwrap();
        let target = a_target("t-1", "dev-vps");
        store.insert_target(&target).unwrap();

        // Nothing registered yet: unregistering is safe.
        let engines = store.list_engines_for_target(&target.id).unwrap();
        assert!(refuse_if_engines_remain(&target, &engines).is_ok());

        store
            .insert_engine(&EngineInstance {
                id: "e-1".into(),
                target_id: target.id.clone(),
                engine: EngineKind::Postgres,
                major_version: "17".into(),
                image: "postgres:17".into(),
                container_name: "linf-postgres-17".into(),
                volume_name: "linf-pg17-data".into(),
                bind_address: "127.0.0.1".into(),
                host_port: 5432,
                console_port: None,
                admin_user: "linf_admin".into(),
                credential_ref: "engine:e-1".into(),
                managed: true,
                created_at: now(),
            })
            .unwrap();

        let engines = store.list_engines_for_target(&target.id).unwrap();
        let err = refuse_if_engines_remain(&target, &engines).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        assert_eq!(err.exit_code(), 2);
        let text = err.to_string();
        assert!(
            text.contains("linf-postgres-17"),
            "names the blocking engine: {text}"
        );
        assert!(text.contains("dev-vps"), "names the target: {text}");

        // The refusal is metadata-only reasoning: nothing was deleted.
        assert!(store.find_target("dev-vps").unwrap().is_some());
    }

    #[test]
    fn display_names_are_validated_before_anything_is_stored() {
        assert!(validate_display_name("dev-vps").is_ok());
        assert!(validate_display_name("개발 서버").is_ok());
        assert!(matches!(
            validate_display_name("").unwrap_err(),
            Error::Usage(_)
        ));
        assert!(matches!(
            validate_display_name(" dev-vps").unwrap_err(),
            Error::Usage(_)
        ));
        assert!(matches!(
            validate_display_name("dev\tvps").unwrap_err(),
            Error::Usage(_)
        ));
        assert!(matches!(
            validate_display_name(&"a".repeat(MAX_NAME_COLS + 1)).unwrap_err(),
            Error::Usage(_)
        ));
    }

    #[test]
    fn ssh_spec_validation_covers_auth_and_address() {
        let base = SshSpec {
            display_name: "dev-vps".into(),
            host: "vps.example.com".into(),
            port: 22,
            username: Some("devdb".into()),
            auth: AuthType::Agent,
            identity_path: None,
            docker_command: "docker".into(),
        };
        assert!(validate_ssh_spec(&base).is_ok());

        let mut key_without_path = base.clone();
        key_without_path.auth = AuthType::Key;
        assert!(matches!(
            validate_ssh_spec(&key_without_path).unwrap_err(),
            Error::Usage(_)
        ));

        let mut key_with_path = key_without_path.clone();
        key_with_path.identity_path = Some("~/.ssh/id_ed25519".into());
        assert!(validate_ssh_spec(&key_with_path).is_ok());

        let mut zero_port = base.clone();
        zero_port.port = 0;
        assert!(matches!(
            validate_ssh_spec(&zero_port).unwrap_err(),
            Error::Usage(_)
        ));

        let mut spaced_host = base.clone();
        spaced_host.host = "vps example.com".into();
        assert!(matches!(
            validate_ssh_spec(&spaced_host).unwrap_err(),
            Error::Usage(_)
        ));
    }

    #[test]
    fn docker_command_falls_back_to_plain_docker() {
        assert_eq!(normalise_docker_command(""), "docker");
        assert_eq!(normalise_docker_command("   "), "docker");
        assert_eq!(
            normalise_docker_command(" /usr/bin/docker "),
            "/usr/bin/docker"
        );
    }
}
