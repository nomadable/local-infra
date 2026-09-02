//! MinIO adapter (PRD §6.3, §8.4).
//!
//! Every administrative and data operation runs `mc` *inside* the engine
//! container, so neither the workstation nor the VPS needs an S3 client
//! installed — the same "no host client dependency" rule [`crate::core::pg`]
//! follows for PostgreSQL.
//!
//! Three invariants:
//!
//! 1. **Administrative credentials travel in one environment variable.**
//!    `MC_HOST_linf` is built by [`engine::minio_admin_env`] and passed through
//!    [`SecretEnv`], which means docker gets the value-less
//!    `--env MC_HOST_linf` form and the root password never reaches an argv,
//!    locally or over ssh.
//! 2. **A new access key or secret key travels on stdin.** Those are values,
//!    not environment variables, so a shell preamble `read`s them into shell
//!    variables before `mc` runs — see [`mc_with_stdin_values`].
//! 3. **The image ships no `tar`.** A bucket archive is therefore streamed one
//!    object at a time; [`cat_argv`] and [`pipe_argv`] are pure so
//!    [`crate::core::bucket`] can hand them straight to
//!    [`Executor::stream_out`] and [`Executor::stream_in`].

use crate::core::docker;
use crate::core::engine::{self, MINIO_ALIAS};
use crate::core::error::{Diagnostic, Error, Result};
use crate::core::exec::{Executor, Output, SecretEnv};
use crate::core::model::{BucketStats, EngineInstance};
use crate::core::util;
use rand::Rng;
use serde::{Deserialize, Serialize};

/// Alias for a temporary `mc` host entry built from *project* credentials. It
/// must differ from [`MINIO_ALIAS`] so a verification can never borrow
/// administrative rights by accident.
const PROJECT_ALIAS: &str = "linfproj";

const BUCKET_MIN_CHARS: usize = 3;
const BUCKET_MAX_CHARS: usize = 63;
const ACCESS_KEY_MIN_CHARS: usize = 5;
const ACCESS_KEY_MAX_CHARS: usize = 128;

/// AWS access key ids are 20 uppercase alphanumerics; generated keys match
/// that shape so they look ordinary to every SDK and console.
const ACCESS_KEY_LEN: usize = 20;
const ACCESS_KEY_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Bucket names are DNS labels: 3–63 characters, lowercase alphanumeric and
/// `-`, starting and ending alphanumeric, and never an IP address.
///
/// Dots are rejected outright rather than merely forbidden in pairs: a dotted
/// bucket name breaks virtual-host style addressing under TLS and buys nothing
/// here, and rejecting them makes "no consecutive dots" true by construction.
pub fn validate_bucket_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Usage("버킷명을 입력하세요.".into()));
    }
    if name.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(Error::Usage(format!(
            "버킷명은 IP 주소 형태일 수 없습니다: `{name}`"
        )));
    }
    if name.contains('.') {
        return Err(Error::Usage(format!(
            "버킷명에는 점(`.`)을 사용할 수 없습니다. `-`로 구분하세요: `{name}`"
        )));
    }
    let chars = name.chars().count();
    if chars < BUCKET_MIN_CHARS {
        return Err(Error::Usage(format!(
            "버킷명은 {BUCKET_MIN_CHARS}자 이상이어야 합니다 (현재 {chars}자): `{name}`"
        )));
    }
    if chars > BUCKET_MAX_CHARS {
        return Err(Error::Usage(format!(
            "버킷명은 {BUCKET_MAX_CHARS}자를 넘을 수 없습니다 (현재 {chars}자)."
        )));
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(Error::Usage(format!(
                "버킷명에는 소문자, 숫자, `-`만 사용할 수 있습니다: `{name}`"
            )));
        }
    }
    let first = name.chars().next().expect("checked non-empty");
    let last = name.chars().next_back().expect("checked non-empty");
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return Err(Error::Usage(format!(
            "버킷명은 소문자 또는 숫자로 시작하고 끝나야 합니다: `{name}`"
        )));
    }
    Ok(())
}

/// Access keys are 5–128 characters. The charset is narrowed to what survives
/// a URL userinfo component and a shell variable unescaped, because the key
/// ends up in both.
pub fn validate_access_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::Usage("액세스 키를 입력하세요.".into()));
    }
    let chars = key.chars().count();
    if chars < ACCESS_KEY_MIN_CHARS {
        return Err(Error::Usage(format!(
            "액세스 키는 {ACCESS_KEY_MIN_CHARS}자 이상이어야 합니다 (현재 {chars}자)."
        )));
    }
    if chars > ACCESS_KEY_MAX_CHARS {
        return Err(Error::Usage(format!(
            "액세스 키는 {ACCESS_KEY_MAX_CHARS}자를 넘을 수 없습니다 (현재 {chars}자)."
        )));
    }
    for c in key.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
            return Err(Error::Usage(format!(
                "액세스 키에는 영문자, 숫자, `-`, `_`, `.`만 사용할 수 있습니다: `{key}`"
            )));
        }
    }
    Ok(())
}

pub fn generate_access_key() -> String {
    let mut rng = rand::thread_rng();
    (0..ACCESS_KEY_LEN)
        .map(|_| ACCESS_KEY_ALPHABET[rng.gen_range(0..ACCESS_KEY_ALPHABET.len())] as char)
        .collect()
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Least privilege, and the whole point of a per-project user: the document
/// names one bucket and nothing else, so a leaked project key cannot reach
/// another project's objects.
pub fn bucket_policy(bucket: &str) -> String {
    let bucket_arn = format!("arn:aws:s3:::{bucket}");
    let object_arn = format!("{bucket_arn}/*");
    let document = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": [
                    "s3:GetBucketLocation",
                    "s3:ListBucket",
                    "s3:ListBucketMultipartUploads"
                ],
                "Resource": [bucket_arn]
            },
            {
                "Effect": "Allow",
                "Action": ["s3:*"],
                "Resource": [object_arn]
            }
        ]
    });
    // A `serde_json::Value` built from strings and arrays cannot fail to
    // serialise: there is no map key that is not a string and no non-finite
    // number.
    serde_json::to_string_pretty(&document).expect("policy document serialises")
}

/// `linf-<bucket>`. Prefixed so a policy this app owns is recognisable next to
/// MinIO's built-in `readwrite`/`readonly` policies.
pub fn policy_name(bucket: &str) -> String {
    format!("linf-{bucket}")
}

// ---------------------------------------------------------------------------
// Running mc
// ---------------------------------------------------------------------------

/// Name of the single variable carrying administrative credentials.
fn admin_env_name() -> String {
    format!("MC_HOST_{MINIO_ALIAS}")
}

fn target_path(bucket: &str, key: Option<&str>) -> String {
    match key {
        Some(k) => format!("{MINIO_ALIAS}/{bucket}/{k}"),
        None => format!("{MINIO_ALIAS}/{bucket}"),
    }
}

fn mc_argv(args: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push("mc".to_string());
    argv.extend(args.iter().cloned());
    argv
}

/// Run `mc <args>` inside the engine container. The exit code is *not*
/// checked: several callers read a non-zero exit as an answer rather than a
/// failure.
pub async fn mc(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    args: &[String],
) -> Result<Output> {
    let secrets = engine::minio_admin_env(e, admin_password)?;
    let argv = mc_argv(args);
    docker::exec(x, &e.container_name, None, &argv, &secrets, None).await
}

/// Same as [`mc`], with `stdin` handed to the command. Used for
/// `mc admin policy create … /dev/stdin`, which is how a policy document
/// reaches `mc` without ever being written to a file.
async fn mc_stdin(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    args: &[String],
    stdin: &[u8],
) -> Result<Output> {
    let secrets = engine::minio_admin_env(e, admin_password)?;
    let argv = mc_argv(args);
    docker::exec(x, &e.container_name, None, &argv, &secrets, Some(stdin)).await
}

/// [`mc`] plus the three-part diagnostic every failure owes the user.
async fn mc_checked(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    args: &[String],
    what: &str,
    next: &str,
) -> Result<Output> {
    let out = mc(x, e, admin_password, args).await?;
    if !out.ok() {
        let secrets = engine::minio_admin_env(e, admin_password)?;
        return Err(failure(x, e, &secrets, &mc_argv(args), &out, what, next));
    }
    Ok(out)
}

/// The `sh -c` argv that reads one value per line from stdin into `var_names`
/// and then runs `script`. Pure, so a test can prove no secret is in it.
fn stdin_values_argv(var_names: &[&str], script: &str) -> Vec<String> {
    let reads = var_names
        .iter()
        .map(|n| format!("IFS= read -r {n}; "))
        .collect::<String>();
    vec!["sh".to_string(), "-c".into(), format!("{reads}{script}")]
}

/// `value1\nvalue2\n`, consumed by the preamble of [`stdin_values_argv`] in
/// order.
fn stdin_values_payload(values: &[&str]) -> Vec<u8> {
    let mut buf = Vec::new();
    for v in values {
        buf.extend_from_slice(v.as_bytes());
        buf.push(b'\n');
    }
    buf
}

/// Run `script` inside the engine container with `stdin_values` read into
/// `var_names` first, so a value that must not appear in any argv can still be
/// an argument to `mc`.
///
/// Ordering holds for both transports: [`Executor`] prepends its own
/// [`SecretEnv`] lines to stdin for ssh targets, in declaration order, ahead of
/// these bytes.
pub async fn mc_with_stdin_values(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    var_names: &[&str],
    stdin_values: &[&str],
    script: &str,
) -> Result<Output> {
    if var_names.len() != stdin_values.len() {
        return Err(Error::Usage(format!(
            "stdin 변수 {}개에 값 {}개가 전달되었습니다.",
            var_names.len(),
            stdin_values.len()
        )));
    }
    for name in var_names {
        let legal = !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !legal {
            return Err(Error::Usage(format!(
                "`{name}`은(는) 셸 변수 이름으로 사용할 수 없습니다."
            )));
        }
    }
    for value in stdin_values {
        if value.contains('\n') || value.contains('\r') {
            // A newline would desynchronise the line-delimited preamble, and
            // every later value would be read wrong.
            return Err(Error::Usage(
                "stdin으로 전달하는 값에는 줄바꿈을 포함할 수 없습니다.".into(),
            ));
        }
    }
    let secrets = engine::minio_admin_env(e, admin_password)?;
    let argv = stdin_values_argv(var_names, script);
    let payload = stdin_values_payload(stdin_values);
    docker::exec(x, &e.container_name, None, &argv, &secrets, Some(&payload)).await
}

fn failure(
    x: &Executor,
    e: &EngineInstance,
    secrets: &SecretEnv,
    argv: &[String],
    out: &Output,
    what: &str,
    next: &str,
) -> Error {
    let names = secrets.names();
    let full = docker::exec_argv(x.docker_bin(), &e.container_name, None, &names, argv);
    Error::diagnostic(
        Diagnostic::new(
            what.to_string(),
            format!("mc가 종료 코드 {}(으)로 실패했습니다.", out.code),
            next.to_string(),
        )
        .with_command(x.describe(&full))
        .with_output(scrub(&out.message())),
    )
}

/// Belt and braces on top of [`util::redact`]: `mc --json admin user add`
/// echoes back the secret key it has just set, and a chained call that fails
/// *after* that line would otherwise carry the credential into a diagnostic
/// and the activity log. Any JSON member whose name contains `secret` has its
/// string value masked.
fn scrub(text: &str) -> String {
    let redacted = util::redact(text);
    let lower = redacted.to_ascii_lowercase();
    let bytes = redacted.as_bytes();
    let mut out = String::with_capacity(redacted.len());
    let mut cursor = 0usize;
    // Every byte compared below is ASCII, so no index can land inside a
    // multi-byte character.
    while let Some(rel) = lower[cursor..].find("secret") {
        let keyword_end = cursor + rel + "secret".len();
        // Rest of the member name, then `"`, `:`, and the opening quote of the
        // value. Anything else means this was prose, not a JSON member.
        let mut i = keyword_end;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let value_start = quoted_value_start(bytes, i);
        match value_start {
            Some(open) => {
                let mut end = open + 1;
                while end < bytes.len() {
                    match bytes[end] {
                        b'\\' => end += 2,
                        b'"' => break,
                        _ => end += 1,
                    }
                }
                out.push_str(&redacted[cursor..=open]);
                out.push_str("****");
                cursor = end.min(redacted.len());
            }
            None => {
                out.push_str(&redacted[cursor..keyword_end]);
                cursor = keyword_end;
            }
        }
    }
    out.push_str(&redacted[cursor..]);
    out
}

/// Index of the opening quote of `"…": "value"`, starting at the closing quote
/// of the member name.
fn quoted_value_start(bytes: &[u8], mut i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    while matches!(bytes.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while matches!(bytes.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    (bytes.get(i) == Some(&b'"')).then_some(i)
}

// ---------------------------------------------------------------------------
// JSON line parsing
// ---------------------------------------------------------------------------

/// `mc --json` emits one JSON object per line. Anything unparseable or not
/// marked `success` is skipped rather than failing the call: a warning line
/// must not look like data.
fn success_lines(text: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| match v.get("status").and_then(|s| s.as_str()) {
            Some(status) => status == "success",
            None => true,
        })
}

/// Numbers occasionally arrive as strings from older `mc` builds.
fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

fn json_string(value: Option<&serde_json::Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Bucket listing: `mc --json ls <alias>` reports every bucket as a folder
/// whose `key` carries a trailing slash.
fn parse_bucket_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = success_lines(text)
        .filter_map(|v| json_string(v.get("key")))
        .map(|k| k.trim_matches('/').to_string())
        .filter(|k| !k.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn parse_user_keys(text: &str) -> Vec<String> {
    success_lines(text)
        .filter_map(|v| json_string(v.get("accessKey")))
        .collect()
}

fn parse_policy_names(text: &str) -> Vec<String> {
    success_lines(text)
        .filter_map(|v| json_string(v.get("policy")))
        .collect()
}

/// `mc --json du <alias>/<bucket>` prints one line per scanned prefix and the
/// total last, so the last line wins.
fn parse_du_json(text: &str) -> BucketStats {
    let mut stats = BucketStats::default();
    for value in success_lines(text) {
        let size = json_u64(value.get("size"));
        let objects = json_u64(value.get("objects"));
        if size.is_some() || objects.is_some() {
            stats = BucketStats {
                objects,
                size_bytes: size,
            };
        }
    }
    stats
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

pub async fn list_bucket_names(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
) -> Result<Vec<String>> {
    let args = vec!["--json".to_string(), "ls".into(), MINIO_ALIAS.to_string()];
    let out = mc_checked(
        x,
        e,
        admin_password,
        &args,
        "버킷 목록을 읽지 못했습니다",
        "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(parse_bucket_names(&out.stdout_str()))
}

/// Asked by listing rather than by `mc stat`, so an existing-but-empty bucket
/// and a missing one are never confused by an exit code.
pub async fn bucket_exists(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    bucket: &str,
) -> Result<bool> {
    Ok(list_bucket_names(x, e, admin_password)
        .await?
        .iter()
        .any(|b| b == bucket))
}

pub async fn user_exists(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    access_key: &str,
) -> Result<bool> {
    let args = vec![
        "--json".to_string(),
        "admin".into(),
        "user".into(),
        "list".into(),
        MINIO_ALIAS.to_string(),
    ];
    let out = mc_checked(
        x,
        e,
        admin_password,
        &args,
        "액세스 키 목록을 읽지 못했습니다",
        "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(parse_user_keys(&out.stdout_str())
        .iter()
        .any(|k| k == access_key))
}

async fn policy_exists(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    policy: &str,
) -> Result<bool> {
    let args = vec![
        "--json".to_string(),
        "admin".into(),
        "policy".into(),
        "list".into(),
        MINIO_ALIAS.to_string(),
    ];
    let out = mc_checked(
        x,
        e,
        admin_password,
        &args,
        "정책 목록을 읽지 못했습니다",
        "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(parse_policy_names(&out.stdout_str())
        .iter()
        .any(|p| p == policy))
}

/// `None` for either field whenever the value cannot be read — statistics must
/// never break a list view (PRD §7.6).
pub async fn bucket_usage(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    bucket: &str,
) -> Result<BucketStats> {
    let args = vec!["--json".to_string(), "du".into(), target_path(bucket, None)];
    let out = mc(x, e, admin_password, &args).await?;
    if !out.ok() {
        return Ok(BucketStats::default());
    }
    Ok(parse_du_json(&out.stdout_str()))
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

pub async fn create_bucket(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    bucket: &str,
) -> Result<()> {
    validate_bucket_name(bucket)?;
    let args = vec!["--json".to_string(), "mb".into(), target_path(bucket, None)];
    mc_checked(
        x,
        e,
        admin_password,
        &args,
        &format!("버킷 `{bucket}` 생성에 실패했습니다"),
        "이름이 이미 사용 중인지, 엔진에 여유 공간이 있는지 확인하세요.",
    )
    .await?;
    Ok(())
}

/// `mc rb --force` removes the bucket together with its objects: a bucket with
/// content cannot be dropped any other way.
pub async fn remove_bucket(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    bucket: &str,
) -> Result<()> {
    let args = vec![
        "--json".to_string(),
        "rb".into(),
        "--force".into(),
        target_path(bucket, None),
    ];
    mc_checked(
        x,
        e,
        admin_password,
        &args,
        &format!("버킷 `{bucket}` 삭제에 실패했습니다"),
        "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    Ok(())
}

/// `mc admin user add` + `mc admin policy attach`, with both keys read from
/// stdin. Pure so the script can be asserted to carry no credential.
fn create_user_script(policy: &str) -> String {
    format!(
        "mc --json admin user add {alias} \"$AK\" \"$SK\" && \
         mc --json admin policy attach {alias} {policy} --user \"$AK\"",
        alias = util::shell_quote(MINIO_ALIAS),
        policy = util::shell_quote(policy),
    )
}

/// The project's user, its bucket-scoped policy, and the attachment between
/// them (the object-storage counterpart of DB-002).
///
/// The policy document goes to `mc` on stdin as `/dev/stdin`, and the two keys
/// go on stdin behind a `read` preamble, so neither a policy nor a credential
/// is ever visible in `ps(1)`.
pub async fn create_scoped_user(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<()> {
    validate_bucket_name(bucket)?;
    validate_access_key(access_key)?;
    let policy = policy_name(bucket);

    let args = vec![
        "--json".to_string(),
        "admin".into(),
        "policy".into(),
        "create".into(),
        MINIO_ALIAS.to_string(),
        policy.clone(),
        "/dev/stdin".into(),
    ];
    let document = bucket_policy(bucket);
    let out = mc_stdin(x, e, admin_password, &args, document.as_bytes()).await?;
    if !out.ok() {
        let secrets = engine::minio_admin_env(e, admin_password)?;
        return Err(failure(
            x,
            e,
            &secrets,
            &mc_argv(&args),
            &out,
            &format!("`{bucket}` 전용 정책 생성에 실패했습니다"),
            "같은 이름의 정책이 이미 있는지 확인한 뒤 다시 시도하세요.",
        ));
    }

    let script = create_user_script(&policy);
    let out = mc_with_stdin_values(
        x,
        e,
        admin_password,
        &["AK", "SK"],
        &[access_key, secret_key],
        &script,
    )
    .await?;
    if !out.ok() {
        let secrets = engine::minio_admin_env(e, admin_password)?;
        return Err(failure(
            x,
            e,
            &secrets,
            &stdin_values_argv(&["AK", "SK"], &script),
            &out,
            format!("액세스 키 `{access_key}` 생성에 실패했습니다").as_str(),
            "키가 이미 존재하는지 확인한 뒤 다른 액세스 키로 다시 시도하세요.",
        ));
    }
    Ok(())
}

/// Rotation (DB-009 analogue). `mc admin user add` on an existing access key
/// replaces its secret and leaves the policy attachment alone, which is why
/// there is no separate "set secret" command to call.
pub async fn set_user_secret(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<()> {
    validate_access_key(access_key)?;
    let script = format!(
        "mc --json admin user add {alias} \"$AK\" \"$SK\"",
        alias = util::shell_quote(MINIO_ALIAS)
    );
    let out = mc_with_stdin_values(
        x,
        e,
        admin_password,
        &["AK", "SK"],
        &[access_key, secret_key],
        &script,
    )
    .await?;
    if !out.ok() {
        let secrets = engine::minio_admin_env(e, admin_password)?;
        return Err(failure(
            x,
            e,
            &secrets,
            &stdin_values_argv(&["AK", "SK"], &script),
            &out,
            format!("액세스 키 `{access_key}`의 시크릿 교체에 실패했습니다").as_str(),
            "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
        ));
    }
    Ok(())
}

/// Removes the project's user and its policy. Absence is success: this is the
/// rollback path for a half-finished creation, so it must not fail merely
/// because the attempt never got that far.
pub async fn remove_scoped_user(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    bucket: &str,
    access_key: &str,
) -> Result<()> {
    if user_exists(x, e, admin_password, access_key).await? {
        let script = format!(
            "mc --json admin user remove {alias} \"$AK\"",
            alias = util::shell_quote(MINIO_ALIAS)
        );
        let out =
            mc_with_stdin_values(x, e, admin_password, &["AK"], &[access_key], &script).await?;
        if !out.ok() {
            let secrets = engine::minio_admin_env(e, admin_password)?;
            return Err(failure(
                x,
                e,
                &secrets,
                &stdin_values_argv(&["AK"], &script),
                &out,
                format!("액세스 키 `{access_key}` 삭제에 실패했습니다").as_str(),
                "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
            ));
        }
    }

    let policy = policy_name(bucket);
    if policy_exists(x, e, admin_password, &policy).await? {
        let args = vec![
            "--json".to_string(),
            "admin".into(),
            "policy".into(),
            "rm".into(),
            MINIO_ALIAS.to_string(),
            policy.clone(),
        ];
        mc_checked(
            x,
            e,
            admin_password,
            &args,
            &format!("정책 `{policy}` 삭제에 실패했습니다"),
            "엔진이 실행 중인지 확인한 뒤 다시 시도하세요.",
        )
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Credentials for a temporary, project-scoped `mc` alias.
fn project_env(e: &EngineInstance, access_key: &str, secret_key: &str) -> Result<SecretEnv> {
    SecretEnv::new().set(
        format!("MC_HOST_{PROJECT_ALIAS}"),
        format!(
            "http://{}:{}@127.0.0.1:{}",
            util::pct_encode(access_key),
            util::pct_encode(secret_key),
            e.engine.container_port()
        ),
    )
}

/// The inner argv of a verification. Pure, so a test can prove the credentials
/// are not in it.
fn verify_argv(bucket: &str) -> Vec<String> {
    vec![
        "mc".to_string(),
        "--json".into(),
        "ls".into(),
        format!("{PROJECT_ALIAS}/{bucket}"),
    ]
}

/// DB-005 for object storage: prove the *project* key works against its own
/// bucket, the way an application will use it. The alias exists only for the
/// duration of the call, as an environment variable.
pub async fn verify_access(
    x: &Executor,
    e: &EngineInstance,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<()> {
    let secrets = project_env(e, access_key, secret_key)?;
    let argv = verify_argv(bucket);
    let out = docker::exec(x, &e.container_name, None, &argv, &secrets, None).await?;
    if !out.ok() {
        return Err(failure(
            x,
            e,
            &secrets,
            &argv,
            &out,
            format!("액세스 키 `{access_key}`(으)로 버킷 `{bucket}`에 접근하지 못했습니다")
                .as_str(),
            "키와 정책이 올바른지 확인한 뒤 `linf bucket test`로 다시 시도하세요.",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

/// One entry of a bucket archive manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEntry {
    pub key: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// Parse `mc ls --recursive --json` output.
///
/// `key` is relative to the listed bucket, which is exactly what
/// [`cat_argv`] and [`pipe_argv`] want. Folder entries carry `type: "folder"`
/// and a trailing slash; they are prefixes, not objects, and are skipped —
/// restoring one would create a stray empty object.
pub fn parse_ls_json(text: &str) -> Vec<ObjectEntry> {
    success_lines(text)
        .filter(|v| v.get("type").and_then(|t| t.as_str()) != Some("folder"))
        .filter_map(|v| {
            let key = json_string(v.get("key"))?;
            let key = key.trim_start_matches('/').to_string();
            if key.is_empty() {
                return None;
            }
            Some(ObjectEntry {
                key,
                size: json_u64(v.get("size")).unwrap_or(0),
                etag: json_string(v.get("etag")),
                content_type: json_string(v.get("contentType")),
            })
        })
        .collect()
}

/// Every object in the bucket, sorted by key so an archive of unchanged
/// content is byte-identical twice running.
pub async fn list_objects(
    x: &Executor,
    e: &EngineInstance,
    admin_password: &str,
    bucket: &str,
) -> Result<Vec<ObjectEntry>> {
    let args = vec![
        "--json".to_string(),
        "ls".into(),
        "--recursive".into(),
        target_path(bucket, None),
    ];
    let out = mc_checked(
        x,
        e,
        admin_password,
        &args,
        &format!("버킷 `{bucket}`의 객체 목록을 읽지 못했습니다"),
        "엔진이 실행 중인지, 버킷이 존재하는지 확인한 뒤 다시 시도하세요.",
    )
    .await?;
    let mut objects = parse_ls_json(&out.stdout_str());
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(objects)
}

/// Full argv, `docker exec` prefix included, writing one object to stdout so
/// the caller can stream it straight into the archive file. Administrative
/// credentials arrive through the value-less `--env MC_HOST_linf` form, so the
/// caller must pass [`engine::minio_admin_env`] as the [`SecretEnv`].
pub fn cat_argv(docker_bin: &str, e: &EngineInstance, bucket: &str, key: &str) -> Vec<String> {
    let inner = vec![
        "mc".to_string(),
        "--quiet".into(),
        "cat".into(),
        target_path(bucket, Some(key)),
    ];
    let env = admin_env_name();
    docker::exec_argv(docker_bin, &e.container_name, None, &[env.as_str()], &inner)
}

/// Full argv, `docker exec` prefix included, reading one object from stdin.
pub fn pipe_argv(
    docker_bin: &str,
    e: &EngineInstance,
    bucket: &str,
    key: &str,
    content_type: Option<&str>,
) -> Vec<String> {
    let mut inner = vec!["mc".to_string(), "--quiet".into(), "pipe".into()];
    if let Some(ct) = content_type {
        inner.push("--attr".into());
        inner.push(format!("Content-Type={ct}"));
    }
    inner.push(target_path(bucket, Some(key)));
    let env = admin_env_name();
    docker::exec_argv(docker_bin, &e.container_name, None, &[env.as_str()], &inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::EngineKind;
    use chrono::Utc;

    fn engine_row() -> EngineInstance {
        EngineInstance {
            id: "eng-2".into(),
            target_id: "tgt-1".into(),
            engine: EngineKind::Minio,
            major_version: "latest".into(),
            image: "minio/minio:latest".into(),
            container_name: "linf-minio-latest".into(),
            volume_name: "linf-minio-latest-data".into(),
            bind_address: "127.0.0.1".into(),
            host_port: 9000,
            console_port: Some(9001),
            admin_user: "linf_admin".into(),
            credential_ref: "engine:eng-2".into(),
            managed: true,
            created_at: Utc::now(),
        }
    }

    // -- validation ---------------------------------------------------------

    #[test]
    fn accepts_dns_label_bucket_names() {
        for name in ["abc", "letsbid-dev", "a1b", "2024-project-dev", "x9"]
            .iter()
            .filter(|n| n.len() >= 3)
        {
            assert!(
                validate_bucket_name(name).is_ok(),
                "`{name}`은 허용되어야 한다"
            );
        }
        let longest = "a".repeat(BUCKET_MAX_CHARS);
        assert!(validate_bucket_name(&longest).is_ok());
    }

    #[test]
    fn rejects_bucket_names_at_the_length_boundaries() {
        assert!(matches!(validate_bucket_name(""), Err(Error::Usage(_))));
        assert!(matches!(validate_bucket_name("ab"), Err(Error::Usage(_))));
        assert!(validate_bucket_name("abc").is_ok());
        let max = "a".repeat(BUCKET_MAX_CHARS);
        assert!(validate_bucket_name(&max).is_ok());
        let over = "a".repeat(BUCKET_MAX_CHARS + 1);
        assert!(matches!(validate_bucket_name(&over), Err(Error::Usage(_))));
    }

    #[test]
    fn rejects_illegal_bucket_names() {
        for name in [
            "Letsbid",      // uppercase
            "letsbid_dev",  // underscore
            "-letsbid",     // leading hyphen
            "letsbid-",     // trailing hyphen
            "letsbid dev",  // space
            "letsbid.dev",  // dot
            "letsbid..dev", // consecutive dots
            "192.168.0.1",  // IP address
            "달빛에디터",   // non-ascii
        ] {
            assert!(
                matches!(validate_bucket_name(name), Err(Error::Usage(_))),
                "`{name}`은 거부되어야 한다"
            );
        }
    }

    #[test]
    fn bucket_name_errors_name_the_actual_problem() {
        let ip = validate_bucket_name("10.0.0.1").unwrap_err().to_string();
        assert!(ip.contains("IP"), "{ip}");
        let dotted = validate_bucket_name("a.b.c").unwrap_err().to_string();
        assert!(dotted.contains('.'), "{dotted}");
    }

    #[test]
    fn access_key_length_boundaries() {
        assert!(matches!(validate_access_key(""), Err(Error::Usage(_))));
        assert!(matches!(validate_access_key("abcd"), Err(Error::Usage(_))));
        assert!(validate_access_key("abcde").is_ok());
        let max = "A".repeat(ACCESS_KEY_MAX_CHARS);
        assert!(validate_access_key(&max).is_ok());
        let over = "A".repeat(ACCESS_KEY_MAX_CHARS + 1);
        assert!(matches!(validate_access_key(&over), Err(Error::Usage(_))));
    }

    #[test]
    fn rejects_access_keys_that_would_need_escaping() {
        for key in [
            "with space",
            "with/slash",
            "with@at",
            "with:colon",
            "키값입니다",
        ] {
            assert!(
                matches!(validate_access_key(key), Err(Error::Usage(_))),
                "`{key}`은 거부되어야 한다"
            );
        }
        assert!(validate_access_key("LETSBID-DEV_1.0").is_ok());
    }

    #[test]
    fn generated_access_keys_look_like_aws_keys() {
        let key = generate_access_key();
        assert_eq!(key.len(), ACCESS_KEY_LEN);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "{key}"
        );
        assert!(validate_access_key(&key).is_ok());
        assert_ne!(key, generate_access_key(), "키는 매번 달라야 한다");
    }

    // -- policy -------------------------------------------------------------

    /// Every `Resource` ARN in the document, in document order.
    fn policy_resources(document: &str) -> Vec<String> {
        let parsed: serde_json::Value = serde_json::from_str(document).expect("정책은 JSON이다");
        parsed["Statement"]
            .as_array()
            .expect("Statement는 배열이다")
            .iter()
            .flat_map(|s| {
                s["Resource"]
                    .as_array()
                    .expect("Resource는 배열이다")
                    .iter()
                    .map(|r| r.as_str().expect("ARN은 문자열이다").to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn policy_names_exactly_one_bucket() {
        let document = bucket_policy("letsbid-dev");
        let mut resources = policy_resources(&document);
        resources.sort();
        assert_eq!(
            resources,
            vec![
                "arn:aws:s3:::letsbid-dev".to_string(),
                "arn:aws:s3:::letsbid-dev/*".to_string(),
            ],
            "정책은 자기 버킷만 언급해야 한다: {document}"
        );
    }

    #[test]
    fn policy_grants_never_deny_and_never_use_a_wildcard_bucket() {
        let document = bucket_policy("letsbid-dev");
        assert!(!document.contains("Deny"), "{document}");
        assert!(
            !document.contains("arn:aws:s3:::*"),
            "와일드카드 버킷은 최소 권한이 아니다: {document}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        assert_eq!(parsed["Version"], "2012-10-17");
        for statement in parsed["Statement"].as_array().unwrap() {
            assert_eq!(statement["Effect"], "Allow");
        }
    }

    #[test]
    fn policy_name_is_prefixed() {
        assert_eq!(policy_name("letsbid-dev"), "linf-letsbid-dev");
    }

    // -- argv ---------------------------------------------------------------

    #[test]
    fn cat_argv_is_exact() {
        let e = engine_row();
        assert_eq!(
            cat_argv("docker", &e, "letsbid-dev", "uploads/a.png"),
            vec![
                "docker",
                "exec",
                "--interactive",
                "--env",
                "MC_HOST_linf",
                "linf-minio-latest",
                "mc",
                "--quiet",
                "cat",
                "linf/letsbid-dev/uploads/a.png",
            ]
        );
    }

    #[test]
    fn pipe_argv_is_exact_with_and_without_a_content_type() {
        let e = engine_row();
        assert_eq!(
            pipe_argv("docker", &e, "letsbid-dev", "uploads/a.png", None),
            vec![
                "docker",
                "exec",
                "--interactive",
                "--env",
                "MC_HOST_linf",
                "linf-minio-latest",
                "mc",
                "--quiet",
                "pipe",
                "linf/letsbid-dev/uploads/a.png",
            ]
        );
        assert_eq!(
            pipe_argv(
                "docker",
                &e,
                "letsbid-dev",
                "uploads/a.png",
                Some("image/png")
            ),
            vec![
                "docker",
                "exec",
                "--interactive",
                "--env",
                "MC_HOST_linf",
                "linf-minio-latest",
                "mc",
                "--quiet",
                "pipe",
                "--attr",
                "Content-Type=image/png",
                "linf/letsbid-dev/uploads/a.png",
            ]
        );
    }

    #[test]
    fn no_argv_ever_carries_a_credential() {
        let e = engine_row();
        let secret = "S3cr3t-Key-Value";
        let access = "AKIALETSBIDDEV000001";
        let script = create_user_script(&policy_name("letsbid-dev"));
        let mut argvs: Vec<Vec<String>> = vec![
            cat_argv("docker", &e, "letsbid-dev", "a.png"),
            pipe_argv("docker", &e, "letsbid-dev", "a.png", Some("image/png")),
            verify_argv("letsbid-dev"),
            stdin_values_argv(&["AK", "SK"], &script),
            stdin_values_argv(&["AK"], "mc --json admin user remove linf \"$AK\""),
        ];
        argvs.push(mc_argv(&[
            "--json".to_string(),
            "admin".into(),
            "policy".into(),
            "create".into(),
            MINIO_ALIAS.to_string(),
            policy_name("letsbid-dev"),
            "/dev/stdin".into(),
        ]));
        for argv in &argvs {
            let joined = argv.join(" ");
            assert!(!joined.contains(secret), "시크릿이 argv에 노출됨: {joined}");
            assert!(
                !joined.contains(access),
                "액세스 키가 argv에 노출됨: {joined}"
            );
            assert!(
                !joined.contains("linf_admin"),
                "관리자 계정이 argv에 노출됨: {joined}"
            );
        }
        // The values travel on stdin instead, in declaration order.
        assert_eq!(
            stdin_values_payload(&[access, secret]),
            format!("{access}\n{secret}\n").into_bytes()
        );
    }

    #[test]
    fn the_stdin_preamble_reads_one_variable_per_line() {
        let argv = stdin_values_argv(
            &["AK", "SK"],
            "mc --json admin user add linf \"$AK\" \"$SK\"",
        );
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        assert_eq!(
            argv[2],
            "IFS= read -r AK; IFS= read -r SK; mc --json admin user add linf \"$AK\" \"$SK\""
        );
    }

    #[test]
    fn the_create_script_attaches_the_scoped_policy_to_the_new_user() {
        let script = create_user_script(&policy_name("letsbid-dev"));
        assert!(script.contains("admin user add"), "{script}");
        assert!(script.contains("admin policy attach"), "{script}");
        assert!(script.contains("linf-letsbid-dev"), "{script}");
        assert!(script.contains("--user \"$AK\""), "{script}");
    }

    #[test]
    fn a_verification_uses_a_separate_alias_from_the_admin_one() {
        let e = engine_row();
        let secrets = project_env(&e, "AKIA0000000000000001", "secret").unwrap();
        assert_eq!(secrets.names(), vec!["MC_HOST_linfproj"]);
        assert_ne!(secrets.names()[0], admin_env_name());
        assert!(verify_argv("letsbid-dev")
            .join(" ")
            .contains("linfproj/letsbid-dev"));
    }

    #[tokio::test]
    async fn stdin_values_must_match_their_names_and_stay_single_line() {
        let e = engine_row();
        let x = Executor::local();
        assert!(matches!(
            mc_with_stdin_values(&x, &e, "pw", &["AK", "SK"], &["only-one"], "true").await,
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            mc_with_stdin_values(&x, &e, "pw", &["1BAD"], &["value"], "true").await,
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            mc_with_stdin_values(&x, &e, "pw", &["AK"], &["two\nlines"], "true").await,
            Err(Error::Usage(_))
        ));
    }

    // -- parsing ------------------------------------------------------------

    /// The fixtures below are verbatim `mc --json` output captured from
    /// `minio/minio:latest` (RELEASE.2025-08-13), so a change in `mc` shows up
    /// here rather than in production.
    #[test]
    fn parses_recursive_ls_output_and_skips_directories() {
        let text = r#"
{"status":"success","type":"folder","lastModified":"2026-09-01T13:23:15.586580844Z","size":0,"key":"uploads/","etag":"","url":"http://127.0.0.1:9000/letsbid-dev/","versionOrdinal":1}
{"status":"success","type":"file","lastModified":"2026-09-01T13:23:15.367Z","size":11,"key":"uploads/a.txt","etag":"241d8a27c836427bd7f04461b60e7359-1","url":"http://127.0.0.1:9000/letsbid-dev/","versionOrdinal":1,"storageClass":"STANDARD"}
{"status":"success","type":"file","lastModified":"2026-09-01T13:23:15.444Z","size":0,"key":"empty.bin","etag":"59adb24ef3cdbe0297f05b395827453f-1","url":"http://127.0.0.1:9000/letsbid-dev/","versionOrdinal":1,"storageClass":"STANDARD","contentType":"application/octet-stream"}
not json at all
{"status":"error","error":{"message":"Unable to list folder.","cause":{"message":"Access Denied."},"type":"error"}}
"#;
        let objects = parse_ls_json(text);
        assert_eq!(objects.len(), 2, "{objects:?}");
        assert_eq!(
            objects[0],
            ObjectEntry {
                key: "uploads/a.txt".into(),
                size: 11,
                etag: Some("241d8a27c836427bd7f04461b60e7359-1".into()),
                // `mc ls --json` carries no content type; the field exists for
                // the manifest, which is why it stays `None` here.
                content_type: None,
            }
        );
        assert_eq!(
            objects[1],
            ObjectEntry {
                key: "empty.bin".into(),
                size: 0,
                etag: Some("59adb24ef3cdbe0297f05b395827453f-1".into()),
                content_type: Some("application/octet-stream".into()),
            }
        );
    }

    #[test]
    fn parses_bucket_listing_which_reports_buckets_as_folders() {
        let text = concat!(
            r#"{"status":"success","type":"folder","lastModified":"2026-09-01T13:22:51.025Z","size":0,"key":"letsbid-dev/","etag":"","url":"http://127.0.0.1:9000/","versionOrdinal":1}"#,
            "\n",
            r#"{"status":"success","type":"folder","lastModified":"2026-09-01T13:23:15.084Z","size":0,"key":"dalbit-dev/","etag":"","url":"http://127.0.0.1:9000/","versionOrdinal":1}"#,
            "\n",
        );
        assert_eq!(parse_bucket_names(text), vec!["dalbit-dev", "letsbid-dev"]);
        assert!(parse_bucket_names("").is_empty());
    }

    #[test]
    fn parses_user_and_policy_listings() {
        let users = concat!(
            r#"{"status":"success","accessKey":"AKIALETSBID","policyName":"linf-letsbid-dev","userStatus":"enabled"}"#,
            "\n",
            r#"{"status":"success","accessKey":"AKIADALBIT","policyName":"linf-dalbit-dev","userStatus":"enabled"}"#,
        );
        assert_eq!(parse_user_keys(users), vec!["AKIALETSBID", "AKIADALBIT"]);
        let policies = concat!(
            r#"{"status":"success","policy":"readwrite","policyInfo":{"PolicyName":"","Policy":null},"isGroup":false}"#,
            "\n",
            r#"{"status":"success","policy":"linf-letsbid-dev","policyInfo":{"PolicyName":"","Policy":null},"isGroup":false}"#,
        );
        assert_eq!(
            parse_policy_names(policies),
            vec!["readwrite", "linf-letsbid-dev"]
        );
    }

    #[test]
    fn parses_du_output_taking_the_total() {
        let text = concat!(
            r#"{"prefix":"letsbid-dev/uploads","size":11,"objects":1,"status":"success"}"#,
            "\n",
            r#"{"prefix":"letsbid-dev","size":4096,"objects":3,"status":"success","isVersions":false}"#,
        );
        assert_eq!(
            parse_du_json(text),
            BucketStats {
                objects: Some(3),
                size_bytes: Some(4096),
            }
        );
        assert_eq!(parse_du_json(""), BucketStats::default());
        // Older builds render the numbers as strings.
        assert_eq!(
            parse_du_json(r#"{"status":"success","size":"512","objects":"2"}"#),
            BucketStats {
                objects: Some(2),
                size_bytes: Some(512),
            }
        );
    }

    #[test]
    fn a_chained_failure_never_logs_the_secret_mc_echoed_back() {
        // Verbatim stdout of `mc --json admin user add` followed by a failing
        // `mc --json admin policy attach`, captured from the real image.
        let out = concat!(
            r#"{"status":"success","accessKey":"AKIALEAKPROBE00001","secretKey":"super-secret-value-xyz","userStatus":"enabled"}"#,
            "\n",
            r#"{"status":"error","error":{"message":"Unable to make user/group policy association","cause":{"message":"The canned policy does not exist."}}}"#,
        );
        let scrubbed = scrub(out);
        assert!(
            !scrubbed.contains("super-secret-value-xyz"),
            "시크릿이 진단에 남았다: {scrubbed}"
        );
        assert!(scrubbed.contains(r#""secretKey":"****""#), "{scrubbed}");
        // Everything a user needs to act on survives.
        assert!(scrubbed.contains("AKIALEAKPROBE00001"), "{scrubbed}");
        assert!(
            scrubbed.contains("The canned policy does not exist."),
            "{scrubbed}"
        );
    }

    #[test]
    fn scrub_masks_every_secret_shape_and_leaves_prose_alone() {
        assert_eq!(
            scrub(r#"{"secretAccessKey": "abc123", "accessKey":"AK"}"#),
            r#"{"secretAccessKey": "****", "accessKey":"AK"}"#
        );
        assert_eq!(
            scrub(r#"{"SecretKey":"abc\"123","n":1}"#),
            r#"{"SecretKey":"****","n":1}"#
        );
        // Not a JSON member: nothing to mask, nothing to mangle.
        assert_eq!(
            scrub("the secret is not a json member"),
            "the secret is not a json member"
        );
        // The URL rule from `util::redact` still applies underneath.
        assert_eq!(
            scrub("http://linf_admin:hunter2@127.0.0.1:9000"),
            "http://linf_admin:****@127.0.0.1:9000"
        );
        // Non-ASCII around the match must survive byte-index scanning.
        assert_eq!(
            scrub(r#"오류: {"secretKey":"키값"} 발생"#),
            r#"오류: {"secretKey":"****"} 발생"#
        );
    }
}
