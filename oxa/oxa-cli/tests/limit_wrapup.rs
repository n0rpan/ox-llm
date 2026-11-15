//! #332 step 4: limits finish active tools, save a final commit/session, and exit interrupted.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::io::Write;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn git(env: &TestEnv, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn fixture() -> TestEnv {
    let env = TestEnv::new();
    git(&env, &["init", "-q", "-b", "main"]);
    git(&env, &["config", "user.name", "Fixture"]);
    git(&env, &["config", "user.email", "fixture@example.invalid"]);
    git(&env, &["commit", "--allow-empty", "-qm", "Initial"]);
    env.write("work/.git/info/exclude", "started\n");
    env
}

fn server() -> FakeServer {
    FakeServer::start(vec![
        ScriptTurn::tool("edit","bash",r#"{"command":"printf draft > change.txt; printf started > started; sleep 0.5; printf finished > tool-finished"}"#),
        ScriptTurn::tool("save","bash",r#"{"command":"git add change.txt tool-finished && git commit -m 'Save interrupted work'"}"#),
    ],FakeOpts::default()).unwrap()
}

fn start(env: &TestEnv, server: &FakeServer, mode: &str, limit: bool) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .args(["--permission-mode", "workspace", "--yolo"]);
    if mode == "print" {
        command.args(["-p", "edit the file"]);
    } else if mode == "json" {
        command.args(["--mode", "json", "edit the file"]);
    } else {
        command.args(["--mode", "rpc"]);
    }
    if limit {
        command.args(["--max-time", "0.2"]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if mode == "rpc" {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"{\"type\":\"prompt\",\"message\":\"edit the file\"}\n")
            .unwrap();
    } else {
        child.stdin.take();
    }
    child
}

fn check(env: &TestEnv, server: &FakeServer, output: Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout:{}\nstderr:{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: serde_json::Value = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["type"] == "oxa_result")
        .expect("typed terminal result");
    assert_eq!(result["outcome"], "interrupted");
    assert_eq!(
        std::fs::read_to_string(env.work().join("tool-finished")).unwrap(),
        "finished",
        "the active tool was not cancelled"
    );
    assert!(
        String::from_utf8_lossy(&git(env, &["log", "-1", "--format=%s"]).stdout)
            .contains("Save interrupted work")
    );
    assert!(
        git(env, &["status", "--porcelain"]).stdout.is_empty(),
        "the wrap-up committed the work"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "only one additional final turn");
    assert!(requests[1].max_tokens <= 1024);
    assert!(!requests[1].thinking);
    let sessions = FsStore::new(env.agent_dir().join("sessions")).list(None);
    assert_eq!(sessions.len(), 1, "session is retained");
    let text = std::fs::read_to_string(&sessions[0].sref.path).unwrap();
    assert!(text.contains("Save interrupted work") && text.contains("tool-finished"));
}

#[test]
fn max_time_finishes_the_active_tool_then_allows_one_final_commit() {
    for mode in ["print", "json", "rpc"] {
        let env = fixture();
        let server = server();
        let mut child = start(&env, &server, mode, true);
        let _input = child.stdin.take();
        check(&env, &server, child.wait_with_output().unwrap());
    }
}

#[test]
fn sigterm_finishes_the_active_tool_then_allows_one_final_commit() {
    for mode in ["print", "json", "rpc"] {
        let env = fixture();
        let server = server();
        let mut child = start(&env, &server, mode, false);
        let started = Instant::now();
        while !env.work().join("started").exists() {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                child.kill().unwrap();
                panic!("tool never started");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success());
        // Keep RPC stdin open: SIGTERM must terminate the server without waiting for EOF.
        let _input = child.stdin.take();
        check(&env, &server, child.wait_with_output().unwrap());
    }
}

#[test]
fn wrap_up_can_summarize_and_stops_before_later_prompts() {
    let env = fixture();
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "edit",
                "bash",
                r#"{"command":"printf draft > change.txt; sleep 0.5; printf finished > tool-finished"}"#,
            ),
            ScriptTurn::text("Work retained; continue this session."),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    let output = command
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .args([
            "--yolo",
            "-p",
            "first instruction",
            "later instruction",
            "--max-time",
            "0.2",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Work retained; continue this session."
    );
    assert_eq!(server.requests().len(), 2);
    assert_eq!(
        std::fs::read_to_string(env.work().join("tool-finished")).unwrap(),
        "finished"
    );
    assert!(!format!("{:?}", server.requests()).contains("later instruction"));
}

#[test]
fn a_failed_final_request_retains_the_session_and_interrupted_outcome() {
    let env = fixture();
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "edit",
                "bash",
                r#"{"command":"printf draft > change.txt; sleep 0.5; printf finished > tool-finished"}"#,
            ),
            ScriptTurn::fault(oxa_fake::Fault::Http {
                status: 429,
                body:
                    serde_json::json!({"error":{"code":"budget_exceeded","message":"Daily budget exhausted"}})
                        .to_string(),
            }),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = start(&env, &server, "print", true).wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert!(stderr.contains("budget_exceeded"));
    assert_eq!(server.requests().len(), 2, "the final turn is not retried");
    assert_eq!(
        std::fs::read_to_string(env.work().join("tool-finished")).unwrap(),
        "finished"
    );
    assert_eq!(FsStore::new(env.agent_dir().join("sessions")).list(None).len(), 1);
}

#[test]
fn a_limit_during_done_checks_still_gets_the_final_turn() {
    let env = fixture();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    env.write(
        "work/.oxa/settings.json",
        r#"{"done":["sleep 0.5; printf finished > tool-finished"]}"#,
    );
    env.write("work/.git/info/exclude", "started\n.oxa/\n");
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("edit", "write", r#"{"path":"change.txt","content":"draft"}"#),
            ScriptTurn::text("completed"),
            ScriptTurn::tool(
                "save",
                "bash",
                r#"{"command":"git add change.txt tool-finished && git commit -m 'Save interrupted work'"}"#,
            ),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = start(&env, &server, "print", true).wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        server.requests().len(),
        3,
        "the final request follows the completed check"
    );
    assert!(
        String::from_utf8_lossy(&git(&env, &["log", "-1", "--format=%s"]).stdout)
            .contains("Save interrupted work")
    );
}
