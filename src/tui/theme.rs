//! Colour, symbols and glyph fallbacks (PRD §12.4, §14.2).
//!
//! Three rules drive everything here:
//!
//! * The terminal's own background is never painted over, so the app reads on
//!   light and dark themes alike.
//! * Colour is only ever an *additional* carrier of meaning; every state also
//!   has a symbol and a word.
//! * `NO_COLOR` and non-UTF-8 terminals degrade to plain ASCII without losing
//!   information.

use crate::core::config::Config;
use crate::core::model::{ActivityStatus, EngineStatus, Health, TunnelStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::BorderType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    pub unicode: bool,
    pub reduced_motion: bool,
}

impl Theme {
    pub fn from_config(config: &Config) -> Self {
        Self {
            color: config.color_enabled(),
            unicode: config.unicode_enabled(),
            reduced_motion: config.ui.reduced_motion,
        }
    }

    pub fn plain() -> Self {
        Self {
            color: false,
            unicode: false,
            reduced_motion: true,
        }
    }

    fn fg(&self, color: Color) -> Style {
        if self.color {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    // -- text roles ---------------------------------------------------------

    pub fn normal(&self) -> Style {
        Style::default()
    }

    /// Dimmed rather than grey, so it works on any background.
    pub fn muted(&self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    pub fn heading(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    pub fn ok(&self) -> Style {
        self.fg(Color::Green)
    }

    pub fn warn(&self) -> Style {
        self.fg(Color::Yellow)
    }

    pub fn danger(&self) -> Style {
        self.fg(Color::Red).add_modifier(Modifier::BOLD)
    }

    /// The terminal's single identity colour: a vivid violet that is reserved
    /// for the wordmark, active tabs, focus and keyboard affordances.
    pub fn accent(&self) -> Style {
        self.fg(Color::Rgb(108, 76, 244))
    }

    /// Selection is reversed, never a background fill: it survives `NO_COLOR`
    /// and unusual palettes.
    pub fn selected(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }

    /// A focused frame is violet and bold. Everything else is quiet enough
    /// that the active tab and the selected row carry the hierarchy.
    pub fn border(&self, focused: bool) -> Style {
        if focused {
            let base = Style::default().add_modifier(Modifier::BOLD);
            if self.color {
                base.fg(Color::Rgb(108, 76, 244))
            } else {
                base
            }
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Structural frames have softened corners in a Unicode terminal; ASCII
    /// keeps an unambiguous fallback shape.
    pub fn border_type(&self) -> BorderType {
        if self.unicode {
            BorderType::Rounded
        } else {
            BorderType::QuadrantInside
        }
    }

    /// Key hints share the one identity colour with the active tab.
    pub fn key(&self) -> Style {
        let base = Style::default().add_modifier(Modifier::BOLD);
        if self.color {
            base.fg(Color::Rgb(108, 76, 244))
        } else {
            base
        }
    }

    // -- symbols ------------------------------------------------------------

    pub fn engine_symbol(&self, status: &EngineStatus) -> &'static str {
        let symbol = status.symbol();
        if self.unicode {
            symbol
        } else {
            match symbol {
                "●" => "*",
                "○" => "o",
                "!" => "!",
                _ => ".",
            }
        }
    }

    pub fn engine_style(&self, status: &EngineStatus) -> Style {
        if !status.exists {
            self.muted()
        } else if status.health == Health::Unhealthy {
            self.danger()
        } else if status.running {
            self.ok()
        } else {
            self.muted()
        }
    }

    pub fn tunnel_symbol(&self, status: TunnelStatus) -> &'static str {
        if self.unicode {
            status.symbol()
        } else {
            match status {
                TunnelStatus::Active => "*",
                TunnelStatus::Stopped => "o",
                TunnelStatus::Failed => "!",
            }
        }
    }

    pub fn tunnel_style(&self, status: TunnelStatus) -> Style {
        match status {
            TunnelStatus::Active => self.ok(),
            TunnelStatus::Stopped => self.muted(),
            TunnelStatus::Failed => self.danger(),
        }
    }

    pub fn activity_symbol(&self, status: ActivityStatus) -> &'static str {
        if self.unicode {
            status.symbol()
        } else {
            match status {
                ActivityStatus::Started => "..",
                ActivityStatus::Ok => "ok",
                ActivityStatus::Failed => "!!",
                ActivityStatus::RolledBack => "<-",
            }
        }
    }

    pub fn activity_style(&self, status: ActivityStatus) -> Style {
        match status {
            ActivityStatus::Ok => self.ok(),
            ActivityStatus::Failed => self.danger(),
            ActivityStatus::RolledBack => self.warn(),
            ActivityStatus::Started => self.muted(),
        }
    }

    /// Spinner frames, or a static marker under reduced motion (§12.4).
    pub fn spinner(&self, tick: usize) -> &'static str {
        if self.reduced_motion {
            return "…";
        }
        const UNICODE: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
        if self.unicode {
            UNICODE[tick % UNICODE.len()]
        } else {
            ASCII[tick % ASCII.len()]
        }
    }

    pub fn ellipsis(&self) -> &'static str {
        if self.unicode {
            "…"
        } else {
            "..."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(running: bool, health: Health, exists: bool) -> EngineStatus {
        EngineStatus {
            exists,
            running,
            state: if running {
                "running".into()
            } else {
                "exited".into()
            },
            health,
            image: None,
            started_at: None,
        }
    }

    #[test]
    fn state_is_readable_without_colour() {
        let t = Theme::plain();
        assert_eq!(t.engine_symbol(&status(true, Health::None, true)), "*");
        assert_eq!(t.engine_symbol(&status(false, Health::None, true)), "o");
        assert_eq!(t.engine_symbol(&status(true, Health::Unhealthy, true)), "!");
        assert_eq!(t.engine_symbol(&EngineStatus::missing()), ".");
        assert_eq!(t.tunnel_symbol(TunnelStatus::Active), "*");
        assert_eq!(t.activity_symbol(ActivityStatus::Failed), "!!");
    }

    #[test]
    fn unicode_terminals_get_the_prd_symbols() {
        let t = Theme {
            color: true,
            unicode: true,
            reduced_motion: false,
        };
        assert_eq!(t.engine_symbol(&status(true, Health::None, true)), "●");
        assert_eq!(t.engine_symbol(&status(false, Health::None, true)), "○");
        assert_eq!(t.tunnel_symbol(TunnelStatus::Failed), "!");
        assert_eq!(t.ellipsis(), "…");
    }

    #[test]
    fn no_color_theme_emits_no_foreground_colours() {
        let t = Theme::plain();
        assert_eq!(t.ok().fg, None);
        assert_eq!(t.danger().fg, None);
        assert_eq!(t.accent().fg, None);
        // Selection and focus survive without colour.
        assert!(t.selected().add_modifier.contains(Modifier::REVERSED));
        assert!(t.border(true).add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn reduced_motion_freezes_the_spinner() {
        let still = Theme {
            color: true,
            unicode: true,
            reduced_motion: true,
        };
        assert_eq!(still.spinner(0), still.spinner(7));

        let moving = Theme {
            color: true,
            unicode: true,
            reduced_motion: false,
        };
        assert_ne!(moving.spinner(0), moving.spinner(1));
    }

    #[test]
    fn danger_is_distinguishable_by_modifier_not_only_hue() {
        let t = Theme {
            color: true,
            unicode: true,
            reduced_motion: false,
        };
        assert!(t.danger().add_modifier.contains(Modifier::BOLD));
    }
}
