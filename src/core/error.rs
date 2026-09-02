//! Error taxonomy shared by `core`, `cli` and `tui`.
//!
//! Every user-visible failure carries three things (PRD §10): what failed, the
//! likely cause, and the next action the user can take.

use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Structured, three-part diagnostic rendered identically by the TUI modal and
/// the CLI stderr block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Diagnostic {
    /// What failed, one sentence.
    pub what: String,
    /// Why it probably failed.
    pub cause: String,
    /// What the user should do next.
    pub next: String,
    /// The command that was attempted, already redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Captured output from the failed command, already redacted and trimmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl Diagnostic {
    pub fn new(what: impl Into<String>, cause: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            cause: cause.into(),
            next: next.into(),
            command: None,
            output: None,
        }
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        let text = output.into();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            self.output = Some(clamp(trimmed, 2000));
        }
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} — {}", self.what, self.cause, self.next)
    }
}

fn clamp(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bad arguments or bad input from the user. Exit code 2.
    #[error("{0}")]
    Usage(String),
    /// The named resource does not exist. Exit code 2.
    #[error("{0}")]
    NotFound(String),
    /// Name/port/resource already taken. Exit code 2.
    #[error("{0}")]
    Conflict(String),
    /// The operation is not permitted by policy (e.g. unmanaged resource,
    /// missing `--yes`). Exit code 2.
    #[error("{0}")]
    Refused(String),
    /// Execution failed. Exit code 1.
    #[error("{0}")]
    Failed(Box<Diagnostic>),
    /// The user cancelled a long-running operation. Exit code 1.
    #[error("작업이 취소되었습니다")]
    Cancelled,
}

impl Error {
    pub fn failed(
        what: impl Into<String>,
        cause: impl Into<String>,
        next: impl Into<String>,
    ) -> Self {
        Error::Failed(Box::new(Diagnostic::new(what, cause, next)))
    }

    pub fn diagnostic(diagnostic: Diagnostic) -> Self {
        Error::Failed(Box::new(diagnostic))
    }

    /// Process exit code, per CLI-004.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) | Error::NotFound(_) | Error::Conflict(_) | Error::Refused(_) => 2,
            Error::Failed(_) | Error::Cancelled => 1,
        }
    }

    /// Stable machine-readable kind for `--json` error envelopes.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Usage(_) => "usage",
            Error::NotFound(_) => "not_found",
            Error::Conflict(_) => "conflict",
            Error::Refused(_) => "refused",
            Error::Failed(_) => "failed",
            Error::Cancelled => "cancelled",
        }
    }

    /// Rich diagnostic when present; otherwise a single-line one synthesised
    /// from the message so every surface can render the same shape.
    pub fn as_diagnostic(&self) -> Diagnostic {
        match self {
            Error::Failed(d) => (**d).clone(),
            Error::Usage(m) => Diagnostic::new(
                m.clone(),
                "입력값이 올바르지 않습니다.",
                "값을 수정한 뒤 다시 시도하세요.",
            ),
            Error::NotFound(m) => Diagnostic::new(
                m.clone(),
                "등록된 항목에서 찾을 수 없습니다.",
                "목록을 확인한 뒤 정확한 이름을 사용하세요.",
            ),
            Error::Conflict(m) => Diagnostic::new(
                m.clone(),
                "이미 사용 중인 이름 또는 포트입니다.",
                "다른 이름이나 포트를 선택하세요.",
            ),
            Error::Refused(m) => Diagnostic::new(
                m.clone(),
                "안전 정책에 의해 거부되었습니다.",
                "필요한 확인 절차를 거친 뒤 다시 시도하세요.",
            ),
            Error::Cancelled => Diagnostic::new(
                "작업이 취소되었습니다",
                "사용자가 중단했습니다.",
                "필요하면 다시 실행하세요.",
            ),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::failed(
            "입출력 오류",
            e.to_string(),
            "파일 권한과 디스크 공간을 확인한 뒤 다시 시도하세요.",
        )
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::failed(
            "로컬 상태 데이터베이스 오류",
            e.to_string(),
            "상태 디렉터리 권한을 확인하거나 `linf doctor`를 실행하세요.",
        )
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::failed("JSON 처리 오류", e.to_string(), "입력 형식을 확인하세요.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_errors_exit_two_and_failures_exit_one() {
        assert_eq!(Error::Usage("x".into()).exit_code(), 2);
        assert_eq!(Error::NotFound("x".into()).exit_code(), 2);
        assert_eq!(Error::Conflict("x".into()).exit_code(), 2);
        assert_eq!(Error::Refused("x".into()).exit_code(), 2);
        assert_eq!(Error::failed("a", "b", "c").exit_code(), 1);
        assert_eq!(Error::Cancelled.exit_code(), 1);
    }

    #[test]
    fn every_error_renders_a_three_part_diagnostic() {
        for e in [
            Error::Usage("u".into()),
            Error::NotFound("n".into()),
            Error::Conflict("c".into()),
            Error::Refused("r".into()),
            Error::Cancelled,
            Error::failed("w", "c", "n"),
        ] {
            let d = e.as_diagnostic();
            assert!(!d.what.is_empty() && !d.cause.is_empty() && !d.next.is_empty());
        }
    }

    #[test]
    fn diagnostic_output_is_clamped_on_char_boundary() {
        let long = "가".repeat(2000);
        let d = Diagnostic::new("a", "b", "c").with_output(long);
        assert!(d.output.unwrap().ends_with('…'));
    }
}
