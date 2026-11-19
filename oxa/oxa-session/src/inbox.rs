//! File-based session inbox. Writers rename a private temporary file into place. A single
//! owner watches with inotify, scans once on startup and after notifications, and removes
//! a message only after its callback accepts it. Plain follow-ups use `.msg` files.
//! Off Linux (macOS) there is no inotify: the owner scans the same directory every 500 ms.
//! Acceptance means admission to the owner's in-memory queue, not durable execution.
//! A crash can lose an admitted message or replay one whose file wasn't removed yet.
use crate::SessionRef;
use sha2::{Digest, Sha256};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
#[cfg(not(target_os = "linux"))]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Maximum admitted message size. Larger externally published files stay on disk.
pub const INBOX_MESSAGE_BYTES: usize = 256 * 1024;
const CONTROL_BYTES: usize = 32 * 1024;
/// Scan interval for the polling receiver used where inotify is unavailable.
#[cfg(not(target_os = "linux"))]
const POLL_INTERVAL_MS: libc::c_int = 500;

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
struct Directory {
    file: File,
    // Off Linux there is no /proc handle for the pinned fd, so the opened path is kept.
    #[cfg(not(target_os = "linux"))]
    path: PathBuf,
}
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
        Ok(Self {
            file,
            #[cfg(not(target_os = "linux"))]
            path: path.to_path_buf(),
        })
    }
    fn lock(&self) -> io::Result<()> {
        // SAFETY: the directory fd is live; flock doesn't access memory.
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }
    #[cfg(not(target_os = "linux"))]
    fn path(&self) -> PathBuf {
        self.path.clone()
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
    if text.len() > INBOX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inbox message exceeds 256 KiB",
        ));
    }
    publish(agent_dir, id, "msg", text)
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
    publish(agent_dir, &session.id, &control_extension(session), packet)
}

fn control_extension(session: &SessionRef) -> String {
    format!(
        "control-{:x}",
        Sha256::digest(session.path.as_os_str().as_encoded_bytes())
    )
}

fn publish(agent_dir: &Path, id: &str, extension: &str, text: &str) -> io::Result<()> {
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
    let ready = dir.path().join(format!("{name}.{extension}"));
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
    #[cfg(target_os = "linux")]
    retry: Arc<File>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Inbox {
    /// Reconsider disk-backed messages after the owner has drained queue capacity.
    /// An eventfd coalesces retries without polling or blocking the frontend.
    #[cfg(target_os = "linux")]
    pub fn retry(&self) {
        let value = 1u64;
        // SAFETY: retry is live and value points to eight initialized bytes.
        unsafe { libc::write(self.retry.as_raw_fd(), (&value as *const u64).cast(), 8) };
    }

    /// Off Linux the polling receiver rescans every 500 ms, so capacity is reconsidered
    /// without a wakeup.
    #[cfg(not(target_os = "linux"))]
    pub fn retry(&self) {}

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
        deliver: impl FnMut(InboxItem) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let dir = directory(agent_dir, id)?;
        dir.lock()?;
        Self::start(dir, control_extension, follow_ups, deliver)
    }

    #[cfg(target_os = "linux")]
    fn start(
        dir: Directory,
        control_extension: Option<String>,
        follow_ups: bool,
        mut deliver: impl FnMut(InboxItem) -> bool + Send + 'static,
    ) -> io::Result<Self> {
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
            let extension = control_extension.as_deref();
            scan(&dir, extension, follow_ups, &mut deliver, &stopped);
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

    /// Off Linux: the same single-consumer, rescan-on-tick contract as the inotify path,
    /// with a bounded poll of the inbox directory instead of file notifications.
    #[cfg(not(target_os = "linux"))]
    fn start(
        dir: Directory,
        control_extension: Option<String>,
        follow_ups: bool,
        mut deliver: impl FnMut(InboxItem) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let (stop_read, stop) = std::io::pipe()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let thread = thread::Builder::new().name("oxa-inbox".into()).spawn(move || {
            let extension = control_extension.as_deref();
            scan(&dir, extension, follow_ups, &mut deliver, &stopped);
            let mut fds = [libc::pollfd {
                fd: stop_read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            loop {
                // SAFETY: fds holds one live descriptor and is writable for its length.
                let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, POLL_INTERVAL_MS) };
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                // The stop pipe wakes the poll at once; a timeout falls through to a rescan.
                if fds[0].revents != 0 || stopped.load(Ordering::Relaxed) {
                    break;
                }
                scan(&dir, extension, follow_ups, &mut deliver, &stopped);
            }
        })?;
        Ok(Self {
            stop,
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
    after: &mut Option<OsString>,
) -> bool {
    // Keep only a bounded sorted batch, not one name per file in a stopped inbox.
    let mut paths = std::collections::BTreeMap::new();
    let mut retry = false;
    let enumerated = for_each_entry_name(dir, |name| {
        if stopped.load(Ordering::Relaxed) {
            return false;
        }
        if after.as_ref().is_some_and(|last| &name <= last) {
            return true;
        }
        let Some(extension) = Path::new(&name)
            .extension()
            .and_then(|extension| extension.to_str())
        else {
            return true;
        };
        let (limit, item): (usize, fn(String) -> InboxItem) = if follow_ups && extension == "msg" {
            (INBOX_MESSAGE_BYTES, InboxItem::FollowUp)
        } else if Some(extension) == control_extension {
            (CONTROL_BYTES, InboxItem::Control)
        } else {
            return true;
        };
        paths.insert(name, (limit, item));
        if paths.len() > 64 {
            paths.pop_last();
            retry = true;
        }
        true
    });
    if enumerated.is_err() || stopped.load(Ordering::Relaxed) {
        return false;
    }
    // Advance even past unreadable/invalid UTF-8 files, without deleting those files.
    *after = paths.last_key_value().map(|(path, _)| path.clone());
    for (name, (limit, item)) in paths {
        if stopped.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(file) = open_entry(dir, &name) else {
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
        remove_entry(dir, &name);
    }
    retry
}

/// Visit names in `dir` through the pinned descriptor without first allocating the entire
/// directory listing.
#[cfg(target_os = "linux")]
fn for_each_entry_name(dir: &Directory, mut visit: impl FnMut(OsString) -> bool) -> io::Result<()> {
    for entry in fs::read_dir(dir.path())?.flatten() {
        if !visit(entry.file_name()) {
            break;
        }
    }
    Ok(())
}

/// Visit names in `dir` through a fresh descriptor for the pinned directory. Opening `.`
/// rather than duplicating `dir.file` gives every poll an independent directory offset.
#[cfg(not(target_os = "linux"))]
fn for_each_entry_name(dir: &Directory, mut visit: impl FnMut(OsString) -> bool) -> io::Result<()> {
    let dot = c".";
    // SAFETY: `.` is one component relative to the live, pinned directory descriptor.
    let fd = unsafe {
        libc::openat(
            dir.file.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a live descriptor this call takes ownership of.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so `fd` is still ours to close.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    loop {
        // SAFETY: `stream` is a live directory stream; the returned entry is borrowed from it.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: d_name is a NUL-terminated name inside the entry.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if !visit(OsStr::from_bytes(name).to_os_string()) {
            break;
        }
    }
    // SAFETY: `stream` is live and not used after this point.
    unsafe { libc::closedir(stream) };
    Ok(())
}

/// Open one inbox entry with `O_NOFOLLOW` through `dir`'s descriptor: the file comes from the
/// pinned directory, never from a path that may have been replaced.
#[cfg(target_os = "linux")]
fn open_entry(dir: &Directory, name: &OsStr) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.path().join(name))
}

/// Open one inbox entry with `O_NOFOLLOW` through `dir`'s descriptor: the file comes from the
/// pinned directory, never from a path that may have been replaced.
#[cfg(not(target_os = "linux"))]
fn open_entry(dir: &Directory, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "inbox name holds a NUL"))?;
    // SAFETY: a single name component relative to a live descriptor; no memory is shared.
    let fd = unsafe {
        libc::openat(
            dir.file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Remove one delivered inbox entry in the pinned directory.
#[cfg(target_os = "linux")]
fn remove_entry(dir: &Directory, name: &OsStr) {
    let _ = fs::remove_file(dir.path().join(name));
}

/// Remove one delivered inbox entry in the pinned directory.
#[cfg(not(target_os = "linux"))]
fn remove_entry(dir: &Directory, name: &OsStr) {
    let Ok(name) = CString::new(name.as_bytes()) else {
        return;
    };
    // SAFETY: a single name component relative to a live descriptor.
    unsafe { libc::unlinkat(dir.file.as_raw_fd(), name.as_ptr(), 0) };
}
