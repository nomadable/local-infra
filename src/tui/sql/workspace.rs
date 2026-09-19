//! Independent SQL workspace event loop.

use super::catalog::CatalogState;
use super::editor::Buffers;
use super::render;
use super::results::Results;
use crate::core::error::{Error, Result};
use crate::core::model::Origin;
use crate::core::progress::Cancel;
use crate::core::sql::{self, ResolvedSqlConnection, SqlConnectionSource, SqlEndpoint};
use crate::core::Ctx;
use crate::tui::terminal::{TerminalGuard, Tui};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

const TICK_MS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Catalog,
    Editor,
    Results,
}

impl Pane {
    fn next(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::Catalog, false) => Self::Editor,
            (Self::Editor, false) => Self::Results,
            (Self::Results, false) => Self::Catalog,
            (Self::Catalog, true) => Self::Results,
            (Self::Results, true) => Self::Editor,
            (Self::Editor, true) => Self::Catalog,
        }
    }
}

pub(crate) enum Overlay {
    Search(String),
    Goto(String),
    Export(String),
    Password(String),
    Message { title: String, body: String },
    History { cursor: usize },
    ConfirmCloseBuffer,
    ProfileTestPassword { cursor: usize, input: String },
    ConfirmExitTransaction,
    Connections { cursor: usize },
    ProfileForm(ProfileForm),
    ConfirmForgetProfile { cursor: usize },
    ConfirmReconnect,
}

#[derive(Clone)]
pub(crate) struct ProfileForm {
    pub editing: Option<String>,
    pub name: String,
    pub host: String,
    pub port: String,
    pub database: String,
    pub username: String,
    pub tls_mode: sql::TlsMode,
    pub access_mode: sql::AccessMode,
    pub root_ca_path: String,
    pub store_password: bool,
    pub password: String,
    pub history_text: bool,
    pub preview_rows: String,
    pub focus: usize,
    pub existing_secret: bool,
}

impl ProfileForm {
    fn new() -> Self {
        Self {
            editing: None,
            name: String::new(),
            host: String::new(),
            port: "5432".into(),
            database: String::new(),
            username: String::new(),
            tls_mode: sql::TlsMode::VerifyFull,
            access_mode: sql::AccessMode::ReadOnly,
            root_ca_path: String::new(),
            store_password: false,
            password: String::new(),
            history_text: false,
            preview_rows: "5000".into(),
            focus: 0,
            existing_secret: false,
        }
    }

    fn edit(profile: &sql::SqlConnectionProfile) -> Self {
        Self {
            editing: Some(profile.id.clone()),
            name: profile.name.clone(),
            host: profile.host.clone(),
            port: profile.port.to_string(),
            database: profile.database.clone(),
            username: profile.username.clone(),
            tls_mode: profile.tls_mode,
            access_mode: profile.access_mode,
            root_ca_path: profile
                .root_ca_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            store_password: profile.credential_ref.is_some(),
            password: String::new(),
            history_text: profile.history_text,
            preview_rows: profile.preview_rows.to_string(),
            focus: 0,
            existing_secret: profile.credential_ref.is_some(),
        }
    }

    fn spec(&self) -> Result<sql::ProfileSpec> {
        let port = self
            .port
            .parse::<u16>()
            .map_err(|_| Error::Usage("SQL port는 1부터 65535 사이여야 합니다.".into()))?;
        let preview_rows = self
            .preview_rows
            .parse::<usize>()
            .map_err(|_| Error::Usage("preview row limit은 1 이상의 정수여야 합니다.".into()))?;
        Ok(sql::ProfileSpec {
            name: self.name.clone(),
            host: self.host.clone(),
            port,
            database: self.database.clone(),
            username: self.username.clone(),
            tls_mode: self.tls_mode,
            access_mode: self.access_mode,
            root_ca_path: (!self.root_ca_path.trim().is_empty())
                .then(|| PathBuf::from(self.root_ca_path.trim())),
            connect_timeout_ms: 10_000,
            query_timeout_ms: 30_000,
            application_name: "linf-sql".into(),
            history_text: self.history_text,
            preview_rows,
        })
    }

    pub(crate) fn value_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            0 => Some(&mut self.name),
            1 => Some(&mut self.host),
            2 => Some(&mut self.port),
            3 => Some(&mut self.database),
            4 => Some(&mut self.username),
            7 => Some(&mut self.root_ca_path),
            9 => Some(&mut self.password),
            11 => Some(&mut self.preview_rows),
            _ => None,
        }
    }
}

pub struct Workspace {
    pub(crate) ctx: Arc<Ctx>,
    pub(crate) source_key: String,
    writable: bool,
    confirmation: Option<String>,
    password: Option<String>,
    pending_resolved: Option<ResolvedSqlConnection>,
    session: Arc<Mutex<Option<sql::connection::SqlSession>>>,
    events: mpsc::UnboundedSender<WorkspaceEvent>,
    pub(crate) endpoint: Option<SqlEndpoint>,
    pub(crate) source: Option<SqlConnectionSource>,
    pub(crate) connected: bool,
    pub(crate) generation: u64,
    pub(crate) transaction: String,
    pub(crate) focus: Pane,
    pub(crate) fullscreen: Option<Pane>,
    pub(crate) buffers: Buffers,
    pub(crate) catalog: CatalogState,
    pub(crate) results: Results,
    pub(crate) history: Vec<sql::QueryHistoryEntry>,
    pub(crate) profiles: Vec<sql::SqlConnectionProfile>,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) completions: Vec<sql::catalog::CompletionItem>,
    pub(crate) completion_index: usize,
    pub(crate) editor_scroll: usize,
    pub(crate) editor_horizontal: usize,
    pub(crate) status: String,
    pub(crate) tick: usize,
    current_cancel: Option<Cancel>,
    last_sql: Option<String>,
    quit: bool,
}

enum WorkspaceEvent {
    NeedPassword(ResolvedSqlConnection),
    Connected {
        endpoint: SqlEndpoint,
        source: SqlConnectionSource,
        generation: u64,
    },
    Catalog(Result<sql::catalog::Catalog>),
    Query(sql::runner::QueryEvent),
    Exported(Result<sql::export::ExportReceipt>),
    RolledBack(Result<()>),
    ProfileSaved(Result<sql::SqlConnectionProfile>),
    ProfileTested(Result<()>),
    ProfileForgotten(Result<sql::SqlConnectionProfile>),
    Failed(Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceStart {
    Editor,
    Catalog,
    History,
    Connections,
    NewConnection,
}

pub fn run_open(source: String, writable: bool, confirmation: Option<String>) -> Result<()> {
    let ctx = Arc::new(Ctx::open(Origin::Tui)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    crate::tui::terminal::install_panic_hook();
    let mut guard = TerminalGuard::enter(true)?;
    runtime.block_on(run_in_terminal(
        ctx,
        &mut guard.terminal,
        source,
        writable,
        confirmation,
    ))
}

pub async fn run_in_terminal(
    ctx: Arc<Ctx>,
    terminal: &mut Tui,
    source: String,
    writable: bool,
    confirmation: Option<String>,
) -> Result<()> {
    run_in_terminal_with_start(
        ctx,
        terminal,
        source,
        writable,
        confirmation,
        WorkspaceStart::Editor,
    )
    .await
}

pub(crate) async fn run_in_terminal_with_start(
    ctx: Arc<Ctx>,
    terminal: &mut Tui,
    source: String,
    writable: bool,
    confirmation: Option<String>,
    start: WorkspaceStart,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut workspace = Workspace::new(ctx, source, writable, confirmation, tx)?;
    match start {
        WorkspaceStart::Editor => {}
        WorkspaceStart::Catalog => workspace.focus = Pane::Catalog,
        WorkspaceStart::History => {
            workspace.overlay = Some(Overlay::History { cursor: 0 });
        }
        WorkspaceStart::Connections => workspace.open_connections(),
        WorkspaceStart::NewConnection => {
            workspace.overlay = Some(Overlay::ProfileForm(ProfileForm::new()));
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (mut terminal_events, reader) = spawn_reader(stop.clone());
    let mut ticker = tokio::time::interval(Duration::from_millis(TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    if !workspace.source_key.is_empty() {
        workspace.connect(None);
    } else {
        workspace.status = "SQL connection을 추가하거나 선택하세요".into();
    }

    let outcome = loop {
        if let Err(error) = terminal.draw(|frame| render::draw(frame, &workspace)) {
            break Err(error.into());
        }
        if workspace.quit {
            break Ok(());
        }
        tokio::select! {
            event = terminal_events.recv() => match event {
                Some(event) => workspace.on_terminal_event(event),
                None => break Ok(()),
            },
            Some(event) = rx.recv() => workspace.on_workspace_event(event),
            _ = ticker.tick() => workspace.on_tick(),
        }
    };
    stop.store(true, Ordering::SeqCst);
    let _ = reader.join();
    if outcome.is_ok() {
        workspace.buffers.cleanup()?;
    } else {
        let _ = workspace.buffers.persist();
    }
    outcome
}

impl Workspace {
    fn new(
        ctx: Arc<Ctx>,
        source_key: String,
        writable: bool,
        confirmation: Option<String>,
        events: mpsc::UnboundedSender<WorkspaceEvent>,
    ) -> Result<Self> {
        let buffers = Buffers::load(&ctx.paths.state_dir)?;
        let recovered = buffers.recovered_count();
        Ok(Self {
            history: sql::history::list(&ctx, 100).unwrap_or_default(),
            profiles: sql::profile::list(&ctx).unwrap_or_default(),
            status: if recovered > 0 {
                format!("crash recovery buffer {recovered}개를 열었습니다")
            } else {
                "connection 준비 중 · editor는 바로 입력할 수 있습니다".into()
            },
            ctx,
            source_key,
            writable,
            confirmation,
            password: None,
            pending_resolved: None,
            session: Arc::new(Mutex::new(None)),
            events,
            endpoint: None,
            source: None,
            connected: false,
            generation: 0,
            transaction: "idle".into(),
            focus: Pane::Editor,
            fullscreen: None,
            buffers,
            catalog: CatalogState::default(),
            results: Results::default(),
            overlay: None,
            completions: Vec::new(),
            completion_index: 0,
            editor_scroll: 0,
            editor_horizontal: 0,
            tick: 0,
            current_cancel: None,
            last_sql: None,
            quit: false,
        })
    }

    fn connect(&mut self, password: Option<String>) {
        self.connected = false;
        self.status = "connecting…".into();
        let ctx = self.ctx.clone();
        let source = self.source_key.clone();
        let writable = self.writable;
        let confirmation = self.confirmation.clone();
        let session = self.session.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let resolved = sql::connection::resolve(
                &ctx,
                &source,
                sql::connection::ResolveOptions {
                    writable,
                    confirmation: confirmation.as_deref(),
                    password,
                    start_tunnel: true,
                },
            )
            .await;
            let resolved = match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    let _ = events.send(WorkspaceEvent::Failed(error));
                    return;
                }
            };
            if resolved.source.external() && resolved.password().is_none() {
                let _ = events.send(WorkspaceEvent::NeedPassword(resolved));
                return;
            }
            connect_resolved(session, events, resolved).await;
        });
    }

    fn connect_pending(&mut self, password: Option<String>) {
        let Some(mut resolved) = self.pending_resolved.take() else {
            return;
        };
        resolved.set_password(password.clone());
        self.password = password;
        let session = self.session.clone();
        let events = self.events.clone();
        tokio::spawn(connect_resolved(session, events, resolved));
    }

    fn open_connections(&mut self) {
        self.profiles = sql::profile::list(&self.ctx).unwrap_or_default();
        self.overlay = Some(Overlay::Connections { cursor: 0 });
    }

    fn save_profile(&mut self, form: ProfileForm) {
        let ctx = self.ctx.clone();
        let events = self.events.clone();
        self.status = "connection을 검사한 뒤 저장합니다…".into();
        tokio::spawn(async move {
            let result = async {
                let spec = form.spec()?;
                if form.store_password && !form.existing_secret && form.password.is_empty() {
                    return Err(Error::Usage(
                        "password 저장을 선택했다면 password를 입력하세요.".into(),
                    ));
                }
                let test_password = if !form.password.is_empty() {
                    Some(form.password.clone())
                } else if let Some(key) = &form.editing {
                    let profile = sql::profile::require(&ctx, key)?;
                    match profile.credential_ref {
                        Some(reference) => ctx.secrets.get(&reference)?,
                        None => None,
                    }
                } else {
                    None
                };
                let resolved =
                    sql::connection::resolve_external_spec(&spec, test_password.clone())?;
                sql::connection::test(resolved).await?;
                if let Some(key) = &form.editing {
                    let password = if !form.store_password {
                        sql::PasswordUpdate::Remove
                    } else if form.password.is_empty() {
                        sql::PasswordUpdate::Keep
                    } else {
                        sql::PasswordUpdate::Replace(&form.password)
                    };
                    sql::profile::update(&ctx, key, spec, password)
                } else {
                    sql::profile::create(&ctx, spec, test_password.as_deref(), form.store_password)
                }
            }
            .await;
            let _ = events.send(WorkspaceEvent::ProfileSaved(result));
        });
    }

    fn test_profile(&mut self, index: usize, supplied_password: Option<String>) {
        let Some(profile) = self.profiles.get(index).cloned() else {
            return;
        };
        let ctx = self.ctx.clone();
        let events = self.events.clone();
        self.status = format!("{} connection 검사 중…", profile.name);
        tokio::spawn(async move {
            let result = async {
                let password = match supplied_password {
                    Some(password) => Some(password),
                    None => match &profile.credential_ref {
                        Some(reference) => ctx.secrets.get(reference)?,
                        None => None,
                    },
                };
                let resolved =
                    sql::connection::resolve_external_spec(&profile_spec(&profile), password)?;
                sql::connection::test(resolved).await
            }
            .await;
            let _ = events.send(WorkspaceEvent::ProfileTested(result));
        });
    }

    fn forget_profile(&mut self, index: usize) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        let result = sql::profile::forget(&self.ctx, &profile.id);
        let _ = self.events.send(WorkspaceEvent::ProfileForgotten(result));
    }

    fn switch_profile(&mut self, index: usize) {
        if self.transaction != "idle" {
            self.status =
                "열린 transaction을 commit 또는 rollback한 뒤 connection을 전환하세요".into();
            return;
        }
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        self.source_key = profile.id.clone();
        self.writable = false;
        self.confirmation = None;
        self.password = None;
        self.connected = false;
        self.catalog = CatalogState::default();
        self.results = Results::default();
        self.connect(None);
    }

    fn reconnect(&mut self) {
        if self.results.running {
            self.status = "실행 중인 query를 먼저 취소하세요".into();
            return;
        }
        self.generation = self.generation.saturating_add(1);
        self.connected = false;
        self.status =
            "reconnecting · transaction, temporary table, session setting은 사라집니다".into();
        self.connect(self.password.clone());
    }

    fn refresh_catalog(&mut self) {
        if self.catalog.loading {
            return;
        }
        self.catalog.begin_refresh();
        let ctx = self.ctx.clone();
        let source = self.source_key.clone();
        let writable = self.writable;
        let confirmation = self.confirmation.clone();
        let password = self.password.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let result = async {
                let resolved = sql::connection::resolve(
                    &ctx,
                    &source,
                    sql::connection::ResolveOptions {
                        writable,
                        confirmation: confirmation.as_deref(),
                        password,
                        start_tunnel: true,
                    },
                )
                .await?;
                let session = sql::connection::connect(resolved).await?;
                sql::catalog::refresh(&session).await
            }
            .await;
            let _ = events.send(WorkspaceEvent::Catalog(result));
        });
    }

    fn execute(&mut self, whole: bool) {
        if !self.connected || self.results.running {
            self.status = if self.results.running {
                "query가 이미 실행 중입니다".into()
            } else {
                "connection이 준비되지 않았습니다".into()
            };
            return;
        }
        let buffer = self.buffers.active();
        let range = if whole {
            let start = buffer.text.len() - buffer.text.trim_start().len();
            let end = buffer.text.trim_end().len();
            (start < end).then_some(start..end)
        } else if let Some(selection) = buffer.selection() {
            Some(selection)
        } else {
            sql::statement::statement_at(&buffer.text, buffer.cursor)
        };
        let Some(range) = range else {
            self.status = "실행할 SQL statement가 없습니다".into();
            return;
        };
        let text = buffer.text[range.clone()].to_string();
        self.last_sql = Some(text.clone());
        self.results.begin();
        self.focus = Pane::Results;
        let cancel = Cancel::new();
        self.current_cancel = Some(cancel.clone());
        let session = self.session.clone();
        let ctx = self.ctx.clone();
        let events = self.events.clone();
        let preview_rows = self
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.preview_rows)
            .unwrap_or(sql::profile::DEFAULT_PREVIEW_ROWS);
        tokio::spawn(async move {
            let mut guard = session.lock().await;
            let Some(session) = guard.as_mut() else {
                let _ = events.send(WorkspaceEvent::Failed(Error::failed(
                    "SQL 실행에 실패했습니다",
                    "session이 연결되어 있지 않습니다.",
                    "reconnect한 뒤 다시 실행하세요.",
                )));
                return;
            };
            let (query_tx, mut query_rx) = mpsc::channel(8);
            let request = sql::runner::ExecuteRequest {
                sql: text,
                base_offset: range.start,
                limits: sql::runner::QueryLimits::preview(preview_rows),
            };
            let run = sql::runner::execute(&ctx, session, request, &cancel, &query_tx);
            let bridge = async {
                while let Some(event) = query_rx.recv().await {
                    let finished = matches!(event, sql::runner::QueryEvent::Finished(_));
                    let _ = events.send(WorkspaceEvent::Query(event));
                    if finished {
                        break;
                    }
                }
            };
            let (outcome, ()) = tokio::join!(run, bridge);
            if let Err(error) = outcome {
                let _ = events.send(WorkspaceEvent::Failed(error));
            }
        });
    }

    fn export(&mut self, path: PathBuf) {
        let Some(sql_text) = self.last_sql.clone() else {
            self.status = "export할 result가 없습니다".into();
            return;
        };
        let format = match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("csv") => sql::export::ExportFormat::Csv,
            Some("jsonl") => sql::export::ExportFormat::Jsonl,
            Some("json") => sql::export::ExportFormat::Json,
            _ => {
                self.status = "export 파일 확장자는 .csv, .json, .jsonl 중 하나여야 합니다".into();
                return;
            }
        };
        let cancel = Cancel::new();
        self.current_cancel = Some(cancel.clone());
        let session = self.session.clone();
        let ctx = self.ctx.clone();
        let events = self.events.clone();
        self.status = "전체 result를 streaming export하기 위해 query를 다시 실행합니다…".into();
        tokio::spawn(async move {
            let mut guard = session.lock().await;
            let result = match guard.as_mut() {
                Some(session) => {
                    sql::export::export(&ctx, session, &sql_text, &path, format, &cancel).await
                }
                None => Err(Error::failed(
                    "SQL export에 실패했습니다",
                    "session이 연결되어 있지 않습니다.",
                    "reconnect한 뒤 다시 export하세요.",
                )),
            };
            let _ = events.send(WorkspaceEvent::Exported(result));
        });
    }

    fn rollback_and_exit(&mut self) {
        let session = self.session.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut guard = session.lock().await;
            let result = match guard.as_mut() {
                Some(session) => {
                    session
                        .client()
                        .batch_execute("ROLLBACK")
                        .await
                        .map_err(|error| {
                            sql::connection::postgres_failure(
                                "열린 transaction을 rollback할 수 없습니다",
                                &error,
                            )
                        })
                }
                None => Ok(()),
            };
            let _ = events.send(WorkspaceEvent::RolledBack(result));
        });
    }

    fn on_workspace_event(&mut self, event: WorkspaceEvent) {
        match event {
            WorkspaceEvent::NeedPassword(resolved) => {
                self.pending_resolved = Some(resolved);
                self.overlay = Some(Overlay::Password(String::new()));
                self.status = "외부 connection password를 입력하세요".into();
            }
            WorkspaceEvent::Connected {
                endpoint,
                source,
                generation,
            } => {
                self.endpoint = Some(endpoint.clone());
                self.source = Some(source);
                self.connected = true;
                self.generation = generation;
                self.transaction = "idle".into();
                self.status = if let Some(warning) = endpoint.tls_mode.warning() {
                    format!(
                        "connected · {} · {} · WARNING: {warning}",
                        endpoint.redacted_url(),
                        endpoint.access.label()
                    )
                } else {
                    format!(
                        "connected · {} · {}",
                        endpoint.redacted_url(),
                        endpoint.access.label()
                    )
                };
                self.refresh_catalog();
            }
            WorkspaceEvent::Catalog(result) => match result {
                Ok(catalog) => {
                    self.catalog.set_catalog(catalog);
                    self.catalog.expand_defaults();
                    self.status = "catalog를 갱신했습니다".into();
                }
                Err(error) => {
                    self.catalog.set_error(error.to_string());
                    self.status = "catalog refresh 실패 · query session은 유지됩니다".into();
                }
            },
            WorkspaceEvent::Query(event) => {
                if let sql::runner::QueryEvent::Finished(summary) = &event {
                    self.transaction = summary.transaction.clone();
                    self.current_cancel = None;
                    self.status = if summary.success {
                        format!(
                            "{} statement · {} rows · {} ms",
                            summary.statements, summary.row_count, summary.elapsed_ms
                        )
                    } else {
                        summary
                            .error
                            .as_ref()
                            .map(|error| error.message.clone())
                            .unwrap_or_else(|| "query failed".into())
                    };
                    self.history = sql::history::list(&self.ctx, 100).unwrap_or_default();
                }
                self.results.apply(event);
            }
            WorkspaceEvent::Exported(result) => {
                self.current_cancel = None;
                match result {
                    Ok(receipt) => {
                        self.status = format!(
                            "{} rows · {} bytes → {}",
                            receipt.rows,
                            receipt.bytes,
                            receipt.path.display()
                        );
                    }
                    Err(error) => self.show_error(error),
                }
            }
            WorkspaceEvent::RolledBack(result) => match result {
                Ok(()) => {
                    self.transaction = "idle".into();
                    self.quit = true;
                }
                Err(error) => self.show_error(error),
            },
            WorkspaceEvent::ProfileSaved(result) => match result {
                Ok(profile) => {
                    self.profiles = sql::profile::list(&self.ctx).unwrap_or_default();
                    let cursor = self
                        .profiles
                        .iter()
                        .position(|candidate| candidate.id == profile.id)
                        .unwrap_or(0);
                    self.status = format!("{} connection을 저장했습니다", profile.name);
                    self.overlay = Some(Overlay::Connections { cursor });
                }
                Err(error) => self.show_error(error),
            },
            WorkspaceEvent::ProfileTested(result) => match result {
                Ok(()) => self.status = "PostgreSQL connection 검사를 통과했습니다".into(),
                Err(error) => self.show_error(error),
            },
            WorkspaceEvent::ProfileForgotten(result) => match result {
                Ok(profile) => {
                    self.profiles = sql::profile::list(&self.ctx).unwrap_or_default();
                    self.status = format!("{} connection을 삭제했습니다", profile.name);
                    self.overlay = Some(Overlay::Connections { cursor: 0 });
                }
                Err(error) => self.show_error(error),
            },
            WorkspaceEvent::Failed(error) => {
                self.results.running = false;
                self.current_cancel = None;
                if error.to_string().contains("연결") || error.to_string().contains("connection")
                {
                    self.connected = false;
                }
                self.show_error(error);
            }
        }
    }

    fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if let Err(error) = self.buffers.persist_if_due() {
            self.status = format!("buffer recovery 저장 실패: {error}");
        }
        self.keep_cursor_visible();
    }

    fn on_terminal_event(&mut self, event: Event) {
        match event {
            Event::Paste(text) if self.overlay.is_none() && self.focus == Pane::Editor => {
                self.buffers.active_mut().insert(&text);
                self.completions.clear();
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Resize(..)
            | Event::Mouse(_)
            | Event::FocusGained
            | Event::FocusLost
            | Event::Paste(_)
            | Event::Key(_) => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.consume_overlay(key) || self.consume_completion(key) {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if ctrl && key.code == KeyCode::Char('c') && self.results.running {
            if let Some(cancel) = &self.current_cancel {
                cancel.cancel();
                self.status = "query cancellation을 요청했습니다".into();
            }
            return;
        }
        match (key.code, ctrl, alt) {
            (KeyCode::Esc, _, _) => self.request_exit(),
            (KeyCode::Tab, false, false) => self.focus = self.focus.next(false),
            (KeyCode::BackTab, false, false) => self.focus = self.focus.next(true),
            (KeyCode::F(10), false, false) => {
                self.fullscreen = if self.fullscreen == Some(self.focus) {
                    None
                } else {
                    Some(self.focus)
                };
            }
            (KeyCode::F(5), false, false) => self.execute(true),
            (KeyCode::Enter, true, false) => self.execute(false),
            (KeyCode::F(4), false, false) if self.focus == Pane::Editor => {
                let formatted = sql::statement::format(&self.buffers.active().text);
                self.buffers.active_mut().replace_all(formatted);
            }
            (KeyCode::F(6), false, false) => self.focus = Pane::Catalog,
            (KeyCode::F(7), false, false) => self.open_connections(),
            (KeyCode::F(8), false, false) => {
                self.history = sql::history::list(&self.ctx, 100).unwrap_or_default();
                self.overlay = Some(Overlay::History { cursor: 0 });
            }
            (KeyCode::Char('n'), true, false) => self.buffers.new_buffer(),
            (KeyCode::Char('w'), true, false) => {
                if !self.buffers.close_active(false) {
                    self.overlay = Some(Overlay::ConfirmCloseBuffer);
                }
            }
            (KeyCode::Left, false, true) => self.buffers.switch(-1),
            (KeyCode::Right, false, true) => self.buffers.switch(1),
            (KeyCode::Char('f'), true, false) => {
                self.overlay = Some(Overlay::Search(String::new()))
            }
            (KeyCode::Char('g'), true, false) => self.overlay = Some(Overlay::Goto(String::new())),
            (KeyCode::Char('e'), true, false) if self.results.active().is_some() => {
                self.overlay = Some(Overlay::Export("result.csv".into()))
            }
            (KeyCode::Char('r'), true, false) => {
                if self.transaction == "idle" {
                    self.reconnect();
                } else {
                    self.overlay = Some(Overlay::ConfirmReconnect);
                }
            }
            (KeyCode::Char(' '), true, false) if self.focus == Pane::Editor => {
                self.open_completions()
            }
            _ => self.on_pane_key(key, shift, ctrl),
        }
    }

    fn on_pane_key(&mut self, key: KeyEvent, shift: bool, ctrl: bool) {
        match self.focus {
            Pane::Editor => {
                let buffer = self.buffers.active_mut();
                match key.code {
                    KeyCode::Char('z') if ctrl => buffer.undo(),
                    KeyCode::Char('y') if ctrl => buffer.redo(),
                    KeyCode::Char('a') if ctrl => buffer.select_all(),
                    KeyCode::Left => buffer.move_horizontal(-1, shift),
                    KeyCode::Right => buffer.move_horizontal(1, shift),
                    KeyCode::Up => buffer.move_vertical(-1, shift),
                    KeyCode::Down => buffer.move_vertical(1, shift),
                    KeyCode::Home => buffer.home(shift),
                    KeyCode::End => buffer.end(shift),
                    KeyCode::Backspace => buffer.backspace(),
                    KeyCode::Delete => buffer.delete(),
                    KeyCode::Enter => buffer.insert("\n"),
                    KeyCode::Tab => buffer.insert("    "),
                    KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                        buffer.insert(&ch.to_string())
                    }
                    _ => return,
                }
                self.completions.clear();
            }
            Pane::Catalog => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.catalog.move_by(-1, 20),
                KeyCode::Down | KeyCode::Char('j') => self.catalog.move_by(1, 20),
                KeyCode::Left => self.catalog.toggle(),
                KeyCode::Right => self.catalog.toggle(),
                KeyCode::Enter => {
                    if let Some(node) = self.catalog.selected() {
                        if let Some(value) = node.insert {
                            self.buffers.active_mut().insert(&value);
                            self.focus = Pane::Editor;
                        } else {
                            self.catalog.toggle();
                        }
                    }
                }
                KeyCode::Char('r') => self.refresh_catalog(),
                _ => {}
            },
            Pane::Results => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.results.move_row(-1, 10),
                KeyCode::Down | KeyCode::Char('j') => self.results.move_row(1, 10),
                KeyCode::Left | KeyCode::Char('h') => self.results.move_column(-1, 4),
                KeyCode::Right | KeyCode::Char('l') => self.results.move_column(1, 4),
                KeyCode::Char('[') => self.results.switch(-1),
                KeyCode::Char(']') => self.results.switch(1),
                KeyCode::Enter => {
                    if let Some(cell) = self.results.selected_cell() {
                        self.overlay = Some(Overlay::Message {
                            title: "Cell detail".into(),
                            body: cell.unwrap_or("NULL").to_string(),
                        });
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(value) = self.results.selected_tsv() {
                        match crate::tui::clipboard::copy(&self.ctx.config.ui, &value, false) {
                            Ok(outcome) => self.status = outcome.message("선택한 cell"),
                            Err(error) => self.show_error(error),
                        }
                    }
                }
                _ => {}
            },
        }
    }

    fn consume_overlay(&mut self, key: KeyEvent) -> bool {
        let Some(mut overlay) = self.overlay.take() else {
            return false;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut overlay {
            Overlay::Search(input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let found = self.buffers.active_mut().find(input, false);
                    self.status = if found {
                        "검색 결과로 이동했습니다"
                    } else {
                        "검색 결과가 없습니다"
                    }
                    .into();
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.overlay = Some(overlay);
                }
                KeyCode::Char(ch) if !ctrl => {
                    input.push(ch);
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Goto(input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if let Ok(line) = input.parse::<usize>() {
                        self.buffers.active_mut().goto_line(line);
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.overlay = Some(overlay);
                }
                KeyCode::Char(ch) if ch.is_ascii_digit() => {
                    input.push(ch);
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Export(input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.export(PathBuf::from(input.clone())),
                KeyCode::Backspace => {
                    input.pop();
                    self.overlay = Some(overlay);
                }
                KeyCode::Char(ch) if !ctrl => {
                    input.push(ch);
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Password(input) => match key.code {
                KeyCode::Esc => self.status = "connection password 입력을 취소했습니다".into(),
                KeyCode::Enter => {
                    let password = (!input.is_empty()).then(|| std::mem::take(input));
                    self.connect_pending(password);
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.overlay = Some(overlay);
                }
                KeyCode::Char(ch) if !ctrl => {
                    input.push(ch);
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::Message { .. } => {
                if key.code != KeyCode::Esc && key.code != KeyCode::Enter {
                    self.overlay = Some(overlay);
                }
            }
            Overlay::Connections { cursor } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                    self.overlay = Some(overlay);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1).min(self.profiles.len().saturating_sub(1));
                    self.overlay = Some(overlay);
                }
                KeyCode::Char('n') => self.overlay = Some(Overlay::ProfileForm(ProfileForm::new())),
                KeyCode::Char('e') => {
                    if let Some(profile) = self.profiles.get(*cursor) {
                        self.overlay = Some(Overlay::ProfileForm(ProfileForm::edit(profile)));
                    }
                }
                KeyCode::Char('t') => {
                    if self
                        .profiles
                        .get(*cursor)
                        .is_some_and(|profile| profile.credential_ref.is_none())
                    {
                        self.overlay = Some(Overlay::ProfileTestPassword {
                            cursor: *cursor,
                            input: String::new(),
                        });
                    } else {
                        let selected = *cursor;
                        self.test_profile(selected, None);
                        self.overlay = Some(overlay);
                    }
                }
                KeyCode::Char('x') | KeyCode::Delete => {
                    self.overlay = Some(Overlay::ConfirmForgetProfile { cursor: *cursor });
                }
                KeyCode::Enter => self.switch_profile(*cursor),
                _ => self.overlay = Some(overlay),
            },
            Overlay::ProfileForm(form) => match key.code {
                KeyCode::Esc => self.overlay = Some(Overlay::Connections { cursor: 0 }),
                KeyCode::Tab | KeyCode::Down => {
                    form.focus = (form.focus + 1) % 12;
                    self.overlay = Some(overlay);
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.focus = (form.focus + 11) % 12;
                    self.overlay = Some(overlay);
                }
                KeyCode::Char('s') if ctrl => self.save_profile(form.clone()),
                KeyCode::Char(' ') | KeyCode::Enter if matches!(form.focus, 5 | 6 | 8 | 10) => {
                    match form.focus {
                        5 => {
                            form.tls_mode = if form.tls_mode == sql::TlsMode::VerifyFull {
                                sql::TlsMode::Disable
                            } else {
                                sql::TlsMode::VerifyFull
                            }
                        }
                        6 => {
                            form.access_mode = if form.access_mode == sql::AccessMode::ReadOnly {
                                sql::AccessMode::ReadWrite
                            } else {
                                sql::AccessMode::ReadOnly
                            }
                        }
                        8 => form.store_password = !form.store_password,
                        10 => form.history_text = !form.history_text,
                        _ => {}
                    }
                    self.overlay = Some(overlay);
                }
                KeyCode::Backspace => {
                    if let Some(value) = form.value_mut() {
                        value.pop();
                    }
                    self.overlay = Some(overlay);
                }
                KeyCode::Char(ch) if !ctrl => {
                    if let Some(value) = form.value_mut() {
                        value.push(ch);
                    }
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::ConfirmForgetProfile { cursor } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.forget_profile(*cursor),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.overlay = Some(Overlay::Connections { cursor: *cursor })
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::ProfileTestPassword { cursor, input } => match key.code {
                KeyCode::Esc => {
                    self.overlay = Some(Overlay::Connections { cursor: *cursor });
                }
                KeyCode::Enter => {
                    let password = (!input.is_empty()).then(|| std::mem::take(input));
                    self.test_profile(*cursor, password);
                    self.overlay = Some(Overlay::Connections { cursor: *cursor });
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.overlay = Some(overlay);
                }
                KeyCode::Char(ch) if !ctrl => {
                    input.push(ch);
                    self.overlay = Some(overlay);
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::ConfirmReconnect => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.reconnect(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.overlay = Some(overlay),
            },
            Overlay::History { cursor } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                    self.overlay = Some(overlay);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1).min(self.history.len().saturating_sub(1));
                    self.overlay = Some(overlay);
                }
                KeyCode::Enter => {
                    if let Some(text) = self
                        .history
                        .get(*cursor)
                        .and_then(|entry| entry.query_text.clone())
                    {
                        self.buffers.new_buffer();
                        self.buffers.active_mut().insert(&text);
                    } else {
                        self.status = "이 history 항목에는 query text가 저장되지 않았습니다".into();
                    }
                }
                _ => self.overlay = Some(overlay),
            },
            Overlay::ConfirmCloseBuffer => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.buffers.close_active(true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.overlay = Some(overlay),
            },
            Overlay::ConfirmExitTransaction => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.rollback_and_exit(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.overlay = Some(overlay),
            },
        }
        true
    }

    fn consume_completion(&mut self, key: KeyEvent) -> bool {
        if self.completions.is_empty() {
            return false;
        }
        match key.code {
            KeyCode::Esc => self.completions.clear(),
            KeyCode::Up => {
                self.completion_index = self.completion_index.saturating_sub(1);
            }
            KeyCode::Down => {
                self.completion_index = (self.completion_index + 1).min(self.completions.len() - 1);
            }
            KeyCode::Enter | KeyCode::Tab => {
                let value = self.completions[self.completion_index].value.clone();
                let buffer = self.buffers.active_mut();
                let prefix = sql::statement::identifier_prefix(&buffer.text, buffer.cursor);
                buffer.anchor = Some(prefix.start);
                buffer.cursor = prefix.end;
                buffer.insert(&value);
                self.completions.clear();
            }
            _ => return false,
        }
        true
    }

    fn open_completions(&mut self) {
        let buffer = self.buffers.active();
        let range = sql::statement::identifier_prefix(&buffer.text, buffer.cursor);
        let prefix = &buffer.text[range];
        self.completions = if let Some(catalog) = &self.catalog.catalog {
            catalog.completions(prefix)
        } else {
            sql::statement::keyword_completions(prefix)
                .into_iter()
                .map(|value| sql::catalog::CompletionItem {
                    label: value.clone(),
                    value,
                    kind: sql::catalog::CompletionKind::Keyword,
                })
                .collect()
        };
        self.completion_index = 0;
        if self.completions.is_empty() {
            self.status = "autocomplete 후보가 없습니다".into();
        }
    }

    fn request_exit(&mut self) {
        if self.results.running {
            self.status = "실행 중인 query를 먼저 취소하세요".into();
        } else if self.transaction != "idle" {
            self.overlay = Some(Overlay::ConfirmExitTransaction);
        } else {
            self.quit = true;
        }
    }

    fn show_error(&mut self, error: Error) {
        let diagnostic = error.as_diagnostic();
        self.status = diagnostic.what.clone();
        self.overlay = Some(Overlay::Message {
            title: diagnostic.what,
            body: format!("{}\n\n다음 행동\n{}", diagnostic.cause, diagnostic.next),
        });
    }

    fn keep_cursor_visible(&mut self) {
        let (line, column) = self.buffers.active().line_column();
        const HEIGHT: usize = 12;
        const WIDTH: usize = 60;
        if line < self.editor_scroll {
            self.editor_scroll = line;
        } else if line >= self.editor_scroll + HEIGHT {
            self.editor_scroll = line + 1 - HEIGHT;
        }
        if column < self.editor_horizontal {
            self.editor_horizontal = column;
        } else if column >= self.editor_horizontal + WIDTH {
            self.editor_horizontal = column + 1 - WIDTH;
        }
    }

    pub(crate) fn current_selection(&self) -> Option<Range<usize>> {
        self.buffers.active().selection()
    }
}

fn profile_spec(profile: &sql::SqlConnectionProfile) -> sql::ProfileSpec {
    sql::ProfileSpec {
        name: profile.name.clone(),
        host: profile.host.clone(),
        port: profile.port,
        database: profile.database.clone(),
        username: profile.username.clone(),
        tls_mode: profile.tls_mode,
        access_mode: profile.access_mode,
        root_ca_path: profile.root_ca_path.clone(),
        connect_timeout_ms: profile.connect_timeout_ms,
        query_timeout_ms: profile.query_timeout_ms,
        application_name: profile.application_name.clone(),
        history_text: profile.history_text,
        preview_rows: profile.preview_rows,
    }
}

async fn connect_resolved(
    session: Arc<Mutex<Option<sql::connection::SqlSession>>>,
    events: mpsc::UnboundedSender<WorkspaceEvent>,
    resolved: ResolvedSqlConnection,
) {
    match sql::connection::connect(resolved).await {
        Ok(next) => {
            let endpoint = next.endpoint.clone();
            let source = next.source.clone();
            let generation = next.generation;
            *session.lock().await = Some(next);
            let _ = events.send(WorkspaceEvent::Connected {
                endpoint,
                source,
                generation,
            });
        }
        Err(error) => {
            let _ = events.send(WorkspaceEvent::Failed(error));
        }
    }
}

fn spawn_reader(
    stop: Arc<AtomicBool>,
) -> (mpsc::UnboundedReceiver<Event>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let reader = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    (rx, reader)
}
