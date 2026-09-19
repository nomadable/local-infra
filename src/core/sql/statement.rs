//! SQL-aware statement boundaries, lightweight highlighting, and formatting.
//!
//! The scanner is deliberately not a security parser. Read-only enforcement is
//! performed by PostgreSQL transactions; this module only drives editor UX.

use std::ops::Range;

const KEYWORDS: &[&str] = &[
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CHECK",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATERIALIZED",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "REFERENCES",
    "RELEASE",
    "RETURNING",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SAVEPOINT",
    "SCHEMA",
    "SELECT",
    "SEQUENCE",
    "SET",
    "SHOW",
    "START",
    "TABLE",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    String,
    Comment,
    Number,
    Identifier,
    Symbol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub range: Range<usize>,
    pub kind: TokenKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEffect {
    None,
    Begin,
    End,
}

pub fn statement_ranges(sql: &str) -> Vec<Range<usize>> {
    let bytes = sql.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut cursor = 0usize;
    let mut state = State::Normal;
    while cursor < bytes.len() {
        match &mut state {
            State::Normal => {
                if starts(bytes, cursor, b"--") {
                    state = State::LineComment;
                    cursor += 2;
                } else if starts(bytes, cursor, b"/*") {
                    state = State::BlockComment(1);
                    cursor += 2;
                } else if bytes[cursor] == b'\'' {
                    state = State::SingleQuote;
                    cursor += 1;
                } else if bytes[cursor] == b'"' {
                    state = State::DoubleQuote;
                    cursor += 1;
                } else if bytes[cursor] == b'$' {
                    if let Some(delimiter) = dollar_delimiter(sql, cursor) {
                        cursor += delimiter.len();
                        state = State::DollarQuote(delimiter);
                    } else {
                        cursor += 1;
                    }
                } else if bytes[cursor] == b';' {
                    let end = cursor + 1;
                    if has_sql(&sql[start..end]) {
                        ranges.push(trimmed_range(sql, start..end));
                    }
                    start = end;
                    cursor = end;
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
            State::SingleQuote => {
                if starts(bytes, cursor, b"''") {
                    cursor += 2;
                } else if bytes[cursor] == b'\'' {
                    cursor += 1;
                    state = State::Normal;
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
            State::DoubleQuote => {
                if starts(bytes, cursor, b"\"\"") {
                    cursor += 2;
                } else if bytes[cursor] == b'"' {
                    cursor += 1;
                    state = State::Normal;
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
            State::DollarQuote(delimiter) => {
                if sql[cursor..].starts_with(delimiter.as_str()) {
                    cursor += delimiter.len();
                    state = State::Normal;
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
            State::LineComment => {
                if bytes[cursor] == b'\n' {
                    state = State::Normal;
                }
                cursor += char_len(sql, cursor);
            }
            State::BlockComment(depth) => {
                if starts(bytes, cursor, b"/*") {
                    *depth += 1;
                    cursor += 2;
                } else if starts(bytes, cursor, b"*/") {
                    *depth -= 1;
                    cursor += 2;
                    if *depth == 0 {
                        state = State::Normal;
                    }
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
        }
    }
    if start < sql.len() && has_sql(&sql[start..]) {
        ranges.push(trimmed_range(sql, start..sql.len()));
    }
    ranges
}

pub fn statements(sql: &str) -> Vec<&str> {
    statement_ranges(sql)
        .into_iter()
        .map(|range| &sql[range])
        .collect()
}

pub fn statement_at(sql: &str, cursor: usize) -> Option<Range<usize>> {
    let cursor = floor_boundary(sql, cursor.min(sql.len()));
    let ranges = statement_ranges(sql);
    ranges
        .iter()
        .find(|range| cursor >= range.start && cursor < range.end)
        .cloned()
        .or_else(|| {
            ranges
                .iter()
                .rev()
                .find(|range| cursor == range.end)
                .cloned()
        })
        .or_else(|| ranges.into_iter().find(|range| range.start > cursor))
}

pub fn execution_sql(
    buffer: &str,
    cursor: usize,
    selection: Option<Range<usize>>,
    whole_buffer: bool,
) -> Option<&str> {
    let range = if whole_buffer {
        let start = buffer.len() - buffer.trim_start().len();
        let end = buffer.trim_end().len();
        (start < end).then_some(start..end)?
    } else if let Some(selection) = selection.filter(|range| range.start < range.end) {
        let start = floor_boundary(buffer, selection.start.min(buffer.len()));
        let end = floor_boundary(buffer, selection.end.min(buffer.len()));
        (start < end).then_some(start..end)?
    } else {
        statement_at(buffer, cursor)?
    };
    Some(&buffer[range])
}

pub fn transaction_effect(sql: &str) -> TransactionEffect {
    match first_keywords(sql, 2).as_slice() {
        [first, ..] if first == "BEGIN" => TransactionEffect::Begin,
        [first, second, ..] if first == "START" && second == "TRANSACTION" => {
            TransactionEffect::Begin
        }
        [first, ..] if first == "COMMIT" || first == "ROLLBACK" => TransactionEffect::End,
        _ => TransactionEffect::None,
    }
}

pub fn is_transaction_control(sql: &str) -> bool {
    matches!(
        first_keywords(sql, 2).first().map(String::as_str),
        Some("BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE")
    )
}

pub fn format(sql: &str) -> String {
    sqlformat::format(
        sql,
        &sqlformat::QueryParams::None,
        &sqlformat::FormatOptions::default(),
    )
}

pub fn is_keyword(value: &str) -> bool {
    let value = value.to_ascii_uppercase();
    KEYWORDS.binary_search(&value.as_str()).is_ok()
}

pub fn keyword_completions(prefix: &str) -> Vec<String> {
    let prefix = prefix.to_ascii_uppercase();
    KEYWORDS
        .iter()
        .filter(|keyword| keyword.starts_with(&prefix))
        .map(|keyword| (*keyword).to_string())
        .collect()
}

pub fn identifier_prefix(sql: &str, cursor: usize) -> Range<usize> {
    let cursor = floor_boundary(sql, cursor.min(sql.len()));
    let mut start = cursor;
    while start > 0 {
        let (index, ch) = sql[..start].char_indices().next_back().expect("not empty");
        if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '"' {
            start = index;
        } else {
            break;
        }
    }
    start..cursor
}

pub fn tokens(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if sql[cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
        {
            cursor += char_len(sql, cursor);
            continue;
        }
        let start = cursor;
        if starts(bytes, cursor, b"--") {
            cursor += 2;
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                cursor += char_len(sql, cursor);
            }
            out.push(Token {
                range: start..cursor,
                kind: TokenKind::Comment,
            });
            continue;
        }
        if starts(bytes, cursor, b"/*") {
            cursor += 2;
            let mut depth = 1usize;
            while cursor < bytes.len() && depth > 0 {
                if starts(bytes, cursor, b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if starts(bytes, cursor, b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
            out.push(Token {
                range: start..cursor,
                kind: TokenKind::Comment,
            });
            continue;
        }
        if bytes[cursor] == b'\'' {
            cursor += 1;
            while cursor < bytes.len() {
                if starts(bytes, cursor, b"''") {
                    cursor += 2;
                } else if bytes[cursor] == b'\'' {
                    cursor += 1;
                    break;
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
            out.push(Token {
                range: start..cursor,
                kind: TokenKind::String,
            });
            continue;
        }
        if bytes[cursor] == b'$' {
            if let Some(delimiter) = dollar_delimiter(sql, cursor) {
                cursor += delimiter.len();
                if let Some(relative) = sql[cursor..].find(&delimiter) {
                    cursor += relative + delimiter.len();
                } else {
                    cursor = sql.len();
                }
                out.push(Token {
                    range: start..cursor,
                    kind: TokenKind::String,
                });
                continue;
            }
        }
        if bytes[cursor] == b'"' {
            cursor += 1;
            while cursor < bytes.len() {
                if starts(bytes, cursor, b"\"\"") {
                    cursor += 2;
                } else if bytes[cursor] == b'"' {
                    cursor += 1;
                    break;
                } else {
                    cursor += char_len(sql, cursor);
                }
            }
            out.push(Token {
                range: start..cursor,
                kind: TokenKind::Identifier,
            });
            continue;
        }
        let ch = sql[cursor..].chars().next().expect("in bounds");
        if ch.is_ascii_digit() {
            cursor += ch.len_utf8();
            while cursor < bytes.len() {
                let ch = sql[cursor..].chars().next().expect("in bounds");
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-') {
                    cursor += ch.len_utf8();
                } else {
                    break;
                }
            }
            out.push(Token {
                range: start..cursor,
                kind: TokenKind::Number,
            });
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            cursor += ch.len_utf8();
            while cursor < bytes.len() {
                let ch = sql[cursor..].chars().next().expect("in bounds");
                if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                    cursor += ch.len_utf8();
                } else {
                    break;
                }
            }
            let word = sql[start..cursor].to_ascii_uppercase();
            out.push(Token {
                range: start..cursor,
                kind: if KEYWORDS.binary_search(&word.as_str()).is_ok() {
                    TokenKind::Keyword
                } else {
                    TokenKind::Identifier
                },
            });
            continue;
        }
        cursor += ch.len_utf8();
        out.push(Token {
            range: start..cursor,
            kind: TokenKind::Symbol,
        });
    }
    out
}

fn first_keywords(sql: &str, limit: usize) -> Vec<String> {
    tokens(sql)
        .into_iter()
        .filter(|token| matches!(token.kind, TokenKind::Keyword | TokenKind::Identifier))
        .take(limit)
        .map(|token| sql[token.range].to_ascii_uppercase())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Normal,
    SingleQuote,
    DoubleQuote,
    DollarQuote(String),
    LineComment,
    BlockComment(usize),
}

fn starts(bytes: &[u8], cursor: usize, pattern: &[u8]) -> bool {
    bytes.get(cursor..cursor.saturating_add(pattern.len())) == Some(pattern)
}

fn char_len(sql: &str, cursor: usize) -> usize {
    sql[cursor..]
        .chars()
        .next()
        .map(char::len_utf8)
        .unwrap_or(1)
}

fn floor_boundary(sql: &str, mut index: usize) -> usize {
    while index > 0 && !sql.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn dollar_delimiter(sql: &str, cursor: usize) -> Option<String> {
    let tail = sql.get(cursor..)?;
    if !tail.starts_with('$') {
        return None;
    }
    let close = tail[1..].find('$')? + 1;
    let tag = &tail[1..close];
    if tag
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && tag.chars().next().is_none_or(|ch| !ch.is_ascii_digit())
    {
        Some(tail[..=close].to_string())
    } else {
        None
    }
}

fn trimmed_range(sql: &str, range: Range<usize>) -> Range<usize> {
    let slice = &sql[range.clone()];
    let leading = slice.len() - slice.trim_start().len();
    let trailing_end = slice.trim_end().len();
    range.start + leading..range.start + trailing_end
}

fn has_sql(sql: &str) -> bool {
    tokens(sql)
        .iter()
        .any(|token| token.kind != TokenKind::Comment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitter_ignores_semicolons_in_postgres_quoted_forms() {
        let sql = "select ';'; -- ;\n select $$a;b$$; /* ; /* nested ; */ */ select 3";
        assert_eq!(
            statements(sql),
            vec![
                "select ';';",
                "-- ;\n select $$a;b$$;",
                "/* ; /* nested ; */ */ select 3"
            ]
        );
    }

    #[test]
    fn current_statement_uses_utf8_byte_boundaries() {
        let sql = "select '한글';\nselect 2;";
        let cursor = sql.find('2').unwrap();
        assert_eq!(&sql[statement_at(sql, cursor).unwrap()], "select 2;");
    }

    #[test]
    fn transaction_control_is_editor_guidance_only() {
        assert!(is_transaction_control("/* x */ START TRANSACTION"));
        assert_eq!(transaction_effect("begin;"), TransactionEffect::Begin);
        assert_eq!(transaction_effect("rollback;"), TransactionEffect::End);
        assert!(!is_transaction_control("select 'commit'"));
    }

    #[test]
    fn lexer_classifies_sql_without_panicking_on_unicode() {
        let sql = "SELECT 이름, 42 FROM \"사용자\" -- 메모";
        let kinds: Vec<_> = tokens(sql).into_iter().map(|token| token.kind).collect();
        assert_eq!(kinds.first(), Some(&TokenKind::Keyword));
        assert!(kinds.contains(&TokenKind::Number));
        assert!(kinds.contains(&TokenKind::Comment));
    }
}
