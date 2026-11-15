//! Measure a real oxa binary against the existing owned HTTP fixture.
//! Run in the test sandbox; emits numbers only, never requests or transcripts.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Running(Child);
impl std::ops::Deref for Running {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for Running {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stats(pid: u32) -> (u64, u64) {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let number = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    (number("VmRSS:"), number("Threads:"))
}
fn text_delta(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("type").is_some_and(|kind| kind == "text_delta") || map.values().any(text_delta)
        }
        Value::Array(items) => items.iter().any(text_delta),
        _ => false,
    }
}
fn direct(url: &str, body: &[u8]) -> f64 {
    let address = url.strip_prefix("http://").unwrap().trim_end_matches('/');
    let started = Instant::now();
    let mut socket = TcpStream::connect(address).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(socket, "POST /v1/chat/completions HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
    socket.write_all(body).unwrap();
    for line in BufReader::new(socket).lines() {
        let line = line.unwrap();
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                if value["choices"][0]["delta"]["content"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                {
                    return started.elapsed().as_secs_f64() * 1000.0;
                }
            }
        }
    }
    panic!("owned fixture returned no text");
}
fn measure(binary: &Path) -> Value {
    let server = FakeServer::start(
        vec![ScriptTurn::text("Synthetic footprint reply."); 2],
        FakeOpts {
            n_ctx: 64000,
            done_gap_ms: 250,
            remote: Some("anthropic"),
            no_engine: true,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","retry":{"enabled":false}}"#,
    );
    let started = Instant::now();
    let mut child = Running(
        Command::new(binary)
            .args([
                "--mode",
                "rpc",
                "--model",
                "fake-model",
                "--no-tools",
                "--no-session",
            ])
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(&server.url()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if let Ok(value) = serde_json::from_str::<Value>(&line.unwrap()) {
                if send.send((Instant::now(), value)).is_err() {
                    break;
                }
            }
        }
    });
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", json!({"type":"get_state","id":"idle"})).unwrap();
    input.flush().unwrap();
    loop {
        let (_, value) = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("RPC startup timeout");
        if value["type"] == "response" && value["command"] == "get_state" {
            assert_eq!(value["success"], true);
            break;
        }
    }
    let idle = stats(child.id());
    let request_start = Instant::now();
    writeln!(
        input,
        "{}",
        json!({"type":"prompt","message":"Synthetic footprint request."})
    )
    .unwrap();
    input.flush().unwrap();
    let first = loop {
        let (when, value) = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("RPC text timeout");
        if text_delta(&value) {
            break when;
        }
    };
    let mut peak = stats(child.id());
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(20));
        let current = stats(child.id());
        peak.0 = peak.0.max(current.0);
        peak.1 = peak.1.max(current.1);
    }
    loop {
        let (_, value) = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("RPC completion timeout");
        if value["type"] == "agent_end" {
            break;
        }
    }
    drop(input);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let direct_ms = direct(&server.url(), &requests[0].body);
    let startup_ms = first.duration_since(started).as_secs_f64() * 1000.0;
    json!({"idle_rss_kib":idle.0,"stream_rss_kib":peak.0,"idle_threads":idle.1,
        "stream_threads":peak.1,"start_to_first_text_ms":startup_ms,
        "request_to_first_text_ms":first.duration_since(request_start).as_secs_f64()*1000.0,
        "direct_first_text_ms":direct_ms,"startup_overhead_ms":startup_ms-direct_ms})
}
fn main() {
    let binary = std::env::args_os()
        .nth(1)
        .expect("usage: gateway_footprint /absolute/path/to/oxa");
    let binary = Path::new(&binary).canonicalize().unwrap();
    let samples: Vec<_> = (0..5).map(|_| measure(&binary)).collect();
    println!(
        "{}",
        json!({"binary_bytes":fs::metadata(binary).unwrap().len(),"samples":samples})
    );
}
