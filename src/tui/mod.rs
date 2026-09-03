//! Terminal user interface. A presentation layer over `core` — it must never
//! contain behaviour the CLI cannot reach (PRD §6.2, principle 7).
//!
//! The shape of this module:
//!
//! * [`terminal`] owns raw mode and guarantees restoration (TUI-007).
//! * [`layout`], [`rows`], [`hints`], [`form`] and [`modal`] are pure: plain
//!   data in, strings and geometry out. They hold the behaviour worth testing.
//! * [`render`] draws a [`render::View`] and nothing else, so every screen can
//!   be asserted against a `TestBackend`.
//! * [`job`] runs `core` calls on tokio tasks; [`App`] below is the event loop
//!   that never awaits one of them directly (TUI-006).

pub mod chrome;
pub mod clipboard;
pub mod data;

pub mod form;
pub mod hints;
pub mod hit;
pub mod job;
pub mod keymap;
pub mod layout;
pub mod modal;
pub mod onboard;
pub mod render;
pub mod rows;
pub mod ssh_form;
pub mod terminal;

pub mod theme;

use crate::core::model::{EngineKind, Origin, ResourceKind, TunnelStatus};
use crate::core::progress::Progress;
use crate::core::{backup, bucket, database, engine, target, tunnel, Ctx, Error, Result};
use crate::tui::data::{Resource, Snapshot};
use crate::tui::form::{Field, Form};
use crate::tui::hints::Focus;
use crate::tui::hit::Hit;
use crate::tui::job::{Jobs, Outcome};
use crate::tui::keymap::{Action, Chord, Context, Keymap, Screen};
use crate::tui::modal::{Confirm, ConfirmFocus, Intent, Message, Modal, Palette};
use crate::tui::render::{Toast, View};
use crate::tui::rows::{Endpoint, TableData};
use crate::tui::terminal::{TerminalGuard, Tui};
use crate::tui::theme::Theme;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::widgets::TableState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Frame cadence. Fast enough for a spinner to read as motion, slow enough
/// that an idle app costs nothing (PRD §12.2).
const TICK_MS: u64 = 120;
/// How long `s` keeps a secret on screen.
const REVEAL_TICKS: u16 = (8_000 / TICK_MS) as u16;
/// How long a toast stays on the hint row.
const TOAST_TICKS: u16 = (4_000 / TICK_MS) as u16;
/// Lines of engine log to fetch for `l`.
const LOG_TAIL: usize = 200;

/// Entry point. `cli::main` calls this when `linf` is run with no subcommand.
pub fn run() -> Result<()> {
    let ctx = Arc::new(Ctx::open(Origin::Tui)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // The panic hook goes in before the terminal is touched, so a panic during
    // setup cannot leave a raw terminal behind (TUI-007).
    terminal::install_panic_hook();
    let mut guard = TerminalGuard::enter(true)?;

    let result = runtime.block_on(async {
        let mut app = App::new(ctx.clone());
        app.run(&mut guard.terminal).await
    });

    // Restore before the exit hook talks about tunnels, so anything it prints
    // lands on the real terminal rather than the alternate screen.
    drop(guard);
    let shutdown = runtime.block_on(tunnel::shutdown_for_exit(&ctx));
    result.and(shutdown)
}

/// What the `/` line is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Filter,
}

struct App {
    ctx: Arc<Ctx>,
    theme: Theme,
    keymap: Keymap,
    keymap_problems: Vec<String>,
    screen: Screen,
    focus: Focus,
    /// Cursor and scroll offset per screen, indexed by `Screen::ALL`.
    cursor: [usize; Screen::ALL.len()],
    table_state: TableState,
    detail_scroll: u16,
    filter: String,
    mode: Mode,
    snapshot: Snapshot,
    /// Rebuilt every frame from the snapshot, so the loop and the renderer
    /// always agree on what row the cursor is on.
    table: TableData,
    /// Resource the cached `endpoint` was resolved for.
    endpoint_key: Option<String>,
    modal: Option<Modal>,
    endpoint: Endpoint,
    reveal: u16,
    toast: Option<(Toast, u16)>,
    tick: usize,
    jobs: Jobs,
    quit: bool,
    hits: hit::Hits,
    /// After registering the local target, open the create form.
    pending_open_form: bool,
}

impl App {
    fn new(ctx: Arc<Ctx>) -> Self {
        let theme = Theme::from_config(&ctx.config);
        let (keymap, keymap_problems) = Keymap::defaults().with_overrides(&ctx.config.keymap);
        let (jobs, _) = Jobs::new();
        let mut app = Self {
            theme,
            keymap,
            keymap_problems,
            screen: Screen::Resources,
            focus: Focus::List,
            cursor: [0; Screen::ALL.len()],
            table_state: TableState::default(),
            detail_scroll: 0,
            filter: String::new(),
            mode: Mode::Browse,
            snapshot: Snapshot::empty(),
            endpoint_key: None,
            table: TableData {
                headers: &[],
                rows: Vec::new(),
                visible: Vec::new(),
            },
            modal: None,
            endpoint: Endpoint::default(),
            reveal: 0,
            toast: None,
            tick: 0,
            jobs,
            quit: false,
            hits: hit::Hits::default(),
            pending_open_form: false,
            ctx,
        };
        if let Some(notice) = app.ctx.notices.first() {
            app.notify(notice.clone(), true);
        }
        app
    }

    /// The loop. It selects over terminal events, job progress, job results
    /// and a tick — and never awaits a `core` call, so a slow `docker pull`
    /// cannot stop a keypress from being drawn (TUI-006).
    async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        // A fresh registry, so the channels belong to this loop.
        let (jobs, mut channels) = Jobs::new();
        self.jobs = jobs;

        let stop = Arc::new(AtomicBool::new(false));
        let mut events = spawn_reader(stop.clone());

        self.reconcile_and_load();

        let mut ticker = tokio::time::interval(Duration::from_millis(TICK_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut area = ratatui::layout::Rect::default();
        let outcome = loop {
            self.rebuild_table();
            // The table is what decides which resource is selected, so the
            // endpoint can only be resolved once it exists — doing it while
            // handling the event that changed the snapshot is a frame too early.
            self.sync_endpoint();
            if let Err(e) = terminal.draw(|frame| {
                area = frame.area();
                let view = self.view();
                let mut state = view_state(&self.table_state);
                render::draw(frame, &view, &mut state);
                self.table_state = state;
            }) {
                break Err(Error::from(e));
            }
            self.hits = hit::compute(&self.view(), area, self.table_state.offset());
            if self.quit {
                break Ok(());
            }

            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.on_event(event),
                    // The reader thread is gone; without input there is no app.
                    None => break Ok(()),
                },
                Some((id, progress)) = channels.progress.recv() => {
                    self.on_progress(id, progress);
                }
                Some((id, result)) = channels.done.recv() => {
                    self.on_done(id, result);
                }
                _ = ticker.tick() => self.on_tick(),
            }
        };

        stop.store(true, Ordering::SeqCst);
        outcome
    }

    // -- frame ---------------------------------------------------------------

    fn rebuild_table(&mut self) {
        self.table = rows::table_for(
            self.screen,
            &self.snapshot,
            &self.ctx.config,
            &self.filter,
            &self.theme,
        );
        let count = self.table.len();
        let slot = screen_slot(self.screen);
        if count == 0 {
            self.cursor[slot] = 0;
        } else if self.cursor[slot] >= count {
            self.cursor[slot] = count - 1;
        }
    }

    /// Re-resolve the connection facts whenever the rendered selection changes.
    /// `endpoint_key` is cleared by anything that can change a secret without
    /// changing the selection, such as a key rotation.
    fn sync_endpoint(&mut self) {
        let key = self.selected_resource().map(|r| r.id().to_string());
        if key != self.endpoint_key {
            self.endpoint_key = key;
            self.refresh_endpoint();
        }
    }

    fn view(&self) -> View<'_> {
        View {
            snapshot: &self.snapshot,
            config: &self.ctx.config,
            theme: &self.theme,
            keymap: &self.keymap,
            screen: self.screen,
            focus: self.focus,
            table: &self.table,
            cursor: self.cursor[screen_slot(self.screen)],
            detail_scroll: self.detail_scroll,
            filter: (self.mode == Mode::Filter).then_some(self.filter.as_str()),
            modal: self.modal.as_ref(),
            reveal: self.reveal > 0,
            endpoint: &self.endpoint,
            engine_status: self.selected_engine_status(),
            job: self.jobs.foreground(),
            job_log: self.jobs.log(),
            tick: self.tick,
            notices: &self.ctx.notices,
            keymap_problems: &self.keymap_problems,
            toast: self.toast.as_ref().map(|(toast, _)| toast),
            now: crate::core::util::now(),
        }
    }

    fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.reveal = self.reveal.saturating_sub(1);
        if let Some((_, remaining)) = &mut self.toast {
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                self.toast = None;
            }
        }
    }
    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Resize(..)
            | Event::Key(_)
            | Event::FocusGained
            | Event::FocusLost
            | Event::Paste(_) => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match self.hits.at(mouse.column, mouse.row) {
                    Some(Hit::Nav(screen)) if self.modal.is_none() => {
                        self.screen = screen;
                        self.focus = Focus::List;
                        self.detail_scroll = 0;
                        self.mode = Mode::Browse;
                        self.filter.clear();
                        self.refresh_endpoint();
                    }
                    Some(Hit::Row(index)) => self.set_cursor(index),
                    Some(Hit::Action(action)) => self.on_action(action),
                    Some(Hit::AdvanceOnboard) => self.advance_onboard(),
                    Some(Hit::FormChoice { field, index }) => {
                        if let Some(Modal::Form(form)) = &mut self.modal {
                            if form.select_index(field, index) {
                                self.refresh_plan();
                            }
                        }
                    }
                    Some(Hit::FormField(field)) => {
                        if let Some(Modal::Form(form)) = &mut self.modal {
                            form.click_field(field);
                        }
                    }
                    Some(Hit::Dismiss) => self.on_cancel(),
                    None | Some(Hit::Nav(_)) => {}
                }
            }
            MouseEventKind::ScrollUp => self.on_scroll(-1),
            MouseEventKind::ScrollDown => self.on_scroll(1),
            _ => {}
        }
    }

    fn on_scroll(&mut self, delta: i32) {
        match &mut self.modal {
            Some(Modal::Help { scroll }) => {
                if delta < 0 {
                    *scroll = scroll.saturating_sub(1);
                } else {
                    *scroll = scroll.saturating_add(1);
                }
                return;
            }
            Some(Modal::Message(message)) => {
                if delta < 0 {
                    message.scroll = message.scroll.saturating_sub(1);
                } else {
                    message.scroll = message.scroll.saturating_add(1);
                }
                return;
            }
            Some(Modal::Form(form)) => {
                if delta > 0 {
                    form.next_field();
                } else {
                    form.prev_field();
                }
                return;
            }
            Some(_) => return,
            None => {}
        }
        if self.focus == Focus::Detail {
            if delta < 0 {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            } else {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
            }
        } else {
            self.move_cursor(delta as isize);
        }
    }

    fn setup_active(&self) -> bool {
        self.modal.is_none() && onboard::phase(&self.snapshot).active()
    }

    fn try_onboard(&mut self) -> bool {
        if !self.setup_active() {
            return false;
        }
        if !matches!(
            self.screen,
            Screen::Resources | Screen::Engines | Screen::Targets
        ) {
            return false;
        }
        self.advance_onboard();
        true
    }

    fn advance_onboard(&mut self) {
        if self.jobs.busy() {
            return;
        }
        match onboard::phase(&self.snapshot) {
            onboard::Phase::Checking | onboard::Phase::DockerDown => {
                self.load(false);
                self.notify("Docker를 다시 확인합니다. Desktop을 먼저 켜 주세요.", false);
            }
            onboard::Phase::RegisterLocal => self.register_local(),
            onboard::Phase::CreateFirst => self.open_first_form(),
            onboard::Phase::Done => {}
        }
    }

    fn register_local(&mut self) {
        if self.jobs.busy() {
            return;
        }
        self.pending_open_form = true;
        self.jobs.spawn(
            "이 컴퓨터 등록",
            self.ctx.clone(),
            false,
            |ctx, _, _| async move {
                let target = target::add_local(
                    &ctx,
                    &target::LocalSpec {
                        display_name: "local".into(),
                        docker_command: "docker".into(),
                    },
                )
                .await?;
                Ok(Outcome::Note(format!(
                    "`{}`을(를) 등록했습니다. 이제 첫 DB 또는 버킷을 만드세요.",
                    target.display_name
                )))
            },
        );
    }

    fn open_first_form(&mut self) {
        self.open_form(EngineKind::Postgres);
        if let Some(Modal::Form(form)) = &mut self.modal {
            form.first_run = self.snapshot.resources.is_empty();
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // `Ctrl+C` cancels work before it means anything else (TUI-006).
        if is_ctrl_c(key) && self.jobs.busy() {
            self.jobs.cancel_all();
            self.notify("취소를 요청했습니다", false);
            return;
        }

        // Text-entry contexts claim printable keys, or a bucket named `query`
        // would trip `q` on its way in.
        if self.consume_text(key) {
            return;
        }

        let context = self.key_context();
        let Some(action) = self.keymap.resolve(context, key) else {
            return;
        };
        self.on_action(action);
    }

    fn key_context(&self) -> Context {
        if self.modal.is_some() {
            Context::Modal
        } else {
            Context::List
        }
    }

    /// Returns true when the key was swallowed as text or as a modal-local
    /// control key.
    fn consume_text(&mut self, key: KeyEvent) -> bool {
        let printable = printable(key);

        // The palette runs its selection on Enter; nothing it can run is
        // destructive on its own, since every deletion opens a confirmation.
        if let Some(Modal::Palette(palette)) = &mut self.modal {
            match key.code {
                KeyCode::Enter => {
                    let action = palette.selection();
                    self.modal = None;
                    if let Some(action) = action {
                        self.on_action(action);
                    }
                    return true;
                }
                KeyCode::Backspace => return palette.backspace(),
                _ => {
                    if let Some(c) = printable {
                        return palette.type_char(c);
                    }
                    return false;
                }
            }
        }

        if let Some(Modal::Confirm(confirm)) = &mut self.modal {
            // `Enter` is deliberately inert here (PRD §7.9).
            match key.code {
                KeyCode::Enter => return true,
                KeyCode::Backspace => return confirm.backspace(),
                _ => {
                    if let Some(c) = printable {
                        return confirm.type_char(c);
                    }
                    return false;
                }
            }
        }

        if let Some(Modal::Form(form)) = &mut self.modal {
            let (consumed, changed) = match key.code {
                KeyCode::Backspace => {
                    let changed = form.backspace();
                    (changed, changed)
                }
                // Form rows are stacked, while each row's choices are laid out
                // horizontally. Keep the arrow axes aligned with that geometry.
                KeyCode::Up => {
                    form.prev_field();
                    (true, false)
                }
                KeyCode::Down => {
                    form.next_field();
                    (true, false)
                }
                KeyCode::Left => (true, form.move_option(false)),
                KeyCode::Right => (true, form.move_option(true)),
                KeyCode::Char(' ') if form.focus == Field::Kind => (true, form.toggle()),
                _ => match printable {
                    Some(c) => {
                        let changed = form.type_char(c);
                        (changed, changed)
                    }
                    None => (false, false),
                },
            };
            if changed {
                self.refresh_plan();
            }
            return consumed;
        }

        if let Some(Modal::SshForm(form)) = &mut self.modal {
            return match key.code {
                KeyCode::Backspace => form.backspace(),
                KeyCode::Left => {
                    form.prev_field();
                    true
                }
                KeyCode::Right => {
                    form.next_field();
                    true
                }
                _ => match printable {
                    Some(c) => form.type_char(c),
                    None => false,
                },
            };
        }

        if self.mode == Mode::Filter {
            match key.code {
                KeyCode::Backspace => {
                    self.filter.pop();
                    return true;
                }
                KeyCode::Enter => {
                    self.mode = Mode::Browse;
                    return true;
                }
                _ => {
                    if let Some(c) = printable {
                        self.filter.push(c);
                        return true;
                    }
                }
            }
        }
        false
    }

    fn on_action(&mut self, action: Action) {
        match action {
            Action::Quit => self.request_quit(),
            Action::Help => {
                self.modal = match self.modal {
                    Some(Modal::Help { .. }) => None,
                    _ => Some(Modal::Help { scroll: 0 }),
                }
            }
            Action::Palette => {
                self.modal = Some(Modal::Palette(Palette::new(&self.keymap)));
            }
            Action::Goto(screen) => self.goto_screen(screen),
            Action::NextScreen => self.goto_screen(self.screen.next()),
            Action::PrevScreen => self.goto_screen(self.screen.prev()),
            Action::Cancel => self.on_cancel(),
            Action::Submit => self.on_submit(),
            Action::FocusNext | Action::FocusPrev => self.on_focus_change(action),
            Action::Down => self.move_cursor(1),
            Action::Up => self.move_cursor(-1),
            Action::Top => self.set_cursor(0),
            Action::Bottom => self.set_cursor(self.table.len().saturating_sub(1)),
            Action::Open => {
                if !self.try_onboard() {
                    self.on_open();
                }
            }
            Action::Filter => {
                if self.modal.is_none() {
                    self.mode = Mode::Filter;
                }
            }
            Action::Refresh => self.load(false),
            Action::Add => {
                if !self.try_onboard() {
                    self.on_add();
                }
            }
            Action::Delete => self.on_delete(),
            Action::Test => self.on_test(),
            Action::RevealSecret => {
                self.reveal = REVEAL_TICKS;
                self.refresh_endpoint();
            }
            Action::TunnelToggle => self.toggle_tunnel(),

            // -- one per CLI subcommand ------------------------------------
            Action::Doctor => self.run_doctor(),
            Action::Discover => self.run_discover(),
            Action::Reset => self.confirm_reset(),
            Action::TargetAddLocal => self.register_local(),
            Action::TargetAddSsh => self.open_ssh_form(),
            Action::TargetSshConfig => self.show_ssh_config(),
            Action::TargetList => self.goto_screen(Screen::Targets),
            Action::TargetTest => {
                if self.require_screen(Screen::Targets, "Target을 선택한 뒤 다시 실행하세요")
                {
                    self.test_target();
                }
            }
            Action::TargetVerify => {
                if self.require_screen(Screen::Targets, "Target을 선택한 뒤 다시 실행하세요")
                {
                    self.verify_target_key();
                }
            }
            Action::TargetForget => {
                if self.require_screen(Screen::Targets, "Target을 선택한 뒤 다시 실행하세요")
                {
                    self.confirm_forget_target();
                }
            }
            Action::EngineEnsure => self.ensure_engine(),
            Action::EngineList => self.show_engine_list(),
            Action::EngineStart | Action::EngineStop | Action::EngineRestart => {
                self.engine_lifecycle(action)
            }
            Action::Logs => self.show_logs(),
            Action::EngineRemove => self.confirm_remove_engine(),
            Action::NewDatabase => {
                if !self.try_onboard() {
                    self.open_form(EngineKind::Postgres);
                }
            }
            Action::BucketCreate => {
                if !self.try_onboard() {
                    self.open_form(EngineKind::Minio);
                }
            }
            Action::DbList | Action::BucketList => self.goto_screen(Screen::Resources),
            Action::DbUrl => self.show_endpoint(ResourceKind::Database, false),
            Action::DbEnv => self.show_endpoint(ResourceKind::Database, true),
            Action::BucketUrl => self.show_endpoint(ResourceKind::Bucket, false),
            Action::BucketEnv => self.show_endpoint(ResourceKind::Bucket, true),
            Action::BucketEndpoint => self.show_bucket_endpoint(),
            Action::Copy => self.copy_url(),
            Action::CopyExpanded => self.copy_expanded(),
            Action::BucketCopyUrl => {
                if self.require_resource(ResourceKind::Bucket).is_some() {
                    self.copy_url();
                }
            }
            Action::BucketCopyEnv => {
                if self.require_resource(ResourceKind::Bucket).is_some() {
                    self.copy_expanded();
                }
            }
            Action::DbTest => {
                if self.require_resource(ResourceKind::Database).is_some() {
                    self.test_resource();
                }
            }
            Action::BucketTest => {
                if self.require_resource(ResourceKind::Bucket).is_some() {
                    self.test_resource();
                }
            }
            Action::DbDrop => {
                if self.require_resource(ResourceKind::Database).is_some() {
                    self.confirm_drop_resource();
                }
            }
            Action::BucketDrop => {
                if self.require_resource(ResourceKind::Bucket).is_some() {
                    self.confirm_drop_resource();
                }
            }
            Action::DbForget => self.confirm_forget_resource(ResourceKind::Database),
            Action::BucketForget => self.confirm_forget_resource(ResourceKind::Bucket),
            Action::RotatePassword => {
                if self.require_resource(ResourceKind::Database).is_some() {
                    self.rotate_secret();
                }
            }
            Action::BucketRotateKey => {
                if self.require_resource(ResourceKind::Bucket).is_some() {
                    self.rotate_secret();
                }
            }
            Action::Duplicate => self.duplicate_database(),
            Action::TunnelStart => self.tunnel_lifecycle(Action::TunnelStart),
            Action::TunnelStop => self.tunnel_lifecycle(Action::TunnelStop),
            Action::TunnelRestart => self.tunnel_lifecycle(Action::TunnelRestart),
            Action::TunnelStartAll => self.start_all_tunnels(),
            Action::TunnelStatus => {
                self.goto_screen(Screen::Tunnels);
                self.recheck_tunnels();
            }
            Action::Backup => self.run_backup(),
            Action::BackupList => self.goto_screen(Screen::Backups),
            Action::Restore => self.restore_backup(),
            Action::BackupVerify => {
                if self.require_screen(Screen::Backups, "백업을 선택한 뒤 다시 실행하세요")
                {
                    self.verify_backup();
                }
            }
        }
    }

    /// Selection-scoped commands need the screen that owns that selection.
    /// Switching *and* acting in one keystroke would act on the row the user
    /// last had here, which is not what they asked for — so switch, say so,
    /// and let them confirm with a second press.
    fn require_screen(&mut self, screen: Screen, why: &str) -> bool {
        if self.screen == screen {
            return true;
        }
        self.goto_screen(screen);
        self.notify(why, false);
        false
    }

    /// The selected resource, when it is of `kind`. Moves to the resources
    /// screen first when the user ran the command from somewhere else.
    fn require_resource(&mut self, kind: ResourceKind) -> Option<Resource> {
        if !self.require_screen(Screen::Resources, "리소스를 선택한 뒤 다시 실행하세요")
        {
            return None;
        }
        match self.selected_resource().cloned() {
            Some(resource) if resource.kind() == kind => Some(resource),
            Some(_) => {
                self.notify(
                    match kind {
                        ResourceKind::Database => "선택된 항목은 DB가 아닙니다",
                        ResourceKind::Bucket => "선택된 항목은 버킷이 아닙니다",
                    },
                    true,
                );
                None
            }
            None => {
                self.notify("선택된 리소스가 없습니다", true);
                None
            }
        }
    }

    fn on_cancel(&mut self) {
        if self.modal.is_some() {
            self.modal = None;
            return;
        }
        if self.mode == Mode::Filter {
            self.mode = Mode::Browse;
            self.filter.clear();
            return;
        }
        if !self.filter.is_empty() {
            self.filter.clear();
        }
    }

    fn on_focus_change(&mut self, action: Action) {
        match &mut self.modal {
            Some(Modal::Form(form)) => {
                if action == Action::FocusNext {
                    form.next_field();
                } else {
                    form.prev_field();
                }
            }
            Some(Modal::SshForm(form)) => {
                if action == Action::FocusNext {
                    form.next_field();
                } else {
                    form.prev_field();
                }
            }
            Some(Modal::Confirm(confirm)) => confirm.toggle_focus(),
            Some(_) => {}

            None => {
                let screen = if action == Action::FocusNext {
                    self.screen.next()
                } else {
                    self.screen.prev()
                };
                self.goto_screen(screen);
            }
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        match &mut self.modal {
            Some(Modal::Palette(palette)) => {
                palette.move_by(delta);
                return;
            }
            Some(Modal::Form(form)) => {
                if form.move_option(delta > 0) {
                    self.refresh_plan();
                }
                return;
            }
            Some(Modal::SshForm(form)) => {
                if delta > 0 {
                    form.next_field();
                } else {
                    form.prev_field();
                }
                return;
            }

            Some(Modal::Help { scroll }) => {
                *scroll = scroll.saturating_add_signed(delta as i16);
                return;
            }
            Some(Modal::Message(message)) => {
                message.scroll = message.scroll.saturating_add_signed(delta as i16);
                return;
            }
            Some(Modal::Confirm(_)) => return,
            Some(Modal::Detail { scroll, .. }) => {
                *scroll = scroll.saturating_add_signed(delta as i16);
                return;
            }
            None => {}
        }
        if self.focus == Focus::Detail {
            self.detail_scroll = self.detail_scroll.saturating_add_signed(delta as i16);
            return;
        }
        let count = self.table.len();
        if count == 0 {
            return;
        }
        let slot = screen_slot(self.screen);
        let next = self.cursor[slot] as isize + delta;
        self.cursor[slot] = next.clamp(0, count as isize - 1) as usize;
        self.on_selection_change();
    }

    fn set_cursor(&mut self, index: usize) {
        if self.modal.is_some() || self.table.is_empty() {
            return;
        }
        self.cursor[screen_slot(self.screen)] = index.min(self.table.len() - 1);
        self.on_selection_change();
    }

    fn on_selection_change(&mut self) {
        self.detail_scroll = 0;
        self.reveal = 0;
        self.endpoint_key = None;
    }

    fn goto_screen(&mut self, screen: Screen) {
        if self.modal.is_some() {
            return;
        }
        if self.setup_active() && screen != Screen::Resources {
            return;
        }
        self.screen = screen;
        self.focus = Focus::List;
        self.detail_scroll = 0;
        self.mode = Mode::Browse;
        self.filter.clear();
        self.refresh_endpoint();
    }

    fn on_open(&mut self) {
        if self.modal.is_some() {
            return;
        }
        if self.table.is_empty() {
            return;
        }
        let title = crate::tui::rows::screen_label(self.screen).to_string();
        self.modal = Some(Modal::Detail { title, scroll: 0 });
    }

    fn on_add(&mut self) {
        match self.screen {
            // The local machine is the one-keystroke case; anything else is a
            // remote host, which needs the form and its fingerprint gate.
            Screen::Targets => {
                if self.snapshot.targets.is_empty() {
                    self.register_local();
                } else {
                    self.open_ssh_form();
                }
            }
            Screen::Tunnels => self.start_all_tunnels(),
            _ => self.open_form(EngineKind::Postgres),
        }
    }

    // -- selection helpers ---------------------------------------------------

    fn source_index(&self) -> Option<usize> {
        self.table
            .source_index(self.cursor[screen_slot(self.screen)])
    }

    fn selected_resource(&self) -> Option<&Resource> {
        if self.screen != Screen::Resources {
            return None;
        }
        self.snapshot.resources.get(self.source_index()?)
    }

    fn selected_engine_status(&self) -> Option<&crate::core::model::EngineStatus> {
        let engine_id = &self.selected_resource()?.engine().id;
        self.snapshot
            .engines
            .iter()
            .find(|e| &e.engine.id == engine_id)
            .map(|e| &e.status)
    }

    fn refresh_endpoint(&mut self) {
        self.fill_endpoint(self.reveal > 0);
    }

    fn fill_endpoint(&mut self, with_secret: bool) {
        self.endpoint = Endpoint::default();
        let Some(resource) = self.selected_resource() else {
            return;
        };
        self.endpoint = match resource {
            Resource::Database(view) => {
                let info = if with_secret {
                    database::connection_info(&self.ctx, view)
                } else {
                    database::connection_preview(view)
                };
                match info {
                    Ok(info) => Endpoint {
                        url: Some(if with_secret {
                            info.url()
                        } else {
                            info.redacted_url()
                        }),
                        redacted_url: Some(info.redacted_url()),
                        env_block: with_secret.then(|| info.env_block()),
                        secret: info.password.clone(),
                        address: Some(format!("{}:{}", info.host, info.port)),
                        region: None,
                        note: None,
                    },
                    Err(e) => Endpoint {
                        note: Some(e.as_diagnostic().what),
                        ..Endpoint::default()
                    },
                }
            }
            Resource::Bucket(view) => {
                let info = if with_secret {
                    bucket::connection_info(&self.ctx, view)
                } else {
                    bucket::connection_preview(view)
                };
                match info {
                    Ok(info) => Endpoint {
                        url: Some(if with_secret {
                            info.url()
                        } else {
                            info.redacted_url()
                        }),
                        redacted_url: Some(info.redacted_url()),
                        env_block: with_secret.then(|| info.env_block()),
                        secret: info.secret_key.clone(),
                        address: Some(info.endpoint()),
                        region: Some(info.region.clone()),
                        note: None,
                    },
                    Err(e) => Endpoint {
                        note: Some(e.as_diagnostic().what),
                        ..Endpoint::default()
                    },
                }
            }
        };
    }

    // -- feedback ------------------------------------------------------------

    fn notify(&mut self, text: impl Into<String>, danger: bool) {
        self.toast = Some((
            Toast {
                text: text.into(),
                danger,
            },
            TOAST_TICKS,
        ));
    }

    fn fail(&mut self, error: &Error) {
        self.modal = Some(Modal::Message(Message::failure(&error.as_diagnostic())));
    }

    // -- jobs ----------------------------------------------------------------

    fn on_progress(&mut self, id: u64, progress: Progress) {
        // Mirror the create form's own step trace, so the plan panel streams
        // in place (PRD §7.5).
        if let (
            Some(Modal::Form(form)),
            Progress::Step {
                index,
                total,
                title,
            },
        ) = (&mut self.modal, &progress)
        {
            form.steps.push(format!("{index}/{total} {title}"));
        }
        self.jobs.apply(id, progress);
    }

    fn on_done(&mut self, id: u64, result: Result<Outcome>) {
        let job = self.jobs.take(id);
        let quiet = job.as_ref().is_some_and(|j| j.quiet);
        match result {
            Ok(Outcome::Snapshot(snapshot)) => {
                self.snapshot = *snapshot;
                self.endpoint_key = None;
                if self.pending_open_form && !self.snapshot.targets.is_empty() {
                    self.pending_open_form = false;
                    if self.modal.is_none() {
                        self.open_first_form();
                    }
                }
            }
            Ok(Outcome::Note(note)) => {
                self.notify(note, false);
                self.load(false);
            }
            Ok(Outcome::Report { title, body }) => {
                self.modal = Some(Modal::Message(Message::note(title, body)));
            }
            Ok(Outcome::FormPlan { epoch, plan }) => {
                if let Some(Modal::Form(form)) = &mut self.modal {
                    form.accept_plan(epoch, plan);
                }
            }
            Ok(Outcome::ConfirmPlan(plan)) => {
                if let Some(Modal::Confirm(confirm)) = &mut self.modal {
                    confirm.plan = *plan;
                }
            }
            Ok(Outcome::SshScanned {
                spec,
                fingerprint,
                key_type,
            }) => {
                let plan = crate::core::plan::Plan::new(format!(
                    "{}:{} 호스트 키 승인",
                    spec.host, spec.port
                ))
                .step_detailed(
                    crate::core::plan::StepKind::Verify,
                    format!("타입 {key_type}"),
                    fingerprint.clone(),
                )
                .step_detailed(
                    crate::core::plan::StepKind::New,
                    format!("Target `{}` 등록", spec.display_name),
                    "승인한 지문만 저장합니다.",
                )
                .warn("이 지문이 서버에서 직접 확인한 값과 같은지 비교하세요.");
                self.modal = Some(Modal::Confirm(Box::new(Confirm::new(
                    plan,
                    Intent::AddSshTarget { spec, fingerprint },
                ))));
            }

            Ok(Outcome::FormDone {
                title,
                body,
                copy_url,
                copy_env,
            }) => {
                let mut message = Message::note(title, body);
                if let (Some(url), Some(env)) = (copy_url, copy_env) {
                    message = message.with_copy(url, env);
                }
                self.modal = Some(Modal::Message(message));
                self.load(false);
            }
            Err(Error::Cancelled) => {
                self.pending_open_form = false;
                self.notify("작업을 취소했습니다", true);
            }
            Err(e) => {
                self.pending_open_form = false;
                if quiet {
                    self.notify(e.as_diagnostic().what, true);
                } else {
                    self.fail(&e);
                }
            }
        }
    }

    /// TUN-007 plus the first data load, as one background step so the first
    /// frame is drawn immediately (PRD §12.2).
    fn reconcile_and_load(&mut self) {
        self.jobs.spawn(
            "시작 점검",
            self.ctx.clone(),
            true,
            |ctx, _, _| async move {
                let corrected = tunnel::reconcile(&ctx).await?;
                let snapshot = data::load(&ctx, false).await?;
                let broken = corrected
                    .iter()
                    .filter(|s| s.status != TunnelStatus::Active)
                    .count();
                if broken > 0 {
                    // The snapshot is still what the screens need; the note
                    // rides along as a log line the status bar can show.
                    return Ok(Outcome::Report {
                        title: "터널 상태 정리".to_string(),
                        body: format!(
                            "이전에 실행 중이던 터널 {broken}건이 더 이상 살아 있지 않아 \
                             실패로 표시했습니다. Tunnels 화면에서 다시 시작하세요.\n\n\
                             등록된 리소스 {}건을 불러왔습니다.",
                            snapshot.resources.len()
                        ),
                    });
                }
                Ok(Outcome::Snapshot(Box::new(snapshot)))
            },
        );
        self.load(true);
    }

    fn load(&mut self, with_stats: bool) {
        self.jobs.spawn(
            "새로 고침",
            self.ctx.clone(),
            true,
            move |ctx, _, _| async move {
                Ok(Outcome::Snapshot(Box::new(
                    data::load(&ctx, with_stats).await?,
                )))
            },
        );
    }

    // -- create form ---------------------------------------------------------

    fn open_form(&mut self, engine: EngineKind) {
        if self.modal.is_some() {
            return;
        }
        let targets = target::list(&self.ctx).unwrap_or_default();
        let mut form = Form::new(targets, engine);
        form.first_run = self.snapshot.resources.is_empty();
        self.modal = Some(Modal::Form(Box::new(form)));
        self.refresh_plan();
    }

    /// Recompute the preview for the form's current values. Runs as a quiet
    /// job: a plan that cannot be computed yet is a hint, not an error.
    fn refresh_plan(&mut self) {
        let Some(Modal::Form(form)) = &mut self.modal else {
            return;
        };
        if !form.is_valid() {
            let epoch = form.epoch();
            form.accept_plan(epoch, Err(Error::Usage(String::new())));
            form.plan_error = None;
            return;
        }
        let epoch = form.invalidate_plan();
        let Some(target) = form.target().cloned() else {
            return;
        };
        let want_pg = form.want_postgres;
        let want_minio = form.want_minio;
        let pg_spec = form.engine_spec();
        let minio_spec = crate::core::engine::EngineSpec::new(EngineKind::Minio, "latest");
        let database_spec = form.database_spec();
        let bucket_spec = form.bucket_spec();

        self.jobs.spawn(
            "계획 계산",
            self.ctx.clone(),
            true,
            move |ctx, _, _| async move {
                let mut plan = crate::core::plan::Plan::new("생성");
                if want_pg {
                    let p = database::plan_create(&ctx, &target, &pg_spec, &database_spec).await?;
                    plan.steps.extend(p.steps);
                    plan.warnings.extend(p.warnings);
                }
                if want_minio {
                    let p = bucket::plan_create(&ctx, &target, &minio_spec, &bucket_spec).await?;
                    plan.steps.extend(p.steps);
                    plan.warnings.extend(p.warnings);
                }
                Ok(Outcome::FormPlan {
                    epoch,
                    plan: Ok(plan),
                })
            },
        );
    }

    fn on_submit(&mut self) {
        match self.modal.take() {
            Some(Modal::Form(form)) => self.submit_form(form),
            Some(Modal::SshForm(form)) => self.submit_ssh_form(form),
            Some(Modal::Confirm(confirm)) => self.submit_confirm(*confirm),
            Some(other) => self.modal = Some(other),
            None => {}
        }
    }

    /// Nothing is registered here. The scan runs, and the *user* approves the
    /// fingerprint in the modal that follows (TAR-005).
    fn submit_ssh_form(&mut self, mut form: Box<crate::tui::ssh_form::SshForm>) {
        if let Err(problem) = form.check() {
            self.modal = Some(Modal::SshForm(form));
            self.notify(problem, true);
            return;
        }
        let spec = form.spec();
        let host = spec.host.clone();
        let port = spec.port;
        form.scanning = true;
        self.modal = Some(Modal::SshForm(form));
        self.jobs.spawn(
            format!("`{host}` 호스트 키 조회"),
            self.ctx.clone(),
            false,
            move |_, _, _| async move {
                let keys = crate::core::ssh::scan_host_keys(&host, port).await?;
                let key = keys.first().ok_or_else(|| {
                    Error::NotFound(format!("{host}:{port}에서 호스트 키를 받지 못했습니다."))
                })?;
                Ok(Outcome::SshScanned {
                    spec,
                    fingerprint: key.fingerprint.clone(),
                    key_type: key.key_type.clone(),
                })
            },
        );
    }

    fn submit_form(&mut self, mut form: Box<Form>) {
        if !form.is_valid() {
            let problem = form
                .first_error()
                .unwrap_or_else(|| "입력을 확인하세요.".to_string());
            self.modal = Some(Modal::Form(form));
            self.notify(problem, true);
            return;
        }
        let Some(target) = form.target().cloned() else {
            self.modal = Some(Modal::Form(form));
            return;
        };
        let want_pg = form.want_postgres;
        let want_minio = form.want_minio;
        let pg_spec = form.engine_spec();
        let minio_spec = crate::core::engine::EngineSpec::new(EngineKind::Minio, "latest");
        let database_spec = form.database_spec();
        let bucket_spec = form.bucket_spec();
        let name = form.project.clone();

        form.running = true;
        form.steps.clear();
        self.modal = Some(Modal::Form(form));

        self.jobs.spawn(
            format!("`{name}` 생성"),
            self.ctx.clone(),
            false,
            move |ctx, reporter, cancel| async move {
                let mut titles = Vec::new();
                let mut body = String::new();
                let mut copy_url = None;
                let mut copy_env = None;
                if want_pg {
                    let created = database::create(
                        &ctx,
                        &target,
                        &pg_spec,
                        &database_spec,
                        &reporter,
                        &cancel,
                    )
                    .await?;
                    titles.push(created.database.database_name.clone());
                    body.push_str(&created.connection.redacted_url());
                    body.push_str("\n\n");
                    body.push_str(&created.engine.container_name);
                    copy_url = Some(created.connection.url());
                    copy_env = Some(created.connection.env_block());
                }
                if want_minio {
                    let created = bucket::create(
                        &ctx,
                        &target,
                        &minio_spec,
                        &bucket_spec,
                        &reporter,
                        &cancel,
                    )
                    .await?;
                    titles.push(created.bucket.bucket_name.clone());
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str(&created.connection.redacted_url());
                    body.push_str("\n\n");
                    body.push_str(&created.engine.container_name);
                    if copy_url.is_none() {
                        copy_url = Some(created.connection.url());
                        copy_env = Some(created.connection.env_block());
                    } else if let Some(env) = &mut copy_env {
                        env.push('\n');
                        env.push_str(&created.connection.env_block());
                    }
                }
                body.push_str("\n\n`y`로 URL을, `Y`로 .env 블록을 복사할 수 있습니다.");
                Ok(Outcome::FormDone {
                    title: format!("`{}` 생성 완료", titles.join(", ")),
                    body,
                    copy_url,
                    copy_env,
                })
            },
        );
    }

    // -- destructive paths ---------------------------------------------------

    fn on_delete(&mut self) {
        if self.modal.is_some() {
            return;
        }
        match self.screen {
            Screen::Resources => self.confirm_drop_resource(),
            Screen::Targets => self.confirm_forget_target(),
            Screen::Tunnels => self.confirm_stop_tunnel(),
            // Backup *records* have no core use case that removes them, and
            // the TUI must not invent behaviour the CLI cannot reach.
            Screen::Backups => self.notify(
                "백업 기록은 삭제할 수 없습니다. 파일은 백업 폴더에서 직접 지우세요.",
                false,
            ),
            _ => {}
        }
    }

    /// Opens the modal immediately with the plan that can be built locally,
    /// then replaces it with `core`'s full plan — which counts backups and
    /// inspects the server — when that arrives. Waiting for Docker before
    /// showing anything would make `x` feel broken.
    fn confirm_drop_resource(&mut self) {
        let Some(resource) = self.selected_resource().cloned() else {
            return;
        };
        self.modal = Some(Modal::Confirm(Box::new(Confirm::new(
            local_drop_plan(&resource),
            Intent::DropResource(resource.clone()),
        ))));

        self.jobs.spawn(
            "삭제 계획",
            self.ctx.clone(),
            true,
            move |ctx, _, _| async move {
                let plan = match &resource {
                    Resource::Database(view) => database::plan_drop(&ctx, view).await?,
                    Resource::Bucket(view) => bucket::plan_drop(&ctx, view).await?,
                };
                Ok(Outcome::ConfirmPlan(Box::new(plan)))
            },
        );
    }

    fn confirm_forget_target(&mut self) {
        let Some(index) = self.source_index() else {
            return;
        };
        let Some(overview) = self.snapshot.targets.get(index) else {
            return;
        };
        let target = overview.target.clone();
        let plan = crate::core::plan::Plan::new(format!("`{}` 등록 해제", target.display_name))
            .step_detailed(
                crate::core::plan::StepKind::Destroy,
                format!("Target {} 등록 정보 삭제", target.display_name),
                "서버의 컨테이너와 볼륨은 그대로 남습니다.",
            )
            .warn("등록된 엔진이 남아 있으면 거부됩니다. 먼저 엔진을 정리하세요.");
        self.modal = Some(Modal::Confirm(Box::new(Confirm::new(
            plan,
            Intent::ForgetTarget(target),
        ))));
    }

    fn confirm_stop_tunnel(&mut self) {
        let Some(index) = self.source_index() else {
            return;
        };
        let Some(view) = self.snapshot.tunnels.get(index) else {
            return;
        };
        let plan = crate::core::plan::Plan::new(format!("`{}` 터널 중지", view.resource_name))
            .step_detailed(
                crate::core::plan::StepKind::Destroy,
                format!(
                    "{}:{} 포워딩 중지",
                    view.session.local_host, view.session.local_port
                ),
                "이 포트를 쓰는 애플리케이션의 연결이 끊깁니다.",
            );
        self.modal = Some(Modal::Confirm(Box::new(Confirm::new(
            plan,
            Intent::StopTunnel(view.session.clone()),
        ))));
    }

    fn submit_confirm(&mut self, confirm: Confirm) {
        if confirm.focus == ConfirmFocus::Cancel {
            return;
        }
        if !confirm.armed() {
            let expected = confirm.required_name.clone().unwrap_or_default();
            self.modal = Some(Modal::Confirm(Box::new(confirm)));
            self.notify(format!("`{expected}`를 정확히 입력하세요"), true);
            return;
        }

        match confirm.intent {
            Intent::Quit => {
                self.jobs.cancel_all();
                self.quit = true;
            }
            Intent::Reset => {
                self.jobs.spawn(
                    "전체 초기화",
                    self.ctx.clone(),
                    false,
                    move |ctx, reporter, _| async move {
                        let report = engine::reset_all(&ctx, &reporter).await?;
                        Ok(Outcome::Note(format!(
                            "초기화했습니다. 엔진 {}개, Target {}개 삭제",
                            report.engines_removed, report.targets_removed
                        )))
                    },
                );
            }
            Intent::AddSshTarget { spec, fingerprint } => {
                let name = spec.display_name.clone();
                self.jobs.spawn(
                    format!("`{name}` 등록"),
                    self.ctx.clone(),
                    false,
                    move |ctx, _, _| async move {
                        let target = target::add_ssh(&ctx, &spec, &fingerprint).await?;
                        Ok(Outcome::Note(format!(
                            "Target `{}`을(를) 등록했습니다",
                            target.display_name
                        )))
                    },
                );
            }

            Intent::DropResource(resource) => {
                let name = resource.name().to_string();
                self.jobs.spawn(
                    format!("`{name}` 삭제"),
                    self.ctx.clone(),
                    false,
                    move |ctx, reporter, _| async move {
                        match &resource {
                            Resource::Database(view) => {
                                database::drop(&ctx, view, &reporter).await?
                            }
                            Resource::Bucket(view) => bucket::drop(&ctx, view, &reporter).await?,
                        }
                        Ok(Outcome::Note(format!("`{name}`을(를) 삭제했습니다")))
                    },
                );
            }
            Intent::ForgetResource(resource) => {
                let name = resource.name().to_string();
                let result = match &resource {
                    Resource::Database(view) => database::forget(&self.ctx, view),
                    Resource::Bucket(view) => bucket::forget(&self.ctx, view),
                };
                match result {
                    Ok(()) => {
                        self.notify(format!("`{name}` 등록을 해제했습니다"), false);
                        self.load(false);
                    }
                    Err(e) => self.fail(&e),
                }
            }
            Intent::ForgetTarget(target) => match target::forget(&self.ctx, &target) {
                Ok(()) => {
                    self.notify(
                        format!("`{}` 등록을 해제했습니다", target.display_name),
                        false,
                    );
                    self.load(false);
                }
                Err(e) => self.fail(&e),
            },
            Intent::RemoveEngine { engine, volume } => {
                let label = engine.container_name.clone();
                self.jobs.spawn(
                    format!("`{label}` 삭제"),
                    self.ctx.clone(),
                    false,
                    move |ctx, reporter, _| async move {
                        engine::remove(&ctx, &engine, volume, &reporter).await?;
                        Ok(Outcome::Note(format!("`{label}`을(를) 삭제했습니다")))
                    },
                );
            }
            Intent::StopTunnel(session) => {
                self.jobs.spawn(
                    "터널 중지",
                    self.ctx.clone(),
                    false,
                    move |ctx, _, _| async move {
                        tunnel::stop(&ctx, &session).await?;
                        Ok(Outcome::Note("터널을 중지했습니다".to_string()))
                    },
                );
            }
            Intent::RestoreBackup {
                record,
                resource,
                overwrite,
            } => {
                let Some(view) = resource.as_database().cloned() else {
                    self.notify("버킷 복원은 `linf backup restore`로 실행하세요", true);
                    return;
                };
                let path = record.path();
                self.jobs.spawn(
                    "복원",
                    self.ctx.clone(),
                    false,
                    move |ctx, reporter, cancel| async move {
                        backup::restore(&ctx, &path, &view, overwrite, &reporter, &cancel).await?;
                        Ok(Outcome::Note("복원을 완료했습니다".to_string()))
                    },
                );
            }
        }
    }

    fn request_quit(&mut self) {
        if self.modal.is_some() {
            self.modal = None;
            return;
        }
        if self.jobs.has_loud_work() {
            let titles: Vec<String> = self
                .jobs
                .foreground()
                .map(|job| vec![job.headline()])
                .unwrap_or_default();
            let plan = crate::core::plan::Plan::new("진행 중 작업이 있습니다").step_detailed(
                crate::core::plan::StepKind::Destroy,
                "실행 중인 작업 취소",
                titles.join(", "),
            );
            self.modal = Some(Modal::Confirm(Box::new(Confirm::new(plan, Intent::Quit))));
            return;
        }
        self.quit = true;
    }

    // -- resource operations -------------------------------------------------

    fn copy_url(&mut self) {
        if let Some(text) = self.message_copy(false) {
            self.copy(text, "URL");
            return;
        }
        let Some(text) = self.copy_target(false) else {
            return;
        };
        self.copy(text, "URL");
    }

    fn copy_expanded(&mut self) {
        if let Some(text) = self.message_copy(true) {
            self.copy(text, ".env 블록");
            return;
        }
        if self.screen == Screen::Activity {
            let Some(index) = self.source_index() else {
                return;
            };
            let text = rows::activity_diagnostics(&self.snapshot, index);
            self.copy(text, "진단 정보");
            return;
        }
        let Some(text) = self.copy_target(true) else {
            return;
        };
        self.copy(text, ".env 블록");
    }

    fn message_copy(&self, expanded: bool) -> Option<String> {
        let Modal::Message(message) = self.modal.as_ref()? else {
            return None;
        };
        if expanded {
            message.copy_env().map(str::to_string)
        } else {
            message.copy_url().map(str::to_string)
        }
    }

    fn copy_target(&mut self, expanded: bool) -> Option<String> {
        if self.screen != Screen::Resources {
            return None;
        }
        self.fill_endpoint(true);
        if let Some(note) = &self.endpoint.note {
            let note = note.clone();
            self.notify(note, true);
            return None;
        }
        if expanded {
            self.endpoint.env_block.clone()
        } else {
            self.endpoint.url.clone()
        }
    }

    fn copy(&mut self, text: String, what: &str) {
        if text.is_empty() {
            self.note_copy(&format!("복사할 {what}이(가) 없습니다"), true);
            return;
        }
        match clipboard::copy(&self.ctx.config.ui, &text, true) {
            Ok(outcome) => self.note_copy(&outcome.message(what), false),
            Err(e) => self.note_copy(&e.as_diagnostic().what, true),
        }
    }

    fn note_copy(&mut self, text: &str, danger: bool) {
        if let Some(Modal::Message(message)) = &mut self.modal {
            message.status = Some(text.to_string());
        }
        self.notify(text.to_string(), danger);
    }

    fn on_test(&mut self) {
        match self.screen {
            Screen::Resources => self.test_resource(),
            Screen::Targets => self.test_target(),
            Screen::Tunnels => self.recheck_tunnels(),
            Screen::Backups => self.verify_backup(),
            Screen::Doctor | Screen::Engines => self.load(true),
            Screen::Activity => {}
        }
    }

    fn test_resource(&mut self) {
        let Some(resource) = self.selected_resource().cloned() else {
            return;
        };
        let name = resource.name().to_string();
        self.jobs.spawn(
            format!("`{name}` 접속 테스트"),
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                match &resource {
                    Resource::Database(view) => database::test_connection(&ctx, view).await?,
                    Resource::Bucket(view) => bucket::test_connection(&ctx, view).await?,
                }
                Ok(Outcome::Note(format!("`{name}` 접속에 성공했습니다")))
            },
        );
    }

    fn test_target(&mut self) {
        let Some(index) = self.source_index() else {
            return;
        };
        let Some(overview) = self.snapshot.targets.get(index) else {
            return;
        };
        let target = overview.target.clone();
        self.jobs.spawn(
            format!("`{}` 진단", target.display_name),
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                let checks = target::test(&ctx, &target).await?;
                let body = checks
                    .iter()
                    .map(|c| {
                        let mark = if c.ok { "ok " } else { "!  " };
                        match &c.remedy {
                            Some(remedy) => {
                                format!("{mark}{:<20} {}\n      → {remedy}", c.name, c.detail)
                            }
                            None => format!("{mark}{:<20} {}", c.name, c.detail),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Outcome::Report {
                    title: format!("{} 진단", target.display_name),
                    body,
                })
            },
        );
    }

    fn recheck_tunnels(&mut self) {
        self.jobs.spawn(
            "터널 상태 확인",
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                let corrected = tunnel::reconcile(&ctx).await?;
                let broken = corrected
                    .iter()
                    .filter(|s| s.status != TunnelStatus::Active)
                    .count();
                Ok(Outcome::Note(match broken {
                    0 => "모든 터널이 살아 있습니다".to_string(),
                    n => format!("터널 {n}건을 실패로 표시했습니다"),
                }))
            },
        );
    }

    fn verify_backup(&mut self) {
        let Some(index) = self.source_index() else {
            return;
        };
        let Some(record) = self.snapshot.backups.get(index).cloned() else {
            return;
        };
        let name = record.file_name.clone();
        self.jobs.spawn(
            format!("`{name}` 검증"),
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                let ok = match record.resource_kind {
                    ResourceKind::Database => backup::verify(&ctx, &record).await?,
                    ResourceKind::Bucket => bucket::verify(&ctx, &record).await?,
                };
                Ok(Outcome::Note(if ok {
                    format!("`{name}` 체크섬이 일치합니다")
                } else {
                    format!("`{name}` 체크섬이 다릅니다")
                }))
            },
        );
    }

    fn toggle_tunnel(&mut self) {
        let (resource, session) = match self.screen {
            Screen::Tunnels => {
                let Some(index) = self.source_index() else {
                    return;
                };
                let Some(view) = self.snapshot.tunnels.get(index) else {
                    return;
                };
                let session = view.session.clone();
                let resource = self.snapshot.find_resource(&session.resource_id).cloned();
                (resource, Some(session))
            }
            _ => {
                let Some(resource) = self.selected_resource().cloned() else {
                    return;
                };
                let session = resource.tunnel().cloned();
                (Some(resource), session)
            }
        };

        // An active forward is stopped; anything else is (re)started.
        if let Some(session) = session.filter(|s| s.status == TunnelStatus::Active) {
            self.jobs.spawn(
                "터널 중지",
                self.ctx.clone(),
                false,
                move |ctx, _, _| async move {
                    tunnel::stop(&ctx, &session).await?;
                    Ok(Outcome::Note("터널을 중지했습니다".to_string()))
                },
            );
            return;
        }

        let Some(resource) = resource else {
            self.notify("이 터널의 리소스를 찾을 수 없습니다", true);
            return;
        };
        if !resource.target().is_remote() {
            self.notify("로컬 Target에는 터널이 필요하지 않습니다", false);
            return;
        }
        let name = resource.name().to_string();
        self.jobs.spawn(
            format!("`{name}` 터널 시작"),
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                let request = tunnel_target(&resource);
                let session =
                    tunnel::start(&ctx, &request, resource.engine(), resource.target()).await?;
                Ok(Outcome::Note(format!(
                    "127.0.0.1:{} 에서 `{name}`에 접속할 수 있습니다",
                    session.local_port
                )))
            },
        );
    }

    fn start_all_tunnels(&mut self) {
        let pending: Vec<Resource> = self
            .snapshot
            .resources
            .iter()
            .filter(|r| {
                r.target().is_remote()
                    && !r.tunnel().is_some_and(|t| t.status == TunnelStatus::Active)
            })
            .cloned()
            .collect();
        if pending.is_empty() {
            self.notify("시작할 터널이 없습니다", false);
            return;
        }
        self.jobs.spawn(
            "모든 터널 시작",
            self.ctx.clone(),
            false,
            move |ctx, reporter, cancel| async move {
                let total = pending.len();
                let mut started = 0usize;
                let mut failed = Vec::new();
                for (i, resource) in pending.iter().enumerate() {
                    cancel.check()?;
                    reporter.step(i + 1, total, format!("`{}` 터널", resource.name()));
                    let request = tunnel_target(resource);
                    match tunnel::start(&ctx, &request, resource.engine(), resource.target()).await
                    {
                        Ok(_) => started += 1,
                        Err(e) => {
                            failed.push(format!("{}: {}", resource.name(), e.as_diagnostic().what))
                        }
                    }
                    reporter.step_done(i + 1);
                }
                if failed.is_empty() {
                    Ok(Outcome::Note(format!("터널 {started}건을 시작했습니다")))
                } else {
                    Ok(Outcome::Report {
                        title: format!("터널 {started}/{total}건 시작"),
                        body: failed.join("\n"),
                    })
                }
            },
        );
    }

    fn run_backup(&mut self) {
        let Some(resource) = self.selected_resource().cloned() else {
            return;
        };
        let out_dir = self.ctx.backup_dir();
        let name = resource.name().to_string();
        self.jobs.spawn(
            format!("`{name}` 백업"),
            self.ctx.clone(),
            false,
            move |ctx, reporter, cancel| async move {
                let record = match &resource {
                    Resource::Database(view) => {
                        backup::run(
                            &ctx,
                            view,
                            &out_dir,
                            crate::core::model::BackupFormat::Custom,
                            &reporter,
                            &cancel,
                        )
                        .await?
                    }
                    Resource::Bucket(view) => {
                        bucket::backup(&ctx, view, &out_dir, &reporter, &cancel).await?
                    }
                };
                Ok(Outcome::Note(format!(
                    "{} ({})",
                    record.file_name,
                    crate::core::util::human_bytes(record.size)
                )))
            },
        );
    }

    /// Restore needs a file *and* a destination, so the resources screen sends
    /// the user to the backups screen where the file is the selection.
    fn restore_backup(&mut self) {
        if self.screen != Screen::Backups {
            self.screen = Screen::Backups;
            self.notify("복원할 백업을 선택한 뒤 다시 `R`을 누르세요", false);
            return;
        }
        let Some(index) = self.source_index() else {
            return;
        };
        let Some(record) = self.snapshot.backups.get(index).cloned() else {
            return;
        };
        let Some(resource) = self.snapshot.find_resource(&record.resource_id).cloned() else {
            self.notify("이 백업의 대상 리소스가 등록되어 있지 않습니다", true);
            return;
        };
        let plan = crate::core::plan::Plan::new(format!("`{}` 복원", record.file_name))
            .step_detailed(
                crate::core::plan::StepKind::Destroy,
                format!("{} 에 복원", resource.name()),
                "기존 객체와 이름이 겹치면 덮어씁니다.",
            )
            .warn("복원은 되돌릴 수 없습니다. 필요하면 먼저 백업하세요.");
        self.modal = Some(Modal::Confirm(Box::new(Confirm::new(
            plan,
            Intent::RestoreBackup {
                record,
                resource,
                overwrite: true,
            },
        ))));
    }

    fn rotate_secret(&mut self) {
        let Some(resource) = self.selected_resource().cloned() else {
            return;
        };
        let name = resource.name().to_string();
        self.jobs.spawn(
            format!("`{name}` 비밀 값 교체"),
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                match &resource {
                    Resource::Database(view) => {
                        let info = database::rotate_password(&ctx, view).await?;
                        Ok(Outcome::Report {
                            title: format!("`{name}` 비밀번호 교체"),
                            body: format!("{}\n\n`y`로 새 URL을 복사하세요.", info.redacted_url()),
                        })
                    }
                    Resource::Bucket(view) => {
                        let info = bucket::rotate_key(&ctx, view).await?;
                        Ok(Outcome::Report {
                            title: format!("`{name}` 액세스 키 교체"),
                            body: format!(
                                "{}\n\n`Y`로 새 .env 블록을 복사하세요.",
                                info.redacted_url()
                            ),
                        })
                    }
                }
            },
        );
    }

    fn show_logs(&mut self) {
        let instance = match self.screen {
            Screen::Resources => self.selected_resource().map(|r| r.engine().clone()),
            Screen::Targets => {
                let index = self.source_index();
                index
                    .and_then(|i| self.snapshot.targets.get(i))
                    .and_then(|overview| {
                        self.snapshot
                            .engines_for_target(&overview.target.id)
                            .first()
                            .map(|e| e.engine.clone())
                    })
            }
            _ => None,
        };
        let Some(instance) = instance else {
            self.notify("로그를 볼 엔진이 없습니다", true);
            return;
        };
        let label = instance.container_name.clone();
        self.jobs.spawn(
            format!("`{label}` 로그"),
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                let body = engine::logs(&ctx, &instance, LOG_TAIL).await?;
                Ok(Outcome::Report { title: label, body })
            },
        );
    }

    /// The engine the current selection belongs to: a resource's engine on the
    /// resources screen, the target's first engine on the targets screen.
    fn selected_engine(&self) -> Option<crate::core::model::EngineInstance> {
        match self.screen {
            Screen::Targets => {
                let overview = self.snapshot.targets.get(self.source_index()?)?;
                self.snapshot
                    .engines_for_target(&overview.target.id)
                    .first()
                    .map(|e| e.engine.clone())
            }
            _ => self.selected_resource().map(|r| r.engine().clone()),
        }
    }

    fn engine_lifecycle(&mut self, action: Action) {
        let Some(instance) = self.selected_engine() else {
            self.notify("엔진을 선택할 수 없습니다", true);
            return;
        };
        let label = instance.container_name.clone();
        self.jobs.spawn(
            format!("`{label}` {}", action.label()),
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                match action {
                    Action::EngineStop => engine::stop(&ctx, &instance).await?,
                    Action::EngineRestart => engine::restart(&ctx, &instance).await?,
                    _ => engine::start(&ctx, &instance).await?,
                }
                Ok(Outcome::Note(format!("`{label}` {}", action.label())))
            },
        );
    }

    // -- commands added for CLI parity (PRD §7.10) --------------------------

    fn run_doctor(&mut self) {
        self.jobs.spawn(
            "환경 진단",
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                let checks = crate::core::doctor::run(&ctx).await?;
                let body = checks
                    .iter()
                    .map(|c| {
                        let mark = if c.ok { "ok " } else { "!  " };
                        match &c.remedy {
                            Some(remedy) => {
                                format!("{mark}{:<22} {}\n      → {remedy}", c.name, c.detail)
                            }
                            None => format!("{mark}{:<22} {}", c.name, c.detail),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Outcome::Report {
                    title: "환경 진단".to_string(),
                    body,
                })
            },
        );
    }

    /// MIG-001: read-only. Nothing here can change a container it did not make.
    fn run_discover(&mut self) {
        let targets: Vec<crate::core::model::Target> = self
            .snapshot
            .targets
            .iter()
            .filter(|o| o.reachable)
            .map(|o| o.target.clone())
            .collect();
        if targets.is_empty() {
            self.notify("탐색할 수 있는 Target이 없습니다", true);
            return;
        }
        self.jobs.spawn(
            "미관리 컨테이너 탐색",
            self.ctx.clone(),
            false,
            move |ctx, _, _| async move {
                let mut body = String::new();
                for target in &targets {
                    let found = crate::core::discovery::foreign_containers(&ctx, target).await?;
                    body.push_str(&format!("{} ({}건)\n", target.display_name, found.len()));
                    for c in &found {
                        body.push_str(&format!(
                            "  {:<24} {:<28} {:<10} {}\n",
                            c.name, c.image, c.state, c.ports
                        ));
                    }
                    body.push('\n');
                }
                body.push_str("읽기 전용 목록입니다. local-infra는 이 리소스를 변경하지 않습니다.");
                Ok(Outcome::Report {
                    title: "미관리 컨테이너".to_string(),
                    body,
                })
            },
        );
    }

    fn confirm_reset(&mut self) {
        use crate::core::plan::{Plan, StepKind};
        let mut plan = Plan::new("모든 등록과 관리 컨테이너를 삭제합니다");
        if self.snapshot.engines.is_empty() {
            plan = plan.step(StepKind::Verify, "등록된 엔진 없음");
        }
        for overview in &self.snapshot.engines {
            plan = plan.step_detailed(
                StepKind::Destroy,
                format!("{} 삭제", overview.engine.container_name),
                format!("볼륨 {} 포함", overview.engine.volume_name),
            );
        }
        plan = plan
            .warn("이 앱이 만든 PostgreSQL / MinIO 데이터가 영구 삭제됩니다.")
            .warn("등록된 Target, DB, 버킷, 터널 기록도 함께 지웁니다.");
        self.modal = Some(Modal::Confirm(Box::new(Confirm::new(plan, Intent::Reset))));
    }

    fn open_ssh_form(&mut self) {
        if self.modal.is_some() {
            return;
        }
        self.modal = Some(Modal::SshForm(Box::default()));
    }

    fn show_ssh_config(&mut self) {
        let body = match crate::core::ssh::config_hosts() {
            Ok(hosts) if hosts.is_empty() => {
                "`~/.ssh/config`에 등록된 호스트가 없습니다.".to_string()
            }
            Ok(hosts) => hosts
                .iter()
                .map(|h| {
                    format!(
                        "{:<20} {:<24} {:<12} {}",
                        h.alias,
                        h.host_name.clone().unwrap_or_default(),
                        h.user.clone().unwrap_or_default(),
                        h.port.map(|p| p.to_string()).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(e) => {
                self.fail(&e);
                return;
            }
        };
        self.modal = Some(Modal::Message(Message::note("ssh config 호스트", body)));
    }

    /// TAR-005's read-only half: show what the host is offering right now.
    fn verify_target_key(&mut self) {
        let Some(index) = self.source_index() else {
            return;
        };
        let Some(overview) = self.snapshot.targets.get(index) else {
            return;
        };
        let target = overview.target.clone();
        let Some(host) = target.host.clone() else {
            self.notify("로컬 Target에는 호스트 키가 없습니다", false);
            return;
        };
        let port = target.ssh_port.unwrap_or(22);
        self.jobs.spawn(
            format!("`{host}` 호스트 키"),
            self.ctx.clone(),
            false,
            move |_, _, _| async move {
                let keys = crate::core::ssh::scan_host_keys(&host, port).await?;
                let body = keys
                    .iter()
                    .map(|k| format!("{:<12} {}", k.key_type, k.fingerprint))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Outcome::Report {
                    title: format!("{host}:{port} 호스트 키"),
                    body: format!(
                        "{body}\n\n등록된 지문과 같은지 직접 비교하세요. \
                         이 화면은 조회만 하고 아무것도 저장하지 않습니다."
                    ),
                })
            },
        );
    }

    fn ensure_engine(&mut self) {
        let (target, spec) = match self.screen {
            Screen::Targets => {
                let Some(overview) = self
                    .source_index()
                    .and_then(|i| self.snapshot.targets.get(i))
                else {
                    self.notify("Target을 선택하세요", true);
                    return;
                };
                (
                    overview.target.clone(),
                    engine::EngineSpec::postgres(EngineKind::Postgres.default_major_version()),
                )
            }
            _ => {
                let Some(resource) = self.selected_resource().cloned() else {
                    self.notify("Targets 화면에서 Target을 선택한 뒤 다시 실행하세요", false);
                    return;
                };
                let e = resource.engine();
                (
                    resource.target().clone(),
                    engine::EngineSpec::new(e.engine, &e.major_version),
                )
            }
        };
        let label = format!("{} {}", spec.engine.as_str(), spec.major_version);
        self.jobs.spawn(
            format!("`{label}` 엔진 준비"),
            self.ctx.clone(),
            false,
            move |ctx, reporter, cancel| async move {
                let instance = engine::ensure(&ctx, &target, &spec, &reporter, &cancel).await?;
                Ok(Outcome::Note(format!(
                    "`{}` 컨테이너를 사용할 수 있습니다",
                    instance.container_name
                )))
            },
        );
    }

    fn show_engine_list(&mut self) {
        if self.snapshot.engines.is_empty() {
            self.notify("등록된 엔진이 없습니다", false);
            return;
        }
        let body = self
            .snapshot
            .engines
            .iter()
            .map(|o| {
                format!(
                    "{:<20} {:<16} {:<10} {}:{}  리소스 {}건",
                    o.target.display_name,
                    o.engine.label(),
                    o.status.state,
                    o.engine.bind_address,
                    o.engine.host_port,
                    o.database_count
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.modal = Some(Modal::Message(Message::note("엔진 목록", body)));
    }

    fn confirm_remove_engine(&mut self) {
        let Some(engine_row) = self.selected_engine() else {
            self.notify("삭제할 엔진을 선택할 수 없습니다", true);
            return;
        };
        let resources = self.snapshot.resource_count(&engine_row.id);
        let plan = crate::core::plan::Plan::new(format!(
            "`{}` 엔진 컨테이너 삭제",
            engine_row.container_name
        ))
        .step_detailed(
            crate::core::plan::StepKind::Destroy,
            format!("컨테이너 {} 삭제", engine_row.container_name),
            format!("이 엔진의 리소스 {resources}건이 중단됩니다"),
        )
        .warn("볼륨과 등록 정보는 남습니다. 다시 만들면 데이터가 살아납니다.");
        self.modal = Some(Modal::Confirm(Box::new(Confirm::new(
            plan,
            Intent::RemoveEngine {
                engine: engine_row,
                volume: false,
            },
        ))));
    }

    /// `db url` / `db env` / `bucket url` / `bucket env`: the value on screen,
    /// with `y`/`Y` wired to the clipboard.
    fn show_endpoint(&mut self, kind: ResourceKind, expanded: bool) {
        if self.require_resource(kind).is_none() {
            return;
        }
        self.fill_endpoint(true);
        if let Some(note) = self.endpoint.note.clone() {
            self.notify(note, true);
            return;
        }
        let (Some(url), Some(env)) = (self.endpoint.url.clone(), self.endpoint.env_block.clone())
        else {
            self.notify("접속 정보를 만들 수 없습니다", true);
            return;
        };
        let title = match (kind, expanded) {
            (ResourceKind::Database, false) => "DB 접속 URL",
            (ResourceKind::Database, true) => "DB env 블록",
            (ResourceKind::Bucket, false) => "버킷 접속 문자열",
            (ResourceKind::Bucket, true) => "버킷 env 블록",
        };
        let body = if expanded { env.clone() } else { url.clone() };
        self.modal = Some(Modal::Message(
            Message::note(title, body).with_copy(url, env),
        ));
    }

    fn show_bucket_endpoint(&mut self) {
        if self.require_resource(ResourceKind::Bucket).is_none() {
            return;
        }
        self.fill_endpoint(false);
        match self.endpoint.address.clone() {
            Some(address) => {
                self.modal = Some(Modal::Message(Message::note("버킷 엔드포인트", address)))
            }
            None => self.notify(
                self.endpoint
                    .note
                    .clone()
                    .unwrap_or_else(|| "엔드포인트를 알 수 없습니다".to_string()),
                true,
            ),
        }
    }

    /// DB-007 / the bucket twin: drop the registration, keep the server data.
    fn confirm_forget_resource(&mut self, kind: ResourceKind) {
        let Some(resource) = self.require_resource(kind) else {
            return;
        };
        let plan = crate::core::plan::Plan::new(format!("`{}` 등록 해제", resource.name()))
            .step_detailed(
                crate::core::plan::StepKind::Destroy,
                "앱의 등록 정보와 저장된 비밀 값 삭제",
                "서버의 DB/버킷과 계정은 그대로 남습니다.",
            );
        self.modal = Some(Modal::Confirm(Box::new(Confirm::new(
            plan,
            Intent::ForgetResource(resource),
        ))));
    }

    /// `db duplicate` needs a name the CLI takes as an argument; here it is
    /// derived and shown in the plan, so the user approves the exact name.
    fn duplicate_database(&mut self) {
        let Some(resource) = self.require_resource(ResourceKind::Database) else {
            return;
        };
        let Some(view) = resource.as_database().cloned() else {
            return;
        };
        let taken: Vec<String> = self
            .snapshot
            .resources
            .iter()
            .map(|r| r.name().to_string())
            .collect();
        let source = view.database.database_name.clone();
        let mut new_name = format!("{source}_copy");
        let mut n = 2;
        while taken.contains(&new_name) {
            new_name = format!("{source}_copy{n}");
            n += 1;
        }
        if crate::core::database::validate_new_names(&new_name, &view.database.username).is_err() {
            self.notify(
                format!("`{new_name}`은(는) 사용할 수 없는 DB명입니다. CLI에서 이름을 지정하세요"),
                true,
            );
            return;
        }
        let label = new_name.clone();
        self.jobs.spawn(
            format!("`{source}` 복제"),
            self.ctx.clone(),
            false,
            move |ctx, reporter, _| async move {
                let created = database::duplicate(&ctx, &view, &new_name, &reporter).await?;
                Ok(Outcome::Note(format!(
                    "`{}`을(를) 만들었습니다",
                    created.database.database_name
                )))
            },
        );
        self.notify(format!("`{label}`(으)로 복제합니다"), false);
    }

    /// Explicit start/stop/restart, as opposed to `t`'s toggle.
    fn tunnel_lifecycle(&mut self, action: Action) {
        let Some(resource) = self.selected_resource().cloned() else {
            self.notify("리소스를 선택하세요", true);
            return;
        };
        if !resource.target().is_remote() {
            self.notify("로컬 Target에는 터널이 필요하지 않습니다", false);
            return;
        }
        let session = resource.tunnel().cloned();
        let name = resource.name().to_string();
        match action {
            Action::TunnelStop => {
                let Some(session) = session.filter(|s| s.status == TunnelStatus::Active) else {
                    self.notify("실행 중인 터널이 없습니다", false);
                    return;
                };
                self.jobs.spawn(
                    format!("`{name}` 터널 중지"),
                    self.ctx.clone(),
                    false,
                    move |ctx, _, _| async move {
                        tunnel::stop(&ctx, &session).await?;
                        Ok(Outcome::Note("터널을 중지했습니다".to_string()))
                    },
                );
            }
            _ => {
                let restart = action == Action::TunnelRestart;
                self.jobs.spawn(
                    format!("`{name}` 터널 {}", if restart { "재연결" } else { "시작" }),
                    self.ctx.clone(),
                    false,
                    move |ctx, _, _| async move {
                        let request = tunnel_target(&resource);
                        if let (true, Some(session)) = (restart, resource.tunnel()) {
                            let _ = tunnel::stop(&ctx, session).await;
                        }
                        let session =
                            tunnel::start(&ctx, &request, resource.engine(), resource.target())
                                .await?;
                        Ok(Outcome::Note(format!(
                            "127.0.0.1:{} 에서 `{name}`에 접속할 수 있습니다",
                            session.local_port
                        )))
                    },
                );
            }
        }
    }
}

/// The plan a confirmation can show without touching Docker. The full plan
/// (which counts backups and inspects the server) replaces it when it arrives.
fn local_drop_plan(resource: &Resource) -> crate::core::plan::Plan {
    use crate::core::plan::{Plan, StepKind};
    let plan = Plan::new(format!("`{}` 삭제", resource.name()));
    match resource.kind() {
        ResourceKind::Database => plan
            .step_detailed(
                StepKind::Destroy,
                format!("DB {} 삭제", resource.name()),
                format!("계정 {} 도 함께 제거됩니다.", resource.principal()),
            )
            .warn("이 작업은 되돌릴 수 없습니다. 필요하면 먼저 `b`로 백업하세요."),
        ResourceKind::Bucket => plan
            .step_detailed(
                StepKind::Destroy,
                format!("버킷 {} 삭제", resource.name()),
                "모든 오브젝트, 전용 액세스 키와 정책이 함께 제거됩니다.",
            )
            .warn("이 작업은 되돌릴 수 없습니다. 필요하면 먼저 `b`로 백업하세요."),
    }
}

fn tunnel_target(resource: &Resource) -> tunnel::TunnelTarget {
    match resource {
        Resource::Database(view) => tunnel::TunnelTarget::database(&view.database),
        Resource::Bucket(view) => tunnel::TunnelTarget::bucket(&view.bucket),
    }
}

fn screen_slot(screen: Screen) -> usize {
    Screen::ALL
        .iter()
        .position(|s| *s == screen)
        .unwrap_or_default()
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    Chord::from_event(key) == Chord::ctrl('c')
}

/// A printable character, or `None` for control chords the keymap should see.
fn printable(key: KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if !c.is_control() => Some(c),
        _ => None,
    }
}

/// `TableState` is not `Clone`, so the offset is carried across frames by hand.
fn view_state(previous: &TableState) -> TableState {
    TableState::default()
        .with_offset(previous.offset())
        .with_selected(previous.selected())
}

/// Read terminal events on their own thread and feed them to the loop.
///
/// `poll` with a timeout rather than a blocking `read` so the thread exits
/// promptly when the app does: a thread left blocked on stdin would swallow the
/// user's next shell command.
fn spawn_reader(stop: Arc<AtomicBool>) -> tokio::sync::mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
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
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_screen_has_its_own_cursor_slot() {
        let slots: Vec<usize> = Screen::ALL.iter().map(|s| screen_slot(*s)).collect();
        assert_eq!(slots, (0..Screen::ALL.len()).collect::<Vec<_>>());
    }

    #[test]
    fn ctrl_c_is_recognised_in_either_letter_case() {
        assert!(is_ctrl_c(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(is_ctrl_c(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_ctrl_c(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn control_chords_are_never_treated_as_text() {
        assert_eq!(
            printable(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some('a')
        );
        assert_eq!(
            printable(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some('A')
        );
        assert_eq!(
            printable(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            printable(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn a_local_drop_plan_is_destructive_and_names_what_goes_with_it() {
        use crate::tui::data::fixture;
        let local = fixture::local_target();

        let db = local_drop_plan(&fixture::database(&local, "letsbid_dev", None));
        assert!(db.is_destructive());
        assert!(db.render().contains("letsbid_user"));

        let bucket = local_drop_plan(&fixture::bucket_resource(&local, "letsbid-assets"));
        assert!(bucket.is_destructive());
        assert!(bucket.render().contains("정책"));
    }

    #[test]
    fn the_table_state_offset_survives_a_frame() {
        let previous = TableState::default().with_offset(12).with_selected(Some(3));
        let next = view_state(&previous);
        assert_eq!(next.offset(), 12);
        assert_eq!(next.selected(), Some(3));
    }
}

/// End-to-end key dispatch against a real (temporary) [`Ctx`], rendered into a
/// `TestBackend`.
///
/// This covers the part of the app the pure modules cannot: that a keypress
/// reaches the right state change and that the resulting frame draws. It needs
/// no Docker, no network and no TTY — the key sequences chosen here never
/// spawn a job, so nothing reaches out to the host.
#[cfg(test)]
mod smoke {
    use super::*;
    use crate::tui::data::fixture;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Mutex;

    /// `LINF_STATE_DIR` is process-global, so contexts are built one at a time.
    static STATE_DIR: Mutex<()> = Mutex::new(());

    /// A context rooted in a throwaway directory, with secrets turned off so
    /// the OS keyring is never touched.
    fn temp_ctx() -> (Ctx, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[secrets]\nmode = \"none\"\n\n[ui]\nascii = true\n",
        )
        .expect("config");

        let guard = STATE_DIR.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LINF_STATE_DIR", dir.path());
        let ctx = Ctx::open(Origin::Tui).expect("context opens in a temp dir");
        drop(guard);
        (ctx, dir)
    }

    fn app() -> (App, tempfile::TempDir) {
        let (ctx, dir) = temp_ctx();
        let mut app = App::new(Arc::new(ctx));
        app.snapshot = fixture::snapshot();
        app.rebuild_table();
        (app, dir)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        app.rebuild_table();
    }

    fn key(app: &mut App, c: char) {
        press(app, KeyCode::Char(c));
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, c);
        }
    }

    /// Draw the current state; a panic here is a rendering bug.
    fn frame(app: &mut App) -> String {
        frame_at(app, 110, 30)
    }

    /// A double-width glyph fills two cells, so the reader advances by display
    /// width — otherwise Korean words come back with spaces inside them.
    fn frame_at(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
        let mut area = ratatui::layout::Rect::default();
        terminal
            .draw(|frame| {
                area = frame.area();
                let view = app.view();
                let mut state = view_state(&app.table_state);
                render::draw(frame, &view, &mut state);
                app.table_state = state;
            })
            .expect("draw");
        app.hits = hit::compute(&app.view(), area, app.table_state.offset());
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                out.push_str(symbol);
                x += crate::core::util::display_cols(symbol).max(1) as u16;
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn number_keys_switch_screens_and_every_screen_draws() {
        let (mut app, _dir) = app();
        for screen in Screen::ALL {
            key(&mut app, screen.digit());
            assert_eq!(app.screen, screen);
            let text = frame(&mut app);
            assert!(
                text.contains(rows::screen_label(screen)),
                "{screen:?} did not draw its own title"
            );
        }
    }

    #[test]
    fn arrows_cycle_screens() {
        let (mut app, _dir) = app();
        assert_eq!(app.screen, Screen::Resources, "resources is home");
        press(&mut app, KeyCode::Right);
        assert_eq!(app.screen, Screen::Engines);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.screen, Screen::Resources);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.screen, Screen::Doctor, "and it wraps");
    }

    #[test]
    fn enter_opens_a_detail_popup_and_esc_closes_it() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.modal, Some(Modal::Detail { .. })));
        let text = frame(&mut app);
        assert!(
            text.contains("상세") || text.contains("Resources"),
            "{text}"
        );
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    #[test]
    fn the_cursor_walks_the_resource_list_and_each_screen_remembers_its_own() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        assert_eq!(app.cursor[screen_slot(Screen::Resources)], 0);

        key(&mut app, 'j');
        assert_eq!(app.cursor[screen_slot(Screen::Resources)], 1);
        key(&mut app, 'G');
        assert_eq!(app.cursor[screen_slot(Screen::Resources)], 2);
        key(&mut app, 'j');
        assert_eq!(app.cursor[screen_slot(Screen::Resources)], 2, "clamped");
        key(&mut app, 'g');
        assert_eq!(app.cursor[screen_slot(Screen::Resources)], 0);

        key(&mut app, 'j');
        key(&mut app, '6');
        assert_eq!(app.cursor[screen_slot(Screen::Tunnels)], 0);
        key(&mut app, '1');
        assert_eq!(app.cursor[screen_slot(Screen::Resources)], 1);
    }

    #[test]
    fn the_filter_line_narrows_the_table_and_esc_restores_it() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        let all = app.table.len();

        key(&mut app, '/');
        assert_eq!(app.mode, Mode::Filter);
        typed(&mut app, "bucket");
        assert_eq!(app.filter, "bucket");
        assert_eq!(app.table.len(), 1);
        assert!(frame(&mut app).contains("letsbid-dev-assets"));

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.filter.is_empty());
        assert_eq!(app.table.len(), all);
    }

    #[test]
    fn a_filter_query_containing_q_does_not_quit() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        key(&mut app, '/');
        typed(&mut app, "q");
        assert!(!app.quit, "`q` inside the filter line is text, not a verb");
        assert_eq!(app.filter, "q");
    }

    #[test]
    fn help_and_the_palette_open_and_close_on_their_own_keys() {
        let (mut app, _dir) = app();
        key(&mut app, '?');
        assert!(matches!(app.modal, Some(Modal::Help { .. })));
        assert!(frame(&mut app).contains("도움말"));
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());

        key(&mut app, ':');
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        typed(&mut app, "tunnel");
        let text = frame(&mut app);
        assert!(text.contains("tunnel.toggle"));
        assert!(!text.contains("backup.run"));
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    #[test]
    fn the_palette_runs_the_action_it_has_selected() {
        let (mut app, _dir) = app();
        key(&mut app, ':');
        typed(&mut app, "goto.doctor");
        press(&mut app, KeyCode::Enter);
        assert!(app.modal.is_none());
        assert_eq!(app.screen, Screen::Doctor);
    }

    /// The whole point of the parity work: a command with no key of its own
    /// is still reachable, and it opens the surface it promises.
    #[test]
    fn the_palette_opens_the_ssh_target_form() {
        let (mut app, _dir) = app();
        key(&mut app, ':');
        typed(&mut app, "target.add-ssh");
        press(&mut app, KeyCode::Enter);
        let Some(Modal::SshForm(form)) = &app.modal else {
            panic!("the ssh form did not open: {:?}", app.modal);
        };
        assert!(!form.is_valid(), "an empty form must not look ready");

        typed(&mut app, "dev-vps");
        press(&mut app, KeyCode::Tab);
        typed(&mut app, "vps.example.com");
        let Some(Modal::SshForm(form)) = &app.modal else {
            panic!("the form closed while typing");
        };
        assert!(form.is_valid());
        let spec = form.spec();
        assert_eq!(spec.display_name, "dev-vps");
        assert_eq!(spec.host, "vps.example.com");

        let text = frame(&mut app);
        assert!(text.contains("dev-vps"), "{text}");
        assert!(text.contains("호스트 키"), "{text}");
    }

    #[test]
    fn an_invalid_ssh_form_refuses_to_submit_and_says_why() {
        let (mut app, _dir) = app();
        app.modal = Some(Modal::SshForm(Box::default()));
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(
            matches!(app.modal, Some(Modal::SshForm(_))),
            "the form stays open"
        );
        assert!(app.toast.is_some(), "the refusal is explained");
    }

    #[test]
    fn a_on_the_targets_screen_opens_the_ssh_form() {
        let (mut app, _dir) = app();
        key(&mut app, '5');
        assert!(!app.snapshot.targets.is_empty());
        key(&mut app, 'a');
        assert!(
            matches!(app.modal, Some(Modal::SshForm(_))),
            "`a` must open the form, not point at the CLI"
        );
    }

    #[test]
    fn the_palette_gates_reset_behind_a_typed_name() {
        let (mut app, _dir) = app();
        key(&mut app, ':');
        typed(&mut app, "reset");
        press(&mut app, KeyCode::Enter);
        let Some(Modal::Confirm(confirm)) = &app.modal else {
            panic!("reset must confirm first: {:?}", app.modal);
        };
        assert_eq!(confirm.required_name.as_deref(), Some("reset"));
        assert!(!confirm.armed());
    }

    #[test]
    fn a_resource_command_refuses_a_selection_of_the_wrong_kind() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        // The fixture's first row is a database; ask for a bucket command.
        let kind = app.selected_resource().map(Resource::kind);
        assert_eq!(kind, Some(ResourceKind::Database));
        app.on_action(Action::BucketRotateKey);
        assert!(app.modal.is_none(), "nothing was rotated");
        let (toast, _) = app.toast.as_ref().expect("the mismatch is reported");
        assert!(toast.text.contains("버킷이 아닙니다"), "{}", toast.text);
    }

    #[test]
    fn engine_list_and_ssh_config_answer_without_docker() {
        let (mut app, _dir) = app();
        app.on_action(Action::EngineList);
        let Some(Modal::Message(message)) = &app.modal else {
            panic!("engine.list must show a report: {:?}", app.modal);
        };
        assert_eq!(message.title, "엔진 목록");
        assert!(message.body.iter().any(|l| l.contains("postgres")));

        app.modal = None;
        app.on_action(Action::TargetSshConfig);
        assert!(
            matches!(app.modal, Some(Modal::Message(_))),
            "ssh-config must answer even with no config file"
        );
    }

    #[test]
    fn db_url_shows_the_endpoint_with_the_clipboard_wired() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        app.on_action(Action::DbUrl);
        let Some(Modal::Message(message)) = &app.modal else {
            panic!("db.url must show the URL: {:?}", app.modal);
        };
        assert_eq!(message.title, "DB 접속 URL");
        assert!(message.can_copy(), "`y`/`Y` must work on this box");
    }

    #[test]
    fn a_delete_modal_swallows_letter_keys_and_enter_destroys_nothing() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        let resource = app.selected_resource().cloned().expect("a row is selected");
        let expected = resource.name().to_string();
        app.modal = Some(Modal::Confirm(Box::new(Confirm::new(
            local_drop_plan(&resource),
            Intent::DropResource(resource),
        ))));

        // `q` would quit anywhere else; here it is the first letter of a name.
        key(&mut app, 'q');
        assert!(!app.quit);
        let Some(Modal::Confirm(confirm)) = &app.modal else {
            panic!("the modal stayed open");
        };
        assert_eq!(confirm.typed, "q");
        assert!(!confirm.armed());

        // `Enter` is inert in a confirmation, however tempting.
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));

        press(&mut app, KeyCode::Backspace);
        typed(&mut app, &expected);
        let Some(Modal::Confirm(confirm)) = &app.modal else {
            panic!("the modal stayed open");
        };
        assert!(confirm.armed(), "the exact name arms it");
        assert!(frame(&mut app).contains("일치"));

        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_disarmed_delete_refuses_ctrl_s_and_says_why() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        let resource = app.selected_resource().cloned().expect("a row is selected");
        let mut confirm = Confirm::new(local_drop_plan(&resource), Intent::DropResource(resource));
        confirm.toggle_focus();
        app.modal = Some(Modal::Confirm(Box::new(confirm)));

        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(
            matches!(app.modal, Some(Modal::Confirm(_))),
            "a disarmed modal must not close, let alone act"
        );
        assert!(app.toast.is_some(), "the refusal is explained");
    }

    #[test]
    fn tab_cycles_screens_like_the_arrow_keys() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        assert_eq!(app.screen, Screen::Resources);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Engines);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.screen, Screen::Resources);
    }

    #[test]
    fn s_reveals_a_secret_and_the_reveal_expires_on_its_own() {
        let (mut app, _dir) = app();
        key(&mut app, '1');
        assert_eq!(app.reveal, 0);
        key(&mut app, 's');
        assert_eq!(app.reveal, REVEAL_TICKS);
        for _ in 0..REVEAL_TICKS {
            app.on_tick();
        }
        assert_eq!(app.reveal, 0, "the reveal is temporary");
    }

    #[test]
    fn y_on_a_result_box_confirms_the_copy_in_the_modal() {
        let (mut app, _dir) = app();
        app.modal = Some(Modal::Message(
            Message::note("완료", "redacted")
                .with_copy("postgresql://u:p@h/db", "DATABASE_URL=x\n"),
        ));
        key(&mut app, 'y');
        let Some(Modal::Message(message)) = &app.modal else {
            panic!("the result box stayed open");
        };
        let status = message.status.as_deref().unwrap_or("");
        assert!(
            status.contains("복사") || app.toast.is_some(),
            "copy must be acknowledged: status={status:?} toast={:?}",
            app.toast
        );
        let text = frame(&mut app);
        assert!(
            text.contains("복사") || text.contains("URL"),
            "the frame should mention the copy: {text}"
        );
    }

    #[test]
    fn q_quits_when_nothing_is_running() {
        let (mut app, _dir) = app();
        key(&mut app, 'q');
        assert!(app.quit);
    }

    #[test]
    fn resource_form_arrows_follow_the_visible_field_and_option_axes() {
        let (mut app, _dir) = app();
        key(&mut app, 'n');
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("`n` opens the create form");
        };
        assert_eq!(form.focus, Field::Kind);

        // Database and bucket are side by side: Left/Right move their cursor
        // without leaving the 종류 row.
        press(&mut app, KeyCode::Right);
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.focus, Field::Kind);
        assert_eq!(form.option_cursor, 1);
        press(&mut app, KeyCode::Left);
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.focus, Field::Kind);
        assert_eq!(form.option_cursor, 0);

        // The fields themselves are stacked. This fixture exposes Target too,
        // so Down walks 종류 → Target → 엔진 → 프로젝트명.
        press(&mut app, KeyCode::Down);
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.focus, Field::Target);
        press(&mut app, KeyCode::Down);
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.focus, Field::Engine);
        press(&mut app, KeyCode::Down);
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.focus, Field::Project);
        press(&mut app, KeyCode::Up);
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.focus, Field::Engine);

        press(&mut app, KeyCode::Down);
        typed(&mut app, "Letsbid");
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.name, "letsbid_dev");
        assert_eq!(form.principal, "letsbid_user");

        // Navigation does not invalidate a valid plan.
        let epoch = form.epoch();
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Up);
        let Some(Modal::Form(form)) = &app.modal else {
            panic!("the form stayed open");
        };
        assert_eq!(form.focus, Field::Engine);
        assert_eq!(form.epoch(), epoch);
        press(&mut app, KeyCode::Down);
        let text = frame(&mut app);
        assert!(text.contains("새 리소스"));
        assert!(text.contains("실행 계획"));
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_too_small_terminal_draws_only_the_size_message() {
        let (mut app, _dir) = app();
        let text = frame_at(&mut app, 70, 20);
        assert!(text.contains("터미널이 너무 작습니다"));
        assert!(text.contains("70"));
        assert!(!text.contains("Dashboard"));
        assert!(!text.contains("TARGETS"));
    }

    #[test]
    fn startup_notices_are_toasted_without_a_fake_alert_banner() {
        let (mut ctx, _dir) = temp_ctx();
        ctx.notices.push("키링을 사용할 수 없습니다".to_string());

        let mut app = App::new(Arc::new(ctx));
        app.snapshot = fixture::snapshot();
        app.rebuild_table();
        let text = frame(&mut app);
        assert!(
            text.contains("키링을 사용할 수 없습니다"),
            "the first notice is toasted: {text}"
        );
        assert!(!text.contains("docker degraded"), "{text}");
    }

    fn empty_setup_app() -> (App, tempfile::TempDir) {
        let (ctx, dir) = temp_ctx();
        let mut app = App::new(Arc::new(ctx));
        app.snapshot.checks.push(crate::core::doctor::Check {
            name: "Docker CLI".into(),
            ok: true,
            detail: "docker 27.1.1".into(),
            remedy: None,
        });
        app.rebuild_table();
        (app, dir)
    }

    #[test]
    fn an_empty_dashboard_shows_the_next_setup_step() {
        let (mut app, _dir) = empty_setup_app();
        let text = frame(&mut app);
        assert!(text.contains("이 컴퓨터를 등록할까요"), "{text}");
        assert!(text.contains("Enter"), "{text}");
        assert!(!text.contains("2 Targets"), "{text}");
        assert!(!text.contains("첫 DB 또는 버킷"), "{text}");
        assert!(
            text.contains("이 컴퓨터 등록") && text.contains("q"),
            "hint bar names the next action: {text}"
        );
    }

    #[test]
    fn enter_on_an_empty_dashboard_is_the_setup_action() {
        let (mut app, _dir) = empty_setup_app();
        let _ = frame(&mut app);
        assert_eq!(onboard::phase(&app.snapshot), onboard::Phase::RegisterLocal);
        assert_eq!(app.hits.at(10, 8), Some(Hit::AdvanceOnboard));
        assert_eq!(
            onboard::primary_label(onboard::phase(&app.snapshot)),
            "이 컴퓨터 등록"
        );
    }

    #[test]
    fn a_click_on_the_setup_panel_is_the_primary_action() {
        let (mut app, _dir) = empty_setup_app();
        let _ = frame(&mut app);
        assert_eq!(
            app.hits.at(10, 8),
            Some(Hit::AdvanceOnboard),
            "the setup body is clickable"
        );
    }

    #[test]
    fn number_keys_do_not_leave_the_current_setup_step() {
        let (mut app, _dir) = empty_setup_app();
        let _ = frame(&mut app);
        key(&mut app, '2');
        assert_eq!(app.screen, Screen::Resources);
        let text = frame(&mut app);
        assert!(text.contains("이 컴퓨터를 등록할까요"), "{text}");
        assert!(!text.contains("2 Targets"), "{text}");
    }

    #[test]
    fn a_click_on_the_nav_bar_switches_screens() {
        let (mut app, _dir) = app();
        let _ = frame(&mut app);
        let (targets_x, _) = crate::tui::rows::nav_tab_offset(Screen::Targets);
        // `frame` is 110 columns: the rendered tab row is centred inside its
        // one-cell frame inset, and the hit geometry shares that origin.
        let nav_width = 108;
        let origin = 1 + (nav_width - crate::tui::rows::nav_line_width()) / 2;

        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: origin.saturating_add(targets_x),
            row: 5,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.screen, Screen::Targets);
    }
}
