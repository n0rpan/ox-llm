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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The default of `oxa.sandbox.scratchWarnSize`.
pub const DEFAULT_SCRATCH_WARN: u64 = 20 << 30;

/// How long a measured size is reused before the directory is walked again.
const USAGE_TTL: Duration = Duration::from_secs(60);

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
    use std::os::unix::fs::OpenOptionsExt;
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

    /// The bytes this directory holds on disk, without following symlinks (the linked shared
    /// caches are not counted). Walked at most once a minute; in between the last value.
    pub fn usage(&self) -> u64 {
        let mut guard = self.measured.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, n)) = *guard {
            if at.elapsed() < USAGE_TTL {
                return n;
            }
        }
        let n = dir_usage(&self.path);
        *guard = Some((Instant::now(), n));
        n
    }

    /// The one-line warning when the directory holds more than `limit` bytes; again only after
    /// the size has doubled since the last warning. Never fails or blocks anything.
    pub fn warning(&self, limit: u64) -> Option<String> {
        let n = self.usage();
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

/// The disk usage of the files below `dir`; symlinks are neither followed nor counted.
fn dir_usage(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                total += e.metadata().map(|m| m.blocks() * 512).unwrap_or(0);
            }
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
    fn size_warning_counts_files_not_links_and_repeats_only_after_doubling() {
        let env = oxa_fake::TestEnv::new();
        let sc = Scratch::create(&env.root().join("scratch")).unwrap();
        std::fs::write(sc.path().join("big"), vec![1u8; 64 * 1024]).unwrap();
        std::os::unix::fs::symlink("/usr", sc.path().join("link")).unwrap();
        assert!(sc.usage() >= 64 * 1024 && sc.usage() < 1 << 20);
        assert!(sc
            .warning(1024)
            .is_some_and(|w| w.contains("oxa.sandbox.scratchWarnSize")));
        assert!(sc.warning(1024).is_none(), "once until the size doubles");
        assert!(sc.warning(1 << 30).is_none());
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_scratch_warn("20GiB"), Ok(Some(20 << 30)));
        assert_eq!(parse_scratch_warn("512 MiB"), Ok(Some(512 << 20)));
        assert_eq!(parse_scratch_warn("off"), Ok(None));
        assert!(parse_scratch_warn("lots").is_err());
    }
}
