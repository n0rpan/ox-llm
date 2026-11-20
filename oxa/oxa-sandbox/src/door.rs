//! The owner confirm door's descriptor plumbing: how a tool child reaches the broker without a
//! path. (Here because oxa-core forbids unsafe code; the broker and its clients use this safe API.)
//!
//!
//! The broker makes one `socketpair(AF_UNIX, SOCK_DGRAM)` at start. It keeps one end and reads
//! datagrams from it; the other end is inherited by tool children (`OXA_CONFIRM_FD` names its
//! descriptor number). A client makes a stream socketpair, keeps one end and sends the other
//! through the door as `SCM_RIGHTS`; the broker serves that stream like an accepted connection.
//! There is no socket file, no directory, nothing to mount, hide, symlink or name in an
//! environment variable but a small number, and nothing is left behind when the session ends.
//!
//! Both door ends are close-on-exec in the parent; the sandbox clears the flag on the child end
//! in the child only, for a command that gets a broker.
#![allow(unused_unsafe)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor the caller owns.
    unsafe {
        let flags = cvt(libc::fcntl(fd, libc::F_GETFD))?;
        cvt(libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
    }
    Ok(())
}

fn socketpair(kind: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: socketpair writes two descriptors into the array.
    cvt(unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) })?;
    // SAFETY: both descriptors are fresh and owned here.
    let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(a.as_raw_fd())?;
    set_cloexec(b.as_raw_fd())?;
    Ok((a, b))
}

/// The door: `(broker end, child end)`.
pub fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    socketpair(libc::SOCK_DGRAM)
}

/// Send `stream` through the door `door` as one datagram carrying one descriptor.
pub fn send_stream(door: RawFd, stream: &UnixStream) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let space = {
        // SAFETY: CMSG_SPACE is pure arithmetic.
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize }
    };
    let mut control = vec![0u8; space];
    // SAFETY: a zeroed msghdr is a valid empty message; the fields below are set before use.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer is large enough for one header and one descriptor.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = 0;
    // SAFETY: sendmsg with a message built above; the descriptor stays open during the call.
    cvt(unsafe { libc::sendmsg(door, &msg, flags) as libc::c_int }).map(|_| ())
}

/// Wait up to `millis` for a datagram on `door`.
pub fn wait_readable(door: &OwnedFd, millis: i32) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: door.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll on one valid pollfd.
    let rc = unsafe { libc::poll(&mut poll, 1, millis) };
    if rc < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        };
    }
    Ok(rc > 0)
}

/// Read one datagram from the door. `Some(stream)` when it carried exactly one descriptor that
/// is a stream socket; anything else is dropped (`None`), extra descriptors are closed.
pub fn recv_stream(door: &OwnedFd) -> io::Result<Option<UnixStream>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // Room for a few descriptors, so a sender of many is seen and refused, not truncated.
    let space = {
        // SAFETY: CMSG_SPACE is pure arithmetic.
        unsafe { libc::CMSG_SPACE((8 * std::mem::size_of::<RawFd>()) as u32) as usize }
    };
    let mut control = vec![0u8; space];
    // SAFETY: a zeroed msghdr is a valid empty message; the fields below are set before use.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: recvmsg into buffers owned by this frame.
    let n = unsafe { libc::recvmsg(door.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let mut received: Vec<OwnedFd> = Vec::new();
    // SAFETY: walking the control messages the kernel filled in; each SCM_RIGHTS payload holds
    // whole descriptors, which this process now owns.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let header = libc::CMSG_LEN(0) as usize;
                let count = ((*cmsg).cmsg_len as usize).saturating_sub(header) / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..count {
                    received.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if received.len() != 1 || msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Ok(None);
    }
    let fd = received.remove(0);
    if !is_stream_socket(fd.as_raw_fd()) {
        return Ok(None);
    }
    Ok(Some(UnixStream::from(fd)))
}

fn socket_type(fd: RawFd) -> Option<libc::c_int> {
    let mut kind: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt into an int of the stated length.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            std::ptr::addr_of_mut!(kind).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(kind)
}

fn is_stream_socket(fd: RawFd) -> bool {
    socket_type(fd) == Some(libc::SOCK_STREAM)
}

/// A stream socket pair (close-on-exec on both ends).
pub fn stream_pair() -> io::Result<(UnixStream, UnixStream)> {
    let (a, b) = socketpair(libc::SOCK_STREAM)?;
    Ok((UnixStream::from(a), UnixStream::from(b)))
}

/// `true` when `fd` is an open datagram socket: what a door's child end is.
pub fn is_datagram_socket(fd: RawFd) -> bool {
    socket_type(fd) == Some(libc::SOCK_DGRAM)
}

/// `true` when `fd` is open with close-on-exec set.
pub fn is_cloexec(fd: RawFd) -> bool {
    // SAFETY: fcntl F_GETFD reads the flags of a descriptor number; it changes nothing.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags >= 0 && flags & libc::FD_CLOEXEC != 0
}

/// `true` when the door `door` was made by the session's own oxa: the process that created it is
/// the same user, an ancestor of this process, and runs the same executable. Linux only (peer
/// credentials); a creator in another pid namespace, or any other platform, is not verified.
pub fn creator_is_session_oxa(door: RawFd) -> bool {
    verify_creator(door)
}

#[cfg(target_os = "linux")]
fn verify_creator(door: RawFd) -> bool {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt on a descriptor of this process, into a ucred of the stated length.
    let rc = unsafe {
        libc::getsockopt(
            door,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    // SAFETY: geteuid has no preconditions.
    if rc != 0 || cred.pid <= 0 || cred.uid != unsafe { libc::geteuid() } {
        return false;
    }
    let Ok(creator) = u32::try_from(cred.pid) else {
        return false;
    };
    let exe_of = |pid: u32| {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| std::path::PathBuf::from(p.to_string_lossy().trim_end_matches(" (deleted)")))
    };
    let (Some(own), Some(theirs)) = (exe_of(std::process::id()), exe_of(creator)) else {
        return false;
    };
    let mut ancestors = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..64 {
        let Some(parent) = parent_pid(pid) else { break };
        if parent <= 1 {
            break;
        }
        ancestors.push(parent);
        pid = parent;
    }
    is_session_oxa(creator, &ancestors, &theirs, &own)
}

#[cfg(not(target_os = "linux"))]
fn verify_creator(_door: RawFd) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`: comm may hold spaces and parentheses, so cut after the last `)`.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The verdict on a door's creator: one of the consumer's ancestors that runs the consumer's own
/// executable.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_session_oxa(
    creator: u32,
    ancestors: &[u32],
    creator_exe: &std::path::Path,
    own_exe: &std::path::Path,
) -> bool {
    ancestors.contains(&creator) && creator_exe == own_exe
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_door_creator_is_trusted_only_as_an_ancestor_running_the_same_executable() {
        let oxa = Path::new("/usr/bin/oxa");
        assert!(is_session_oxa(10, &[20, 10, 5], oxa, oxa));
        assert!(!is_session_oxa(99, &[20, 10, 5], oxa, oxa));
        assert!(!is_session_oxa(
            10,
            &[20, 10, 5],
            Path::new("/usr/bin/python3"),
            oxa
        ));
    }

    #[test]
    fn a_stream_sent_through_the_door_arrives_and_other_datagrams_are_dropped() {
        use std::io::{Read, Write};
        let (parent, child) = pair().expect("door");
        let (mine, theirs) = socketpair(libc::SOCK_STREAM).expect("pair");
        let (mine, theirs) = (UnixStream::from(mine), UnixStream::from(theirs));
        send_stream(child.as_raw_fd(), &theirs).expect("send");
        let mut got = recv_stream(&parent).expect("recv").expect("a stream");
        got.write_all(b"hi").expect("write");
        let mut buf = [0u8; 2];
        (&mine).read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"hi");
        // A datagram without a descriptor is dropped.
        // SAFETY: send one byte on a valid descriptor.
        let sent = unsafe { libc::send(child.as_raw_fd(), b"x".as_ptr().cast(), 1, 0) };
        assert_eq!(sent, 1);
        assert!(recv_stream(&parent).expect("recv").is_none());
        // A descriptor that is not a stream socket (the door itself) is dropped.
        let (_other, other_child) = pair().expect("door");
        let datagram = socketpair(libc::SOCK_DGRAM).expect("dgram").0;
        // SAFETY: wrap a valid datagram descriptor to send it as if it were a stream.
        let wrong = unsafe { UnixStream::from_raw_fd(libc::dup(datagram.as_raw_fd())) };
        send_stream(child.as_raw_fd(), &wrong).expect("send");
        assert!(recv_stream(&parent).expect("recv").is_none());
        drop(other_child);
    }
}
