//! The visual vocabulary every screen is built from.
//!
//! One place decides what a banner, a metric strip and a rule look like, so
//! the screens cannot drift apart. Nesting follows the concentric rule: the
//! outer shell is rounded, everything inside it is square, and a box never
//! sits directly inside another box of the same corner style.
//!
//! Every function is pure — plain data in, [`Line`]s out — so the exact
//! characters are asserted in tests instead of eyeballed in a terminal.

use crate::core::util::display_cols;
use crate::tui::theme::Theme;
use ratatui::text::{Line, Span};

/// Box-drawing characters, resolved once per frame from the theme.
///
/// Non-UTF-8 terminals get an ASCII set that keeps the *structure* legible
/// even though it is uglier: a rule is still a rule, a corner still a corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Top-left of a rounded shell.
    pub round_tl: &'static str,
    pub round_tr: &'static str,
    pub round_bl: &'static str,
    pub round_br: &'static str,
    pub horizontal: &'static str,
    pub vertical: &'static str,
    /// Column boundary on a header rule.
    pub cross: &'static str,
    /// Selected row marker.
    pub cursor: &'static str,
    /// Breadcrumb separator.
    pub crumb: &'static str,
}

impl Glyphs {
    pub fn of(theme: &Theme) -> Self {
        if theme.unicode {
            Self {
                round_tl: "╭",
                round_tr: "╮",
                round_bl: "╰",
                round_br: "╯",
                horizontal: "─",
                vertical: "│",
                cross: "┼",
                cursor: "▸",
                crumb: "›",
            }
        } else {
            Self {
                round_tl: "+",
                round_tr: "+",
                round_bl: "+",
                round_br: "+",
                horizontal: "-",
                vertical: "|",
                cross: "+",
                cursor: ">",
                crumb: ">",
            }
        }
    }

    fn line(self, cols: usize) -> String {
        self.horizontal.repeat(cols)
    }
}
/// The product's supplied three-row wordmark. Its broad filled strokes read
/// at terminal distance without the redundant sixth-row shadow treatment.
pub fn wordmark(theme: &Theme) -> Vec<Line<'static>> {
    const MARK: [&str; 3] = [
        "██     ▄████▄ ▄█████ ▄████▄ ██       ██ ███  ██ ██████ █████▄  ▄████▄ ",
        "██     ██  ██ ██     ██▄▄██ ██       ██ ██ ▀▄██ ██▄▄   ██▄▄██▄ ██▄▄██ ",
        "██████ ▀████▀ ▀█████ ██  ██ ██████   ██ ██   ██ ██     ██   ██ ██  ██",
    ];
    if !theme.unicode {
        return vec![Line::from(Span::styled(
            "LOCAL INFRA".to_string(),
            theme.heading(),
        ))];
    }
    let cols = MARK
        .iter()
        .map(|line| display_cols(line))
        .max()
        .unwrap_or(0);
    MARK.into_iter()
        .map(|line| {
            let mut line = line.to_string();
            line.push_str(&" ".repeat(cols.saturating_sub(display_cols(&line))));
            Line::from(Span::styled(line, theme.accent()))
        })
        .collect()
}

/// `──────── Identity ────────`, centred, for grouping inside a panel.
pub fn titled_rule(title: &str, width: u16, theme: &Theme) -> Line<'static> {
    let g = Glyphs::of(theme);
    let width = width as usize;
    let label = format!(" {title} ");
    let label_cols = display_cols(&label);
    if label_cols + 2 > width {
        return Line::from(Span::styled(g.line(width), theme.muted()));
    }
    let remaining = width - label_cols;
    let left = remaining / 2;
    let right = remaining - left;
    Line::from(vec![
        Span::styled(g.line(left), theme.muted()),
        Span::styled(label, theme.heading()),
        Span::styled(g.line(right), theme.muted()),
    ])
}

/// A plain divider.
pub fn rule(width: u16, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        Glyphs::of(theme).line(width as usize),
        theme.muted(),
    ))
}

/// The rule under a table header, with a `┼` at every column boundary.
///
/// The boundaries are what make a wide table scannable; without them the eye
/// has to re-find the columns on every row.
pub fn column_rule(widths: &[u16], spacing: u16, width: u16, theme: &Theme) -> Line<'static> {
    let g = Glyphs::of(theme);
    let width = width as usize;
    let mut out = String::with_capacity(width * 3);
    let mut x = 0usize;
    for (i, w) in widths.iter().enumerate() {
        if x >= width {
            break;
        }
        let w = *w as usize;
        let take = w.min(width - x);
        out.push_str(&g.line(take));
        x += take;
        let last = i + 1 == widths.len();
        if !last && spacing > 0 && x < width {
            // The gap between two columns carries the junction, so it lands
            // between cells rather than inside one.
            let gap = (spacing as usize).min(width - x);
            let mid = gap / 2;
            out.push_str(&g.line(mid));
            if mid < gap {
                out.push_str(g.cross);
                out.push_str(&g.line(gap - mid - 1));
            }
            x += gap;
        }
    }
    if x < width {
        out.push_str(&g.line(width - x));
    }
    Line::from(Span::styled(out, theme.muted()))
}

/// `q quit   Tab focus   Enter select` — keys bold, labels plain, no brackets.
///
/// Brackets around every key turn the footer into a wall of punctuation; the
/// weight difference is enough to find the key.
pub fn hint_row(pairs: &[(String, String)], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, label) in pairs {
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(key.clone(), theme.key()));
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(label.clone(), theme.normal()));
        spans.push(Span::raw("  ".to_string()));
    }
    Line::from(spans)
}

/// `local › projects › my-app`, for a path the user is inside.
pub fn breadcrumb(parts: &[String], theme: &Theme) -> Vec<Span<'static>> {
    let g = Glyphs::of(theme);
    let mut spans = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(format!(" {} ", g.crumb), theme.muted()));
        }
        let last = i + 1 == parts.len();
        spans.push(Span::styled(
            part.clone(),
            if last { theme.normal() } else { theme.muted() },
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::rows::render_lines;

    fn plain() -> Theme {
        Theme::plain()
    }

    fn unicode() -> Theme {
        let mut theme = Theme::plain();
        theme.unicode = true;
        theme
    }

    #[test]
    fn wordmark_uses_the_supplied_three_row_full_name_mark() {
        let mark = wordmark(&unicode());
        let text = render_lines(&mark);
        assert_eq!(text.lines().count(), 3);
        assert!(text.contains('█'));
        assert!(text.contains('▄'));
        assert!(text.contains('▀'));
        assert!(!text.contains('╭'));
        let widths: Vec<usize> = mark
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| display_cols(&span.content))
                    .sum()
            })
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "centred rows must share one width: {widths:?}"
        );

        let ascii = render_lines(&wordmark(&plain()));
        assert_eq!(ascii, "LOCAL INFRA");
        assert!(ascii.is_ascii());
    }

    #[test]
    fn a_titled_rule_centres_its_title_and_fills_the_width() {
        let theme = unicode();
        let line = titled_rule("Identity", 40, &theme);
        let cols: usize = line.spans.iter().map(|s| display_cols(&s.content)).sum();
        assert_eq!(cols, 40);
        let text = render_lines(&[line]);
        assert!(text.contains(" Identity "));
        assert!(text.starts_with('─'));
        assert!(text.trim_end().ends_with('─'));
    }

    #[test]
    fn a_title_wider_than_the_rule_degrades_to_a_plain_rule() {
        let theme = unicode();
        let line = titled_rule("an extremely long section title", 10, &theme);
        let cols: usize = line.spans.iter().map(|s| display_cols(&s.content)).sum();
        assert_eq!(cols, 10);
    }

    #[test]
    fn the_column_rule_puts_a_junction_between_cells_not_inside_one() {
        let theme = unicode();
        let line = column_rule(&[6, 4], 1, 11, &theme);
        let text = render_lines(&[line]);
        assert_eq!(display_cols(&text), 11);
        assert_eq!(
            text.chars().nth(6),
            Some('┼'),
            "the junction sits in the gap: {text}"
        );
    }

    #[test]
    fn the_column_rule_never_exceeds_the_width_it_was_given() {
        let theme = unicode();
        for width in [1u16, 5, 12, 40] {
            let line = column_rule(&[10, 10, 10], 2, width, &theme);
            let text = render_lines(&[line]);
            assert_eq!(display_cols(&text), width as usize, "width {width}");
        }
    }

    #[test]
    fn hints_are_key_then_label_with_no_brackets() {
        let theme = unicode();
        let pairs = [
            ("n".to_string(), "새 리소스".to_string()),
            ("q".to_string(), "종료".to_string()),
        ];
        let text = render_lines(&[hint_row(&pairs, &theme)]);
        assert!(text.contains("n 새 리소스"), "{text}");
        assert!(text.contains("q 종료"), "{text}");
        assert!(!text.contains('['), "no punctuation walls: {text}");
    }

    #[test]
    fn a_breadcrumb_separates_with_a_single_glyph_and_marks_the_leaf() {
        let theme = unicode();
        let spans = breadcrumb(&["local".to_string(), "postgres 17".to_string()], &theme);
        let text = render_lines(&[Line::from(spans)]);
        assert_eq!(text, "local › postgres 17");
    }
}
