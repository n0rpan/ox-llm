//! #699: real process/PTY interrupts against loopback-only fake generation and tools.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Process {
    child: Child,
    text: Arc<Mutex<String>>,
}
impl Process {
    fn start(env: &TestEnv, server: &FakeServer, mode: &str) -> Self {
        let mut cmd = if mode == "tui" {
            // util-linux script provides a real controlling PTY, not a TestTerminal.
            let mut cmd = Command::new("script");
            cmd.args([
                "-qefc",
                &format!("'{}' --yolo", env!("CARGO_BIN_EXE_oxa")),
                "/dev/null",
            ]);
            cmd
        } else {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
            cmd.env_clear().arg("--yolo");
            if mode == "print" {
                cmd.args(["-p", "synthetic cancellation check"]);
            } else {
                cmd.args(["--mode", mode]);
                if mode == "json" {
                    cmd.arg("synthetic cancellation check");
                }
            }
            cmd
        };
        let mut child = cmd
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(&server.url()))
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let text = Arc::new(Mutex::new(String::new()));
        let capture = text.clone();
        let mut stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                capture
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Self { child, text }
    }
    fn send(&mut self, bytes: &[u8]) {
        self.child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
    }
    #[track_caller]
    fn wait_for(&mut self, predicate: impl Fn() -> bool) {
        let predicate_line = std::panic::Location::caller().line();
        let start = Instant::now();
        while !predicate() {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "early exit: {}",
                self.screen()
            );
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timeout predicate_line={predicate_line}: {}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn screen(&self) -> String {
        oxa_tui::strip_ansi(&self.text.lock().unwrap())
    }
    #[track_caller]
    fn wait_text(&mut self, expected: &str) {
        let text = self.text.clone();
        self.wait_for(|| oxa_tui::strip_ansi(&text.lock().unwrap()).contains(expected));
    }
    fn finish(&mut self) -> Output {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "did not exit: {}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stderr = Vec::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        Output {
            status,
            stdout: Vec::new(),
            stderr,
        }
    }
    fn signal(&self) {
        assert!(Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap()
            .success());
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture(turns: Vec<ScriptTurn>) -> (TestEnv, FakeServer) {
    fixture_with_opts(turns, FakeOpts::default())
}
fn fixture_with_opts(turns: Vec<ScriptTurn>, opts: FakeOpts) -> (TestEnv, FakeServer) {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","retry":{"enabled":false},"quietStartup":false}"#,
    );
    let server = FakeServer::start(turns, opts).unwrap();
    (env, server)
}
fn delayed() -> ScriptTurn {
    ScriptTurn {
        fault: Some(Fault::HoldHeaders { ms: 20_000 }),
        ..ScriptTurn::text("must not finish")
    }
}
fn saved(env: &TestEnv) {
    let sessions = FsStore::new(env.agent_dir().join("sessions")).list(None);
    assert_eq!(sessions.len(), 1, "session retained on normal shutdown");
    assert!(std::fs::metadata(&sessions[0].sref.path).unwrap().len() > 0);
}

#[test]
fn idle_pty_ctrl_c_twice_exits_cleanly_and_saves_session() {
    let (env, server) = fixture(vec![]);
    let mut p = Process::start(&env, &server, "tui");
    p.wait_text("READY");
    p.send(b"\x03");
    p.wait_text("ctrl+c exit");
    p.send(b"\x03");
    assert_eq!(p.finish().status.code(), Some(0));
    saved(&env);
}

#[test]
fn generation_pty_ctrl_c_cancels_and_keeps_prompt_usable() {
    let (env, server) = fixture_with_opts(
        vec![delayed(), ScriptTurn::text("synthetic next answer")],
        FakeOpts {
            hold_prefill_ms: 20_000,
            ..FakeOpts::default()
        },
    );
    let mut p = Process::start(&env, &server, "tui");
    p.wait_text("READY");
    p.wait_for(|| server.requests().iter().any(|r| r.max_tokens == 0));
    p.send(b"synthetic cancellation check\r");
    p.wait_for(|| !server.requests().is_empty());
    eprintln!(
        "foreground_before_ctrl_c={}",
        server.requests().iter().filter(|r| r.max_tokens > 0).count()
    );
    p.send(b"\x03");
    p.wait_text("partial reply dropped; your prompt stays");
    // A fresh prompt proves the first Ctrl+C did not arm exit or poison the next turn.
    p.send(b"next synthetic prompt\r");
    p.wait_text("synthetic next answer");
    p.send(b"\x04");
    assert_eq!(p.finish().status.code(), Some(0));
    saved(&env);
}

#[test]
fn headless_sigint_returns_typed_interrupted_with_stdin_still_open() {
    for mode in ["print", "json", "rpc"] {
        let (env, server) = fixture(vec![delayed()]);
        let mut p = Process::start(&env, &server, mode);
        if mode == "rpc" {
            p.send(b"{\"type\":\"prompt\",\"message\":\"synthetic cancellation check\"}\n");
        } else {
            p.child.stdin.take();
        }
        p.wait_for(|| !server.requests().is_empty());
        p.signal();
        let out = p.finish();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(130), "{mode}: {err}");
        let result: serde_json::Value = err
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|v| v["type"] == "oxa_result")
            .expect("typed terminal result");
        assert_eq!(result["outcome"], "interrupted");
        assert_eq!(server.requests().len(), 1, "no final model turn on SIGINT");
        saved(&env);
    }
}

#[test]
fn tool_pty_ctrl_c_stops_tool_without_exiting() {
    let (env, server) = fixture(vec![
        ScriptTurn::tool(
            "synthetic-tool",
            "bash",
            r#"{"command":"printf running > tool-started; printf synthetic-tool-running; sleep 20; printf unwanted > tool-finished"}"#,
        ),
        ScriptTurn::text("synthetic tool recovery"),
    ]);
    let mut p = Process::start(&env, &server, "tui");
    p.wait_text("READY");
    p.send(b"synthetic tool cancellation\r");
    p.wait_for(|| env.work().join("tool-started").exists());
    p.send(b"\x03");
    p.wait_text("Command aborted");
    assert!(!env.work().join("tool-finished").exists());
    p.send(b"next synthetic prompt\r");
    p.wait_text("synthetic tool recovery");
    p.send(b"\x04");
    assert_eq!(p.finish().status.code(), Some(0));
    saved(&env);
}

#[test]
fn idle_pty_ctrl_c_clears_text_and_expired_exit_needs_two_presses() {
    let (env, server) = fixture(vec![]);
    let mut p = Process::start(&env, &server, "tui");
    p.wait_text("READY");
    p.send(b"synthetic draft\x03");
    std::thread::sleep(Duration::from_millis(100));
    p.send(b"\x03"); // Arms, rather than exits: the preceding press cleared the draft.
    p.wait_text("ctrl+c exit");
    std::thread::sleep(Duration::from_millis(2200));
    p.send(b"\x03"); // The old arm expired; this press must not exit.
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        p.child.try_wait().unwrap().is_none(),
        "expired arm exited the process"
    );
    p.send(b"\x03");
    assert_eq!(p.finish().status.code(), Some(0));
    saved(&env);
}

#[test]
fn idle_rpc_sigint_returns_typed_interrupted() {
    let (env, server) = fixture(vec![]);
    let mut p = Process::start(&env, &server, "rpc");
    p.send(b"{\"type\":\"get_state\",\"id\":\"ready\"}\n");
    p.wait_text("\"id\":\"ready\"");
    p.signal();
    let out = p.finish();
    assert_eq!(out.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"outcome\":\"interrupted\""));
    saved(&env);
}

#[test]
fn idle_pty_kitty_ctrl_c_release_does_not_disarm_exit() {
    let (env, server) = fixture(vec![]);
    let mut p = Process::start(&env, &server, "tui");
    p.wait_text("READY");
    p.send(b"\x1b[99;5:1u");
    p.wait_text("ctrl+c exit");
    p.send(b"\x1b[99;5:3u\x1b[99;5:1u");
    assert_eq!(p.finish().status.code(), Some(0));
    saved(&env);
}

#[test]
fn headless_tool_sigint_cancels_without_final_turn() {
    for mode in ["print", "rpc"] {
        let (env, server) = fixture(vec![ScriptTurn::tool(
            "synthetic-tool",
            "bash",
            r#"{"command":"printf running > tool-started; sleep 20; printf unwanted > tool-finished"}"#,
        )]);
        let mut p = Process::start(&env, &server, mode);
        if mode == "rpc" {
            p.send(b"{\"type\":\"prompt\",\"message\":\"synthetic tool cancellation\"}\n");
        } else {
            p.child.stdin.take();
        }
        p.wait_for(|| env.work().join("tool-started").exists());
        p.signal();
        let out = p.finish();
        assert_eq!(out.status.code(), Some(130));
        assert!(String::from_utf8_lossy(&out.stderr).contains("\"outcome\":\"interrupted\""));
        assert!(!env.work().join("tool-finished").exists());
        assert_eq!(server.requests().len(), 1, "no wrap-up turn on SIGINT");
        saved(&env);
    }
}
