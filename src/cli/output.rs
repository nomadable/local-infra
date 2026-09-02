//! Headless output: human text on a terminal, JSON for scripts (CLI-003),
//! and the three-part error block from PRD §10 on stderr.

use crate::core::error::{Error, Result};
use crate::core::plan::Plan;
use crate::core::progress::Progress;
use serde::Serialize;
use std::io::{IsTerminal, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub struct Emitter {
    pub format: Format,
    /// `--yes`; required by every destructive command (CLI-005).
    pub assume_yes: bool,
    /// Whether stdin is a terminal. Drives CLI-006.
    pub interactive: bool,
    pub color: bool,
}

impl Emitter {
    pub fn new(json: bool, assume_yes: bool) -> Self {
        Self {
            format: if json { Format::Json } else { Format::Human },
            assume_yes,
            interactive: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            color: std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal(),
        }
    }

    pub fn is_json(&self) -> bool {
        self.format == Format::Json
    }

    /// A line of prose. Suppressed in JSON mode so stdout stays parseable.
    pub fn note(&self, text: impl AsRef<str>) {
        if self.format == Format::Human {
            println!("{}", text.as_ref());
        }
    }

    /// Diagnostics and warnings always go to stderr, so `linf db url x > .env`
    /// stays clean (CLI-007).
    pub fn warn(&self, text: impl AsRef<str>) {
        eprintln!("! {}", text.as_ref());
    }

    /// Raw value for pipes — no decoration in either mode.
    pub fn value(&self, text: impl AsRef<str>) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", text.as_ref());
    }

    /// The machine-readable payload. In human mode `human` is printed instead.
    pub fn data<T: Serialize>(&self, payload: &T, human: impl FnOnce()) -> Result<()> {
        match self.format {
            Format::Json => {
                let text = serde_json::to_string_pretty(payload)?;
                let mut out = std::io::stdout().lock();
                writeln!(out, "{text}")?;
                Ok(())
            }
            Format::Human => {
                human();
                Ok(())
            }
        }
    }

    pub fn plan(&self, plan: &Plan) -> Result<()> {
        self.data(plan, || {
            println!("{}", plan.title);
            print!("{}", plan.render());
        })
    }

    /// Live step lines while a long operation runs. Silent in JSON mode.
    pub fn progress(&self, event: &Progress) {
        if self.format != Format::Human {
            return;
        }
        match event {
            Progress::Step {
                index,
                total,
                title,
            } => {
                eprintln!("[{index}/{total}] {title}");
            }
            Progress::StepDone { .. } => {}
            Progress::Bytes { transferred } => {
                eprint!(
                    "\r  {} 전송됨",
                    crate::core::util::human_bytes(*transferred)
                );
                let _ = std::io::stderr().flush();
            }
            Progress::Log { line } => eprintln!("  {line}"),
        }
    }

    /// Destructive confirmation. `--yes` satisfies it; without a TTY and
    /// without `--yes` the command fails loudly instead of hanging (CLI-005/006).
    pub fn confirm_destructive(&self, what: &str, plan: &Plan) -> Result<()> {
        if self.assume_yes {
            return Ok(());
        }
        if self.format == Format::Json || !self.interactive {
            return Err(Error::Refused(format!(
                "{what}은(는) 파괴적 작업입니다. 비대화형 환경에서는 `--yes`가 필요합니다."
            )));
        }
        eprintln!("{}", plan.title);
        eprint!("{}", plan.render());
        eprint!("계속하려면 `yes`를 입력하세요: ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() == "yes" {
            Ok(())
        } else {
            Err(Error::Refused("사용자가 취소했습니다.".into()))
        }
    }

    /// Typed-name confirmation for volume and database deletion (PRD §7.9).
    pub fn confirm_by_name(&self, expected: &str, plan: &Plan) -> Result<()> {
        if self.assume_yes {
            return Ok(());
        }
        if self.format == Format::Json || !self.interactive {
            return Err(Error::Refused(format!(
                "`{expected}` 삭제는 파괴적 작업입니다. 비대화형 환경에서는 `--yes`가 필요합니다."
            )));
        }
        eprintln!("{}", plan.title);
        eprint!("{}", plan.render());
        eprint!("삭제하려면 `{expected}`을(를) 그대로 입력하세요: ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() == expected {
            Ok(())
        } else {
            Err(Error::Refused(
                "입력한 이름이 일치하지 않아 취소했습니다.".into(),
            ))
        }
    }
}

/// Render a failure to stderr in the PRD §10 shape and return the exit code.
pub fn report(error: &Error, format: Format) -> i32 {
    let d = error.as_diagnostic();
    match format {
        Format::Json => {
            let payload = serde_json::json!({
                "ok": false,
                "kind": error.kind(),
                "error": d,
            });
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| d.what.clone())
            );
        }
        Format::Human => {
            eprintln!("오류: {}", d.what);
            eprintln!("원인: {}", d.cause);
            eprintln!("조치: {}", d.next);
            if let Some(cmd) = &d.command {
                eprintln!("명령: {cmd}");
            }
            if let Some(out) = &d.output {
                eprintln!("출력: {out}");
            }
        }
    }
    error.exit_code()
}

/// Fixed-width table used by every `list` subcommand, CJK-aware (PRD §12.4).
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    use crate::core::util::display_cols;
    let mut widths: Vec<usize> = headers.iter().map(|h| display_cols(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(display_cols(cell));
            }
        }
    }
    let mut out = String::new();
    for (i, header) in headers.iter().enumerate() {
        out.push_str(&pad(header, widths[i]));
        if i + 1 < headers.len() {
            out.push_str("  ");
        }
    }
    out.push('\n');
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i >= widths.len() {
                continue;
            }
            out.push_str(&pad(cell, widths[i]));
            if i + 1 < row.len() {
                out.push_str("  ");
            }
        }
        out.push('\n');
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let used = crate::core::util::display_cols(text);
    let mut out = text.to_string();
    for _ in used..width {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::plan::StepKind;

    fn emitter(json: bool, yes: bool, interactive: bool) -> Emitter {
        Emitter {
            format: if json { Format::Json } else { Format::Human },
            assume_yes: yes,
            interactive,
            color: false,
        }
    }

    fn destructive() -> Plan {
        Plan::new("DB 삭제")
            .step(StepKind::Destroy, "DB letsbid_dev 삭제")
            .warn("되돌릴 수 없습니다")
    }

    #[test]
    fn destructive_commands_need_yes_when_non_interactive() {
        let err = emitter(false, false, false)
            .confirm_destructive("DB 삭제", &destructive())
            .unwrap_err();
        assert!(matches!(err, Error::Refused(_)));
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--yes"));
    }

    #[test]
    fn json_mode_never_prompts() {
        let err = emitter(true, false, true)
            .confirm_destructive("DB 삭제", &destructive())
            .unwrap_err();
        assert!(matches!(err, Error::Refused(_)));
    }

    #[test]
    fn yes_flag_satisfies_both_confirmation_styles() {
        let e = emitter(false, true, false);
        assert!(e.confirm_destructive("DB 삭제", &destructive()).is_ok());
        assert!(e.confirm_by_name("linf-pg17-data", &destructive()).is_ok());
    }

    #[test]
    fn error_report_uses_the_documented_exit_codes() {
        assert_eq!(report(&Error::Usage("x".into()), Format::Human), 2);
        assert_eq!(report(&Error::NotFound("x".into()), Format::Json), 2);
        assert_eq!(report(&Error::failed("a", "b", "c"), Format::Human), 1);
        assert_eq!(report(&Error::Cancelled, Format::Human), 1);
    }

    #[test]
    fn table_columns_align_with_cjk_content() {
        let text = table(
            &["TARGET", "DATABASE"],
            &[
                vec!["local".into(), "letsbid_dev".into()],
                vec!["개발".into(), "tamche_dev".into()],
            ],
        );
        let lines: Vec<&str> = text.lines().collect();
        // Where the second column starts, measured in terminal columns rather
        // than characters — the whole point of the CJK-aware padding.
        let start_of_second = |line: &str, cell: &str| {
            let byte = line
                .find(cell)
                .unwrap_or_else(|| panic!("`{cell}` not in `{line}`"));
            crate::core::util::display_cols(&line[..byte])
        };
        let header = start_of_second(lines[0], "DATABASE");
        assert_eq!(start_of_second(lines[1], "letsbid_dev"), header);
        assert_eq!(start_of_second(lines[2], "tamche_dev"), header);
        assert_eq!(
            header, 8,
            "widest first cell is TARGET (6) plus the 2-space gutter"
        );
    }
}
