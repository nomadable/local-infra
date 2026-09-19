//! Streaming query execution with server cancellation, timeout, and limits.

use crate::core::error::{Error, Result};
use crate::core::progress::Cancel;
use crate::core::sql::connection::{SqlSession, TransactionState};
use crate::core::sql::history;
use crate::core::sql::profile::{AccessMode, DEFAULT_CELL_BYTES, DEFAULT_RESULT_BYTES};
use crate::core::sql::statement::{self, TransactionEffect};
use crate::core::Ctx;
use futures_util::StreamExt;
use serde::Serialize;
use std::ops::Range;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_postgres::error::ErrorPosition;
use tokio_postgres::SimpleQueryMessage;

const ROW_BATCH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryLimits {
    pub max_rows: Option<usize>,
    pub max_result_bytes: Option<usize>,
    pub max_cell_bytes: Option<usize>,
}

impl QueryLimits {
    pub fn preview(rows: usize) -> Self {
        Self {
            max_rows: Some(rows),
            max_result_bytes: Some(DEFAULT_RESULT_BYTES),
            max_cell_bytes: Some(DEFAULT_CELL_BYTES),
        }
    }

    pub fn streaming_export() -> Self {
        Self {
            max_rows: None,
            max_result_bytes: None,
            max_cell_bytes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteRequest {
    pub sql: String,
    pub base_offset: usize,
    pub limits: QueryLimits,
}

impl ExecuteRequest {
    pub fn preview(sql: impl Into<String>, rows: usize) -> Self {
        Self {
            sql: sql.into(),
            base_offset: 0,
            limits: QueryLimits::preview(rows),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum QueryEvent {
    StatementStarted {
        index: usize,
        total: usize,
        sql_offset: usize,
    },
    ResultStarted {
        statement: usize,
        result: usize,
        columns: Vec<String>,
    },
    Rows {
        statement: usize,
        result: usize,
        rows: Vec<Vec<Option<String>>>,
    },
    StatementComplete {
        index: usize,
        affected_rows: u64,
        elapsed_ms: u64,
    },
    Truncated {
        statement: usize,
        rows: usize,
        bytes: usize,
    },
    Error(SqlExecutionError),
    Finished(ExecutionSummary),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorKind {
    Server,
    Timeout,
    Cancelled,
    Policy,
    Connection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SqlExecutionError {
    pub kind: ExecutionErrorKind,
    pub statement: usize,
    pub sqlstate: Option<String>,
    pub severity: Option<String>,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionSummary {
    pub success: bool,
    pub cancelled: bool,
    pub timed_out: bool,
    pub statements: usize,
    pub row_count: u64,
    pub elapsed_ms: u64,
    pub truncated: bool,
    pub transaction: String,
    pub error: Option<SqlExecutionError>,
}

#[derive(Debug, Default)]
struct StreamSummary {
    row_count: u64,
    truncated: bool,
}

pub async fn execute(
    ctx: &Ctx,
    session: &mut SqlSession,
    request: ExecuteRequest,
    cancel: &Cancel,
    events: &mpsc::Sender<QueryEvent>,
) -> Result<ExecutionSummary> {
    if session.closed() {
        return Err(Error::failed(
            "PostgreSQL session 연결이 끊어졌습니다",
            "connection task가 종료되었습니다.",
            "명시적으로 reconnect하세요. transaction, temporary table, session setting은 복구되지 않습니다.",
        ));
    }
    let started = Instant::now();
    let ranges = statement::statement_ranges(&request.sql);
    if ranges.is_empty() {
        return Err(Error::Usage("실행할 SQL statement가 없습니다.".into()));
    }

    let mut row_count = 0u64;
    let mut truncated = false;
    let mut terminal_error = None;
    let mut completed = 0usize;

    for (statement_index, range) in ranges.iter().enumerate() {
        let sql = &request.sql[range.clone()];
        let offset = request.base_offset + range.start;
        send(
            events,
            QueryEvent::StatementStarted {
                index: statement_index,
                total: ranges.len(),
                sql_offset: offset,
            },
        )
        .await;

        if session.endpoint.access == AccessMode::ReadOnly && statement::is_transaction_control(sql)
        {
            terminal_error = Some(SqlExecutionError {
                kind: ExecutionErrorKind::Policy,
                statement: statement_index,
                sqlstate: None,
                severity: None,
                message:
                    "read-only session에서는 transaction-control statement를 실행할 수 없습니다."
                        .into(),
                detail: None,
                hint: Some("statement를 제거하거나 검증된 writable session을 여세요.".into()),
                position: Some(offset),
            });
            break;
        }

        let statement_started = Instant::now();
        if session.endpoint.access == AccessMode::ReadOnly {
            session
                .client()
                .batch_execute("BEGIN READ ONLY")
                .await
                .map_err(|error| {
                    crate::core::sql::connection::postgres_failure(
                        "read-only transaction을 시작할 수 없습니다",
                        &error,
                    )
                })?;
        }

        let controlled = execute_with_controls(
            session,
            sql,
            statement_index,
            offset,
            &request.limits,
            cancel,
            events,
        )
        .await;

        if session.endpoint.access == AccessMode::ReadOnly {
            session
                .client()
                .batch_execute("ROLLBACK")
                .await
                .map_err(|error| {
                    crate::core::sql::connection::postgres_failure(
                        "read-only transaction을 정리할 수 없습니다",
                        &error,
                    )
                })?;
        }

        match controlled? {
            Controlled::Complete(stream) => {
                row_count = row_count.saturating_add(stream.row_count);
                truncated |= stream.truncated;
                completed += 1;
                if session.endpoint.access == AccessMode::ReadWrite {
                    match statement::transaction_effect(sql) {
                        TransactionEffect::Begin => {
                            session.transaction = TransactionState::InTransaction;
                        }
                        TransactionEffect::End => session.transaction = TransactionState::Idle,
                        TransactionEffect::None => {}
                    }
                }
                send(
                    events,
                    QueryEvent::StatementComplete {
                        index: statement_index,
                        affected_rows: stream.row_count,
                        elapsed_ms: millis(statement_started.elapsed()),
                    },
                )
                .await;
            }
            Controlled::Failed(error) => {
                if session.transaction == TransactionState::InTransaction {
                    session.transaction = TransactionState::Failed;
                }
                terminal_error = Some(error);
                break;
            }
        }
    }

    let elapsed_ms = millis(started.elapsed());
    let error = terminal_error;
    let summary = ExecutionSummary {
        success: error.is_none(),
        cancelled: error
            .as_ref()
            .is_some_and(|error| error.kind == ExecutionErrorKind::Cancelled),
        timed_out: error
            .as_ref()
            .is_some_and(|error| error.kind == ExecutionErrorKind::Timeout),
        statements: completed,
        row_count,
        elapsed_ms,
        truncated,
        transaction: transaction_label(session.transaction).into(),
        error: error.clone(),
    };
    if let Some(error) = error {
        send(events, QueryEvent::Error(error)).await;
    }
    send(events, QueryEvent::Finished(summary.clone())).await;
    history::record(
        ctx,
        &session.source,
        &session.endpoint,
        &request.sql,
        summary.success,
        summary.elapsed_ms,
        summary.row_count,
    )?;
    Ok(summary)
}

async fn execute_with_controls(
    session: &SqlSession,
    sql: &str,
    statement: usize,
    offset: usize,
    limits: &QueryLimits,
    cancel: &Cancel,
    events: &mpsc::Sender<QueryEvent>,
) -> Result<Controlled> {
    cancel.check()?;
    let cancel_token = session.cancel_token();
    let execution = execute_stream(session, sql, statement, offset, limits, events);
    tokio::pin!(execution);
    let timeout = tokio::time::sleep(Duration::from_millis(session.endpoint.query_timeout_ms));
    tokio::pin!(timeout);
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            outcome = &mut execution => return outcome,
            _ = &mut timeout => {
                let _ = session.cancel(cancel_token.clone()).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut execution).await;
                return Ok(Controlled::Failed(control_error(
                    ExecutionErrorKind::Timeout,
                    statement,
                    offset,
                    "query timeout을 초과했습니다.",
                )));
            }
            _ = poll.tick() => {
                if cancel.is_cancelled() {
                    let _ = session.cancel(cancel_token.clone()).await;
                    let _ = tokio::time::timeout(Duration::from_secs(5), &mut execution).await;
                    return Ok(Controlled::Failed(control_error(
                        ExecutionErrorKind::Cancelled,
                        statement,
                        offset,
                        "사용자가 query 실행을 취소했습니다.",
                    )));
                }
            }
        }
    }
}

async fn execute_stream(
    session: &SqlSession,
    sql: &str,
    statement: usize,
    offset: usize,
    limits: &QueryLimits,
    events: &mpsc::Sender<QueryEvent>,
) -> Result<Controlled> {
    let stream = match session.client().simple_query_raw(sql).await {
        Ok(stream) => stream,
        Err(error) => return Ok(Controlled::Failed(sql_error(&error, statement, offset))),
    };
    let mut result_index = 0usize;
    tokio::pin!(stream);
    let mut rows = Vec::with_capacity(ROW_BATCH);
    let mut emitted_rows = 0usize;
    let mut emitted_bytes = 0usize;
    let mut affected_rows = 0u64;
    let mut truncated = false;
    let mut announced_truncation = false;

    while let Some(message) = stream.next().await {
        let message = match message {
            Ok(message) => message,
            Err(error) => return Ok(Controlled::Failed(sql_error(&error, statement, offset))),
        };
        match message {
            SimpleQueryMessage::RowDescription(columns) => {
                flush(events, statement, result_index, &mut rows).await;
                send(
                    events,
                    QueryEvent::ResultStarted {
                        statement,
                        result: result_index,
                        columns: columns
                            .iter()
                            .map(|column| column.name().to_string())
                            .collect(),
                    },
                )
                .await;
            }
            SimpleQueryMessage::Row(row) => {
                let within_rows = limits.max_rows.is_none_or(|limit| emitted_rows < limit);
                let mut values = Vec::with_capacity(row.len());
                let mut row_bytes = 0usize;
                for index in 0..row.len() {
                    let value = row.get(index).map(|value| {
                        let value = limits
                            .max_cell_bytes
                            .map(|limit| truncate_cell(value, limit))
                            .unwrap_or_else(|| value.to_string());
                        row_bytes = row_bytes.saturating_add(value.len());
                        value
                    });
                    row_bytes = row_bytes.saturating_add(1);
                    values.push(value);
                }
                let within_bytes = limits
                    .max_result_bytes
                    .is_none_or(|limit| emitted_bytes.saturating_add(row_bytes) <= limit);
                if within_rows && within_bytes {
                    emitted_rows += 1;
                    emitted_bytes = emitted_bytes.saturating_add(row_bytes);
                    rows.push(values);
                    if rows.len() >= ROW_BATCH {
                        flush(events, statement, result_index, &mut rows).await;
                    }
                } else {
                    truncated = true;
                    if !announced_truncation {
                        flush(events, statement, result_index, &mut rows).await;
                        send(
                            events,
                            QueryEvent::Truncated {
                                statement,
                                rows: emitted_rows,
                                bytes: emitted_bytes,
                            },
                        )
                        .await;
                        announced_truncation = true;
                    }
                }
            }
            SimpleQueryMessage::CommandComplete(count) => {
                flush(events, statement, result_index, &mut rows).await;
                affected_rows = affected_rows.saturating_add(count);
                result_index += 1;
            }
            _ => {}
        }
    }
    flush(events, statement, result_index, &mut rows).await;
    Ok(Controlled::Complete(StreamSummary {
        row_count: affected_rows.max(emitted_rows as u64),
        truncated,
    }))
}

async fn flush(
    events: &mpsc::Sender<QueryEvent>,
    statement: usize,
    result: usize,
    rows: &mut Vec<Vec<Option<String>>>,
) {
    if rows.is_empty() {
        return;
    }
    let batch = std::mem::take(rows);
    send(
        events,
        QueryEvent::Rows {
            statement,
            result,
            rows: batch,
        },
    )
    .await;
}

async fn send(events: &mpsc::Sender<QueryEvent>, event: QueryEvent) {
    let _ = events.send(event).await;
}

fn truncate_cell(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn sql_error(error: &tokio_postgres::Error, statement: usize, offset: usize) -> SqlExecutionError {
    let Some(db) = error.as_db_error() else {
        return SqlExecutionError {
            kind: ExecutionErrorKind::Connection,
            statement,
            sqlstate: None,
            severity: None,
            message: crate::core::util::redact(&error.to_string()),
            detail: None,
            hint: Some("명시적으로 reconnect한 뒤 다시 실행하세요.".into()),
            position: None,
        };
    };
    let position = db.position().map(|position| match position {
        ErrorPosition::Original(position) => offset + (*position as usize).saturating_sub(1),
        ErrorPosition::Internal { position, .. } => *position as usize,
    });
    SqlExecutionError {
        kind: ExecutionErrorKind::Server,
        statement,
        sqlstate: Some(db.code().code().into()),
        severity: Some(db.severity().into()),
        message: crate::core::sql::history::redact(db.message()),
        detail: db.detail().map(crate::core::sql::history::redact),
        hint: db.hint().map(crate::core::sql::history::redact),
        position,
    }
}

fn control_error(
    kind: ExecutionErrorKind,
    statement: usize,
    position: usize,
    message: &str,
) -> SqlExecutionError {
    SqlExecutionError {
        kind,
        statement,
        sqlstate: None,
        severity: None,
        message: message.into(),
        detail: None,
        hint: None,
        position: Some(position),
    }
}

fn transaction_label(state: TransactionState) -> &'static str {
    match state {
        TransactionState::Idle => "idle",
        TransactionState::InTransaction => "in_transaction",
        TransactionState::Failed => "failed_transaction",
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

enum Controlled {
    Complete(StreamSummary),
    Failed(SqlExecutionError),
}

#[allow(dead_code)]
fn _range_contract(_: Range<usize>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_limit_keeps_utf8_valid() {
        assert_eq!(truncate_cell("한글", 4), "한…");
        assert_eq!(truncate_cell("abc", 4), "abc");
    }
}
