//! #457: the session directory. One host directory per oxa session, mounted at `/tmp` inside
//! every sandboxed command of that session (instead of a fresh tmpfs per command), so scratch
//! work survives from one bash call to the next. `TMPDIR=/tmp` and `XDG_CACHE_HOME=/tmp/.cache`
//! point into it (`Layout::build`), which gives every XDG tool (gh, pip, sccache, mise, ...) a
//! writable cache home without a bind per tool.
//!
//! The user's cache home ([`user_cache_home`]: `$XDG_CACHE_HOME` when set to an absolute path,
//! else `~/.cache`) keeps its shared caches: each entry of it that the layout binds read-write
//! (directly or through a bound parent) is linked from `/tmp/.cache/<name>`, so it stays the
//! cache of the user's own builds. Everything else a tool caches goes to the session.
//!
//! It lives at `~/.cache/oxa/scratch/<pid>-<n>` (mode 0700, never under the agent directory).
//! Each session gets its own (`Bwrap::session`, when a session starts, at `/new`, `/resume`,
//! fork and clone); it is deleted when that session ends: when it is replaced by another one
//! or oxa exits. A directory left by an oxa that was killed is removed when a later oxa
//! creates its next one. A directory records its owner (`.oxa-owner`: the pid and the process
//! start time), so the directory of a dead oxa whose pid was reused is removed too (#486).
//!
//! The size of a session directory is reported, never limited: [`Scratch::warning`] says so
//! once the owner-set `oxa.sandbox.scratchWarnSize` is exceeded.
use oxa_proto::CancelToken;
use std::ffi::CStr;
use std::fs::File;
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The default of `oxa.sandbox.scratchWarnSize`.
pub const DEFAULT_SCRATCH_WARN: u64 = 20 << 30;

/// How long a measured size is reused before the directory is walked again.
const USAGE_TTL: Duration = Duration::from_secs(60);
// The measurement is advisory: return the bytes seen so far when a budget runs out.
const USAGE_TIME: Duration = Duration::from_millis(50);
const USAGE_ENTRIES: usize = 32_768;
const USAGE_OPEN_DIRS: usize = 64;
const USAGE_DIR_FLAGS: libc::c_int = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// `oxa.sandbox.scratchWarnSize`: `off` = no warning, else a size such as `20GiB` or `512 MiB`
/// (a bare number is bytes; K, M, G, T with an optional `iB` or `B`, powers of 1024).
pub fn parse_scratch_warn(raw: &str) -> Result<Option<u64>, String> {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("{raw:?} is not a size"))?;
    let unit = unit.trim().to_ascii_lowercase();
    let unit = unit
        .strip_suffix("ib")
        .or_else(|| unit.strip_suffix('b'))
        .unwrap_or(&unit);
    let mult: f64 = match unit {
        "" => 1.0,
        "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        "t" => 1024.0_f64.powi(4),
        _ => return Err(format!("{raw:?} is not a size")),
    };
    Ok(Some((n * mult) as u64))
}

/// `1.5 GiB`.
fn human_size(n: u64) -> String {
    let mut v = n as f64;
    let mut unit = "B";
    for u in ["KiB", "MiB", "GiB", "TiB"] {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        unit = u;
    }
    if unit == "B" {
        format!("{n} B")
    } else {
        format!("{v:.1} {unit}")
    }
}

/// Field 22 of `/proc/<pid>/stat`: when the process started (clock ticks after boot).
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may hold spaces and parentheses: count after the last `)`.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The most a `.oxa-owner` record can hold: two numbers and a newline.
const OWNER_RECORD_MAX: u64 = 64;

/// The pid and start time a directory records, when it has the file.
///
/// `.oxa-owner` sits in the session directory, which is bound read-write at `/tmp` inside the
/// sandbox, so a command writes it. The host therefore opens it without following a symlink and
/// without waiting for a writer (`O_NOFOLLOW | O_NONBLOCK`), requires a plain regular file and
/// reads at most [`OWNER_RECORD_MAX`] bytes: a FIFO, a link, a device or a longer file is no
/// record at all, never a blocked or unbounded read in the owner's process.
fn recorded_owner(dir: &Path) -> Option<(u32, u64)> {
    use std::io::Read;
    let path = dir.join(".oxa-owner");
    if !std::fs::symlink_metadata(&path).ok()?.file_type().is_file() {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .ok()?;
    // The entry may have been swapped between the check and the open.
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(OWNER_RECORD_MAX + 1).read_to_string(&mut text).ok()?;
    if text.len() as u64 > OWNER_RECORD_MAX {
        return None;
    }
    let mut it = text.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// The parent of every session directory of `home`.
pub fn scratch_root(home: &Path) -> PathBuf {
    home.join(".cache/oxa/scratch")
}

/// The user's cache home, as the XDG tools outside the sandbox see it: `XDG_CACHE_HOME` from
/// `env` when it is an absolute path (the XDG rule ignores a relative one), else `~/.cache`.
pub fn user_cache_home(home: &Path, env: &[(String, String)]) -> PathBuf {
    env.iter()
        .find(|(k, _)| k == "XDG_CACHE_HOME")
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".cache"))
}

/// A session directory; removed on drop.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    /// The last measured size and when (see [`Scratch::usage`]).
    measured: Mutex<Option<(Instant, u64)>>,
    /// The size at the last warning (0 = none yet).
    warned: AtomicU64,
}

impl Scratch {
    /// Create a fresh session directory under `root` (see [`scratch_root`]) and remove the
    /// ones whose process has exited.
    pub fn create(root: &Path) -> std::io::Result<Scratch> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        static N: AtomicU32 = AtomicU32::new(0);
        std::fs::create_dir_all(root)?;
        prune(root);
        let pid = std::process::id();
        let path = root.join(format!("{pid}-{}", N.fetch_add(1, Ordering::Relaxed)));
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir(path.join(".cache"))?;
        if let Some(start) = start_time(pid) {
            let _ = std::fs::write(path.join(".oxa-owner"), format!("{pid} {start}\n"));
        }
        Ok(Scratch {
            path,
            measured: Mutex::new(None),
            warned: AtomicU64::new(0),
        })
    }

    /// Estimate bytes on disk, excluding symlinks. Reuse the last bounded scan for a minute.
    pub fn usage(&self) -> u64 {
        self.measure(None).unwrap_or(0)
    }

    fn measure(&self, cancel: Option<&CancelToken>) -> Option<u64> {
        let cancelled = || cancel.is_some_and(CancelToken::is_cancelled);
        if cancelled() {
            return None;
        }
        let mut guard = self.measured.lock().unwrap_or_else(|e| e.into_inner());
        if cancelled() {
            return None;
        }
        if let Some((at, n)) = *guard {
            if at.elapsed() < USAGE_TTL {
                return Some(n);
            }
        }
        let n = dir_usage(&self.path, cancelled, |_| {});
        if cancelled() {
            return None;
        }
        *guard = Some((Instant::now(), n));
        Some(n)
    }

    /// The one-line warning when the directory holds more than `limit` bytes; again only after
    /// the size has doubled since the last warning. It never changes the command result.
    pub fn warning(&self, limit: u64) -> Option<String> {
        self.warning_with_cancel(limit, None)
    }

    pub(crate) fn warning_with_cancel(&self, limit: u64, cancel: Option<&CancelToken>) -> Option<String> {
        let n = self.measure(cancel)?;
        let last = self.warned.load(Ordering::Relaxed);
        if n <= limit || (last != 0 && n < last.saturating_mul(2)) {
            return None;
        }
        self.warned.store(n.max(1), Ordering::Relaxed);
        Some(format!(
            "\nSandbox: this session's /tmp scratch holds {} (warning level {}, oxa.sandbox.scratchWarnSize); remove what you no longer need.\n",
            human_size(n),
            human_size(limit)
        ))
    }

    /// The host path (mounted at `/tmp` inside).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Link `/tmp/.cache/<name>` to each entry of the user's cache home `cache` that a
    /// read-write bind in `write` covers (the bind keeps its host path inside, so the link
    /// resolves there). Links only to what is bound read-write; nothing for an empty `cache`.
    pub(crate) fn link_caches(&self, cache: &Path, write: &[PathBuf]) {
        if !cache.is_absolute() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(cache) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if write.iter().any(|w| p.starts_with(w)) {
                let _ = std::os::unix::fs::symlink(&p, self.path.join(".cache").join(e.file_name()));
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // `remove_dir_all` never follows a symlink a command left in the directory.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl PartialEq for Scratch {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}
impl Eq for Scratch {}

/// Owns a directory stream and its descriptor together.
struct UsageDir(NonNull<libc::DIR>);

impl UsageDir {
    fn new(file: File) -> Option<Self> {
        let fd = file.into_raw_fd();
        // SAFETY: fd is an open directory; fdopendir takes ownership only on success.
        let stream = unsafe { libc::fdopendir(fd) };
        if let Some(stream) = NonNull::new(stream) {
            Some(Self(stream))
        } else {
            // SAFETY: fdopendir failed and left the descriptor to us.
            drop(unsafe { File::from_raw_fd(fd) });
            None
        }
    }

    fn fd(&self) -> RawFd {
        // SAFETY: the stream is open for the lifetime of self.
        unsafe { libc::dirfd(self.0.as_ptr()) }
    }

    fn next_name(&mut self) -> Option<&CStr> {
        // SAFETY: the stream is solely owned; the name is NUL-terminated and borrowed only
        // until the next read or close of this stream.
        unsafe {
            let entry = libc::readdir(self.0.as_ptr());
            if entry.is_null() {
                None
            } else {
                Some(CStr::from_ptr((*entry).d_name.as_ptr()))
            }
        }
    }
}

impl Drop for UsageDir {
    fn drop(&mut self) {
        // SAFETY: this stream and its descriptor belong to self.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

/// The disk usage below `dir`. Each lookup uses a single name relative to a pinned parent.
fn dir_usage(dir: &Path, cancelled: impl Fn() -> bool, mut before_descend: impl FnMut(&CStr)) -> u64 {
    let deadline = Instant::now() + USAGE_TIME;
    let stopped = || cancelled() || Instant::now() >= deadline;
    if stopped() {
        return 0;
    }
    let Some(root) = File::options()
        .read(true)
        .custom_flags(USAGE_DIR_FLAGS)
        .open(dir)
        .ok()
        .and_then(UsageDir::new)
    else {
        return 0;
    };
    let mut total = 0u64;
    let mut remaining = USAGE_ENTRIES;
    let mut stack = vec![root];
    while remaining > 0 && !stopped() {
        let depth = stack.len();
        let Some(parent) = stack.last_mut() else {
            break;
        };
        let fd = parent.fd();
        let Some(name) = parent.next_name() else {
            stack.pop();
            continue;
        };
        remaining -= 1;
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fd is live, name is a C string from readdir, and stat is writable.
        if unsafe { libc::fstatat(fd, name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            continue;
        }
        // SAFETY: fstatat succeeded and filled stat.
        let stat = unsafe { stat.assume_init() };
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                let blocks = u64::try_from(stat.st_blocks).unwrap_or(0);
                total = total.saturating_add(blocks.saturating_mul(512));
            }
            libc::S_IFDIR if depth < USAGE_OPEN_DIRS => {
                before_descend(name);
                if stopped() {
                    break;
                }
                // SAFETY: fd and name are still live. O_NOFOLLOW rejects even a symlink
                // substituted after fstatat; O_DIRECTORY prevents opening other file types.
                let child = unsafe { libc::openat(fd, name.as_ptr(), USAGE_DIR_FLAGS) };
                if child >= 0 {
                    // SAFETY: openat returned a new owned descriptor.
                    if let Some(child) = UsageDir::new(unsafe { File::from_raw_fd(child) }) {
                        stack.push(child);
                    }
                }
            }
            _ => {}
        }
    }
    total
}

/// Remove the session directories of processes that no longer run, or whose pid now belongs
/// to another process (the recorded start time differs).
fn prune(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        let gone = !Path::new("/proc").join(pid.to_string()).exists();
        let reused = recorded_owner(&e.path())
            .is_some_and(|(p, start)| p == pid && start_time(pid).is_some_and(|now| now != start));
        if gone || reused {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_reused_pid_is_pruned_and_the_live_session_is_kept() {
        let env = oxa_fake::TestEnv::new();
        let root = env.root().join("scratch");
        let live = Scratch::create(&root).unwrap();
        // A directory of this (live) pid, left by an earlier process that had the same pid.
        let stale = root.join(format!("{}-999", std::process::id()));
        std::fs::create_dir_all(&stale).unwrap();
        let start = start_time(std::process::id()).unwrap();
        std::fs::write(
            stale.join(".oxa-owner"),
            format!("{} {}\n", std::process::id(), start + 1),
        )
        .unwrap();
        // A directory without the file falls back to the pid rule: its pid runs, so it stays.
        let plain = root.join(format!("{}-998", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        prune(&root);
        assert!(!stale.exists(), "reused pid must be pruned");
        assert!(live.path().exists(), "the live session keeps its directory");
        assert!(plain.exists());
    }

    /// `make_a_fifo` in a session directory, as a sandboxed command could leave it.
    fn a_fifo_at_owner(dir: &Path) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let p = CString::new(dir.join(".oxa-owner").as_os_str().as_bytes()).unwrap();
        // SAFETY: `p` is a valid NUL-terminated path; the call only creates that entry.
        assert_eq!(unsafe { libc::mkfifo(p.as_ptr(), 0o600) }, 0, "mkfifo");
    }

    #[test]
    fn a_model_written_owner_record_never_blocks_the_host_and_prunes_nothing() {
        let env = oxa_fake::TestEnv::new();
        let root = env.root().join("scratch");
        let live = Scratch::create(&root).unwrap();
        let start = start_time(std::process::id()).unwrap();
        let stale = format!("{} {}\n", std::process::id(), start + 1);
        let cases = [
            ("fifo", Some(stale.clone())),
            ("link-zero", None),
            ("link-owner", Some(stale.clone())),
            ("large", Some(format!("{stale}{}", "0".repeat(64)))),
        ];
        for (i, (name, record)) in cases.iter().enumerate() {
            let dir = root.join(format!("{}-{}", std::process::id(), 900 + i));
            std::fs::create_dir_all(&dir).unwrap();
            let owner = dir.join(".oxa-owner");
            match name {
                &"fifo" => a_fifo_at_owner(&dir),
                &"link-zero" => std::os::unix::fs::symlink("/dev/zero", &owner).unwrap(),
                &"link-owner" => {
                    let target = root.join(format!("{}-target", std::process::id()));
                    std::fs::write(&target, record.as_deref().unwrap()).unwrap();
                    std::os::unix::fs::symlink(&target, &owner).unwrap();
                }
                _ => std::fs::write(&owner, record.as_deref().unwrap()).unwrap(),
            }
            assert_eq!(recorded_owner(&dir), None, "{name} is no record");
        }
        // A session start after them (local) and the prune still complete, and a shape the
        // model wrote under a live session's directory does not delete that session.
        let _next = Scratch::create(&root).unwrap();
        for i in 0..cases.len() {
            assert!(root.join(format!("{}-{}", std::process::id(), 900 + i)).exists());
        }
        assert!(live.path().exists());
    }

    #[test]
    fn usage_ignores_a_directory_swapped_for_a_symlink_before_descent() {
        use std::os::unix::fs::{symlink, MetadataExt};

        let env = oxa_fake::TestEnv::new();
        let root = env.root().join("scratch");
        let child = root.join("child");
        let outside = env.root().join("outside");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let inside_file = root.join("inside");
        let outside_file = outside.join("outside");
        std::fs::write(&inside_file, vec![1u8; 4096]).unwrap();
        std::fs::write(&outside_file, vec![1u8; 4096]).unwrap();
        assert!(outside_file.metadata().unwrap().blocks() > 0);
        let outside = outside.canonicalize().unwrap();
        let mut swapped = false;
        let size = dir_usage(
            &root,
            || false,
            |name| {
                assert_eq!(name.to_bytes(), b"child");
                std::fs::remove_dir(&child).unwrap();
                symlink(&outside, &child).unwrap();
                swapped = true;
            },
        );
        assert!(swapped, "the directory was replaced after classification");
        assert_eq!(size, inside_file.metadata().unwrap().blocks() * 512);
    }

    #[test]
    fn cancelled_usage_does_not_record_a_warning() {
        let env = oxa_fake::TestEnv::new();
        let sc = Scratch::create(&env.root().join("scratch")).unwrap();
        std::fs::write(sc.path().join("file"), vec![1u8; 4096]).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(sc.warning_with_cancel(0, Some(&cancel)).is_none());
        cancel.reset();
        assert!(sc.warning_with_cancel(0, Some(&cancel)).is_some());
    }

    #[test]
    fn size_warning_counts_files_not_links_and_repeats_only_after_doubling() {
        let env = oxa_fake::TestEnv::new();
        let sc = Scratch::create(&env.root().join("scratch")).unwrap();
        std::fs::write(sc.path().join("big"), vec![1u8; 64 * 1024]).unwrap();
        std::os::unix::fs::symlink("/usr", sc.path().join("link")).unwrap();
        let size = sc.usage();
        assert!((64 * 1024..1 << 20).contains(&size));
        assert!(sc.warning(DEFAULT_SCRATCH_WARN).is_none());
        assert!(sc.warning(size).is_none(), "the limit must be exceeded");
        let limit = 1024;
        let line = sc.warning(limit).expect("first warning over the limit");
        assert_eq!(line.trim().lines().count(), 1);
        assert!(sc.warning(limit).is_none(), "once at the same size");

        // Supply fresh measurements to hit the exact byte boundary without waiting for the cache.
        *sc.measured.lock().unwrap() = Some((Instant::now(), size * 2 - 1));
        assert!(sc.warning(limit).is_none(), "still less than double");
        *sc.measured.lock().unwrap() = Some((Instant::now(), size * 2));
        assert!(sc.warning(limit).is_some(), "warn again at double the size");
        assert!(sc.warning(limit).is_none(), "once at the new size");
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_scratch_warn("20GiB"), Ok(Some(20 << 30)));
        assert_eq!(parse_scratch_warn("512 MiB"), Ok(Some(512 << 20)));
        assert_eq!(parse_scratch_warn("off"), Ok(None));
        assert!(parse_scratch_warn("lots").is_err());
    }
}
