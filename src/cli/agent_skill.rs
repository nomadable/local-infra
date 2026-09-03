//! Installation of the bundled Agent Skill.
//!
//! The binary owns the skill template with `include_str!`, so release archives
//! and `cargo install` expose the same post-install workflow as a source checkout.

use crate::core::error::{Error, Result};
use directories::UserDirs;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SKILL_NAME: &str = "local-infrastructure";
const SKILL_FILE: &str = "SKILL.md";
const CONTENT: &str = include_str!("../../skills/local-infra/SKILL.md");

pub const PROJECT_SKILL_ROOT: &str = ".agents/skills";
const GLOBAL_SKILL_ROOT: &str = ".agents/skills";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallReceipt {
    pub path: String,
    pub overwritten: bool,
}

/// Resolve a portable Agent Skills root. `.agents/skills` is the cross-client
/// convention at both project and user scope; agent-specific roots remain
/// available through `--dir`.
pub fn resolve_dir(dir: Option<PathBuf>, global: bool) -> Result<PathBuf> {
    if global && dir.is_some() {
        return Err(Error::Usage(
            "`--dir`과 `--global`은 함께 사용할 수 없습니다.".into(),
        ));
    }

    let home = UserDirs::new();
    resolve_dir_for_home(dir, global, home.as_ref().map(|dirs| dirs.home_dir()))
}

fn resolve_dir_for_home(
    dir: Option<PathBuf>,
    global: bool,
    home: Option<&Path>,
) -> Result<PathBuf> {
    if global {
        let home = home.ok_or_else(|| {
            Error::failed(
                "전역 Agent Skill 경로를 결정할 수 없습니다",
                "현재 사용자의 홈 디렉터리를 찾지 못했습니다.",
                "프로젝트 범위로 설치하거나 홈 디렉터리를 설정한 뒤 다시 실행하세요.",
            )
        })?;
        return Ok(global_dir(home));
    }

    Ok(dir.unwrap_or_else(|| PathBuf::from(PROJECT_SKILL_ROOT)))
}

fn global_dir(home: &Path) -> PathBuf {
    home.join(GLOBAL_SKILL_ROOT)
}
/// Copy the bundled portable Agent Skill into a selected skill root.
///
/// An existing non-file is never replaced. Replacing an existing `SKILL.md`
/// needs explicit `--force`, so updating the binary cannot silently alter an
/// agent's local instructions.
pub fn install(dir: &Path, force: bool) -> Result<InstallReceipt> {
    let path = dir.join(SKILL_NAME).join(SKILL_FILE);
    let overwritten = match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(Error::Conflict(format!(
                "Agent Skill 경로 `{}`가 일반 파일이 아닙니다. 안전을 위해 바꾸지 않습니다.",
                path.display()
            )));
        }
        Ok(_) if !force => {
            return Err(Error::Conflict(format!(
                "Agent Skill이 이미 `{}`에 있습니다. 내용을 바꾸려면 `linf skill install --force`를 사용하세요.",
                path.display()
            )));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(io_error(
                "Agent Skill 상태를 확인할 수 없습니다",
                &path,
                error,
            ))
        }
    };

    let parent = path.parent().expect("skill path always has a parent");
    fs::create_dir_all(parent)
        .map_err(|error| io_error("Agent Skill 폴더를 만들 수 없습니다", parent, error))?;

    let temporary = parent.join(format!(".{SKILL_FILE}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(CONTENT.as_bytes())?;
        file.sync_all()?;
        publish(&temporary, &path, force)
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        if !force && error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(Error::Conflict(format!(
                "Agent Skill이 이미 `{}`에 있습니다. 다른 프로세스가 설치했을 수 있습니다. 내용을 바꾸려면 `linf skill install --force`를 사용하세요.",
                path.display()
            )));
        }
        return Err(io_error("Agent Skill을 설치할 수 없습니다", &path, error));
    }

    Ok(InstallReceipt {
        path: path.display().to_string(),
        overwritten,
    })
}

/// Publish without clobbering a concurrently-created skill. A hard link in the
/// same directory creates the destination atomically on the supported platforms;
/// replacement remains an explicit `--force` operation.
fn publish(temporary: &Path, path: &Path, force: bool) -> std::io::Result<()> {
    if force {
        return fs::rename(temporary, path);
    }

    fs::hard_link(temporary, path)?;
    let _ = fs::remove_file(temporary);
    Ok(())
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> Error {
    Error::failed(
        action,
        format!("`{}`: {error}", path.display()),
        "경로의 쓰기 권한을 확인하거나 다른 `--dir`을 지정하세요.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_writes_the_bundled_skill_to_the_selected_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join(".claude/skills");

        let receipt = install(&root, false).unwrap();
        let installed = root.join(SKILL_NAME).join(SKILL_FILE);

        assert_eq!(receipt.path, installed.display().to_string());
        assert!(!receipt.overwritten);
        assert_eq!(fs::read_to_string(installed).unwrap(), CONTENT);
        assert!(CONTENT.starts_with("---\nname: local-infrastructure\n"));
    }

    #[test]
    fn install_requires_force_before_replacing_an_existing_skill() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("skills");
        install(&root, false).unwrap();

        let error = install(&root, false).unwrap_err();
        assert!(matches!(error, Error::Conflict(_)));

        let receipt = install(&root, true).unwrap();
        assert!(receipt.overwritten);
    }
    #[test]
    fn no_force_publish_never_replaces_a_concurrently_created_skill() {
        let temp = tempdir().unwrap();
        let temporary = temp.path().join(".SKILL.md.new");
        let installed = temp.path().join(SKILL_FILE);
        fs::write(&temporary, "new").unwrap();
        fs::write(&installed, "existing").unwrap();

        let error = publish(&temporary, &installed, false).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(installed).unwrap(), "existing");
    }

    #[test]
    fn default_scope_uses_the_portable_project_convention() {
        assert_eq!(
            resolve_dir(None, false).unwrap(),
            PathBuf::from(".agents/skills")
        );
    }

    #[test]
    fn global_scope_uses_the_portable_user_convention() {
        let home = Path::new("/test/home");
        assert_eq!(
            resolve_dir_for_home(None, true, Some(home)).unwrap(),
            home.join(".agents/skills")
        );
    }

    #[test]
    fn custom_and_global_scopes_are_mutually_exclusive() {
        let error = resolve_dir(Some(PathBuf::from(".claude/skills")), true).unwrap_err();
        assert!(matches!(error, Error::Usage(_)));
    }
}
