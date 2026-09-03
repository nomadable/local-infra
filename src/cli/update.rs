//! Self-update through the same checksum-verifying installer used for first install.

use crate::core::error::{Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tempfile::Builder;
use tokio::process::Command;

const INSTALLER_URL: &str = "https://apps.nomadable.io/local-infra/install";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateReceipt {
    pub path: String,
    pub version: String,
}

/// Install the latest release into the directory containing the running binary,
/// then execute that replacement once to verify it reports a version.
pub async fn run() -> Result<(UpdateReceipt, String)> {
    let executable = std::env::current_exe().map_err(|error| {
        Error::failed(
            "현재 linf 실행 파일 경로를 찾을 수 없습니다",
            error.to_string(),
            "공식 installer로 다시 설치한 뒤 `linf update`를 실행하세요.",
        )
    })?;
    let install_dir = installation_dir(&executable)?;
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

    update_inner(&bootstrap, &install_dir, &executable).await
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

async fn update_inner(
    bootstrap: &Path,
    install_dir: &Path,
    executable: &Path,
) -> Result<(UpdateReceipt, String)> {
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

    Ok((
        UpdateReceipt {
            path: executable.display().to_string(),
            version: String::from_utf8_lossy(&version.stdout).trim().to_string(),
        },
        String::from_utf8_lossy(&install.stdout).trim().to_string(),
    ))
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
