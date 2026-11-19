//! The tool-child side of the owner confirm broker (yolo power plan S5).
//!
//! `oxa work --publish` and `--report-problems` ask the owner through the session's broker
//! socket (`OXA_CONFIRM_SOCK`, served by `oxa_core::owner_confirm`). The broker answers yes in
//! yolo, and otherwise asks the owner (dialog or typed message). Nothing this process reads
//! can answer: the only inputs are the broker's reply and, outside a session, `/dev/tty`.
//!
//! The socket path arrives through the environment, which a tool child's parent controls: a
//! model-run command can name a listener of its own. So the path alone proves nothing. On
//! Linux the kernel names the process that created the peer socket (`SO_PEERCRED`), and the
//! answer counts only when that peer is this child's session: the peer's effective user must
//! be this user, and its pid must either be invisible in this child's pid namespace (`0`: the
//! host side of the sandbox, which nothing a tool spawns can enter, because a tool's children
//! share its namespace). Visible peers are never trusted: a model-controlled wrapper can be a
//! same-uid strict ancestor with exactly the shape of an unsandboxed session. Off Linux no kernel check
//! exists and a same-user parent can replace the socket file unseen, so a tool child never
//! accepts a broker answer there: the agent-marker refusal stands and the owner publishes
//! from their own terminal (`tty_confirm`). The host-side protocol (the session performing
//! the publish after its own dialog) remains the stronger design if that convenience is
//! wanted off Linux.
use oxa_config::Env;
#[cfg(target_os = "linux")]
use serde_json::{json, Value};
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
#[cfg(target_os = "linux")]
use std::path::Path;

/// The socket a session exported to its tool children, when there is one.
#[cfg(target_os = "linux")]
pub(crate) fn broker_socket(env: &Env) -> Option<&str> {
    env.get(oxa_core::owner_confirm::SOCK_ENV)
}

/// A session broker this child may trust with its question. On Linux the path from the
/// environment is only a candidate: `ask_broker` proves the peer through the kernel before
/// it asks anything. Off Linux there is no proof a child could check (module docs), so there
/// is never a trusted broker and the agent-marker refusal stands.
pub(crate) fn session_broker(env: &Env) -> Option<&str> {
    #[cfg(target_os = "linux")]
    {
        broker_socket(env)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = env;
        None
    }
}

/// Ask the broker at `sock`. `Some(true)` yes, `Some(false)` no, `None` nobody could answer
/// (broker gone, peer not this child's session, no way to ask the owner, unreadable reply).
#[cfg(target_os = "linux")]
pub(crate) fn ask_broker(sock: &Path, question: &str) -> Option<bool> {
    let mut stream = UnixStream::connect(sock).ok()?;
    if !peer_is_session_broker(&stream) {
        return None;
    }
    let request = json!({
        "question": question,
        "preview_sha256": format!("{:x}", Sha256::digest(question.as_bytes())),
    });
    writeln!(stream, "{request}").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let reply: Value = serde_json::from_str(&line).ok()?;
    match reply.get("answer").and_then(Value::as_str)? {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// The peer must be the session that exported the socket, never a listener this child's
/// parent chose. The kernel reports the credentials of the process that created the peer
/// socket: its effective user must be this user, and its pid must name a host-side owner
/// process (the module docs explain the two proof paths).
#[cfg(target_os = "linux")]
fn peer_is_session_broker(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes only through `cred` and `len`, both live and correctly sized,
    // and the descriptor belongs to the connected `stream`.
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    } == 0;
    // SAFETY: geteuid has no preconditions and touches no memory.
    ok && cred.uid == unsafe { libc::geteuid() } && peer_pid_is_session(cred.pid)
}

#[cfg(target_os = "linux")]
fn peer_pid_is_session(pid: libc::pid_t) -> bool {
    pid == 0
}

/// The confirmation for the owner's publish and report actions: the session broker when the
/// session exported a provable one (the agent marker is then no refusal, the broker is the
/// owner), the controlling terminal otherwise (`tty_confirm`, which refuses under the agent
/// marker). Off Linux the broker cannot be proven from a child, so the marker refusal and the
/// terminal answer stand.
pub(crate) fn owner_confirm(env: &Env) -> impl FnMut(&str) -> Option<bool> + '_ {
    let mut tty = crate::report_cmd::tty_confirm(env);
    move |question: &str| {
        #[cfg(target_os = "linux")]
        if let Some(sock) = session_broker(env) {
            return ask_broker(Path::new(sock), question);
        }
        tty(question)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_core::owner_confirm::OwnerConfirm;
    use oxa_proto::{Autonomy, YoloSwitch};

    fn dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oxa-ocl-{name}-{}", std::process::id()))
    }

    /// The kernel names the creator of the peer socket: a socket this process bound itself
    /// (what a parent wrapping its own listener into the child's exec looks like) is not a
    /// session broker, and no question is asked through it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_socket_this_process_bound_is_not_a_session_broker() {
        let d = dir("self");
        let broker = OwnerConfirm::start(&d, YoloSwitch::new(Autonomy::Yolo)).expect("start");
        let sock = broker.sock_path().to_string_lossy().into_owned();
        let env = Env::from_pairs(&[
            ("OXA_CODING_AGENT", "true"),
            (oxa_core::owner_confirm::SOCK_ENV, &sock),
        ]);
        assert_eq!(owner_confirm(&env)("Push?"), None);
        assert!(broker.take_records().is_empty(), "no question was asked");
        broker.close();
        let _ = std::fs::remove_dir_all(d);
    }

    /// A model can put its listener in a parent wrapper and exec the CLI below it. Such a
    /// same-uid ancestor is still visible in the child's namespace and must not approve.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_visible_ancestor_is_not_a_session_broker() {
        assert!(!peer_pid_is_session(unsafe { libc::getppid() }));
    }

    #[test]
    fn confirm_none_without_broker_under_agent_marker() {
        let env = Env::from_pairs(&[("OXA_CODING_AGENT", "true")]);
        assert_eq!(owner_confirm(&env)("Push?"), None);
        let gone = Env::from_pairs(&[
            ("OXA_CODING_AGENT", "true"),
            (oxa_core::owner_confirm::SOCK_ENV, "/nonexistent/confirm.sock"),
        ]);
        assert_eq!(owner_confirm(&gone)("Push?"), None);
    }

    /// Off Linux no kernel check exists (module docs): even a real session broker cannot
    /// prove itself to a tool child, so the marker refusal stands.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_a_session_socket_is_not_trusted_under_the_marker() {
        let d = dir("other");
        let broker = OwnerConfirm::start(&d, YoloSwitch::new(Autonomy::Yolo)).expect("start");
        let sock = broker.sock_path().to_string_lossy().into_owned();
        let env = Env::from_pairs(&[
            ("OXA_CODING_AGENT", "true"),
            (oxa_core::owner_confirm::SOCK_ENV, &sock),
        ]);
        assert_eq!(owner_confirm(&env)("Push?"), None);
        broker.close();
        let _ = std::fs::remove_dir_all(d);
    }
}
