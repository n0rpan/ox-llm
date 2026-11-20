//! The door: how a tool child reaches the owner confirm broker without a path.
//!
//! The broker makes one `socketpair(AF_UNIX, SOCK_DGRAM)` at start. It keeps one end and reads
//! datagrams from it; the other end is inherited by tool children (`OXA_CONFIRM_FD` names its
//! descriptor number). A client makes a stream socketpair, keeps one end and sends the other
//! through the door as `SCM_RIGHTS`; the broker serves that stream like an accepted connection.
//! There is no socket file, no directory, nothing to mount, hide, symlink or name in an
//! environment variable but a small number, and nothing is left behind when the session ends.
//!
//! The descriptor plumbing is in `oxa_sandbox::door` (this crate forbids unsafe code).
use oxa_config::Env;
use oxa_sandbox::door;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;

/// The environment variable a tool child finds the door's descriptor number in.
pub const FD_ENV: &str = "OXA_CONFIRM_FD";

/// A client connection to the session's broker, made through the door the process inherited.
#[derive(Debug)]
pub struct Conn {
    stream: UnixStream,
    door: RawFd,
}

/// Connect through the descriptor `OXA_CONFIRM_FD` names in `env`: it must be a datagram socket;
/// the client makes a stream pair and sends one end through the door.
pub fn connect(env: &Env) -> Option<Conn> {
    let fd: RawFd = env.get(FD_ENV)?.trim().parse().ok()?;
    if fd < 3 || !door::is_datagram_socket(fd) {
        return None;
    }
    let (mine, theirs) = door::stream_pair().ok()?;
    door::send_stream(fd, &theirs).ok()?;
    Some(Conn {
        stream: mine,
        door: fd,
    })
}

impl Conn {
    /// The stream to the broker, for [`super::exchange`] and [`super::post_on`].
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// One request, one reply.
    pub fn ask(&self, request: &super::Request) -> super::Reply {
        super::exchange(&self.stream, request)
    }

    /// `true` when the door was made by the session's own oxa: the process that created it is
    /// the same user, an ancestor of this process, and runs the same executable. Linux only
    /// (peer credentials); a creator in another pid namespace, or any other platform, is not
    /// verified. A model-run command can swap the descriptor it inherited, but it cannot make
    /// its own door count as the session's.
    pub fn peer_verified(&self) -> bool {
        door::creator_is_session_oxa(self.door)
    }
}
