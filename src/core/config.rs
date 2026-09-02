//! Filesystem layout and user configuration.

use crate::core::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every path the app owns. Resolved once and passed around in `Ctx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Durable state: SQLite database, PID files.
    pub state_dir: PathBuf,
    /// `config.toml`.
    pub config_dir: PathBuf,
    /// PID files for detached tunnels.
    pub run_dir: PathBuf,
    pub db_path: PathBuf,
    pub lock_path: PathBuf,
    pub config_path: PathBuf,
    /// Default backup destination when `--out` is not given.
    pub default_backup_dir: PathBuf,
}

impl Paths {
    /// `$LINF_STATE_DIR` wins (used by tests and by CI), otherwise the platform
    /// convention from PRD §13.
    pub fn resolve() -> Result<Self> {
        if let Some(root) = std::env::var_os("LINF_STATE_DIR") {
            return Ok(Self::rooted(Path::new(&root)));
        }
        let dirs = directories::ProjectDirs::from("", "", "local-infra").ok_or_else(|| {
            Error::failed(
                "상태 디렉터리를 결정할 수 없습니다",
                "홈 디렉터리를 찾지 못했습니다.",
                "`LINF_STATE_DIR` 환경변수로 상태 디렉터리를 지정하세요.",
            )
        })?;
        let state_dir = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_dir())
            .to_path_buf();
        let config_dir = dirs.config_dir().to_path_buf();
        Ok(Self {
            run_dir: state_dir.join("run"),
            db_path: state_dir.join("state.db"),
            lock_path: state_dir.join("instance.lock"),
            config_path: config_dir.join("config.toml"),
            default_backup_dir: state_dir.join("backups"),
            state_dir,
            config_dir,
        })
    }

    fn rooted(root: &Path) -> Self {
        Self {
            state_dir: root.to_path_buf(),
            config_dir: root.to_path_buf(),
            run_dir: root.join("run"),
            db_path: root.join("state.db"),
            lock_path: root.join("instance.lock"),
            config_path: root.join("config.toml"),
            default_backup_dir: root.join("backups"),
        }
    }

    /// Create the directories with `0700` so the SQLite file and PID files are
    /// never world-readable (PRD §11.2).
    pub fn ensure(&self) -> Result<()> {
        for dir in [&self.state_dir, &self.config_dir, &self.run_dir] {
            std::fs::create_dir_all(dir)?;
            harden_dir(dir)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn harden_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(dir)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(dir, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Restrict a file to `0600`.
pub fn harden_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }
    }
    let _ = path;
    Ok(())
}

// ---------------------------------------------------------------------------
// config.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecretMode {
    /// OS keyring. Falls back to `None` at runtime when unavailable.
    #[default]
    Keyring,
    /// Passphrase-encrypted file in the state directory.
    File,
    /// Do not persist secrets at all (restricted mode, PRD §11.1).
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TunnelConfig {
    /// Leave detached tunnels running when the TUI exits (decision §19.7).
    pub keep_alive_on_exit: bool,
    /// First port considered when auto-assigning a local tunnel port.
    pub port_range_start: u16,
    /// How many ports to scan from `port_range_start`.
    pub port_range_span: u16,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            keep_alive_on_exit: true,
            port_range_start: 15432,
            port_range_span: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Allow OSC 52 clipboard writes (PRD §11.2 lets the user turn this off).
    pub osc52: bool,
    /// Replace spinners with static text (PRD §12.4).
    pub reduced_motion: bool,
    /// Force ASCII glyphs instead of box drawing / status symbols.
    pub ascii: bool,
    /// Clear the clipboard this many seconds after copying a secret. 0 = never.
    pub clipboard_clear_seconds: u64,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            osc52: true,
            reduced_motion: false,
            ascii: false,
            clipboard_clear_seconds: 45,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GeneralConfig {
    /// Overrides `Paths::default_backup_dir` when set.
    pub backup_dir: Option<String>,
    /// Image registry prefix, e.g. `docker.io/library`.
    pub image_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub tunnel: TunnelConfig,
    pub ui: UiConfig,
    pub secrets: SecretsConfig,
    /// `action name -> key`, overriding the built-in keymap (TUI-009).
    pub keymap: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SecretsConfig {
    pub mode: SecretMode,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| {
                Error::Usage(format!(
                    "설정 파일을 읽을 수 없습니다 ({}): {e}",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Honour `NO_COLOR` (PRD §12.4) — checked here so both surfaces agree.
    pub fn color_enabled(&self) -> bool {
        std::env::var_os("NO_COLOR").is_none()
    }

    pub fn unicode_enabled(&self) -> bool {
        if self.ui.ascii {
            return false;
        }
        match std::env::var("LANG").or_else(|_| std::env::var("LC_ALL")) {
            Ok(v) => v.to_ascii_lowercase().contains("utf"),
            Err(_) => false,
        }
    }

    pub fn backup_dir(&self, paths: &Paths) -> PathBuf {
        match &self.general.backup_dir {
            Some(dir) => expand_tilde(dir),
            None => paths.default_backup_dir.clone(),
        }
    }
}

pub fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_decisions() {
        let c = Config::default();
        assert!(c.tunnel.keep_alive_on_exit, "§19.7");
        assert_eq!(c.tunnel.port_range_start, 15432);
        assert_eq!(c.secrets.mode, SecretMode::Keyring);
        assert!(c.ui.osc52);
    }

    #[test]
    fn partial_config_files_keep_defaults_for_missing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[tunnel]\nkeep_alive_on_exit = false\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert!(!c.tunnel.keep_alive_on_exit);
        assert_eq!(c.tunnel.port_range_start, 15432);
        assert_eq!(c.secrets.mode, SecretMode::Keyring);
    }

    #[test]
    fn missing_config_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("nope.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn state_directories_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        paths.ensure().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&paths.run_dir)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
