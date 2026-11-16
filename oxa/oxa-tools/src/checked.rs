//! #518: the file tools open the location the permission check judged, not whatever a symlink
//! points at by the time they run. The check pins a path's real location
//! (`ToolCtx::checked`); after opening, a tool compares the real location of what it opened
//! with the pin and refuses a mismatch (a link swapped by a background job in between). With
//! nothing pinned (a direct call) a tool opens as before.
use oxa_proto::ToolCtx;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The error when the opened file is not the one the permission check judged.
pub(crate) const SWAPPED: &str =
    "the path changed after the permission check (was a symlink swapped?), so it was not used";

/// Open `abs` for reading; refused when it is not the location the check pinned for it.
pub(crate) fn open(cx: &ToolCtx, abs: &Path) -> io::Result<File> {
    let file = File::open(abs)?;
    verify(cx, abs, &file)?;
    Ok(file)
}

/// `Ok` when `file`, opened from `abs`, is what the check pinned for `abs` (or nothing was).
pub(crate) fn verify(cx: &ToolCtx, abs: &Path, file: &File) -> io::Result<()> {
    match cx.checked_real(abs) {
        Some(pin) => verify_at(pin, file),
        None => Ok(()),
    }
}

/// `Ok` when `file` (a file or a directory) is the one at `expected`, a real path.
pub(crate) fn verify_at(expected: &Path, file: &File) -> io::Result<()> {
    if is_at(expected, file)? {
        Ok(())
    } else {
        Err(io::Error::other(SWAPPED))
    }
}

/// Whether `file` is at `expected`: the kernel's own path of the open file. Without `/proc`,
/// the same device and inode as `expected` (looked up again; the best that is left).
fn is_at(expected: &Path, file: &File) -> io::Result<bool> {
    match std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())) {
        Ok(real) => Ok(real == expected),
        Err(_) => {
            use std::os::unix::fs::MetadataExt;
            let (a, b) = (file.metadata()?, std::fs::metadata(expected)?);
            Ok(a.dev() == b.dev() && a.ino() == b.ino())
        }
    }
}

/// A file name as a C string (`openat`/`renameat` take one component, relative to a directory).
fn c_name(name: &str) -> io::Result<CString> {
    CString::new(Path::new(name).as_os_str().as_bytes()).map_err(io::Error::other)
}

/// Create `name` in the directory `dir` (never following a link there), write-only, new.
pub(crate) fn create_new_in(dir: &File, name: &str) -> io::Result<File> {
    let c = c_name(name)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open directory, `c` a valid C string; the fd returned is owned below.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, 0o666 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Rename `from` to `to`, both in the directory `dir`.
pub(crate) fn rename_in(dir: &File, from: &str, to: &str) -> io::Result<()> {
    let (f, t) = (c_name(from)?, c_name(to)?);
    let d = dir.as_raw_fd();
    // SAFETY: `dir` is an open directory and both names are valid C strings.
    if unsafe { libc::renameat(d, f.as_ptr(), d, t.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Remove `name` from the directory `dir` (best effort, for a failed write's temp file).
pub(crate) fn remove_in(dir: &File, name: &str) {
    if let Ok(c) = c_name(name) {
        // SAFETY: `dir` is an open directory and `c` a valid C string.
        unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) };
    }
}
