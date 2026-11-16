//! Measure a real oxa binary against the shared owned HTTP or native fixture.
//! Run in the test sandbox; emits numbers only, never requests or transcripts.
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::OpenOptionsExt,
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
fn direct(url: &str, body: &[u8], native: bool) -> f64 {
    use oxa_engine::{post_sse, Flow, SseEvent, StreamOpts, Url};
    let url = Url::parse(url).unwrap();
    let headers = if native {
        vec![
            ("x-api-key", CREDENTIAL.to_owned()),
            ("anthropic-version", "2023-06-01".to_owned()),
        ]
    } else {
        vec![]
    };
    let started = Instant::now();
    let mut first = None;
    post_sse(
        &url,
        if native {
            "/v1/messages"
        } else {
            "/v1/chat/completions"
        },
        &headers,
        body,
        &StreamOpts::default(),
        &oxa_proto::CancelToken::new(),
        &mut |event| {
            if let SseEvent::Data(data) = event {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    let text = if native {
                        &value["delta"]["text"]
                    } else {
                        &value["choices"][0]["delta"]["content"]
                    };
                    if first.is_none() && text.as_str().is_some_and(|s| !s.is_empty()) {
                        first = Some(started.elapsed().as_secs_f64() * 1000.0);
                    }
                    if native && value["type"] == "message_stop" {
                        return Flow::Stop;
                    }
                }
            }
            Flow::Continue
        },
    )
    .unwrap();
    first.expect("owned fixture returned text")
}

enum Peer {
    Http(FakeServer),
    Native(MockAnthropic),
}
impl Peer {
    fn start(native: bool) -> Self {
        if native {
            Self::Native(MockAnthropic::start(
                (0..2)
                    .map(|_| {
                        Reply::text("Synthetic footprint reply.", 11, 7, 3, 5)
                            .before_stop_delay(Duration::from_millis(250))
                    })
                    .collect(),
            ))
        } else {
            Self::Http(
                FakeServer::start(
                    vec![ScriptTurn::text("Synthetic footprint reply."); 2],
                    FakeOpts {
                        n_ctx: 64000,
                        done_gap_ms: 250,
                        remote: Some("anthropic"),
                        no_engine: true,
                        ..FakeOpts::default()
                    },
                )
                .unwrap(),
            )
        }
    }
    fn url(&self) -> String {
        match self {
            Self::Http(server) => server.url(),
            Self::Native(server) => server.base_url(),
        }
    }
    fn body(&self) -> Vec<u8> {
        match self {
            Self::Http(server) => {
                let requests = server.requests();
                assert_eq!(requests.len(), 1);
                requests[0].body.clone()
            }
            Self::Native(server) => {
                let requests = server.requests();
                assert_eq!(requests.len(), 1);
                serde_json::to_vec(&requests[0].body).unwrap()
            }
        }
    }
}
fn measure(binary: &Path, native: bool) -> Value {
    let server = Peer::start(native);
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"defaultProjectTrust":"always","defaultThinkingLevel":"off",
        "retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":native},
        "cloud":{"daily_budget_usd":1,"models":{"fake-model":{
            "provider":"anthropic","upstream_model":"synthetic-anthropic",
            "base_url":server.url(),"context_window":64000,"max_output":16,
            "pricing":{"input_per_million":2,"output_per_million":3,
                "cache_read_per_million":0.2,"cache_write_per_million":2.5}
        }}}}})
        .to_string(),
    );
    if native {
        let path = env.home().join(".config/oxc/llm/cloud-secrets.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        write!(file, "{}", json!({"anthropic":{"api_key":CREDENTIAL}})).unwrap();
    }
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
    let after = stats(child.id());
    drop(input);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    let direct_ms = direct(&server.url(), &server.body(), native);
    let startup_ms = first.duration_since(started).as_secs_f64() * 1000.0;
    json!({"idle_rss_kib":idle.0,"stream_rss_kib":peak.0,"idle_threads":idle.1,
        "stream_threads":peak.1,"after_rss_kib":after.0,"after_threads":after.1,
        "start_to_first_text_ms":startup_ms,
        "request_to_first_text_ms":first.duration_since(request_start).as_secs_f64()*1000.0,
        "direct_first_text_ms":direct_ms,"startup_overhead_ms":startup_ms-direct_ms})
}
fn main() {
    let binary = std::env::args_os()
        .nth(1)
        .expect("usage: gateway_footprint /absolute/path/to/oxa [--native]");
    let native = std::env::args().nth(2).is_some_and(|s| s == "--native");
    let binary = Path::new(&binary).canonicalize().unwrap();
    let samples: Vec<_> = (0..5).map(|_| measure(&binary, native)).collect();
    println!(
        "{}",
        json!({"binary_bytes":fs::metadata(binary).unwrap().len(),"native":native,"samples":samples})
    );
}
