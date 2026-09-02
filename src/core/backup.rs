//! Backup and restore (PRD §8.6).
//!
//! A dump always lands in a **local** file, including for SSH targets: the
//! `pg_dump` stdout produced inside the remote container is streamed straight
//! down the existing SSH channel into the file (BAK-003, decision §19.6). No
//! temporary copy is ever written on the remote host.

use crate::core::activity::Activity;
use crate::core::config::harden_file;
use crate::core::ctx::Ctx;
use crate::core::docker;
use crate::core::error::{Error, Result};
use crate::core::exec::{Executor, SecretEnv};
use crate::core::model::{BackupFormat, BackupRecord, BackupStatus, DatabaseView, ResourceKind};
use crate::core::pg;
use crate::core::plan::{Plan, StepKind};
use crate::core::progress::{Cancel, Reporter};
use crate::core::util;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Magic prefix of a `pg_dump -Fc` archive.
const CUSTOM_MAGIC: &[u8; 5] = b"PGDMP";

const CHUNK: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Naming and format detection
// ---------------------------------------------------------------------------

/// `letsbid_dev-20260831-030000.dump`. The stamp is UTC so it matches
/// [`BackupRecord::created_at`] and never repeats across a DST change.
fn backup_file_name(database: &str, at: DateTime<Utc>, f: BackupFormat) -> String {
    format!(
        "{database}-{}.{}",
        at.format("%Y%m%d-%H%M%S"),
        f.extension()
    )
}

/// Never overwrite an existing dump: two backups inside the same second get a
/// counter rather than one clobbering the other.
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

/// The archive header wins over the file name, because a renamed dump is far
/// more common than a mislabelled one. Without a header the extension decides,
/// and anything unrecognised is treated as plain SQL — a file that is not a
/// custom archive cannot be one.
pub fn detect_format(file: &Path) -> BackupFormat {
    use std::io::Read;
    if let Ok(mut handle) = std::fs::File::open(file) {
        let mut magic = [0u8; CUSTOM_MAGIC.len()];
        if handle.read_exact(&mut magic).is_ok() && &magic == CUSTOM_MAGIC {
            return BackupFormat::Custom;
        }
    }
    match file
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("dump") | Some("custom") | Some("pgdump") | Some("backup") | Some("bak") => {
            BackupFormat::Custom
        }
        _ => BackupFormat::Plain,
    }
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

fn run_plan(v: &DatabaseView, path: &Path, f: BackupFormat, previous: usize) -> Plan {
    let mut plan = Plan::new(format!("`{}` 백업", v.database.database_name))
        .step_detailed(
            StepKind::Verify,
            format!("엔진 {} 상태 확인", v.engine.container_name),
            format!("Target {}", v.target.display_name),
        )
        .step_detailed(
            StepKind::New,
            format!("pg_dump 실행 ({} 형식)", f.as_str()),
            format!(
                "컨테이너 안에서 {} 계정으로 실행합니다.",
                v.engine.admin_user
            ),
        )
        .step_detailed(
            StepKind::New,
            "로컬 파일로 저장",
            format!("{} (0600)", path.display()),
        )
        .step(StepKind::Verify, "SHA-256 체크섬 계산");
    if v.target.is_remote() {
        plan = plan.warn(
            "원격 백업은 SSH 스트림으로 곧바로 로컬 파일에 저장됩니다. VPS에는 아무것도 남지 않습니다.",
        );
    }
    if let Some(size) = v.stats.size_bytes {
        plan = plan.warn(format!(
            "대상 DB 크기는 약 {}입니다. 저장 폴더의 여유 공간을 확인하세요.",
            util::human_bytes(size.max(0) as u64)
        ));
    }
    if previous > 0 {
        plan = plan.warn(format!(
            "이 DB의 기존 백업 기록 {previous}건은 그대로 유지됩니다."
        ));
    }
    plan
}

pub async fn plan_run(
    ctx: &Ctx,
    v: &DatabaseView,
    out_dir: &Path,
    f: BackupFormat,
) -> Result<Plan> {
    let previous = ctx.store.list_backups(Some(&v.database.id))?.len();
    let path = out_dir.join(backup_file_name(&v.database.database_name, util::now(), f));
    Ok(run_plan(v, &path, f, previous))
}

/// BAK-001/002/003/004/007. Streams the dump into a local `0600` file,
/// reports transferred bytes, and can be cancelled at any chunk boundary. A
/// cancelled or failed run deletes the partial file and leaves a `failed`
/// record behind so the attempt is still visible.
pub async fn run(
    ctx: &Ctx,
    v: &DatabaseView,
    out_dir: &Path,
    f: BackupFormat,
    r: &Reporter,
    c: &Cancel,
) -> Result<BackupRecord> {
    ctx.require_write_lock()?;
    let x = ctx.executor(&v.target)?;
    std::fs::create_dir_all(out_dir)?;
    let directory = std::fs::canonicalize(out_dir).unwrap_or_else(|_| out_dir.to_path_buf());

    let created_at = util::now();
    let path = unique_path(
        &directory,
        &backup_file_name(&v.database.database_name, created_at, f),
    )?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("backup")
        .to_string();

    let mut record = BackupRecord {
        id: util::new_id(),
        resource_id: v.database.id.clone(),
        resource_kind: ResourceKind::Database,
        storage_location: directory.display().to_string(),
        file_name,
        format: f,
        size: 0,
        checksum: String::new(),
        status: BackupStatus::Running,
        created_at,
    };
    ctx.store.insert_backup(&record)?;

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "backup",
        "run",
        format!(
            "`{}`을(를) {} 형식으로 백업",
            v.database.database_name,
            f.as_str()
        ),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.database.id);

    let result = run_inner(ctx, &x, v, f, &path, &mut record, r, c, &mut act).await;
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
async fn run_inner(
    ctx: &Ctx,
    x: &Executor,
    v: &DatabaseView,
    f: BackupFormat,
    path: &Path,
    record: &mut BackupRecord,
    r: &Reporter,
    c: &Cancel,
    act: &mut Activity<'_>,
) -> Result<()> {
    c.check()?;
    let argv = pg::dump_argv(x.docker_bin(), &v.engine, &v.database.database_name, f);

    r.step(1, 3, format!("{} 덤프 중", v.database.database_name));
    let bytes = {
        let mut sink = tokio::fs::File::from_std(create_private(path)?);
        let (out, bytes) = x
            .stream_out(&argv, &SecretEnv::new(), &mut sink, c, r)
            .await?;
        sink.flush().await?;
        sink.sync_all().await?;
        if !out.ok() {
            return Err(x.failure(
                &argv,
                &out,
                &format!("`{}` 백업에 실패했습니다", v.database.database_name),
                "엔진이 실행 중인지, DB 이름이 정확한지 확인한 뒤 다시 시도하세요.",
            ));
        }
        bytes
    };
    if bytes == 0 {
        return Err(Error::failed(
            format!("`{}` 백업 결과가 비어 있습니다", v.database.database_name),
            "pg_dump가 한 바이트도 출력하지 않았습니다.",
            "엔진 로그를 확인한 뒤 다시 시도하세요.",
        ));
    }
    act.step(format!("{} 덤프 완료", util::human_bytes(bytes)));
    r.step_done(1);

    r.step(2, 3, "체크섬 계산");
    record.size = bytes;
    record.checksum = sha256_file(path).await?;
    r.step_done(2);

    r.step(3, 3, "기록 갱신");
    let mut row = v.database.clone();
    row.last_backup_at = Some(util::now());
    ctx.store.update_database(&row)?;
    act.step(format!("{} 저장", path.display()));
    r.step_done(3);
    Ok(())
}

/// A dump may contain every row of a project's data, so it is never
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

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// Which client tool consumes each dump format on the way back in.
fn restore_tool(f: BackupFormat) -> &'static str {
    match f {
        BackupFormat::Custom => "pg_restore",
        BackupFormat::Plain => "psql",
        // Object archives belong to a MinIO engine and never reach this module.
        BackupFormat::Objects => "mc",
    }
}

fn restore_plan(
    file: &Path,
    size: u64,
    f: BackupFormat,
    v: &DatabaseView,
    overwrite: bool,
    has_objects: bool,
) -> Plan {
    let mut plan = Plan::new(format!("`{}`(으)로 복원", v.database.database_name)).step_detailed(
        StepKind::Verify,
        "백업 파일 확인",
        format!(
            "{} · {} · {} 형식",
            file.display(),
            util::human_bytes(size),
            f.as_str()
        ),
    );
    if has_objects {
        if overwrite {
            plan = plan
                .step_detailed(
                    StepKind::Destroy,
                    format!("`{}`의 기존 데이터를 덮어씁니다", v.database.database_name),
                    "백업에 들어 있는 객체와 이름이 같은 기존 객체는 교체됩니다.",
                )
                .warn("덮어쓰기: 이 작업은 되돌릴 수 없습니다. 먼저 현재 상태를 백업하세요.");
        } else {
            plan = plan.warn(format!(
                "대상 DB `{}`에 이미 데이터가 있어 복원이 거부됩니다. \
                 덮어쓰려면 overwrite를 지정하세요.",
                v.database.database_name
            ));
        }
    } else {
        plan = plan.step_detailed(
            StepKind::Reuse,
            format!("`{}`은(는) 비어 있습니다", v.database.database_name),
            "덮어써지는 데이터가 없습니다.",
        );
    }
    plan = plan
        .step_detailed(
            StepKind::New,
            format!("{} 실행", restore_tool(f)),
            format!(
                "파일을 컨테이너 stdin으로 흘려보냅니다 (Target {}).",
                v.target.display_name
            ),
        )
        .step_detailed(
            StepKind::Verify,
            format!("소유권을 {}(으)로 재설정", v.database.username),
            "복원된 객체가 프로젝트 계정 소유가 되도록 정리합니다.",
        );
    plan
}

/// BAK-006: the preview always says, in words, whether data will be lost.
pub async fn plan_restore(
    ctx: &Ctx,
    file: &Path,
    v: &DatabaseView,
    overwrite: bool,
) -> Result<Plan> {
    let meta = std::fs::metadata(file).map_err(|_| {
        Error::NotFound(format!(
            "백업 파일 `{}`을(를) 찾을 수 없습니다.",
            file.display()
        ))
    })?;
    let f = detect_format(file);
    let x = ctx.executor(&v.target)?;
    let has_objects = pg::has_user_objects(&x, &v.engine, &v.database.database_name).await?;
    Ok(restore_plan(file, meta.len(), f, v, overwrite, has_objects))
}

/// BAK-005/006. Refuses to touch a database that already holds user objects
/// unless `overwrite` was granted explicitly.
pub async fn restore(
    ctx: &Ctx,
    file: &Path,
    v: &DatabaseView,
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

    let mut act = Activity::start(
        &ctx.store,
        ctx.origin,
        "backup",
        "restore",
        format!(
            "`{}`을(를) `{}`(으)로 복원{}",
            file.display(),
            v.database.database_name,
            if overwrite { " (덮어쓰기)" } else { "" }
        ),
    )?
    .on_target(&v.target.id)
    .on_resource(&v.database.id);

    let result = restore_inner(&x, file, v, overwrite, r, c, &mut act).await;
    act.finish(&result);
    result
}

async fn restore_inner(
    x: &Executor,
    file: &Path,
    v: &DatabaseView,
    overwrite: bool,
    r: &Reporter,
    c: &Cancel,
    act: &mut Activity<'_>,
) -> Result<()> {
    c.check()?;
    let database = &v.database.database_name;
    let f = detect_format(file);
    let mut reset_destination = false;

    r.step(1, 3, "대상 DB 확인");
    if pg::database_exists(x, &v.engine, database).await? {
        let has_objects = pg::has_user_objects(x, &v.engine, database).await?;
        if has_objects && !overwrite {
            return Err(Error::Refused(format!(
                "대상 DB `{database}`에 이미 데이터가 있습니다. \
                 덮어쓰려면 overwrite를 지정하고 다시 실행하세요."
            )));
        }
        if overwrite {
            let drop_sql = format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = {} AND pid <> pg_backend_pid();",
                pg::quote_literal(database)
            );
            let _ = pg::psql(x, &v.engine, &drop_sql).await;
            pg::psql(
                x,
                &v.engine,
                &format!("DROP DATABASE {};", pg::quote_ident(database)),
            )
            .await?;
            pg::create_database_owned_by(x, &v.engine, database, &v.database.username).await?;
            reset_destination = true;
            act.step(format!("대상 DB {database}을(를) 비우고 다시 만들었습니다"));
        }
    } else {
        pg::create_database_owned_by(x, &v.engine, database, &v.database.username).await?;
        reset_destination = true;
        act.step(format!("대상 DB {database} 생성"));
    }
    r.step_done(1);
    c.check()?;

    r.step(2, 3, format!("{} 실행", restore_tool(f)));
    let argv = pg::restore_argv(x.docker_bin(), &v.engine, database, f);
    let mut source = tokio::fs::File::open(file).await?;
    let (out, bytes) = match x
        .stream_in(&argv, &SecretEnv::new(), &mut source, c, r)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            undo_reset(x, v, reset_destination, act).await;
            return Err(e);
        }
    };
    if !out.ok() {
        undo_reset(x, v, reset_destination, act).await;
        return Err(x.failure(
            &argv,
            &out,
            &format!("`{database}` 복원에 실패했습니다"),
            "백업 파일 형식과 대상 DB 상태를 확인한 뒤 다시 시도하세요.",
        ));
    }
    act.step(format!("{} 복원", util::human_bytes(bytes)));
    r.step_done(2);

    r.step(3, 3, "소유권 재설정");
    if let Err(e) = pg::take_ownership(x, &v.engine, database, &v.database.username).await {
        undo_reset(x, v, reset_destination, act).await;
        return Err(e);
    }
    act.step(format!("소유권을 {}(으)로 이전", v.database.username));
    r.step_done(3);
    Ok(())
}

async fn undo_reset(
    x: &Executor,
    v: &DatabaseView,
    reset_destination: bool,
    act: &mut Activity<'_>,
) {
    if !reset_destination {
        return;
    }
    let database = &v.database.database_name;
    let _ = pg::psql(
        x,
        &v.engine,
        &format!("DROP DATABASE IF EXISTS {};", pg::quote_ident(database)),
    )
    .await;
    act.step(format!("복원 실패로 대상 DB {database}을(를) 삭제했습니다"));
}

// ---------------------------------------------------------------------------
// Verification and listing
// ---------------------------------------------------------------------------

/// BAK-009: re-hash the file on disk and compare it with what was recorded
/// when the dump was written.
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
        "backup",
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

pub fn list(ctx: &Ctx, database_id: Option<&str>) -> Result<Vec<BackupRecord>> {
    ctx.store.list_backups(database_id)
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
    use crate::core::model::{
        AuthType, DatabaseStats, EngineInstance, EngineKind, ManagedDatabase, Target, TargetKind,
    };
    use chrono::TimeZone;

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
            ssh_username: None,
            auth_type: remote.then_some(AuthType::Agent),
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: remote.then(|| "SHA256:abc".to_string()),
            created_at: Utc::now(),
            last_connected_at: None,
        }
    }

    fn view_of(remote: bool) -> DatabaseView {
        DatabaseView {
            database: ManagedDatabase {
                id: "db-1".into(),
                engine_instance_id: "eng-1".into(),
                project_name: "Letsbid".into(),
                database_name: "letsbid_dev".into(),
                username: "letsbid_user".into(),
                credential_ref: "database:db-1".into(),
                preferred_local_tunnel_port: None,
                created_at: Utc::now(),
                last_connection_test_at: None,
                last_backup_at: None,
            },
            engine: EngineInstance {
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
            },
            target: target(remote),
            stats: DatabaseStats::default(),
            tunnel: None,
        }
    }

    #[test]
    fn file_name_carries_database_and_utc_stamp() {
        let at = Utc.with_ymd_and_hms(2026, 8, 31, 3, 4, 5).unwrap();
        assert_eq!(
            backup_file_name("letsbid_dev", at, BackupFormat::Custom),
            "letsbid_dev-20260831-030405.dump"
        );
        assert_eq!(
            backup_file_name("letsbid_dev", at, BackupFormat::Plain),
            "letsbid_dev-20260831-030405.sql"
        );
    }

    #[test]
    fn unique_path_never_clobbers() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_path(dir.path(), "letsbid_dev-20260831-030405.dump").unwrap();
        assert_eq!(
            first.file_name().unwrap(),
            "letsbid_dev-20260831-030405.dump"
        );
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(dir.path(), "letsbid_dev-20260831-030405.dump").unwrap();
        assert_eq!(
            second.file_name().unwrap(),
            "letsbid_dev-20260831-030405-2.dump"
        );
    }

    #[test]
    fn detect_format_by_extension() {
        assert_eq!(
            detect_format(Path::new("/nowhere/letsbid.sql")),
            BackupFormat::Plain
        );
        assert_eq!(
            detect_format(Path::new("/nowhere/letsbid.dump")),
            BackupFormat::Custom
        );
        assert_eq!(
            detect_format(Path::new("/nowhere/letsbid.DUMP")),
            BackupFormat::Custom
        );
        assert_eq!(
            detect_format(Path::new("/nowhere/letsbid")),
            BackupFormat::Plain
        );
    }

    #[test]
    fn detect_format_by_magic_beats_the_extension() {
        let dir = tempfile::tempdir().unwrap();

        // A custom archive that someone renamed to `.sql`.
        let renamed = dir.path().join("renamed.sql");
        std::fs::write(&renamed, b"PGDMP\x01\x0e\x00").unwrap();
        assert_eq!(detect_format(&renamed), BackupFormat::Custom);

        // Real SQL text named `.dump` still has no header, so the extension
        // decides — the header can only ever prove `Custom`.
        let text = dir.path().join("script.sql");
        std::fs::write(&text, b"-- dumped by pg_dump\nCREATE TABLE t();\n").unwrap();
        assert_eq!(detect_format(&text), BackupFormat::Plain);

        // Too short to hold a header.
        let tiny = dir.path().join("tiny.sql");
        std::fs::write(&tiny, b"PG").unwrap();
        assert_eq!(detect_format(&tiny), BackupFormat::Plain);
    }

    #[test]
    fn run_plan_streams_remote_dumps_to_a_local_file() {
        let mut v = view_of(true);
        v.stats.size_bytes = Some(251_658_240);
        let plan = run_plan(
            &v,
            Path::new("/backups/letsbid_dev-20260831-030405.dump"),
            BackupFormat::Custom,
            0,
        );
        assert!(!plan.is_destructive());
        assert_eq!(plan.steps.len(), 4);
        let rendered = plan.render();
        assert!(
            rendered.contains("pg_dump 실행 (custom 형식)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("/backups/letsbid_dev-20260831-030405.dump (0600)"),
            "{rendered}"
        );
        assert!(rendered.contains("SHA-256"), "{rendered}");
        assert!(rendered.contains("SSH 스트림"), "{rendered}");
        assert!(rendered.contains("약 240 MB"), "{rendered}");
    }

    #[test]
    fn restore_plan_with_overwrite_is_destructive_and_says_so() {
        let plan = restore_plan(
            Path::new("/backups/letsbid_dev-20260831-030405.dump"),
            1024,
            BackupFormat::Custom,
            &view_of(false),
            true,
            true,
        );
        assert!(plan.is_destructive());
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(plan.steps[1].kind, StepKind::Destroy);
        let rendered = plan.render();
        assert!(rendered.contains("기존 데이터를 덮어씁니다"), "{rendered}");
        assert!(rendered.contains("되돌릴 수 없습니다"), "{rendered}");
        assert!(rendered.contains("pg_restore 실행"), "{rendered}");
        assert!(rendered.contains("소유권을 letsbid_user"), "{rendered}");
    }

    #[test]
    fn restore_plan_without_overwrite_warns_it_will_be_refused() {
        let plan = restore_plan(
            Path::new("/backups/x.dump"),
            1024,
            BackupFormat::Custom,
            &view_of(false),
            false,
            true,
        );
        assert!(!plan.is_destructive());
        let rendered = plan.render();
        assert!(rendered.contains("복원이 거부됩니다"), "{rendered}");
    }

    #[test]
    fn restore_plan_into_an_empty_database_loses_nothing() {
        let plan = restore_plan(
            Path::new("/backups/x.sql"),
            1024,
            BackupFormat::Plain,
            &view_of(false),
            false,
            false,
        );
        assert!(!plan.is_destructive());
        assert_eq!(plan.steps[1].kind, StepKind::Reuse);
        let rendered = plan.render();
        assert!(
            rendered.contains("덮어써지는 데이터가 없습니다"),
            "{rendered}"
        );
        assert!(rendered.contains("psql 실행"), "{rendered}");
    }

    #[tokio::test]
    async fn sha256_matches_the_reference_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).await.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn sha256_reads_files_larger_than_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let payload = vec![7u8; CHUNK * 2 + 13];
        std::fs::write(&path, &payload).unwrap();
        let expected: String = Sha256::digest(&payload)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(sha256_file(&path).await.unwrap(), expected);
    }

    #[test]
    fn dumps_are_created_unreadable_to_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.dump");
        let _file = create_private(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        // Creating over an existing file is refused, which is what keeps
        // `unique_path` honest under a race.
        assert!(create_private(&path).is_err());
    }
}
