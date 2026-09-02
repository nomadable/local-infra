//! Activity log recording (PRD §7.8, §12.3).
//!
//! An operation opens a record before it starts and closes it with the outcome,
//! so a crash mid-way still leaves evidence of what ran. Every string passing
//! through here is redacted.

use crate::core::error::{Error, Result};
use crate::core::model::{ActivityRecord, ActivityStatus, Origin};
use crate::core::store::Store;
use crate::core::util::{new_id, now, redact};

pub struct Activity<'a> {
    store: &'a Store,
    record: ActivityRecord,
    finished: bool,
}

impl<'a> Activity<'a> {
    /// Open a record in `started` state and persist it immediately.
    pub fn start(
        store: &'a Store,
        origin: Origin,
        resource_type: &str,
        action: &str,
        summary: impl AsRef<str>,
    ) -> Result<Self> {
        let record = ActivityRecord {
            id: new_id(),
            target_id: None,
            resource_type: resource_type.to_string(),
            resource_id: None,
            action: action.to_string(),
            origin,
            status: ActivityStatus::Started,
            redacted_summary: redact(summary.as_ref()),
            steps: Vec::new(),
            started_at: now(),
            completed_at: None,
        };
        store.upsert_activity(&record)?;
        Ok(Self {
            store,
            record,
            finished: false,
        })
    }

    pub fn on_target(mut self, target_id: &str) -> Self {
        self.record.target_id = Some(target_id.to_string());
        self
    }

    pub fn on_resource(mut self, resource_id: &str) -> Self {
        self.record.resource_id = Some(resource_id.to_string());
        self
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    /// Append a completed step. Persisted right away so partial failures are
    /// visible in the log (PRD §10).
    pub fn step(&mut self, text: impl AsRef<str>) {
        self.record.steps.push(redact(text.as_ref()));
        let _ = self.store.upsert_activity(&self.record);
    }

    pub fn ok(mut self) {
        self.close(ActivityStatus::Ok);
    }

    pub fn rolled_back(mut self, reason: impl AsRef<str>) {
        self.record
            .steps
            .push(format!("롤백: {}", redact(reason.as_ref())));
        self.close(ActivityStatus::RolledBack);
    }

    pub fn failed(mut self, error: &Error) {
        let d = error.as_diagnostic();
        self.record.steps.push(format!("실패: {}", redact(&d.what)));
        self.close(ActivityStatus::Failed);
    }

    /// Convenience for `match result { Ok => ok(), Err => failed() }`.
    pub fn finish<T>(self, result: &Result<T>) {
        match result {
            Ok(_) => self.ok(),
            Err(e) => self.failed(e),
        }
    }

    fn close(&mut self, status: ActivityStatus) {
        self.record.status = status;
        self.record.completed_at = Some(now());
        self.finished = true;
        let _ = self.store.upsert_activity(&self.record);
    }
}

impl Drop for Activity<'_> {
    /// A dropped-without-outcome record would look "in progress" forever.
    fn drop(&mut self) {
        if !self.finished {
            self.record
                .steps
                .push("중단됨: 결과가 기록되지 않았습니다".into());
            self.close(ActivityStatus::Failed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_operation_is_recorded_with_its_steps() {
        let store = Store::open_in_memory().unwrap();
        let mut a =
            Activity::start(&store, Origin::Cli, "database", "create", "letsbid_dev").unwrap();
        a.step("컨테이너 확인");
        a.step("DB 생성");
        a.ok();

        let log = store.list_activity(10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].status, ActivityStatus::Ok);
        assert_eq!(log[0].steps, vec!["컨테이너 확인", "DB 생성"]);
        assert!(log[0].completed_at.is_some());
    }

    #[test]
    fn summaries_and_steps_are_redacted() {
        let store = Store::open_in_memory().unwrap();
        let mut a = Activity::start(
            &store,
            Origin::Tui,
            "database",
            "create",
            "postgresql://u:hunter2@127.0.0.1:5432/db",
        )
        .unwrap();
        a.step("PGPASSWORD=hunter2 psql");
        a.ok();

        let log = store.list_activity(1).unwrap();
        assert!(!log[0].redacted_summary.contains("hunter2"));
        assert!(log[0].redacted_summary.contains("****"));
        assert!(!log[0].steps[0].contains("hunter2"));
    }

    #[test]
    fn dropping_without_an_outcome_records_a_failure_not_a_stuck_record() {
        let store = Store::open_in_memory().unwrap();
        {
            let _a = Activity::start(&store, Origin::Cli, "engine", "create", "pg17").unwrap();
        }
        let log = store.list_activity(1).unwrap();
        assert_eq!(log[0].status, ActivityStatus::Failed);
        assert!(log[0].steps[0].contains("중단됨"));
    }

    #[test]
    fn finish_maps_the_result_to_the_right_status() {
        let store = Store::open_in_memory().unwrap();
        let a = Activity::start(&store, Origin::Cli, "engine", "create", "pg17").unwrap();
        let outcome: Result<()> = Err(Error::Usage("bad".into()));
        a.finish(&outcome);
        let log = store.list_activity(1).unwrap();
        assert_eq!(log[0].status, ActivityStatus::Failed);
        assert!(log[0].steps[0].contains("실패"));
    }
}
