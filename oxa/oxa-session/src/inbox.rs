//! File-based session follow-ups. Only `.msg` files are published messages; writers rename
//! a private temporary file into place. A single owner watches with inotify, scans once on
//! startup and after notifications, and removes a message only after its callback accepts it.
//! Acceptance means admission to the owner's in-memory queue, not durable execution.
//! A crash can lose an admitted message or replay one whose file wasn't removed yet.
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Maximum admitted message size. Larger externally published files stay on disk.
pub const INBOX_MESSAGE_BYTES: usize = 256 * 1024;

/// The session-id alphabet is deliberately not a path syntax.
pub fn valid_inbox_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    matches!((bytes.first(), bytes.last()), (Some(a), Some(b)) if a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric())
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(c))
}

// All subsequent operations are relative to a pinned directory inode. O_NOFOLLOW at each
// inbox component prevents a symlink from redirecting reads, writes, chmod or watch setup.
struct Directory(File);
impl Directory {
    fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let meta = file.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "inbox is not owned by this user",
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
        Ok(Self(file))
    }
    fn lock(&self) -> io::Result<()> {
        // SAFETY: the directory fd is live; flock doesn't access memory.
        if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
    fn child(&self, name: &str) -> io::Result<Self> {
        let path = self.path().join(name);
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        Self::open(&path)
    }
}

fn directory(agent_dir: &Path, id: &str) -> io::Result<Directory> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "session inbox IPC is supported only on Linux",
        ));
    }
    if !valid_inbox_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session id for inbox",
        ));
    }
    DirBuilder::new().recursive(true).mode(0o700).create(agent_dir)?;
    Directory::open(agent_dir)?.child("inbox")?.child(id)
}

/// Privacy wipe: remove one session's queued prompts, or all inbox storage including
/// orphaned queues and unpublished temporary files. Pin each parent before deleting so a
/// symlink at `agent_dir` or `inbox` cannot redirect a session-specific wipe.
pub fn remove_inbox(agent_dir: &Path, id: Option<&str>) -> Result<usize, oxa_proto::OxaError> {
    let remove = || -> Result<usize, oxa_proto::OxaError> {
        let agent = match Directory::open(agent_dir) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(oxa_proto::OxaError::io("cannot open inbox parent", &e)),
        };
        let root = agent.path().join("inbox");
        match id {
            None => crate::remove_all_spill(&root),
            Some(id) => {
                let inbox = match Directory::open(&root) {
                    Ok(dir) => dir,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
                    Err(e) => return Err(oxa_proto::OxaError::io("cannot open inbox", &e)),
                };
                crate::remove_spill_dir(&inbox.path(), id)
            }
        }
    };
    if let Some(id) = id {
        crate::store::validate_session_id(id)?;
    }
    remove()
}

/// Publish exactly one message without contacting an engine or requiring a running owner.
pub fn send_inbox(agent_dir: &Path, id: &str, text: &str) -> io::Result<()> {
    if text.len() > INBOX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inbox message exceeds 256 KiB",
        ));
    }
    let dir = directory(agent_dir, id)?;
    let name = format!(
        "{:032}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos(),
        crate::new_entry_id(&|_| false)
    );
    let temp = dir.path().join(format!(".{name}.tmp"));
    let ready = dir.path().join(format!("{name}.msg"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temp)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_data()?;
        fs::rename(&temp, ready)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Owns the event-driven receiver. Dropping it wakes and joins its thread, so an old
/// session's callback cannot enqueue anything after a frontend has rebound to a new one.
pub struct Inbox {
    stop: std::io::PipeWriter,
    retry: Arc<File>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Inbox {
    /// Reconsider disk-backed messages after the owner has drained queue capacity.
    /// An eventfd coalesces retries without polling or blocking the frontend.
    pub fn retry(&self) {
        let value = 1u64;
        // SAFETY: retry is live and value points to eight initialized bytes.
        unsafe { libc::write(self.retry.as_raw_fd(), (&value as *const u64).cast(), 8) };
    }

    #[cfg(target_os = "linux")]
    pub fn watch(
        agent_dir: &Path,
        id: &str,
        mut deliver: impl FnMut(String) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let dir = directory(agent_dir, id)?;
        dir.lock()?;
        // SAFETY: inotify_init1 has no pointer arguments; the returned fd is owned below.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a newly created, owned descriptor.
        let mut notify = unsafe { File::from_raw_fd(fd) };
        let path = CString::new(dir.path().as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
        // Subscribe before the initial scan so messages arriving during startup aren't lost.
        // SAFETY: path is NUL terminated and notify remains open.
        if unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let (stop_read, stop) = std::io::pipe()?;
        // SAFETY: eventfd has no pointer arguments; its new descriptor is owned below.
        let retry_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if retry_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: retry_fd is a newly created owned descriptor.
        let retry = Arc::new(unsafe { File::from_raw_fd(retry_fd) });
        let retry_read = retry.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let thread = thread::Builder::new().name("oxa-inbox".into()).spawn(move || {
            scan(&dir, &mut deliver, &stopped);
            let mut fds = [
                libc::pollfd {
                    fd: notify.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop_read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: retry_read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let mut buffer = [0u8; 8192];
            loop {
                // SAFETY: fds contains three live descriptors and is writable for its length.
                let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if fds[1].revents != 0 || stopped.load(Ordering::Relaxed) {
                    break;
                }
                if fds[0].revents & libc::POLLIN != 0 && notify.read(&mut buffer).is_err() {
                    break;
                }
                if fds[2].revents & libc::POLLIN != 0 {
                    let mut value = 0u64;
                    // SAFETY: retry_read is live and value is writable for eight bytes.
                    unsafe { libc::read(retry_read.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
                }
                // Capacity notifications and inotify overflow both trigger a rescan.
                scan(&dir, &mut deliver, &stopped);
            }
        })?;
        Ok(Self {
            stop,
            retry,
            stopping,
            thread: Some(thread),
        })
    }
}
#[cfg(not(target_os = "linux"))]
impl Inbox {
    /// Native file notification is not available on this platform yet.
    pub fn watch(
        _agent_dir: &Path,
        _id: &str,
        _deliver: impl FnMut(String) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "session inbox watching is supported only on Linux",
        ))
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = self.stop.write_all(&[1]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn scan(dir: &Directory, deliver: &mut impl FnMut(String) -> bool, stopped: &AtomicBool) {
    let mut after = None;
    while scan_batch(dir, deliver, stopped, &mut after) {}
}

fn scan_batch(
    dir: &Directory,
    deliver: &mut impl FnMut(String) -> bool,
    stopped: &AtomicBool,
    after: &mut Option<PathBuf>,
) -> bool {
    let Ok(entries) = fs::read_dir(dir.path()) else {
        return false;
    };
    // Keep only a bounded sorted batch, not one PathBuf per file in a stopped inbox.
    let mut paths = std::collections::BTreeSet::new();
    let mut retry = false;
    for entry in entries.filter_map(Result::ok) {
        if stopped.load(Ordering::Relaxed) {
            return false;
        }
        let path = entry.path();
        if after.as_ref().is_some_and(|last| &path <= last) {
            continue;
        }
        if !path.extension().is_some_and(|s| s == "msg") {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.nlink() != 1 || meta.len() > INBOX_MESSAGE_BYTES as u64 {
            continue;
        }
        paths.insert(path);
        if paths.len() > 64 {
            paths.pop_last();
            retry = true;
        }
    }
    // Advance even past unreadable/invalid UTF-8 files, without deleting those files.
    *after = paths.last().cloned();
    for path in paths {
        if stopped.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        else {
            continue;
        };
        // Don't follow symlinks, read devices/FIFOs, or consume hardlinks to other files.
        let Ok(meta) = file.metadata() else { continue };
        if !meta.is_file() || meta.nlink() != 1 || meta.len() > INBOX_MESSAGE_BYTES as u64 {
            continue;
        }
        let mut text = String::new();
        // Bound the actual read too: the file can grow after metadata was checked.
        if file
            .take(INBOX_MESSAGE_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .is_err()
            || text.len() > INBOX_MESSAGE_BYTES
        {
            continue;
        }
        if !deliver(text) {
            return false;
        }
        let _ = fs::remove_file(path);
    }
    retry
}
