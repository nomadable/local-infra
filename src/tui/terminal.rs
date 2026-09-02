//! Terminal ownership: raw mode, alternate screen, and restoring both no matter
//! how the process ends (TUI-007, PRD §12.3).
//!
//! A TUI that panics while the terminal is in raw mode leaves the user's shell
//! unusable. Two mechanisms guarantee restoration:
//!
//! * [`TerminalGuard`] restores on `Drop`, covering normal exit, `?`
//!   propagation and unwinding panics.
//! * [`install_panic_hook`] restores *before* the default hook prints, so the
//!   backtrace is readable, and covers the case where the guard itself is not
//!   on the unwind path.

use crate::core::error::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set while the terminal is in raw mode, so the panic hook knows whether it
/// has anything to undo and never double-restores.
static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);
static MOUSE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub struct TerminalGuard {
    pub terminal: Tui,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen. The alternate screen is what
    /// keeps displayed passwords out of the scrollback (PRD §11.2).
    pub fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode()?;
        RAW_ACTIVE.store(true, Ordering::SeqCst);
        match finish_enter(mouse) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }

    /// Leave the alternate screen temporarily, run `f` on the real terminal,
    /// then come back. Used for `Ctrl+Z` suspend and for shelling out.
    pub fn suspended<T>(&mut self, f: impl FnOnce() -> T) -> Result<T> {
        let mouse = MOUSE_ACTIVE.load(Ordering::SeqCst);
        restore();
        let value = f();
        enable_raw_mode()?;
        RAW_ACTIVE.store(true, Ordering::SeqCst);
        match finish_enter(mouse) {
            Ok(terminal) => {
                self.terminal = terminal;
                self.terminal.clear()?;
                Ok(value)
            }
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn finish_enter(mouse: bool) -> Result<Tui> {
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
        MOUSE_ACTIVE.store(true, Ordering::SeqCst);
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Idempotent teardown. Safe to call from a panic hook, a signal handler and
/// `Drop` in any order.
pub fn restore() {
    if !RAW_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = stdout();
    if MOUSE_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(out, crossterm::cursor::Show);
}

/// Restore the terminal before the default panic output is printed.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Minimum usable size (PRD §12.1). Below this the app renders a single
pub const MIN_WIDTH: u16 = crate::core::doctor::MIN_TERMINAL_WIDTH;
pub const MIN_HEIGHT: u16 = crate::core::doctor::MIN_TERMINAL_HEIGHT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeCheck {
    pub width: u16,
    pub height: u16,
}

impl SizeCheck {
    pub fn ok(&self) -> bool {
        self.width >= MIN_WIDTH && self.height >= MIN_HEIGHT
    }

    /// Below 100 columns the master-detail split collapses to a single stacked
    /// column (PRD §7.1).
    pub fn stacked(&self) -> bool {
        self.width < 100
    }

    /// Below 80 columns even the left navigation is dropped.
    pub fn hide_nav(&self) -> bool {
        self.width < MIN_WIDTH
    }

    pub fn message(&self) -> String {
        format!(
            "터미널이 너무 작습니다: {}×{} (최소 {}×{})\n창을 넓히거나 글꼴 크기를 줄이세요.",
            self.width, self.height, MIN_WIDTH, MIN_HEIGHT
        )
    }
}

pub fn size() -> SizeCheck {
    let (width, height) = crossterm::terminal::size().unwrap_or((MIN_WIDTH, MIN_HEIGHT));
    SizeCheck { width, height }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_idempotent_and_cheap_when_nothing_was_entered() {
        RAW_ACTIVE.store(false, Ordering::SeqCst);
        restore();
        restore();
        assert!(!RAW_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn layout_thresholds_match_the_prd() {
        assert!(SizeCheck {
            width: 80,
            height: 24
        }
        .ok());
        assert!(!SizeCheck {
            width: 79,
            height: 24
        }
        .ok());
        assert!(!SizeCheck {
            width: 80,
            height: 23
        }
        .ok());
        assert!(SizeCheck {
            width: 99,
            height: 30
        }
        .stacked());
        assert!(!SizeCheck {
            width: 100,
            height: 30
        }
        .stacked());
        assert!(SizeCheck {
            width: 70,
            height: 30
        }
        .hide_nav());
    }

    #[test]
    fn too_small_message_states_actual_and_required_size() {
        let m = SizeCheck {
            width: 60,
            height: 20,
        }
        .message();
        assert!(m.contains("60×20"));
        assert!(m.contains("80×24"));
    }
}
