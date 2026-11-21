//! Parent publication policy at the real RPC dispatch boundary. No GitHub calls.
use oxa_core::owner_confirm::{exchange, Origin, Publication, Reply, Request};
use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

fn ownerless_rpc_publication(flags: &[&str], note: Option<&str>) -> Reply {
    // macOS's default temp path can exceed the Unix socket path limit.
    let env = if cfg!(target_os = "macos") {
        TestEnv::new_in("/tmp")
    } else {
        TestEnv::new()
    };
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let agent = env.root().join("a");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["--mode", "rpc", "--no-session"])
        .args(flags)
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("OXA_AGENT_DIR", &agent)
        .current_dir(env.work())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (ready, wait) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(record) = serde_json::from_str::<Value>(&line) {
                if record["id"] == "ready" && record["type"] == "response" {
                    let _ = ready.send(record["success"].as_bool() == Some(true));
                    return;
                }
            }
        }
    });
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writeln!(stdin, "{}", json!({"type":"get_state", "id":"ready"})).unwrap();
        assert!(wait.recv_timeout(Duration::from_secs(15)).expect("RPC startup"));
        let socket = std::fs::read_dir(agent.join("tmp"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("confirm.sock"))
            .find(|path| path.exists())
            .expect("RPC session broker socket");
        let stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Exercise authorization only. Never send the post frame, even if approved.
        exchange(
            &stream,
            &Request::Publish(Publication {
                repository: "oxstack/ox-llm".into(),
                issue: None,
                title: "Synthetic report".into(),
                body: "Synthetic observation.".into(),
                note: note.map(str::to_string),
            }),
        )
    }));
    drop(stdin);
    let _ = child.kill();
    child.wait().unwrap();
    reader.join().unwrap();
    reply.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

#[test]
fn ownerless_rpc_does_not_install_a_publisher_for_client_approval() {
    for flag in ["--standard", "--auto"] {
        let reply = ownerless_rpc_publication(&[flag], None);
        assert!(!reply.approved);
        assert_eq!(reply.reason.as_deref(), Some("no_publisher"));
    }
}

#[test]
fn owner_selected_yolo_needs_no_rpc_dialog_but_still_refuses_offline() {
    let online = ownerless_rpc_publication(&["--yolo"], None);
    assert!(online.approved, "{online:?}");
    assert_eq!(online.origin, Origin::Yolo);
    let offline = ownerless_rpc_publication(&["--yolo", "--offline"], None);
    assert!(!offline.approved);
    assert!(offline.reason.unwrap_or_default().contains("offline mode is on"));
}

#[test]
fn the_parent_refuses_an_unsafe_preview_note_even_in_yolo() {
    for (note, reason) in [
        ("Warning\u{001b}", "control or bidirectional"),
        ("Warning\u{202e}", "control or bidirectional"),
        ("password=synthetic-fixture", "holds a secret"),
    ] {
        let reply = ownerless_rpc_publication(&["--yolo"], Some(note));
        assert!(!reply.approved);
        let refusal = reply.reason.unwrap_or_default();
        assert!(refusal.contains(reason), "{refusal}");
        assert!(!refusal.contains(note), "the refusal must not echo the note");
    }
}
