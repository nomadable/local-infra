//! Geometry (PRD §7.1, TUI-002/003).
//!
//! Every decision about *where* something goes is a pure function over a
//! [`Rect`] and a [`SizeCheck`], so the responsive behaviour — the stacked
//! fallback under 100 columns, the dropped navigation under 80, the
//! too-small screen — is unit-testable without a terminal.

use crate::tui::terminal::SizeCheck;
use ratatui::layout::{Constraint, Layout, Rect};

/// The horizontal bands every screen is drawn into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shell {
    /// Three-row wordmark plus one quiet breathing row; deliberately unframed.
    pub status: Rect,
    /// Rounded menu-tab frame. Zero-height during first-run setup.
    pub nav: Rect,
    pub nav_visible: bool,
    /// Rounded active-engine frame. Zero-height when the body needs the room.
    pub strip: Rect,
    pub strip_visible: bool,
    /// One rounded frame for the active tab's working area.
    pub body: Rect,
    /// Keys valid in the current focus.
    pub hints: Rect,
}

/// How a master-detail screen is arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// List left, detail right.
    Columns,
    /// List above, detail below — the narrow-terminal fallback (PRD §7.1).
    Stacked,
}

/// Three compact wordmark rows plus one row of breathing room.
const BANNER_ROWS: u16 = 4;
/// The menu tabs live in a three-row rounded frame.
const NAV_ROWS: u16 = 3;
/// Active engines carry a title and two service lines inside a rounded frame.
const STRIP_ROWS: u16 = 4;
/// Body rows below which the engine frame is dropped: a table still needs room
/// to be usable.
const MIN_BODY_WITH_STRIP: u16 = 10;

pub fn shell(area: Rect, size: SizeCheck, hint_rows: u16) -> Shell {
    shell_bands(area, size, hint_rows, true, true)
}

/// Same bands as [`shell`], with the nav row forced off during first-run
/// setup so the only thing on screen is the current step.
pub fn shell_nav(area: Rect, size: SizeCheck, hint_rows: u16, allow_nav: bool) -> Shell {
    shell_bands(area, size, hint_rows, allow_nav, allow_nav)
}

pub fn shell_bands(
    area: Rect,
    size: SizeCheck,
    hint_rows: u16,
    allow_nav: bool,
    allow_strip: bool,
) -> Shell {
    let nav_visible = allow_nav && !size.hide_nav();
    let hints_rows = hint_rows.clamp(1, 3);
    let fixed = BANNER_ROWS + if nav_visible { NAV_ROWS } else { 0 } + hints_rows;
    let strip_visible = allow_strip
        && area
            .height
            .saturating_sub(fixed + STRIP_ROWS)
            .ge(&MIN_BODY_WITH_STRIP);
    let [status, nav, strip, body, hints] = Layout::vertical([
        Constraint::Length(BANNER_ROWS),
        Constraint::Length(if nav_visible { NAV_ROWS } else { 0 }),
        Constraint::Length(if strip_visible { STRIP_ROWS } else { 0 }),
        Constraint::Min(1),
        Constraint::Length(hints_rows),
    ])
    .areas(area);
    Shell {
        status,
        nav,
        nav_visible,
        strip,
        strip_visible,
        body,
        hints,
    }
}

pub fn split_mode(size: SizeCheck) -> Split {
    if size.stacked() {
        Split::Stacked
    } else {
        Split::Columns
    }
}

/// The list gets the larger share in both orientations: it is what the user
/// navigates, and the detail pane is a readout.
pub fn master_detail(body: Rect, mode: Split) -> (Rect, Rect) {
    let constraints = [Constraint::Percentage(58), Constraint::Percentage(42)];
    let [list, detail] = match mode {
        Split::Columns => Layout::horizontal(constraints).areas(body),
        Split::Stacked => Layout::vertical(constraints).areas(body),
    };
    (list, detail)
}

/// A centred overlay of at most `cols`×`rows`, always inside `area`.
pub fn popup(area: Rect, cols: u16, rows: u16) -> Rect {
    let width = cols.min(area.width);
    let height = rows.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Split a body into a fixed-height top band and the rest. Used by the form's
/// live plan preview and the activity detail.
pub fn top_band(area: Rect, rows: u16) -> (Rect, Rect) {
    let rows = rows.min(area.height);
    let [top, rest] = Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(area);
    (top, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u16, height: u16) -> SizeCheck {
        SizeCheck { width, height }
    }

    #[test]
    fn shell_layers_a_compact_wordmark_menu_engines_and_one_work_frame() {
        let s = shell(Rect::new(0, 0, 120, 40), size(120, 40), 1);
        assert!(s.nav_visible);
        assert!(s.strip_visible);
        assert_eq!(s.status.height, 4);
        assert_eq!(s.nav.height, 3);
        assert_eq!(s.strip.height, 4);
        assert_eq!(s.hints.height, 1);
        assert_eq!(s.body.height, 28);
        assert_eq!(s.body.y, 11, "the three framed layers precede the work");
    }

    #[test]
    fn a_two_row_hint_band_takes_its_row_from_the_work_frame() {
        let s = shell(Rect::new(0, 0, 120, 40), size(120, 40), 2);
        assert_eq!(s.hints.height, 2);
        assert_eq!(s.body.height, 27);
    }

    #[test]
    fn menu_tabs_drop_below_eighty_columns_and_the_body_gains_the_frame() {
        let s = shell(Rect::new(0, 0, 70, 24), size(70, 24), 1);
        assert!(!s.nav_visible);
        assert_eq!(s.nav.height, 0);
        assert_eq!(s.body.height, 15);
    }

    /// The engine card disappears before the large work frame drops below the
    /// smallest height that can still scroll a table.
    #[test]
    fn a_short_terminal_drops_the_engine_card_before_the_work_frame() {
        let tall = shell(Rect::new(0, 0, 120, 40), size(120, 40), 1);
        assert!(tall.strip_visible);
        let short = shell(Rect::new(0, 0, 120, 17), size(120, 17), 2);
        assert!(!short.strip_visible);
        assert_eq!(short.strip.height, 0);
        assert_eq!(short.body.height, 8, "the work frame keeps every row");
    }

    #[test]
    fn ninety_nine_columns_stacks_and_one_hundred_splits() {
        assert_eq!(split_mode(size(99, 30)), Split::Stacked);
        assert_eq!(split_mode(size(100, 30)), Split::Columns);
    }

    #[test]
    fn stacked_puts_the_list_above_the_detail_like_the_prd_mock() {
        let body = Rect::new(0, 2, 80, 20);
        let (list, detail) = master_detail(body, Split::Stacked);
        assert_eq!(list.width, detail.width);
        assert!(list.y < detail.y);
        assert!(list.height >= detail.height);
    }

    #[test]
    fn columns_put_the_list_left_of_the_detail() {
        let body = Rect::new(0, 2, 120, 20);
        let (list, detail) = master_detail(body, Split::Columns);
        assert_eq!(list.height, detail.height);
        assert!(list.x < detail.x);
        assert!(list.width >= detail.width);
    }

    #[test]
    fn a_popup_never_escapes_its_container() {
        let area = Rect::new(0, 0, 40, 10);
        let p = popup(area, 200, 200);
        assert_eq!((p.x, p.y, p.width, p.height), (0, 0, 40, 10));

        let p = popup(area, 20, 6);
        assert_eq!((p.x, p.y, p.width, p.height), (10, 2, 20, 6));
    }
}
