//! Result tabs and keyboard viewport state.

use crate::core::sql::runner::{ExecutionSummary, QueryEvent, SqlExecutionError};

#[derive(Debug, Clone, Default)]
pub struct ResultTab {
    pub statement: usize,
    pub result: usize,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub affected_rows: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub truncated: bool,
    pub error: Option<SqlExecutionError>,
}

#[derive(Debug, Default)]
pub struct Results {
    pub tabs: Vec<ResultTab>,
    pub active: usize,
    pub row: usize,
    pub column: usize,
    pub row_offset: usize,
    pub column_offset: usize,
    pub running: bool,
    pub summary: Option<ExecutionSummary>,
}

impl Results {
    pub fn begin(&mut self) {
        self.tabs.clear();
        self.active = 0;
        self.row = 0;
        self.column = 0;
        self.row_offset = 0;
        self.column_offset = 0;
        self.running = true;
        self.summary = None;
    }

    pub fn apply(&mut self, event: QueryEvent) {
        match event {
            QueryEvent::StatementStarted { .. } => {}
            QueryEvent::ResultStarted {
                statement,
                result,
                columns,
            } => {
                self.tabs.push(ResultTab {
                    statement,
                    result,
                    columns,
                    ..Default::default()
                });
                self.active = self.tabs.len() - 1;
            }
            QueryEvent::Rows {
                statement,
                result,
                rows,
            } => {
                if let Some(tab) = self.tab_mut(statement, result) {
                    tab.rows.extend(rows);
                }
            }
            QueryEvent::StatementComplete {
                index,
                affected_rows,
                elapsed_ms,
            } => {
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .rev()
                    .find(|tab| tab.statement == index)
                {
                    tab.affected_rows = Some(affected_rows);
                    tab.elapsed_ms = Some(elapsed_ms);
                } else {
                    self.tabs.push(ResultTab {
                        statement: index,
                        affected_rows: Some(affected_rows),
                        elapsed_ms: Some(elapsed_ms),
                        ..Default::default()
                    });
                    self.active = self.tabs.len() - 1;
                }
            }
            QueryEvent::Truncated { statement, .. } => {
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .rev()
                    .find(|tab| tab.statement == statement)
                {
                    tab.truncated = true;
                }
            }
            QueryEvent::Error(error) => {
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .rev()
                    .find(|tab| tab.statement == error.statement)
                {
                    tab.error = Some(error);
                } else {
                    self.tabs.push(ResultTab {
                        statement: error.statement,
                        error: Some(error),
                        ..Default::default()
                    });
                    self.active = self.tabs.len() - 1;
                }
            }
            QueryEvent::Finished(summary) => {
                self.running = false;
                self.summary = Some(summary);
            }
        }
        self.clamp();
    }

    pub fn active(&self) -> Option<&ResultTab> {
        self.tabs.get(self.active)
    }

    pub fn switch(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        self.active = if delta < 0 {
            (self.active + self.tabs.len() - 1) % self.tabs.len()
        } else {
            (self.active + 1) % self.tabs.len()
        };
        self.row = 0;
        self.column = 0;
        self.row_offset = 0;
        self.column_offset = 0;
    }

    pub fn move_row(&mut self, delta: isize, viewport: usize) {
        self.row = self.row.saturating_add_signed(delta);
        self.clamp();
        if self.row < self.row_offset {
            self.row_offset = self.row;
        } else if viewport > 0 && self.row >= self.row_offset + viewport {
            self.row_offset = self.row + 1 - viewport;
        }
    }

    pub fn move_column(&mut self, delta: isize, viewport: usize) {
        self.column = self.column.saturating_add_signed(delta);
        self.clamp();
        if self.column < self.column_offset {
            self.column_offset = self.column;
        } else if viewport > 0 && self.column >= self.column_offset + viewport {
            self.column_offset = self.column + 1 - viewport;
        }
    }

    pub fn selected_cell(&self) -> Option<Option<&str>> {
        self.active()?
            .rows
            .get(self.row)?
            .get(self.column)
            .map(|cell| cell.as_deref())
    }

    pub fn selected_tsv(&self) -> Option<String> {
        self.selected_cell()
            .map(|cell| cell.unwrap_or("NULL").to_string())
    }

    fn tab_mut(&mut self, statement: usize, result: usize) -> Option<&mut ResultTab> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.statement == statement && tab.result == result)
    }

    fn clamp(&mut self) {
        if self.tabs.is_empty() {
            self.active = 0;
            self.row = 0;
            self.column = 0;
            return;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        let tab = &self.tabs[self.active];
        self.row = self.row.min(tab.rows.len().saturating_sub(1));
        self.column = self.column.min(tab.columns.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_empty_cells_stay_distinct() {
        let mut results = Results::default();
        results.apply(QueryEvent::ResultStarted {
            statement: 0,
            result: 0,
            columns: vec!["v".into()],
        });
        results.apply(QueryEvent::Rows {
            statement: 0,
            result: 0,
            rows: vec![vec![None], vec![Some(String::new())]],
        });
        assert_eq!(results.selected_cell(), Some(None));
        results.move_row(1, 1);
        assert_eq!(results.selected_cell(), Some(Some("")));
    }
}
