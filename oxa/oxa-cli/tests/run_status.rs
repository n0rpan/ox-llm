//! Worker status is a bounded snapshot, usable without reading the event stream.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_proto::{Entry, EntryBody, Finish, Part, ToolCall, WireMsg};
use oxa_session::{FsStore, SessionRef, SessionStore};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(env: &TestEnv, url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.env_clear().envs(env.envs(url)).current_dir(env.work());
    cmd
}

fn session(env: &TestEnv) -> oxa_session::SessionRef {
    FsStore::new(env.agent_dir().join("sessions"))
        .list(None)
        .into_iter()
        .next()
        .expect("a saved session")
        .sref
}

fn status(env: &TestEnv, id: &str) -> Value {
    // A dead loopback port proves this subcommand never needs engine discovery.
    let output = command(env, "http://127.0.0.1:1")
        .args(["status", id])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn saved_runs_have_bounded_status_and_terminal_outcomes() {
    for mode in ["print", "json", "rpc"] {
        for remote in [false, true] {
            let env = TestEnv::new();
            env.write(
                "home/.oxa/agent/settings.json",
                r#"{"oxa":{"envNote":false,"bootPreload":false},"cacheWarming":"off"}"#,
            );
            let srv = FakeServer::start(
                vec![
                    ScriptTurn::tool(
                        "save",
                        "write",
                        r#"{"path":"result.txt","content":"TOOL_CONTENT_SENTINEL"}"#,
                    ),
                    ScriptTurn::text("ANSWER_SENTINEL"),
                ],
                FakeOpts {
                    remote: remote.then_some("openrouter"),
                    ..FakeOpts::default()
                },
            )
            .unwrap();
            let mut cmd = command(&env, &srv.url());
            if mode == "print" {
                cmd.arg("-p");
            } else {
                cmd.args(["--mode", mode]);
            }
            cmd.args([
                "--no-mcp",
                "--no-extensions",
                "--no-skills",
                "--no-context-files",
                "--tools",
                "write",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            if mode == "rpc" {
                cmd.stdin(Stdio::piped());
            } else {
                cmd.arg("PROMPT_SENTINEL").stdin(Stdio::null());
            }
            let mut child = cmd.spawn().unwrap();
            if let Some(mut stdin) = child.stdin.take() {
                writeln!(stdin, "{}", json!({"type":"prompt", "message":"PROMPT_SENTINEL"})).unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let sref = session(&env);
            let path = sref.path.with_extension("status.json");
            let bytes = std::fs::read(&path).expect("the atomic status sidecar");
            assert!(bytes.len() <= 4096);
            let text = String::from_utf8(bytes).unwrap();
            for secret in ["PROMPT_SENTINEL", "TOOL_CONTENT_SENTINEL", "ANSWER_SENTINEL"] {
                assert!(!text.contains(secret));
            }
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let value = status(&env, &sref.id);
            assert_eq!(value["type"], "oxa_status");
            assert_eq!(value["version"], 1);
            assert_eq!(value["session_id"], sref.id);
            assert_eq!(value["phase"], "finished");
            assert_eq!(value["last_tool"], "write");
            assert!(value["step"].as_u64().unwrap() >= 2);
            assert!(value["tokens"]["completion"].as_u64().unwrap() > 0);
            assert_eq!(value["outcome"]["outcome"], "done");
            assert_eq!(value["outcome"]["exit_code"], 0);
            assert_eq!(value["stalled"], false);
            assert!(value["stall_deadline_ms"].is_null());
            assert_eq!(value["cost_usd"], if remote { Value::Null } else { json!(0) });
            if mode == "json" {
                let header: Value =
                    serde_json::from_slice(output.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
                assert_eq!(header["statusFile"], path.to_string_lossy().as_ref());
            }
            assert_eq!(srv.requests().len(), 2, "status lookup makes no model request");
        }
    }
}

#[test]
fn stalled_model_is_detected_without_changing_progress() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"envNote":false,"bootPreload":false},"cacheWarming":"off"}"#,
    );
    let mut turn = ScriptTurn::text(&"x".repeat(1024));
    turn.chunk = 16;
    turn.fault = Some(Fault::Stall { bytes: 512, ms: 1000 });
    let srv = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    let mut child = command(&env, &srv.url())
        .args(["--mode", "rpc", "--no-tools", "--stall-timeout", "0.1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"type":"prompt","message":"say hello"})).unwrap();
    stdin.flush().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(lines.read_line(&mut line).unwrap() > 0);
        if serde_json::from_str::<Value>(&line).unwrap()["type"] == "message_update" {
            break;
        }
    }
    let sref = session(&env);
    let path = sref.path.with_extension("status.json");
    let deadline = Instant::now() + Duration::from_millis(700);
    let stalled = loop {
        let value = status(&env, &sref.id);
        if value["stalled"] == true {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "stream never signalled its configured stall: {value}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let before = std::fs::read(&path).unwrap();
    assert!(stalled["outcome"].is_null());
    assert_eq!(stalled["phase"], "model");
    assert!(stalled["stall_deadline_ms"].as_u64().unwrap() > stalled["last_progress_ms"].as_u64().unwrap());
    let again = status(&env, &sref.id);
    assert_eq!(again["last_progress_ms"], stalled["last_progress_ms"]);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "reading status must not manufacture progress"
    );
    drop(stdin);
    // Drain the protocol while waiting, so pipe back-pressure cannot become a fake stall.
    for line in lines.lines() {
        serde_json::from_str::<Value>(&line.unwrap()).unwrap();
    }
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(0));
    let done = status(&env, &sref.id);
    assert_eq!(done["outcome"]["outcome"], "done");
    assert_eq!(done["stalled"], false);
}

#[test]
fn status_keeps_an_earlier_prompt_failure() {
    for mode in ["print", "json"] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"retry":{"enabled":false},"cacheWarming":"off","oxa":{"envNote":false}}"#,
        );
        let mut cut = ScriptTurn::text("partial");
        cut.finish = Finish::Length;
        let srv = FakeServer::start(vec![cut, ScriptTurn::text("fine")], FakeOpts::default()).unwrap();
        let mut cmd = command(&env, &srv.url());
        if mode == "print" {
            cmd.arg("-p");
        } else {
            cmd.args(["--mode", mode]);
        }
        let output = cmd
            .args(["--no-tools", "one", "two"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let value = status(&env, &session(&env).id);
        assert_eq!(value["outcome"]["outcome"], "failed");
        assert_eq!(value["outcome"]["error_class"], "output_limit");
    }
}

#[test]
fn rpc_compact_with_an_empty_queue_leaves_status_idle() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"cacheWarming":"off","compaction":{"keepRecentTokens":1},"oxa":{"envNote":false,"bootPreload":false}}"#,
    );
    let srv = FakeServer::start(
        vec![
            ScriptTurn::text("parser ready"),
            ScriptTurn::text("lexer ready"),
            ScriptTurn::text("session summary"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let mut child = command(&env, &srv.url())
        .args([
            "--mode",
            "rpc",
            "--no-tools",
            "--no-mcp",
            "--no-extensions",
            "--stall-timeout",
            "0.01",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    for id in ["first", "second"] {
        writeln!(
            stdin,
            "{}",
            json!({"id":id, "type":"prompt", "message":"explain"})
        )
        .unwrap();
        loop {
            let value: Value = serde_json::from_str(&lines.next().expect("RPC output").unwrap()).unwrap();
            if value["type"] == "agent_settled" {
                break;
            }
        }
    }
    writeln!(stdin, "{}", json!({"id":"compact", "type":"compact"})).unwrap();
    loop {
        let value: Value = serde_json::from_str(&lines.next().expect("RPC output").unwrap()).unwrap();
        assert_ne!(
            value["type"], "agent_settled",
            "compaction without input must not invent an agent turn"
        );
        if value["id"] == "compact" {
            assert_eq!(value["success"], true, "{value}");
            break;
        }
    }
    let value = status(&env, &session(&env).id);
    drop(stdin);
    for line in lines {
        serde_json::from_str::<Value>(&line.unwrap()).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(value["phase"], "idle");
    assert!(value["stall_deadline_ms"].is_null());
    assert_eq!(value["stalled"], false);
    assert_eq!(
        srv.requests().len(),
        3,
        "only two prompts and compaction use the engine"
    );
}

#[test]
fn ephemeral_runs_create_no_status() {
    let env = TestEnv::new();
    let srv = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let output = command(&env, &srv.url())
        .args(["-p", "--no-session", "--no-tools", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(FsStore::new(env.agent_dir().join("sessions"))
        .list(None)
        .is_empty());
    assert!(!env.agent_dir().join("sessions").exists());
}

#[test]
fn stall_timeout_rejects_invalid_or_ephemeral_configuration() {
    let env = TestEnv::new();
    for value in ["NaN", "inf", "0", "-1", "0.00001", "1e30"] {
        let output = command(&env, "http://127.0.0.1:1")
            .args(["-p", "--stall-timeout", value, "go"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{value}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--stall-timeout"));
    }
    let output = command(&env, "http://127.0.0.1:1")
        .args(["-p", "--no-session", "--stall-timeout", "1", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn status_lookup_respects_the_configured_session_directory() {
    let env = TestEnv::new();
    let custom = env.work().join("saved");
    let srv = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let output = command(&env, &srv.url())
        .args(["-p", "--no-tools", "--session-dir"])
        .arg(&custom)
        .arg("go")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let sref = FsStore::new(custom.clone()).list(None).remove(0).sref;
    let output = command(&env, "http://127.0.0.1:1")
        .args(["status", &sref.id, "--session-dir"])
        .arg(custom)
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["outcome"]["outcome"], "done");
}

#[test]
fn status_lookup_expands_tilde_like_session_creation() {
    for source in ["flag", "env", "settings"] {
        let env = TestEnv::new();
        let dir = "~/saved-status";
        if source == "settings" {
            env.write(
                "home/.oxa/agent/settings.json",
                &json!({"sessionDir":dir}).to_string(),
            );
        }
        let srv = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
        let mut run = command(&env, &srv.url());
        run.args(["-p", "--no-tools"]);
        if source == "flag" {
            run.args(["--session-dir", dir]);
        }
        if source == "env" {
            run.env("OXA_SESSION_DIR", dir);
        }
        let output = run.arg("go").stdin(Stdio::null()).output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        let sref = FsStore::new(env.home().join("saved-status"))
            .list(None)
            .remove(0)
            .sref;
        let mut read = command(&env, "http://127.0.0.1:1");
        read.args(["status", &sref.id]);
        if source == "flag" {
            read.args(["--session-dir", dir]);
        }
        if source == "env" {
            read.env("OXA_SESSION_DIR", dir);
        }
        let output = read.output().unwrap();
        assert!(
            output.status.success(),
            "{source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn wipe_removes_the_session_status_snapshot() {
    let env = TestEnv::new();
    let srv = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let output = command(&env, &srv.url())
        .args(["-p", "--no-tools", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let sref = session(&env);
    let path = sref.path.with_extension("status.json");
    assert!(path.exists());
    let output = command(&env, "http://127.0.0.1:1")
        .args(["wipe", "--session", &sref.id, "--yes"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !path.exists(),
        "wipe must remove the status artifact with its session"
    );
}

#[test]
fn status_refuses_malformed_oversized_mismatched_and_symlinked_snapshots() {
    use std::os::unix::fs::symlink;
    let env = TestEnv::new();
    let store = FsStore::new(env.agent_dir().join("sessions"));
    let sref = store.create(&env.work(), None, None).unwrap();
    let path = sref.path.with_extension("status.json");
    for bytes in [
        b"not JSON".to_vec(),
        vec![b'x'; 4097],
        serde_json::to_vec(&json!({"type":"oxa_status","version":2,"session_id":sref.id})).unwrap(),
        serde_json::to_vec(&json!({"type":"oxa_status","version":1,"session_id":"another"})).unwrap(),
    ] {
        std::fs::write(&path, bytes).unwrap();
        let output = command(&env, "http://127.0.0.1:1")
            .args(["status", &sref.id])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    std::fs::remove_file(&path).unwrap();
    let other = env.work().join("unrelated");
    std::fs::write(&other, "PRIVATE_TEST_CONTENT").unwrap();
    symlink(&other, &path).unwrap();
    let output = command(&env, "http://127.0.0.1:1")
        .args(["status", &sref.id])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_TEST_CONTENT"));
    assert_eq!(std::fs::read_to_string(other).unwrap(), "PRIVATE_TEST_CONTENT");
}

// ---------------------------------------------------------------------------------------------
// Bare `oxa status`: the current/selected saved session in the one shared status view.

/// A saved session in the current directory, with one user/assistant/tool turn.
fn saved_session(store: &FsStore, cwd: &std::path::Path, text: &str, ts_ms: u64) -> SessionRef {
    let sref = store.create(cwd, None, None).unwrap();
    let mut assistant = WireMsg::assistant("done");
    assistant.tool_calls = vec![ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        args_raw: "{}".into(),
    }];
    for (i, wire) in [WireMsg::user(text), assistant].into_iter().enumerate() {
        let entry = Entry {
            id: format!("entry-{i}"),
            parent: None,
            ts_ms: ts_ms + i as u64,
            body: EntryBody::Msg {
                wire,
                parts: vec![Part::Text(text.to_string())],
                tool: None,
            },
        };
        store.append(&sref, &entry).unwrap();
    }
    sref
}

/// A worker snapshot with every version-1 field, as the status writer leaves it.
fn write_snapshot(sref: &SessionRef, phase: &str, outcome: Option<Value>) {
    let path = sref.path.with_extension("status.json");
    let value = json!({
        "type": "oxa_status", "version": 1, "session_id": sref.id.clone(),
        "pid": 4242, "started_ms": 1, "phase": phase, "step": 2,
        "last_tool": "bash",
        "tokens": {"prompt": 100, "completion": 10, "cached": 50, "complete": true,
                   "unreported_requests": 0},
        "cost_usd": 0, "last_progress_ms": 1, "stall_timeout_ms": null,
        "stall_deadline_ms": null, "outcome": outcome
    });
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

/// Bare `oxa status` picks this directory's most recently modified session (the `--continue`
/// choice), reports it as static rows, and never contacts an engine: the endpoint is a dead
/// loopback port and no provider or sign-in data is in the environment.
#[test]
fn bare_status_reports_the_current_session_without_an_engine() {
    let env = TestEnv::new();
    let store = FsStore::new(env.agent_dir().join("sessions"));
    let cwd = env.work().canonicalize().unwrap();
    let older = saved_session(&store, &cwd, "older", 1_700_000_000_000);
    let newer = saved_session(&store, &cwd, "newer", 1_900_000_000_000);
    write_snapshot(&older, "idle", None);
    write_snapshot(&newer, "tool", None);

    let output = command(&env, "http://127.0.0.1:1")
        .args(["status"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.starts_with(&format!("Status of session {}\n", newer.id)),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("  Session id: {}\n", newer.id)),
        "{stdout}"
    );
    assert!(
        stdout.contains("  Messages: user 1, assistant 1, tool calls 1\n"),
        "{stdout}"
    );
    assert!(stdout.contains("  Tokens: input 100, output 10\n"), "{stdout}");
    assert!(
        stdout.contains("  Cache, last request: 50 / 100 tokens (50%)\n"),
        "{stdout}"
    );
    assert!(stdout.contains("  Turn: tool (bash)\n"), "{stdout}");
    assert!(stdout.contains("  Cost: $0.0000\n"), "{stdout}");
    assert!(
        !stdout.contains(&older.id),
        "the older session is not the current one: {stdout}"
    );
    // Facts this surface cannot read are unavailable with their reason, never a zero.
    for row in [
        "Background jobs",
        "Child sessions",
        "Goal",
        "Lead",
        "Engine queue",
        "Model",
    ] {
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with(&format!("  {row}: unavailable ("))),
            "{row} must say why it is unavailable:\n{stdout}"
        );
    }
    let file_row = format!("  Session file: {}", newer.path.display());
    assert!(stdout.lines().any(|l| l == file_row), "{stdout}");
}

/// The same rows carry a failed, an idle and a stalled turn from the saved snapshot; an
/// explicit session keeps printing the JSON snapshot instead.
#[test]
fn bare_status_reports_failed_idle_and_stalled_turns() {
    let env = TestEnv::new();
    let store = FsStore::new(env.agent_dir().join("sessions"));
    let cwd = env.work().canonicalize().unwrap();
    let sref = saved_session(&store, &cwd, "one", 1_700_000_000_000);
    let run = || {
        let output = command(&env, "http://127.0.0.1:1")
            .args(["status"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let outcome = json!({"type": "oxa_result", "outcome": "failed",
                         "error_class": "output_limit", "exit_code": 1});
    write_snapshot(&sref, "finished", Some(outcome));
    let failed = run();
    assert!(
        failed.contains("  Turn: finished (bash); last job: failed (output_limit)\n"),
        "{failed}"
    );

    write_snapshot(&sref, "idle", None);
    let idle = run();
    assert!(idle.contains("  Turn: idle (bash)\n"), "{idle}");
    assert!(idle.contains("  Cost: $0.0000\n"), "{idle}");

    // An expired deadline is a stall signal, not a cancellation; reading it writes nothing.
    let path = sref.path.with_extension("status.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["phase"] = json!("model");
    value["stall_timeout_ms"] = json!(1);
    value["stall_deadline_ms"] = json!(1);
    // A cloud session: the wire reports no settled charge, so the cost row says that.
    value["cost_usd"] = Value::Null;
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let stalled = run();
    assert!(stalled.contains("  Turn: model (bash); stalled\n"), "{stalled}");
    assert!(
        stalled.contains("  Cost: cloud (no settled charge is reported)\n"),
        "{stalled}"
    );

    // The explicit form is unchanged: one version-1 JSON object, no view rows.
    let output = command(&env, "http://127.0.0.1:1")
        .args(["status", &sref.id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["type"], "oxa_status");
    assert_eq!(value["session_id"], sref.id);
    assert_eq!(value["stalled"], true);
}

/// A session without a worker snapshot still reports its saved rows; a directory without a
/// saved session says so instead of failing.
#[test]
fn bare_status_without_a_snapshot_or_a_session_states_what_is_known() {
    let env = TestEnv::new();
    let store = FsStore::new(env.agent_dir().join("sessions"));
    let cwd = env.work().canonicalize().unwrap();
    let sref = saved_session(&store, &cwd, "saved", 1_700_000_000_000);
    assert!(!sref.path.with_extension("status.json").exists());

    let output = command(&env, "http://127.0.0.1:1")
        .args(["status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!("Status of session {}", sref.id)),
        "{stdout}"
    );
    assert!(
        stdout.contains("  Messages: user 1, assistant 1, tool calls 1\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  Turn: unavailable (this view receives no turn state)\n"),
        "{stdout}"
    );

    // An empty session directory has no current session: the command says so and still lists
    // the rows with their reasons instead of failing.
    let empty = env.root().join("empty-sessions").display().to_string();
    let output = command(&env, "http://127.0.0.1:1")
        .args(["status", "--session-dir", &empty])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("No saved session for this directory.\n"),
        "{stdout}"
    );
    let jobs = "  Background jobs: unavailable (no live job registry on this surface)";
    assert!(stdout.contains(jobs), "{stdout}");
}

/// `--offline` is a global flag: status reads local files only and does not start a provider,
/// a sign-in or the engine, even when the configured endpoint is unreachable.
#[test]
fn offline_status_reads_only_local_files() {
    let env = TestEnv::new();
    let store = FsStore::new(env.agent_dir().join("sessions"));
    let cwd = env.work().canonicalize().unwrap();
    let sref = saved_session(&store, &cwd, "offline", 1_700_000_000_000);
    write_snapshot(&sref, "idle", None);
    let output = command(&env, "http://127.0.0.1:1")
        .args(["--offline", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(&sref.id));
}
