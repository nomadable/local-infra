//! Progress reporting and cooperative cancellation for long operations
//! (TUI-006, BAK-007).
//!
//! `core` never talks to a terminal. It emits `Progress` events on an unbounded
//! channel; the CLI prints them as lines and the TUI streams them into the plan
//! panel, so the UI thread is never blocked.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress {
    /// Entering step `index` of `total` (1-based).
    Step {
        index: usize,
        total: usize,
        title: String,
    },
    /// The current step finished successfully.
    StepDone { index: usize },
    /// Bytes transferred so far, for dumps and restores.
    Bytes { transferred: u64 },
    /// Free-form, already redacted line.
    Log { line: String },
}

/// Sink handed into core use cases. `None` means "nobody is watching".
#[derive(Clone, Default)]
pub struct Reporter {
    tx: Option<tokio::sync::mpsc::UnboundedSender<Progress>>,
}

impl Reporter {
    pub fn channel() -> (Self, tokio::sync::mpsc::UnboundedReceiver<Progress>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx: Some(tx) }, rx)
    }

    pub fn silent() -> Self {
        Self { tx: None }
    }

    pub fn send(&self, event: Progress) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }

    pub fn step(&self, index: usize, total: usize, title: impl Into<String>) {
        self.send(Progress::Step {
            index,
            total,
            title: title.into(),
        });
    }

    pub fn step_done(&self, index: usize) {
        self.send(Progress::StepDone { index });
    }

    pub fn bytes(&self, transferred: u64) {
        self.send(Progress::Bytes { transferred });
    }

    /// Redacts before emitting: callers may pass raw command output.
    pub fn log(&self, line: impl AsRef<str>) {
        self.send(Progress::Log {
            line: crate::core::util::redact(line.as_ref()),
        });
    }
}

/// Cooperative cancellation flag, polled between steps and between chunks of a
/// stream. Cloning shares the same flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(Error::Cancelled)` when the user asked to stop.
    pub fn check(&self) -> crate::core::error::Result<()> {
        if self.is_cancelled() {
            Err(crate::core::error::Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reporter_forwards_events_and_redacts_logs() {
        let (r, mut rx) = Reporter::channel();
        r.step(1, 3, "이미지 확인");
        r.log("PGPASSWORD=leaky value");
        drop(r);

        assert_eq!(
            rx.recv().await.unwrap(),
            Progress::Step {
                index: 1,
                total: 3,
                title: "이미지 확인".into()
            }
        );
        match rx.recv().await.unwrap() {
            Progress::Log { line } => {
                assert!(!line.contains("leaky"), "{line}");
                assert!(line.contains("PGPASSWORD=****"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn silent_reporter_is_a_no_op() {
        let r = Reporter::silent();
        r.step(1, 1, "x");
        r.log("y");
    }

    #[test]
    fn cancel_is_shared_across_clones() {
        let a = Cancel::new();
        let b = a.clone();
        assert!(a.check().is_ok());
        b.cancel();
        assert!(a.is_cancelled());
        assert!(a.check().is_err());
    }
}
