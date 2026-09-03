//! Self-update through the same checksum-verifying installer used for first install.

use crate::core::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use tempfile::Builder;
use tokio::process::Command;

const INSTALLER_URL: &str = "https://apps.nomadable.io/local-infra/install";
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/nomadable/local-infra/releases/latest";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateReceipt {
    pub path: String,
    pub current_version: String,
    pub latest_version: String,
    pub updated: bool,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Compare the running package with GitHub's canonical latest release. The
/// installer only runs when that release is newer, and receives the discovered
/// tag so the metadata check and installed archive cannot drift apart.
pub async fn run() -> Result<(UpdateReceipt, String)> {
    let executable = std::env::current_exe().map_err(|error| {
        Error::failed(
            "현재 linf 실행 파일 경로를 찾을 수 없습니다",
            error.to_string(),
            "공식 installer로 다시 설치한 뒤 `linf update`를 실행하세요.",
        )
    })?;
    let install_dir = installation_dir(&executable)?;
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let current = parse_plain_version(&current_version)?;
    let (latest_tag, latest_version, latest) = latest_release().await?;

    if current.cmp(&latest) != Ordering::Less {
        return Ok((
            UpdateReceipt {
                path: executable.display().to_string(),
                current_version: current_version.clone(),
                latest_version,
                updated: false,
            },
            String::new(),
        ));
    }

    let temp = Builder::new()
        .prefix("linf-update-")
        .tempdir()
        .map_err(|error| {
            Error::failed(
                "업데이트 임시 폴더를 만들 수 없습니다",
                error.to_string(),
                "임시 폴더 권한과 디스크 공간을 확인한 뒤 다시 실행하세요.",
            )
        })?;
    let bootstrap = temp.path().join("installer.sh");
    let installer_output =
        update_inner(&bootstrap, &install_dir, &executable, &latest_tag, latest).await?;

    Ok((
        UpdateReceipt {
            path: executable.display().to_string(),
            current_version,
            latest_version,
            updated: true,
        },
        installer_output,
    ))
}

fn installation_dir(executable: &Path) -> Result<PathBuf> {
    let parent = executable.parent().ok_or_else(|| {
        Error::failed(
            "현재 linf 실행 파일 경로를 사용할 수 없습니다",
            executable.display().to_string(),
            "공식 installer로 다시 설치한 뒤 `linf update`를 실행하세요.",
        )
    })?;

    let is_profile_dir = matches!(
        parent.file_name().and_then(|name| name.to_str()),
        Some("debug" | "release")
    );
    let is_cargo_build = is_profile_dir
        && parent.parent().is_some_and(|candidate| {
            candidate.file_name().is_some_and(|name| name == "target")
                || candidate.parent().is_some_and(|ancestor| {
                    ancestor.file_name().is_some_and(|name| name == "target")
                })
        });
    if is_cargo_build {
        return Err(Error::Refused(
            "소스 빌드 실행 파일은 `linf update`로 바꾸지 않습니다. `cargo install --path . --locked`로 다시 설치하세요."
                .into(),
        ));
    }

    Ok(parent.to_path_buf())
}

async fn latest_release() -> Result<(String, String, Version)> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2026-03-10",
            "--header",
            "User-Agent: local-infra",
            LATEST_RELEASE_URL,
        ])
        .output()
        .await
        .map_err(|error| {
            command_error("최신 릴리즈 정보를 가져올 수 없습니다", error.to_string())
        })?;
    if !output.status.success() {
        return Err(command_error(
            "최신 릴리즈 정보를 가져올 수 없습니다",
            output_detail(&output.stderr),
        ));
    }

    let release: LatestRelease = serde_json::from_slice(&output.stdout).map_err(|error| {
        Error::failed(
            "최신 릴리즈 정보를 읽을 수 없습니다",
            error.to_string(),
            "잠시 뒤 다시 실행하거나 GitHub API 상태를 확인하세요.",
        )
    })?;
    let version = parse_release_tag(&release.tag_name)?;
    Ok((release.tag_name, version.to_string(), version))
}

async fn update_inner(
    bootstrap: &Path,
    install_dir: &Path,
    executable: &Path,
    latest_tag: &str,
    expected_version: Version,
) -> Result<String> {
    let download = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--tlsv1.2",
            INSTALLER_URL,
            "--output",
        ])
        .arg(bootstrap)
        .output()
        .await
        .map_err(|error| {
            command_error(
                "업데이트 installer를 내려받을 수 없습니다",
                error.to_string(),
            )
        })?;
    if !download.status.success() {
        return Err(command_error(
            "업데이트 installer를 내려받을 수 없습니다",
            output_detail(&download.stderr),
        ));
    }

    let install = Command::new("sh")
        // `linf update` has one trust target: the canonical installer. Letting
        // a caller's test/fork override redirect its release repository breaks
        // that guarantee.
        .env_remove("LINF_REPOSITORY")
        .arg(bootstrap)
        .arg("--version")
        .arg(latest_tag)
        .arg("--install-dir")
        .arg(install_dir)
        .output()
        .await
        .map_err(|error| command_error("최신 linf를 설치할 수 없습니다", error.to_string()))?;
    if !install.status.success() {
        return Err(command_error(
            "최신 linf를 설치할 수 없습니다",
            output_detail(&install.stderr),
        ));
    }

    let version = Command::new(executable)
        .arg("--version")
        .output()
        .await
        .map_err(|error| {
            command_error("업데이트한 linf를 확인할 수 없습니다", error.to_string())
        })?;
    if !version.status.success() {
        return Err(command_error(
            "업데이트한 linf를 확인할 수 없습니다",
            output_detail(&version.stderr),
        ));
    }
    let version_text = String::from_utf8_lossy(&version.stdout);
    let reported = version_text.split_whitespace().last().ok_or_else(|| {
        Error::failed(
            "업데이트한 linf 버전을 확인할 수 없습니다",
            "`linf --version` 출력이 비어 있습니다.",
            "공식 installer로 다시 설치한 뒤 `linf --version`을 확인하세요.",
        )
    })?;
    let installed = parse_plain_version(reported)?;
    if installed != expected_version {
        return Err(Error::failed(
            "업데이트한 linf 버전이 예상과 다릅니다",
            format!(
                "요청한 버전은 {}, 설치된 버전은 {}입니다.",
                expected_version, installed
            ),
            "`linf update`를 다시 실행하거나 GitHub Release 상태를 확인하세요.",
        ));
    }

    Ok(String::from_utf8_lossy(&install.stdout).trim().to_string())
}

fn parse_release_tag(tag: &str) -> Result<Version> {
    let version = tag
        .trim()
        .strip_prefix('v')
        .ok_or_else(|| invalid_version(tag))?;
    parse_plain_version(version)
}

fn parse_plain_version(input: &str) -> Result<Version> {
    let normalized = input.trim();
    let mut parts = normalized.split('.');
    let version = Version(
        parse_component(parts.next(), input)?,
        parse_component(parts.next(), input)?,
        parse_component(parts.next(), input)?,
    );
    if parts.next().is_some() {
        return Err(invalid_version(input));
    }
    Ok(version)
}

fn parse_component(value: Option<&str>, input: &str) -> Result<u64> {
    let value = value.ok_or_else(|| invalid_version(input))?;
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_version(input));
    }
    value.parse::<u64>().map_err(|_| invalid_version(input))
}

fn invalid_version(input: &str) -> Error {
    Error::failed(
        "릴리즈 버전을 읽을 수 없습니다",
        format!("`{input}`은(는) vX.Y.Z 형식이 아닙니다."),
        "공식 GitHub Release의 tag 형식을 확인하세요.",
    )
}
fn command_error(what: &str, cause: String) -> Error {
    Error::failed(
        what,
        if cause.trim().is_empty() {
            "명령이 실패했지만 추가 출력을 제공하지 않았습니다.".into()
        } else {
            cause
        },
        "네트워크와 설치 경로 권한을 확인한 뒤 다시 실행하세요.",
    )
}

fn output_detail(output: &[u8]) -> String {
    const MAX_CHARS: usize = 4096;
    let text = String::from_utf8_lossy(output).trim().to_string();
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let truncated: String = text.chars().take(MAX_CHARS).collect();
    format!("{truncated}\n… 출력이 너무 길어 일부만 표시했습니다.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tags_compare_numerically() {
        assert_eq!(parse_release_tag("v0.2.10").unwrap(), Version(0, 2, 10));
        assert!(parse_release_tag("v0.2.10").unwrap() > parse_release_tag("v0.2.9").unwrap());
        assert_eq!(parse_plain_version("0.2.10").unwrap(), Version(0, 2, 10));
    }

    #[test]
    fn malformed_or_noncanonical_release_tags_are_refused() {
        assert!(parse_release_tag("0.2.3").is_err());
        assert!(parse_release_tag("v0.2").is_err());
        assert!(parse_release_tag("v0.2.3-beta").is_err());
        assert!(parse_release_tag("v0.02.3").is_err());
    }

    #[test]
    fn installed_binary_updates_its_own_parent_directory() {
        let path = Path::new("/Users/example/.local/bin/linf");
        assert_eq!(
            installation_dir(path).unwrap(),
            PathBuf::from("/Users/example/.local/bin")
        );
    }

    #[test]
    fn cargo_target_binary_refuses_self_replacement() {
        let path = Path::new("/work/local-infra/target/debug/linf");
        assert!(matches!(installation_dir(path), Err(Error::Refused(_))));
    }

    #[test]
    fn target_triple_release_binary_refuses_self_replacement() {
        let path = Path::new("/work/local-infra/target/aarch64-apple-darwin/release/linf");
        assert!(matches!(installation_dir(path), Err(Error::Refused(_))));
    }
}
