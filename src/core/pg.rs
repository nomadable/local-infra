//! PostgreSQL adapter (PRD §6.3, §8.3).
//!
//! Everything here runs the Postgres client tools *inside* the engine
//! container with `docker exec`, so neither the workstation nor the VPS needs
//! `psql`, `pg_dump` or `pg_restore` installed (decision: no host client
//! dependency).
//!
//! Two invariants:
//!
//! 1. **Administrative SQL travels on stdin.** `psql … -f -` reads the
//!    statement from stdin, so a `CREATE ROLE … PASSWORD 'x'` never appears in
//!    `ps(1)`. The container image's default `pg_hba.conf` trusts local socket
//!    connections, which is why the admin password is not needed either
//!    (PRD §11.2).
//! 2. **The project role's password only ever moves through `SecretEnv`**, as
//!    `PGPASSWORD` forwarded by the value-less `docker exec --env PGPASSWORD`
//!    form.

use crate::core::docker;
use crate::core::error::{Diagnostic, Error, Result};
use crate::core::exec::{Executor, Output, SecretEnv};
use crate::core::model::{BackupFormat, EngineInstance};

/// OS user inside the official `postgres` image. Running as this user makes
/// the local socket connection authenticate by `peer`/`trust`.
const OS_USER: &str = "postgres";

/// Port PostgreSQL listens on *inside* the container. The host-side port lives
/// in [`EngineInstance::host_port`]; anything reached through `docker exec`
/// talks to the container port.
const CONTAINER_PORT: u16 = 5432;

/// Maintenance database every administrative statement connects to.
const MAINTENANCE_DB: &str = "postgres";

// ---------------------------------------------------------------------------
// Quoting
// ---------------------------------------------------------------------------

/// Quote an identifier: `letsbid_dev` → `"letsbid_dev"`. An embedded double
/// quote is doubled, which is the only escape SQL identifiers have.
pub fn quote_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Quote a string literal: `p'w` → `'p''w'`.
///
/// A value containing a backslash is emitted as an `E''` escape string with
/// the backslashes doubled, mirroring PostgreSQL's own `quote_literal()`. That
/// keeps the result correct whether or not `standard_conforming_strings` is
/// on — which matters because passwords flow through here.
pub fn quote_literal(s: &str) -> String {
    let escaped = s.contains('\\');
    let mut out = String::with_capacity(s.len() + 3);
    if escaped {
        out.push('E');
    }
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// Running SQL
// ---------------------------------------------------------------------------

/// `psql` invocation shared by every administrative call: abort on the first
/// error, ignore `~/.psqlrc`, and print bare unaligned tuples so the output
/// parses without a CSV reader.
fn psql_argv(e: &EngineInstance, database: &str, tail: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "psql".to_string(),
        "-U".into(),
        e.admin_user.clone(),
        "-d".into(),
        database.to_string(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
        "--no-psqlrc".into(),
        "-X".into(),
        "-q".into(),
        "-A".into(),
        "-t".into(),
    ];
    argv.extend(tail.iter().map(|s| (*s).to_string()));
    argv
}

/// Send `sql` to the maintenance database and return its stdout.
pub async fn psql(x: &Executor, e: &EngineInstance, sql: &str) -> Result<String> {
    run_sql(
        x,
        e,
        MAINTENANCE_DB,
        sql,
        "SQL 실행에 실패했습니다",
        "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
    )
    .await
}

/// First value of the first row, trimmed. Empty when the query returned no row.
pub async fn psql_scalar(x: &Executor, e: &EngineInstance, sql: &str) -> Result<String> {
    Ok(first_value(&psql(x, e, sql).await?))
}

async fn run_sql(
    x: &Executor,
    e: &EngineInstance,
    database: &str,
    sql: &str,
    what: &str,
    next: &str,
) -> Result<String> {
    let argv = psql_argv(e, database, &["-f", "-"]);
    let out = docker::exec(
        x,
        &e.container_name,
        Some(OS_USER),
        &argv,
        &SecretEnv::new(),
        Some(sql.as_bytes()),
    )
    .await?;
    if !out.ok() {
        return Err(sql_failure(x, e, &argv, &out, what, next));
    }
    Ok(out.stdout_str())
}

fn sql_failure(
    x: &Executor,
    e: &EngineInstance,
    argv: &[String],
    out: &Output,
    what: &str,
    next: &str,
) -> Error {
    let full = docker::exec_argv(x.docker_bin(), &e.container_name, Some(OS_USER), &[], argv);
    Error::diagnostic(
        Diagnostic::new(
            what.to_string(),
            format!("psql이 종료 코드 {}(으)로 실패했습니다.", out.code),
            next.to_string(),
        )
        .with_command(x.describe(&full))
        .with_output(scrub(&out.message())),
    )
}

/// Belt and braces on top of [`crate::core::util::redact`]: a server message
/// could in principle echo the statement that failed, so any `PASSWORD '…'`
/// clause is masked before the text reaches a diagnostic or the activity log.
fn scrub(text: &str) -> String {
    let redacted = crate::core::util::redact(text);
    let lower = redacted.to_ascii_lowercase();
    let bytes = redacted.as_bytes();
    let mut out = String::with_capacity(redacted.len());
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find("password") {
        let keyword_end = cursor + rel + "password".len();
        out.push_str(&redacted[cursor..keyword_end]);
        let mut open = keyword_end;
        while open < bytes.len() && matches!(bytes[open], b' ' | b'\t' | b'=') {
            open += 1;
        }
        if open < bytes.len() && bytes[open] == b'\'' {
            let mut close = open + 1;
            while close < bytes.len() {
                if bytes[close] == b'\'' {
                    if bytes.get(close + 1) == Some(&b'\'') {
                        close += 2;
                        continue;
                    }
                    break;
                }
                close += 1;
            }
            out.push_str(&redacted[keyword_end..open]);
            out.push_str("'****'");
            cursor = (close + 1).min(redacted.len());
        } else {
            cursor = keyword_end;
        }
    }
    out.push_str(&redacted[cursor..]);
    out
}

fn first_value(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

pub async fn database_exists(x: &Executor, e: &EngineInstance, name: &str) -> Result<bool> {
    let sql = format!(
        "SELECT 1 FROM pg_database WHERE datname = {};",
        quote_literal(name)
    );
    Ok(psql_scalar(x, e, &sql).await? == "1")
}

pub async fn role_exists(x: &Executor, e: &EngineInstance, name: &str) -> Result<bool> {
    let sql = format!(
        "SELECT 1 FROM pg_roles WHERE rolname = {};",
        quote_literal(name)
    );
    Ok(psql_scalar(x, e, &sql).await? == "1")
}

pub async fn list_database_names(x: &Executor, e: &EngineInstance) -> Result<Vec<String>> {
    let out = psql(
        x,
        e,
        "SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname;",
    )
    .await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// True when `database` holds at least one relation outside the system
/// schemas — the check behind the restore overwrite guard (BAK-006).
pub async fn has_user_objects(x: &Executor, e: &EngineInstance, database: &str) -> Result<bool> {
    let sql = "SELECT count(*) FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
               WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
                 AND n.nspname NOT LIKE 'pg_toast%' \
                 AND n.nspname NOT LIKE 'pg_temp%' \
                 AND c.relkind IN ('r', 'p', 'm', 'v', 'S', 'f');";
    let out = run_sql(
        x,
        e,
        database,
        sql,
        "대상 DB의 기존 객체를 확인하지 못했습니다",
        "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(first_value(&out).parse::<i64>().unwrap_or(0) > 0)
}

/// `None` whenever the value cannot be read — a stopped engine yields empty
/// statistics rather than an error (PRD §7.6).
pub async fn database_size_bytes(
    x: &Executor,
    e: &EngineInstance,
    db: &str,
) -> Result<Option<i64>> {
    Ok(soft_scalar(
        x,
        e,
        &format!("SELECT pg_database_size({});", quote_literal(db)),
    )
    .await)
}

/// `None` whenever the value cannot be read. See [`database_size_bytes`].
pub async fn connection_count(x: &Executor, e: &EngineInstance, db: &str) -> Result<Option<i64>> {
    Ok(soft_scalar(
        x,
        e,
        &format!(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = {};",
            quote_literal(db)
        ),
    )
    .await)
}

/// A scalar query that never fails: statistics must not break a list view.
async fn soft_scalar(x: &Executor, e: &EngineInstance, sql: &str) -> Option<i64> {
    let argv = psql_argv(e, MAINTENANCE_DB, &["-f", "-"]);
    let out = docker::exec(
        x,
        &e.container_name,
        Some(OS_USER),
        &argv,
        &SecretEnv::new(),
        Some(sql.as_bytes()),
    )
    .await
    .ok()?;
    if !out.ok() {
        return None;
    }
    first_value(&out.stdout_str()).parse::<i64>().ok()
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// What one project needs on the shared engine (PRD §8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbSpec {
    pub database: String,
    pub owner: String,
    pub encoding: String,
    pub locale: String,
}

/// DB-002: a project role may log in and own its own database, nothing more.
fn create_role_sql(role: &str, password: &str) -> String {
    format!(
        "CREATE ROLE {role} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
         NOREPLICATION NOBYPASSRLS PASSWORD {password};",
        role = quote_ident(role),
        password = quote_literal(password),
    )
}

/// `TEMPLATE template0` is required so a non-default encoding or locale is
/// accepted; `template1` may carry incompatible settings.
fn create_database_sql(s: &DbSpec) -> String {
    format!(
        "CREATE DATABASE {db} WITH OWNER {owner} ENCODING {encoding} \
         LC_COLLATE {locale} LC_CTYPE {locale} TEMPLATE template0;",
        db = quote_ident(&s.database),
        owner = quote_ident(&s.owner),
        encoding = quote_literal(&s.encoding),
        locale = quote_literal(&s.locale),
    )
}

fn create_database_from_template_sql(database: &str, owner: &str, template: &str) -> String {
    format!(
        "CREATE DATABASE {db} WITH OWNER {owner} TEMPLATE {template};",
        db = quote_ident(database),
        owner = quote_ident(owner),
        template = quote_ident(template),
    )
}

/// DB-002: taking `CONNECT` away from `PUBLIC` is what stops another
/// project's role from reading this database.
fn database_grants_sql(database: &str, owner: &str) -> String {
    format!(
        "REVOKE ALL ON DATABASE {db} FROM PUBLIC;\nGRANT ALL ON DATABASE {db} TO {owner};",
        db = quote_ident(database),
        owner = quote_ident(owner),
    )
}

/// Run *inside* the new database: `public` is world-writable by default on
/// PostgreSQL 14 and older, so it has to be handed to the owner explicitly.
fn schema_grants_sql(owner: &str) -> String {
    format!(
        "REVOKE ALL ON SCHEMA public FROM PUBLIC;\n\
         GRANT ALL ON SCHEMA public TO {owner};\n\
         ALTER SCHEMA public OWNER TO {owner};",
        owner = quote_ident(owner),
    )
}

/// Hand every *user* object in the current database to `owner`.
///
/// Two approaches were tried and rejected. `REASSIGN OWNED BY <admin>` is
/// refused outright ("cannot reassign ownership of objects owned by role …
/// because they are required by the database system") because the bootstrap
/// role also owns cluster objects. Filtering by a known source owner does not
/// work either: `pg_dump --format=plain` emits `ALTER … OWNER TO <original>`,
/// so after a plain restore the objects belong to whoever owned them in the
/// source cluster, a name this side cannot know.
///
/// So the rule is stated positively instead: inside a project database every
/// user object belongs to the project role. Extension members and
/// column-owned sequences are skipped: the former would break the extension,
/// and the latter are refused outright ("sequence is linked to table") because
/// they already follow their table's owner.
fn take_ownership_sql(owner: &str) -> String {
    format!(
        "DO $linf$\n\
         DECLARE\n\
             owner_name CONSTANT text := {owner_literal};\n\
             owner_oid  CONSTANT oid  := {owner_literal}::regrole;\n\
             item record;\n\
         BEGIN\n\
             FOR item IN\n\
                 SELECT n.nspname AS schema_name, c.relname AS object_name,\n\
                        CASE c.relkind\n\
                            WHEN 'S' THEN 'SEQUENCE'\n\
                            WHEN 'v' THEN 'VIEW'\n\
                            WHEN 'm' THEN 'MATERIALIZED VIEW'\n\
                            WHEN 'f' THEN 'FOREIGN TABLE'\n\
                            ELSE 'TABLE'\n\
                        END AS kind\n\
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace\n\
                 WHERE c.relkind IN ('r', 'p', 'S', 'v', 'm', 'f')\n\
                   AND c.relowner <> owner_oid\n\
                   AND n.nspname NOT LIKE 'pg\\_%'\n\
                   AND n.nspname <> 'information_schema'\n\
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d\n\
                                    WHERE d.classid = 'pg_class'::regclass\n\
                                      AND d.objid = c.oid AND d.deptype = 'e')\n\
                   AND NOT (c.relkind = 'S' AND EXISTS (\n\
                            SELECT 1 FROM pg_depend d\n\
                             WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid\n\
                               AND d.refclassid = 'pg_class'::regclass\n\
                               AND d.deptype IN ('a', 'i')))\n\
                 ORDER BY CASE c.relkind WHEN 'S' THEN 2 ELSE 1 END\n\
             LOOP\n\
                 EXECUTE format('ALTER %s %I.%I OWNER TO %I',\n\
                                item.kind, item.schema_name, item.object_name, owner_name);\n\
             END LOOP;\n\
             FOR item IN\n\
                 SELECT p.oid::regprocedure AS signature\n\
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace\n\
                 WHERE p.proowner <> owner_oid\n\
                   AND n.nspname NOT LIKE 'pg\\_%'\n\
                   AND n.nspname <> 'information_schema'\n\
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d\n\
                                    WHERE d.classid = 'pg_proc'::regclass\n\
                                      AND d.objid = p.oid AND d.deptype = 'e')\n\
             LOOP\n\
                 EXECUTE format('ALTER ROUTINE %s OWNER TO %I', item.signature, owner_name);\n\
             END LOOP;\n\
             FOR item IN\n\
                 SELECT t.oid::regtype AS type_name\n\
                 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace\n\
                 WHERE t.typowner <> owner_oid\n\
                   AND t.typtype IN ('c', 'd', 'e', 'r')\n\
                   AND n.nspname NOT LIKE 'pg\\_%'\n\
                   AND n.nspname <> 'information_schema'\n\
                   AND (t.typrelid = 0\n\
                        OR (SELECT c.relkind FROM pg_class c WHERE c.oid = t.typrelid) = 'c')\n\
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d\n\
                                    WHERE d.classid = 'pg_type'::regclass\n\
                                      AND d.objid = t.oid AND d.deptype IN ('e', 'i'))\n\
             LOOP\n\
                 EXECUTE format('ALTER TYPE %s OWNER TO %I', item.type_name, owner_name);\n\
             END LOOP;\n\
             FOR item IN\n\
                 SELECT nspname FROM pg_namespace\n\
                 WHERE nspowner <> owner_oid\n\
                   AND nspname NOT LIKE 'pg\\_%'\n\
                   AND nspname <> 'information_schema'\n\
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d\n\
                                    WHERE d.classid = 'pg_namespace'::regclass\n\
                                      AND d.objid = pg_namespace.oid AND d.deptype = 'e')\n\
             LOOP\n\
                 EXECUTE format('ALTER SCHEMA %I OWNER TO %I', item.nspname, owner_name);\n\
             END LOOP;\n\
         END\n\
         $linf$;\n\
         {schema}",
        owner_literal = quote_literal(owner),
        schema = schema_grants_sql(owner),
    )
}

/// DB-001/DB-002. Creates the login role, its database, and the least
/// privileges that make the pair usable and invisible to everyone else.
///
/// `CREATE DATABASE` cannot run inside a transaction block, so the statements
/// are streamed to `psql` in autocommit rather than wrapped in `BEGIN`. A
/// failure part-way therefore leaves objects behind on purpose — the caller
/// rolls back with [`drop_database_and_role`].
pub async fn create_database_and_role(
    x: &Executor,
    e: &EngineInstance,
    s: &DbSpec,
    password: &str,
) -> Result<()> {
    let bootstrap = format!(
        "{}\n{}\n{}\n",
        create_role_sql(&s.owner, password),
        create_database_sql(s),
        database_grants_sql(&s.database, &s.owner),
    );
    run_sql(
        x,
        e,
        MAINTENANCE_DB,
        &bootstrap,
        format!(
            "DB `{}`와 계정 `{}`을(를) 만들지 못했습니다",
            s.database, s.owner
        )
        .as_str(),
        "이름 중복 여부와 엔진 상태를 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    grant_schema(x, e, &s.database, &s.owner).await
}

async fn grant_schema(x: &Executor, e: &EngineInstance, database: &str, owner: &str) -> Result<()> {
    run_sql(
        x,
        e,
        database,
        &schema_grants_sql(owner),
        format!("DB `{database}`의 스키마 권한을 설정하지 못했습니다").as_str(),
        "엔진 관리자 계정 권한을 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

/// A login role with no database of its own yet — used by DB-010 duplication,
/// which creates the copy from a template instead of from scratch.
pub async fn create_role(
    x: &Executor,
    e: &EngineInstance,
    role: &str,
    password: &str,
) -> Result<()> {
    run_sql(
        x,
        e,
        MAINTENANCE_DB,
        &create_role_sql(role, password),
        format!("계정 `{role}`을(를) 만들지 못했습니다").as_str(),
        "같은 이름의 계정이 이미 있는지 확인하세요.",
    )
    .await?;
    Ok(())
}

/// An empty database owned by an existing role, e.g. when a restore target
/// vanished with its volume (BAK-005).
pub async fn create_database_owned_by(
    x: &Executor,
    e: &EngineInstance,
    database: &str,
    owner: &str,
) -> Result<()> {
    let spec = DbSpec {
        database: database.to_string(),
        owner: owner.to_string(),
        encoding: "UTF8".into(),
        locale: "C".into(),
    };
    let sql = format!(
        "{}\n{}\n",
        create_database_sql(&spec),
        database_grants_sql(database, owner)
    );
    run_sql(
        x,
        e,
        MAINTENANCE_DB,
        &sql,
        format!("DB `{database}`을(를) 만들지 못했습니다").as_str(),
        "같은 이름의 DB가 이미 있는지 확인하세요.",
    )
    .await?;
    grant_schema(x, e, database, owner).await
}

/// DB-010: `CREATE DATABASE … TEMPLATE` is a physical copy, so PostgreSQL
/// refuses it while anything else is connected to the source.
pub async fn create_database_from_template(
    x: &Executor,
    e: &EngineInstance,
    database: &str,
    owner: &str,
    template: &str,
) -> Result<()> {
    let sql = format!(
        "{}\n{}\n",
        create_database_from_template_sql(database, owner, template),
        database_grants_sql(database, owner),
    );
    run_sql(
        x,
        e,
        MAINTENANCE_DB,
        &sql,
        format!("DB `{template}`을(를) `{database}`(으)로 복제하지 못했습니다").as_str(),
        "원본 DB에 연결된 세션을 모두 종료한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

/// Make `owner` the owner of every user object in `database`, then hand the
/// `public` schema over as well. Used after a template copy and after a
/// restore, both of which land objects under the wrong owner.
pub async fn take_ownership(
    x: &Executor,
    e: &EngineInstance,
    database: &str,
    owner: &str,
) -> Result<()> {
    run_sql(
        x,
        e,
        database,
        &take_ownership_sql(owner),
        format!("DB `{database}`의 소유권을 `{owner}`(으)로 옮기지 못했습니다").as_str(),
        "관리자 계정 권한을 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

/// DB-008: open sessions are terminated first, otherwise `DROP DATABASE`
/// fails while an editor or a dev server still holds a connection.
fn drop_sql(database: &str, role: &str) -> String {
    format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = {db_literal} AND pid <> pg_backend_pid();\n\
         DROP DATABASE IF EXISTS {db};\n\
         DROP ROLE IF EXISTS {role};",
        db_literal = quote_literal(database),
        db = quote_ident(database),
        role = quote_ident(role),
    )
}

/// DB-008: drops exactly one project's database and its role, never the
/// engine and never another project's objects.
pub async fn drop_database_and_role(
    x: &Executor,
    e: &EngineInstance,
    database: &str,
    role: &str,
) -> Result<()> {
    let sql = drop_sql(database, role);
    run_sql(
        x,
        e,
        MAINTENANCE_DB,
        &sql,
        format!("DB `{database}`을(를) 삭제하지 못했습니다").as_str(),
        "해당 DB에 연결된 세션이 남아 있는지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

/// DB-009: rotate one project's password without touching its privileges.
pub async fn set_role_password(
    x: &Executor,
    e: &EngineInstance,
    role: &str,
    password: &str,
) -> Result<()> {
    let sql = format!(
        "ALTER ROLE {role} WITH PASSWORD {password};",
        role = quote_ident(role),
        password = quote_literal(password),
    );
    run_sql(
        x,
        e,
        MAINTENANCE_DB,
        &sql,
        format!("계정 `{role}`의 비밀번호를 변경하지 못했습니다").as_str(),
        "계정이 존재하는지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// The argv that logs in as `user` over TCP from inside the container.
///
/// It deliberately does **not** connect to `127.0.0.1`. The official image
/// ships `host all all 127.0.0.1/32 trust`, so a loopback connection succeeds
/// with any password whatsoever and would prove nothing. The container's own
/// address falls through to the `host all all all scram-sha-256` catch-all —
/// the same rule a client on the host meets through the published port — so
/// this is a real credential check (DB-005). `$(hostname)` is resolved by the
/// shell *inside* the container, which is why a shell is involved at all; the
/// password still travels only in the environment.
fn login_argv(db: &str, user: &str) -> Vec<String> {
    let tail: Vec<String> = vec![
        "-p".into(),
        CONTAINER_PORT.to_string(),
        "-U".into(),
        user.to_string(),
        "-d".into(),
        db.to_string(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
        "--no-psqlrc".into(),
        "-X".into(),
        "-q".into(),
        "-A".into(),
        "-t".into(),
        "-c".into(),
        "select 1".into(),
    ];
    vec![
        "sh".to_string(),
        "-c".into(),
        format!(
            "exec psql -h \"$(hostname)\" {}",
            crate::core::util::shell_join(&tail)
        ),
    ]
}

/// DB-005: prove the *project* credentials work, over TCP, exactly the way an
/// application will connect.
pub async fn verify_login(
    x: &Executor,
    e: &EngineInstance,
    db: &str,
    user: &str,
    password: &str,
) -> Result<()> {
    let argv = login_argv(db, user);
    let secrets = SecretEnv::new().set("PGPASSWORD", password)?;
    let out = docker::exec(x, &e.container_name, Some(OS_USER), &argv, &secrets, None).await?;
    if !out.ok() {
        let full = docker::exec_argv(
            x.docker_bin(),
            &e.container_name,
            Some(OS_USER),
            &secrets.names(),
            &argv,
        );
        return Err(Error::diagnostic(
            Diagnostic::new(
                format!("`{user}` 계정으로 `{db}`에 접속하지 못했습니다"),
                format!("psql이 종료 코드 {}(으)로 실패했습니다.", out.code),
                "비밀번호와 계정 권한을 확인한 뒤 `linf db test`로 다시 시도하세요.",
            )
            .with_command(x.describe(&full))
            .with_output(scrub(&out.message())),
        ));
    }
    if first_value(&out.stdout_str()) != "1" {
        return Err(Error::failed(
            format!("`{user}` 계정 접속 테스트 결과가 올바르지 않습니다"),
            "`select 1`이 기대한 값을 돌려주지 않았습니다.",
            "엔진 로그를 확인한 뒤 다시 시도하세요.",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Backup argv (pure)
// ---------------------------------------------------------------------------

/// Full argv, `docker exec` prefix included, writing the dump to stdout so the
/// caller can stream it straight into a local file (BAK-003).
pub fn dump_argv(
    docker_bin: &str,
    e: &EngineInstance,
    database: &str,
    f: BackupFormat,
) -> Vec<String> {
    let mut inner = vec![
        "pg_dump".to_string(),
        "-U".into(),
        e.admin_user.clone(),
        "-d".into(),
        database.to_string(),
    ];
    match f {
        BackupFormat::Custom => inner.push("-Fc".into()),
        // `Objects` is an object-storage archive; `bucket` owns that path, and
        // routing one here would silently produce an empty SQL dump.
        BackupFormat::Plain | BackupFormat::Objects => inner.push("--format=plain".into()),
    }
    docker::exec_argv(docker_bin, &e.container_name, Some(OS_USER), &[], &inner)
}

/// Full argv, `docker exec` prefix included, reading the dump from stdin.
///
/// A custom archive lands owned by the admin role (`--no-owner --role`),
/// because the dump's original owner need not exist on this engine; a plain
/// dump carries its own `ALTER … OWNER TO` statements. Either way the caller
/// normalises ownership afterwards with [`take_ownership`].
pub fn restore_argv(
    docker_bin: &str,
    e: &EngineInstance,
    database: &str,
    f: BackupFormat,
) -> Vec<String> {
    let inner = match f {
        BackupFormat::Custom => vec![
            "pg_restore".to_string(),
            "-U".into(),
            e.admin_user.clone(),
            "-d".into(),
            database.to_string(),
            "--no-owner".into(),
            format!("--role={}", e.admin_user),
        ],
        // Object archives belong to a MinIO engine; `bucket` owns that path.
        BackupFormat::Plain | BackupFormat::Objects => vec![
            "psql".to_string(),
            "-U".into(),
            e.admin_user.clone(),
            "-d".into(),
            database.to_string(),
            "-v".into(),
            "ON_ERROR_STOP=1".into(),
            "-f".into(),
            "-".into(),
        ],
    };
    docker::exec_argv(docker_bin, &e.container_name, Some(OS_USER), &[], &inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::EngineKind;
    use chrono::Utc;

    fn engine() -> EngineInstance {
        EngineInstance {
            id: "eng-1".into(),
            target_id: "tgt-1".into(),
            engine: EngineKind::Postgres,
            major_version: "17".into(),
            image: "postgres:17".into(),
            container_name: "linf-postgres-17".into(),
            volume_name: "linf-pg17-data".into(),
            bind_address: "127.0.0.1".into(),
            host_port: 5432,
            console_port: None,
            admin_user: "linf_admin".into(),
            credential_ref: "engine:eng-1".into(),
            managed: true,
            created_at: Utc::now(),
        }
    }

    fn spec() -> DbSpec {
        DbSpec {
            database: "letsbid_dev".into(),
            owner: "letsbid_user".into(),
            encoding: "UTF8".into(),
            locale: "C".into(),
        }
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_ident("letsbid_dev"), "\"letsbid_dev\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident("a\"\"b"), "\"a\"\"\"\"b\"");
        assert_eq!(quote_ident(""), "\"\"");
    }

    #[test]
    fn quotes_literals_with_embedded_quotes() {
        assert_eq!(quote_literal("pw"), "'pw'");
        assert_eq!(quote_literal("p'w"), "'p''w'");
        assert_eq!(quote_literal("''"), "''''''");
    }

    #[test]
    fn quotes_literals_with_backslashes_as_escape_strings() {
        // A backslash is only unambiguous inside an E'' string.
        assert_eq!(quote_literal("a\\b"), "E'a\\\\b'");
        assert_eq!(quote_literal("\\'"), "E'\\\\'''");
        // No backslash means no E prefix.
        assert!(!quote_literal("plain").starts_with('E'));
    }

    #[test]
    fn create_role_sql_is_least_privilege() {
        let sql = create_role_sql("letsbid_user", "s3cret");
        assert!(sql.contains("NOSUPERUSER"), "{sql}");
        assert!(sql.contains("NOCREATEDB"), "{sql}");
        assert!(sql.contains("NOCREATEROLE"), "{sql}");
        assert!(sql.contains("LOGIN"), "{sql}");
        assert!(sql.contains("PASSWORD 's3cret'"), "{sql}");
        assert!(sql.starts_with("CREATE ROLE \"letsbid_user\""), "{sql}");
    }

    #[test]
    fn create_role_sql_quotes_a_hostile_password() {
        let sql = create_role_sql("u", "it's\\bad");
        assert!(sql.contains("PASSWORD E'it''s\\\\bad'"), "{sql}");
    }

    #[test]
    fn create_database_sql_pins_template0_and_locale() {
        let sql = create_database_sql(&spec());
        assert!(sql.contains("CREATE DATABASE \"letsbid_dev\""), "{sql}");
        assert!(sql.contains("OWNER \"letsbid_user\""), "{sql}");
        assert!(sql.contains("ENCODING 'UTF8'"), "{sql}");
        assert!(sql.contains("LC_COLLATE 'C'"), "{sql}");
        assert!(sql.contains("LC_CTYPE 'C'"), "{sql}");
        assert!(sql.contains("TEMPLATE template0"), "{sql}");
    }

    #[test]
    fn privilege_sql_revokes_from_public() {
        let db = database_grants_sql("letsbid_dev", "letsbid_user");
        assert!(
            db.contains("REVOKE ALL ON DATABASE \"letsbid_dev\" FROM PUBLIC"),
            "{db}"
        );
        assert!(
            db.contains("GRANT ALL ON DATABASE \"letsbid_dev\" TO \"letsbid_user\""),
            "{db}"
        );

        let schema = schema_grants_sql("letsbid_user");
        assert!(
            schema.contains("REVOKE ALL ON SCHEMA public FROM PUBLIC"),
            "{schema}"
        );
        assert!(
            schema.contains("GRANT ALL ON SCHEMA public TO \"letsbid_user\""),
            "{schema}"
        );
        assert!(
            schema.contains("ALTER SCHEMA public OWNER TO \"letsbid_user\""),
            "{schema}"
        );
    }

    #[test]
    fn drop_terminates_backends_before_dropping() {
        let sql = drop_sql("letsbid_dev", "letsbid_user");
        let terminate = sql
            .find("pg_terminate_backend")
            .expect("terminates backends");
        let drop_db = sql.find("DROP DATABASE").expect("drops the database");
        let drop_role = sql.find("DROP ROLE").expect("drops the role");
        assert!(terminate < drop_db, "{sql}");
        assert!(drop_db < drop_role, "{sql}");
        assert!(sql.contains("datname = 'letsbid_dev'"), "{sql}");
        assert!(sql.contains("pid <> pg_backend_pid()"), "{sql}");
        assert!(
            sql.contains("DROP DATABASE IF EXISTS \"letsbid_dev\""),
            "{sql}"
        );
        assert!(
            sql.contains("DROP ROLE IF EXISTS \"letsbid_user\""),
            "{sql}"
        );
    }

    #[test]
    fn template_copy_sql_grants_the_new_owner() {
        let sql = create_database_from_template_sql("copy_dev", "copy_user", "letsbid_dev");
        assert_eq!(
            sql,
            "CREATE DATABASE \"copy_dev\" WITH OWNER \"copy_user\" TEMPLATE \"letsbid_dev\";"
        );
    }

    #[test]
    fn take_ownership_sql_rehomes_only_user_objects() {
        let sql = take_ownership_sql("letsbid_user");
        // `REASSIGN OWNED BY` is refused for a role that owns system objects,
        // and filtering by source owner misses plain dumps entirely.
        assert!(!sql.contains("REASSIGN OWNED"), "{sql}");
        assert!(
            sql.contains("owner_name CONSTANT text := 'letsbid_user'"),
            "{sql}"
        );
        assert!(sql.contains("'letsbid_user'::regrole"), "{sql}");
        for guard in [
            "ALTER %s %I.%I OWNER TO %I",
            "ALTER ROUTINE %s OWNER TO %I",
            "ALTER TYPE %s OWNER TO %I",
            "ALTER SCHEMA %I OWNER TO %I",
        ] {
            assert!(sql.contains(guard), "missing `{guard}` in:\n{sql}");
        }
        // System catalogs, extension members and internal sequences are
        // never touched.
        assert_eq!(
            sql.matches("n.nspname NOT LIKE 'pg\\_%'").count(),
            3,
            "{sql}"
        );
        assert_eq!(
            sql.matches("nspname <> 'information_schema'").count(),
            4,
            "{sql}"
        );
        assert_eq!(sql.matches("deptype = 'e'").count(), 3, "{sql}");
        assert_eq!(sql.matches("deptype IN ('e', 'i')").count(), 1, "{sql}");
        // A serial/identity sequence follows its table and must be skipped.
        assert_eq!(sql.matches("deptype IN ('a', 'i')").count(), 1, "{sql}");
        assert!(
            sql.contains("ORDER BY CASE c.relkind WHEN 'S' THEN 2 ELSE 1 END"),
            "{sql}"
        );
        // The schema hand-over still runs afterwards.
        assert!(
            sql.contains("ALTER SCHEMA public OWNER TO \"letsbid_user\""),
            "{sql}"
        );
        assert!(
            sql.contains("REVOKE ALL ON SCHEMA public FROM PUBLIC"),
            "{sql}"
        );
    }

    #[test]
    fn admin_sql_argv_reads_the_statement_from_stdin() {
        let argv = psql_argv(&engine(), "postgres", &["-f", "-"]);
        assert_eq!(
            argv,
            vec![
                "psql",
                "-U",
                "linf_admin",
                "-d",
                "postgres",
                "-v",
                "ON_ERROR_STOP=1",
                "--no-psqlrc",
                "-X",
                "-q",
                "-A",
                "-t",
                "-f",
                "-",
            ]
        );
    }

    #[test]
    fn login_argv_avoids_the_trusted_loopback_rule() {
        let argv = login_argv("letsbid_dev", "letsbid_user");
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        let script = &argv[2];
        // `127.0.0.1` is `trust` in the official image: any password passes.
        assert!(!script.contains("127.0.0.1"), "{script}");
        assert!(script.contains("-h \"$(hostname)\""), "{script}");
        assert!(script.starts_with("exec psql "), "{script}");
        assert!(script.contains("-U letsbid_user"), "{script}");
        assert!(script.contains("-d letsbid_dev"), "{script}");
        assert!(script.contains("-p 5432"), "{script}");
        assert!(script.contains("'select 1'"), "{script}");
    }

    #[test]
    fn login_argv_quotes_a_hostile_identifier() {
        let script = login_argv("db$1", "u ser").pop().unwrap();
        assert!(script.contains("-d 'db$1'"), "{script}");
        assert!(script.contains("-U 'u ser'"), "{script}");
    }

    #[test]
    fn dump_argv_custom_format() {
        let argv = dump_argv("docker", &engine(), "letsbid_dev", BackupFormat::Custom);
        assert_eq!(
            argv,
            vec![
                "docker",
                "exec",
                "--interactive",
                "--user",
                "postgres",
                "linf-postgres-17",
                "pg_dump",
                "-U",
                "linf_admin",
                "-d",
                "letsbid_dev",
                "-Fc",
            ]
        );
    }

    #[test]
    fn dump_argv_plain_format() {
        let argv = dump_argv("docker", &engine(), "letsbid_dev", BackupFormat::Plain);
        assert_eq!(argv.last().unwrap(), "--format=plain");
        assert!(argv.contains(&"pg_dump".to_string()));
    }

    #[test]
    fn restore_argv_custom_format() {
        let argv = restore_argv("docker", &engine(), "letsbid_dev", BackupFormat::Custom);
        assert_eq!(
            argv,
            vec![
                "docker",
                "exec",
                "--interactive",
                "--user",
                "postgres",
                "linf-postgres-17",
                "pg_restore",
                "-U",
                "linf_admin",
                "-d",
                "letsbid_dev",
                "--no-owner",
                "--role=linf_admin",
            ]
        );
    }

    #[test]
    fn restore_argv_plain_format_reads_stdin() {
        let argv = restore_argv(
            "/usr/bin/docker",
            &engine(),
            "letsbid_dev",
            BackupFormat::Plain,
        );
        assert_eq!(argv[0], "/usr/bin/docker");
        let tail = &argv[argv.len() - 9..];
        assert_eq!(
            tail,
            [
                "psql",
                "-U",
                "linf_admin",
                "-d",
                "letsbid_dev",
                "-v",
                "ON_ERROR_STOP=1",
                "-f",
                "-",
            ]
        );
    }

    #[test]
    fn no_argv_ever_carries_a_password() {
        let secret = "hunter2-Sup3r";
        let e = engine();
        let mut argvs: Vec<Vec<String>> = vec![
            dump_argv("docker", &e, "letsbid_dev", BackupFormat::Custom),
            dump_argv("docker", &e, "letsbid_dev", BackupFormat::Plain),
            restore_argv("docker", &e, "letsbid_dev", BackupFormat::Custom),
            restore_argv("docker", &e, "letsbid_dev", BackupFormat::Plain),
            psql_argv(&e, "postgres", &["-f", "-"]),
        ];
        // The verify-login argv is the only one that needs the password at all,
        // and it must carry the *name* only.
        let names = SecretEnv::new()
            .set("PGPASSWORD", secret)
            .unwrap()
            .names()
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["PGPASSWORD".to_string()]);
        argvs.push(docker::exec_argv(
            "docker",
            &e.container_name,
            Some(OS_USER),
            &["PGPASSWORD"],
            &["psql".to_string(), "-c".into(), "select 1".into()],
        ));
        for argv in argvs {
            for token in &argv {
                assert!(
                    !token.contains(secret),
                    "password leaked into argv: {token}"
                );
            }
        }
        // And the SQL that does carry it never becomes an argument.
        let sql = create_role_sql("u", secret);
        assert!(sql.contains(secret));
        assert!(!psql_argv(&e, "postgres", &["-f", "-"]).contains(&sql));
    }

    #[test]
    fn scrub_masks_password_clauses_in_server_output() {
        let raw = "psql:<stdin>:1: ERROR: role already exists\n\
                   STATEMENT: CREATE ROLE \"u\" WITH LOGIN PASSWORD 'hunter2';";
        let out = scrub(raw);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("PASSWORD '****'"), "{out}");
        assert!(out.contains("role already exists"), "{out}");
    }

    #[test]
    fn scrub_is_a_no_op_without_secrets() {
        let raw = "ERROR:  database \"letsbid_dev\" already exists";
        assert_eq!(scrub(raw), raw);
    }

    #[test]
    fn first_value_takes_the_first_non_empty_line() {
        assert_eq!(first_value("\n\n 42 \n7\n"), "42");
        assert_eq!(first_value(""), "");
    }
}
