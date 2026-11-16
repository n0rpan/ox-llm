//! Wave H lane `harden` (pre-flight A1/A2/B9, CT-13): the REAL `oxa` binary (debug under
//! `cb test`, the release profile under `cb test --release`) survives an extension whose host
//! function panics, in print mode and in RPC mode: the turn reports the extension error and a
//! following prompt in the same session works. The panicking host function exists only under
//! `OXA_TEST_ISOLATION=1 OXA_TEST_PANIC=host_function` (`oxa_ext::api_ctx::install`).
//!
//! Also a guard that the release profile never says `panic = "abort"` again.

#[path = "common/http.rs"]
mod http;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(60);

/// A user-level extension whose `turn_start` handler calls the panicking host function.
fn setup(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/extensions/boom.js",
        "export default function (oxa) { oxa.on('turn_start', (e, ctx) => { globalThis.__oxaTestPanic(); }); }",
    );
}

fn command(env: &TestEnv, url: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.args(args)
        .env_clear()
        .envs(http::envs(env, url))
        .env("OXA_TEST_ISOLATION", "1")
        .env("OXA_TEST_PANIC", "host_function")
        .current_dir(env.work());
    cmd
}

#[test]
fn print_mode_survives_a_panicking_host_function() {
    let env = TestEnv::new();
    setup(&env);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("first answer"),
            ScriptTurn::text("second answer"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    // Two prompts in one print-mode session: the second runs after the panic of the first.
    let mut cmd = command(&env, &server.url(), &["-p", "one", "two"]);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output().expect("run oxa"));
    });
    let out = rx.recv_timeout(LIMIT).expect("oxa finished");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the process survived: {:?}\n{stderr}",
        out.status
    );
    assert!(
        stderr.contains("injected host function panic"),
        "the extension error is reported: {stderr}"
    );
    assert!(
        stdout.contains("second answer"),
        "the following prompt ran: {stdout}\n{stderr}"
    );
}

#[test]
fn rpc_mode_survives_a_panicking_host_function() {
    let env = TestEnv::new();
    setup(&env);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("first answer"),
            ScriptTurn::text("second answer"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let mut child = command(&env, &server.url(), &["--mode", "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn oxa");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen: Vec<String> = Vec::new();
    let wait = |pred: &dyn Fn(&str) -> bool, what: &str, seen: &mut Vec<String>| {
        let end = Instant::now() + LIMIT;
        loop {
            if seen.iter().any(|l| pred(l)) {
                return;
            }
            let left = end.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(l) => seen.push(l),
                Err(_) => panic!("no {what}; records: {seen:#?}"),
            }
        }
    };
    writeln!(stdin, r#"{{"id":"p1","type":"prompt","message":"one"}}"#).unwrap();
    wait(
        &|l| l.contains("\"agent_settled\""),
        "first agent_settled",
        &mut seen,
    );
    assert!(
        // RPC reports a failed handler like pi: an `extension_error` record or an error
        // notification to the client, naming the panic.
        seen.iter().any(|l| l.contains("injected host function panic")),
        "{seen:#?}"
    );
    seen.clear();
    writeln!(stdin, r#"{{"id":"p2","type":"prompt","message":"two"}}"#).unwrap();
    wait(&|l| l.contains("second answer"), "the second answer", &mut seen);
    drop(stdin);
    let end = Instant::now() + LIMIT;
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break s;
        }
        assert!(Instant::now() < end, "oxa did not exit after stdin closed");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "the process survived: {status:?}");
}

/// The release profile unwinds: `panic = "abort"` would make every `catch_unwind` a no-op and
/// one panicking extension would end the session (A2).
#[test]
fn the_release_profile_does_not_abort_on_panic() {
    let manifest = include_str!("../../Cargo.toml");
    let release = manifest
        .split("[profile.release]")
        .nth(1)
        .expect("a [profile.release] section");
    let section = release.split("\n[").next().unwrap_or("");
    let panic_line = section
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("panic"))
        .expect("the release profile names its panic strategy");
    assert_eq!(panic_line.replace(' ', ""), "panic=\"unwind\"", "{section}");
}
