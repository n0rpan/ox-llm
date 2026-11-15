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

// Inside a user namespace (the oxa sandbox) host root shows up as the overflow uid
// because it is unmapped; it is still root-controlled, so accept it there.
fn trusted_owner(owner: u32, user: u32) -> bool {
    owner == user || owner == 0 || (owner == overflow_uid() && in_user_namespace())
}

fn overflow_uid() -> u32 {
    std::fs::read_to_string("/proc/sys/kernel/overflowuid")
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(65534)
}

fn in_user_namespace() -> bool {
    std::fs::read_to_string("/proc/self/uid_map")
        .map(|map| map.split_whitespace().collect::<Vec<_>>() != ["0", "0", "4294967295"])
        .unwrap_or(false)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmapped_root_is_trusted_only_inside_a_user_namespace() {
        let user = unsafe { libc::geteuid() };
        assert!(trusted_owner(0, user));
        assert!(trusted_owner(user, user));
        assert_eq!(trusted_owner(overflow_uid(), user), in_user_namespace());
        assert!(!trusted_owner(user.wrapping_add(12345), user));
    }
}
