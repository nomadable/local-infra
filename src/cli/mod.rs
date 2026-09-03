//! Headless command surface (PRD §8.9).
//!
//! Two rules make the CLI trustworthy for scripts:
//!
//! * Everything the TUI can do is reachable here, because both call the same
//!   `core` use cases (principle 7). [`Command::palette_names`] and the test at
//!   the bottom of this file keep the command-palette names in step 1:1.
//! * No subcommand ever accepts a password. Values are generated or read from
//!   stdin, so nothing lands in shell history (PRD §11.2).

mod agent_skill;
pub mod output;
use crate::core::config::SecretMode;
use crate::core::error::{Error, Result};
use crate::core::model::{AuthType, BackupFormat, EngineKind, Origin, ResourceKind};
use crate::core::progress::{Cancel, Reporter};
use crate::core::{backup, bucket, database, discovery, doctor, engine, ssh, target, tunnel, Ctx};
use clap::{Args, CommandFactory, Parser, Subcommand};
use output::{report, table, Emitter, Format};
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "linf",
    version,
    about = "local-infra — 로컬과 원격 개발 DB 엔진을 터미널에서 공유 관리합니다",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// 기계 판독 가능한 JSON으로 출력합니다.
    #[arg(long, global = true)]
    pub json: bool,

    /// 파괴적 작업의 확인을 생략합니다.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 환경을 진단합니다.
    Doctor,
    /// Target(로컬 Docker 또는 SSH 호스트)을 관리합니다.
    Target {
        #[command(subcommand)]
        cmd: TargetCmd,
    },
    /// 공유 DB 엔진 컨테이너를 관리합니다.
    Engine {
        #[command(subcommand)]
        cmd: EngineCmd,
    },
    /// 프로젝트별 데이터베이스를 관리합니다.
    Db {
        #[command(subcommand)]
        cmd: DbCmd,
    },
    /// 프로젝트별 오브젝트 스토리지 버킷을 관리합니다.
    Bucket {
        #[command(subcommand)]
        cmd: BucketCmd,
    },
    /// 원격 DB로 가는 SSH 터널을 관리합니다.
    Tunnel {
        #[command(subcommand)]
        cmd: TunnelCmd,
    },
    /// 백업과 복원을 수행합니다.
    Backup {
        #[command(subcommand)]
        cmd: BackupCmd,
    },
    /// 앱이 관리하지 않는 컨테이너를 읽기 전용으로 탐색합니다.
    Discover {
        /// Target 이름 또는 id.
        target: String,
    },
    /// 등록 정보와 이 앱이 만든 Docker 리소스를 모두 삭제합니다.
    Reset,
    /// Agent Skill을 프로젝트 또는 지정한 skill 폴더에 설치합니다.
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },

    /// 셸 자동완성 스크립트를 출력합니다.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum TargetCmd {
    /// 로컬 Docker를 Target으로 등록합니다.
    AddLocal {
        /// 표시 이름.
        #[arg(long, default_value = "local")]
        name: String,
        /// 사용할 docker 실행 파일.
        #[arg(long, default_value = "docker")]
        docker: String,
    },
    /// SSH 호스트를 Target으로 등록합니다.
    AddSsh {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long)]
        user: Option<String>,
        /// SSH 개인키 경로. 생략하면 ssh-agent를 사용합니다.
        #[arg(long)]
        identity: Option<String>,
        #[arg(long, default_value = "docker")]
        docker: String,
        /// 서버에서 직접 확인한 호스트 키 지문(`SHA256:…`).
        /// 비대화형 환경에서는 반드시 필요합니다.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// `~/.ssh/config`의 호스트 목록을 보여줍니다.
    SshConfig,
    /// 등록된 Target을 나열합니다.
    List,
    /// SSH와 Docker 권한을 각각 테스트합니다.
    Test { target: String },
    /// 호스트 키 지문을 조회합니다(등록 전 확인용).
    Verify {
        host: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
    },
    /// 등록만 해제합니다. Docker 리소스는 건드리지 않습니다.
    Forget { target: String },
}

#[derive(Debug, Args)]
pub struct EngineRef {
    /// Target 이름 또는 id.
    pub target: String,
    /// 엔진 종류: postgres 또는 minio.
    #[arg(default_value = "postgres")]
    pub engine: String,
    /// 메이저 버전. 생략하면 엔진의 기본값(postgres 17, minio latest).
    pub version: Option<String>,
}

impl EngineRef {
    fn kind(&self) -> Result<EngineKind> {
        EngineKind::parse(&self.engine)
            .ok_or_else(|| Error::Usage(format!("지원하지 않는 엔진입니다: `{}`", self.engine)))
    }

    fn version(&self) -> Result<String> {
        let kind = self.kind()?;
        Ok(self
            .version
            .clone()
            .unwrap_or_else(|| kind.default_major_version().to_string()))
    }
}

#[derive(Debug, Subcommand)]
pub enum EngineCmd {
    /// 엔진이 없으면 만들고, 있으면 그대로 사용합니다.
    Ensure {
        #[command(flatten)]
        r#ref: EngineRef,
        /// 호스트 포트. 생략하면 5432부터 비어 있는 포트를 찾습니다.
        #[arg(long)]
        port: Option<u16>,
        /// 바인딩 주소. 기본값은 루프백입니다.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// 사용할 이미지. 생략하면 `postgres:<버전>`.
        #[arg(long)]
        image: Option<String>,
        /// 실행하지 않고 계획만 출력합니다.
        #[arg(long)]
        plan: bool,
    },
    /// 등록된 엔진을 나열합니다.
    List,
    Start {
        #[command(flatten)]
        r#ref: EngineRef,
    },
    Stop {
        #[command(flatten)]
        r#ref: EngineRef,
    },
    Restart {
        #[command(flatten)]
        r#ref: EngineRef,
    },
    /// 컨테이너 로그를 출력합니다.
    Logs {
        #[command(flatten)]
        r#ref: EngineRef,
        #[arg(long, default_value_t = 200)]
        tail: usize,
    },
    /// 엔진 컨테이너를 삭제합니다.
    Rm {
        #[command(flatten)]
        r#ref: EngineRef,
        /// 데이터 볼륨까지 영구 삭제합니다.
        #[arg(long)]
        volume: bool,
        #[arg(long)]
        plan: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DbCmd {
    /// 프로젝트용 DB와 전용 계정을 만듭니다.
    Create {
        #[arg(long)]
        target: String,
        #[arg(long)]
        project: String,
        /// DB명. 생략하면 프로젝트명에서 만듭니다.
        #[arg(long)]
        name: Option<String>,
        /// 계정명. 생략하면 프로젝트명에서 만듭니다.
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value = "postgres")]
        engine: String,
        #[arg(long, default_value = "17")]
        version: String,
        #[arg(long, default_value = "UTF8")]
        encoding: String,
        #[arg(long, default_value = "C")]
        locale: String,
        /// 이 DB의 터널이 항상 사용할 로컬 포트.
        #[arg(long)]
        tunnel_port: Option<u16>,
        #[arg(long)]
        plan: bool,
    },
    /// 관리 중인 DB를 나열합니다.
    List,
    /// 접속 URL을 stdout으로 출력합니다.
    Url { database: String },
    /// `.env` 블록을 stdout으로 출력합니다.
    Env { database: String },
    /// 접속 URL을 클립보드로 복사합니다.
    CopyUrl { database: String },
    /// `.env` 블록을 클립보드로 복사합니다.
    CopyEnv { database: String },
    /// 실제 접속을 테스트합니다.
    Test { database: String },
    /// DB와 전용 계정을 삭제합니다.
    Drop {
        database: String,
        #[arg(long)]
        plan: bool,
    },
    /// 실제 DB는 두고 등록만 해제합니다.
    Forget { database: String },
    /// 비밀번호를 교체합니다.
    RotatePassword { database: String },
    /// 같은 엔진에 DB를 복제합니다.
    Duplicate {
        database: String,
        /// 새 DB명.
        new_name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BucketCmd {
    /// 프로젝트용 버킷과 그 버킷만 접근하는 전용 액세스 키를 만듭니다.
    Create {
        #[arg(long)]
        target: String,
        #[arg(long)]
        project: String,
        /// 버킷명. 생략하면 프로젝트명에서 만듭니다.
        #[arg(long)]
        name: Option<String>,
        /// 액세스 키. 생략하면 무작위로 생성합니다.
        #[arg(long)]
        access_key: Option<String>,
        #[arg(long, default_value = "latest")]
        version: String,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 이 버킷의 터널이 항상 사용할 로컬 포트.
        #[arg(long)]
        tunnel_port: Option<u16>,
        #[arg(long)]
        plan: bool,
    },
    /// 관리 중인 버킷을 나열합니다.
    List,
    /// S3 접속 문자열을 stdout으로 출력합니다.
    Url { bucket: String },
    /// S3 엔드포인트 주소만 출력합니다.
    Endpoint { bucket: String },
    /// `.env` 블록을 stdout으로 출력합니다.
    Env { bucket: String },
    /// 접속 문자열을 클립보드로 복사합니다.
    CopyUrl { bucket: String },
    /// `.env` 블록을 클립보드로 복사합니다.
    CopyEnv { bucket: String },
    /// 실제 접근을 테스트합니다.
    Test { bucket: String },
    /// 버킷과 전용 계정을 삭제합니다.
    Drop {
        bucket: String,
        #[arg(long)]
        plan: bool,
    },
    /// 실제 버킷은 두고 등록만 해제합니다.
    Forget { bucket: String },
    /// 액세스 키를 교체합니다.
    RotateKey { bucket: String },
}

#[derive(Debug, Subcommand)]
pub enum TunnelCmd {
    Start {
        database: String,
    },
    Stop {
        database: String,
    },
    Restart {
        database: String,
    },
    /// 원격 리소스의 터널을 한 번에 모두 시작합니다.
    StartAll,
    Status,
}

#[derive(Debug, Subcommand)]
pub enum BackupCmd {
    /// DB를 로컬 파일로 백업합니다.
    Run {
        database: String,
        /// 저장할 디렉터리.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value = "custom")]
        format: String,
    },
    /// 백업 기록을 나열합니다.
    List { database: Option<String> },
    /// 백업 파일을 DB에 복원합니다.
    Restore {
        file: PathBuf,
        /// 복원 대상 DB.
        #[arg(long)]
        into: String,
        /// 기존 데이터를 덮어씁니다.
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        plan: bool,
    },
    /// 백업 파일의 체크섬을 검증합니다.
    Verify { id: String },
}

#[derive(Debug, Subcommand)]
pub enum SkillCmd {
    /// 번들된 Agent Skill을 설치합니다.
    Install {
        /// Skill을 설치할 루트 폴더. Claude Code 프로젝트 기본 경로는 `.claude/skills`입니다.
        #[arg(long, default_value = ".claude/skills")]
        dir: PathBuf,
        /// 기존 local-infrastructure Skill을 새 번들 내용으로 교체합니다.
        #[arg(long)]
        force: bool,
    },
}

impl Command {
    /// The palette name for this command, matching PRD §7.10's `: db create`
    /// form. Kept in sync with `tui::keymap::Action::name` by a test.
    pub fn palette_names() -> Vec<String> {
        let mut names = Vec::new();
        collect_names(&Cli::command(), &mut Vec::new(), &mut names);
        names
    }
}

fn collect_names(cmd: &clap::Command, path: &mut Vec<String>, out: &mut Vec<String>) {
    let subs: Vec<&clap::Command> = cmd.get_subcommands().collect();
    if subs.is_empty() {
        if !path.is_empty() {
            out.push(path.join("."));
        }
        return;
    }
    for sub in subs {
        path.push(sub.get_name().to_string());
        collect_names(sub, path, out);
        path.pop();
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parse, run, and translate the outcome into a process exit code (CLI-004).
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = if cli.json {
        Format::Json
    } else {
        Format::Human
    };

    // `linf` with no subcommand opens the TUI (CLI-001).
    let Some(command) = cli.command else {
        return match crate::tui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => ExitCode::from(report(&e, Format::Human) as u8),
        };
    };

    if let Command::Completions { shell } = command {
        let mut cmd = Cli::command();
        let bin = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, bin, &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }

    let emitter = Emitter::new(cli.json, cli.yes);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let err = Error::failed(
                "런타임을 시작할 수 없습니다",
                e.to_string(),
                "시스템 자원 상태를 확인하세요.",
            );
            return ExitCode::from(report(&err, format) as u8);
        }
    };

    match runtime.block_on(dispatch(command, emitter)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => ExitCode::from(report(&e, format) as u8),
    }
}

/// Open the context and surface non-fatal notices on stderr.
fn context(emitter: &Emitter) -> Result<Ctx> {
    let ctx = Ctx::open(Origin::Cli)?;
    for notice in &ctx.notices {
        emitter.warn(notice);
    }
    Ok(ctx)
}

/// Run `body` with a progress reporter wired to stderr and `Ctrl+C` mapped to
/// cooperative cancellation (TUI-006's headless twin).
async fn reported<T, F>(emitter: Emitter, body: impl FnOnce(Reporter, Cancel) -> F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let (reporter, mut rx) = Reporter::channel();
    let cancel = Cancel::new();
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            emitter.progress(&event);
        }
    });
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });

    let outcome = body(reporter, cancel).await;
    signal.abort();
    let _ = printer.await;
    outcome
}

async fn dispatch(command: Command, e: Emitter) -> Result<()> {
    match command {
        Command::Completions { .. } => Ok(()),
        Command::Doctor => run_doctor(e).await,
        Command::Target { cmd } => run_target(cmd, e).await,
        Command::Engine { cmd } => run_engine(cmd, e).await,
        Command::Db { cmd } => run_db(cmd, e).await,
        Command::Bucket { cmd } => run_bucket(cmd, e).await,
        Command::Tunnel { cmd } => run_tunnel(cmd, e).await,
        Command::Backup { cmd } => run_backup(cmd, e).await,
        Command::Reset => run_reset(e).await,
        Command::Skill { cmd } => run_skill(cmd, e).await,
        Command::Discover { target } => run_discover(&target, e).await,
    }
}

async fn run_skill(cmd: SkillCmd, e: Emitter) -> Result<()> {
    match cmd {
        SkillCmd::Install { dir, force } => {
            let receipt = agent_skill::install(&dir, force)?;
            e.data(&receipt, || {
                println!("Agent Skill을 `{}`에 설치했습니다.", receipt.path);
                println!("새 agent 세션에서 로컬 인프라 요청을 시작하세요.");
            })
        }
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

async fn run_doctor(e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    let checks = doctor::run(&ctx).await?;
    let failed = checks.iter().filter(|c| !c.ok).count();
    e.data(&checks, || {
        for check in &checks {
            println!("{} {}", if check.ok { "ok  " } else { "FAIL" }, check.name);
            if !check.detail.is_empty() {
                println!("     {}", check.detail);
            }
            if let (false, Some(remedy)) = (check.ok, &check.remedy) {
                println!("     조치: {remedy}");
            }
        }
    })?;
    if failed > 0 && !e.is_json() {
        e.warn(format!("{failed}개 항목이 실패했습니다."));
    }
    Ok(())
}

async fn run_reset(e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    ctx.require_write_lock()?;
    let engines = ctx.store.list_engines()?;
    let mut preview = crate::core::plan::Plan::new("모든 등록과 관리 컨테이너를 삭제합니다");
    if engines.is_empty() {
        preview = preview.step(crate::core::plan::StepKind::Verify, "등록된 엔진 없음");
    }
    for engine in &engines {
        preview = preview.step(
            crate::core::plan::StepKind::Destroy,
            format!(
                "{} 컨테이너와 볼륨 {}",
                engine.container_name, engine.volume_name
            ),
        );
    }
    preview = preview
        .warn("이 앱이 만든 PostgreSQL / MinIO 데이터가 영구 삭제됩니다.")
        .warn("등록된 Target, DB, 버킷, 터널 기록도 함께 지웁니다.");
    e.confirm_by_name("reset", &preview)?;
    let report = reported(e, |reporter, _| async move {
        engine::reset_all(&ctx, &reporter).await
    })
    .await?;
    e.note(format!(
        "초기화했습니다. 엔진 {}개, Target {}개를 삭제했습니다.",
        report.engines_removed, report.targets_removed
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// target
// ---------------------------------------------------------------------------

async fn run_target(cmd: TargetCmd, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    match cmd {
        TargetCmd::AddLocal { name, docker } => {
            ctx.require_write_lock()?;
            let t = target::add_local(
                &ctx,
                &target::LocalSpec {
                    display_name: name,
                    docker_command: docker,
                },
            )
            .await?;
            e.data(&t, || {
                println!("Target `{}`을(를) 등록했습니다.", t.display_name)
            })
        }
        TargetCmd::AddSsh {
            name,
            host,
            port,
            user,
            identity,
            docker,
            fingerprint,
        } => {
            ctx.require_write_lock()?;
            let approved = match fingerprint {
                Some(f) => f,
                None => approve_fingerprint_interactively(&e, &host, port)?,
            };
            let spec = target::SshSpec {
                display_name: name,
                host,
                port,
                username: user,
                auth: if identity.is_some() {
                    AuthType::Key
                } else {
                    AuthType::Agent
                },
                identity_path: identity,
                docker_command: docker,
            };
            let t = target::add_ssh(&ctx, &spec, &approved).await?;
            e.data(&t, || {
                println!("Target `{}`을(를) 등록했습니다.", t.display_name)
            })
        }
        TargetCmd::SshConfig => {
            let hosts = ssh::config_hosts()?;
            e.data(&hosts, || {
                let rows: Vec<Vec<String>> = hosts
                    .iter()
                    .map(|h| {
                        vec![
                            h.alias.clone(),
                            h.host_name.clone().unwrap_or_default(),
                            h.user.clone().unwrap_or_default(),
                            h.port.map(|p| p.to_string()).unwrap_or_default(),
                            h.identity_file.clone().unwrap_or_default(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(&["ALIAS", "HOST", "USER", "PORT", "IDENTITY"], &rows)
                );
            })
        }
        TargetCmd::List => {
            let overview = target::overview(&ctx).await?;
            e.data(&overview, || {
                let rows: Vec<Vec<String>> = overview
                    .iter()
                    .map(|o| {
                        vec![
                            o.target.display_name.clone(),
                            o.target.location(),
                            if o.reachable {
                                "connected"
                            } else {
                                "unreachable"
                            }
                            .into(),
                            o.docker.clone().unwrap_or_else(|| "-".into()),
                            o.detail.clone(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(&["NAME", "LOCATION", "STATE", "DOCKER", "DETAIL"], &rows)
                );
            })
        }
        TargetCmd::Test { target: key } => {
            let t = target::get(&ctx, &key)?;
            let checks = target::test(&ctx, &t).await?;
            let failed = checks.iter().any(|c| !c.ok);
            e.data(&checks, || {
                for c in &checks {
                    println!(
                        "{} {} — {}",
                        if c.ok { "ok  " } else { "FAIL" },
                        c.name,
                        c.detail
                    );
                    if let (false, Some(r)) = (c.ok, &c.remedy) {
                        println!("     조치: {r}");
                    }
                }
            })?;
            if failed {
                return Err(Error::failed(
                    format!("Target `{key}` 점검에 실패했습니다"),
                    "위 항목 중 하나 이상이 통과하지 못했습니다.",
                    "출력된 조치를 수행한 뒤 `linf target test`를 다시 실행하세요.",
                ));
            }
            Ok(())
        }
        TargetCmd::Verify { host, port } => {
            let keys = ssh::scan_host_keys(&host, port).await?;
            e.data(&keys, || {
                for k in &keys {
                    println!("{}:{}  {}  {}", k.host, k.port, k.key_type, k.fingerprint);
                }
            })
        }
        TargetCmd::Forget { target: key } => {
            ctx.require_write_lock()?;
            let t = target::get(&ctx, &key)?;
            target::forget(&ctx, &t)?;
            e.note(format!(
                "Target `{}` 등록을 해제했습니다. Docker 리소스는 그대로입니다.",
                t.display_name
            ));
            Ok(())
        }
    }
}

/// Show the offered fingerprints and require an explicit `yes` (TAR-005).
/// Without a TTY this is a usage error, never a silent trust-on-first-use.
fn approve_fingerprint_interactively(e: &Emitter, host: &str, port: u16) -> Result<String> {
    if !e.interactive || e.is_json() {
        return Err(Error::Usage(format!(
            "비대화형 환경에서는 `--fingerprint`가 필요합니다. \
             `linf target verify {host} --port {port}`로 지문을 확인한 뒤 전달하세요."
        )));
    }
    let keys = futures_block_on_scan(host, port)?;
    eprintln!("호스트  {host}:{port}");
    for k in &keys {
        eprintln!("타입    {}", k.key_type);
        eprintln!("지문    {}", k.fingerprint);
    }
    eprintln!("이 지문이 서버에서 확인한 값과 같습니까?");
    eprint!("승인하려면 `yes`를 입력하세요: ");
    use std::io::Write;
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != "yes" {
        return Err(Error::Refused("호스트 키 승인을 취소했습니다.".into()));
    }
    keys.first()
        .map(|k| k.fingerprint.clone())
        .ok_or_else(|| Error::NotFound(format!("{host}:{port}에서 호스트 키를 받지 못했습니다.")))
}

/// The approval prompt is synchronous; the scan is not.
fn futures_block_on_scan(host: &str, port: u16) -> Result<Vec<ssh::HostKey>> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(ssh::scan_host_keys(host, port))
    })
}

// ---------------------------------------------------------------------------
// engine
// ---------------------------------------------------------------------------

fn engine_spec(r: &EngineRef) -> Result<engine::EngineSpec> {
    Ok(engine::EngineSpec::new(r.kind()?, &r.version()?))
}

async fn resolve_engine(
    ctx: &Ctx,
    r: &EngineRef,
) -> Result<(
    crate::core::model::Target,
    crate::core::model::EngineInstance,
)> {
    let t = target::get(ctx, &r.target)?;
    let kind = r.kind()?;
    let version = r.version()?;
    let found = ctx
        .store
        .find_engine(&t.id, kind, &version)?
        .ok_or_else(|| {
            Error::NotFound(format!(
                "Target `{}`에 {} {} 엔진이 없습니다. 먼저 `linf engine ensure`를 실행하세요.",
                t.display_name, r.engine, version
            ))
        })?;
    Ok((t, found))
}

async fn run_engine(cmd: EngineCmd, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    match cmd {
        EngineCmd::Ensure {
            r#ref,
            port,
            bind,
            image,
            plan,
        } => {
            let t = target::get(&ctx, &r#ref.target)?;
            let mut spec = engine_spec(&r#ref)?;
            spec.host_port = port;
            spec.bind_address = bind;
            spec.image = image;
            if plan {
                let plan = engine::plan_ensure(&ctx, &t, &spec).await?;
                return e.plan(&plan);
            }
            ctx.require_write_lock()?;
            let instance = reported(e, |reporter, cancel| async move {
                engine::ensure(&ctx, &t, &spec, &reporter, &cancel).await
            })
            .await?;
            e.data(&instance, || {
                println!(
                    "{} 엔진 준비 완료: {} ({}:{})",
                    instance.label(),
                    instance.container_name,
                    instance.bind_address,
                    instance.host_port
                );
            })
        }
        EngineCmd::List => {
            let overview = engine::overview(&ctx).await?;
            e.data(&overview, || {
                let rows: Vec<Vec<String>> = overview
                    .iter()
                    .map(|o| {
                        vec![
                            o.target.display_name.clone(),
                            o.engine.label(),
                            o.status.symbol().to_string() + " " + &o.status.state,
                            format!("{}:{}", o.engine.bind_address, o.engine.host_port),
                            o.database_count.to_string(),
                            o.engine.container_name.clone(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &["TARGET", "ENGINE", "STATE", "BIND", "DB", "CONTAINER"],
                        &rows
                    )
                );
            })
        }
        EngineCmd::Start { r#ref } => {
            ctx.require_write_lock()?;
            let (_, instance) = resolve_engine(&ctx, &r#ref).await?;
            engine::start(&ctx, &instance).await?;
            e.note(format!("{}을(를) 시작했습니다.", instance.container_name));
            Ok(())
        }
        EngineCmd::Stop { r#ref } => {
            ctx.require_write_lock()?;
            let (_, instance) = resolve_engine(&ctx, &r#ref).await?;
            engine::stop(&ctx, &instance).await?;
            e.note(format!("{}을(를) 중지했습니다.", instance.container_name));
            Ok(())
        }
        EngineCmd::Restart { r#ref } => {
            ctx.require_write_lock()?;
            let (_, instance) = resolve_engine(&ctx, &r#ref).await?;
            engine::restart(&ctx, &instance).await?;
            e.note(format!("{}을(를) 재시작했습니다.", instance.container_name));
            Ok(())
        }
        EngineCmd::Logs { r#ref, tail } => {
            let (_, instance) = resolve_engine(&ctx, &r#ref).await?;
            let text = engine::logs(&ctx, &instance, tail).await?;
            e.value(text);
            Ok(())
        }
        EngineCmd::Rm {
            r#ref,
            volume,
            plan,
        } => {
            let (_, instance) = resolve_engine(&ctx, &r#ref).await?;
            let preview = engine::plan_remove(&ctx, &instance, volume).await?;
            if plan {
                return e.plan(&preview);
            }
            ctx.require_write_lock()?;
            if volume {
                e.confirm_by_name(&instance.volume_name, &preview)?;
            } else {
                e.confirm_destructive("엔진 컨테이너 삭제", &preview)?;
            }
            reported(e, |reporter, _| async move {
                engine::remove(&ctx, &instance, volume, &reporter).await
            })
            .await?;
            e.note("엔진을 삭제했습니다.");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// db
// ---------------------------------------------------------------------------

async fn run_db(cmd: DbCmd, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    match cmd {
        DbCmd::Create {
            target: target_key,
            project,
            name,
            user,
            engine: engine_name,
            version,
            encoding,
            locale,
            tunnel_port,
            plan,
        } => {
            let t = target::get(&ctx, &target_key)?;
            let mut espec = engine_spec(&EngineRef {
                target: target_key,
                engine: engine_name,
                version: Some(version),
            })?;
            if espec.engine != EngineKind::Postgres {
                return Err(Error::Usage(
                    "`linf db`는 PostgreSQL 전용입니다. 오브젝트 스토리지는 `linf bucket`을 사용하세요."
                        .into(),
                ));
            }
            espec.bind_address = "127.0.0.1".into();
            espec.host_port = None;

            let mut spec = database::CreateSpec::for_project(&project);
            if let Some(n) = name {
                spec.database_name = n;
            }
            if let Some(u) = user {
                spec.username = u;
            }
            spec.encoding = encoding;
            spec.locale = locale;
            spec.preferred_local_tunnel_port = tunnel_port;
            database::validate_new_names(&spec.database_name, &spec.username)?;

            if plan {
                let preview = database::plan_create(&ctx, &t, &espec, &spec).await?;
                return e.plan(&preview);
            }
            ctx.require_write_lock()?;
            let created = reported(e, |reporter, cancel| async move {
                database::create(&ctx, &t, &espec, &spec, &reporter, &cancel).await
            })
            .await?;

            #[derive(serde::Serialize)]
            struct CreatedOut<'a> {
                database: &'a crate::core::model::ManagedDatabase,
                engine: &'a crate::core::model::EngineInstance,
                url: String,
                redacted_url: String,
            }
            let payload = CreatedOut {
                database: &created.database,
                engine: &created.engine,
                url: created.connection.url(),
                redacted_url: created.connection.redacted_url(),
            };
            e.data(&payload, || {
                println!(
                    "DB `{}`을(를) 만들었습니다.",
                    created.database.database_name
                );
                println!("{}", created.connection.redacted_url());
                println!(
                    "접속 URL은 `linf db url {}`로 확인하세요.",
                    created.database.database_name
                );
            })
        }
        DbCmd::List => {
            let views = database::views(&ctx, true).await?;
            e.data(&views, || {
                let rows: Vec<Vec<String>> = views
                    .iter()
                    .map(|v| {
                        vec![
                            v.target.display_name.clone(),
                            v.database.database_name.clone(),
                            v.engine.label(),
                            v.stats
                                .size_bytes
                                .map(|b| crate::core::util::human_bytes(b as u64))
                                .unwrap_or_else(|| "-".into()),
                            v.stats
                                .connections
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "-".into()),
                            match &v.tunnel {
                                Some(t) => format!("{} :{}", t.status.symbol(), t.local_port),
                                None => "-".into(),
                            },
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &["TARGET", "DATABASE", "ENGINE", "SIZE", "CONN", "TUNNEL"],
                        &rows
                    )
                );
            })
        }
        DbCmd::Url { database } => {
            let view = database::view(&ctx, &database).await?;
            let conn = database::connection_info(&ctx, &view)?;
            require_password(&ctx, &conn)?;
            e.value(conn.url());
            Ok(())
        }
        DbCmd::Env { database } => {
            let view = database::view(&ctx, &database).await?;
            let conn = database::connection_info(&ctx, &view)?;
            require_password(&ctx, &conn)?;
            print!("{}", conn.env_block());
            Ok(())
        }
        DbCmd::CopyUrl { database } => {
            let view = database::view(&ctx, &database).await?;
            let conn = database::connection_info(&ctx, &view)?;
            require_password(&ctx, &conn)?;
            let outcome = crate::tui::clipboard::copy(&ctx.config.ui, &conn.url(), true)?;
            e.note(outcome.message("접속 URL"));
            Ok(())
        }
        DbCmd::CopyEnv { database } => {
            let view = database::view(&ctx, &database).await?;
            let conn = database::connection_info(&ctx, &view)?;
            require_password(&ctx, &conn)?;
            let outcome = crate::tui::clipboard::copy(&ctx.config.ui, &conn.env_block(), true)?;
            e.note(outcome.message(".env 블록"));
            Ok(())
        }
        DbCmd::Test { database } => {
            let view = database::view(&ctx, &database).await?;
            database::test_connection(&ctx, &view).await?;
            e.note(format!(
                "`{}` 접속에 성공했습니다.",
                view.database.database_name
            ));
            Ok(())
        }
        DbCmd::Drop { database, plan } => {
            let view = database::view(&ctx, &database).await?;
            let preview = database::plan_drop(&ctx, &view).await?;
            if plan {
                return e.plan(&preview);
            }
            ctx.require_write_lock()?;
            e.confirm_by_name(&view.database.database_name, &preview)?;
            reported(e, |reporter, _| async move {
                database::drop(&ctx, &view, &reporter).await
            })
            .await?;
            e.note("DB를 삭제했습니다.");
            Ok(())
        }
        DbCmd::Forget { database } => {
            ctx.require_write_lock()?;
            let view = database::view(&ctx, &database).await?;
            database::forget(&ctx, &view)?;
            e.note(format!(
                "`{}` 등록을 해제했습니다. 서버의 DB는 그대로입니다.",
                view.database.database_name
            ));
            Ok(())
        }
        DbCmd::RotatePassword { database } => {
            ctx.require_write_lock()?;
            let view = database::view(&ctx, &database).await?;
            let conn = database::rotate_password(&ctx, &view).await?;
            e.data(&conn.redacted_url(), || {
                println!("비밀번호를 교체했습니다: {}", conn.redacted_url());
            })
        }
        DbCmd::Duplicate { database, new_name } => {
            ctx.require_write_lock()?;
            let view = database::view(&ctx, &database).await?;
            let created = reported(e, |reporter, _| async move {
                database::duplicate(&ctx, &view, &new_name, &reporter).await
            })
            .await?;
            e.data(&created.database, || {
                println!("`{}`(으)로 복제했습니다.", created.database.database_name);
            })
        }
    }
}

/// In restricted secret mode there is no password to print; say so instead of
/// emitting a URL that cannot connect (PRD §11.1).
fn require_password(ctx: &Ctx, conn: &crate::core::model::ConnectionInfo) -> Result<()> {
    if conn.password.is_some() {
        return Ok(());
    }
    let hint = match ctx.secrets.mode() {
        SecretMode::None => {
            "비밀번호 미저장 모드입니다. `secrets.mode`를 `keyring` 또는 `file`로 바꾸거나 \
             `linf db rotate-password`로 새 비밀번호를 발급하세요."
        }
        _ => "`linf db rotate-password`로 새 비밀번호를 발급하세요.",
    };
    Err(Error::NotFound(format!(
        "`{}`의 비밀번호를 찾을 수 없습니다. {hint}",
        conn.database
    )))
}

// ---------------------------------------------------------------------------
// bucket
// ---------------------------------------------------------------------------

async fn run_bucket(cmd: BucketCmd, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    match cmd {
        BucketCmd::Create {
            target: target_key,
            project,
            name,
            access_key,
            version,
            region,
            tunnel_port,
            plan,
        } => {
            let t = target::get(&ctx, &target_key)?;
            let espec = engine::EngineSpec::minio(&version);

            let mut spec = bucket::CreateSpec::for_project(&project);
            if let Some(n) = name {
                spec.bucket_name = n;
            }
            spec.access_key = access_key;
            spec.region = region;
            spec.preferred_local_tunnel_port = tunnel_port;

            if plan {
                let preview = bucket::plan_create(&ctx, &t, &espec, &spec).await?;
                return e.plan(&preview);
            }
            ctx.require_write_lock()?;
            let created = reported(e, |reporter, cancel| async move {
                bucket::create(&ctx, &t, &espec, &spec, &reporter, &cancel).await
            })
            .await?;

            #[derive(serde::Serialize)]
            struct CreatedOut<'a> {
                bucket: &'a crate::core::model::ManagedBucket,
                engine: &'a crate::core::model::EngineInstance,
                endpoint: String,
                url: String,
                redacted_url: String,
            }
            let payload = CreatedOut {
                bucket: &created.bucket,
                engine: &created.engine,
                endpoint: created.connection.endpoint(),
                url: created.connection.url(),
                redacted_url: created.connection.redacted_url(),
            };
            e.data(&payload, || {
                println!("버킷 `{}`을(를) 만들었습니다.", created.bucket.bucket_name);
                println!("{}", created.connection.redacted_url());
                println!(
                    "접속 정보는 `linf bucket env {}`로 확인하세요.",
                    created.bucket.bucket_name
                );
            })
        }
        BucketCmd::List => {
            let views = bucket::views(&ctx, true).await?;
            e.data(&views, || {
                let rows: Vec<Vec<String>> = views
                    .iter()
                    .map(|v| {
                        vec![
                            v.target.display_name.clone(),
                            v.bucket.bucket_name.clone(),
                            v.engine.label(),
                            v.stats
                                .size_bytes
                                .map(crate::core::util::human_bytes)
                                .unwrap_or_else(|| "-".into()),
                            v.stats
                                .objects
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "-".into()),
                            match &v.tunnel {
                                Some(t) => format!("{} :{}", t.status.symbol(), t.local_port),
                                None => "-".into(),
                            },
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &["TARGET", "BUCKET", "ENGINE", "SIZE", "OBJECTS", "TUNNEL"],
                        &rows
                    )
                );
            })
        }
        BucketCmd::Url { bucket: key } => {
            let conn = bucket_connection(&ctx, &key).await?;
            e.value(conn.url());
            Ok(())
        }
        BucketCmd::Endpoint { bucket: key } => {
            let view = bucket::view(&ctx, &key).await?;
            let conn = bucket::connection_info(&ctx, &view)?;
            e.value(conn.endpoint());
            Ok(())
        }
        BucketCmd::Env { bucket: key } => {
            let conn = bucket_connection(&ctx, &key).await?;
            print!("{}", conn.env_block());
            Ok(())
        }
        BucketCmd::CopyUrl { bucket: key } => {
            let conn = bucket_connection(&ctx, &key).await?;
            let outcome = crate::tui::clipboard::copy(&ctx.config.ui, &conn.url(), true)?;
            e.note(outcome.message("S3 접속 문자열"));
            Ok(())
        }
        BucketCmd::CopyEnv { bucket: key } => {
            let conn = bucket_connection(&ctx, &key).await?;
            let outcome = crate::tui::clipboard::copy(&ctx.config.ui, &conn.env_block(), true)?;
            e.note(outcome.message(".env 블록"));
            Ok(())
        }
        BucketCmd::Test { bucket: key } => {
            let view = bucket::view(&ctx, &key).await?;
            bucket::test_connection(&ctx, &view).await?;
            e.note(format!(
                "`{}` 접근에 성공했습니다.",
                view.bucket.bucket_name
            ));
            Ok(())
        }
        BucketCmd::Drop { bucket: key, plan } => {
            let view = bucket::view(&ctx, &key).await?;
            let preview = bucket::plan_drop(&ctx, &view).await?;
            if plan {
                return e.plan(&preview);
            }
            ctx.require_write_lock()?;
            e.confirm_by_name(&view.bucket.bucket_name, &preview)?;
            reported(e, |reporter, _| async move {
                bucket::drop(&ctx, &view, &reporter).await
            })
            .await?;
            e.note("버킷을 삭제했습니다.");
            Ok(())
        }
        BucketCmd::Forget { bucket: key } => {
            ctx.require_write_lock()?;
            let view = bucket::view(&ctx, &key).await?;
            bucket::forget(&ctx, &view)?;
            e.note(format!(
                "`{}` 등록을 해제했습니다. 서버의 버킷은 그대로입니다.",
                view.bucket.bucket_name
            ));
            Ok(())
        }
        BucketCmd::RotateKey { bucket: key } => {
            ctx.require_write_lock()?;
            let view = bucket::view(&ctx, &key).await?;
            let conn = bucket::rotate_key(&ctx, &view).await?;
            e.data(&conn.redacted_url(), || {
                println!("액세스 키를 교체했습니다: {}", conn.redacted_url());
            })
        }
    }
}

/// Resolve a bucket's connection details, refusing when the secret store has
/// no key to hand back.
async fn bucket_connection(ctx: &Ctx, key: &str) -> Result<crate::core::model::S3ConnectionInfo> {
    let view = bucket::view(ctx, key).await?;
    let conn = bucket::connection_info(ctx, &view)?;
    if conn.secret_key.is_none() {
        return Err(Error::NotFound(format!(
            "`{}`의 시크릿 키를 찾을 수 없습니다. `linf bucket rotate-key {}`로 새 키를 발급하세요.",
            conn.bucket, conn.bucket
        )));
    }
    Ok(conn)
}

/// A project resource named on the command line: a database or a bucket.
/// The name spaces are disjoint in practice, so a bare name is resolved by
/// looking in both and refusing only when it truly is ambiguous.
enum Resource {
    Database(Box<crate::core::model::DatabaseView>),
    Bucket(Box<crate::core::model::BucketView>),
}

impl Resource {
    fn tunnel_target(&self) -> tunnel::TunnelTarget {
        match self {
            Resource::Database(v) => tunnel::TunnelTarget::database(&v.database),
            Resource::Bucket(v) => tunnel::TunnelTarget::bucket(&v.bucket),
        }
    }

    fn engine(&self) -> &crate::core::model::EngineInstance {
        match self {
            Resource::Database(v) => &v.engine,
            Resource::Bucket(v) => &v.engine,
        }
    }

    fn target(&self) -> &crate::core::model::Target {
        match self {
            Resource::Database(v) => &v.target,
            Resource::Bucket(v) => &v.target,
        }
    }

    fn id(&self) -> &str {
        match self {
            Resource::Database(v) => &v.database.id,
            Resource::Bucket(v) => &v.bucket.id,
        }
    }

    fn name(&self) -> &str {
        match self {
            Resource::Database(v) => &v.database.database_name,
            Resource::Bucket(v) => &v.bucket.bucket_name,
        }
    }

    fn tunnel(&self) -> Option<&crate::core::model::TunnelSession> {
        match self {
            Resource::Database(v) => v.tunnel.as_ref(),
            Resource::Bucket(v) => v.tunnel.as_ref(),
        }
    }
}

async fn resolve_resource(ctx: &Ctx, key: &str) -> Result<Resource> {
    let as_database = ctx.store.find_database(key)?;
    let as_bucket = ctx.store.find_bucket(key)?;
    match (as_database, as_bucket) {
        (Some(_), Some(_)) => Err(Error::Conflict(format!(
            "`{key}`이라는 DB와 버킷이 모두 있습니다. id로 지정하세요."
        ))),
        (Some(db), None) => Ok(Resource::Database(Box::new(
            database::view(ctx, &db.id).await?,
        ))),
        (None, Some(b)) => Ok(Resource::Bucket(Box::new(bucket::view(ctx, &b.id).await?))),
        (None, None) => Err(Error::NotFound(format!(
            "`{key}`이라는 DB 또는 버킷을 찾을 수 없습니다."
        ))),
    }
}

// ---------------------------------------------------------------------------
// tunnel
// ---------------------------------------------------------------------------

async fn run_tunnel(cmd: TunnelCmd, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    match cmd {
        TunnelCmd::Start { database } => {
            ctx.require_write_lock()?;
            let resource = resolve_resource(&ctx, &database).await?;
            let session = tunnel::start(
                &ctx,
                &resource.tunnel_target(),
                resource.engine(),
                resource.target(),
            )
            .await?;
            e.data(&session, || {
                println!(
                    "터널 활성: {}:{} → {}:{} (pid {})",
                    session.local_host,
                    session.local_port,
                    session.remote_host,
                    session.remote_port,
                    session.pid.unwrap_or(-1)
                );
            })
        }
        TunnelCmd::Stop { database } => {
            ctx.require_write_lock()?;
            let resource = resolve_resource(&ctx, &database).await?;
            let session = ctx
                .store
                .latest_tunnel(resource.id())?
                .ok_or_else(|| Error::NotFound(format!("`{database}`의 터널 기록이 없습니다.")))?;
            tunnel::stop(&ctx, &session).await?;
            e.note("터널을 중지했습니다.");
            Ok(())
        }
        TunnelCmd::Restart { database } => {
            ctx.require_write_lock()?;
            let resource = resolve_resource(&ctx, &database).await?;
            let session = tunnel::restart(
                &ctx,
                &resource.tunnel_target(),
                resource.engine(),
                resource.target(),
            )
            .await?;
            e.data(&session, || {
                println!("터널을 재연결했습니다: :{}", session.local_port);
            })
        }
        TunnelCmd::StartAll => {
            ctx.require_write_lock()?;
            let mut pending: Vec<Resource> = Vec::new();
            for view in database::views(&ctx, false).await? {
                pending.push(Resource::Database(Box::new(view)));
            }
            for view in bucket::views(&ctx, false).await? {
                pending.push(Resource::Bucket(Box::new(view)));
            }
            pending.retain(|r| {
                r.target().is_remote()
                    && !r
                        .tunnel()
                        .is_some_and(|t| t.status == crate::core::model::TunnelStatus::Active)
            });
            if pending.is_empty() {
                e.note("시작할 터널이 없습니다.");
                return Ok(());
            }
            let mut started = Vec::new();
            let mut failed = Vec::new();
            for resource in &pending {
                match tunnel::start(
                    &ctx,
                    &resource.tunnel_target(),
                    resource.engine(),
                    resource.target(),
                )
                .await
                {
                    Ok(session) => {
                        started.push(format!("{}:{}", resource.name(), session.local_port))
                    }
                    Err(err) => {
                        failed.push(format!("{}: {}", resource.name(), err.as_diagnostic().what))
                    }
                }
            }
            e.data(&started, || {
                for line in &started {
                    println!("터널 활성: {line}");
                }
                for line in &failed {
                    println!("실패: {line}");
                }
            })?;
            if failed.is_empty() {
                Ok(())
            } else {
                Err(Error::failed(
                    format!("터널 {}건을 시작하지 못했습니다", failed.len()),
                    failed.join("\n"),
                    "`linf target test`로 SSH 연결을 확인한 뒤 다시 시도하세요.",
                ))
            }
        }

        TunnelCmd::Status => {
            // Reconcile first so a tunnel killed outside the app is not
            // reported as active (TUN-007).
            tunnel::reconcile(&ctx).await?;
            let views = tunnel::status(&ctx).await?;
            e.data(&views, || {
                let rows: Vec<Vec<String>> = views
                    .iter()
                    .map(|v| {
                        vec![
                            v.resource_name.clone(),
                            v.resource_kind.as_str().to_string(),
                            v.target_name.clone(),
                            v.session.status.as_str().to_string(),
                            format!("{}:{}", v.session.local_host, v.session.local_port),
                            format!("{}:{}", v.session.remote_host, v.session.remote_port),
                            v.session
                                .pid
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "-".into()),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &["RESOURCE", "KIND", "TARGET", "STATE", "LOCAL", "REMOTE", "PID"],
                        &rows
                    )
                );
            })
        }
    }
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

async fn run_backup(cmd: BackupCmd, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    match cmd {
        BackupCmd::Run {
            database,
            out,
            format,
        } => {
            ctx.require_write_lock()?;
            let resource = resolve_resource(&ctx, &database).await?;
            let dir = out.unwrap_or_else(|| ctx.backup_dir());
            let record = match resource {
                Resource::Database(view) => {
                    let fmt = BackupFormat::parse(&format)
                        .ok_or_else(|| Error::Usage(format!("알 수 없는 백업 형식: `{format}`")))?;
                    if fmt.resource_kind() != ResourceKind::Database {
                        return Err(Error::Usage(format!(
                            "`{format}` 형식은 DB 백업에 사용할 수 없습니다."
                        )));
                    }
                    reported(e, |reporter, cancel| async move {
                        backup::run(&ctx, &view, &dir, fmt, &reporter, &cancel).await
                    })
                    .await?
                }
                Resource::Bucket(view) => {
                    reported(e, |reporter, cancel| async move {
                        bucket::backup(&ctx, &view, &dir, &reporter, &cancel).await
                    })
                    .await?
                }
            };
            e.data(&record, || {
                println!(
                    "백업 완료: {} ({})",
                    record.path().display(),
                    crate::core::util::human_bytes(record.size)
                );
            })
        }
        BackupCmd::List { database } => {
            let id = match &database {
                Some(key) => Some(resolve_resource(&ctx, key).await?.id().to_string()),
                None => None,
            };
            let records = backup::list(&ctx, id.as_deref())?;
            e.data(&records, || {
                let rows: Vec<Vec<String>> = records
                    .iter()
                    .map(|r| {
                        vec![
                            r.id.clone(),
                            r.resource_kind.as_str().to_string(),
                            r.file_name.clone(),
                            r.format.as_str().to_string(),
                            crate::core::util::human_bytes(r.size),
                            r.status.as_str().to_string(),
                            r.created_at.to_rfc3339(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &["ID", "KIND", "FILE", "FORMAT", "SIZE", "STATUS", "CREATED"],
                        &rows
                    )
                );
            })
        }
        BackupCmd::Restore {
            file,
            into,
            overwrite,
            plan,
        } => {
            let resource = resolve_resource(&ctx, &into).await?;
            let preview = match &resource {
                Resource::Database(view) => {
                    backup::plan_restore(&ctx, &file, view, overwrite).await?
                }
                Resource::Bucket(view) => {
                    bucket::plan_restore(&ctx, &file, view, overwrite).await?
                }
            };
            if plan {
                return e.plan(&preview);
            }
            ctx.require_write_lock()?;
            let name = match &resource {
                Resource::Database(view) => view.database.database_name.clone(),
                Resource::Bucket(view) => view.bucket.bucket_name.clone(),
            };
            if overwrite {
                e.confirm_by_name(&name, &preview)?;
            } else {
                e.confirm_destructive("복원", &preview)?;
            }
            match resource {
                Resource::Database(view) => {
                    reported(e, |reporter, cancel| async move {
                        backup::restore(&ctx, &file, &view, overwrite, &reporter, &cancel).await
                    })
                    .await?
                }
                Resource::Bucket(view) => {
                    reported(e, |reporter, cancel| async move {
                        bucket::restore(&ctx, &file, &view, overwrite, &reporter, &cancel).await
                    })
                    .await?
                }
            }
            e.note("복원을 마쳤습니다.");
            Ok(())
        }
        BackupCmd::Verify { id } => {
            let record = ctx
                .store
                .find_backup(&id)?
                .ok_or_else(|| Error::NotFound(format!("백업 `{id}`을(를) 찾을 수 없습니다.")))?;
            let ok = backup::verify(&ctx, &record).await?;
            e.data(&serde_json::json!({ "id": record.id, "ok": ok }), || {
                println!(
                    "{} {}",
                    if ok { "ok  " } else { "FAIL" },
                    record.path().display()
                );
            })?;
            if !ok {
                return Err(Error::failed(
                    "백업 무결성 검증에 실패했습니다",
                    "파일의 체크섬이 기록과 다릅니다.",
                    "해당 백업을 신뢰하지 말고 다시 생성하세요.",
                ));
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// discover
// ---------------------------------------------------------------------------

async fn run_discover(key: &str, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    let t = target::get(&ctx, key)?;
    let found = discovery::foreign_containers(&ctx, &t).await?;
    e.data(&found, || {
        let rows: Vec<Vec<String>> = found
            .iter()
            .map(|c| {
                vec![
                    c.name.clone(),
                    c.image.clone(),
                    c.state.clone(),
                    c.guessed_engine.clone().unwrap_or_else(|| "-".into()),
                    c.ports.clone(),
                ]
            })
            .collect();
        print!(
            "{}",
            table(&["NAME", "IMAGE", "STATE", "ENGINE", "PORTS"], &rows)
        );
        println!("\n읽기 전용 목록입니다. local-infra는 이 리소스를 변경하지 않습니다.");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::keymap::{Action, Keymap};

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_accepts_a_password_argument() {
        fn walk(cmd: &clap::Command) {
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str();
                assert!(
                    !id.contains("password") || id == "rotate_password",
                    "`{}`의 `--{id}`: 비밀번호는 인자로 받지 않습니다 (PRD §11.2)",
                    cmd.get_name()
                );
            }
            for sub in cmd.get_subcommands() {
                walk(sub);
            }
        }
        walk(&Cli::command());
    }

    #[test]
    fn every_palette_command_has_a_matching_cli_subcommand() {
        let cli_names = Command::palette_names();
        for (name, action) in Keymap::defaults().palette_entries() {
            // Navigation and view-local actions have no headless twin.
            if matches!(
                action,
                Action::Quit
                    | Action::Help
                    | Action::Palette
                    | Action::Goto(_)
                    | Action::NextScreen
                    | Action::PrevScreen
                    | Action::FocusNext
                    | Action::FocusPrev
                    | Action::Down
                    | Action::Up
                    | Action::Top
                    | Action::Bottom
                    | Action::Open
                    | Action::Filter
                    | Action::Refresh
                    | Action::Cancel
                    | Action::Submit
                    | Action::RevealSecret
                    | Action::Add
                    | Action::Delete
                    | Action::TunnelToggle
                    | Action::Test
            ) {
                continue;
            }
            assert!(
                cli_names.contains(&name),
                "팔레트 명령 `{name}`에 대응하는 CLI 서브커맨드가 없습니다 (PRD §7.10). \
                 사용 가능한 이름: {cli_names:?}"
            );
        }
    }

    #[test]
    fn destructive_subcommands_are_reachable_only_through_confirmation() {
        // `--yes` is global, so every destructive path can be scripted, and the
        // confirmation helpers refuse without it (covered in output.rs tests).
        let cli = Cli::try_parse_from(["linf", "db", "drop", "letsbid_dev", "--yes"]).unwrap();
        assert!(cli.yes);
        let cli = Cli::try_parse_from(["linf", "db", "drop", "letsbid_dev"]).unwrap();
        assert!(!cli.yes);
    }

    /// The other direction of PRD §7.10: anything that operates TUI-managed
    /// resources is available from the command palette. Output-only tooling and
    /// Agent Skill installation are intentionally CLI-only.
    #[test]
    fn every_cli_subcommand_is_reachable_from_the_palette() {
        let palette: Vec<String> = Keymap::defaults()
            .palette_entries()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let exempt = ["completions", "skill.install"];
        let missing: Vec<String> = Command::palette_names()
            .into_iter()
            .filter(|name| !exempt.contains(&name.as_str()))
            .filter(|name| !palette.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "TUI 팔레트에서 실행할 수 없는 CLI 명령: {missing:?}"
        );
    }

    /// Every `` `linf …` `` in a user-facing message must name a real
    /// subcommand. These strings are the app telling the user what to type
    /// next, so a stale one is a broken instruction, not a typo.
    #[test]
    fn every_command_hint_in_a_message_names_a_real_subcommand() {
        let mut files = Vec::new();
        collect_rs(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(!files.is_empty(), "no sources scanned");
        let skill =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/local-infra/SKILL.md");
        assert!(
            skill.is_file(),
            "bundled local-infrastructure Agent Skill is missing"
        );
        files.push(skill);

        let root = Cli::command();
        // The exact regression this test exists for: `target add` was hinted
        // for a year while the real names are `add-local` / `add-ssh`.
        assert!(
            !hint_resolves(&root, &["target".to_string(), "add".to_string()]),
            "the guard must reject a subcommand that does not exist"
        );
        assert!(hint_resolves(
            &root,
            &["target".to_string(), "add-ssh".to_string()]
        ));

        let mut checked = 0usize;

        for path in &files {
            let text = std::fs::read_to_string(path).expect("read source");
            for hint in command_hints(&text) {
                checked += 1;
                assert!(
                    hint_resolves(&root, &hint),
                    "{}: `linf {}`은(는) 존재하지 않는 서브커맨드입니다.",
                    path.display(),
                    hint.join(" ")
                );
            }
        }
        assert!(
            checked > 10,
            "expected to find command hints, found {checked}"
        );
    }

    fn collect_rs(dir: std::path::PathBuf, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// The token sequence inside each `` `linf …` `` span, argument-free.
    fn command_hints(text: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for span in text.split("`linf").skip(1) {
            // `linf-postgres-17` is a container name, not an invocation.
            if !span.starts_with(' ') && !span.starts_with('`') {
                continue;
            }
            let Some(end) = span.find('`') else {
                continue;
            };

            let words: Vec<String> = span[..end]
                .split_whitespace()
                .take_while(|w| {
                    !w.starts_with('-')
                        && w.chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                })
                .map(str::to_string)
                .collect();
            if !words.is_empty() {
                out.push(words);
            }
        }
        out
    }

    /// Walk the clap tree. Tokens stop being subcommand names as soon as the
    /// current command only has positionals left (`linf db url <database>`).
    fn hint_resolves(root: &clap::Command, tokens: &[String]) -> bool {
        let mut current = root;
        for token in tokens {
            match current.find_subcommand(token.as_str()) {
                Some(next) => current = next,
                None => return current.get_positionals().count() > 0,
            }
        }
        true
    }

    #[test]
    fn bare_invocation_selects_the_tui() {
        let cli = Cli::try_parse_from(["linf"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn skill_install_defaults_to_the_claude_project_skill_root() {
        let cli = Cli::try_parse_from(["linf", "skill", "install"]).unwrap();
        match cli.command {
            Some(Command::Skill {
                cmd: SkillCmd::Install { dir, force },
            }) => {
                assert_eq!(dir, PathBuf::from(".claude/skills"));
                assert!(!force);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn json_and_yes_are_accepted_after_the_subcommand_too() {
        let cli = Cli::try_parse_from(["linf", "tunnel", "status", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn minio_engine_reference_defaults_to_latest() {
        let cli = Cli::try_parse_from(["linf", "engine", "ensure", "local", "minio"]).unwrap();
        match cli.command {
            Some(Command::Engine {
                cmd: EngineCmd::Ensure { r#ref, .. },
            }) => {
                assert_eq!(r#ref.kind().unwrap(), EngineKind::Minio);
                assert_eq!(r#ref.version().unwrap(), "latest");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bucket_and_database_commands_are_separate_surfaces() {
        assert!(Cli::try_parse_from([
            "linf",
            "bucket",
            "create",
            "--target",
            "local",
            "--project",
            "P"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["linf", "bucket", "env", "letsbid-dev"]).is_ok());
        assert!(Cli::try_parse_from(["linf", "bucket", "rotate-key", "letsbid-dev"]).is_ok());
    }

    #[test]
    fn engine_reference_defaults_to_postgres_17() {
        let cli = Cli::try_parse_from(["linf", "engine", "ensure", "local"]).unwrap();
        match cli.command {
            Some(Command::Engine {
                cmd: EngineCmd::Ensure { r#ref, .. },
            }) => {
                assert_eq!(r#ref.target, "local");
                assert_eq!(r#ref.engine, "postgres");
                assert_eq!(r#ref.version, None, "the engine decides its own default");
                assert_eq!(r#ref.version().unwrap(), "17");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
