//! Read a private file through one checked handle.
//!
//! The file is opened once, without following a final symbolic link. The checks (a regular
//! file, owned by the effective user, no group or other permission bits) are made on that open
//! handle, and the bytes are read from the same handle, up to a size bound. The same module is
//! kept identical in `oxa-web` and `oxa-mcp`: the shared crates both depend on are frozen and
//! forbid `unsafe`, which the owner check needs.
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Why a private file was not read.
#[derive(Debug)]
pub(crate) enum PrivateFileError {
    /// Nothing at the path.
    Missing,
    /// The path is a symbolic link.
    SymbolicLink,
    /// Not a regular file (a directory, FIFO, socket or device).
    NotRegular,
    /// Owned by another user.
    WrongOwner,
    /// Group or other permission bits are set; carries the `0o777` mode.
    Permissions(u32),
    /// Larger than the limit; carries the limit in bytes.
    TooLarge(u64),
    /// Any other open, stat or read failure.
    Io(io::Error),
}

/// Open `path` once and check the open handle: a regular file, owned by the effective user,
/// mode without group or other bits.
pub(crate) fn open_private(path: &Path) -> Result<File, PrivateFileError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => PrivateFileError::SymbolicLink,
            _ if e.kind() == io::ErrorKind::NotFound => PrivateFileError::Missing,
            _ => PrivateFileError::Io(e),
        })?;
    let info = file.metadata().map_err(PrivateFileError::Io)?;
    if !info.is_file() {
        return Err(PrivateFileError::NotRegular);
    }
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    if info.uid() != unsafe { libc::geteuid() } {
        return Err(PrivateFileError::WrongOwner);
    }
    let mode = info.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(PrivateFileError::Permissions(mode));
    }
    Ok(file)
}

/// [`open_private`], then read at most `limit` bytes from that same handle. A file with more
/// than `limit` bytes is refused, not cut.
pub(crate) fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>, PrivateFileError> {
    let file = open_private(path)?;
    let mut data = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(PrivateFileError::Io)?;
    if data.len() as u64 > limit {
        return Err(PrivateFileError::TooLarge(limit));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;

    fn private(env: &TestEnv, rel: &str, text: &str, mode: u32) -> std::path::PathBuf {
        let p = env.write(rel, text);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn a_private_file_is_read() {
        let env = TestEnv::new();
        let p = private(&env, "private.txt", "fixture-bytes", 0o600);
        assert_eq!(read_private(&p, 64).unwrap(), b"fixture-bytes");
        let p = private(&env, "owner-only-read.txt", "fixture-bytes", 0o400);
        assert_eq!(read_private(&p, 64).unwrap(), b"fixture-bytes");
    }

    #[test]
    fn a_missing_file_is_missing() {
        let env = TestEnv::new();
        let err = read_private(&env.root().join("absent"), 64).unwrap_err();
        assert!(matches!(err, PrivateFileError::Missing), "{err:?}");
    }

    #[test]
    fn a_link_to_a_private_file_is_refused() {
        let env = TestEnv::new();
        let target = private(&env, "target.txt", "fixture-bytes", 0o600);
        let link = env.root().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = read_private(&link, 64).unwrap_err();
        assert!(matches!(err, PrivateFileError::SymbolicLink), "{err:?}");
    }

    #[test]
    fn group_or_world_bits_are_refused() {
        let env = TestEnv::new();
        for mode in [0o640, 0o604, 0o644, 0o660] {
            let p = private(&env, &format!("shared-{mode:o}.txt"), "fixture-bytes", mode);
            let err = read_private(&p, 64).unwrap_err();
            assert!(matches!(err, PrivateFileError::Permissions(m) if m == mode), "{mode:o}: {err:?}");
        }
    }

    #[test]
    fn a_directory_is_refused() {
        let env = TestEnv::new();
        let dir = env.root().join("dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = read_private(&dir, 64).unwrap_err();
        assert!(matches!(err, PrivateFileError::NotRegular), "{err:?}");
    }

    #[test]
    fn the_size_bound_holds() {
        let env = TestEnv::new();
        let p = private(&env, "sized.txt", "0123456789", 0o600);
        assert_eq!(read_private(&p, 10).unwrap(), b"0123456789");
        let err = read_private(&p, 9).unwrap_err();
        assert!(matches!(err, PrivateFileError::TooLarge(9)), "{err:?}");
    }
}
