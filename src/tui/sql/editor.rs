//! UTF-8 SQL buffers, selection, undo/redo, and crash recovery.

use crate::core::config::harden_file;
use crate::core::error::{Error, Result};
use crate::core::util::new_id;
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const MAX_RECOVERED: usize = 20;
const UNDO_LIMIT: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recovery {
    id: String,
    title: String,
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone)]
pub struct Buffer {
    pub id: String,
    pub title: String,
    pub text: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub dirty: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl Buffer {
    pub fn empty(index: usize) -> Self {
        Self {
            id: new_id(),
            title: format!("query {index}"),
            text: String::new(),
            cursor: 0,
            anchor: None,
            dirty: false,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|range| &self.text[range])
    }

    pub fn insert(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        self.checkpoint();
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.text.replace_range(range.clone(), value);
        self.cursor = range.start + value.len();
        self.anchor = None;
        self.changed();
    }

    pub fn backspace(&mut self) {
        if let Some(range) = self.selection() {
            self.checkpoint();
            self.text.replace_range(range.clone(), "");
            self.cursor = range.start;
            self.anchor = None;
            self.changed();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.checkpoint();
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.changed();
    }

    pub fn delete(&mut self) {
        if let Some(range) = self.selection() {
            self.checkpoint();
            self.text.replace_range(range.clone(), "");
            self.cursor = range.start;
            self.anchor = None;
            self.changed();
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        self.checkpoint();
        let next = self.cursor
            + self.text[self.cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0);
        self.text.replace_range(self.cursor..next, "");
        self.changed();
    }

    pub fn move_horizontal(&mut self, delta: isize, select: bool) {
        self.begin_selection(select);
        if delta < 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(index, _)| index)
                .unwrap_or(0);
        } else if delta > 0 && self.cursor < self.text.len() {
            self.cursor += self.text[self.cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0);
        }
        self.end_selection(select);
    }

    pub fn move_vertical(&mut self, delta: isize, select: bool) {
        let (line, column) = self.line_column();
        let target = if delta < 0 {
            line.saturating_sub(delta.unsigned_abs())
        } else {
            line.saturating_add(delta as usize)
        };
        self.begin_selection(select);
        self.cursor = self.byte_at_line_column(target, column);
        self.end_selection(select);
    }

    pub fn home(&mut self, select: bool) {
        self.begin_selection(select);
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        self.cursor = line_start;
        self.end_selection(select);
    }

    pub fn end(&mut self, select: bool) {
        self.begin_selection(select);
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset)
            .unwrap_or(self.text.len());
        self.end_selection(select);
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    pub fn undo(&mut self) {
        let Some(snapshot) = self.undo.pop() else {
            return;
        };
        self.redo.push(self.snapshot());
        self.restore(snapshot);
        self.changed();
    }

    pub fn redo(&mut self) {
        let Some(snapshot) = self.redo.pop() else {
            return;
        };
        self.undo.push(self.snapshot());
        self.restore(snapshot);
        self.changed();
    }

    pub fn replace_all(&mut self, text: String) {
        if self.text == text {
            return;
        }
        self.checkpoint();
        self.text = text;
        self.cursor = self.cursor.min(self.text.len());
        self.cursor = floor_boundary(&self.text, self.cursor);
        self.anchor = None;
        self.changed();
    }

    pub fn find(&mut self, needle: &str, reverse: bool) -> bool {
        if needle.is_empty() {
            return false;
        }
        let found = if reverse {
            self.text[..self.cursor]
                .rfind(needle)
                .or_else(|| self.text.rfind(needle))
        } else {
            self.text[self.cursor..]
                .find(needle)
                .map(|offset| self.cursor + offset)
                .or_else(|| self.text.find(needle))
        };
        if let Some(start) = found {
            self.anchor = Some(start);
            self.cursor = start + needle.len();
            true
        } else {
            false
        }
    }

    pub fn goto_line(&mut self, line: usize) {
        self.cursor = self.byte_at_line_column(line.saturating_sub(1), 0);
        self.anchor = None;
    }

    pub fn line_column(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.bytes().filter(|byte| *byte == b'\n').count();
        let start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
        let column = self.text[start..self.cursor].chars().count();
        (line, column)
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.split('\n')
    }

    fn byte_at_line_column(&self, line: usize, column: usize) -> usize {
        let mut start = 0usize;
        for _ in 0..line {
            let Some(next) = self.text[start..].find('\n') else {
                return self.text.len();
            };
            start += next + 1;
        }
        let end = self.text[start..]
            .find('\n')
            .map(|offset| start + offset)
            .unwrap_or(self.text.len());
        self.text[start..end]
            .char_indices()
            .nth(column)
            .map(|(offset, _)| start + offset)
            .unwrap_or(end)
    }

    fn begin_selection(&mut self, select: bool) {
        if select && self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
    }

    fn end_selection(&mut self, select: bool) {
        if !select || self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
    }

    fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor.min(self.text.len());
        self.anchor = snapshot.anchor.filter(|anchor| *anchor <= self.text.len());
    }

    fn changed(&mut self) {
        self.dirty = true;
    }
}

#[derive(Debug)]
pub struct Buffers {
    pub items: Vec<Buffer>,
    pub active: usize,
    recovery_dir: PathBuf,
    last_saved: SystemTime,
}

impl Buffers {
    pub fn load(state_dir: &Path) -> Result<Self> {
        let recovery_dir = state_dir.join("sql-buffers");
        std::fs::create_dir_all(&recovery_dir)?;
        harden_dir(&recovery_dir)?;
        let mut paths: Vec<_> = std::fs::read_dir(&recovery_dir)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort_by_key(|entry| {
            std::cmp::Reverse(
                entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
            )
        });
        let mut items = Vec::new();
        for entry in paths.into_iter().take(MAX_RECOVERED) {
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let Ok(recovery) = serde_json::from_slice::<Recovery>(&bytes) else {
                continue;
            };
            let cursor = floor_boundary(&recovery.text, recovery.cursor.min(recovery.text.len()));
            items.push(Buffer {
                id: recovery.id,
                title: recovery.title,
                text: recovery.text,
                cursor,
                anchor: None,
                dirty: true,
                undo: Vec::new(),
                redo: Vec::new(),
            });
        }
        if items.is_empty() {
            items.push(Buffer::empty(1));
        }
        Ok(Self {
            items,
            active: 0,
            recovery_dir,
            last_saved: SystemTime::UNIX_EPOCH,
        })
    }

    pub fn active(&self) -> &Buffer {
        &self.items[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Buffer {
        &mut self.items[self.active]
    }

    pub fn new_buffer(&mut self) {
        let index = self.items.len() + 1;
        self.items.push(Buffer::empty(index));
        self.active = self.items.len() - 1;
    }

    pub fn close_active(&mut self, force: bool) -> bool {
        if self.active().dirty && !self.active().text.trim().is_empty() && !force {
            return false;
        }
        let id = self.active().id.clone();
        self.items.remove(self.active);
        let _ = std::fs::remove_file(self.recovery_dir.join(format!("{id}.json")));
        if self.items.is_empty() {
            self.items.push(Buffer::empty(1));
        }
        self.active = self.active.min(self.items.len() - 1);
        true
    }

    pub fn switch(&mut self, delta: isize) {
        let len = self.items.len();
        self.active = if delta < 0 {
            (self.active + len - 1) % len
        } else {
            (self.active + 1) % len
        };
    }

    pub fn persist_if_due(&mut self) -> Result<()> {
        if self.last_saved.elapsed().unwrap_or(Duration::MAX) < Duration::from_millis(500) {
            return Ok(());
        }
        self.persist()
    }

    pub fn persist(&mut self) -> Result<()> {
        for buffer in self.items.iter().filter(|buffer| buffer.dirty) {
            let recovery = Recovery {
                id: buffer.id.clone(),
                title: buffer.title.clone(),
                text: buffer.text.clone(),
                cursor: buffer.cursor,
            };
            let path = self.recovery_dir.join(format!("{}.json", buffer.id));
            let temporary = tempfile::NamedTempFile::new_in(&self.recovery_dir)?;
            serde_json::to_writer(temporary.as_file(), &recovery)?;
            temporary.as_file().sync_all()?;
            temporary
                .persist(&path)
                .map_err(|error| Error::from(error.error))?;
            harden_file(&path)?;
        }
        self.last_saved = SystemTime::now();
        Ok(())
    }

    pub fn cleanup(&self) -> Result<()> {
        for buffer in &self.items {
            let path = self.recovery_dir.join(format!("{}.json", buffer.id));
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn recovered_count(&self) -> usize {
        self.items.iter().filter(|buffer| buffer.dirty).count()
    }
}

#[cfg(unix)]
fn harden_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_editing_keeps_cursor_on_boundaries() {
        let mut buffer = Buffer::empty(1);
        buffer.insert("한글");
        buffer.move_horizontal(-1, false);
        buffer.backspace();
        assert_eq!(buffer.text, "글");
        assert_eq!(buffer.cursor, 0);
    }

    #[test]
    fn selection_replacement_is_one_undo_step() {
        let mut buffer = Buffer::empty(1);
        buffer.insert("select 1");
        buffer.anchor = Some(7);
        buffer.cursor = 8;
        buffer.insert("2");
        assert_eq!(buffer.text, "select 2");
        buffer.undo();
        assert_eq!(buffer.text, "select 1");
    }
}
