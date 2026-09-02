//! Click targets for the last drawn frame.
//!
//! Geometry is computed from the same layout functions the renderer uses, so a
//! click lands on the thing the user saw. The event loop stores the result and
//! looks up `(column, row)` on button-down.

use crate::tui::form::Field;
use crate::tui::hints::{self, Hint};
use crate::tui::keymap::{Action, Screen};
use crate::tui::layout;

use crate::tui::modal::Modal;
use crate::tui::onboard;
use crate::tui::render::View;
use crate::tui::terminal::SizeCheck;
use ratatui::layout::Rect;

const MAX_HINT_ROWS: usize = 2;
const MAX_FORM_COLS: u16 = 80;
const MAX_POPUP_COLS: u16 = 78;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Nav(Screen),
    /// Visible table row, already offset-adjusted.
    Row(usize),
    Action(Action),
    /// Advance the first-run guide (Enter).
    AdvanceOnboard,
    FormField(Field),
    FormChoice {
        field: Field,
        index: usize,
    },
    /// Clicked the overlay chrome or empty overlay body.
    Dismiss,
}

#[derive(Debug, Clone, Default)]
pub struct Hits {
    nav: Vec<(Rect, Screen)>,
    rows: Vec<(Rect, usize)>,
    hints: Vec<(Rect, Action)>,
    onboard: Option<Rect>,
    form_fields: Vec<(Rect, Field)>,
    form_choices: Vec<(Rect, Field, usize)>,
    overlay: Option<Rect>,
    /// True when a click outside the overlay should close it.
    dismiss_outside: bool,
    body: Option<Rect>,
}

impl Hits {
    pub fn at(&self, column: u16, row: u16) -> Option<Hit> {
        if let Some(overlay) = self.overlay {
            if overlay.holds(column, row) {
                if let Some((_, field, index)) = self
                    .form_choices
                    .iter()
                    .find(|(rect, _, _)| rect.holds(column, row))
                {
                    return Some(Hit::FormChoice {
                        field: *field,
                        index: *index,
                    });
                }
                if let Some((_, field)) = self
                    .form_fields
                    .iter()
                    .find(|(rect, _)| rect.holds(column, row))
                {
                    return Some(Hit::FormField(*field));
                }
                return None;
            }
            if self.dismiss_outside {
                return Some(Hit::Dismiss);
            }
            return None;
        }
        if let Some((_, screen)) = self.nav.iter().find(|(rect, _)| rect.holds(column, row)) {
            return Some(Hit::Nav(*screen));
        }
        if let Some((_, action)) = self.hints.iter().find(|(rect, _)| rect.holds(column, row)) {
            return Some(Hit::Action(*action));
        }
        if self.onboard.is_some_and(|rect| rect.holds(column, row)) {
            return Some(Hit::AdvanceOnboard);
        }
        if let Some((_, index)) = self.rows.iter().find(|(rect, _)| rect.holds(column, row)) {
            return Some(Hit::Row(*index));
        }
        None
    }

    pub fn scroll_over_list(&self, column: u16, row: u16) -> bool {
        if self.overlay.is_some() {
            return false;
        }
        self.rows.iter().any(|(rect, _)| rect.holds(column, row))
            || self.body.is_some_and(|rect| rect.holds(column, row))
    }
}

trait HoldsPoint {
    fn holds(self, column: u16, row: u16) -> bool;
}

impl HoldsPoint for Rect {
    fn holds(self, column: u16, row: u16) -> bool {
        column >= self.x
            && column < self.x.saturating_add(self.width)
            && row >= self.y
            && row < self.y.saturating_add(self.height)
    }
}

pub fn compute(view: &View<'_>, area: Rect, table_offset: usize) -> Hits {
    let size = SizeCheck {
        width: area.width,
        height: area.height,
    };
    if !size.ok() {
        return Hits::default();
    }

    let hints = hints::hints(view.keymap, view.hint_context());
    let toast_cols = view
        .toast
        .map(|t| (crate::core::util::display_cols(&t.text) as u16 + 2).min(area.width / 2))
        .unwrap_or(0);
    let hint_width = area.width.saturating_sub(toast_cols) as usize;
    let (hint_rows, _) = if view.filter.is_some() {
        (Vec::new(), false)
    } else {
        hints::wrap(&hints, hint_width, MAX_HINT_ROWS)
    };
    let setup = onboard::phase(view.snapshot).active();
    let shell = layout::shell_nav(area, size, hint_rows.len().max(1) as u16, !setup);

    let mut hits = Hits {
        body: Some(inset(shell.body)),
        ..Hits::default()
    };

    if shell.nav_visible {
        hits.nav = nav_hits(inset(shell.nav));
    }

    let hint_body = if toast_cols > 0 {
        let width = shell.hints.width.saturating_sub(toast_cols);
        Rect {
            x: shell.hints.x,
            y: shell.hints.y,
            width,
            height: shell.hints.height,
        }
    } else {
        shell.hints
    };
    hits.hints = hint_hits(&hint_rows, hint_body);

    if view.screen == Screen::Resources && onboard::phase(view.snapshot).active() {
        hits.onboard = Some(inset(shell.body));
    } else if view.modal.is_none() {
        hits.rows = row_hits(inset(shell.body), view.table.len(), table_offset);
    }

    if let Some(modal) = view.modal {
        let (cols, rows_high) = overlay_size(modal, area);
        let overlay = layout::popup(area, cols, rows_high);
        hits.overlay = Some(overlay);
        hits.dismiss_outside = matches!(
            modal,
            Modal::Help { .. } | Modal::Palette(_) | Modal::Message(_) | Modal::Detail { .. }
        );

        if let Modal::Form(form) = modal {
            let mapped = form.layout_hits(inset(overlay));
            hits.form_fields = mapped.fields;
            hits.form_choices = mapped.choices;
        }
    }

    hits
}

fn nav_hits(nav: Rect) -> Vec<(Rect, Screen)> {
    let content_width = crate::tui::rows::nav_line_width().min(nav.width);
    let origin = nav
        .x
        .saturating_add(nav.width.saturating_sub(content_width) / 2);
    Screen::ALL
        .into_iter()
        .map(|screen| {
            let (offset, width) = crate::tui::rows::nav_tab_offset(screen);
            (
                Rect {
                    x: origin.saturating_add(offset),
                    y: nav.y,
                    width,
                    height: 1,
                },
                screen,
            )
        })
        .collect()
}

/// Mirrors [`crate::tui::chrome::hint_row`], which draws each entry as
/// `" key label  "`: one leading space, then the pair, then a two-space gap.
fn hint_hits(rows: &[Vec<Hint>], area: Rect) -> Vec<(Rect, Action)> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let y = area.y.saturating_add(i as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        let mut x = area.x;
        for hint in row {
            x = x.saturating_add(1);
            let width = hint.cols() as u16;
            if let Some(action) = hint.action {
                out.push((
                    Rect {
                        x,
                        y,
                        width,
                        height: 1,
                    },
                    action,
                ));
            }
            x = x.saturating_add(width).saturating_add(2);
        }
    }
    out
}

/// Rows start below the header *and* its rule, so a click two rows into the
/// panel is the first row, not the second.
const HEADER_ROWS: u16 = 2;

fn row_hits(inner: Rect, count: usize, offset: usize) -> Vec<(Rect, usize)> {
    if inner.height <= HEADER_ROWS || count == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let visible = inner.height.saturating_sub(HEADER_ROWS) as usize;
    for i in 0..visible.min(count.saturating_sub(offset)) {
        let index = offset + i;
        out.push((
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(HEADER_ROWS).saturating_add(i as u16),
                width: inner.width,
                height: 1,
            },
            index,
        ));
    }
    out
}

fn overlay_size(modal: &Modal, area: Rect) -> (u16, u16) {
    match modal {
        Modal::Help { .. } => (76, area.height.saturating_sub(4).max(10)),
        Modal::Palette(_) => (72, 20),
        Modal::Form(_) => (MAX_FORM_COLS, area.height.saturating_sub(2).max(20)),
        Modal::SshForm(_) => (72, area.height.saturating_sub(4).max(20)),

        Modal::Confirm(_) => (70, 22),
        Modal::Message(_) => (MAX_POPUP_COLS, 22),
        Modal::Detail { .. } => (72, area.height.saturating_sub(4).max(12)),
    }
}

fn inset(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::doctor::Check;
    use crate::tui::data::{fixture, Snapshot};
    use crate::tui::hints::Focus;
    use crate::tui::keymap::Keymap;
    use crate::tui::render::View;
    use crate::tui::rows::{self, Endpoint};
    use crate::tui::theme::Theme;
    use chrono::Utc;

    #[test]
    fn a_nav_click_selects_the_screen() {
        let snap = fixture::snapshot();
        let keymap = Keymap::defaults();
        let theme = Theme::plain();
        let config = Config::default();
        let empty: Vec<String> = Vec::new();
        let endpoint = Endpoint::default();
        let table = rows::table_for(Screen::Resources, &snap, &config, "", &theme);
        let view = View {
            snapshot: &snap,
            config: &config,
            theme: &theme,
            keymap: &keymap,
            screen: Screen::Resources,
            focus: Focus::List,
            table: &table,
            cursor: 0,
            detail_scroll: 0,
            filter: None,
            modal: None,
            reveal: false,
            endpoint: &endpoint,
            engine_status: None,
            job: None,
            job_log: &[],
            tick: 0,
            notices: &empty,
            keymap_problems: &empty,
            toast: None,
            now: Utc::now(),
        };
        let hits = compute(&view, Rect::new(0, 0, 120, 30), 0);
        let nav_width = 118;
        let origin = 1 + (nav_width - crate::tui::rows::nav_line_width()) / 2;
        assert_eq!(hits.at(origin, 5), Some(Hit::Nav(Screen::Resources)));
        let (targets_x, _) = crate::tui::rows::nav_tab_offset(Screen::Targets);
        assert_eq!(
            hits.at(origin.saturating_add(targets_x), 5),
            Some(Hit::Nav(Screen::Targets))
        );
    }

    #[test]
    fn an_empty_home_screen_click_advances_onboarding() {
        let mut snap = Snapshot::empty();
        snap.checks.push(Check {
            name: "Docker CLI".into(),
            ok: true,
            detail: "docker".into(),
            remedy: None,
        });
        let keymap = Keymap::defaults();
        let theme = Theme::plain();
        let config = Config::default();
        let empty: Vec<String> = Vec::new();
        let endpoint = Endpoint::default();
        let table = rows::table_for(Screen::Resources, &snap, &config, "", &theme);
        let view = View {
            snapshot: &snap,
            config: &config,
            theme: &theme,
            keymap: &keymap,
            screen: Screen::Resources,
            focus: Focus::List,
            table: &table,
            cursor: 0,
            detail_scroll: 0,
            filter: None,
            modal: None,
            reveal: false,
            endpoint: &endpoint,
            engine_status: None,
            job: None,
            job_log: &[],
            tick: 0,
            notices: &empty,
            keymap_problems: &empty,
            toast: None,
            now: Utc::now(),
        };
        let hits = compute(&view, Rect::new(0, 0, 120, 30), 0);
        assert_eq!(hits.at(10, 8), Some(Hit::AdvanceOnboard));
    }

    #[test]
    fn at_prefers_overlay_over_the_body() {
        let hits = Hits {
            onboard: Some(Rect::new(0, 0, 80, 20)),
            overlay: Some(Rect::new(10, 5, 20, 8)),
            dismiss_outside: true,
            ..Hits::default()
        };
        assert_eq!(hits.at(0, 0), Some(Hit::Dismiss));
        assert_eq!(hits.at(12, 6), None);
    }
}
