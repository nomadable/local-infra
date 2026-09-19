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
mod update;
use crate::core::config::SecretMode;
use crate::core::error::{Error, Result};
use crate::core::model::{AuthType, BackupFormat, EngineKind, Origin, ResourceKind};
use crate::core::progress::{Cancel, Reporter};
use crate::core::{
    backup, bucket, database, discovery, doctor, engine, sql, ssh, target, tunnel, Ctx,
};
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
    /// PostgreSQL SQL 작업공간과 외부 connection을 관리합니다.
    Sql {
        #[command(subcommand)]
        cmd: SqlCmd,
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
    /// 현재 설치 경로에 최신 GitHub Release를 설치합니다.
    Update,
    /// Agent Skills 호환 프로젝트 또는 전역 skill 폴더에 설치합니다.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum SqlOutputFormat {
    Table,
    Csv,
    Json,
    Jsonl,
}

#[derive(Debug, Subcommand)]
pub enum SqlCmd {
    /// DB 또는 외부 connection을 SQL 작업공간에서 엽니다.
    Open {
        source: String,
        /// 외부 connection을 이 작업공간에서만 writable로 엽니다.
        #[arg(long)]
        writable: bool,
        /// writable 승인을 위해 다시 입력한 connection 이름.
        #[arg(long, requires = "writable")]
        confirm_profile: Option<String>,
    },
    /// SQL을 headless로 실행합니다.
    Exec {
        source: String,
        #[arg(long, conflicts_with = "file")]
        command: Option<String>,
        /// SQL 파일. `-`이면 stdin에서 읽습니다.
        #[arg(long, conflicts_with = "command")]
        file: Option<String>,
        #[arg(long, value_enum, default_value = "table")]
        output: SqlOutputFormat,
        #[arg(long)]
        writable: bool,
        #[arg(long, requires = "writable")]
        confirm_profile: Option<String>,
    },
    /// schema, relation, column, key catalog를 출력합니다.
    Catalog { source: String },
    /// 최근 query history metadata를 출력합니다.
    History {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// 외부 PostgreSQL connection profile을 관리합니다.
    Connection {
        #[command(subcommand)]
        cmd: SqlConnectionCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum SqlConnectionCmd {
    /// connection을 검사한 뒤 저장합니다. secret은 argv로 받지 않습니다.
    Add {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 5432)]
        port: u16,
        #[arg(long)]
        database: String,
        #[arg(long)]
        user: String,
        #[arg(long, value_enum, default_value = "verify-full")]
        tls: sql::TlsMode,
        #[arg(long, value_enum, default_value = "read-only")]
        access: sql::AccessMode,
        #[arg(long)]
        root_ca: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        connect_timeout: u64,
        #[arg(long, default_value_t = 30)]
        query_timeout: u64,
        #[arg(long, default_value = "linf-sql")]
        application_name: String,
        #[arg(long, default_value_t = 5_000)]
        preview_rows: usize,
        #[arg(long)]
        history_text: bool,
        /// secret을 stdin 첫 줄에서 읽습니다.
        #[arg(long)]
        secret_stdin: bool,
        /// 입력한 secret을 암호화 vault에 저장합니다. 기본값은 미저장입니다.
        #[arg(long)]
        store_secret: bool,
    },
    /// 기존 connection 설정을 수정하고 다시 검사합니다.
    Edit {
        name: String,
        #[arg(long)]
        rename: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long, value_enum)]
        tls: Option<sql::TlsMode>,
        #[arg(long, value_enum)]
        access: Option<sql::AccessMode>,
        #[arg(long)]
        root_ca: Option<PathBuf>,
        #[arg(long)]
        clear_root_ca: bool,
        #[arg(long)]
        connect_timeout: Option<u64>,
        #[arg(long)]
        query_timeout: Option<u64>,
        #[arg(long)]
        application_name: Option<String>,
        #[arg(long)]
        preview_rows: Option<usize>,
        #[arg(long)]
        history_text: Option<bool>,
        #[arg(long, conflicts_with = "remove_secret")]
        replace_secret_stdin: bool,
        #[arg(long, conflicts_with = "replace_secret_stdin")]
        remove_secret: bool,
    },
    /// 저장된 connection을 나열합니다.
    List,
    /// 실제 TLS와 인증으로 connection을 검사합니다.
    Test {
        name: String,
        /// 저장된 secret 대신 stdin 첫 줄을 사용합니다.
        #[arg(long)]
        secret_stdin: bool,
    },
    /// profile과 저장된 secret을 삭제합니다. 원격 DB는 변경하지 않습니다.
    Forget { name: String },
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
        /// Agent Skills 호환 skill 루트. 기본값은 프로젝트의 `.agents/skills`입니다.
        #[arg(long, value_name = "DIR", conflicts_with_all = ["agent", "global"])]
        dir: Option<PathBuf>,
        /// 특정 코딩 에이전트의 기본 skill 경로에 설치합니다 (claude, codex, cursor, gemini, copilot).
        #[arg(long, value_enum, value_name = "AGENT")]
        agent: Option<agent_skill::Agent>,
        /// 사용자 전역(`~/.agents/skills` 또는 `--agent`의 전역 경로)에 설치합니다.
        #[arg(short = 'g', long)]
        global: bool,
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

    let command = match command {
        Command::Sql {
            cmd:
                SqlCmd::Open {
                    source,
                    writable,
                    confirm_profile,
                },
        } => {
            return match crate::tui::sql::run_open(source, writable, confirm_profile) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => ExitCode::from(report(&error, format) as u8),
            };
        }
        other => other,
    };

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
        Command::Sql { cmd } => run_sql(cmd, e).await,
        Command::Bucket { cmd } => run_bucket(cmd, e).await,
        Command::Tunnel { cmd } => run_tunnel(cmd, e).await,
        Command::Backup { cmd } => run_backup(cmd, e).await,
        Command::Reset => run_reset(e).await,
        Command::Update => run_update(e).await,
        Command::Skill { cmd } => run_skill(cmd, e).await,
        Command::Discover { target } => run_discover(&target, e).await,
    }
}

async fn run_update(e: Emitter) -> Result<()> {
    let (receipt, installer_output) = update::run().await?;
    e.data(&receipt, || {
        if receipt.updated {
            if !installer_output.is_empty() {
                println!("{installer_output}");
            }
            println!("\n업데이트 확인: linf {}", receipt.latest_version);
        } else {
            println!(
                "업데이트가 필요 없습니다. 현재 linf {} · 최신 linf {}",
                receipt.current_version, receipt.latest_version
            );
        }
    })
}

async fn run_skill(cmd: SkillCmd, e: Emitter) -> Result<()> {
    match cmd {
        SkillCmd::Install {
            dir,
            agent,
            global,
            force,
        } => {
            let dir = agent_skill::resolve_dir(dir, agent, global)?;
            let receipt = agent_skill::install(&dir, force)?;
            e.data(&receipt, || {
                println!("Agent Skill을 `{}`에 설치했습니다.", receipt.path);
                for file in receipt.files.iter().skip(1) {
                    println!("  + {file}");
                }
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

// ---------------------------------------------------------------------------
// sql
// ---------------------------------------------------------------------------

async fn run_sql(cmd: SqlCmd, e: Emitter) -> Result<()> {
    let ctx = context(&e)?;
    match cmd {
        SqlCmd::Open { .. } => Err(Error::Usage(
            "`linf sql open`은 terminal 진입점에서 처리되어야 합니다.".into(),
        )),
        SqlCmd::Connection { cmd } => run_sql_connection(&ctx, cmd, e).await,
        SqlCmd::Exec {
            source,
            command,
            file,
            output,
            writable,
            confirm_profile,
        } => {
            let sql_text = read_sql_input(command, file)?;
            let mut resolved = sql::connection::resolve(
                &ctx,
                &source,
                sql::connection::ResolveOptions {
                    writable,
                    confirmation: confirm_profile.as_deref(),
                    password: None,
                    start_tunnel: true,
                },
            )
            .await?;
            prompt_for_external_secret(&mut resolved, &e)?;
            if let Some(warning) = resolved.endpoint.tls_mode.warning() {
                e.warn(warning);
            }
            let mut session = sql::connection::connect(resolved).await?;
            let actual_output = if e.is_json() && output == SqlOutputFormat::Table {
                SqlOutputFormat::Json
            } else {
                output
            };
            let limits = if actual_output == SqlOutputFormat::Table {
                sql::runner::QueryLimits::preview(session.endpoint.preview_rows)
            } else {
                sql::runner::QueryLimits::streaming_export()
            };
            let request = sql::runner::ExecuteRequest {
                sql: sql_text,
                base_offset: 0,
                limits,
            };
            let cancel = Cancel::new();
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let run = sql::runner::execute(&ctx, &mut session, request, &cancel, &tx);
            let render = render_sql_events(rx, actual_output);
            let (summary, ()) = tokio::try_join!(run, render)?;
            if summary.success {
                Ok(())
            } else {
                Err(Error::failed(
                    "SQL 실행에 실패했습니다",
                    summary
                        .error
                        .map(|error| error.message)
                        .unwrap_or_else(|| "PostgreSQL이 statement를 완료하지 못했습니다.".into()),
                    "표시된 SQLSTATE와 위치를 확인한 뒤 query를 수정하세요.",
                ))
            }
        }
        SqlCmd::Catalog { source } => {
            let mut resolved =
                sql::connection::resolve(&ctx, &source, sql::connection::ResolveOptions::default())
                    .await?;
            prompt_for_external_secret(&mut resolved, &e)?;
            let session = sql::connection::connect(resolved).await?;
            let catalog = sql::catalog::refresh(&session).await?;
            e.data(&catalog, || {
                let rows: Vec<Vec<String>> = catalog
                    .relations
                    .iter()
                    .flat_map(|relation| {
                        if relation.columns.is_empty() {
                            vec![vec![
                                relation.schema.clone(),
                                relation.name.clone(),
                                format!("{:?}", relation.kind),
                                "-".into(),
                                "-".into(),
                                "-".into(),
                            ]]
                        } else {
                            relation
                                .columns
                                .iter()
                                .map(|column| {
                                    vec![
                                        relation.schema.clone(),
                                        relation.name.clone(),
                                        format!("{:?}", relation.kind),
                                        column.name.clone(),
                                        column.data_type.clone(),
                                        if column.nullable { "YES" } else { "NO" }.into(),
                                    ]
                                })
                                .collect()
                        }
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &["SCHEMA", "RELATION", "KIND", "COLUMN", "TYPE", "NULL"],
                        &rows
                    )
                );
            })
        }
        SqlCmd::History { limit } => {
            let entries = sql::history::list(&ctx, limit)?;
            e.data(&entries, || {
                let rows: Vec<Vec<String>> = entries
                    .iter()
                    .map(|entry| {
                        vec![
                            entry.executed_at.to_rfc3339(),
                            entry.profile_label.clone(),
                            if entry.success { "ok" } else { "failed" }.into(),
                            format!("{} ms", entry.elapsed_ms),
                            entry.row_count.to_string(),
                            entry
                                .query_text
                                .as_deref()
                                .map(one_line)
                                .unwrap_or_else(|| "(text disabled)".into()),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &[
                            "EXECUTED",
                            "CONNECTION",
                            "STATUS",
                            "ELAPSED",
                            "ROWS",
                            "QUERY"
                        ],
                        &rows
                    )
                );
            })
        }
    }
}

async fn run_sql_connection(ctx: &Ctx, cmd: SqlConnectionCmd, e: Emitter) -> Result<()> {
    match cmd {
        SqlConnectionCmd::Add {
            name,
            host,
            port,
            database,
            user,
            tls,
            access,
            root_ca,
            connect_timeout,
            query_timeout,
            application_name,
            preview_rows,
            history_text,
            secret_stdin,
            store_secret,
        } => {
            if let Some(warning) = tls.warning() {
                e.warn(warning);
            }
            let secret = read_sql_secret(secret_stdin, e.interactive)?;
            if store_secret && secret.is_none() {
                return Err(Error::Usage(
                    "`--store-secret`을 사용하려면 비어 있지 않은 secret을 입력하세요.".into(),
                ));
            }
            let spec = sql::ProfileSpec {
                name,
                host,
                port,
                database,
                username: user,
                tls_mode: tls,
                access_mode: access,
                root_ca_path: root_ca,
                connect_timeout_ms: connect_timeout.saturating_mul(1_000),
                query_timeout_ms: query_timeout.saturating_mul(1_000),
                application_name,
                history_text,
                preview_rows,
            };
            let resolved = sql::connection::resolve_external_spec(&spec, secret.clone())?;
            sql::connection::test(resolved).await?;
            let saved = sql::profile::create(ctx, spec, secret.as_deref(), store_secret)?;
            e.data(&saved, || {
                println!(
                    "SQL connection `{}`을(를) 검사하고 저장했습니다: {}",
                    saved.name,
                    saved.endpoint_label()
                );
                if saved.credential_ref.is_none() {
                    println!("secret은 저장하지 않았습니다.");
                }
            })
        }
        SqlConnectionCmd::Edit {
            name,
            rename,
            host,
            port,
            database,
            user,
            tls,
            access,
            root_ca,
            clear_root_ca,
            connect_timeout,
            query_timeout,
            application_name,
            preview_rows,
            history_text,
            replace_secret_stdin,
            remove_secret,
        } => {
            let existing = sql::profile::require(ctx, &name)?;
            let replacement = if replace_secret_stdin {
                read_sql_secret(true, e.interactive)?
            } else {
                None
            };
            if replace_secret_stdin && replacement.is_none() {
                return Err(Error::Usage("교체할 secret이 비어 있습니다.".into()));
            }
            let test_secret = if remove_secret {
                None
            } else if replacement.is_some() {
                replacement.clone()
            } else {
                match &existing.credential_ref {
                    Some(reference) => ctx.secrets.get(reference)?,
                    None => None,
                }
            };
            let spec = sql::ProfileSpec {
                name: rename.unwrap_or_else(|| existing.name.clone()),
                host: host.unwrap_or_else(|| existing.host.clone()),
                port: port.unwrap_or(existing.port),
                database: database.unwrap_or_else(|| existing.database.clone()),
                username: user.unwrap_or_else(|| existing.username.clone()),
                tls_mode: tls.unwrap_or(existing.tls_mode),
                access_mode: access.unwrap_or(existing.access_mode),
                root_ca_path: if clear_root_ca {
                    None
                } else {
                    root_ca.or(existing.root_ca_path.clone())
                },
                connect_timeout_ms: connect_timeout
                    .map(|seconds| seconds.saturating_mul(1_000))
                    .unwrap_or(existing.connect_timeout_ms),
                query_timeout_ms: query_timeout
                    .map(|seconds| seconds.saturating_mul(1_000))
                    .unwrap_or(existing.query_timeout_ms),
                application_name: application_name
                    .unwrap_or_else(|| existing.application_name.clone()),
                history_text: history_text.unwrap_or(existing.history_text),
                preview_rows: preview_rows.unwrap_or(existing.preview_rows),
            };
            if let Some(warning) = spec.tls_mode.warning() {
                e.warn(warning);
            }
            sql::connection::test(sql::connection::resolve_external_spec(&spec, test_secret)?)
                .await?;
            let password = if remove_secret {
                sql::PasswordUpdate::Remove
            } else if let Some(secret) = replacement.as_deref() {
                sql::PasswordUpdate::Replace(secret)
            } else {
                sql::PasswordUpdate::Keep
            };
            let saved = sql::profile::update(ctx, &existing.id, spec, password)?;
            e.data(&saved, || {
                println!("SQL connection `{}`을(를) 수정했습니다.", saved.name);
            })
        }
        SqlConnectionCmd::List => {
            let profiles = sql::profile::list(ctx)?;
            e.data(&profiles, || {
                let rows: Vec<Vec<String>> = profiles
                    .iter()
                    .map(|profile| {
                        vec![
                            profile.name.clone(),
                            profile.endpoint_label(),
                            profile.tls_mode.as_str().into(),
                            profile.access_mode.label().into(),
                            if profile.credential_ref.is_some() {
                                "stored"
                            } else {
                                "prompt"
                            }
                            .into(),
                            if profile.history_text { "on" } else { "off" }.into(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table(
                        &["NAME", "ENDPOINT", "TLS", "ACCESS", "SECRET", "HISTORY"],
                        &rows
                    )
                );
            })
        }
        SqlConnectionCmd::Test { name, secret_stdin } => {
            let override_secret = if secret_stdin {
                read_sql_secret(true, e.interactive)?
            } else {
                None
            };
            let mut resolved = sql::connection::resolve(
                ctx,
                &name,
                sql::connection::ResolveOptions {
                    password: override_secret,
                    start_tunnel: false,
                    ..Default::default()
                },
            )
            .await?;
            prompt_for_external_secret(&mut resolved, &e)?;
            let label = resolved.endpoint.label.clone();
            sql::connection::test(resolved).await?;
            e.note(format!("SQL connection `{label}` 접속에 성공했습니다."));
            Ok(())
        }
        SqlConnectionCmd::Forget { name } => {
            let forgotten = sql::profile::forget(ctx, &name)?;
            e.data(&forgotten, || {
                println!(
                    "SQL connection `{}`과 저장된 secret을 삭제했습니다. 원격 DB는 변경하지 않았습니다.",
                    forgotten.name
                );
            })
        }
    }
}

fn read_sql_input(command: Option<String>, file: Option<String>) -> Result<String> {
    match (command, file) {
        (Some(command), None) if !command.trim().is_empty() => Ok(command),
        (None, Some(file)) if file == "-" => {
            use std::io::Read;
            let mut sql = String::new();
            std::io::stdin().read_to_string(&mut sql)?;
            Ok(sql)
        }
        (None, Some(file)) => Ok(std::fs::read_to_string(file)?),
        _ => Err(Error::Usage(
            "`sql exec`에는 `--command <sql>` 또는 `--file <path|->` 중 하나가 필요합니다.".into(),
        )),
    }
}

fn read_sql_secret(from_stdin: bool, interactive: bool) -> Result<Option<String>> {
    let value = if from_stdin {
        let mut secret = String::new();
        std::io::stdin().read_line(&mut secret)?;
        secret.trim_end_matches(['\r', '\n']).to_string()
    } else if let Ok(secret) = std::env::var("LINF_SQL_PASSWORD") {
        secret
    } else if interactive {
        rpassword::prompt_password("PostgreSQL password (empty for none): ").map_err(Error::from)?
    } else {
        return Ok(None);
    };
    Ok((!value.is_empty()).then_some(value))
}

fn prompt_for_external_secret(
    resolved: &mut sql::ResolvedSqlConnection,
    e: &Emitter,
) -> Result<()> {
    if resolved.source.external() && resolved.password().is_none() {
        resolved.set_password(read_sql_secret(false, e.interactive)?);
    }
    Ok(())
}

async fn render_sql_events(
    mut events: tokio::sync::mpsc::Receiver<sql::runner::QueryEvent>,
    format: SqlOutputFormat,
) -> Result<()> {
    if format != SqlOutputFormat::Table {
        let export_format = match format {
            SqlOutputFormat::Csv => sql::export::ExportFormat::Csv,
            SqlOutputFormat::Json => sql::export::ExportFormat::Json,
            SqlOutputFormat::Jsonl => sql::export::ExportFormat::Jsonl,
            SqlOutputFormat::Table => unreachable!(),
        };
        sql::export::write_events(events, std::io::stdout(), export_format).await?;
        return Ok(());
    }
    let mut columns = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            sql::runner::QueryEvent::ResultStarted { columns: next, .. } => {
                columns = next;
                println!("{}", columns.join(" │ "));
                println!(
                    "{}",
                    columns
                        .iter()
                        .map(|column| "─".repeat(crate::core::util::display_cols(column).max(1)))
                        .collect::<Vec<_>>()
                        .join("─┼─")
                );
            }
            sql::runner::QueryEvent::Rows { rows, .. } => {
                for row in rows {
                    println!(
                        "{}",
                        row.iter()
                            .map(|cell| cell.as_deref().unwrap_or("NULL"))
                            .collect::<Vec<_>>()
                            .join(" │ ")
                    );
                }
            }
            sql::runner::QueryEvent::StatementComplete {
                affected_rows,
                elapsed_ms,
                ..
            } if columns.is_empty() => {
                eprintln!("{affected_rows} rows · {elapsed_ms} ms");
            }
            sql::runner::QueryEvent::Truncated { rows, bytes, .. } => {
                eprintln!("preview truncated at {rows} rows / {bytes} bytes");
            }
            sql::runner::QueryEvent::Finished(_) => break,
            sql::runner::QueryEvent::StatementStarted { .. }
            | sql::runner::QueryEvent::StatementComplete { .. }
            | sql::runner::QueryEvent::Error(_) => {}
        }
    }
    Ok(())
}

fn one_line(sql: &str) -> String {
    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 80 {
        compact
    } else {
        format!("{}…", compact.chars().take(79).collect::<String>())
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
    /// resources is available from the command palette. Output-only tooling,
    /// self-update, and Agent Skill installation are intentionally CLI-only.
    #[test]
    fn every_cli_subcommand_is_reachable_from_the_palette() {
        let palette: Vec<String> = Keymap::defaults()
            .palette_entries()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let exempt = ["completions", "skill.install", "update"];
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
        let skill_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/local-infra");
        for relative in [
            "SKILL.md",
            "references/commands.md",
            "references/workflows.md",
        ] {
            let file = skill_dir.join(relative);
            assert!(
                file.is_file(),
                "bundled local-infrastructure Agent Skill file {relative} is missing"
            );
            files.push(file);
        }

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

    /// The token sequence of each `linf …` invocation, argument-free: inline
    /// `` `linf …` `` spans plus lines that start with `linf ` (fenced recipe
    /// blocks in the skill files, which agents copy verbatim).
    fn command_hints(text: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        let mut push = |invocation: &str| {
            let words: Vec<String> = invocation
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
        };
        for span in text.split("`linf").skip(1) {
            // `linf-postgres-17` is a container name, not an invocation.
            if !span.starts_with(' ') && !span.starts_with('`') {
                continue;
            }
            let Some(end) = span.find('`') else {
                continue;
            };
            push(&span[..end]);
        }
        for line in text.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("linf ") {
                push(rest);
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
    fn update_is_a_headless_command() {
        let cli = Cli::try_parse_from(["linf", "update"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update)));
    }

    #[test]
    fn skill_install_defaults_to_the_portable_project_skill_root() {
        let cli = Cli::try_parse_from(["linf", "skill", "install"]).unwrap();
        match cli.command {
            Some(Command::Skill {
                cmd:
                    SkillCmd::Install {
                        dir,
                        agent,
                        global,
                        force,
                    },
            }) => {
                assert_eq!(dir, None);
                assert_eq!(agent, None);
                assert!(!global);
                assert!(!force);
                assert_eq!(
                    agent_skill::resolve_dir(dir, agent, global).unwrap(),
                    PathBuf::from(".agents/skills")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn skill_install_accepts_an_agent_preset_and_rejects_it_with_dir() {
        let cli = Cli::try_parse_from(["linf", "skill", "install", "--agent", "claude"]).unwrap();
        match cli.command {
            Some(Command::Skill {
                cmd:
                    SkillCmd::Install {
                        dir, agent, global, ..
                    },
            }) => {
                assert_eq!(agent, Some(agent_skill::Agent::Claude));
                assert_eq!(
                    agent_skill::resolve_dir(dir, agent, global).unwrap(),
                    PathBuf::from(".claude/skills")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from([
            "linf", "skill", "install", "--agent", "claude", "--dir", "x"
        ])
        .is_err());
        assert!(Cli::try_parse_from(["linf", "skill", "install", "--agent", "unknown"]).is_err());
    }

    #[test]
    fn skill_install_accepts_the_portable_global_scope() {
        let cli = Cli::try_parse_from(["linf", "skill", "install", "-g"]).unwrap();
        match cli.command {
            Some(Command::Skill {
                cmd: SkillCmd::Install { dir, global, .. },
            }) => {
                assert_eq!(dir, None);
                assert!(global);
            }
            other => panic!("unexpected {other:?}"),
        }
        // `--dir` names one exact root; combining it with a scope is a usage
        // error at parse time and, for callers that build the arguments by
        // hand, in `resolve_dir` too.
        assert!(Cli::try_parse_from([
            "linf",
            "skill",
            "install",
            "--dir",
            ".claude/skills",
            "--global",
        ])
        .is_err());
        assert!(matches!(
            agent_skill::resolve_dir(Some(PathBuf::from(".claude/skills")), None, true),
            Err(Error::Usage(_))
        ));
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
