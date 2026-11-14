//! Cooperative controller ownership, tied to a pinned journal inode, never a PID.
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub(crate) const PROTOCOL: &str = "journal-flock-v1";

pub(crate) fn lock(file: &File) -> io::Result<()> {
    // SAFETY: live owned descriptor. Nonblocking acquisition never waits for
    // another controller, and the kernel releases ownership on descriptor close.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "run journal is owned by another controller",
            ))
        } else {
            Err(error)
        }
    }
}
