//! #332 step 4: limits finish active tools, save a final commit/session, and exit interrupted.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::io::{Read, Write};
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
    start_with_max_time(env, server, mode, limit.then_some("0.2"))
}

fn start_with_max_time(env: &TestEnv, server: &FakeServer, mode: &str, max_time: Option<&str>) -> Child {
    start_with_limits(env, server, mode, max_time.map(|n| ("--max-time", n)))
}

fn start_with_limits(env: &TestEnv, server: &FakeServer, mode: &str, limit: Option<(&str, &str)>) -> Child {
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
    if let Some((flag, value)) = limit {
        command.args([flag, value]);
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
fn supervised_children_exit_on_the_first_sigterm() {
    use std::os::unix::process::ExitStatusExt;
    for mode in ["print", "json", "rpc"] {
        for (identity, deadline) in [
            ("depth", false),
            ("hint", false),
            ("malformed-depth", false),
            ("depth", true),
        ] {
            let env = fixture();
            let mut held = ScriptTurn::text("never sent");
            held.fault = Some(oxa_fake::Fault::HoldHeaders { ms: 60_000 });
            let server = FakeServer::start(vec![held], FakeOpts::default()).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
            command
                .current_dir(env.work())
                .env_clear()
                .envs(env.envs(&server.url()));
            if identity == "hint" {
                command.env("OXA_SESSION_HINT", "fixture.s1");
            } else {
                command.env(
                    "OXA_SUBAGENT_DEPTH",
                    if identity == "depth" { "1" } else { "child" },
                );
            }
            if mode == "print" {
                command.args(["-p", "go"]);
            } else if mode == "rpc" {
                command.args(["--mode", "rpc"]);
            } else {
                command.args(["--mode", mode, "go"]);
            }
            if deadline {
                command.args(["--max-time", "60"]);
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            if mode == "rpc" {
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"{\"type\":\"prompt\",\"message\":\"go\"}\n")
                    .unwrap();
            } else {
                drop(child.stdin.take());
            }
            let started = Instant::now();
            while server.requests().is_empty() {
                if started.elapsed() > Duration::from_secs(10) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("the supervised request did not start: {mode}/{identity}");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap()
                .success());
            let sent = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if sent.elapsed() > Duration::from_secs(2) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("supervisor SIGTERM was treated as cooperative wrap-up: {mode}/{identity}");
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(status.signal(), Some(15), "{mode}/{identity}");
            assert_eq!(server.requests().len(), 1, "no child final turn");
        }
    }
}

#[test]
fn max_tokens_finishes_the_active_tool_then_allows_one_final_commit() {
    for mode in ["print", "json", "rpc"] {
        let env = fixture();
        let server = server();
        let mut child = start_with_limits(&env, &server, mode, Some(("--max-tokens", "1")));
        let _input = child.stdin.take();
        check(&env, &server, child.wait_with_output().unwrap());
    }
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

// Failure-only facts: no stderr, request bodies, paths, IDs or session text.
fn wrap_up_diagnostics(env: &TestEnv, server: &FakeServer, output: &Output) -> String {
    const SUMMARY: &str = "Work retained; continue this session.";
    fn tag(value: Option<&str>) -> &'static str {
        match value {
            None => "none",
            Some("done") => "done",
            Some("interrupted") => "interrupted",
            Some("failed") => "failed",
            Some("blocked") => "blocked",
            Some("aborted") => "aborted",
            Some("engine_http") => "engine_http",
            Some("engine_down") => "engine_down",
            Some("stream_broken") => "stream_broken",
            Some("timeout") => "timeout",
            Some("budget_exhausted") => "budget_exhausted",
            Some("chat") => "chat",
            Some("compaction") => "compaction",
            Some("branch_summary") => "branch_summary",
            Some("prefill") => "prefill",
            Some("subagent") => "subagent",
            _ => "other",
        }
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let terminal = stderr
        .lines()
        .filter(|line| line.len() <= 16 * 1024)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["type"] == "oxa_result")
        .next_back()
        .unwrap_or_default();
    let requests = server.requests();
    let mut facts = format!(
        "exit={:?} stdout_bytes={} stdout_summary={} stderr_summary={} outcome={} error_class={} terminal_requests={:?} requests={}",
        output.status.code(), output.stdout.len(), stdout.contains(SUMMARY), stderr.contains(SUMMARY),
        tag(terminal["outcome"].as_str()), tag(terminal["error_class"].as_str()),
        terminal["requests"].as_u64(), requests.len(),
    );
    for (index, request) in requests.iter().take(4).enumerate() {
        facts.push_str(&format!(
            " request{index}[max_tokens={} thinking={} kind={} wrap_up={} later_instruction={} reply_text={} reply_tools={}]",
            request.max_tokens, request.thinking, tag(Some(&request.kind)),
            request.messages.iter().any(|m| m.content.as_deref().is_some_and(|s| s.contains("Wrap up now: this run is ending."))),
            request.messages.iter().any(|m| m.content.as_deref().is_some_and(|s| s.contains("later instruction"))),
            request.reply.as_ref().is_some_and(|m| m.content.as_deref().is_some_and(|s| !s.is_empty())),
            request.reply.as_ref().map_or(0, |m| m.tool_calls.len()),
        ));
    }
    let mut marker = String::new();
    let marker_read = std::fs::File::open(env.work().join("tool-finished"))
        .and_then(|file| file.take(16).read_to_string(&mut marker))
        .is_ok();
    facts.push_str(&format!(" tool_complete={}", marker_read && marker == "finished"));
    let sessions = FsStore::new(env.agent_dir().join("sessions")).list(None);
    facts.push_str(&format!(" sessions={}", sessions.len()));
    for (index, session) in sessions.iter().take(2).enumerate() {
        let mut bytes = Vec::new();
        let read = std::fs::File::open(&session.sref.path)
            .and_then(|file| file.take(256 * 1024 + 1).read_to_end(&mut bytes))
            .is_ok();
        let truncated = bytes.len() > 256 * 1024;
        let (mut summary_msg, mut summary_shown) = (false, false);
        let (mut last_visible, mut shown_why) = ("none", "none");
        if read && !truncated {
            for line in bytes.split(|b| *b == b'\n') {
                if let Ok(entry) = oxa_session::decode_entry(line) {
                    match entry.body {
                        oxa_proto::EntryBody::Msg { wire, .. } => {
                            summary_msg |= wire.content.as_deref().is_some_and(|s| s.contains(SUMMARY));
                            last_visible = wire.role.as_str();
                            shown_why = "none";
                        }
                        oxa_proto::EntryBody::Shown { text, why } => {
                            summary_shown |= text.contains(SUMMARY);
                            last_visible = "shown";
                            shown_why = why;
                        }
                        _ => {}
                    }
                }
            }
        }
        facts.push_str(&format!(" session{index}[read={read} truncated={truncated} summary_msg={summary_msg} summary_shown={summary_shown} last_msg_or_shown={last_visible} shown_why={shown_why}]"));
    }
    facts
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
    let diagnostics = || wrap_up_diagnostics(&env, &server, &output);
    assert_eq!(output.status.code(), Some(130), "{}", diagnostics());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Work retained; continue this session.",
        "{}",
        diagnostics()
    );
    assert_eq!(server.requests().len(), 2, "{}", diagnostics());
    assert_eq!(
        std::fs::read_to_string(env.work().join("tool-finished"))
            .unwrap_or_else(|_| panic!("tool marker unreadable: {}", diagnostics())),
        "finished",
        "{}",
        diagnostics()
    );
    assert!(
        !format!("{:?}", server.requests()).contains("later instruction"),
        "{}",
        diagnostics()
    );
}

#[test]
fn a_failed_final_request_retains_the_session_and_typed_failure() {
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
    assert_eq!(output.status.code(), Some(oxa_proto::EXIT_BUDGET), "{stderr}");
    assert!(stderr.contains("budget_exceeded"));
    assert_eq!(server.requests().len(), 2, "the final turn is not retried");
    assert_eq!(
        std::fs::read_to_string(env.work().join("tool-finished")).unwrap(),
        "finished"
    );
    assert_eq!(FsStore::new(env.agent_dir().join("sessions")).list(None).len(), 1);
}

#[test]
fn max_time_during_done_checks_still_gets_the_final_turn() {
    limit_during_done_checks(true);
}

#[test]
fn sigterm_during_done_checks_still_gets_the_final_turn() {
    limit_during_done_checks(false);
}

fn limit_during_done_checks(max_time: bool) {
    let env = fixture();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    // Leave five seconds for startup/replies in the deadline case, and keep the done
    // command active longer than that deadline. The signal case synchronizes on started.
    let seconds = if max_time { 6.0 } else { 0.5 };
    env.write(
        "work/.oxa/settings.json",
        &serde_json::json!({"done": [format!(
            "printf started > started; sleep {seconds}; printf finished > tool-finished"
        )]})
        .to_string(),
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
    let mut child = start_with_max_time(&env, &server, "print", max_time.then_some("5"));
    let started = Instant::now();
    while !env.work().join("started").exists() {
        if child.try_wait().unwrap().is_some() || started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "done check did not start: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !max_time {
        assert!(Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success());
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    let reason = if max_time {
        "time limit reached"
    } else {
        "SIGTERM received"
    };
    assert!(stderr.contains(reason), "expected {reason}: {stderr}");
    assert_eq!(
        server.requests().len(),
        3,
        "the final request follows the completed check"
    );
    assert_eq!(
        std::fs::read_to_string(env.work().join("tool-finished")).unwrap(),
        "finished"
    );
    assert!(
        String::from_utf8_lossy(&git(&env, &["log", "-1", "--format=%s"]).stdout)
            .contains("Save interrupted work"),
        "stdout:{}\nstderr:{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn sigterm_during_wrap_up_restores_default_termination() {
    use std::os::unix::process::ExitStatusExt;
    for limit in [false, true] {
        let env = fixture();
        let server = FakeServer::start(
            vec![ScriptTurn::tool(
                "hang",
                "bash",
                r#"{"command":"printf started > started; sleep 30"}"#,
            )],
            FakeOpts::default(),
        )
        .unwrap();
        let mut child = start(&env, &server, "print", limit);
        let started = Instant::now();
        while !env.work().join("started").exists() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(env.work().join("started").exists());
        let terminate = || {
            assert!(Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap()
                .success())
        };
        if !limit {
            terminate();
        }
        // Let either the deadline or first signal request cooperative wrap-up.
        std::thread::sleep(Duration::from_millis(300));
        terminate();
        let sent = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if sent.elapsed() > Duration::from_secs(1) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("SIGTERM during wrap-up was swallowed (limit={limit})");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.signal(), Some(15));
    }
}

#[test]
fn max_tokens_counts_output_but_not_cached_prompt_tokens() {
    let env = fixture();
    let script = || {
        vec![
            ScriptTurn::tool("read", "read", r#"{"path":".git/info/exclude"}"#),
            ScriptTurn::text("finished"),
            ScriptTurn::text("unexpected wrap-up"),
        ]
    };
    let baseline = FakeServer::start(script(), FakeOpts::default()).unwrap();
    let output = start_with_limits(&env, &baseline, "print", None)
        .wait_with_output()
        .unwrap();
    assert!(output.status.success());
    let requests = baseline.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].usage.unwrap().cached > 0);
    let spent: u64 = requests
        .iter()
        .map(|r| {
            let u = r.usage.unwrap();
            u64::from(u.prompt.saturating_sub(u.cached)) + u64::from(u.completion)
        })
        .sum();

    // One token above actual usage must complete, even though the full prompt
    // (most of it cached) would exceed the budget on the second reply.
    let budget = (spent + 1).to_string();
    let server = FakeServer::start(script(), FakeOpts::default()).unwrap();
    let output = start_with_limits(&env, &server, "print", Some(("--max-tokens", &budget)))
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.requests().len(), 2);

    // At the exact boundary the output tokens must count too: wrap up once.
    let budget = spent.to_string();
    let server = FakeServer::start(script(), FakeOpts::default()).unwrap();
    let output = start_with_limits(&env, &server, "print", Some(("--max-tokens", &budget)))
        .wait_with_output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].max_tokens <= 1024);
}
