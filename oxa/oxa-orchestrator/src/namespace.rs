//! Protected output namespaces shared by run setup and candidate publication.
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt},
};
use std::path::Path;
// Bind setup/publication to an enforced namespace: untrusted users must not be able
// to replace either the staging child or an ancestor named in the output path.
// Same-UID processes and root are the operator's trusted authority boundary.
pub(crate) fn protected_directory(path: &Path) -> io::Result<File> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute.as_os_str().as_bytes().len() > 4096 {
        return Err(invalid("output destination path is too long"));
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    check_namespace(&directory)?;
    for component in absolute.components() {
        let name = match component {
            std::path::Component::RootDir | std::path::Component::CurDir => continue,
            std::path::Component::Normal(name) => name,
            _ => return Err(invalid("output destination cannot contain parent traversal")),
        };
        let name =
            CString::new(name.as_bytes()).map_err(|_| invalid("invalid output directory component"))?;
        // SAFETY: live owned parent FD, valid C component; no symlink traversal.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
        check_namespace(&directory)?;
    }
    Ok(directory)
}

fn check_namespace(directory: &File) -> io::Result<()> {
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no arguments or memory access side effects.
    let user = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || !trusted_owner(metadata.uid(), user)
        || (metadata.mode() & 0o022 != 0 && metadata.mode() & libc::S_ISVTX == 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "output namespace must be owner/root controlled and protect entries from other users",
        ));
    }
    Ok(())
}

pub(crate) fn require_private_directory(directory: &File, message: &'static str) -> io::Result<()> {
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no arguments or memory access side effects.
    let owner = unsafe { libc::geteuid() };
    if metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
    }
    Ok(())
}

// Only the operator and real root are trusted. The overflow uid (65534) stands for
// every unmapped host uid inside a user namespace, so it is never trusted by value.
fn trusted_owner(owner: u32, user: u32) -> bool {
    owner == user || owner == 0
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn overflow_and_other_owners_stay_rejected() {
        let user = 1000;
        assert!(trusted_owner(user, user));
        assert!(trusted_owner(0, user));
        assert!(!trusted_owner(65534, user));
        assert!(!trusted_owner(1001, user));
    }

    // Fresh directory per test; removed on drop, even when an assert fails.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str, mode: u32) -> Self {
            let path = std::env::temp_dir().join(format!("oxa-ns-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn world_writable_directory_without_sticky_bit_is_rejected() {
        let dir = TempDir::new("open", 0o777);
        assert!(check_namespace(&File::open(&dir.0).unwrap()).is_err());
    }

    #[test]
    fn private_own_directory_is_accepted() {
        let dir = TempDir::new("ok", 0o700);
        assert!(check_namespace(&File::open(&dir.0).unwrap()).is_ok());
    }
}
