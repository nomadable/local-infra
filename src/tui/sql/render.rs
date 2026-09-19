//! Ratatui renderer for the independent SQL workspace.

use super::catalog::CatalogNodeKind;
use super::workspace::{Overlay, Pane, Workspace};
use crate::core::sql::statement::{self, Token, TokenKind};
use crate::core::util::display_cols;
use crate::tui::terminal::SizeCheck;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::ops::Range;

const BG: Color = Color::Rgb(13, 15, 18);
const PANEL: Color = Color::Rgb(20, 23, 28);
const BORDER: Color = Color::Rgb(65, 72, 84);
const FOCUS: Color = Color::Rgb(104, 163, 255);
const TEXT: Color = Color::Rgb(224, 228, 236);
const MUTED: Color = Color::Rgb(132, 140, 154);
const WARN: Color = Color::Rgb(255, 190, 92);
const DANGER: Color = Color::Rgb(255, 112, 112);
const SELECT: Color = Color::Rgb(44, 72, 112);

pub fn draw(frame: &mut Frame, workspace: &Workspace) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    let size = SizeCheck {
        width: area.width,
        height: area.height,
    };
    if !size.ok() {
        frame.render_widget(
            Paragraph::new(size.message())
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let shell = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .split(area);
    header(frame, workspace, shell[0]);
    body(frame, workspace, shell[1]);
    footer(frame, workspace, shell[2]);
    if let Some(overlay) = &workspace.overlay {
        overlay_widget(frame, workspace, overlay, area);
    } else if !workspace.completions.is_empty() {
        completions(frame, workspace, area);
    }
}

fn header(frame: &mut Frame, workspace: &Workspace, area: Rect) {
    let endpoint = workspace.endpoint.as_ref();
    let kind = endpoint
        .map(|endpoint| {
            if endpoint.external {
                "EXTERNAL"
            } else {
                "MANAGED"
            }
        })
        .unwrap_or("CONNECTING");
    let access = endpoint
        .map(|endpoint| endpoint.access.label())
        .unwrap_or("WAIT");
    let label = endpoint
        .map(|endpoint| endpoint.label.as_str())
        .unwrap_or(workspace.source_key.as_str());
    let connection = if workspace.connected {
        "CONNECTED"
    } else {
        "DISCONNECTED"
    };
    let tls = endpoint
        .map(|endpoint| {
            if endpoint.tls_mode == crate::core::sql::TlsMode::Disable {
                "TLS OFF"
            } else {
                "TLS VERIFY"
            }
        })
        .unwrap_or("TLS WAIT");
    let style = if endpoint.is_some_and(|endpoint| {
        endpoint.external && endpoint.access == crate::core::sql::AccessMode::ReadWrite
    }) {
        Style::default().fg(DANGER).add_modifier(Modifier::BOLD)
    } else if access == "READ ONLY" {
        Style::default().fg(WARN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(FOCUS).add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " SQL WORKBENCH ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {kind} · {access} "), style),
            Span::styled(
                format!(" {tls} "),
                Style::default()
                    .fg(if tls == "TLS OFF" { DANGER } else { MUTED })
                    .add_modifier(if tls == "TLS OFF" {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(format!(" {label} "), Style::default().fg(TEXT)),
            Span::styled(
                format!(" {connection} · TX {} ", workspace.transaction),
                Style::default().fg(MUTED),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

fn body(frame: &mut Frame, workspace: &Workspace, area: Rect) {
    if let Some(pane) = workspace.fullscreen {
        match pane {
            Pane::Catalog => catalog(frame, workspace, area),
            Pane::Editor => editor(frame, workspace, area),
            Pane::Results => results(frame, workspace, area),
        }
        return;
    }
    let horizontal =
        Layout::horizontal([Constraint::Percentage(29), Constraint::Percentage(71)]).split(area);
    let right = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(horizontal[1]);
    catalog(frame, workspace, horizontal[0]);
    editor(frame, workspace, right[0]);
    results(frame, workspace, right[1]);
}

fn panel(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { FOCUS } else { BORDER }))
        .style(Style::default().bg(PANEL).fg(TEXT))
}

fn catalog(frame: &mut Frame, workspace: &Workspace, area: Rect) {
    let title = if workspace.catalog.loading {
        " Connections / Catalog · loading ".into()
    } else {
        " Connections / Catalog · F6 ".into()
    };
    let block = panel(title, workspace.focus == Pane::Catalog);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let nodes = workspace.catalog.nodes();
    let mut lines = Vec::new();
    if let Some(endpoint) = &workspace.endpoint {
        lines.push(Line::styled(
            format!(
                "● {} · {} · F7 manage",
                endpoint.label,
                endpoint.access.label()
            ),
            Style::default().fg(FOCUS).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            endpoint.redacted_url(),
            Style::default().fg(MUTED),
        ));
    } else {
        lines.push(Line::styled(
            "○ connecting · F7 manage",
            Style::default().fg(MUTED),
        ));
    }
    if let Some(error) = &workspace.catalog.error {
        lines.push(Line::styled(error.clone(), Style::default().fg(DANGER)));
    }
    for (index, node) in nodes
        .iter()
        .enumerate()
        .skip(workspace.catalog.offset)
        .take(inner.height as usize)
    {
        let marker = if node.expandable {
            if node.expanded {
                "▾"
            } else {
                "▸"
            }
        } else {
            " "
        };
        let kind = match node.kind {
            CatalogNodeKind::Schema => "schema",
            CatalogNodeKind::Relation(kind) => match kind {
                crate::core::sql::catalog::RelationKind::Table => "table",
                crate::core::sql::catalog::RelationKind::PartitionedTable => "partition",
                crate::core::sql::catalog::RelationKind::View => "view",
                crate::core::sql::catalog::RelationKind::MaterializedView => "matview",
                crate::core::sql::catalog::RelationKind::Sequence => "sequence",
            },
            CatalogNodeKind::Column => "column",
            CatalogNodeKind::PrimaryKey => "key",
            CatalogNodeKind::ForeignKey => "key",
        };
        let style = if index == workspace.catalog.cursor {
            Style::default().bg(SELECT).fg(TEXT)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::styled(
            format!(
                "{}{} {}  {}",
                "  ".repeat(node.depth),
                marker,
                node.label,
                kind
            ),
            style,
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "catalog가 준비되는 동안 editor를 사용할 수 있습니다",
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn editor(frame: &mut Frame, workspace: &Workspace, area: Rect) {
    let buffer = workspace.buffers.active();
    let tabs = workspace
        .buffers
        .items
        .iter()
        .enumerate()
        .map(|(index, buffer)| {
            format!(
                "{}{} {}",
                if index == workspace.buffers.active {
                    "●"
                } else {
                    "○"
                },
                if buffer.dirty { "*" } else { "" },
                buffer.title
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    let block = panel(
        format!(" SQL Editor · {tabs} "),
        workspace.focus == Pane::Editor,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let tokens = statement::tokens(&buffer.text);
    let selection = workspace.current_selection();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let line_count_width = buffer.text.lines().count().max(1).to_string().len();
    for (line_index, text) in buffer.text.split('\n').enumerate() {
        let end = start + text.len();
        if line_index >= workspace.editor_scroll && lines.len() < inner.height as usize {
            let mut spans = vec![Span::styled(
                format!("{:>width$} │ ", line_index + 1, width = line_count_width),
                Style::default().fg(MUTED),
            )];
            spans.extend(styled_line(
                &buffer.text,
                start..end,
                &tokens,
                selection.clone(),
            ));
            lines.push(Line::from(spans));
        }
        start = end.saturating_add(1);
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("1 │ ", Style::default().fg(MUTED)),
            Span::raw(""),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if workspace.focus == Pane::Editor && workspace.overlay.is_none() {
        let (line, column) = buffer.line_column();
        if line >= workspace.editor_scroll {
            let y = inner.y + (line - workspace.editor_scroll) as u16;
            let x = inner.x
                + line_count_width as u16
                + 3
                + column.saturating_sub(workspace.editor_horizontal) as u16;
            if x < inner.right() && y < inner.bottom() {
                frame.set_cursor_position(Position::new(x, y));
            }
        }
    }
}

fn styled_line(
    source: &str,
    line: Range<usize>,
    tokens: &[Token],
    selection: Option<Range<usize>>,
) -> Vec<Span<'static>> {
    if line.is_empty() {
        return vec![Span::raw("")];
    }
    let mut boundaries = vec![line.start, line.end];
    for token in tokens {
        if token.range.end > line.start && token.range.start < line.end {
            boundaries.push(token.range.start.max(line.start));
            boundaries.push(token.range.end.min(line.end));
        }
    }
    if let Some(selection) = &selection {
        if selection.end > line.start && selection.start < line.end {
            boundaries.push(selection.start.max(line.start));
            boundaries.push(selection.end.min(line.end));
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
        .windows(2)
        .filter(|window| window[0] < window[1])
        .map(|window| {
            let range = window[0]..window[1];
            let kind = tokens
                .iter()
                .find(|token| token.range.start <= range.start && token.range.end >= range.end)
                .map(|token| token.kind);
            let mut style = match kind {
                Some(TokenKind::Keyword) => Style::default()
                    .fg(Color::Rgb(128, 185, 255))
                    .add_modifier(Modifier::BOLD),
                Some(TokenKind::String) => Style::default().fg(Color::Rgb(174, 215, 130)),
                Some(TokenKind::Comment) => {
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
                }
                Some(TokenKind::Number) => Style::default().fg(Color::Rgb(224, 173, 255)),
                Some(TokenKind::Identifier) | Some(TokenKind::Symbol) | None => {
                    Style::default().fg(TEXT)
                }
            };
            if selection
                .as_ref()
                .is_some_and(|selection| selection.start < range.end && selection.end > range.start)
            {
                style = style.bg(SELECT);
            }
            Span::styled(source[range].to_string(), style)
        })
        .collect()
}

fn results(frame: &mut Frame, workspace: &Workspace, area: Rect) {
    let title = if workspace.results.running {
        format!(" Results · running {} ", spinner(workspace.tick))
    } else if workspace.results.tabs.is_empty() {
        " Results ".into()
    } else {
        format!(
            " Results · {}/{} ",
            workspace.results.active + 1,
            workspace.results.tabs.len()
        )
    };
    let block = panel(title, workspace.focus == Pane::Results);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(tab) = workspace.results.active() else {
        frame.render_widget(
            Paragraph::new("Ctrl+Enter current/selection · F5 whole buffer")
                .style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    };
    if let Some(error) = &tab.error {
        let detail = format!(
            "{}{}\n{}{}",
            error
                .sqlstate
                .as_ref()
                .map(|state| format!("[{state}] "))
                .unwrap_or_default(),
            error.message,
            error.detail.as_deref().unwrap_or(""),
            error
                .hint
                .as_ref()
                .map(|hint| format!("\nHint: {hint}"))
                .unwrap_or_default()
        );
        frame.render_widget(
            Paragraph::new(detail)
                .style(Style::default().fg(DANGER))
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    if tab.columns.is_empty() {
        frame.render_widget(
            Paragraph::new(format!(
                "command complete · {} rows · {} ms",
                tab.affected_rows.unwrap_or(0),
                tab.elapsed_ms.unwrap_or(0)
            )),
            inner,
        );
        return;
    }
    let cell_width = 18usize;
    let visible_columns = ((inner.width as usize + 2) / (cell_width + 3)).max(1);
    let start_column = workspace.results.column_offset;
    let end_column = (start_column + visible_columns).min(tab.columns.len());
    let columns = start_column..end_column;
    let mut lines = Vec::new();
    lines.push(Line::styled(
        columns
            .clone()
            .map(|index| fit(&tab.columns[index], cell_width))
            .collect::<Vec<_>>()
            .join(" │ "),
        Style::default().fg(FOCUS).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        columns
            .clone()
            .map(|_| "─".repeat(cell_width))
            .collect::<Vec<_>>()
            .join("─┼─"),
        Style::default().fg(BORDER),
    ));
    let viewport = inner.height.saturating_sub(3) as usize;
    for (row_index, row) in tab
        .rows
        .iter()
        .enumerate()
        .skip(workspace.results.row_offset)
        .take(viewport)
    {
        let mut spans = Vec::new();
        for column in columns.clone() {
            if column > start_column {
                spans.push(Span::styled(" │ ", Style::default().fg(BORDER)));
            }
            let value = row
                .get(column)
                .and_then(|cell| cell.as_deref())
                .map(|value| fit(value, cell_width))
                .unwrap_or_else(|| fit("NULL", cell_width));
            let style = if row_index == workspace.results.row && column == workspace.results.column
            {
                Style::default().bg(SELECT).fg(TEXT)
            } else if row.get(column).is_some_and(Option::is_none) {
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
            } else {
                Style::default().fg(TEXT)
            };
            spans.push(Span::styled(value, style));
        }
        lines.push(Line::from(spans));
    }
    if tab.truncated {
        lines.push(Line::styled(
            "preview truncated · Ctrl+E streams full export",
            Style::default().fg(WARN),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn footer(frame: &mut Frame, workspace: &Workspace, area: Rect) {
    let hints = match workspace.focus {
        Pane::Catalog => "Tab pane  ↑↓ move  Enter insert  r refresh  F10 fullscreen",
        Pane::Editor => "Ctrl+Enter run  F5 all  F4 format  Ctrl+Space complete  Ctrl+N/W buffer",
        Pane::Results => "↑↓←→ navigate  [ ] result  Enter detail  y copy  Ctrl+E export",
    };
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    frame.render_widget(
        Paragraph::new(workspace.status.as_str()).style(Style::default().fg(
            if workspace.status.contains("실패") {
                DANGER
            } else {
                MUTED
            },
        )),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(format!("{hints}  ·  Esc close  ·  F8 history"))
            .style(Style::default().fg(MUTED)),
        rows[1],
    );
}

fn overlay_widget(frame: &mut Frame, workspace: &Workspace, overlay: &Overlay, area: Rect) {
    let rect = centered(
        area,
        70,
        match overlay {
            Overlay::History { .. } | Overlay::Connections { .. } => 70,
            Overlay::ProfileForm(_) => 86,
            Overlay::Message { .. } => 50,
            _ => 24,
        },
    );
    frame.render_widget(Clear, rect);
    let (title, body, danger): (String, String, bool) = match overlay {
        Overlay::Search(value) => (" Search ".into(), format!("/{value}"), false),
        Overlay::Goto(value) => (" Go to line ".into(), value.clone(), false),
        Overlay::Export(value) => (
            " Export full result ".into(),
            format!("Path: {value}\nThe query is streamed again; choose .csv, .json, or .jsonl."),
            false,
        ),
        Overlay::Password(value) => (
            " PostgreSQL password ".into(),
            format!(
                "{}\nEnter connect · Esc cancel",
                "•".repeat(value.chars().count())
            ),
            false,
        ),
        Overlay::ProfileTestPassword { input, .. } => (
            " Test connection password ".into(),
            format!(
                "{}\nEnter test · Esc back",
                "•".repeat(input.chars().count())
            ),
            false,
        ),
        Overlay::Message { title, body } => (format!(" {title} "), body.clone(), true),
        Overlay::History { cursor } => {
            let lines = workspace
                .history
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    format!(
                        "{} {}  {}  {} ms  {}",
                        if index == *cursor { "›" } else { " " },
                        entry.executed_at.format("%m-%d %H:%M:%S"),
                        entry.profile_label,
                        entry.elapsed_ms,
                        entry
                            .query_text
                            .as_deref()
                            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                            .unwrap_or_else(|| "(text disabled)".into())
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            (" Query history · Enter opens buffer ".into(), lines, false)
        }
        Overlay::Connections { cursor } => {
            let body = if workspace.profiles.is_empty() {
                "저장된 외부 connection이 없습니다.\n\nn 새 connection · Esc 닫기".into()
            } else {
                workspace
                    .profiles
                    .iter()
                    .enumerate()
                    .map(|(index, profile)| {
                        format!(
                            "{} {:<20} {}:{}/{}  {}  {}  secret:{}",
                            if index == *cursor { "›" } else { " " },
                            profile.name,
                            profile.host,
                            profile.port,
                            profile.database,
                            profile.tls_mode.as_str(),
                            profile.access_mode.label(),
                            if profile.credential_ref.is_some() {
                                "vault"
                            } else {
                                "prompt"
                            }
                        )
                    })
                    .chain(std::iter::once(
                        "\nEnter open · n new · e edit · t test · x delete · Esc close".into(),
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (" SQL Connections · F7 ".into(), body, false)
        }
        Overlay::ProfileForm(form) => {
            let values = [
                ("Name", form.name.clone()),
                ("Host", form.host.clone()),
                ("Port", form.port.clone()),
                ("Database", form.database.clone()),
                ("Username", form.username.clone()),
                ("TLS", form.tls_mode.as_str().into()),
                ("Default access", form.access_mode.label().into()),
                ("Root CA", form.root_ca_path.clone()),
                (
                    "Store password",
                    if form.store_password { "yes" } else { "no" }.into(),
                ),
                (
                    "Password",
                    if form.password.is_empty() {
                        if form.existing_secret {
                            "(unchanged vault secret)".into()
                        } else {
                            "(prompt when opened)".into()
                        }
                    } else {
                        "•".repeat(form.password.chars().count())
                    },
                ),
                (
                    "History text",
                    if form.history_text { "yes" } else { "no" }.into(),
                ),
                ("Preview rows", form.preview_rows.clone()),
            ];
            let mut body = values
                .into_iter()
                .enumerate()
                .map(|(index, (label, value))| {
                    format!(
                        "{} {:<16} {}",
                        if index == form.focus { "›" } else { " " },
                        label,
                        value
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            body.push_str("\n\nTab field · Space toggle · Ctrl+S test and save · Esc back");
            if form.tls_mode == crate::core::sql::TlsMode::Disable {
                body.push_str(
                    "\nWARNING: TLS is disabled; credentials and queries may be exposed.",
                );
            }
            (
                if form.editing.is_some() {
                    " Edit SQL Connection ".into()
                } else {
                    " New SQL Connection ".into()
                },
                body,
                form.tls_mode == crate::core::sql::TlsMode::Disable,
            )
        }
        Overlay::ConfirmForgetProfile { cursor } => (
            " Delete SQL Connection ".into(),
            format!(
                "Delete {} and its stored password? y / n",
                workspace
                    .profiles
                    .get(*cursor)
                    .map(|profile| profile.name.as_str())
                    .unwrap_or("this connection")
            ),
            true,
        ),
        Overlay::ConfirmReconnect => (
            " Reconnect ".into(),
            "The open transaction will be lost. Start a new session? y / n".into(),
            true,
        ),
        Overlay::ConfirmCloseBuffer => (
            " Unsaved buffer ".into(),
            "Discard this buffer? y / n".into(),
            true,
        ),
        Overlay::ConfirmExitTransaction => (
            " Open transaction ".into(),
            "Rollback the open transaction and close workspace? y / n".into(),
            true,
        ),
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(panel(title, true))
            .style(Style::default().fg(if danger { WARN } else { TEXT }))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn completions(frame: &mut Frame, workspace: &Workspace, area: Rect) {
    let height = (workspace.completions.len().min(8) + 2) as u16;
    let width = 46u16.min(area.width.saturating_sub(4));
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width + 2),
        area.y + 3,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let lines = workspace
        .completions
        .iter()
        .enumerate()
        .take(8)
        .map(|(index, item)| {
            Line::styled(
                format!("{}  {:?}", item.label, item.kind),
                if index == workspace.completion_index {
                    Style::default().bg(SELECT).fg(TEXT)
                } else {
                    Style::default().fg(TEXT)
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(panel(" Autocomplete ".into(), true)),
        rect,
    );
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height_percent) / 2),
        Constraint::Percentage(height_percent),
        Constraint::Percentage((100 - height_percent) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width_percent) / 2),
        Constraint::Percentage(width_percent),
        Constraint::Percentage((100 - width_percent) / 2),
    ])
    .split(vertical[1])[1]
}

fn fit(value: &str, width: usize) -> String {
    let value = value.replace('\n', "↵").replace('\r', "");
    if display_cols(&value) <= width {
        return format!("{value:<width$}");
    }
    let mut output = String::new();
    for ch in value.chars() {
        if display_cols(&output) + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) + 1
            > width
        {
            break;
        }
        output.push(ch);
    }
    output.push('…');
    output
}

fn spinner(tick: usize) -> &'static str {
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][tick % 10]
}
