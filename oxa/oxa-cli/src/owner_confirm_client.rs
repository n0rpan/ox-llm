//! The tool-child side of the owner confirm broker (yolo power plan S5).
//!
//! `oxa work --publish` and `--report-problems` ask the owner through the session's broker
//! socket (`OXA_CONFIRM_SOCK`, served by `oxa_core::owner_confirm`). The broker answers yes in
//! yolo, and otherwise asks the owner (dialog or typed message): the owner's typed yes in oxa
//! counts, text the model read never does. Nothing this process reads can answer: the only
//! inputs are the broker's reply and, outside a session, `/dev/tty`.
//!
//! The socket path arrives through the environment, which a tool child's parent controls: a
//! model-run command can name a listener of its own. Where that matters, the child proves the
//! peer. Inside the Linux sandbox (a user namespace of its own, with nested user namespaces
//! denied by bwrap whenever the broker is mounted) the kernel names the process that created
//! the peer socket (`SO_PEERCRED`): the answer counts only when the peer is this user and its
//! pid is invisible in this child's pid namespace (`0`: the host side of the sandbox, which
//! nothing a tool spawns can enter). A visible peer there is a listener the model made and is
//! refused. Without an OS sandbox (the interactive and yolo standard, and every platform
//! other than Linux) a command already runs with the owner's full rights: it could run `gh`
//! itself, so a peer check would protect nothing the command cannot do anyway. There the
//! session's broker is used as exported, so the owner's dialog or typed yes works the same on
//! every platform, and the broker itself still never answers from model-read text.
use oxa_config::Env;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// The socket a session exported to its tool children, when there is one. `ask_broker`
/// proves the peer where the child can (inside the Linux sandbox) before it asks anything.
pub(crate) fn session_broker(env: &Env) -> Option<&str> {
    env.get(oxa_core::owner_confirm::SOCK_ENV)
}

/// Ask the broker at `sock`. `Some(true)` yes, `Some(false)` no, `None` nobody could answer
/// (broker gone, peer not this child's session, no way to ask the owner, unreadable reply).
pub(crate) fn ask_broker(sock: &Path, question: &str) -> Option<bool> {
    let mut stream = UnixStream::connect(sock).ok()?;
    #[cfg(target_os = "linux")]
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
/// parent chose, wherever the child can tell the two apart. The kernel reports the
/// credentials of the process that created the peer socket: its effective user must be this
/// user, and inside the sandbox its pid must name a host-side process (module docs).
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
    ok && cred.uid == unsafe { libc::geteuid() } && peer_pid_accepted(cred.pid, in_sandbox_userns())
}

/// Inside the sandbox only a host-side peer (pid `0` here) is the session; without a sandbox
/// the session is an ordinary visible process and the command holds the owner's rights anyway.
#[cfg(target_os = "linux")]
fn peer_pid_accepted(pid: libc::pid_t, sandboxed: bool) -> bool {
    pid == 0 || !sandboxed
}

/// `true` unless this process is in the initial user namespace (`uid_map` maps the whole id
/// range onto itself). bwrap gives every sandboxed command a user namespace of its own and,
/// with the broker mounted, denies nested ones, so a command cannot leave it or fake this.
/// An unreadable map counts as sandboxed (fail closed).
#[cfg(target_os = "linux")]
fn in_sandbox_userns() -> bool {
    match std::fs::read_to_string("/proc/self/uid_map") {
        Ok(map) => !is_initial_uid_map(&map),
        Err(_) => true,
    }
}

#[cfg(target_os = "linux")]
fn is_initial_uid_map(map: &str) -> bool {
    let rows: Vec<Vec<&str>> = map
        .lines()
        .map(|line| line.split_whitespace().collect())
        .filter(|row: &Vec<&str>| !row.is_empty())
        .collect();
    rows == [vec!["0", "0", "4294967295"]]
}

/// The confirmation for the owner's publish and report actions: the session broker when the
/// session exported one (the agent marker is then no refusal, the broker is the owner), the
/// controlling terminal otherwise (`tty_confirm`, which refuses under the agent marker).
pub(crate) fn owner_confirm(env: &Env) -> impl FnMut(&str) -> Option<bool> + '_ {
    let mut tty = crate::report_cmd::tty_confirm(env);
    move |question: &str| {
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

    /// Without an OS sandbox the session's broker answers for the owner (here yolo), on every
    /// platform: the owner's yes in oxa counts, never a terminal-only path.
    #[test]
    fn an_unsandboxed_child_reaches_the_session_broker() {
        #[cfg(target_os = "linux")]
        if in_sandbox_userns() {
            return; // The test itself runs sandboxed; the sandbox rule is tested below.
        }
        let d = dir("self");
        let broker = OwnerConfirm::start(&d, YoloSwitch::new(Autonomy::Yolo)).expect("start");
        let sock = broker.sock_path().to_string_lossy().into_owned();
        let env = Env::from_pairs(&[
            ("OXA_CODING_AGENT", "true"),
            (oxa_core::owner_confirm::SOCK_ENV, &sock),
        ]);
        assert_eq!(owner_confirm(&env)("Push?"), Some(true));
        assert_eq!(broker.take_records().len(), 1, "the broker recorded the answer");
        broker.close();
        let _ = std::fs::remove_dir_all(d);
    }

    /// Inside the sandbox a model can put its listener in a parent wrapper and exec the CLI
    /// below it. Such a same-uid peer is visible in the child's namespace and must not
    /// approve; only the host-side session (pid 0 here) does.
    #[cfg(target_os = "linux")]
    #[test]
    fn inside_the_sandbox_only_a_host_side_peer_is_the_broker() {
        let visible = unsafe { libc::getppid() };
        assert!(!peer_pid_accepted(visible, true));
        assert!(peer_pid_accepted(0, true));
        assert!(peer_pid_accepted(visible, false));
        assert!(is_initial_uid_map("         0          0 4294967295\n"));
        assert!(!is_initial_uid_map("      1000       1000          1\n"));
        assert!(!is_initial_uid_map(""));
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
}
