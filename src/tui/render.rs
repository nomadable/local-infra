//! Drawing. Takes a [`View`] — plain borrowed data — so every screen can be
//! rendered into a `TestBackend` without a terminal, a store or Docker.

use crate::core::config::Config;
use crate::core::model::EngineStatus;
use crate::tui::data::Snapshot;
use crate::tui::hints::{self, Focus, HintContext};
use crate::tui::job::Job;
use crate::tui::keymap::{Keymap, Screen};
use crate::tui::layout;
use crate::tui::modal::Modal;
use crate::tui::rows::{self, Endpoint, TableData};
use crate::tui::terminal::SizeCheck;
use crate::tui::theme::Theme;
use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

/// A short-lived message on the hint row: what a copy or a toggle just did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub danger: bool,
}

/// Everything one frame needs. Borrowed, never owned, so building it is free.
pub struct View<'a> {
    pub snapshot: &'a Snapshot,
    pub config: &'a Config,
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub screen: Screen,
    pub focus: Focus,
    pub table: &'a TableData,
    pub cursor: usize,
    pub detail_scroll: u16,
    /// `Some` while the `/` line is open, even when empty.
    pub filter: Option<&'a str>,
    pub modal: Option<&'a Modal>,
    /// Secrets are shown for a few seconds after `s` (PRD §7.6).
    pub reveal: bool,
    pub endpoint: &'a Endpoint,
    pub engine_status: Option<&'a EngineStatus>,
    pub job: Option<&'a Job>,
    pub job_log: &'a [String],
    pub tick: usize,
    pub notices: &'a [String],
    pub keymap_problems: &'a [String],
    pub toast: Option<&'a Toast>,
    pub now: DateTime<Utc>,
}

impl View<'_> {
    fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Index into the screen's underlying list for the selected row.
    fn selected(&self) -> Option<usize> {
        self.table.source_index(self.cursor)
    }

    /// Kind of the selected project resource, when the screen shows them.
    fn selected_kind(&self) -> Option<crate::core::model::ResourceKind> {
        if self.screen != Screen::Resources {
            return None;
        }
        self.snapshot
            .resources
            .get(self.selected()?)
            .map(crate::tui::data::Resource::kind)
    }

    pub(crate) fn hint_context(&self) -> HintContext {
        match self.modal {
            Some(Modal::Help { .. }) => HintContext::Help,
            Some(Modal::Palette(_)) => HintContext::Palette,
            Some(Modal::Form(_)) => HintContext::Form,
            Some(Modal::SshForm(_)) => HintContext::SshForm,

            Some(Modal::Confirm(confirm)) => HintContext::Confirm {
                needs_typing: confirm.needs_typing(),
                armed: confirm.armed(),
            },
            Some(Modal::Message(message)) => HintContext::Message {
                copy: message.can_copy(),
            },
            Some(Modal::Detail { .. }) => HintContext::Inspect,
            None if self.filter.is_some() => HintContext::Filter,

            None => {
                let phase = crate::tui::onboard::phase(self.snapshot);
                if phase.active()
                    && matches!(
                        self.screen,
                        Screen::Resources | Screen::Engines | Screen::Targets
                    )
                {
                    HintContext::Onboard {
                        phase,
                        busy: self.busy(),
                    }
                } else {
                    HintContext::Screen {
                        screen: self.screen,
                        focus: self.focus,
                        selected: self.selected().is_some(),
                        resource: self.selected_kind(),
                        busy: self.busy(),
                    }
                }
            }
        }
    }
}

/// Draw one frame. `state` carries the table's scroll offset between frames.
pub fn draw(frame: &mut Frame, view: &View, state: &mut TableState) {
    let area = frame.area();
    let size = SizeCheck {
        width: area.width,
        height: area.height,
    };

    // TUI-002: below the minimum, one message and nothing else. A half-drawn
    // layout is worse than an explicit refusal.
    if !size.ok() {
        frame.render_widget(
            Paragraph::new(size.message())
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    // The hint rows are laid out first: how many rows they need decides how
    // much height is left for the body. A completion toast takes precedence
    // over job progress in the same footer slot.
    let hints = hints::hints(view.keymap, view.hint_context());
    let feedback = feedback_text(view);
    let toast_cols = feedback
        .as_ref()
        .map(|(text, _)| (crate::core::util::display_cols(text) as u16 + 2).min(area.width / 2))
        .unwrap_or(0);
    let hint_width = area.width.saturating_sub(toast_cols) as usize;
    let (hint_rows, hints_dropped) = if view.filter.is_some() {
        (Vec::new(), false)
    } else {
        hints::wrap(&hints, hint_width, MAX_HINT_ROWS)
    };

    let setup = crate::tui::onboard::phase(view.snapshot).active();
    let shell = layout::shell_nav(area, size, hint_rows.len().max(1) as u16, !setup);
    frame.render_widget(
        Paragraph::new(crate::tui::chrome::wordmark(view.theme)).alignment(Alignment::Center),
        shell.status,
    );
    if shell.nav_visible {
        let block = panel(view, "Menu tabs", false);
        let inner = block.inner(shell.nav);
        frame.render_widget(block, shell.nav);
        frame.render_widget(
            Paragraph::new(rows::nav_line(view.screen, view.theme)).alignment(Alignment::Center),
            inner,
        );
    }
    if shell.strip_visible {
        let block = panel(view, "Active engines", false);
        let inner = block.inner(shell.strip);
        frame.render_widget(block, shell.strip);
        frame.render_widget(
            Paragraph::new(rows::engine_strip(view.snapshot, inner.width, view.theme)),
            inner,
        );
    }

    body(frame, view, shell.body, size, state);

    hint_bar(
        frame,
        view,
        shell.hints,
        &hint_rows,
        hints_dropped,
        toast_cols,
        feedback.as_ref(),
    );

    if let Some(modal) = view.modal {
        overlay(frame, view, modal, area);
    }
}

fn spinner_text(view: &View) -> Option<String> {
    let job = view.job?;
    Some(format!(
        "{} {}",
        view.theme.spinner(view.tick),
        job.headline()
    ))
}

/// A completion toast wins the footer. Otherwise a running job reports its
/// current step there, leaving the active-tab title to name only the tab.
fn feedback_text(view: &View) -> Option<(String, bool)> {
    view.toast
        .map(|toast| (toast.text.clone(), toast.danger))
        .or_else(|| spinner_text(view).map(|text| (text, false)))
}

fn body(frame: &mut Frame, view: &View, area: Rect, _size: SizeCheck, state: &mut TableState) {
    // Resources is home, so its empty state is where first-run setup lives.
    if view.screen == Screen::Resources && crate::tui::onboard::phase(view.snapshot).active() {
        setup_card(frame, view, area);
        return;
    }
    list(frame, view, area, state);
}

fn panel(view: &View, title: &str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(view.theme.border_type())
        .border_style(view.theme.border(focused))
        .title_top(Line::from(Span::styled(
            format!(" {title} "),
            view.theme.heading(),
        )))
}

/// One step, one card, centred. Nothing else on the body during first run.
fn setup_card(frame: &mut Frame, view: &View, area: Rect) {
    let lines = crate::tui::onboard::lines(view.snapshot, view.notices, view.theme);
    let width = 56.min(area.width.saturating_sub(2)).max(24.min(area.width));
    let height = (lines.len() as u16)
        .saturating_add(2)
        .min(area.height)
        .max(7.min(area.height));
    let popup = layout::popup(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(view.theme.border_type())
        .border_style(view.theme.border(true));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// Widest a single column may claim, so one long name cannot squeeze the
/// identifying columns out of an 80-column terminal.
const MAX_COLUMN: u16 = 28;
/// Gap between columns. One space is too tight to read a boundary into, and
/// the header rule puts its junction in the middle of this gap.
const COLUMN_SPACING: u16 = 2;

/// The table, with its header and a ruled boundary drawn above the rows.
///
/// The header is drawn by hand rather than by `Table::header` so the rule can
/// carry a `┼` at every column boundary — that is what makes a six-column row
/// scannable without vertical separators on every line.
fn list(frame: &mut Frame, view: &View, area: Rect, state: &mut TableState) {
    let count = view.table.len();
    let title = view.screen.title();
    let block = panel(view, title, view.focus == Focus::List);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if view.table.headers.is_empty() || inner.height < 3 {
        return;
    }

    let widths = rows::column_widths(view.table, MAX_COLUMN);
    let [header_area, rule_area, rows_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    // The selection symbol shifts every cell right, so the header carries the
    // same indent or the columns stop lining up with their labels. Both are
    // measured in *columns*: `▸ ` is four bytes and two columns wide.
    let cursor = format!("{} ", crate::tui::chrome::Glyphs::of(view.theme).cursor);
    let indent_cols = crate::core::util::display_cols(&cursor);
    let indent = " ".repeat(indent_cols);

    let constraints: Vec<Constraint> = widths.iter().copied().map(Constraint::Length).collect();
    frame.render_widget(
        Table::new(
            vec![Row::new(view.table.headers.to_vec()).style(view.theme.heading())],
            constraints.clone(),
        )
        .column_spacing(COLUMN_SPACING)
        .highlight_symbol(indent.clone()),
        header_area,
    );

    let mut ruled = widths.clone();
    if let Some(first) = ruled.first_mut() {
        *first += indent_cols as u16;
    }
    frame.render_widget(
        Paragraph::new(crate::tui::chrome::column_rule(
            &ruled,
            COLUMN_SPACING,
            rule_area.width,
            view.theme,
        )),
        rule_area,
    );

    if count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                empty_message(view).to_string(),
                view.theme.muted(),
            )))
            .wrap(Wrap { trim: true }),
            rows_area,
        );
        return;
    }

    let body: Vec<Row> = view
        .table
        .visible
        .iter()
        .filter_map(|i| view.table.rows.get(*i))
        .map(|cells| Row::new(cells.iter().map(String::as_str).collect::<Vec<_>>()))
        .collect();

    state.select(Some(view.cursor.min(count.saturating_sub(1))));
    frame.render_stateful_widget(
        Table::new(body, constraints)
            .column_spacing(COLUMN_SPACING)
            .row_highlight_style(view.theme.selected())
            .highlight_symbol(if view.focus == Focus::List {
                cursor
            } else {
                indent
            }),
        rows_area,
        state,
    );
}

fn empty_message(view: &View) -> &'static str {
    if view.filter.is_some_and(|f| !f.trim().is_empty()) {
        return "필터에 일치하는 항목이 없습니다. `Esc`로 해제하세요.";
    }
    match view.screen {
        Screen::Resources => "아직 없습니다. `n`으로 DB나 버킷을 만드세요.",
        Screen::Engines => "엔진이 없습니다. 리소스를 만들면 컨테이너가 함께 생깁니다.",
        Screen::Targets => "등록된 Target이 없습니다. `a`로 이 컴퓨터를 등록하세요.",
        Screen::Tunnels => "터널 기록이 없습니다. 원격 리소스에서 `t`로 시작하세요.",
        Screen::Backups => "백업 기록이 없습니다. 리소스에서 `b`로 백업하세요.",
        Screen::Activity => "기록된 활동이 없습니다.",
        Screen::Doctor => "진단 결과가 없습니다. `Ctrl+T`로 실행하세요.",
    }
}

fn inspect_lines(view: &View) -> Vec<Line<'static>> {
    let index = view.selected().unwrap_or(0);
    let mut lines: Vec<Line<'static>> = match view.screen {
        Screen::Engines => rows::engine_detail_lines(view.snapshot, index, view.theme),
        Screen::Targets => rows::target_detail_lines(view.snapshot, index, view.theme),
        Screen::Resources => match view.snapshot.resources.get(index) {
            Some(resource) => rows::resource_detail_lines(
                resource,
                view.engine_status,
                view.endpoint,
                view.reveal,
                view.theme,
            ),
            None => vec![Line::from(Span::styled(
                "선택된 리소스가 없습니다.".to_string(),
                view.theme.muted(),
            ))],
        },
        Screen::Tunnels => rows::tunnel_detail_lines(view.snapshot, index, view.theme),
        Screen::Backups => rows::backup_detail_lines(view.snapshot, index, view.theme),
        Screen::Activity => rows::activity_detail_lines(view.snapshot, index, view.theme),
        Screen::Doctor => rows::doctor_detail_lines(
            view.snapshot,
            view.keymap_problems,
            view.notices,
            view.theme,
        ),
    };
    if !view.job_log.is_empty() && view.busy() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::from(Span::styled(
            "진행".to_string(),
            view.theme.heading(),
        )));
        for entry in view.job_log.iter().rev().take(6).rev() {
            lines.push(Line::from(Span::styled(
                format!("  {entry}"),
                view.theme.normal(),
            )));
        }
    }
    lines
}

/// At most two rows of hints, with the toast tucked into the right end of the
/// first one (PRD §7.6 draws the resources verbs on two lines).
const MAX_HINT_ROWS: usize = 2;

fn hint_bar(
    frame: &mut Frame,
    view: &View,
    area: Rect,
    hint_rows: &[Vec<hints::Hint>],
    dropped: bool,
    toast_cols: u16,
    feedback: Option<&(String, bool)>,
) {
    if let Some(filter) = view.filter {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("/".to_string(), view.theme.accent()),
                Span::styled(format!("{filter}_"), view.theme.normal()),
                Span::styled(format!("   {} 항목", view.table.len()), view.theme.muted()),
            ])),
            area,
        );
        return;
    }

    let (body, feedback_area) = if toast_cols > 0 {
        let [body, feedback] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(toast_cols)]).areas(area);
        (body, Some(feedback))
    } else {
        (area, None)
    };

    let mut lines = Vec::new();
    for (index, row) in hint_rows.iter().enumerate() {
        let pairs: Vec<(String, String)> = row
            .iter()
            .map(|h| (h.key.clone(), h.label.clone()))
            .collect();
        let mut line = crate::tui::chrome::hint_row(&pairs, view.theme);
        if dropped && index + 1 == hint_rows.len() {
            line.spans.push(Span::styled(
                view.theme.ellipsis().to_string(),
                view.theme.muted(),
            ));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), body);

    if let (Some(area), Some((text, danger))) = (feedback_area, feedback) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text.clone(),
                if *danger {
                    view.theme.danger()
                } else {
                    view.theme.accent()
                },
            )))
            .alignment(Alignment::Right),
            area,
        );
    }
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

fn overlay(frame: &mut Frame, view: &View, modal: &Modal, area: Rect) {
    let (cols, rows_high) = match modal {
        Modal::Help { .. } => (76, area.height.saturating_sub(4).max(10)),
        Modal::Palette(_) => (72, 20),
        Modal::Form(_) => (80, area.height.saturating_sub(2).max(20)),
        Modal::SshForm(_) => (72, area.height.saturating_sub(4).max(20)),
        Modal::Confirm(_) => (70, 22),
        Modal::Message(_) => (78, 22),
        Modal::Detail { .. } => (72, area.height.saturating_sub(4).max(12)),
    };
    let rect = layout::popup(area, cols, rows_high);
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(view.theme.border_type())
        .border_style(view.theme.border(true))
        .title_top(
            Line::from(Span::styled(
                format!(" {} ", modal.title()),
                view.theme.heading(),
            ))
            .centered(),
        );
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let (lines, scroll) = match modal {
        Modal::Help { scroll } => (help_lines(view.keymap, view.theme), *scroll),
        Modal::Palette(palette) => (palette.lines(view.keymap, view.theme), 0),
        Modal::Form(form) => (form.lines(view.theme), 0),
        Modal::SshForm(form) => (form.lines(view.theme), 0),
        Modal::Confirm(confirm) => (confirm.lines(view.theme), 0),
        Modal::Message(message) => (message.lines(view.theme), message.scroll),
        Modal::Detail { scroll, .. } => (inspect_lines(view), *scroll),
    };

    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

/// Two columns of `key  action`, straight from the keymap (TUI-008).
fn help_lines(keymap: &Keymap, theme: &Theme) -> Vec<Line<'static>> {
    let columns = hints::help_columns(keymap, 2);
    let height = columns.iter().map(Vec::len).max().unwrap_or(0);
    let mut lines = vec![
        Line::from(Span::styled(
            "모든 동작은 키보드로 실행할 수 있습니다.".to_string(),
            theme.muted(),
        )),
        Line::raw(String::new()),
    ];
    for row in 0..height {
        let mut spans = Vec::new();
        for column in &columns {
            match column.get(row) {
                Some((key, label)) => {
                    spans.push(Span::styled(format!("  {key:>10} "), theme.key()));
                    spans.push(Span::styled(rows::pad(label, 22), theme.normal()));
                }
                None => spans.push(Span::raw(" ".repeat(35))),
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::data::fixture;
    use crate::tui::form::Form;
    use crate::tui::modal::{Confirm, Intent, Message, Palette};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    struct Harness {
        snapshot: Snapshot,
        config: Config,
        theme: Theme,
        keymap: Keymap,
        endpoint: Endpoint,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                snapshot: fixture::snapshot(),
                config: Config::default(),
                theme: Theme::plain(),
                keymap: Keymap::defaults(),
                endpoint: Endpoint {
                    url: Some("postgresql://letsbid_user:s3cr3t@127.0.0.1:5432/letsbid_dev".into()),
                    redacted_url: Some(
                        "postgresql://letsbid_user:****@127.0.0.1:5432/letsbid_dev".into(),
                    ),
                    env_block: Some("DATABASE_URL=postgresql://…\n".into()),
                    secret: Some("s3cr3t".into()),
                    address: Some("127.0.0.1:5432".into()),
                    region: None,
                    note: None,
                },
            }
        }

        fn table(&self, screen: Screen, filter: &str) -> TableData {
            rows::table_for(screen, &self.snapshot, &self.config, filter, &self.theme)
        }

        fn view<'a>(&'a self, screen: Screen, table: &'a TableData) -> View<'a> {
            View {
                snapshot: &self.snapshot,
                config: &self.config,
                theme: &self.theme,
                keymap: &self.keymap,
                screen,
                focus: Focus::List,
                table,
                cursor: 0,
                detail_scroll: 0,
                filter: None,
                modal: None,
                reveal: false,
                endpoint: &self.endpoint,
                engine_status: None,
                job: None,
                job_log: &[],
                tick: 0,
                notices: &[],
                keymap_problems: &[],
                toast: None,
                now: fixture::when(1, 21, 4),
            }
        }
    }

    /// Flatten a rendered buffer back into text.
    ///
    /// A double-width glyph occupies two cells and the second is a filler, so
    /// the reader advances by the glyph's display width rather than one cell —
    /// otherwise every Korean word comes back with spaces wedged into it.
    fn shot(width: u16, height: u16, view: &View) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        let mut state = TableState::default();
        terminal
            .draw(|frame| draw(frame, view, &mut state))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
                out.push_str(symbol);
                x += (crate::core::util::display_cols(symbol).max(1)) as u16;
            }
            out.push('\n');
        }
        out
    }

    fn screen_shot(screen: Screen) -> String {
        let harness = Harness::new();
        let table = harness.table(screen, "");
        let view = harness.view(screen, &table);
        shot(120, 30, &view)
    }

    fn inspect_shot(screen: Screen) -> String {
        let harness = Harness::new();
        let table = harness.table(screen, "");
        let mut view = harness.view(screen, &table);
        let modal = Modal::Detail {
            title: "상세".into(),
            scroll: 0,
        };
        view.modal = Some(&modal);
        shot(120, 30, &view)
    }

    #[test]
    fn every_screen_draws_the_reference_shell_in_one_clear_hierarchy() {
        let text = screen_shot(Screen::Resources);
        assert!(text.contains("Menu tabs"));
        assert!(text.contains("Active engines"));
        assert!(text.contains("PORT:5432"));
        assert!(!text.contains("Active Tab Details"));
        assert!(!text.contains("Resources 1/3"));
        assert!(!text.contains("Dashboard"));
        assert!(!text.contains("secrets.mode"));
        assert!(
            text.contains("새 리소스"),
            "the operational key footer remains: {text}"
        );
    }

    #[test]
    fn targets_screen_renders_its_table_and_detail() {
        let text = screen_shot(Screen::Targets);
        assert!(text.contains("NAME"));
        assert!(text.contains("LOCATION"));
        assert!(text.contains("dev-vps"));
        let detail = inspect_shot(Screen::Targets);
        assert!(detail.contains("ENGINES"));
    }

    #[test]
    fn resources_screen_renders_both_services_in_one_table() {
        let text = screen_shot(Screen::Resources);
        for header in rows::RESOURCE_COLUMNS {
            assert!(text.contains(header), "missing column {header}");
        }
        assert!(text.contains("letsbid-dev-assets"));
        assert!(text.contains("parantica_dev"));
        assert!(text.contains("bucket"));
        assert!(text.contains("minio latest"));
    }

    #[test]
    fn the_resource_detail_masks_the_password_until_reveal_is_on() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let mut view = harness.view(Screen::Resources, &table);
        let modal = Modal::Detail {
            title: "상세".into(),
            scroll: 0,
        };
        view.modal = Some(&modal);
        let masked = shot(120, 30, &view);
        assert!(masked.contains("****"));
        assert!(!masked.contains("s3cr3t"));

        view.reveal = true;
        let shown = shot(120, 30, &view);
        assert!(shown.contains("s3cr3t"));
    }

    #[test]
    fn tunnels_screen_renders_the_prd_fields() {
        let text = screen_shot(Screen::Tunnels);
        assert!(text.contains("LOCAL"));
        assert!(text.contains("REMOTE"));
        assert!(text.contains("PID"));
        assert!(text.contains("48122"));
        assert!(text.contains("active"));
    }

    #[test]
    fn backups_screen_renders_records_and_the_selected_detail() {
        let text = screen_shot(Screen::Backups);
        assert!(text.contains("FORMAT"));
        assert!(text.contains("custom"));
        let detail = inspect_shot(Screen::Backups);
        assert!(detail.contains("checksum"));
    }

    #[test]
    fn log_screen_renders_entries_and_their_steps() {
        let text = screen_shot(Screen::Activity);
        assert!(text.contains("ACTION"));
        assert!(text.contains("create"));
        let detail = inspect_shot(Screen::Activity);
        assert!(detail.contains("STEPS"));
    }
    #[test]
    fn doctor_screen_renders_diagnostics_not_read_only_configuration() {
        let text = screen_shot(Screen::Doctor);
        assert!(text.contains("Doctor"));
        assert!(text.contains("CHECK"));
        assert!(text.contains("docker"));
        assert!(!text.contains("secrets.mode"));
        let detail = inspect_shot(Screen::Doctor);
        assert!(detail.contains("DOCTOR"));
    }

    #[test]
    fn a_terminal_under_eighty_by_twenty_four_gets_only_the_size_message() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let view = harness.view(Screen::Resources, &table);
        let text = shot(60, 20, &view);
        assert!(text.contains("터미널이 너무 작습니다"));
        assert!(text.contains("60"));
        assert!(!text.contains("TARGET"), "no layout is attempted: {text}");
        assert!(!text.contains("Dashboard"));
    }

    #[test]
    fn at_eighty_columns_the_navigation_survives_without_a_detail_pane() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let view = harness.view(Screen::Resources, &table);
        let text = shot(80, 24, &view);
        assert!(text.contains("Resources"));
        assert!(!text.contains("Dashboard"));
        assert!(text.contains("NAME"));
        assert!(
            !text.contains("상세"),
            "detail is a popup, not a stacked pane: {text}"
        );
    }

    #[test]
    fn the_help_overlay_lists_the_keymap() {
        let harness = Harness::new();
        let table = harness.table(Screen::Engines, "");
        let mut view = harness.view(Screen::Engines, &table);
        let modal = Modal::Help { scroll: 0 };
        view.modal = Some(&modal);
        let text = shot(120, 30, &view);
        assert!(text.contains("도움말"));
        assert!(text.contains("커맨드 팔레트"));
        assert!(text.contains("종료"));
    }

    #[test]
    fn the_palette_overlay_shows_filtered_commands() {
        let harness = Harness::new();
        let table = harness.table(Screen::Engines, "");
        let mut view = harness.view(Screen::Engines, &table);
        let mut palette = Palette::new(&harness.keymap);
        for c in "tunnel".chars() {
            palette.type_char(c);
        }
        let modal = Modal::Palette(palette);
        view.modal = Some(&modal);
        let text = shot(120, 30, &view);
        assert!(text.contains("tunnel.toggle"));
        assert!(!text.contains("backup.run"));
    }

    #[test]
    fn the_form_overlay_shows_fields_and_the_plan() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let mut view = harness.view(Screen::Resources, &table);
        let mut form = Form::new(
            vec![fixture::local_target()],
            crate::core::model::EngineKind::Postgres,
        );
        form.focus = crate::tui::form::Field::Project;
        for c in "Letsbid".chars() {
            form.type_char(c);
        }
        let epoch = form.invalidate_plan();
        form.accept_plan(
            epoch,
            Ok(crate::core::plan::Plan::new("생성").step(
                crate::core::plan::StepKind::New,
                "컨테이너 linf-postgres-17 생성",
            )),
        );
        let modal = Modal::Form(Box::new(form));
        view.modal = Some(&modal);
        let text = shot(120, 34, &view);
        assert!(text.contains("새 리소스"));
        assert!(text.contains("요약"));
        assert!(text.contains("실행 계획"));
        assert!(text.contains("linf-postgres-17"));
    }

    #[test]
    fn the_delete_overlay_shows_the_typed_gate_and_the_cancel_default() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let mut view = harness.view(Screen::Resources, &table);
        let resource = fixture::database(&fixture::local_target(), "letsbid_dev", None);
        let plan = crate::core::plan::Plan::new("삭제")
            .step(crate::core::plan::StepKind::Destroy, "DB letsbid_dev 삭제");
        let modal = Modal::Confirm(Box::new(Confirm::new(plan, Intent::DropResource(resource))));
        view.modal = Some(&modal);
        let text = shot(120, 30, &view);
        assert!(text.contains("되돌릴 수 없습니다"));
        assert!(text.contains("삭제하려면 이름을 입력하세요"));
        assert!(text.contains("[ 취소 ]"));
        assert!(text.contains("이름 입력 후 활성화"));
    }

    #[test]
    fn a_failure_overlay_shows_all_three_diagnostic_parts() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let mut view = harness.view(Screen::Resources, &table);
        let diagnostic = crate::core::Diagnostic::new(
            "컨테이너를 만들 수 없습니다",
            "포트 충돌",
            "포트를 바꾸세요",
        );
        let modal = Modal::Message(Message::failure(&diagnostic));
        view.modal = Some(&modal);
        let text = shot(120, 30, &view);
        assert!(text.contains("컨테이너를 만들 수 없습니다"));
        assert!(text.contains("포트 충돌"));
        assert!(text.contains("포트를 바꾸세요"));
    }

    #[test]
    fn a_running_job_shows_a_spinner_its_step_and_the_cancel_key() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let mut view = harness.view(Screen::Resources, &table);
        let job = Job {
            id: 1,
            title: "백업".into(),
            cancel: crate::core::progress::Cancel::new(),
            step: Some(crate::tui::job::Step {
                index: 2,
                total: 5,
                title: "덤프 스트리밍".into(),
            }),
            bytes: Some(1024),
            log: vec!["2/5 덤프 스트리밍".into()],
            quiet: false,
        };
        view.job = Some(&job);
        view.job_log = &job.log;
        let text = shot(120, 30, &view);
        assert!(text.contains("백업 (2/5)"));
        assert!(text.contains("Ctrl+C"));
    }

    #[test]
    fn the_filter_line_replaces_the_hint_bar_and_reports_the_match_count() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "bucket");
        let mut view = harness.view(Screen::Resources, &table);
        view.filter = Some("bucket");
        let text = shot(120, 30, &view);
        assert!(text.contains("/bucket_"));
        assert!(text.contains("1 항목"));
        assert!(text.contains("letsbid-dev-assets"));
        assert!(!text.contains("parantica_dev"));
    }

    #[test]
    fn a_filter_matching_nothing_says_so_instead_of_showing_a_blank_pane() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "zzzz");
        let mut view = harness.view(Screen::Resources, &table);
        view.filter = Some("zzzz");
        let text = shot(120, 30, &view);
        assert!(text.contains("일치하는 항목이 없습니다"));
    }

    #[test]
    fn an_empty_home_screen_is_the_setup_card() {
        let harness = Harness {
            snapshot: Snapshot::empty(),
            ..Harness::new()
        };
        let table = harness.table(Screen::Resources, "");
        let view = harness.view(Screen::Resources, &table);
        let text = shot(120, 30, &view);
        assert!(
            text.contains("Docker가 응답하는지") || text.contains("살펴봅니다"),
            "{text}"
        );
        assert!(text.contains("Enter"), "{text}");
    }

    #[test]
    fn a_toast_is_shown_beside_the_hints_without_hiding_them() {
        let harness = Harness::new();
        let table = harness.table(Screen::Resources, "");
        let mut view = harness.view(Screen::Resources, &table);
        let toast = Toast {
            text: "URL을 복사했습니다".into(),
            danger: false,
        };
        view.toast = Some(&toast);
        let text = shot(120, 30, &view);
        assert!(text.contains("URL을 복사했습니다"));
        assert!(
            text.contains("←/→") || text.contains("새 리소스"),
            "hints remain visible beside the toast: {text}"
        );
    }
}
