//! File-based session inbox. Writers rename a private temporary file into place. A single
//! owner watches with inotify (Linux) or a short rescan timer (elsewhere), scans once on startup and after notifications, and removes
//! a message only after its callback accepts it. Plain follow-ups use `.msg` files.
//! Acceptance means admission to the owner's in-memory queue, not durable execution.
//! A crash can lose an admitted message or replay one whose file wasn't removed yet.
use crate::SessionRef;
use sha2::{Digest, Sha256};
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
const CONTROL_BYTES: usize = 32 * 1024;

/// Typed inbox delivery keeps the control payload out of the model's follow-up queue.
#[derive(Debug, PartialEq, Eq)]
pub enum InboxItem {
    FollowUp(String),
    Control(String),
}

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
    if text.len() <= INBOX_MESSAGE_BYTES {
        return publish(agent_dir, id, "msg", text).map(|_| ());
    }
    // `Dirs::tmp_dir` is `<agent_dir>/tmp`; the active session installs its own child as the
    // narrowly scoped readable spill exception without exposing the rest of the agent directory.
    let spill_root = agent_dir.join("tmp");
    let spill = publish_in(directory_at(&spill_root, id)?, "txt", text)?;
    let path = spill_root.join(id).join(spill);
    let pointer = format!(
        "The message is {} bytes, too large to queue inline. Its full text is in the private file {}; read that file.",
        text.len(),
        path.display()
    );
    publish(agent_dir, id, "msg", &pointer).map(|_| ())
}

/// Publish a control packet for a trusted, already resolved session reference.
/// The path digest routes delivery within the ID directory. Authorization and receipt
/// claims belong to the frontend and session owner.
pub fn send_inbox_control(agent_dir: &Path, session: &SessionRef, packet: &str) -> io::Result<()> {
    if packet.len() > CONTROL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inbox control packet exceeds 32 KiB",
        ));
    }
    publish(agent_dir, &session.id, &control_extension(session), packet).map(|_| ())
}

fn control_extension(session: &SessionRef) -> String {
    format!(
        "control-{:x}",
        Sha256::digest(session.path.as_os_str().as_encoded_bytes())
    )
}

/// Publish atomically and return the published file name inside the session's inbox directory.
fn publish(agent_dir: &Path, id: &str, extension: &str, text: &str) -> io::Result<String> {
    let dir = directory(agent_dir, id)?;
    publish_in(dir, extension, text)
}

fn directory_at(root: &Path, id: &str) -> io::Result<Directory> {
    if !valid_inbox_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid session id"));
    }
    DirBuilder::new().recursive(true).mode(0o700).create(root)?;
    Directory::open(root)?.child(id)
}

fn publish_in(dir: Directory, extension: &str, text: &str) -> io::Result<String> {
    let name = format!(
        "{:032}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos(),
        crate::new_entry_id(&|_| false)
    );
    let temp = dir.path().join(format!(".{name}.tmp"));
    let file_name = format!("{name}.{extension}");
    let ready = dir.path().join(&file_name);
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
    result.map(|()| file_name)
}

/// The retry signal of a running [`Inbox`]: cheap to clone and to call from any thread.
#[derive(Clone)]
pub struct InboxRetry(Arc<RetrySignal>);
impl InboxRetry {
    /// Same effect as [`Inbox::retry`]; calls after the watcher stopped are harmless.
    pub fn retry(&self) {
        self.0.signal();
    }
}

/// Coalescing wake-up for the watcher thread: an eventfd on Linux, a non-blocking pipe elsewhere.
struct RetrySignal {
    read: File,
    write: File,
}
impl RetrySignal {
    #[cfg(target_os = "linux")]
    fn new() -> io::Result<Self> {
        // SAFETY: eventfd has no pointer arguments; its new descriptor is owned below.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a newly created owned descriptor.
        let read = unsafe { File::from_raw_fd(fd) };
        let write = read.try_clone()?;
        Ok(Self { read, write })
    }

    #[cfg(not(target_os = "linux"))]
    fn new() -> io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: fds has room for the two descriptors pipe writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both descriptors are newly created and owned here.
        let (read, write) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        for fd in [read.as_raw_fd(), write.as_raw_fd()] {
            // SAFETY: fd is open; these flag calls take no pointers.
            let ok = unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0
                    && libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) == 0
            };
            if !ok {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { read, write })
    }

    fn signal(&self) {
        let value = 1u64;
        // SAFETY: the descriptor stays open while self exists, and value points to eight initialized bytes.
        unsafe { libc::write(self.write.as_raw_fd(), (&value as *const u64).cast(), 8) };
    }

    fn drain(&self) {
        let mut value = [0u64; 8];
        loop {
            // SAFETY: the descriptor is live and value is writable for 64 bytes.
            let n = unsafe { libc::read(self.read.as_raw_fd(), value.as_mut_ptr().cast(), 64) };
            // An eventfd yields one counter per read; a pipe may hold several queued signals.
            if n <= 0 || cfg!(target_os = "linux") {
                break;
            }
        }
    }
}

/// Directory change notification: inotify on Linux. Elsewhere there is no kernel watch, so the
/// descriptor is -1 (ignored by poll) and the watcher rescans on a short timeout instead.
#[cfg(target_os = "linux")]
struct Notify(File);
#[cfg(target_os = "linux")]
impl Notify {
    const POLL_TIMEOUT_MS: libc::c_int = -1;

    fn new(path: &CString) -> io::Result<Self> {
        // SAFETY: inotify_init1 has no pointer arguments; the returned fd is owned below.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a newly created, owned descriptor.
        let notify = unsafe { File::from_raw_fd(fd) };
        // Subscribe before the initial scan so messages arriving during startup aren't lost.
        // SAFETY: path is NUL terminated and notify remains open.
        if unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(notify))
    }
    fn fd(&self) -> libc::c_int {
        self.0.as_raw_fd()
    }
    fn drain(&mut self, buffer: &mut [u8]) -> bool {
        self.0.read(buffer).is_ok()
    }
}
#[cfg(not(target_os = "linux"))]
struct Notify;
#[cfg(not(target_os = "linux"))]
impl Notify {
    const POLL_TIMEOUT_MS: libc::c_int = 200;

    fn new(_path: &CString) -> io::Result<Self> {
        Ok(Self)
    }
    fn fd(&self) -> libc::c_int {
        -1
    }
    fn drain(&mut self, _buffer: &mut [u8]) -> bool {
        true
    }
}

/// Owns the event-driven receiver. Dropping it wakes and joins its thread, so an old
/// session's callback cannot enqueue anything after a frontend has rebound to a new one.
pub struct Inbox {
    stop: std::io::PipeWriter,
    retry: Arc<RetrySignal>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Inbox {
    /// Reconsider disk-backed messages after the owner has drained queue capacity.
    /// An eventfd coalesces retries without polling or blocking the frontend.
    pub fn retry(&self) {
        self.retry.signal();
    }

    /// A clone of this watcher's retry signal, for an owner that learns of a queue drain
    /// somewhere other than its own run loop (a headless event sink).
    pub fn retry_handle(&self) -> InboxRetry {
        InboxRetry(self.retry.clone())
    }

    pub fn watch(
        agent_dir: &Path,
        id: &str,
        mut deliver: impl FnMut(String) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        Self::watch_inner(agent_dir, id, None, true, move |item| match item {
            InboxItem::FollowUp(text) => deliver(text),
            InboxItem::Control(_) => false,
        })
    }

    /// Watch the same inbox for follow-ups and control packets routed to this full
    /// session reference. The watcher retains the existing single-consumer lock.
    pub fn watch_bound(
        agent_dir: &Path,
        session: &SessionRef,
        deliver: impl FnMut(InboxItem) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        Self::watch_inner(
            agent_dir,
            &session.id,
            Some(control_extension(session)),
            true,
            deliver,
        )
    }

    /// Watch only typed controls for a managed session using the existing inbox lock.
    pub fn watch_control(
        agent_dir: &Path,
        session: &SessionRef,
        mut deliver: impl FnMut(String) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        Self::watch_inner(
            agent_dir,
            &session.id,
            Some(control_extension(session)),
            false,
            move |item| match item {
                InboxItem::Control(packet) => deliver(packet),
                InboxItem::FollowUp(_) => false,
            },
        )
    }

    fn watch_inner(
        agent_dir: &Path,
        id: &str,
        control_extension: Option<String>,
        follow_ups: bool,
        mut deliver: impl FnMut(InboxItem) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let dir = directory(agent_dir, id)?;
        dir.lock()?;
        let path = CString::new(dir.path().as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
        let mut notify = Notify::new(&path)?;
        let (stop_read, stop) = std::io::pipe()?;
        let retry = Arc::new(RetrySignal::new()?);
        let retry_read = retry.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let thread = thread::Builder::new().name("oxa-inbox".into()).spawn(move || {
            let extension = control_extension.as_deref();
            scan(&dir, extension, follow_ups, &mut deliver, &stopped);
            let mut fds = [
                libc::pollfd {
                    fd: notify.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop_read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: retry_read.read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let mut buffer = [0u8; 8192];
            loop {
                // SAFETY: fds contains three live descriptors and is writable for its length.
                let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, Notify::POLL_TIMEOUT_MS) };
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if fds[1].revents != 0 || stopped.load(Ordering::Relaxed) {
                    break;
                }
                if fds[0].revents & libc::POLLIN != 0 && !notify.drain(&mut buffer) {
                    break;
                }
                if fds[2].revents & libc::POLLIN != 0 {
                    retry_read.drain();
                }
                // Capacity notifications and inotify overflow both trigger a rescan.
                scan(&dir, extension, follow_ups, &mut deliver, &stopped);
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
impl Drop for Inbox {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = self.stop.write_all(&[1]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn scan(
    dir: &Directory,
    control_extension: Option<&str>,
    follow_ups: bool,
    deliver: &mut impl FnMut(InboxItem) -> bool,
    stopped: &AtomicBool,
) {
    let mut after = None;
    while scan_batch(dir, control_extension, follow_ups, deliver, stopped, &mut after) {}
}

fn scan_batch(
    dir: &Directory,
    control_extension: Option<&str>,
    follow_ups: bool,
    deliver: &mut impl FnMut(InboxItem) -> bool,
    stopped: &AtomicBool,
    after: &mut Option<PathBuf>,
) -> bool {
    let Ok(entries) = fs::read_dir(dir.path()) else {
        return false;
    };
    // Keep only a bounded sorted batch, not one PathBuf per file in a stopped inbox.
    let mut paths = std::collections::BTreeMap::new();
    let mut retry = false;
    for entry in entries.filter_map(Result::ok) {
        if stopped.load(Ordering::Relaxed) {
            return false;
        }
        let path = entry.path();
        if after.as_ref().is_some_and(|last| &path <= last) {
            continue;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        let (limit, item): (usize, fn(String) -> InboxItem) = if follow_ups && extension == "msg" {
            (INBOX_MESSAGE_BYTES, InboxItem::FollowUp)
        } else if Some(extension) == control_extension {
            (CONTROL_BYTES, InboxItem::Control)
        } else {
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.nlink() != 1 || meta.len() > limit as u64 {
            continue;
        }
        paths.insert(path, (limit, item));
        if paths.len() > 64 {
            paths.pop_last();
            retry = true;
        }
    }
    // Advance even past unreadable/invalid UTF-8 files, without deleting those files.
    *after = paths.last_key_value().map(|(path, _)| path.clone());
    for (path, (limit, item)) in paths {
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
        if !meta.is_file() || meta.nlink() != 1 || meta.len() > limit as u64 {
            continue;
        }
        let mut text = String::new();
        // Bound the actual read too: the file can grow after metadata was checked.
        if file.take(limit as u64 + 1).read_to_string(&mut text).is_err() || text.len() > limit {
            continue;
        }
        if !deliver(item(text)) {
            return false;
        }
        let _ = fs::remove_file(path);
    }
    retry
}
