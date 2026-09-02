//! Secret storage with three modes (decision §19.8).
//!
//! * `Keyring` — OS keychain / Secret Service on Linux. On macOS this is a
//!   0600 encrypted file in the state directory, not the login keychain:
//!   unsigned `cargo` binaries change code hash every rebuild, so Keychain
//!   would prompt on every Resources refresh.
//! * `File` — AES-256-GCM envelope in the state directory, key derived from a
//!   passphrase with Argon2id. For headless servers.
//! * `None` — restricted mode: nothing is persisted. Passwords are returned
//!   once at creation time and then unrecoverable.
//!
//! A secret is addressed by a stable *reference* string such as
//! `engine:<id>` or `database:<id>` stored in SQLite; the value never is.

use crate::core::config::{harden_file, SecretMode};
use crate::core::error::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SERVICE: &str = "local-infra";

pub fn engine_ref(engine_id: &str) -> String {
    format!("engine:{engine_id}")
}

pub fn database_ref(database_id: &str) -> String {
    format!("database:{database_id}")
}

pub fn bucket_ref(bucket_id: &str) -> String {
    format!("bucket:{bucket_id}")
}

#[derive(Debug, Serialize, Deserialize)]
struct Vault {
    version: u32,
    /// Argon2id salt, hex.
    salt: String,
    /// AES-GCM ciphertext of [`VERIFIER_PLAIN`]. Present even with zero
    /// secret entries so an empty vault still authenticates the passphrase.
    verifier: String,
    /// `reference -> hex(nonce):hex(ciphertext)`
    entries: BTreeMap<String, String>,
}

/// Constant sealed under the derived key. Never a user secret.
const VERIFIER_PLAIN: &[u8] = b"local-infra-vault-v1";

pub struct SecretStore {
    mode: SecretMode,
    /// Present only in `File` mode.
    file: Option<Mutex<FileVault>>,
    /// Values produced this process in `None` mode, so a single command can
    /// still print the URL it just created.
    ephemeral: Mutex<BTreeMap<String, String>>,
}

struct FileVault {
    path: PathBuf,
    key: Key<Aes256Gcm>,
    vault: Vault,
}

impl SecretStore {
    /// Open the configured backend, degrading to restricted mode with a warning
    /// when the keyring is unreachable (headless servers, PRD §10).
    pub fn open(mode: SecretMode, state_dir: &Path) -> (Self, Option<String>) {
        let passphrase = if mode == SecretMode::File {
            match read_passphrase() {
                Ok(p) => Some(p),
                Err(e) => {
                    return (
                        Self::restricted(),
                        Some(format!(
                        "암호화 파일 저장소를 열지 못해 비밀번호 미저장 모드로 동작합니다 ({e})."
                    )),
                    )
                }
            }
        } else {
            None
        };
        Self::open_with_passphrase(mode, state_dir, passphrase.as_deref())
    }

    /// Same as [`SecretStore::open`] but with the file-mode passphrase supplied
    /// by the caller instead of read from the environment or the terminal.
    pub fn open_with_passphrase(
        mode: SecretMode,
        state_dir: &Path,
        passphrase: Option<&str>,
    ) -> (Self, Option<String>) {
        match mode {
            SecretMode::None => (Self::restricted(), None),
            SecretMode::Keyring => {
                #[cfg(target_os = "macos")]
                let opened = open_macos_vault(state_dir);
                #[cfg(not(target_os = "macos"))]
                let opened = match keyring_probe() {
                    Ok(()) => (
                        Self {
                            mode: SecretMode::Keyring,
                            file: None,
                            ephemeral: Mutex::new(BTreeMap::new()),
                        },
                        None,
                    ),
                    Err(why) => (
                        Self::restricted(),
                        Some(format!(
                            "OS 키체인을 사용할 수 없어 비밀번호 미저장 모드로 동작합니다 ({why}). \
                             `secrets.mode = \"file\"`로 전환하면 암호화 파일에 저장할 수 있습니다."
                        )),
                    ),
                };
                opened
            }

            SecretMode::File => match FileVault::open(state_dir, passphrase.unwrap_or_default()) {
                Ok(vault) => (
                    Self {
                        mode: SecretMode::File,
                        file: Some(Mutex::new(vault)),
                        ephemeral: Mutex::new(BTreeMap::new()),
                    },
                    None,
                ),
                Err(e) => (
                    Self::restricted(),
                    Some(format!(
                        "암호화 파일 저장소를 열지 못해 비밀번호 미저장 모드로 동작합니다 ({e})."
                    )),
                ),
            },
        }
    }

    pub fn restricted() -> Self {
        Self {
            mode: SecretMode::None,
            file: None,
            ephemeral: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn mode(&self) -> SecretMode {
        self.mode
    }

    /// True when a stored password can be read back in a later process.
    pub fn persistent(&self) -> bool {
        self.mode != SecretMode::None
    }

    pub fn set(&self, reference: &str, secret: &str) -> Result<()> {
        self.remember(reference, secret);
        if let Some(file) = &self.file {
            return file.lock().expect("vault poisoned").set(reference, secret);
        }
        match self.mode {
            SecretMode::None => Ok(()),
            SecretMode::Keyring => keyring_entry(reference)?
                .set_password(secret)
                .map_err(|e| keyring_error("비밀번호 저장", e)),
            SecretMode::File => unreachable!("file mode always has a vault"),
        }
    }

    pub fn get(&self, reference: &str) -> Result<Option<String>> {
        if let Some(v) = self
            .ephemeral
            .lock()
            .expect("secret cache poisoned")
            .get(reference)
        {
            return Ok(Some(v.clone()));
        }
        if let Some(file) = &self.file {
            return file.lock().expect("vault poisoned").get(reference);
        }
        match self.mode {
            SecretMode::None => Ok(None),
            SecretMode::Keyring => match keyring_entry(reference)?.get_password() {
                Ok(v) => {
                    self.remember(reference, &v);
                    Ok(Some(v))
                }
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(keyring_error("비밀번호 조회", e)),
            },
            SecretMode::File => unreachable!("file mode always has a vault"),
        }
    }

    fn remember(&self, reference: &str, secret: &str) {
        self.ephemeral
            .lock()
            .expect("secret cache poisoned")
            .insert(reference.to_string(), secret.to_string());
    }

    pub fn delete(&self, reference: &str) -> Result<()> {
        self.ephemeral
            .lock()
            .expect("secret cache poisoned")
            .remove(reference);
        if let Some(file) = &self.file {
            return file.lock().expect("vault poisoned").delete(reference);
        }
        match self.mode {
            SecretMode::None => Ok(()),
            SecretMode::Keyring => match keyring_entry(reference)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(keyring_error("비밀번호 삭제", e)),
            },
            SecretMode::File => unreachable!("file mode always has a vault"),
        }
    }
}

fn keyring_entry(reference: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, reference).map_err(|e| keyring_error("키체인 항목 생성", e))
}

#[cfg(not(target_os = "macos"))]
fn keyring_probe() -> std::result::Result<(), String> {
    match keyring::Entry::new(SERVICE, "__probe__") {
        Ok(entry) => match entry.get_password() {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    }
}

fn keyring_error(what: &str, e: keyring::Error) -> Error {
    Error::failed(
        format!("{what}에 실패했습니다"),
        e.to_string(),
        "OS 키체인 접근을 허용하거나 `secrets.mode`를 `file` 또는 `none`으로 변경하세요.",
    )
}

#[cfg(target_os = "macos")]
fn open_macos_vault(state_dir: &Path) -> (SecretStore, Option<String>) {
    match macos_machine_vault(state_dir) {
        Ok(vault) => (
            SecretStore {
                mode: SecretMode::Keyring,
                file: Some(Mutex::new(vault)),
                ephemeral: Mutex::new(BTreeMap::new()),
            },
            None,
        ),
        Err(e) => (
            SecretStore::restricted(),
            Some(format!(
                "로컬 비밀 금고를 열지 못해 비밀번호 미저장 모드로 동작합니다 ({e})."
            )),
        ),
    }
}

#[cfg(target_os = "macos")]
fn macos_machine_vault(state_dir: &Path) -> Result<FileVault> {
    let key = macos_machine_key(state_dir)?;
    FileVault::open_path(state_dir.join("keychain.vault"), &key)
}

#[cfg(target_os = "macos")]
fn macos_machine_key(state_dir: &Path) -> Result<String> {
    let path = state_dir.join("machine.key");
    if path.exists() {
        let key = std::fs::read_to_string(&path)?;
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err(Error::failed(
                "로컬 비밀 금고 키가 비어 있습니다",
                path.display().to_string(),
                "`machine.key`를 삭제한 뒤 앱을 다시 시작하세요.",
            ));
        }
        return Ok(key);
    }
    let key = hex(&random_bytes(32));
    std::fs::write(&path, format!("{key}\n"))?;
    harden_file(&path)?;
    Ok(key)
}

impl FileVault {
    fn open(state_dir: &Path, passphrase: &str) -> Result<Self> {
        Self::open_path(state_dir.join("secrets.vault"), passphrase)
    }

    fn open_path(path: PathBuf, passphrase: &str) -> Result<Self> {
        if passphrase.is_empty() {
            return Err(Error::Usage("암호화 저장소 암호가 비어 있습니다.".into()));
        }

        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let vault: Vault = serde_json::from_str(&text)?;
            let key = derive_key(passphrase, &vault.salt)?;
            authenticate(&key, &vault.verifier)?;
            return Ok(Self { path, key, vault });
        }
        let salt = hex(&random_bytes(16));
        let key = derive_key(passphrase, &salt)?;
        let vault = Vault {
            version: 2,
            salt,
            verifier: seal(&key, VERIFIER_PLAIN)?,
            entries: BTreeMap::new(),
        };
        let me = Self { path, key, vault };
        me.flush()?;
        Ok(me)
    }

    fn set(&mut self, reference: &str, secret: &str) -> Result<()> {
        self.vault
            .entries
            .insert(reference.to_string(), seal(&self.key, secret.as_bytes())?);
        self.flush()
    }

    fn get(&self, reference: &str) -> Result<Option<String>> {
        let Some(blob) = self.vault.entries.get(reference) else {
            return Ok(None);
        };
        let plain = open(&self.key, blob)?;
        Ok(Some(String::from_utf8_lossy(&plain).into_owned()))
    }

    fn delete(&mut self, reference: &str) -> Result<()> {
        self.vault.entries.remove(reference);
        self.flush()
    }

    fn flush(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.vault)?;
        std::fs::write(&self.path, text)?;
        harden_file(&self.path)
    }
}

fn authenticate(key: &Key<Aes256Gcm>, verifier: &str) -> Result<()> {
    if verifier.is_empty() {
        return Err(Error::Usage(
            "암호화 저장소가 손상되었습니다. 암호 검증자가 없습니다.".into(),
        ));
    }
    let plain = open(key, verifier).map_err(|_| {
        Error::Usage(
            "암호화 저장소 암호가 올바르지 않습니다. `LINF_PASSPHRASE`를 확인하세요.".into(),
        )
    })?;
    if plain != VERIFIER_PLAIN {
        return Err(Error::Usage(
            "암호화 저장소 암호가 올바르지 않습니다. `LINF_PASSPHRASE`를 확인하세요.".into(),
        ));
    }
    Ok(())
}

fn seal(key: &Key<Aes256Gcm>, plain: &[u8]) -> Result<String> {
    let cipher = Aes256Gcm::new(key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher.encrypt(&nonce, plain).map_err(|_| {
        Error::failed(
            "비밀 값 암호화 실패",
            "AES-GCM 암호화 오류",
            "다시 시도하세요.",
        )
    })?;
    Ok(format!("{}:{}", hex(&nonce), hex(&ct)))
}

fn open(key: &Key<Aes256Gcm>, blob: &str) -> Result<Vec<u8>> {
    let (nonce_hex, ct_hex) = blob.split_once(':').ok_or_else(|| {
        Error::failed(
            "비밀 저장소 손상",
            "항목 형식이 올바르지 않습니다.",
            "해당 항목을 다시 생성하세요.",
        )
    })?;
    let nonce_bytes = unhex(nonce_hex)?;
    let ct = unhex(ct_hex)?;
    let cipher = Aes256Gcm::new(key);
    cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ct.as_ref())
        .map_err(|_| {
            Error::failed(
                "비밀 값 복호화 실패",
                "암호가 다르거나 파일이 손상되었습니다.",
                "`LINF_PASSPHRASE`를 확인하세요.",
            )
        })
}
fn read_passphrase() -> Result<String> {
    if let Ok(v) = std::env::var("LINF_PASSPHRASE") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return rpassword::prompt_password("local-infra 암호화 저장소 암호: ").map_err(Into::into);
    }
    Err(Error::Usage(
        "암호화 저장소 암호가 필요합니다. `LINF_PASSPHRASE` 환경변수를 설정하세요.".into(),
    ))
}

fn derive_key(passphrase: &str, salt_hex: &str) -> Result<Key<Aes256Gcm>> {
    use argon2::Argon2;
    let salt = unhex(salt_hex)?;
    let mut out = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), &salt, &mut out)
        .map_err(|e| Error::failed("키 유도 실패", e.to_string(), "다른 암호를 사용해 보세요."))?;
    Ok(*Key::<Aes256Gcm>::from_slice(&out))
}

fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(Error::failed(
            "비밀 저장소 손상",
            "16진 문자열 길이가 홀수입니다.",
            "해당 항목을 다시 생성하세요.",
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| {
                Error::failed(
                    "비밀 저장소 손상",
                    "16진 문자열을 해석할 수 없습니다.",
                    "해당 항목을 다시 생성하세요.",
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_mode_persists_nothing_but_serves_the_current_process() {
        let s = SecretStore::restricted();
        assert!(!s.persistent());
        s.set("database:1", "hunter2").unwrap();
        assert_eq!(s.get("database:1").unwrap().as_deref(), Some("hunter2"));

        let fresh = SecretStore::restricted();
        assert_eq!(fresh.get("database:1").unwrap(), None);
    }

    fn open_file(dir: &Path, passphrase: &str) -> (SecretStore, Option<String>) {
        SecretStore::open_with_passphrase(SecretMode::File, dir, Some(passphrase))
    }

    #[test]
    fn file_vault_round_trips_across_processes() {
        let dir = tempfile::tempdir().unwrap();
        let pw = "correct horse battery staple";

        let (a, warn) = open_file(dir.path(), pw);
        assert!(warn.is_none(), "{warn:?}");
        a.set("database:1", "s3cret").unwrap();
        drop(a);

        let (b, _) = open_file(dir.path(), pw);
        assert_eq!(b.get("database:1").unwrap().as_deref(), Some("s3cret"));
        b.delete("database:1").unwrap();
        drop(b);

        let (c, _) = open_file(dir.path(), pw);
        assert_eq!(c.get("database:1").unwrap(), None);
    }

    #[test]
    fn file_vault_is_never_world_readable_and_stores_no_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _) = open_file(dir.path(), "pw");
        s.set("engine:1", "PLAINTEXT_MARKER").unwrap();
        let raw = std::fs::read_to_string(dir.path().join("secrets.vault")).unwrap();
        assert!(!raw.contains("PLAINTEXT_MARKER"));
        assert!(
            !raw.contains("local-infra-vault-v1"),
            "the passphrase verifier must never be stored in plaintext"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("secrets.vault"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn wrong_passphrase_is_rejected_instead_of_forking_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _) = open_file(dir.path(), "right");
        s.set("engine:1", "v").unwrap();
        drop(s);

        let (fallback, warn) = open_file(dir.path(), "wrong");
        assert_eq!(fallback.mode(), SecretMode::None);
        assert!(warn.unwrap().contains("암호화 파일 저장소를 열지 못해"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_keyring_mode_uses_a_local_vault_not_the_os_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let (a, warn) = SecretStore::open(SecretMode::Keyring, dir.path());
        assert!(warn.is_none(), "{warn:?}");
        assert!(a.persistent());
        a.set("database:1", "s3cret").unwrap();
        drop(a);

        let (b, warn) = SecretStore::open(SecretMode::Keyring, dir.path());
        assert!(warn.is_none());
        assert_eq!(b.get("database:1").unwrap().as_deref(), Some("s3cret"));
        assert!(dir.path().join("machine.key").exists());
        assert!(dir.path().join("keychain.vault").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("machine.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn an_empty_vault_still_rejects_the_wrong_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _) = open_file(dir.path(), "right");
        s.set("engine:1", "v").unwrap();
        s.delete("engine:1").unwrap();
        drop(s);

        let (ok, warn) = open_file(dir.path(), "right");
        assert!(warn.is_none(), "{warn:?}");
        assert_eq!(ok.mode(), SecretMode::File);
        assert_eq!(ok.get("engine:1").unwrap(), None);

        let (fallback, warn) = open_file(dir.path(), "wrong");
        assert_eq!(fallback.mode(), SecretMode::None);
        assert!(warn.unwrap().contains("암호화 파일 저장소를 열지 못해"));
    }

    #[test]
    fn a_fresh_empty_vault_authenticates_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (created, warn) = open_file(dir.path(), "right");
        assert!(warn.is_none(), "{warn:?}");
        assert_eq!(created.mode(), SecretMode::File);
        drop(created);

        let (ok, warn) = open_file(dir.path(), "right");
        assert!(warn.is_none());
        assert_eq!(ok.mode(), SecretMode::File);

        let (wrong, warn) = open_file(dir.path(), "wrong");
        assert_eq!(wrong.mode(), SecretMode::None);
        assert!(warn.is_some());
    }

    #[test]
    fn empty_passphrase_never_opens_a_vault() {
        let dir = tempfile::tempdir().unwrap();
        let (s, warn) = open_file(dir.path(), "");
        assert_eq!(s.mode(), SecretMode::None);
        assert!(warn.is_some());
    }
}
