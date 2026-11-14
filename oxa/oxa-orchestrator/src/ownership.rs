//! Cooperative controller ownership, tied to a pinned journal inode, never a PID.
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub(crate) const PROTOCOL: &str = "journal-flock-v1";

/// Explicit normal release also unlocks copies temporarily inherited by an
/// unrelated concurrent fork before exec closes its CLOEXEC descriptors. Fatal
/// process death still falls back to the kernel's last-description-close rule.
pub(crate) struct Lease(File);
impl Lease {
    pub(crate) fn acquire(file: File) -> io::Result<Self> {
        lock(&file)?;
        Ok(Self(file))
    }
}
impl std::ops::Deref for Lease {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl std::ops::DerefMut for Lease {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        // SAFETY: live private owned file; normal authority ends with this lease.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn lock(file: &File) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::fd::FromRawFd;
    struct HeldFork {
        pid: libc::pid_t,
        release: File,
    }
    impl Drop for HeldFork {
        fn drop(&mut self) {
            // SAFETY: private pipe and known unreaped fixture child. Send a byte
            // rather than rely on EOF, since other concurrent forks can inherit
            // this descriptor too. Nothing identifies or signals an ambient PID.
            unsafe {
                libc::write(self.release.as_raw_fd(), b"x".as_ptr().cast(), 1);
                loop {
                    if libc::waitpid(self.pid, std::ptr::null_mut(), 0) >= 0 {
                        break;
                    }
                    if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }
    }
    #[test]
    fn normal_release_unlocks_a_description_temporarily_inherited_before_exec() {
        let env = TestEnv::new();
        let path = env.root().join("ownership");
        let lease = Lease::acquire(File::create(&path).unwrap()).unwrap();
        let mut descriptors = [-1; 2];
        // SAFETY: space for two descriptors; new private CLOEXEC pipe.
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 returned two new owned descriptors.
        let read = unsafe { File::from_raw_fd(descriptors[0]) };
        let write = unsafe { File::from_raw_fd(descriptors[1]) };
        // SAFETY: child performs only async-signal-safe syscalls, no Rust
        // allocation, destructors, locks or journal I/O before _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::close(write.as_raw_fd());
                let mut byte = 0u8;
                libc::read(read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        let child = HeldFork { pid, release: write };
        drop(read);
        drop(lease);
        // The child still holds the original open-file description. Waiting for
        // last-close here used to spuriously report another controller owner.
        let next = Lease::acquire(File::open(&path).unwrap()).unwrap();
        drop(next);
        drop(child);
    }
}
