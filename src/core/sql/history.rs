//! Query history metadata and conservative credential redaction.

use crate::core::error::Result;
use crate::core::sql::profile::{
    QueryHistoryEntry, SqlConnectionSource, SqlEndpoint, DEFAULT_HISTORY_ENTRIES,
};
use crate::core::util::{new_id, now};
use crate::core::Ctx;

pub fn record(
    ctx: &Ctx,
    source: &SqlConnectionSource,
    endpoint: &SqlEndpoint,
    sql: &str,
    success: bool,
    elapsed_ms: u64,
    row_count: u64,
) -> Result<QueryHistoryEntry> {
    let entry = QueryHistoryEntry {
        id: new_id(),
        profile_id: match source {
            SqlConnectionSource::ExternalProfile { profile_id }
                if ctx.store.find_sql_profile(profile_id)?.is_some() =>
            {
                Some(profile_id.clone())
            }
            SqlConnectionSource::ExternalProfile { .. }
            | SqlConnectionSource::ManagedDatabase { .. } => None,
        },
        profile_label: endpoint.label.clone(),
        executed_at: now(),
        success,
        elapsed_ms,
        row_count,
        query_text: endpoint.history_text.then(|| redact(sql)),
    };
    ctx.store.insert_sql_history(&entry)?;
    ctx.store.trim_sql_history(DEFAULT_HISTORY_ENTRIES)?;
    Ok(entry)
}

pub fn list(ctx: &Ctx, limit: usize) -> Result<Vec<QueryHistoryEntry>> {
    ctx.store
        .list_sql_history(limit.min(DEFAULT_HISTORY_ENTRIES))
}

/// Mask forms that are known to carry credentials. This is intentionally
/// conservative and is not a claim that arbitrary SQL literals are safe.
pub fn redact(sql: &str) -> String {
    let mut out = redact_connection_uris(sql);
    let lower = out.to_ascii_lowercase();
    let mut search = 0usize;
    while let Some(relative) = lower[search..].find("password") {
        let keyword_end = search + relative + "password".len();
        let Some(quote_relative) = out[keyword_end..].find('\'') else {
            search = keyword_end;
            continue;
        };
        let quote = keyword_end + quote_relative;
        let Some(end) = quoted_literal_end(&out, quote) else {
            out.replace_range(quote.., "'[REDACTED]'");
            break;
        };
        out.replace_range(quote..end, "'[REDACTED]'");
        search = quote + "'[REDACTED]'".len();
    }
    out
}

fn redact_connection_uris(input: &str) -> String {
    let mut out = input.to_string();
    let mut cursor = 0usize;
    loop {
        let lower = out[cursor..].to_ascii_lowercase();
        let Some(relative) = lower
            .find("postgresql://")
            .or_else(|| lower.find("postgres://"))
        else {
            break;
        };
        let start = cursor + relative;
        let scheme_end = out[start..]
            .find("//")
            .map(|offset| start + offset + 2)
            .unwrap_or(start);
        let authority_end = out[scheme_end..]
            .find(|ch: char| ch == '/' || ch.is_whitespace() || matches!(ch, '\'' | '"'))
            .map(|offset| scheme_end + offset)
            .unwrap_or(out.len());
        let Some(at_relative) = out[scheme_end..authority_end].rfind('@') else {
            cursor = authority_end;
            continue;
        };
        let at = scheme_end + at_relative;
        let Some(colon_relative) = out[scheme_end..at].find(':') else {
            cursor = authority_end;
            continue;
        };
        let password_start = scheme_end + colon_relative + 1;
        out.replace_range(password_start..at, "[REDACTED]");
        cursor = password_start + "[REDACTED]".len() + 1;
    }
    out
}

fn quoted_literal_end(input: &str, quote: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut cursor = quote + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\'' {
            if bytes.get(cursor + 1) == Some(&b'\'') {
                cursor += 2;
            } else {
                return Some(cursor + 1);
            }
        } else {
            cursor += input[cursor..].chars().next()?.len_utf8();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_password_clauses_and_connection_uris() {
        let sql = "alter role x password 's''ecret'; select 'postgresql://u:p@db/app'";
        let redacted = redact(sql);
        assert!(!redacted.contains("s''ecret"));
        assert!(!redacted.contains(":p@"));
        assert!(redacted.contains("[REDACTED]"));
    }
}
