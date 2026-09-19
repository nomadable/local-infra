//! Streaming CSV and JSON exports independent of preview limits.

use crate::core::config::harden_file;
use crate::core::error::{Error, Result};
use crate::core::progress::Cancel;
use crate::core::sql::connection::SqlSession;
use crate::core::sql::profile::AccessMode;
use crate::core::sql::runner::{self, ExecuteRequest, QueryEvent, QueryLimits};
use crate::core::Ctx;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "lower")]
pub enum ExportFormat {
    Csv,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportReceipt {
    pub path: PathBuf,
    pub format: ExportFormat,
    pub rows: u64,
    pub bytes: u64,
}

pub async fn export(
    ctx: &Ctx,
    session: &mut SqlSession,
    sql: &str,
    path: &Path,
    format: ExportFormat,
    cancel: &Cancel,
) -> Result<ExportReceipt> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    harden_file(temporary.path())?;
    let file = temporary.reopen()?;
    let (tx, rx) = mpsc::channel(8);
    let request = ExecuteRequest {
        sql: sql.to_string(),
        base_offset: 0,
        limits: QueryLimits::streaming_export(),
    };
    // A full export re-executes the query. Force server-enforced read-only
    // transactions even when the interactive workspace is writable, so
    // `INSERT ... RETURNING`, DDL, and transaction-control statements cannot
    // be replayed as an export side effect.
    let previous_access = session.endpoint.access;
    session.endpoint.access = AccessMode::ReadOnly;
    let run = runner::execute(ctx, session, request, cancel, &tx);
    let write = write_events(rx, BufWriter::new(file), format);
    let joined = tokio::try_join!(run, write);
    session.endpoint.access = previous_access;
    let (summary, (rows, bytes)) = joined?;
    drop(tx);
    if !summary.success {
        return Err(Error::failed(
            "SQL export query가 실패했습니다",
            summary
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "server가 query를 완료하지 못했습니다.".into()),
            "SQL 오류를 수정한 뒤 다시 export하세요.",
        ));
    }
    temporary
        .persist(path)
        .map_err(|error| Error::from(error.error))?;
    harden_file(path)?;
    Ok(ExportReceipt {
        path: path.to_path_buf(),
        format,
        rows,
        bytes,
    })
}

pub async fn write_events<W: Write>(
    mut events: mpsc::Receiver<QueryEvent>,
    output: W,
    format: ExportFormat,
) -> Result<(u64, u64)> {
    let mut writer = CountingWriter::new(output);
    let mut state = WriterState::new(format, &mut writer)?;
    while let Some(event) = events.recv().await {
        let finished = matches!(event, QueryEvent::Finished(_));
        state.event(event, &mut writer)?;
        if finished {
            break;
        }
    }
    state.finish(&mut writer)?;
    writer.flush()?;
    Ok((state.rows, writer.bytes))
}

struct WriterState {
    format: ExportFormat,
    columns: Vec<String>,
    csv: Option<csv::Writer<Vec<u8>>>,
    statement: usize,
    result: usize,
    first_result: bool,
    first_json_row: bool,
    rows: u64,
}

impl WriterState {
    fn new(format: ExportFormat, writer: &mut impl Write) -> Result<Self> {
        if format == ExportFormat::Json {
            writer.write_all(b"{\"results\":[")?;
        }
        Ok(Self {
            format,
            columns: Vec::new(),
            csv: None,
            statement: 0,
            result: 0,
            first_result: true,
            first_json_row: true,
            rows: 0,
        })
    }

    fn event(&mut self, event: QueryEvent, writer: &mut impl Write) -> Result<()> {
        match event {
            QueryEvent::ResultStarted {
                statement,
                result,
                columns,
            } => {
                self.close_result(writer)?;
                self.statement = statement;
                self.result = result;
                self.columns = columns;
                self.first_json_row = true;
                match self.format {
                    ExportFormat::Csv => {
                        let mut csv = csv::WriterBuilder::new()
                            .has_headers(false)
                            .from_writer(Vec::new());
                        csv.write_record(&self.columns).map_err(csv_error)?;
                        self.csv = Some(csv);
                    }
                    ExportFormat::Json => {
                        if !self.first_result {
                            writer.write_all(b",")?;
                        }
                        write!(
                            writer,
                            "{{\"statement\":{statement},\"result\":{result},\"columns\":"
                        )?;
                        serde_json::to_writer(&mut *writer, &self.columns)?;
                        writer.write_all(b",\"rows\":[")?;
                    }
                    ExportFormat::Jsonl => {}
                }
                self.first_result = false;
            }
            QueryEvent::Rows { rows, .. } => {
                for row in rows {
                    self.write_row(row, writer)?;
                    self.rows = self.rows.saturating_add(1);
                }
            }
            QueryEvent::Error(error) => {
                return Err(Error::failed(
                    "SQL export query가 실패했습니다",
                    error.message,
                    "SQL 오류를 수정한 뒤 다시 export하세요.",
                ));
            }
            QueryEvent::StatementStarted { .. }
            | QueryEvent::StatementComplete { .. }
            | QueryEvent::Truncated { .. }
            | QueryEvent::Finished(_) => {}
        }
        Ok(())
    }

    fn write_row(&mut self, row: Vec<Option<String>>, writer: &mut impl Write) -> Result<()> {
        match self.format {
            ExportFormat::Csv => {
                let csv = self.csv.as_mut().expect("result starts before rows");
                csv.write_record(row.iter().map(|cell| cell.as_deref().unwrap_or("\\N")))
                    .map_err(csv_error)?;
            }
            ExportFormat::Json | ExportFormat::Jsonl => {
                let object: BTreeMap<&str, Option<&str>> = self
                    .columns
                    .iter()
                    .zip(row.iter())
                    .map(|(column, value)| (column.as_str(), value.as_deref()))
                    .collect();
                if self.format == ExportFormat::Json {
                    if !self.first_json_row {
                        writer.write_all(b",")?;
                    }
                    serde_json::to_writer(&mut *writer, &object)?;
                    self.first_json_row = false;
                } else {
                    write!(
                        writer,
                        "{{\"statement\":{},\"result\":{},\"row\":",
                        self.statement, self.result
                    )?;
                    serde_json::to_writer(&mut *writer, &object)?;
                    writer.write_all(b"}\n")?;
                }
            }
        }
        Ok(())
    }

    fn close_result(&mut self, writer: &mut impl Write) -> Result<()> {
        if self.first_result {
            return Ok(());
        }
        match self.format {
            ExportFormat::Csv => {
                let mut csv = self.csv.take().expect("CSV result writer exists");
                csv.flush()?;
                let bytes = csv
                    .into_inner()
                    .map_err(|error| Error::from(error.into_error()))?;
                writer.write_all(&bytes)?;
                writer.write_all(b"\n")?;
            }
            ExportFormat::Json => writer.write_all(b"]}")?,
            ExportFormat::Jsonl => {}
        }
        Ok(())
    }

    fn finish(&mut self, writer: &mut impl Write) -> Result<()> {
        self.close_result(writer)?;
        if self.format == ExportFormat::Json {
            writer.write_all(b"]}\n")?;
        }
        Ok(())
    }
}

struct CountingWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn csv_error(error: csv::Error) -> Error {
    Error::failed(
        "CSV export에 실패했습니다",
        error.to_string(),
        "export 경로의 권한과 디스크 공간을 확인하세요.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_writer_preserves_null_and_unicode() {
        let mut bytes = Vec::new();
        let mut state = WriterState::new(ExportFormat::Json, &mut bytes).unwrap();
        state
            .event(
                QueryEvent::ResultStarted {
                    statement: 0,
                    result: 0,
                    columns: vec!["name".into(), "note".into()],
                },
                &mut bytes,
            )
            .unwrap();
        state
            .event(
                QueryEvent::Rows {
                    statement: 0,
                    result: 0,
                    rows: vec![vec![Some("한글".into()), None]],
                },
                &mut bytes,
            )
            .unwrap();
        state.finish(&mut bytes).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["results"][0]["rows"][0]["name"], "한글");
        assert!(value["results"][0]["rows"][0]["note"].is_null());
    }
}
