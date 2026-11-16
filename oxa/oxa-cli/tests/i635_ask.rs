//! Ask is an owner interaction, not a worker-prefix feature. Fake engine only.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

const QUESTION: &str = r#"{"question":"Which format?","options":[{"label":"JSON","detail":"Machine readable"},{"label":"Text","detail":"Easy to read"}],"recommended":"JSON"}"#;

fn run(child: bool) -> (TestEnv, FakeServer, std::process::Output) {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("ask1", "ask", QUESTION),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.args(["-p", "Choose a format"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work());
    if child {
        cmd.env("OXA_SUBAGENT_DEPTH", "1");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(cmd.output().unwrap()).unwrap();
    });
    let out = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("ask must not block without an owner");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (env, server, out)
}

#[test]
fn print_ask_records_recommendation_and_never_blocks() {
    let (env, server, _) = run(false);
    let requests = server.requests();
    assert!(requests[0].tools_json.contains("\"name\":\"ask\""));
    let result = requests[1].messages.last().unwrap().content.as_deref().unwrap();
    assert!(
        result.contains("no owner; proceed on the recommended option"),
        "{result}"
    );
    assert!(result.contains("JSON"));
    let lines = std::fs::read_to_string(env.home().join(".oxa/agent/asks.jsonl")).unwrap();
    let record: Value = serde_json::from_str(lines.trim()).unwrap();
    assert_eq!(record["question"], "Which format?");
    assert_eq!(record["recommended"], "JSON");
    assert_eq!(record["options"].as_array().unwrap().len(), 2);
    assert_eq!(record["cwd"], json!(env.work()));
}

#[test]
fn worker_does_not_declare_ask() {
    let (env, server, _) = run(true);
    assert!(!server.requests()[0].tools_json.contains("\"name\":\"ask\""));
    assert!(!env.home().join(".oxa/agent/asks.jsonl").exists());
}

#[test]
fn rpc_ask_uses_existing_select_protocol_and_returns_owner_choice() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("ask1", "ask", QUESTION),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["--mode", "rpc"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
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
    writeln!(stdin, "{}", json!({"type":"prompt", "message":"Choose a format"})).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut selected = false;
        loop {
            let record = rx.recv_timeout(Duration::from_secs(10)).expect("RPC ask stalled");
            if record["type"] == "extension_ui_request" {
                assert_eq!(record["method"], "select");
                assert_eq!(record["title"], "Which format?");
                assert_eq!(
                    record["options"],
                    json!(["JSON (recommended): Machine readable", "Text: Easy to read"])
                );
                writeln!(
                    stdin,
                    "{}",
                    json!({
                        "type":"extension_ui_response", "id":record["id"], "value":record["options"][1]
                    })
                )
                .unwrap();
                selected = true;
            }
            if record["type"] == "agent_end" {
                assert!(selected, "must ask the owner before ending");
                break;
            }
        }
    }));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
    let requests = server.requests();
    assert!(requests[0].tools_json.contains("\"name\":\"ask\""));
    assert_eq!(
        requests[1].messages.last().unwrap().content.as_deref(),
        Some("Owner selected: Text")
    );
    assert!(!env.home().join(".oxa/agent/asks.jsonl").exists());
}
