//! Background work (TUI-006, PRD §12.2).
//!
//! The event loop must answer a keypress in 16 ms, and a `docker pull` takes
//! minutes. So every `core` call runs on a tokio task with its own
//! [`Reporter`] and [`Cancel`], and reports back over two channels the loop
//! selects on. Nothing in the UI ever awaits a use case directly.

use crate::core::plan::Plan;
use crate::core::progress::{Cancel, Progress, Reporter};
use crate::core::{Ctx, Result};
use crate::tui::data::Snapshot;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// What a finished job gives back to the UI.
#[derive(Debug)]
pub enum Outcome {
    /// Fresh data for every screen.
    Snapshot(Box<Snapshot>),
    /// One line for the status bar.
    Note(String),
    /// Something the user must read: a connection block, engine logs.
    Report { title: String, body: String },
    /// A live plan preview for the open form, tagged with the edit it belongs
    /// to so a late answer to an old keystroke can be dropped.
    FormPlan { epoch: u64, plan: Result<Plan> },
    /// The full plan for an open confirmation modal, replacing the local
    /// approximation it was opened with.
    ConfirmPlan(Box<Plan>),
    /// Host keys scanned for a pending SSH target. The loop turns this into
    /// the approval modal; nothing is registered until it is confirmed.
    SshScanned {
        spec: crate::core::target::SshSpec,
        fingerprint: String,
        key_type: String,
    },
    FormDone {
        title: String,
        body: String,
        copy_url: Option<String>,
        copy_env: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub index: usize,
    pub total: usize,
    pub title: String,
}

/// One in-flight operation, as the UI knows it.
#[derive(Debug)]
pub struct Job {
    pub id: u64,
    pub title: String,
    pub cancel: Cancel,
    pub step: Option<Step>,
    pub bytes: Option<u64>,
    pub log: Vec<String>,
    /// Failures of quiet jobs go to the status line, not a modal — a preview
    /// that cannot be computed yet is not an error the user must acknowledge.
    pub quiet: bool,
}

impl Job {
    /// `3/6 컨테이너 생성` — what the spinner sits next to.
    pub fn headline(&self) -> String {
        match &self.step {
            Some(step) => format!(
                "{} ({}/{}) {}",
                self.title, step.index, step.total, step.title
            ),
            None => self.title.clone(),
        }
    }
}

/// The receiving ends the event loop selects on.
pub struct Channels {
    pub progress: UnboundedReceiver<(u64, Progress)>,
    pub done: UnboundedReceiver<(u64, Result<Outcome>)>,
}

pub struct Jobs {
    next_id: u64,
    running: Vec<Job>,
    progress_tx: UnboundedSender<(u64, Progress)>,
    done_tx: UnboundedSender<(u64, Result<Outcome>)>,
    /// Kept so `log` survives a job finishing, for the form's step trace.
    last_log: Vec<String>,
}

impl Jobs {
    pub fn new() -> (Self, Channels) {
        let (progress_tx, progress) = unbounded_channel();
        let (done_tx, done) = unbounded_channel();
        (
            Self {
                next_id: 1,
                running: Vec::new(),
                progress_tx,
                done_tx,
                last_log: Vec::new(),
            },
            Channels { progress, done },
        )
    }

    /// Start `body` on the runtime. The returned id tags its progress events.
    ///
    /// Progress is forwarded through a per-job [`Reporter`] so `core` stays
    /// unaware of job identity, exactly as the CLI uses it.
    pub fn spawn<F, Fut>(
        &mut self,
        title: impl Into<String>,
        ctx: Arc<Ctx>,
        quiet: bool,
        body: F,
    ) -> u64
    where
        F: FnOnce(Arc<Ctx>, Reporter, Cancel) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Outcome>> + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        let cancel = Cancel::new();
        let (reporter, mut rx) = Reporter::channel();

        let forward = self.progress_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if forward.send((id, event)).is_err() {
                    break;
                }
            }
        });

        let done = self.done_tx.clone();
        let job_cancel = cancel.clone();
        tokio::spawn(async move {
            let result = body(ctx, reporter, job_cancel).await;
            let _ = done.send((id, result));
        });

        self.running.push(Job {
            id,
            title: title.into(),
            cancel,
            step: None,
            bytes: None,
            log: Vec::new(),
            quiet,
        });
        id
    }

    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// Work the user would be upset to lose, i.e. anything but a refresh or a
    /// plan preview. Drives the exit confirmation (requirement 10).
    pub fn has_loud_work(&self) -> bool {
        self.running.iter().any(|j| !j.quiet)
    }

    pub fn count(&self) -> usize {
        self.running.len()
    }

    /// The job the status bar reports on: the newest loud one, else any.
    pub fn foreground(&self) -> Option<&Job> {
        self.running
            .iter()
            .rev()
            .find(|j| !j.quiet)
            .or_else(|| self.running.last())
    }

    /// `Ctrl+C`: trip every token. Cancellation is cooperative, so the tasks
    /// end at their next `cancel.check()` and report normally.
    pub fn cancel_all(&self) {
        for job in &self.running {
            job.cancel.cancel();
        }
    }

    pub fn apply(&mut self, id: u64, event: Progress) {
        let Some(job) = self.running.iter_mut().find(|j| j.id == id) else {
            return;
        };
        match event {
            Progress::Step {
                index,
                total,
                title,
            } => {
                job.log.push(format!("{index}/{total} {title}"));
                job.step = Some(Step {
                    index,
                    total,
                    title,
                });
            }
            Progress::StepDone { index } => {
                if let Some(step) = &job.step {
                    if step.index == index {
                        job.log.push(format!("{index} 완료"));
                    }
                }
            }
            Progress::Bytes { transferred } => job.bytes = Some(transferred),
            Progress::Log { line } => job.log.push(line),
        }
        self.last_log = job.log.clone();
    }

    /// Remove a finished job and keep its trace for the form panel.
    pub fn take(&mut self, id: u64) -> Option<Job> {
        let index = self.running.iter().position(|j| j.id == id)?;
        let job = self.running.remove(index);
        self.last_log = job.log.clone();
        Some(job)
    }

    pub fn contains(&self, id: u64) -> bool {
        self.running.iter().any(|j| j.id == id)
    }

    /// Streaming step log of the foreground job, newest last (PRD §7.5).
    pub fn log(&self) -> &[String] {
        match self.foreground() {
            Some(job) if !job.log.is_empty() => &job.log,
            _ => &self.last_log,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, title: &str, quiet: bool) -> Job {
        Job {
            id,
            title: title.to_string(),
            cancel: Cancel::new(),
            step: None,
            bytes: None,
            log: Vec::new(),
            quiet,
        }
    }

    #[test]
    fn a_fresh_registry_is_idle() {
        let (jobs, _channels) = Jobs::new();
        assert!(!jobs.busy());
        assert!(!jobs.has_loud_work());
        assert!(jobs.foreground().is_none());
        assert!(jobs.log().is_empty());
    }

    #[test]
    fn progress_events_become_a_step_a_byte_count_and_a_log_line() {
        let (mut jobs, _channels) = Jobs::new();
        jobs.running.push(job(7, "백업", false));

        jobs.apply(
            7,
            Progress::Step {
                index: 2,
                total: 5,
                title: "덤프 스트리밍".into(),
            },
        );
        jobs.apply(7, Progress::StepDone { index: 2 });
        jobs.apply(7, Progress::Bytes { transferred: 4096 });
        jobs.apply(
            7,
            Progress::Log {
                line: "pg_dump 완료".into(),
            },
        );

        let running = jobs.foreground().expect("one job is running");
        assert_eq!(
            running.step,
            Some(Step {
                index: 2,
                total: 5,
                title: "덤프 스트리밍".into()
            })
        );
        assert_eq!(running.bytes, Some(4096));
        assert_eq!(running.headline(), "백업 (2/5) 덤프 스트리밍");
        assert_eq!(jobs.log(), ["2/5 덤프 스트리밍", "2 완료", "pg_dump 완료"]);
    }

    #[test]
    fn progress_for_an_unknown_job_is_dropped_rather_than_panicking() {
        let (mut jobs, _channels) = Jobs::new();
        jobs.apply(99, Progress::Bytes { transferred: 1 });
        assert!(!jobs.busy());
    }

    #[test]
    fn a_quiet_refresh_does_not_count_as_work_worth_confirming() {
        let (mut jobs, _channels) = Jobs::new();
        jobs.running.push(job(1, "새로 고침", true));
        assert!(jobs.busy());
        assert!(!jobs.has_loud_work());

        jobs.running.push(job(2, "백업", false));
        assert!(jobs.has_loud_work());
        assert_eq!(jobs.foreground().map(|j| j.id), Some(2));
    }

    #[test]
    fn taking_a_finished_job_keeps_its_trace_for_the_form_panel() {
        let (mut jobs, _channels) = Jobs::new();
        jobs.running.push(job(3, "생성", false));
        jobs.apply(
            3,
            Progress::Log {
                line: "DB 생성".into(),
            },
        );
        assert!(jobs.contains(3));

        let finished = jobs.take(3).expect("job 3 was running");
        assert_eq!(finished.log, ["DB 생성"]);
        assert!(!jobs.busy());
        assert_eq!(jobs.log(), ["DB 생성"]);
        assert!(jobs.take(3).is_none());
    }

    #[tokio::test]
    async fn a_spawned_job_streams_progress_and_reports_its_outcome() {
        let (mut jobs, mut channels) = Jobs::new();
        let ctx = std::sync::Arc::new(());

        // `spawn` is generic over the context only to hand it to the body, so
        // the mechanism can be exercised with a stand-in.
        let id = spawn_with(
            &mut jobs,
            "테스트",
            ctx,
            false,
            |_, reporter, _| async move {
                reporter.step(1, 1, "한 걸음");
                Ok(Outcome::Note("끝".into()))
            },
        );

        let (progress_id, event) = channels.progress.recv().await.expect("progress arrives");
        assert_eq!(progress_id, id);
        jobs.apply(progress_id, event);
        assert_eq!(jobs.foreground().unwrap().step.as_ref().unwrap().index, 1);

        let (done_id, result) = channels.done.recv().await.expect("completion arrives");
        assert_eq!(done_id, id);
        assert!(matches!(result, Ok(Outcome::Note(note)) if note == "끝"));
        assert!(jobs.take(done_id).is_some());
    }

    #[tokio::test]
    async fn cancelling_trips_the_token_the_body_is_polling() {
        let (mut jobs, mut channels) = Jobs::new();
        let ctx = std::sync::Arc::new(());

        let id = spawn_with(
            &mut jobs,
            "긴 작업",
            ctx,
            false,
            |_, _, cancel| async move {
                for _ in 0..1000 {
                    cancel.check()?;
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
                Ok(Outcome::Note("완주".into()))
            },
        );

        jobs.cancel_all();
        let (done_id, result) = channels.done.recv().await.expect("the body gives up");
        assert_eq!(done_id, id);
        assert!(matches!(result, Err(crate::core::Error::Cancelled)));
    }

    /// `Jobs::spawn` takes `Arc<Ctx>`; these tests need the same plumbing
    /// without opening a real context, so they drive it through a tiny twin
    /// that shares the exact channel wiring.
    fn spawn_with<C, F, Fut>(
        jobs: &mut Jobs,
        title: &str,
        ctx: std::sync::Arc<C>,
        quiet: bool,
        body: F,
    ) -> u64
    where
        C: Send + Sync + 'static,
        F: FnOnce(std::sync::Arc<C>, Reporter, Cancel) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Outcome>> + Send + 'static,
    {
        let id = jobs.next_id;
        jobs.next_id += 1;
        let cancel = Cancel::new();
        let (reporter, mut rx) = Reporter::channel();

        let forward = jobs.progress_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if forward.send((id, event)).is_err() {
                    break;
                }
            }
        });

        let done = jobs.done_tx.clone();
        let job_cancel = cancel.clone();
        tokio::spawn(async move {
            let result = body(ctx, reporter, job_cancel).await;
            let _ = done.send((id, result));
        });

        jobs.running.push(Job {
            id,
            title: title.to_string(),
            cancel,
            step: None,
            bytes: None,
            log: Vec::new(),
            quiet,
        });
        id
    }
}
