//! The application context: everything a use case needs, resolved once.

use crate::core::config::{Config, Paths};
use crate::core::error::{Error, Result};
use crate::core::exec::Executor;
use crate::core::model::{Origin, Target};
use crate::core::secrets::SecretStore;
use crate::core::store::Store;
use fs4::fs_std::FileExt;
use std::fs::File;
use std::path::PathBuf;

pub struct Ctx {
    pub paths: Paths,
    pub config: Config,
    pub store: Store,
    pub secrets: SecretStore,
    pub origin: Origin,
    /// Non-fatal notices raised while opening (e.g. keyring unavailable). The
    /// CLI prints them to stderr, the TUI shows them as alerts.
    pub notices: Vec<String>,
    /// Held for the process lifetime; `None` when another instance holds it.
    lock: Option<File>,
    lock_holder: Option<i32>,
}

impl Ctx {
    pub fn open(origin: Origin) -> Result<Self> {
        let paths = Paths::resolve()?;
        paths.ensure()?;
        let config = Config::load(&paths.config_path)?;
        let store = Store::open(&paths.db_path)?;
        let (secrets, warning) = SecretStore::open(config.secrets.mode, &paths.state_dir);

        let mut notices = Vec::new();
        if let Some(w) = warning {
            notices.push(w);
        }

        let (lock, lock_holder) = acquire_lock(&paths.lock_path)?;
        if lock.is_none() {
            notices.push(match lock_holder {
                Some(pid) => format!(
                    "다른 local-infra 인스턴스(pid {pid})가 실행 중입니다. \
                     상태 변경은 한 번에 하나만 안전합니다."
                ),
                None => "다른 local-infra 인스턴스가 실행 중입니다.".to_string(),
            });
        }

        Ok(Self {
            paths,
            config,
            store,
            secrets,
            origin,
            notices,
            lock,
            lock_holder,
        })
    }

    /// True when this process owns the exclusive state lock (PRD §12.3).
    pub fn has_write_lock(&self) -> bool {
        self.lock.is_some()
    }

    /// Refuse a mutation when another instance owns the lock, instead of
    /// racing it.
    pub fn require_write_lock(&self) -> Result<()> {
        if self.has_write_lock() {
            return Ok(());
        }
        Err(Error::Refused(match self.lock_holder {
            Some(pid) => format!(
                "다른 local-infra 인스턴스(pid {pid})가 상태를 사용 중입니다. \
                 해당 인스턴스를 종료한 뒤 다시 시도하세요."
            ),
            None => "다른 local-infra 인스턴스가 상태를 사용 중입니다.".to_string(),
        }))
    }

    pub fn executor(&self, target: &Target) -> Result<Executor> {
        Executor::for_target(target)
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.config.backup_dir(&self.paths)
    }

    pub fn pid_file(&self, tunnel_id: &str) -> PathBuf {
        self.paths.run_dir.join(format!("tunnel-{tunnel_id}.pid"))
    }
}

/// `Ok((Some(file), None))` when the lock was taken, `Ok((None, Some(pid)))`
/// when a *live* other process holds it.
///
/// A failed `flock` alone is not proof of contention. Spawning a child
/// duplicates this process's descriptors until the child `exec`s, and the
/// duplicate keeps the lock alive for that window — and this process spawns
/// `docker` and `ssh` constantly. A stale lock file left by a killed process
/// has the same shape. So the recorded pid, not the descriptor, is the
/// authority: contention is only reported for a pid that is alive and is not
/// us.
fn acquire_lock(path: &std::path::Path) -> Result<(Option<File>, Option<i32>)> {
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    crate::core::config::harden_file(path)?;

    let me = std::process::id() as i32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        if try_lock(&file)? {
            claim(&file, me)?;
            return Ok((Some(file), None));
        }
        let recorded = read_pid(path);
        if let Some(pid) = contending_pid(recorded, me, process_alive) {
            return Ok((None, Some(pid)));
        }
        // Stale file, or our own descriptor duplicated into a child. Give the
        // duplicate a moment to disappear so we still end up holding a real
        // `flock`, then take ownership regardless.
        if std::time::Instant::now() >= deadline {
            claim(&file, me)?;
            return Ok((Some(file), None));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Who genuinely owns the lock, given the recorded pid. `None` means the lock
/// is free to take.
fn contending_pid(recorded: Option<i32>, me: i32, alive: impl Fn(i32) -> bool) -> Option<i32> {
    match recorded {
        Some(pid) if pid != me && alive(pid) => Some(pid),
        _ => None,
    }
}

fn read_pid(path: &std::path::Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
}

/// `kill(pid, 0)` answers "does this process exist and may we signal it".
/// `EPERM` still means it exists, which is what we are asking.
fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs error checking only and sends nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn claim(file: &File, pid: i32) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = file;
    f.set_len(0)?;
    f.seek(SeekFrom::Start(0))?;
    write!(f, "{pid}")?;
    f.flush()?;
    Ok(())
}

/// `flock(2)` on a busy multi-threaded process can be interrupted by a signal.
/// `EINTR` means "ask again", not "somebody else holds it" — treating it as
/// contention made the app claim a second instance was running when none was.
fn try_lock(file: &File) -> Result<bool> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..16 {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => last = Some(e),
            Err(e) if is_contended(&e) => return Ok(false),
            Err(e) => {
                return Err(Error::failed(
                    "상태 잠금 파일을 사용할 수 없습니다",
                    e.to_string(),
                    "상태 디렉터리 권한과 파일 시스템(잠금을 지원하지 않는 NFS 등)을 확인하세요.",
                ))
            }
        }
    }
    Err(Error::failed(
        "상태 잠금을 획득하지 못했습니다",
        last.map(|e| e.to_string())
            .unwrap_or_else(|| "잠금 시도가 반복해서 중단되었습니다.".into()),
        "잠시 후 다시 시도하세요.",
    ))
}

/// Contention is reported as `EWOULDBLOCK`/`EAGAIN`, which some platforms do
/// not map onto `ErrorKind::WouldBlock`.
fn is_contended(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
        || matches!(e.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pid that is alive and is not us is the only thing that counts as
    /// contention. Everything else is a lock we may take.
    #[test]
    fn only_a_live_foreign_pid_counts_as_another_instance() {
        let me = 4242;
        let alive = |_: i32| true;
        let dead = |_: i32| false;

        assert_eq!(contending_pid(Some(99), me, alive), Some(99));
        assert_eq!(
            contending_pid(Some(99), me, dead),
            None,
            "a killed instance leaves a stale lock, not a permanent one"
        );
        assert_eq!(
            contending_pid(Some(me), me, alive),
            None,
            "our own descriptor duplicated into a child is not another instance"
        );
        assert_eq!(
            contending_pid(None, me, alive),
            None,
            "an empty lock file is free"
        );
    }

    #[test]
    fn our_own_pid_is_alive_and_an_impossible_one_is_not() {
        assert!(process_alive(std::process::id() as i32));
        assert!(!process_alive(0));
        assert!(!process_alive(-1));
        assert!(!process_alive(i32::MAX));
    }

    #[test]
    fn acquiring_records_our_pid_and_survives_release_and_reacquire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.lock");

        let (first, holder) = acquire_lock(&path).unwrap();
        assert!(first.is_some());
        assert_eq!(holder, None);
        assert_eq!(read_pid(&path), Some(std::process::id() as i32));

        drop(first);
        let (again, holder) = acquire_lock(&path).unwrap();
        assert!(again.is_some(), "the lock is free once the holder exits");
        assert_eq!(holder, None);
    }

    #[test]
    fn a_stale_lock_file_from_a_dead_process_does_not_block_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.lock");
        // i32::MAX is not a live pid on any supported platform.
        std::fs::write(&path, i32::MAX.to_string()).unwrap();

        let (file, holder) = acquire_lock(&path).unwrap();
        assert!(file.is_some());
        assert_eq!(holder, None);
        assert_eq!(read_pid(&path), Some(std::process::id() as i32));
    }

    #[test]
    fn a_live_foreign_holder_is_reported_and_blocks_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.lock");
        // A real, live pid that is not us: our own parent.
        let other = unsafe { libc::getppid() };
        assert!(process_alive(other));
        std::fs::write(&path, other.to_string()).unwrap();
        // Hold the flock so the fast path cannot succeed.
        let held = File::options().read(true).write(true).open(&path).unwrap();
        assert!(try_lock(&held).unwrap());

        let (file, holder) = acquire_lock(&path).unwrap();
        assert!(file.is_none());
        assert_eq!(holder, Some(other));
    }
}
