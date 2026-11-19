//! The tool-child side of the owner confirm broker (yolo power plan S5).
//!
//! `oxa work --publish` and `--report-problems` ask the owner through the session's broker
//! socket (`OXA_CONFIRM_SOCK`, served by `oxa_core::owner_confirm`). The broker answers yes in
//! yolo, and otherwise asks the owner (dialog or typed message). Nothing this process reads
//! can answer: the only inputs are the broker's reply and, outside a session, `/dev/tty`.
use oxa_config::Env;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// The socket a session exported to its tool children, when there is one.
pub(crate) fn broker_socket(env: &Env) -> Option<&str> {
    env.get(oxa_core::owner_confirm::SOCK_ENV)
}

/// Ask the broker at `sock`. `Some(true)` yes, `Some(false)` no, `None` nobody could answer
/// (broker gone, no way to ask the owner, unreadable reply).
pub(crate) fn ask_broker(sock: &Path, question: &str) -> Option<bool> {
    let mut stream = UnixStream::connect(sock).ok()?;
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

/// The confirmation for the owner's publish and report actions: the session broker when the
/// session exported one (the agent marker is then no refusal, the broker is the owner), the
/// controlling terminal otherwise (`tty_confirm`, which refuses under the agent marker).
pub(crate) fn owner_confirm(env: &Env) -> impl FnMut(&str) -> Option<bool> + '_ {
    let mut tty = crate::report_cmd::tty_confirm(env);
    move |question: &str| match broker_socket(env) {
        Some(sock) => ask_broker(Path::new(sock), question),
        None => tty(question),
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

    #[test]
    fn confirm_uses_broker_under_agent_marker() {
        let d = dir("yolo");
        let broker = OwnerConfirm::start(&d, YoloSwitch::new(Autonomy::Yolo)).expect("start");
        let sock = broker.sock_path().to_string_lossy().into_owned();
        let env = Env::from_pairs(&[
            ("OXA_CODING_AGENT", "true"),
            (oxa_core::owner_confirm::SOCK_ENV, &sock),
        ]);
        assert_eq!(owner_confirm(&env)("Push?"), Some(true));
        assert_eq!(broker.take_records().len(), 1);
        broker.close();
        let _ = std::fs::remove_dir_all(d);
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
