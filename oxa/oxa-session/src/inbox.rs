//! File-based session inbox. Writers rename a private temporary file into place. A single
//! owner watches with inotify, scans once on startup and after notifications, and removes
//! a message only after its callback accepts it. Plain follow-ups use `.msg` files.
//! Off Linux (macOS) there is no inotify: the owner scans the same directory every 200 ms.
//! Acceptance means admission to the owner's in-memory queue, not durable execution.
//! A crash can lose an admitted message or replay one whose file wasn't removed yet.
use crate::SessionRef;
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
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
struct Directory {
    file: File,
}
impl Directory {
    fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        make_private(&file)?;
        Ok(Self { file })
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
        fd_path(&self.file)
    }
    fn child(&self, name: &str) -> io::Result<Self> {
        let name = CString::new(name)?;
        // SAFETY: the parent descriptor is live and name is a NUL-terminated component.
        if unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        let file =
            open_directory_at(&self.file, &name)?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        make_private(&file)?;
        Ok(Self { file })
    }
}

fn make_private(dir: &File) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if dir.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "inbox is not owned by this user",
        ));
    }
    dir.set_permissions(fs::Permissions::from_mode(0o700))
}

#[cfg(target_os = "linux")]
fn fd_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
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
    if let Some(id) = id {
        crate::store::validate_session_id(id)?;
    }
    let agent = match Directory::open(agent_dir) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(oxa_proto::OxaError::io("cannot open inbox parent", &e)),
    };
    remove_pinned_inbox(&agent.file, id).map_err(|e| oxa_proto::OxaError::io("cannot remove inbox", &e))
}

fn remove_pinned_inbox(agent: &File, id: Option<&str>) -> io::Result<usize> {
    let Some(inbox) = open_directory_at(agent, c"inbox")? else {
        return Ok(0);
    };
    if let Some(id) = id {
        make_private(&inbox)?;
        let name = CString::new(id)?;
        if let Some(session) = open_directory_at(&inbox, &name)? {
            return remove_open_directory(&inbox, &name, &session);
        }
        return Ok(0);
    }
    remove_open_directory(agent, c"inbox", &inbox)
}

fn open_directory_at(parent: &File, name: &CStr) -> io::Result<Option<File>> {
    // SAFETY: parent is a live directory descriptor and name is a single NUL-terminated component.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        // SAFETY: openat returned a newly owned descriptor.
        return Ok(Some(unsafe { File::from_raw_fd(fd) }));
    }
    let error = io::Error::last_os_error();
    match error.kind() {
        io::ErrorKind::NotFound => Ok(None),
        _ => Err(error),
    }
}

fn remove_open_directory(parent: &File, name: &CStr, directory: &File) -> io::Result<usize> {
    let mut removed = 0;
    let mut result = Ok(());
    for_each_entry_name(directory, |entry| {
        result = remove_wipe_entry(directory, &entry).map(|count| removed += count);
        result.is_ok()
    })?;
    result?;
    unlink_at(parent, name, libc::AT_REMOVEDIR)?;
    Ok(removed)
}

fn remove_wipe_entry(parent: &File, name: &OsStr) -> io::Result<usize> {
    let name = CString::new(name.as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: parent is a live descriptor, name is one component, and stat is writable storage.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::NotFound => Ok(0),
            _ => Err(error),
        };
    }
    // SAFETY: fstatat succeeded and initialized stat.
    let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
    if kind == libc::S_IFDIR {
        return match open_directory_at(parent, &name)? {
            Some(dir) => remove_open_directory(parent, &name, &dir),
            None => Ok(0),
        };
    }
    let unlinked = unlink_at(parent, &name, 0)?;
    Ok(usize::from(unlinked && kind == libc::S_IFREG))
}

fn unlink_at(parent: &File, name: &CStr, flags: libc::c_int) -> io::Result<bool> {
    // SAFETY: parent is a live directory descriptor and name is a single NUL-terminated component.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.kind() {
        io::ErrorKind::NotFound => Ok(false),
        _ => Err(error),
    }
}

/// Publish exactly one message without contacting an engine or requiring a running owner.
pub fn send_inbox(agent_dir: &Path, id: &str, text: &str) -> io::Result<()> {
    if text.len() <= INBOX_MESSAGE_BYTES {
        return publish(agent_dir, id, "msg", text);
    }
    // `Dirs::tmp_dir` is `<agent_dir>/tmp`; the active session installs its own child as the
    // narrowly scoped readable spill exception without exposing the rest of the agent directory.
    if !valid_inbox_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session id for inbox",
        ));
    }
    DirBuilder::new().recursive(true).mode(0o700).create(agent_dir)?;
    let agent = Directory::open(agent_dir)?;
    let spill = publish_into(&agent.child("tmp")?.child(id)?, "txt", text)?;
    let path = agent_dir.join("tmp").join(id).join(spill);
    let pointer = format!(
        "The message is {} bytes, too large to queue inline. Its full text is in the private file {}; read that file.",
        text.len(),
        path.display()
    );
    publish_into(&agent.child("inbox")?.child(id)?, "msg", &pointer).map(|_| ())
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
    directory(agent_dir, id).and_then(|dir| publish_into(&dir, extension, text).map(|_| ()))
}

/// The write, rename and cleanup run through the pinned directory descriptor, never
/// through the saved pathname, so a replaced or renamed path cannot redirect them.
/// Returns the published file name inside that directory.
fn publish_into(dir: &Directory, extension: &str, text: &str) -> io::Result<String> {
    let name = format!(
        "{:032}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos(),
        crate::new_entry_id(&|_| false)
    );
    let temp = CString::new(format!(".{name}.tmp"))?;
    let ready = CString::new(format!("{name}.{extension}"))?;
    // Publication stays on the pinned, private directory inode: a renamed or replaced
    // path can never redirect the write, and O_NOFOLLOW keeps a symlink out of it.
    // SAFETY: both names are single components relative to the live pinned descriptor.
    let fd = unsafe {
        libc::openat(
            dir.file.as_raw_fd(),
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_data()?;
        // SAFETY: a rename between two single components of the same live descriptor.
        if unsafe {
            libc::renameat(
                dir.file.as_raw_fd(),
                temp.as_ptr(),
                dir.file.as_raw_fd(),
                ready.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: one component of the live pinned descriptor.
        unsafe { libc::unlinkat(dir.file.as_raw_fd(), temp.as_ptr(), 0) };
    }
    result.map(|_| format!("{name}.{extension}"))
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

    fn new(dir: &Directory) -> io::Result<Self> {
        let path = CString::new(dir.path().as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
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

    fn new(_dir: &Directory) -> io::Result<Self> {
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
    /// An eventfd or pipe coalesces retries without blocking the frontend.
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
        let mut notify = Notify::new(&dir)?;
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
    after: &mut Option<OsString>,
) -> bool {
    // Keep only a bounded sorted batch, not one name per file in a stopped inbox.
    let mut paths = std::collections::BTreeMap::new();
    let mut retry = false;
    let enumerated = for_each_entry_name(&dir.file, |name| {
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
        // Entries that can never be delivered must not take slots in the bounded batch, or a
        // retained backlog of them turns every scan into ceil(N / 64) full enumerations.
        if !entry_eligible(dir, &name, limit) {
            return true;
        }
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

/// Descriptor-relative pre-check of the delivery rules (regular file, one link, within the
/// size limit) without following a symlink. Delivery re-checks the opened file itself.
fn entry_eligible(dir: &Directory, name: &OsStr, limit: usize) -> bool {
    let Ok(name) = CString::new(name.as_bytes()) else {
        return false;
    };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a single name component relative to a live descriptor; `stat` is writable.
    let result = unsafe {
        libc::fstatat(
            dir.file.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return false;
    }
    // SAFETY: fstatat succeeded and initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    stat.st_mode & libc::S_IFMT == libc::S_IFREG
        && stat.st_nlink == 1
        && u64::try_from(stat.st_size).is_ok_and(|size| size <= limit as u64)
}

/// Visit names in `dir` through the pinned descriptor without first allocating the entire
/// directory listing.
#[cfg(target_os = "linux")]
fn for_each_entry_name(dir: &File, mut visit: impl FnMut(OsString) -> bool) -> io::Result<()> {
    for entry in fs::read_dir(fd_path(dir))?.flatten() {
        if !visit(entry.file_name()) {
            break;
        }
    }
    Ok(())
}

/// Visit names in `dir` through a fresh descriptor for the pinned directory. Opening `.`
/// rather than duplicating `dir.file` gives every poll an independent directory offset.
#[cfg(not(target_os = "linux"))]
fn for_each_entry_name(dir: &File, mut visit: impl FnMut(OsString) -> bool) -> io::Result<()> {
    let dot = c".";
    // SAFETY: `.` is one component relative to the live, pinned directory descriptor.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_creation_stays_in_the_pinned_parent_after_a_rename() {
        let env = oxa_fake::TestEnv::new();
        let agent = env.root().join("agent");
        fs::create_dir(&agent).unwrap();
        let pinned = Directory::open(&agent).unwrap();
        let moved = env.root().join("moved");
        fs::rename(&agent, &moved).unwrap();
        fs::create_dir(&agent).unwrap();

        let child = pinned.child("inbox").unwrap().child("session-pin").unwrap();
        publish_into(&child, "msg", "queued").unwrap();

        assert!(!agent.join("inbox").exists());
        assert_eq!(fs::read_dir(moved.join("inbox/session-pin")).unwrap().count(), 1);
    }

    #[test]
    fn publish_keeps_writing_the_pinned_directory_after_a_rename() {
        use std::os::unix::fs::symlink;
        let env = oxa_fake::TestEnv::new();
        let agent = env.root().join("agent");
        send_inbox(&agent, "session-pin", "first").unwrap();
        let pinned = directory(&agent, "session-pin").unwrap();
        let dir = agent.join("inbox/session-pin");
        let moved = env.root().join("moved");
        fs::rename(&dir, &moved).unwrap();
        let attacker = env.root().join("attacker");
        fs::create_dir_all(&attacker).unwrap();
        symlink(&attacker, &dir).unwrap();

        publish_into(&pinned, "msg", "second").unwrap();

        assert!(
            fs::read_dir(&attacker).unwrap().next().is_none(),
            "the replacement directory receives nothing"
        );
        let names: Vec<String> = fs::read_dir(&moved)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "the pinned directory keeps both: {names:?}");
        let texts: Vec<String> = names
            .iter()
            .map(|n| fs::read_to_string(moved.join(n)).unwrap())
            .collect();
        assert!(texts.contains(&"first".to_string()) && texts.contains(&"second".to_string()));
    }

    #[test]
    fn undeliverable_entries_do_not_fill_the_bounded_batch() {
        let env = oxa_fake::TestEnv::new();
        let agent = env.root().join("agent");
        send_inbox(&agent, "session-skip", "deliver me").unwrap();
        let pinned = directory(&agent, "session-skip").unwrap();
        let dir = agent.join("inbox/session-skip");
        // One oversized message plus hard links to it: more than a batch of names, none deliverable.
        let big = dir.join("0-big.msg");
        fs::write(&big, vec![b'x'; INBOX_MESSAGE_BYTES + 1]).unwrap();
        for index in 0..100 {
            fs::hard_link(&big, dir.join(format!("0-link-{index:03}.msg"))).unwrap();
        }
        let stopped = AtomicBool::new(false);
        let mut after = None;
        let mut delivered = Vec::new();
        let more = scan_batch(
            &pinned,
            None,
            true,
            &mut |item| {
                delivered.push(item);
                true
            },
            &stopped,
            &mut after,
        );
        assert!(!more, "ineligible entries must not force another enumeration");
        assert_eq!(delivered, vec![InboxItem::FollowUp("deliver me".into())]);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            101,
            "rejected entries stay on disk"
        );
    }

    #[test]
    fn inbox_wipe_keeps_the_open_parent_after_a_rename() {
        for id in [None, Some("session-wipe")] {
            let env = oxa_fake::TestEnv::new();
            let agent = env.root().join("agent");
            send_inbox(&agent, "session-wipe", "remove").unwrap();
            let pinned = Directory::open(&agent).unwrap();
            let moved = env.root().join("moved");
            fs::rename(&agent, &moved).unwrap();
            send_inbox(&agent, "session-wipe", "keep").unwrap();

            assert_eq!(remove_pinned_inbox(&pinned.file, id).unwrap(), 1);
            assert!(!moved.join("inbox/session-wipe").exists());
            assert_eq!(moved.join("inbox").exists(), id.is_some());
            let mut remaining = fs::read_dir(agent.join("inbox/session-wipe")).unwrap();
            let kept = remaining.next().unwrap().unwrap();
            assert_eq!(fs::read_to_string(kept.path()).unwrap(), "keep");
            assert!(remaining.next().is_none());
        }
    }
}
