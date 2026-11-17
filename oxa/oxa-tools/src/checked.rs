//! #518: the file tools open the location the permission check judged, not whatever a symlink
//! points at by the time they run. The check pins a path's real location
//! (`ToolCtx::checked`); after opening, a tool compares the real location of what it opened
//! with the pin and refuses a mismatch (a link swapped by a background job in between). With
//! nothing pinned (a direct call) a tool opens as before.
use oxa_proto::ToolCtx;
use std::ffi::{CString, OsStr};
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
/// compare device and inode against a descriptor opened without following any symlinks.
fn is_at(expected: &Path, file: &File) -> io::Result<bool> {
    match std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())) {
        Ok(real) => Ok(real == expected),
        Err(_) => is_at_without_proc(expected, file),
    }
}

fn is_at_without_proc(expected: &Path, file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let opened = (|| {
        if let Some(name) = expected.file_name() {
            let dir = open_dir_chain(expected.parent().unwrap(), false)?;
            open_in(&dir, name, false)
        } else {
            open_dir_chain(expected, false)
        }
    })();
    let other = match opened {
        Ok(other) => other,
        Err(e) if redirected(&e) || e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let (a, b) = (file.metadata()?, other.metadata()?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

fn redirected(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
}

/// Create a pinned real parent path without ever following a substituted ancestor symlink.
/// Each mkdir and subsequent open is relative to the already opened ancestor, not its name.
pub(crate) fn create_parent(expected: &Path) -> io::Result<File> {
    open_dir_chain(expected, true).map_err(|e| {
        if redirected(&e) {
            io::Error::other(SWAPPED)
        } else {
            e
        }
    })
}

fn open_dir_chain(expected: &Path, create: bool) -> io::Result<File> {
    use std::path::Component;
    if !expected.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a real absolute path",
        ));
    }
    let mut dir = File::open("/")?;
    for component in expected.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected a real absolute path",
                ))
            }
        };
        dir = match open_in(&dir, name, true) {
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                let c = c_name(name)?;
                // SAFETY: dir is open and c is a valid, single-component C string.
                if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o777) } != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                }
                open_in(&dir, name, true)?
            }
            other => other?,
        };
    }
    Ok(dir)
}

fn open_in(dir: &File, name: &OsStr, directory: bool) -> io::Result<File> {
    let c = c_name(name)?;
    // O_PATH requires search permission, not directory read permission, and cannot block on
    // special files. A final symlink may open as a link itself but cannot match the target inode.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let access = libc::O_PATH;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let access = libc::O_RDONLY | libc::O_NONBLOCK;
    let mut flags = access | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if directory {
        flags |= libc::O_DIRECTORY;
    }
    // SAFETY: dir is open and c is a valid, single-component C string.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just opened and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// A file name as a C string (`openat`/`renameat` take one component, relative to a directory).
fn c_name(name: impl AsRef<OsStr>) -> io::Result<CString> {
    CString::new(name.as_ref().as_bytes()).map_err(io::Error::other)
}

/// Create `name` in the directory `dir` (never following a link there), write-only, new.
pub(crate) fn create_new_in(dir: &File, name: &str, mode: u32) -> io::Result<File> {
    let c = c_name(name)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open directory, `c` a valid C string; the fd returned is owned below.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Rename `from` to `to`, both in the directory `dir`. `to` keeps its raw bytes (a target name
/// need not be UTF-8).
pub(crate) fn rename_in(dir: &File, from: &str, to: &OsStr) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn metadata_fallback_rejects_redirected_ancestors_and_leaf_links() {
        let env = oxa_fake::TestEnv::new();
        let outside = env.write("outside/file", "synthetic private content");
        let file = File::open(&outside).unwrap();
        let work = std::fs::canonicalize(env.work()).unwrap();
        symlink(outside.parent().unwrap(), work.join("swapped")).unwrap();
        assert!(!is_at_without_proc(&work.join("swapped/file"), &file).unwrap());
        symlink(&outside, work.join("leaf")).unwrap();
        assert!(!is_at_without_proc(&work.join("leaf"), &file).unwrap());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn metadata_fallback_needs_only_search_permission_on_ancestors() {
        use std::os::unix::fs::PermissionsExt;
        let env = oxa_fake::TestEnv::new();
        let path = std::fs::canonicalize(env.write("work/dir/file", "normal content")).unwrap();
        let file = File::open(&path).unwrap();
        let dir = path.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o100)).unwrap();
        let result = is_at_without_proc(&path, &file);
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.unwrap());
    }

    #[test]
    fn metadata_fallback_accepts_unchanged_files_and_directories() {
        let env = oxa_fake::TestEnv::new();
        let path = std::fs::canonicalize(env.write("work/file", "normal content")).unwrap();
        assert!(is_at_without_proc(&path, &File::open(&path).unwrap()).unwrap());
        let dir = path.parent().unwrap();
        assert!(is_at_without_proc(dir, &File::open(dir).unwrap()).unwrap());
    }
}
