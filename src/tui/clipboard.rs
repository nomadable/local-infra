//! Clipboard writing (PRD §7.6, §11.2).
//!
//! Two transports, tried in order:
//!
//! 1. **OSC 52** — an escape sequence the *terminal emulator* interprets, so it
//!    works through SSH and tmux where a local helper binary would put the text
//!    on the wrong machine. Disabled by `ui.osc52 = false` for untrusted relays.
//! 2. **A platform helper** (`pbcopy`, `wl-copy`, `xclip`) for terminals that
//!    do not implement OSC 52.
//!
//! Secrets copied this way can be scheduled for erasure with
//! `ui.clipboard_clear_seconds`.

use crate::core::config::UiConfig;
use crate::core::error::{Error, Result};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Osc52,
    Helper,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyOutcome {
    pub transport: Transport,
    /// Set when the value will be wiped again.
    pub clears_in: Option<u64>,
}

impl CopyOutcome {
    pub fn message(&self, what: &str) -> String {
        let via = match self.transport {
            Transport::Osc52 => "OSC 52",
            Transport::Helper => "시스템 클립보드",
        };
        match self.clears_in {
            Some(secs) => format!("{what}을(를) 복사했습니다 ({via}, {secs}초 후 자동 삭제)"),
            None => format!("{what}을(를) 복사했습니다 ({via})"),
        }
    }
}

/// Copy `text`. `secret` selects whether the auto-clear timer applies.
pub fn copy(ui: &UiConfig, text: &str, secret: bool) -> Result<CopyOutcome> {
    let clears_in =
        (secret && ui.clipboard_clear_seconds > 0).then_some(ui.clipboard_clear_seconds);

    if ui.osc52 {
        write_osc52(text)?;
        schedule_clear(ui, clears_in, Transport::Osc52);
        return Ok(CopyOutcome {
            transport: Transport::Osc52,
            clears_in,
        });
    }
    run_helper(text)?;
    schedule_clear(ui, clears_in, Transport::Helper);
    Ok(CopyOutcome {
        transport: Transport::Helper,
        clears_in,
    })
}

/// `ESC ] 52 ; c ; <base64> BEL`, wrapped for tmux when running inside it so
/// the sequence reaches the outer terminal.
pub fn osc52_sequence(text: &str, tmux: bool) -> String {
    let payload = base64(text.as_bytes());
    let inner = format!("\x1b]52;c;{payload}\x07");
    if tmux {
        // tmux only forwards escape sequences wrapped in its passthrough.
        format!("\x1bPtmux;{}\x1b\\", inner.replace('\x1b', "\x1b\x1b"))
    } else {
        inner
    }
}

fn write_osc52(text: &str) -> Result<()> {
    let tmux = std::env::var_os("TMUX").is_some();
    let seq = osc52_sequence(text, tmux);
    // Write to the controlling terminal, not stdout: stdout may be redirected
    // and the alternate screen buffer must not be disturbed.
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|e| {
            Error::failed(
                "클립보드에 복사할 수 없습니다",
                format!("/dev/tty를 열 수 없습니다: {e}"),
                "터미널에서 직접 실행 중인지 확인하거나 `ui.osc52 = false`로 설정하세요.",
            )
        })?;
    tty.write_all(seq.as_bytes())?;
    tty.flush()?;
    Ok(())
}

/// `pbcopy` / `wl-copy` / `xclip`, whichever exists.
pub fn helper_command() -> Option<(&'static str, Vec<&'static str>)> {
    for (bin, args) in [
        ("pbcopy", vec![]),
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
    ] {
        if which(bin) {
            return Some((bin, args));
        }
    }
    None
}

fn which(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

fn run_helper(text: &str) -> Result<()> {
    let (bin, args) = helper_command().ok_or_else(|| {
        Error::failed(
            "클립보드에 복사할 수 없습니다",
            "pbcopy, wl-copy, xclip 중 어느 것도 찾을 수 없습니다.",
            "클립보드 도구를 설치하거나 `ui.osc52 = true`로 설정하세요.",
        )
    })?;
    let mut child = std::process::Command::new(bin)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        return Err(Error::failed(
            "클립보드에 복사할 수 없습니다",
            format!("`{bin}`이(가) 실패했습니다."),
            "클립보드 도구 설정을 확인하세요.",
        ));
    }
    Ok(())
}

/// Overwrite the clipboard after the configured delay. Detached so the UI never
/// waits on it; failure is silent because the copy itself already succeeded.
fn schedule_clear(ui: &UiConfig, clears_in: Option<u64>, transport: Transport) {
    let Some(secs) = clears_in else { return };
    let osc52 = ui.osc52;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        match transport {
            Transport::Osc52 if osc52 => {
                let _ = write_osc52("");
            }
            _ => {
                let _ = run_helper("");
            }
        }
    });
}

/// Minimal RFC 4648 base64; avoids a dependency for a dozen lines.
fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("한".as_bytes()), "7ZWc");
    }

    #[test]
    fn osc52_sequence_is_well_formed() {
        let seq = osc52_sequence("hi", false);
        assert_eq!(seq, "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn tmux_passthrough_doubles_escapes_and_wraps() {
        let seq = osc52_sequence("hi", true);
        assert!(seq.starts_with("\x1bPtmux;"));
        assert!(seq.ends_with("\x1b\\"));
        assert!(seq.contains("\x1b\x1b]52;c;aGk="), "inner ESC is doubled");
    }

    #[test]
    fn secrets_get_a_clear_timer_and_plain_values_do_not() {
        let ui = UiConfig::default();
        assert_eq!(ui.clipboard_clear_seconds, 45);

        let secret = CopyOutcome {
            transport: Transport::Osc52,
            clears_in: Some(ui.clipboard_clear_seconds),
        };
        assert!(secret.message("접속 URL").contains("45초 후 자동 삭제"));

        let plain = CopyOutcome {
            transport: Transport::Helper,
            clears_in: None,
        };
        assert!(!plain.message("DB명").contains("자동 삭제"));
    }
}
