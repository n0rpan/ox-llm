//! #332 step 2: bounded, visible retries before output, using only the local fake gateway.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use serde_json::json;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(mode: &str, turns: Vec<ScriptTurn>, provider_retries: u32) -> (Output, usize) {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({
            "retry":{"enabled":true,"maxRetries":2,"baseDelayMs":1,"maxAgentDelayMs":5,
                "provider":{"maxRetries":provider_retries}}
        })
        .to_string(),
    );
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command.current_dir(env.work()).env_clear();
    for (key, value) in env.envs(&server.url()) {
        command.env(key, value);
    }
    command.args(["--no-session", "--no-tools"]);
    if mode == "print" {
        command.args(["-p", "hello"]);
    } else if mode == "json" {
        command.args(["--mode", "json", "hello"]);
    } else {
        command.args(["--mode", "rpc"]);
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
            .take()
            .unwrap()
            .write_all(b"{\"type\":\"prompt\",\"message\":\"hello\"}\n")
            .unwrap();
    } else {
        child.stdin.take();
    }
    let output = child.wait_with_output().unwrap();
    let requests = server.requests();
    if requests.len() > 1 {
        assert!(!requests[0].idem_key.is_empty());
        assert!(
            requests
                .iter()
                .all(|request| request.idem_key == requests[0].idem_key),
            "all retries reuse the same idempotency key"
        );
    }
    (output, requests.len())
}

fn refusal(status: u16, code: &str) -> ScriptTurn {
    ScriptTurn::fault(Fault::Http {
        status,
        body: json!({"error":{"type":"cloud_error","code":code,"message":"Synthetic gateway refusal"}})
            .to_string(),
    })
}

fn successful_retry(mode: &str, first: ScriptTurn, provider_retries: u32) {
    let (output, requests) = run(mode, vec![first, ScriptTurn::text("finished")], provider_retries);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "stdout:{stdout}\nstderr:{stderr}");
    assert_eq!(requests, 2, "one retry");
    assert!(stdout.contains("finished"), "{stdout}");
    let visible = if mode == "print" {
        stderr.as_ref()
    } else {
        stdout.as_ref()
    };
    assert!(
        visible.contains("retrying request"),
        "one-line retry notice: {visible}"
    );
}

#[test]
fn transient_credentials_and_ledger_refusals_are_retried_in_every_mode() {
    for mode in ["print", "json", "rpc"] {
        for class in ["cloud_credentials_unavailable", "cloud_ledger_unavailable"] {
            successful_retry(mode, refusal(503, class), 0);
        }
    }
}

#[test]
fn upstream_rate_limits_and_server_failures_have_visible_retries() {
    for mode in ["print", "json", "rpc"] {
        for status in [429, 500, 502, 503] {
            successful_retry(mode, refusal(status, "cloud_upstream_error"), 0);
        }
    }
}

#[test]
fn provider_level_retries_are_visible_too() {
    successful_retry("print", refusal(502, "cloud_upstream_error"), 1);
}

#[test]
fn connection_reset_before_output_has_a_visible_retry() {
    successful_retry("print", ScriptTurn::fault(Fault::DropAt { bytes: 0 }), 0);
}

#[test]
fn permanent_refusals_are_not_retried() {
    for (status, class) in [
        (429, "budget_exceeded"),
        (503, "cloud_sdk_unavailable"),
        (401, "authentication_error"),
        (403, "permission_error"),
        (400, "invalid_request"),
    ] {
        let (output, requests) = run(
            "print",
            vec![refusal(status, class), ScriptTurn::text("must not run")],
            0,
        );
        assert_ne!(output.status.code(), Some(0), "{class}");
        assert_eq!(requests, 1, "{class}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("retrying request"));
    }
}

#[test]
fn retries_stop_at_the_configured_limit() {
    let (output, requests) = run("print", vec![refusal(503, "cloud_ledger_unavailable"); 4], 0);
    assert_ne!(output.status.code(), Some(0));
    assert_eq!(requests, 3, "one request plus two retries");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .matches("retrying request")
            .count(),
        2
    );
}

#[test]
fn output_already_streamed_is_not_replayed() {
    let mut turn = ScriptTurn::text(&"partial ".repeat(100));
    turn.chunk = 1;
    turn.fault = Some(Fault::DropAt { bytes: 300 });
    let (output, requests) = run("print", vec![turn, ScriptTurn::text("must not run")], 0);
    assert_ne!(output.status.code(), Some(0));
    assert_eq!(requests, 1);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("retrying request"));
}

#[test]
fn print_flushes_the_retry_notice_before_sending_the_next_request() {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    struct RetrySink {
        server: Arc<FakeServer>,
        bytes: Vec<u8>,
        observed: Arc<Mutex<Vec<usize>>>,
    }
    impl Write for RetrySink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if String::from_utf8_lossy(&self.bytes).contains("retrying request") {
                self.observed.lock().unwrap().push(self.server.requests().len());
            }
            self.bytes.clear();
            Ok(())
        }
    }
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"retry":{"baseDelayMs":1,"maxAgentDelayMs":5}}"#,
    );
    let server = Arc::new(
        FakeServer::start(
            vec![refusal(502, "cloud_upstream_error"), ScriptTurn::text("finished")],
            FakeOpts::default(),
        )
        .unwrap(),
    );
    let owned = env.envs(&server.url());
    let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let code = oxa_cli::run(
        &["--no-session", "--no-tools", "-p", "hello"].map(String::from),
        &oxa_config::Env::from_pairs(&pairs),
        oxa_cli::Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(std::io::sink()),
            stderr: Box::new(RetrySink {
                server,
                bytes: Vec::new(),
                observed: observed.clone(),
            }),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd: env.work(),
        },
    );
    assert_eq!(code, 0);
    assert_eq!(
        *observed.lock().unwrap(),
        vec![1],
        "the notice was flushed while only the first request had run"
    );
}
