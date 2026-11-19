//! The fork offer for a session from another directory, asked over RPC when a client is
//! listening (yolo power plan S15). Fake engine only.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// A session `fork-owner-1` in the work directory.
fn seed(env: &TestEnv, server: &FakeServer) {
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["-p", "--session-id", "fork-owner-1", "one"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// An RPC client for `fork-owner-1` from `other`, with an owner present.
fn spawn_rpc(env: &TestEnv, server: &FakeServer, other: &Path) -> (Child, ChildStdin, Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["--mode", "rpc", "--session", "fork-owner-1"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("OXA_RPC_OWNER_PRESENT", "1")
        .current_dir(other)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(record) = serde_json::from_str::<Value>(&line) {
                if tx.send(record).is_err() {
                    break;
                }
            }
        }
    });
    (child, stdin, rx)
}

/// The first extension-UI request of the run: the fork offer.
fn next_request(rx: &Receiver<Value>) -> Value {
    loop {
        let record = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the fork offer must reach the owner over RPC");
        if record["type"] == "extension_ui_request" {
            return record;
        }
    }
}

fn other_dir(env: &TestEnv) -> std::path::PathBuf {
    let other = env.root().join("other-fork");
    std::fs::create_dir_all(&other).unwrap();
    other
}

#[test]
fn rpc_fork_question_reaches_owner() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("first"), ScriptTurn::text("done")],
        FakeOpts::default(),
    )
    .unwrap();
    seed(&env, &server);
    let (mut child, mut stdin, rx) = spawn_rpc(&env, &server, &other_dir(&env));
    let request = next_request(&rx);
    assert_eq!(request["method"], "confirm");
    assert!(request["title"]
        .as_str()
        .unwrap()
        .starts_with("Session found in different project:"));
    assert_eq!(request["message"], "Fork this session into current directory?");
    writeln!(
        stdin,
        "{}",
        json!({"type":"extension_ui_response", "id":request["id"], "confirmed":true})
    )
    .unwrap();
    writeln!(stdin, "{}", json!({"type":"prompt", "message":"four"})).unwrap();
    let mut answered = false;
    while let Ok(record) = rx.recv_timeout(Duration::from_secs(10)) {
        if record["type"] == "agent_end" {
            answered = true;
            break;
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    assert!(answered, "the forked session must answer the prompt");
    // The yes forked the history into this directory: the prompt went out with it.
    let requests = server.requests();
    let texts: Vec<String> = requests
        .last()
        .expect("the forked prompt reaches the engine")
        .messages
        .iter()
        .filter_map(|m| m.content.clone())
        .collect();
    assert!(texts.contains(&"one".to_string()), "{texts:?}");
    assert!(texts.contains(&"four".to_string()), "{texts:?}");
}

#[test]
fn rpc_fork_question_declined_aborts_without_a_session() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("first")], FakeOpts::default()).unwrap();
    seed(&env, &server);
    let (mut child, mut stdin, rx) = spawn_rpc(&env, &server, &other_dir(&env));
    let request = next_request(&rx);
    writeln!(
        stdin,
        "{}",
        json!({"type":"extension_ui_response", "id":request["id"], "confirmed":false})
    )
    .unwrap();
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("RPC did not exit after the fork was declined");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    assert_eq!(server.requests().len(), 1, "nothing is sent after a decline");
}
