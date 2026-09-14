//! Installation of the bundled Agent Skill.
//!
//! The binary owns the skill files with `include_str!`, so release archives
//! and `cargo install` expose the same post-install workflow as a source
//! checkout. The bundle is a directory: `SKILL.md` carries the always-loaded
//! instructions and `references/` the progressive-disclosure detail an agent
//! opens on demand.

use crate::core::error::{Error, Result};
use directories::UserDirs;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SKILL_NAME: &str = "local-infrastructure";
const SKILL_FILE: &str = "SKILL.md";

/// Every file of the bundle, `SKILL.md` first. Paths are relative to the
/// installed skill directory.
const BUNDLE: &[(&str, &str)] = &[
    (
        SKILL_FILE,
        include_str!("../../skills/local-infra/SKILL.md"),
    ),
    (
        "references/commands.md",
        include_str!("../../skills/local-infra/references/commands.md"),
    ),
    (
        "references/workflows.md",
        include_str!("../../skills/local-infra/references/workflows.md"),
    ),
];

/// The always-loaded instructions, for tests and diagnostics.
pub const CONTENT: &str = BUNDLE[0].1;

pub const PROJECT_SKILL_ROOT: &str = ".agents/skills";
const GLOBAL_SKILL_ROOT: &str = ".agents/skills";

/// Coding agents with a skill directory of their own. The portable
/// `.agents/skills` root stays the default because several clients scan it,
/// but a user who runs one specific agent wants its native path without
/// looking it up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Agent {
    /// Claude Code: `.claude/skills` / `~/.claude/skills`
    Claude,
    /// OpenAI Codex: `.agents/skills` / `~/.agents/skills`
    Codex,
    /// Cursor: `.cursor/skills` / `~/.cursor/skills`
    Cursor,
    /// Gemini CLI: `.gemini/skills` / `~/.gemini/skills`
    Gemini,
    /// GitHub Copilot: `.github/skills` / `~/.copilot/skills`
    Copilot,
}

impl Agent {
    pub fn project_root(self) -> &'static str {
        match self {
            Agent::Claude => ".claude/skills",
            Agent::Codex => ".agents/skills",
            Agent::Cursor => ".cursor/skills",
            Agent::Gemini => ".gemini/skills",
            Agent::Copilot => ".github/skills",
        }
    }

    pub fn global_root(self) -> &'static str {
        match self {
            Agent::Claude => ".claude/skills",
            Agent::Codex => ".agents/skills",
            Agent::Cursor => ".cursor/skills",
            Agent::Gemini => ".gemini/skills",
            Agent::Copilot => ".copilot/skills",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallReceipt {
    /// The installed `SKILL.md`.
    pub path: String,
    /// Every installed file, `SKILL.md` first.
    pub files: Vec<String>,
    pub overwritten: bool,
}

/// Resolve the skill root. `.agents/skills` is the cross-client convention at
/// both project and user scope; `--agent` selects a client's native root and
/// `--dir` names any other one.
pub fn resolve_dir(dir: Option<PathBuf>, agent: Option<Agent>, global: bool) -> Result<PathBuf> {
    if dir.is_some() && (global || agent.is_some()) {
        return Err(Error::Usage(
            "`--dir`은 `--global`, `--agent`와 함께 사용할 수 없습니다.".into(),
        ));
    }

    let home = UserDirs::new();
    resolve_dir_for_home(
        dir,
        agent,
        global,
        home.as_ref().map(|dirs| dirs.home_dir()),
    )
}

fn resolve_dir_for_home(
    dir: Option<PathBuf>,
    agent: Option<Agent>,
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
        return Ok(home.join(agent.map_or(GLOBAL_SKILL_ROOT, Agent::global_root)));
    }

    if let Some(agent) = agent {
        return Ok(PathBuf::from(agent.project_root()));
    }
    Ok(dir.unwrap_or_else(|| PathBuf::from(PROJECT_SKILL_ROOT)))
}

/// Copy the bundled portable Agent Skill into a selected skill root.
///
/// An existing non-file at any bundle path is never replaced. Replacing an
/// existing bundle file, `SKILL.md` or a reference, needs explicit `--force`,
/// so updating the binary cannot silently alter an agent's local
/// instructions. Reference files are written first and the `SKILL.md` last,
/// so a skill an agent can see is always complete.
pub fn install(dir: &Path, force: bool) -> Result<InstallReceipt> {
    let skill_dir = dir.join(SKILL_NAME);
    let path = skill_dir.join(SKILL_FILE);
    let overwritten = existing_regular_file(&path)?;
    if overwritten && !force {
        return Err(Error::Conflict(format!(
            "Agent Skill이 이미 `{}`에 있습니다. 내용을 바꾸려면 `linf skill install --force`를 사용하세요.",
            path.display()
        )));
    }

    // A reference file is an instruction too: a plain install never replaces
    // one, even when `SKILL.md` itself is missing after a partial install, and
    // a symlink or directory at any bundle path is never replaced at all.
    for (relative, _) in BUNDLE.iter().skip(1) {
        let target = skill_dir.join(relative);
        if existing_regular_file(&target)? && !force {
            return Err(Error::Conflict(format!(
                "Agent Skill 파일이 이미 `{}`에 있습니다. 번들 내용으로 교체하려면 `linf skill install --force`를 사용하세요.",
                target.display()
            )));
        }
    }

    let mut files = Vec::with_capacity(BUNDLE.len());
    for (relative, content) in BUNDLE.iter().skip(1) {
        let target = skill_dir.join(relative);
        write_file(&target, content, force)?;
        files.push(target.display().to_string());
    }
    write_file(&path, CONTENT, force)?;
    files.insert(0, path.display().to_string());

    Ok(InstallReceipt {
        path: path.display().to_string(),
        files,
        overwritten,
    })
}

/// `Ok(true)` when a regular file exists at `path`, `Ok(false)` when nothing
/// does. Anything else (symlink, directory, socket) is refused: replacing it
/// could redirect an agent's instructions somewhere the user did not choose.
fn existing_regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(true),
        Ok(_) => Err(Error::Conflict(format!(
            "Agent Skill 경로 `{}`가 일반 파일이 아닙니다. 안전을 위해 바꾸지 않습니다.",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(
            "Agent Skill 상태를 확인할 수 없습니다",
            path,
            error,
        )),
    }
}

/// Write `content` to `path` through a same-directory temporary file. With
/// `replace` the destination is renamed over; without it the destination is
/// created atomically and an existing file is a conflict.
fn write_file(path: &Path, content: &str, replace: bool) -> Result<()> {
    let parent = path.parent().expect("bundle paths always have a parent");
    fs::create_dir_all(parent)
        .map_err(|error| io_error("Agent Skill 폴더를 만들 수 없습니다", parent, error))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("bundle file names are utf-8");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        publish(&temporary, path, replace)
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        if !replace && error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(Error::Conflict(format!(
                "Agent Skill이 이미 `{}`에 있습니다. 다른 프로세스가 설치했을 수 있습니다. 내용을 바꾸려면 `linf skill install --force`를 사용하세요.",
                path.display()
            )));
        }
        return Err(io_error("Agent Skill을 설치할 수 없습니다", path, error));
    }
    Ok(())
}

/// Publish without clobbering a concurrently-created skill. A hard link in the
/// same directory creates the destination atomically on the supported platforms;
/// replacement remains an explicit `--force` operation.
fn publish(temporary: &Path, path: &Path, force: bool) -> std::io::Result<()> {
    if force {
        return fs::rename(temporary, path);
    }

    match fs::hard_link(temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(temporary);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(error),
        // Filesystems without hard links (some network and FUSE mounts):
        // `create_new` keeps the same no-clobber guarantee.
        Err(_) => {
            let content = fs::read(temporary)?;
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            file.write_all(&content)?;
            file.sync_all()?;
            let _ = fs::remove_file(temporary);
            Ok(())
        }
    }
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
    fn install_writes_the_whole_bundle_to_the_selected_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join(".claude/skills");

        let receipt = install(&root, false).unwrap();
        let installed = root.join(SKILL_NAME).join(SKILL_FILE);

        assert_eq!(receipt.path, installed.display().to_string());
        assert!(!receipt.overwritten);
        assert_eq!(receipt.files.len(), BUNDLE.len());
        assert_eq!(receipt.files[0], receipt.path);
        for (relative, content) in BUNDLE {
            let file = root.join(SKILL_NAME).join(relative);
            assert_eq!(fs::read_to_string(&file).unwrap(), *content, "{relative}");
        }
        assert!(CONTENT.starts_with("---\nname: local-infrastructure\n"));
    }

    #[test]
    fn the_bundle_links_only_to_files_it_ships() {
        for (relative, content) in BUNDLE {
            for link in content
                .split("](")
                .skip(1)
                .filter_map(|rest| rest.split(')').next())
                .filter(|target| target.ends_with(".md"))
            {
                assert!(
                    BUNDLE.iter().any(|(name, _)| *name == link),
                    "{relative} links to `{link}`, which the bundle does not ship"
                );
            }
        }
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
    fn force_refreshes_stale_reference_files_too() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("skills");
        install(&root, false).unwrap();
        let reference = root.join(SKILL_NAME).join("references/commands.md");
        fs::write(&reference, "stale").unwrap();

        install(&root, true).unwrap();

        assert_eq!(fs::read_to_string(reference).unwrap(), BUNDLE[1].1);
    }

    #[test]
    fn a_plain_install_refuses_to_replace_a_leftover_reference_file() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("skills");
        let reference = root.join(SKILL_NAME).join("references/commands.md");
        fs::create_dir_all(reference.parent().unwrap()).unwrap();
        fs::write(&reference, "edited by the user").unwrap();

        let error = install(&root, false).unwrap_err();

        assert!(matches!(error, Error::Conflict(_)), "{error:?}");
        assert_eq!(
            fs::read_to_string(&reference).unwrap(),
            "edited by the user"
        );
        assert!(!root.join(SKILL_NAME).join(SKILL_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_reference_file_is_never_replaced_even_with_force() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("skills");
        let reference = root.join(SKILL_NAME).join("references/commands.md");
        fs::create_dir_all(reference.parent().unwrap()).unwrap();
        let elsewhere = temp.path().join("elsewhere.md");
        fs::write(&elsewhere, "user content").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &reference).unwrap();

        let error = install(&root, true).unwrap_err();

        assert!(matches!(error, Error::Conflict(_)), "{error:?}");
        assert!(fs::symlink_metadata(&reference)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "user content");
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
            resolve_dir(None, None, false).unwrap(),
            PathBuf::from(".agents/skills")
        );
    }

    #[test]
    fn global_scope_uses_the_portable_user_convention() {
        let home = Path::new("/test/home");
        assert_eq!(
            resolve_dir_for_home(None, None, true, Some(home)).unwrap(),
            home.join(".agents/skills")
        );
    }

    #[test]
    fn agent_presets_select_the_native_roots_at_both_scopes() {
        let home = Path::new("/test/home");
        assert_eq!(
            resolve_dir_for_home(None, Some(Agent::Claude), false, Some(home)).unwrap(),
            PathBuf::from(".claude/skills")
        );
        assert_eq!(
            resolve_dir_for_home(None, Some(Agent::Claude), true, Some(home)).unwrap(),
            home.join(".claude/skills")
        );
        assert_eq!(
            resolve_dir_for_home(None, Some(Agent::Copilot), false, Some(home)).unwrap(),
            PathBuf::from(".github/skills")
        );
        assert_eq!(
            resolve_dir_for_home(None, Some(Agent::Copilot), true, Some(home)).unwrap(),
            home.join(".copilot/skills")
        );
        assert_eq!(
            resolve_dir_for_home(None, Some(Agent::Codex), false, Some(home)).unwrap(),
            PathBuf::from(PROJECT_SKILL_ROOT)
        );
    }

    #[test]
    fn custom_dir_is_exclusive_with_global_and_agent() {
        let error = resolve_dir(Some(PathBuf::from(".claude/skills")), None, true).unwrap_err();
        assert!(matches!(error, Error::Usage(_)));
        let error = resolve_dir(Some(PathBuf::from("x")), Some(Agent::Cursor), false).unwrap_err();
        assert!(matches!(error, Error::Usage(_)));
    }
}
