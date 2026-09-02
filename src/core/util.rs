//! Small, dependency-free helpers shared by every core module.

use crate::core::error::{Error, Result};
use rand::Rng;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// PostgreSQL reserved words that cannot be used unquoted as a database or role
/// name. This app never quotes generated identifiers, so it rejects them.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "authorization",
    "binary",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "cross",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "inner",
    "intersect",
    "into",
    "is",
    "join",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "natural",
    "new",
    "not",
    "null",
    "off",
    "offset",
    "old",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "overlaps",
    "placing",
    "primary",
    "references",
    "right",
    "select",
    "session_user",
    "similar",
    "some",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "verbose",
    "when",
    "where",
    "with",
];

/// Validate a PostgreSQL database or role name against the rules this app
/// relies on: unquoted-safe, lowercase, ≤63 bytes, no `pg_` prefix (DB-003).
pub fn validate_pg_identifier(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Usage(format!("{kind}을(를) 입력하세요.")));
    }
    if name.len() > 63 {
        return Err(Error::Usage(format!(
            "{kind}은(는) 63바이트를 넘을 수 없습니다 (현재 {}바이트).",
            name.len()
        )));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("checked non-empty");
    if !(first.is_ascii_lowercase() || first == '_') {
        return Err(Error::Usage(format!(
            "{kind}은(는) 소문자 또는 밑줄로 시작해야 합니다: `{name}`"
        )));
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$') {
            return Err(Error::Usage(format!(
                "{kind}에는 소문자, 숫자, `_`, `$`만 사용할 수 있습니다: `{name}`"
            )));
        }
    }
    if name.starts_with("pg_") {
        return Err(Error::Usage(format!(
            "{kind}은(는) `pg_`로 시작할 수 없습니다 (PostgreSQL 예약 접두사): `{name}`"
        )));
    }
    if RESERVED.contains(&name) {
        return Err(Error::Usage(format!(
            "`{name}`은(는) PostgreSQL 예약어라 {kind}으로 사용할 수 없습니다."
        )));
    }
    Ok(())
}

/// Turn a free-form project name into a legal identifier stem
/// (`Letsbid` → `letsbid`, `Dalbit Editor` → `dalbit_editor`).
pub fn slugify(project: &str) -> String {
    let mut out = String::with_capacity(project.len());
    let mut prev_underscore = false;
    for c in project.chars() {
        let mapped = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '_'
        };
        if mapped == '_' {
            if prev_underscore || out.is_empty() {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        out.push(mapped);
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out.truncate(50);
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Suggested `(database_name, username)` for a project (PRD §9.1 step 4).
pub fn suggest_names(project: &str) -> (String, String) {
    let stem = slugify(project);
    (format!("{stem}_dev"), format!("{stem}_user"))
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// Alphabet without characters that are easy to misread or that need shell or
/// URL escaping in the common case.
const PASSWORD_ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789-._~";

pub fn generate_password(len: usize) -> String {
    let len = len.max(16);
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| PASSWORD_ALPHABET[rng.gen_range(0..PASSWORD_ALPHABET.len())] as char)
        .collect()
}

/// Replace anything that looks like a password inside a URL or a `PGPASSWORD=`
/// assignment. Applied to every string that reaches a log, the activity table
/// or the diagnostics clipboard (PRD §11.1).
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if text[i..].starts_with("://") {
            // scheme://user:password@host  →  scheme://user:****@host
            let rest = &text[i + 3..];
            if let Some(at) = rest.find('@') {
                let creds = &rest[..at];
                let stop = creds.find(['/', ' ', '\n']).is_some();
                if !stop {
                    if let Some(colon) = creds.find(':') {
                        out.push_str("://");
                        out.push_str(&creds[..colon]);
                        out.push_str(":****");
                        out.push('@');
                        i += 3 + at + 1;
                        continue;
                    }
                }
            }
        }
        for key in ["PGPASSWORD=", "password=", "PASSWORD="] {
            if text[i..].starts_with(key) {
                out.push_str(key);
                out.push_str("****");
                i += key.len();
                let rest = &text[i..];
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
                    .unwrap_or(rest.len());
                i += end;
                break;
            }
        }
        if i >= bytes.len() {
            break;
        }
        let ch = text[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Percent-encode a URL userinfo component.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

/// POSIX single-quote a token so it survives the remote shell that `ssh`
/// spawns. Never used to embed secrets — those go over stdin (PRD §11.2).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'_' | b'.' | b'/' | b':' | b'=' | b',' | b'@' | b'+'
                )
        })
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Join an argv into a single command line for a remote shell.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// True when a TCP listener can be bound, i.e. the port is free locally.
///
/// Loopback publication still collides with a process listening on `0.0.0.0`
/// (Docker Desktop reports that as `Bind for 0.0.0.0:PORT`). Checking only
/// `127.0.0.1` would miss it.
pub fn local_port_free(host: &str, port: u16) -> bool {
    if !bind_ok(host, port) {
        return false;
    }
    match host {
        "127.0.0.1" | "::1" | "localhost" => bind_ok("0.0.0.0", port),
        _ => true,
    }
}

fn bind_ok(host: &str, port: u16) -> bool {
    std::net::TcpListener::bind((host, port)).is_ok()
}

/// First free local port at or after `start`, scanning at most `span` ports.
pub fn pick_local_port(host: &str, start: u16, span: u16) -> Option<u16> {
    (start..=start.saturating_add(span)).find(|p| *p != 0 && local_port_free(host, *p))
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Truncate to a terminal *column* budget, honouring CJK double-width
/// characters (PRD §12.4). Appends `…` when it had to cut.
pub fn truncate_cols(text: &str, max_cols: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_cols == 0 {
        return String::new();
    }
    let total: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= max_cols {
        return text.to_string();
    }
    let budget = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Display width in terminal columns.
pub fn display_cols(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_rules_match_postgres_constraints() {
        assert!(validate_pg_identifier("DB명", "letsbid_dev").is_ok());
        assert!(validate_pg_identifier("DB명", "_x$1").is_ok());
        assert!(validate_pg_identifier("DB명", "").is_err());
        assert!(
            validate_pg_identifier("DB명", "Letsbid").is_err(),
            "uppercase rejected"
        );
        assert!(
            validate_pg_identifier("DB명", "1abc").is_err(),
            "digit start rejected"
        );
        assert!(
            validate_pg_identifier("DB명", "a-b").is_err(),
            "hyphen rejected"
        );
        assert!(
            validate_pg_identifier("DB명", "pg_stat").is_err(),
            "pg_ prefix reserved"
        );
        assert!(
            validate_pg_identifier("DB명", "user").is_err(),
            "reserved word"
        );
        assert!(
            validate_pg_identifier("DB명", &"a".repeat(64)).is_err(),
            "63 byte limit"
        );
        assert!(validate_pg_identifier("DB명", &"a".repeat(63)).is_ok());
    }

    #[test]
    fn suggested_names_are_always_valid_identifiers() {
        for project in ["Letsbid", "Dalbit Editor", "  9 lives  ", "탐체", "a--b__c"] {
            let (db, user) = suggest_names(project);
            validate_pg_identifier("DB명", &db).unwrap_or_else(|e| panic!("{project}: {e}"));
            validate_pg_identifier("사용자명", &user).unwrap_or_else(|e| panic!("{project}: {e}"));
        }
        assert_eq!(suggest_names("Dalbit Editor").0, "dalbit_editor_dev");
        assert_eq!(suggest_names("Letsbid").1, "letsbid_user");
    }

    #[test]
    fn generated_passwords_are_url_safe_and_long_enough() {
        let pw = generate_password(4);
        assert_eq!(pw.len(), 16, "short requests are raised to the floor");
        let pw = generate_password(32);
        assert_eq!(pw.len(), 32);
        assert_eq!(pct_encode(&pw), pw, "alphabet needs no percent-encoding");
    }

    #[test]
    fn redaction_hides_passwords_in_urls_and_env_assignments() {
        let text = "postgresql://u:secret@127.0.0.1:5432/db PGPASSWORD=topsecret next";
        let out = redact(text);
        assert!(!out.contains("secret@"));
        assert!(!out.contains("topsecret"));
        assert!(out.contains("postgresql://u:****@127.0.0.1:5432/db"));
        assert!(out.contains("PGPASSWORD=****"));
        assert!(out.contains("next"), "surrounding text is preserved");
    }

    #[test]
    fn shell_quoting_survives_hostile_tokens() {
        assert_eq!(shell_quote("docker"), "docker");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(
            shell_join(&["docker".into(), "exec".into(), "a b".into()]),
            "docker exec 'a b'"
        );
    }

    #[test]
    fn cjk_columns_are_measured_not_counted() {
        assert_eq!(display_cols("한글"), 4);
        assert_eq!(display_cols("ab"), 2);
        assert_eq!(truncate_cols("한글자름", 5), "한글…");
        assert_eq!(truncate_cols("abcdef", 6), "abcdef");
        assert_eq!(truncate_cols("abcdef", 4), "abc…");
    }

    #[test]
    fn human_bytes_reads_like_the_mockups() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(84 * 1024 * 1024), "84.0 MB");
        assert_eq!(human_bytes(240 * 1024 * 1024), "240 MB");
    }

    #[test]
    fn port_zero_is_never_suggested() {
        let p = pick_local_port("127.0.0.1", 15432, 100).expect("a free port exists");
        assert!(p >= 15432);
    }
}
