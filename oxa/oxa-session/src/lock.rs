//! Session file lock: one writer per session file.
//!
//! The writer holds `flock(LOCK_EX)` on a sidecar file `<session>.jsonl.lock` for as long as the
//! session is open. The kernel drops the lock when the process dies, so a crash never leaves a
//! stale block (the sidecar file may stay; it is only a name for the lock and holds the pid for
//! the message). A second oxa that opens the same session is refused with a clear message; it
//! can read the file (picker, `--fork`), it just never writes. The append's own short `flock`
//! on the session file itself is separate and unaffected.
use oxa_proto::{codes, OxaError};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Error code of a refused open (the session is locked by another oxa).
pub const LOCKED: &str = "oxa_session_locked";

/// The held lock; dropping it releases the session.
#[derive(Debug)]
pub struct SessionLock {
    file: File,
    lock_path: PathBuf,
    session: PathBuf,
}

/// The sidecar path of a session file.
pub fn lock_path_of(session: &Path) -> PathBuf {
    let mut s = session.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// The pid recorded in the sidecar of `session` when that process is alive: a read-only probe
/// of "is this session open in some oxa right now". It never takes `flock` and never unlinks
/// the sidecar, so a probe cannot make a starting oxa see [`LOCKED`]. A missing, unreadable,
/// empty or stale (dead pid) sidecar is `None`; a recycled pid can briefly look alive.
pub fn lock_holder(session: &Path) -> Option<u32> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path_of(session))
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(32).read_to_string(&mut text).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    pid_alive(pid).then_some(pid)
}

fn pid_alive(pid: u32) -> bool {
    // A pid outside the signed range would be a process group for `kill`; it is never a holder.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 only checks that the process exists.
        let hit = unsafe { libc::kill(pid as libc::pid_t, 0) };
        hit == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

impl SessionLock {
    /// The session file this lock guards.
    pub fn session(&self) -> &Path {
        &self.session
    }

    /// Take the lock of `session` without waiting. `Err` with code [`LOCKED`] when another
    /// process holds it.
    pub fn acquire(session: &Path) -> Result<SessionLock, OxaError> {
        let lock_path = lock_path_of(session);
        let io_err = |e: &io::Error| OxaError::io(&format!("cannot lock {}", session.display()), e);
        for _ in 0..8 {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&lock_path)
                .map_err(|e| io_err(&e))?;
            // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    let mut text = String::new();
                    let _ = file.read_to_string(&mut text);
                    let who = match text.trim().parse::<u32>() {
                        Ok(pid) => format!(" (pid {pid})"),
                        Err(_) => String::new(),
                    };
                    return Err(OxaError::new(
                        LOCKED,
                        format!("session is open in another oxa{who}; use --fork to continue separately"),
                    )
                    .with_hint("use --fork <id> to copy the session, or close the other oxa"));
                }
                return Err(io_err(&err));
            }
            // The holder before us unlinks the sidecar while it still holds the lock; if that
            // happened between our open and our lock, we hold a dead inode. Look again.
            let same = match (file.metadata(), fs::symlink_metadata(&lock_path)) {
                (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
                _ => false,
            };
            if !same {
                continue;
            }
            file.set_len(0).map_err(|e| io_err(&e))?;
            file.rewind().map_err(|e| io_err(&e))?;
            file.write_all(std::process::id().to_string().as_bytes())
                .map_err(|e| io_err(&e))?;
            return Ok(SessionLock {
                file,
                lock_path,
                session: session.to_path_buf(),
            });
        }
        Err(OxaError::new(
            codes::INTERNAL,
            "cannot take the session lock (the lock file kept changing)",
        ))
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Unlink while still holding the lock; the fd closes (and unlocks) right after.
        let _ = fs::remove_file(&self.lock_path);
        let _ = &self.file;
    }
}
