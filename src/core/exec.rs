//! Command execution against a target — locally or over `ssh` (PRD §6.3).
//!
//! Two invariants hold for every call:
//!
//! 1. **No secret ever reaches an argv.** Values that must not appear in
//!    `ps(1)` are passed with [`SecretEnv`]: locally as child-process
//!    environment, remotely by streaming them on stdin and having a two-line
//!    shell preamble `read` and `export` them before `exec`.
//! 2. **Host key checking is never disabled** (PRD §11.3). There is no option
//!    to turn it off.

use crate::core::error::{Diagnostic, Error, Result};
use crate::core::model::Target;
use crate::core::progress::{Cancel, Reporter};
use crate::core::util::{redact, shell_join, shell_quote};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

/// Seconds before an SSH connection attempt is abandoned.
const SSH_CONNECT_TIMEOUT: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }

    /// Combined output, redacted — the only form safe for logs and modals.
    pub fn message(&self) -> String {
        let mut parts = Vec::new();
        let err = self.stderr_str();
        if !err.is_empty() {
            parts.push(err);
        }
        let out = self.stdout_str();
        if !out.is_empty() {
            parts.push(out);
        }
        redact(&parts.join("\n"))
    }
}

/// Environment variables that must stay out of argv.
#[derive(Debug, Clone, Default)]
pub struct SecretEnv(Vec<(String, String)>);

impl SecretEnv {
    pub fn new() -> Self {
        Self::default()
    }

    /// Values containing a newline are rejected: the remote transport is
    /// line-delimited, so a newline would desynchronise the preamble.
    pub fn set(mut self, name: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let name = name.into();
        let value = value.into();
        if value.contains('\n') || value.contains('\r') {
            return Err(Error::Usage(format!(
                "`{name}` 값에는 줄바꿈을 포함할 수 없습니다."
            )));
        }
        self.0.push((name, value));
        Ok(self)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Variable names in declaration order. Values are deliberately not
    /// exposed — callers may only pass them through [`Executor`].
    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// `value1\nvalue2\n`, consumed by the remote `read` preamble in order.
    fn payload(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for (_, v) in &self.0 {
            buf.extend_from_slice(v.as_bytes());
            buf.push(b'\n');
        }
        buf
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub identity: Option<PathBuf>,
}

impl SshConfig {
    pub fn destination(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// Options shared by command execution, keyscan and tunnels.
    /// `StrictHostKeyChecking=yes` is not configurable (PRD §11.3).
    pub fn base_options(&self) -> Vec<String> {
        let mut argv = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={SSH_CONNECT_TIMEOUT}"),
            "-p".into(),
            self.port.to_string(),
        ];
        if let Some(id) = &self.identity {
            argv.push("-i".into());
            argv.push(id.display().to_string());
            // An explicit key means "use exactly this one".
            argv.push("-o".into());
            argv.push("IdentitiesOnly=yes".into());
        }
        argv
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Executor {
    Local { docker: String },
    Ssh { ssh: SshConfig, docker: String },
}

impl Executor {
    pub fn local() -> Self {
        Executor::Local {
            docker: "docker".into(),
        }
    }

    /// Build the executor for a target. Remote targets without an approved host
    /// key fingerprint are refused here rather than at connect time (TAR-005).
    pub fn for_target(target: &Target) -> Result<Self> {
        match target.kind {
            crate::core::model::TargetKind::Local => Ok(Executor::Local {
                docker: target.docker_command.clone(),
            }),
            crate::core::model::TargetKind::Ssh => {
                if target.host_key_fingerprint.is_none() {
                    return Err(Error::Refused(format!(
                        "Target `{}`의 SSH 호스트 키가 승인되지 않았습니다. \
                         `linf target verify {}`로 지문을 확인하세요.",
                        target.display_name, target.display_name
                    )));
                }
                let host = target.host.clone().ok_or_else(|| {
                    Error::Usage(format!(
                        "Target `{}`에 SSH 호스트가 없습니다.",
                        target.display_name
                    ))
                })?;
                Ok(Executor::Ssh {
                    ssh: SshConfig {
                        host,
                        port: target.ssh_port.unwrap_or(22),
                        user: target.ssh_username.clone(),
                        identity: target
                            .identity_path
                            .as_deref()
                            .map(crate::core::config::expand_tilde),
                    },
                    docker: target.docker_command.clone(),
                })
            }
        }
    }

    pub fn docker_bin(&self) -> &str {
        match self {
            Executor::Local { docker } | Executor::Ssh { docker, .. } => docker,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Executor::Ssh { .. })
    }

    pub fn ssh(&self) -> Option<&SshConfig> {
        match self {
            Executor::Ssh { ssh, .. } => Some(ssh),
            Executor::Local { .. } => None,
        }
    }

    /// The full local argv that runs `argv` on this target. Exposed so tests
    /// and diagnostics can show exactly what will run.
    pub fn command_line(&self, argv: &[String], secrets: &SecretEnv) -> Vec<String> {
        match self {
            Executor::Local { .. } => argv.to_vec(),
            Executor::Ssh { ssh, .. } => {
                let mut out = vec!["ssh".to_string()];
                out.extend(ssh.base_options());
                out.push(ssh.destination());
                out.push("--".into());
                out.push(remote_script(argv, secrets));
                out
            }
        }
    }

    /// Human-readable, redacted rendering for diagnostics and the activity log.
    pub fn describe(&self, argv: &[String]) -> String {
        redact(&shell_join(&self.command_line(argv, &SecretEnv::new())))
    }

    pub async fn run(&self, argv: &[String]) -> Result<Output> {
        self.run_full(argv, &SecretEnv::new(), None).await
    }

    pub async fn run_with_stdin(&self, argv: &[String], stdin: &[u8]) -> Result<Output> {
        self.run_full(argv, &SecretEnv::new(), Some(stdin)).await
    }

    pub async fn run_secret(
        &self,
        argv: &[String],
        secrets: &SecretEnv,
        stdin: Option<&[u8]>,
    ) -> Result<Output> {
        self.run_full(argv, secrets, stdin).await
    }

    /// Run and fail with a rich diagnostic when the exit code is non-zero.
    pub async fn run_checked(&self, argv: &[String], what: &str, next: &str) -> Result<Output> {
        let out = self.run(argv).await?;
        if !out.ok() {
            return Err(self.failure(argv, &out, what, next));
        }
        Ok(out)
    }

    pub fn failure(&self, argv: &[String], out: &Output, what: &str, next: &str) -> Error {
        Error::diagnostic(
            Diagnostic::new(
                what.to_string(),
                format!("종료 코드 {}", out.code),
                next.to_string(),
            )
            .with_command(self.describe(argv))
            .with_output(out.message()),
        )
    }

    async fn run_full(
        &self,
        argv: &[String],
        secrets: &SecretEnv,
        stdin: Option<&[u8]>,
    ) -> Result<Output> {
        let mut cmd = self.build(argv, secrets)?;
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| self.spawn_error(argv, e))?;
        let mut input = self.stdin_payload(secrets);
        if let Some(extra) = stdin {
            input.extend_from_slice(extra);
        }
        {
            let mut sink = child.stdin.take().expect("stdin piped");
            sink.write_all(&input).await?;
            sink.shutdown().await?;
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    /// Stream stdout into `writer`, reporting bytes and honouring cancellation.
    /// Used by `pg_dump` (BAK-003/007).
    pub async fn stream_out<W: AsyncWrite + Unpin>(
        &self,
        argv: &[String],
        secrets: &SecretEnv,
        writer: &mut W,
        cancel: &Cancel,
        reporter: &Reporter,
    ) -> Result<(Output, u64)> {
        let mut cmd = self.build(argv, secrets)?;
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| self.spawn_error(argv, e))?;

        {
            let mut sink = child.stdin.take().expect("stdin piped");
            sink.write_all(&self.stdin_payload(secrets)).await?;
            sink.shutdown().await?;
        }

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr_handle = child.stderr.take().expect("stderr piped");
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_handle.read_to_end(&mut buf).await;
            buf
        });

        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        let mut last_report = 0u64;
        loop {
            if cancel.is_cancelled() {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(Error::Cancelled);
            }
            let n = stdout.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).await?;
            total += n as u64;
            if total - last_report >= 1024 * 1024 {
                last_report = total;
                reporter.bytes(total);
            }
        }
        writer.flush().await?;
        reporter.bytes(total);

        let status = child.wait().await?;
        let stderr = stderr_task.await.unwrap_or_default();
        Ok((
            Output {
                code: status.code().unwrap_or(-1),
                stdout: Vec::new(),
                stderr,
            },
            total,
        ))
    }

    /// Feed `reader` into the command's stdin. Used by `pg_restore` (BAK-005).
    pub async fn stream_in<R: AsyncRead + Unpin>(
        &self,
        argv: &[String],
        secrets: &SecretEnv,
        reader: &mut R,
        cancel: &Cancel,
        reporter: &Reporter,
    ) -> Result<(Output, u64)> {
        let mut cmd = self.build(argv, secrets)?;
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| self.spawn_error(argv, e))?;

        let mut sink = child.stdin.take().expect("stdin piped");
        sink.write_all(&self.stdin_payload(secrets)).await?;

        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        let mut last_report = 0u64;
        loop {
            if cancel.is_cancelled() {
                drop(sink);
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(Error::Cancelled);
            }
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            sink.write_all(&buf[..n]).await?;
            total += n as u64;
            if total - last_report >= 1024 * 1024 {
                last_report = total;
                reporter.bytes(total);
            }
        }
        sink.shutdown().await?;
        drop(sink);
        reporter.bytes(total);

        let out = child.wait_with_output().await?;
        Ok((
            Output {
                code: out.status.code().unwrap_or(-1),
                stdout: out.stdout,
                stderr: out.stderr,
            },
            total,
        ))
    }

    fn build(&self, argv: &[String], secrets: &SecretEnv) -> Result<Command> {
        if argv.is_empty() {
            return Err(Error::Usage("실행할 명령이 비어 있습니다.".into()));
        }
        let line = self.command_line(argv, secrets);
        let mut cmd = Command::new(&line[0]);
        cmd.args(&line[1..]);
        cmd.kill_on_drop(true);
        if let Executor::Local { .. } = self {
            for (k, v) in &secrets.0 {
                cmd.env(k, v);
            }
        }
        Ok(cmd)
    }

    /// Bytes that must precede any caller-supplied stdin.
    fn stdin_payload(&self, secrets: &SecretEnv) -> Vec<u8> {
        match self {
            // Locally the values are real environment variables already.
            Executor::Local { .. } => Vec::new(),
            Executor::Ssh { .. } => secrets.payload(),
        }
    }

    fn spawn_error(&self, argv: &[String], e: std::io::Error) -> Error {
        let program = match self {
            Executor::Local { .. } => argv[0].clone(),
            Executor::Ssh { .. } => "ssh".to_string(),
        };
        Error::diagnostic(
            Diagnostic::new(
                format!("`{program}` 명령을 실행할 수 없습니다"),
                e.to_string(),
                format!("`{program}`이(가) 설치되어 있고 PATH에 있는지 확인하세요."),
            )
            .with_command(self.describe(argv)),
        )
    }
}

/// The command string handed to the remote shell.
///
/// With secrets, a preamble reads one line per variable from stdin and exports
/// it, then `exec` replaces the shell so the rest of stdin belongs to the real
/// command. POSIX requires `read` on a non-seekable fd not to read ahead, which
/// is what makes handing the remainder to `exec` safe.
fn remote_script(argv: &[String], secrets: &SecretEnv) -> String {
    let command = shell_join(argv);
    if secrets.is_empty() {
        return format!("exec {command}");
    }
    let names = secrets.names();
    let reads = names
        .iter()
        .map(|n| format!("IFS= read -r {n};"))
        .collect::<Vec<_>>()
        .join(" ");
    let exports = names
        .iter()
        .map(|n| shell_quote(n))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{reads} export {exports}; exec {command}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_exec() -> Executor {
        Executor::Ssh {
            ssh: SshConfig {
                host: "vps.ts.net".into(),
                port: 2222,
                user: Some("dev".into()),
                identity: None,
            },
            docker: "docker".into(),
        }
    }

    #[test]
    fn local_command_line_is_the_argv_itself() {
        let e = Executor::local();
        let argv = vec!["docker".to_string(), "ps".to_string()];
        assert_eq!(e.command_line(&argv, &SecretEnv::new()), argv);
    }

    #[test]
    fn remote_command_line_never_disables_host_key_checking() {
        let line = ssh_exec().command_line(&["docker".into(), "ps".into()], &SecretEnv::new());
        let joined = line.join(" ");
        assert!(joined.contains("StrictHostKeyChecking=yes"));
        assert!(!joined.contains("StrictHostKeyChecking=no"));
        assert!(joined.contains("BatchMode=yes"));
        assert!(joined.contains("-p 2222"));
        assert!(joined.contains("dev@vps.ts.net"));
    }

    #[test]
    fn remote_arguments_are_shell_quoted() {
        let line = ssh_exec().command_line(
            &["docker".into(), "exec".into(), "a; rm -rf /".into()],
            &SecretEnv::new(),
        );
        let script = line.last().unwrap();
        assert_eq!(script, "exec docker exec 'a; rm -rf /'");
    }

    #[test]
    fn secrets_never_appear_in_the_remote_command_line() {
        let secrets = SecretEnv::new()
            .set("POSTGRES_PASSWORD", "sup3r-s3cret")
            .unwrap();
        let line = ssh_exec().command_line(
            &[
                "docker".into(),
                "run".into(),
                "-e".into(),
                "POSTGRES_PASSWORD".into(),
            ],
            &secrets,
        );
        let joined = line.join(" ");
        assert!(!joined.contains("sup3r-s3cret"), "{joined}");
        assert!(joined.contains("IFS= read -r POSTGRES_PASSWORD;"));
        assert!(joined.contains("export POSTGRES_PASSWORD;"));
        assert!(joined.contains("exec docker run -e POSTGRES_PASSWORD"));
    }

    #[test]
    fn secret_payload_is_one_line_per_variable_in_order() {
        let secrets = SecretEnv::new()
            .set("A", "one")
            .unwrap()
            .set("B", "two")
            .unwrap();
        assert_eq!(secrets.payload(), b"one\ntwo\n");
        assert_eq!(ssh_exec().stdin_payload(&secrets), b"one\ntwo\n");
        assert!(
            Executor::local().stdin_payload(&secrets).is_empty(),
            "locally the values are real env vars"
        );
    }

    #[test]
    fn newline_values_are_rejected_because_the_transport_is_line_based() {
        assert!(SecretEnv::new().set("A", "one\ntwo").is_err());
        assert!(SecretEnv::new().set("A", "one\rtwo").is_err());
    }

    #[test]
    fn describe_is_redacted_and_shows_the_real_invocation() {
        let text = ssh_exec().describe(&["docker".into(), "ps".into()]);
        assert!(text.starts_with("ssh "));
        assert!(text.contains("'exec docker ps'"));
    }

    #[test]
    fn identity_path_forces_identities_only() {
        let e = Executor::Ssh {
            ssh: SshConfig {
                host: "h".into(),
                port: 22,
                user: None,
                identity: Some(PathBuf::from("/k/id_ed25519")),
            },
            docker: "docker".into(),
        };
        let joined = e
            .command_line(&["true".into()], &SecretEnv::new())
            .join(" ");
        assert!(joined.contains("-i /k/id_ed25519"));
        assert!(joined.contains("IdentitiesOnly=yes"));
    }

    #[tokio::test]
    async fn local_run_captures_streams_and_exit_code() {
        let e = Executor::local();
        let out = e
            .run(&[
                "sh".into(),
                "-c".into(),
                "printf out; printf err >&2; exit 3".into(),
            ])
            .await
            .unwrap();
        assert_eq!(out.code, 3);
        assert_eq!(out.stdout_str(), "out");
        assert_eq!(out.stderr_str(), "err");
        assert!(!out.ok());
    }

    #[tokio::test]
    async fn local_secret_env_reaches_the_child_but_not_the_argv() {
        let e = Executor::local();
        let secrets = SecretEnv::new().set("MY_SECRET", "abc123").unwrap();
        let argv = vec!["sh".into(), "-c".into(), "printf %s \"$MY_SECRET\"".into()];
        assert!(!e.describe(&argv).contains("abc123"));
        let out = e.run_secret(&argv, &secrets, None).await.unwrap();
        assert_eq!(out.stdout_str(), "abc123");
    }

    #[tokio::test]
    async fn stdin_is_forwarded_to_the_command() {
        let e = Executor::local();
        let out = e
            .run_with_stdin(&["cat".into()], b"hello stdin")
            .await
            .unwrap();
        assert_eq!(out.stdout_str(), "hello stdin");
    }

    #[tokio::test]
    async fn stream_out_writes_everything_and_counts_bytes() {
        let e = Executor::local();
        let mut buf: Vec<u8> = Vec::new();
        let (out, total) = e
            .stream_out(
                &["sh".into(), "-c".into(), "printf 'abcdefghij'".into()],
                &SecretEnv::new(),
                &mut buf,
                &Cancel::new(),
                &Reporter::silent(),
            )
            .await
            .unwrap();
        assert!(out.ok());
        assert_eq!(total, 10);
        assert_eq!(buf, b"abcdefghij");
    }

    #[tokio::test]
    async fn stream_out_stops_when_cancelled() {
        let e = Executor::local();
        let cancel = Cancel::new();
        cancel.cancel();
        let mut buf: Vec<u8> = Vec::new();
        let err = e
            .stream_out(
                &["sh".into(), "-c".into(), "sleep 30".into()],
                &SecretEnv::new(),
                &mut buf,
                &cancel,
                &Reporter::silent(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }

    #[tokio::test]
    async fn stream_in_feeds_stdin_from_a_reader() {
        let e = Executor::local();
        let mut src = std::io::Cursor::new(b"streamed payload".to_vec());
        let (out, total) = e
            .stream_in(
                &["cat".into()],
                &SecretEnv::new(),
                &mut src,
                &Cancel::new(),
                &Reporter::silent(),
            )
            .await
            .unwrap();
        assert!(out.ok());
        assert_eq!(total, 16);
        assert_eq!(out.stdout_str(), "streamed payload");
    }

    #[tokio::test]
    async fn missing_program_produces_an_actionable_diagnostic() {
        let e = Executor::local();
        let err = e
            .run(&["definitely-not-a-real-binary-xyz".into()])
            .await
            .unwrap_err();
        let d = err.as_diagnostic();
        assert!(d.what.contains("definitely-not-a-real-binary-xyz"));
        assert!(d.next.contains("PATH"));
    }

    #[test]
    fn unapproved_host_key_blocks_executor_creation() {
        use crate::core::model::{Target, TargetKind};
        let t = Target {
            id: "t".into(),
            kind: TargetKind::Ssh,
            display_name: "dev-vps".into(),
            host: Some("h".into()),
            ssh_port: Some(22),
            ssh_username: None,
            auth_type: None,
            identity_path: None,
            docker_command: "docker".into(),
            host_key_fingerprint: None,
            created_at: crate::core::util::now(),
            last_connected_at: None,
        };
        let err = Executor::for_target(&t).unwrap_err();
        assert!(matches!(err, Error::Refused(_)));
        assert_eq!(err.exit_code(), 2);
    }
}
