//! C10 acceptance tests: RPC server, run-control commands. Owner: task C10.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The server runs in a thread over an OS pipe, so a test can send a command while a run is
//! still in progress. A `GatedEngine` holds each model request until the test releases it (or
//! until the run is cancelled), which makes "during a run" deterministic.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, AssistantTurn, CancelToken, ChatRequest, Decision, Engine, EngineInfo, ExecResult,
    ExecSpec, Mode, OxaError, Policy, Sandbox, StreamEvent,
};
use oxa_rpc::{parse_command, response_err, response_ok, serve, Out, RpcCommand};
use oxa_session::{FsStore, SessionStore};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A policy for sessions without tools: every access is refused.
struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Deny("no tools in this test")
    }
    fn remember(&mut self, _a: &Access) {}
}

/// A sandbox that is never reached: the sessions here run no tools.
struct NoBwrap;

impl Sandbox for NoBwrap {
    fn check(&self) -> Result<(), OxaError> {
        Err(OxaError::new(codes::NO_SANDBOX, "no sandbox in this test"))
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

/// Holds every model request until `open` is set, or until the run is cancelled. Otherwise it
/// is the scripted engine. `models` replaces the model list the engine reports (when set).
struct GatedEngine {
    inner: Arc<ScriptedEngine>,
    open: Arc<AtomicBool>,
    models: Vec<String>,
}

impl Engine for GatedEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let mut info = self.inner.info(target)?;
        if !self.models.is_empty() {
            info.models = self.models.clone();
        }
        Ok(info)
    }

    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }

    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        while !self.open.load(Ordering::SeqCst) {
            if cancel.is_cancelled() {
                return Err(OxaError::new(
                    codes::ABORTED,
                    "aborted while the test held the request",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
        self.inner.chat(r, cancel, on)
    }
}

/// A session over `engine`, stored under the test's own agent directory.
fn open_session(env: &TestEnv, engine: Arc<dyn Engine>) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.agent_dir().join("sessions")));
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        // Exact prompt-byte fixtures select note-free queues/RPC/replay.
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    };
    Session::open(deps, Open::New).expect("the session opens over the engine")
}

/// A `Write` that appends to a shared buffer: the server's stdout in the tests.
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A running `serve` loop driven through its stdin pipe.
struct Rpc {
    input: Option<std::io::PipeWriter>,
    stdout: Arc<Mutex<Vec<u8>>>,
    thread: Option<JoinHandle<Result<i32, OxaError>>>,
    /// `agent_settled` records already consumed by [`Rpc::agent_settled`].
    settled: usize,
}

impl Rpc {
    fn start(session: Session, env: &TestEnv) -> Rpc {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let out: Out = Arc::new(Mutex::new(Box::new(SharedBuf(stdout.clone()))));
        let dirs = Dirs::under(env.root());
        let input: Box<dyn BufRead + Send> = Box::new(BufReader::new(reader));
        let thread = thread::spawn(move || serve(session, dirs, input, out));
        Rpc {
            input: Some(writer),
            stdout,
            thread: Some(thread),
            settled: 0,
        }
    }

    fn send_raw(&mut self, line: &str) {
        let w = self.input.as_mut().expect("stdin is open");
        writeln!(w, "{line}").expect("the server reads stdin");
        w.flush().expect("flush");
    }

    fn send(&mut self, command: Value) {
        self.send_raw(&command.to_string());
    }

    /// Every record written so far, in order.
    fn records(&self) -> Vec<Value> {
        let bytes = self.stdout.lock().unwrap().clone();
        bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("every stdout line is one JSON record"))
            .collect()
    }

    /// The first record that satisfies `pred`, waiting up to 20 seconds.
    fn wait_for(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            if let Some(r) = self.records().into_iter().find(|r| pred(r)) {
                return r;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!(
                    "timed out waiting for {what}; stdout so far: {:?}",
                    self.records()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn response_to(&self, id: &str) -> Value {
        self.wait_for(&format!("the response to {id}"), |r| {
            r["type"] == "response" && r["id"] == id
        })
    }

    /// Send a command with an id and wait for its response.
    fn call(&mut self, id: &str, mut command: Value) -> Value {
        command["id"] = Value::from(id);
        self.send(command);
        self.response_to(id)
    }

    /// The next `agent_settled`. The session is idle once it is written (the server releases
    /// every claim first), so the next command may follow at once.
    fn agent_settled(&mut self) -> Value {
        let n = self.settled;
        self.settled += 1;
        self.nth_settled(n)
    }

    /// The `n`th (0-based) `agent_settled` record, waiting up to 20 seconds.
    fn nth_settled(&self, n: usize) -> Value {
        let start = Instant::now();
        loop {
            let all: Vec<Value> = self
                .records()
                .into_iter()
                .filter(|r| r["type"] == "agent_settled")
                .collect();
            if let Some(r) = all.get(n) {
                return r.clone();
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "timed out waiting for agent_settled #{n}; stdout so far: {:?}",
                self.records()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Close stdin, wait for the server to finish its queued work, and return its exit code and
    /// every record it wrote.
    fn finish(mut self) -> (i32, Vec<Value>) {
        drop(self.input.take());
        let code = self
            .thread
            .take()
            .expect("the server thread")
            .join()
            .expect("the server thread does not panic")
            .expect("serve returns Ok");
        (code, self.records())
    }
}

fn has_settled(records: &[Value], aborted: bool) -> bool {
    records
        .iter()
        .any(|r| r["type"] == "agent_settled" && r["aborted"] == aborted)
}

/// docs example request/response/events
#[test]
fn prompt_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Hello back")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call("req-2", json!({"type": "prompt", "message": "Review"}));
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["command"], "prompt");
    assert_eq!(resp["success"], true);
    assert_eq!(resp["id"], "req-2");
    assert_eq!(resp["data"], json!({"disposition": "started"}));

    rpc.agent_settled();
    let (code, records) = rpc.finish();
    assert_eq!(code, 0);

    let response_at = records.iter().position(|r| r["type"] == "response").unwrap();
    let start_at = records
        .iter()
        .position(|r| r["type"] == "agent_start")
        .expect("agent_start");
    assert!(
        response_at < start_at,
        "the response comes before the run's events"
    );

    let reply = records
        .iter()
        .find(|r| r["type"] == "message_end" && r["message"]["role"] == "assistant")
        .expect("the assistant message_end");
    assert_eq!(
        reply["message"]["content"][0],
        json!({"type": "text", "text": "Hello back"})
    );
    assert_eq!(reply["message"]["stopReason"], "stop");
    assert!(has_settled(&records, false), "agent_settled with aborted false");
}

/// during a run
#[test]
fn steer_and_follow_up_fixtures() {
    let env = TestEnv::new();
    let open = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("first"),
            ScriptTurn::text("second"),
        ])),
        open: open.clone(),
        models: Vec::new(),
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "start"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");

    let steer = rpc.call("s", json!({"type": "steer", "message": "change course"}));
    assert_eq!(steer["success"], true);
    assert_eq!(steer["command"], "steer");
    assert_eq!(steer["data"], json!({"disposition": "queued"}));

    let follow = rpc.call("f", json!({"type": "follow_up", "message": "then this"}));
    assert_eq!(follow["success"], true);
    assert_eq!(follow["data"], json!({"disposition": "queued"}));

    // The run is held open: get_state answers during it, with the queued messages counted.
    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["isStreaming"], true);
    assert_eq!(state["data"]["pendingMessageCount"], 2);

    open.store(true, Ordering::SeqCst);
    rpc.agent_settled();

    let messages = rpc.call("m", json!({"type": "get_messages"}));
    let texts: Vec<&Value> = messages["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| &m["content"])
        .collect();
    assert!(texts.contains(&&json!("start")));
    assert!(
        texts.contains(&&json!("then this")),
        "the follow-up is delivered after the run: {texts:?}"
    );

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// agent_settled aborted
#[test]
fn abort_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(Vec::new())),
        open: Arc::new(AtomicBool::new(false)),
        models: Vec::new(),
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "long task"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");

    let resp = rpc.call("a", json!({"type": "abort"}));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["command"], "abort");

    // The abort response comes after the run has settled.
    let records = rpc.records();
    let settled_at = records.iter().position(|r| r["type"] == "agent_settled");
    let response_at = records
        .iter()
        .position(|r| r["type"] == "response" && r["id"] == "a");
    assert!(
        settled_at.is_some() && response_at.is_some(),
        "both records are written"
    );
    assert!(
        settled_at < response_at,
        "agent_settled comes before the abort response"
    );

    let (code, records) = rpc.finish();
    assert_eq!(code, 0);
    assert!(has_settled(&records, true), "agent_settled with aborted true");
}

/// returns queued
#[test]
fn clear_queue_fixture() {
    let env = TestEnv::new();
    let open = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("done")])),
        open: open.clone(),
        models: Vec::new(),
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "work"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");
    rpc.call("s", json!({"type": "steer", "message": "Change direction"}));
    rpc.call(
        "f",
        json!({"type": "follow_up", "message": "Summarize when finished"}),
    );

    let cleared = rpc.call("c", json!({"type": "clear_queue"}));
    assert_eq!(cleared["success"], true);
    assert_eq!(
        cleared["data"],
        json!({"steering": ["Change direction"], "followUp": ["Summarize when finished"]})
    );

    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["pendingMessageCount"], 0);

    open.store(true, Ordering::SeqCst);
    rpc.agent_settled();
    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// state changes
#[test]
fn new_session_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let before = rpc.call("g1", json!({"type": "get_state"}));
    let before_id = before["data"]["sessionId"].as_str().unwrap().to_string();

    let resp = rpc.call("n", json!({"type": "new_session"}));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"], json!({"cancelled": false}));

    let after = rpc.call("g2", json!({"type": "get_state"}));
    assert_ne!(
        after["data"]["sessionId"].as_str().unwrap(),
        before_id,
        "a new session id"
    );
    assert_eq!(after["data"]["messageCount"], 0);

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// fields
#[test]
fn get_state_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(resp["success"], true);
    let data = resp["data"].as_object().expect("the state is an object");
    for key in [
        "model",
        "thinkingLevel",
        "isStreaming",
        "isCompacting",
        "steeringMode",
        "followUpMode",
        "sessionFile",
        "sessionId",
        "autoCompactionEnabled",
        "autoRetryEnabled",
        "messageCount",
        "pendingMessageCount",
        "mode",
    ] {
        assert!(data.contains_key(key), "get_state has {key}");
    }
    assert_eq!(data["model"]["id"], "fake-model");
    // pi settings.md: `defaultThinkingLevel` defaults to `medium`.
    assert_eq!(data["thinkingLevel"], "medium");
    assert_eq!(data["isStreaming"], false);
    // pi's default for both queue modes (rpc-commands.md: "one-at-a-time (default)").
    assert_eq!(data["steeringMode"], "one-at-a-time");
    assert_eq!(data["followUpMode"], "one-at-a-time");
    assert_eq!(data["autoCompactionEnabled"], true);
    assert_eq!(data["mode"], "workspace");

    rpc.finish();
}

/// message shapes
#[test]
fn get_messages_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("hi there")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "hello"}));
    rpc.agent_settled();

    let resp = rpc.call("m", json!({"type": "get_messages"}));
    let messages = resp["data"]["messages"].as_array().expect("a message list");
    assert_eq!(messages.len(), 2);

    let user = &messages[0];
    assert_eq!(user["role"], "user");
    assert_eq!(user["content"], "hello");
    assert!(user["timestamp"].is_u64());

    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(
        assistant["content"],
        json!([{"type": "text", "text": "hi there"}])
    );
    assert_eq!(assistant["stopReason"], "stop");
    assert_eq!(assistant["provider"], "oxe");
    assert_eq!(assistant["api"], "openai-completions");
    assert_eq!(assistant["model"], "fake-model");
    assert!(assistant["usage"].is_object());

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// set/cycle/get_available
#[test]
fn model_commands_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(Vec::new())),
        open: Arc::new(AtomicBool::new(true)),
        models: vec!["fake-model".to_string(), "second-model".to_string()],
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let available = rpc.call("a", json!({"type": "get_available_models"}));
    let ids: Vec<&str> = available["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["fake-model", "second-model"]);

    let set = rpc.call(
        "s",
        json!({"type": "set_model", "provider": "oxe", "modelId": "second-model"}),
    );
    assert_eq!(set["success"], true);
    assert_eq!(set["data"]["id"], "second-model");
    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["model"]["id"], "second-model");

    let cycled = rpc.call("c", json!({"type": "cycle_model"}));
    assert_eq!(cycled["data"]["model"]["id"], "fake-model");
    assert_eq!(cycled["data"]["isScoped"], false);

    let unknown = rpc.call("u", json!({"type": "set_model", "modelId": "no-such-model"}));
    assert_eq!(unknown["success"], false);
    assert!(unknown["error"].as_str().unwrap().contains("Model not found"));

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// set/cycle/levels
#[test]
fn thinking_commands_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let set = rpc.call("s", json!({"type": "set_thinking_level", "level": "high"}));
    assert_eq!(set["success"], true);
    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["thinkingLevel"], "high");

    let cycled = rpc.call("c", json!({"type": "cycle_thinking_level"}));
    assert_eq!(cycled["data"], json!({"level": "off"}), "high wraps to off");

    let levels = rpc.call("l", json!({"type": "get_available_thinking_levels"}));
    assert_eq!(
        levels["data"]["levels"],
        json!(["off", "minimal", "low", "medium", "high"])
    );

    let bad = rpc.call("b", json!({"type": "set_thinking_level", "level": "ludicrous"}));
    assert_eq!(bad["success"], false);

    rpc.finish();
}

/// steering/follow-up modes
#[test]
fn queue_mode_commands_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let steering = rpc.call("s", json!({"type": "set_steering_mode", "mode": "all"}));
    assert_eq!(steering["success"], true);
    let follow = rpc.call("f", json!({"type": "set_follow_up_mode", "mode": "all"}));
    assert_eq!(follow["success"], true);

    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["steeringMode"], "all");
    assert_eq!(state["data"]["followUpMode"], "all");

    let bad = rpc.call("b", json!({"type": "set_follow_up_mode", "mode": "sometimes"}));
    assert_eq!(bad["success"], false);

    rpc.finish();
}

/// oxa addition
#[test]
fn set_mode_command() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call("m", json!({"type": "set_mode", "mode": "plan"}));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["command"], "set_mode");

    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["mode"], "plan");

    let (code, records) = {
        let changed = rpc.wait_for("mode_changed", |r| r["type"] == "mode_changed");
        assert_eq!(changed["mode"], "plan");
        rpc.finish()
    };
    assert_eq!(code, 0);
    assert!(records
        .iter()
        .any(|r| r["type"] == "mode_changed" && r["mode"] == "plan"));

    let bad_env = TestEnv::new();
    let mut rpc = Rpc::start(
        open_session(&bad_env, Arc::new(ScriptedEngine::new(Vec::new()))),
        &bad_env,
    );
    let bad = rpc.call("b", json!({"type": "set_mode", "mode": "yolo"}));
    assert_eq!(bad["success"], false);
    rpc.finish();
}

/// bad line
#[test]
fn parse_error_response_keeps_running() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.send_raw("this is not json");
    let parse = rpc.wait_for("the parse error", |r| r["command"] == "parse");
    assert_eq!(parse["type"], "response");
    assert_eq!(parse["success"], false);
    assert!(parse["error"]
        .as_str()
        .unwrap()
        .starts_with("Failed to parse command"));
    assert!(parse.get("id").is_none(), "a line with no command has no id");

    let after = rpc.call("after-bad", json!({"type": "get_state"}));
    assert_eq!(
        after["success"], true,
        "the server keeps running after a bad line"
    );

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// correlation id
#[test]
fn id_echoed() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let with_id = rpc.call("abc-7", json!({"type": "get_state"}));
    assert_eq!(with_id["id"], "abc-7");
    assert_eq!(with_id["command"], "get_state");

    rpc.send(json!({"type": "get_state"}));
    let without = rpc.wait_for("a response without id", |r| {
        r["type"] == "response" && r.get("id").is_none() && r["command"] == "get_state"
    });
    assert_eq!(without["success"], true);

    rpc.finish();
}

/// A prompt during a run needs streamingBehavior; with it the prompt is queued.
#[test]
fn prompt_while_busy_needs_streaming_behavior() {
    let env = TestEnv::new();
    let open = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
        ])),
        open: open.clone(),
        models: Vec::new(),
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p1", json!({"type": "prompt", "message": "first"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");

    let refused = rpc.call("p2", json!({"type": "prompt", "message": "second"}));
    assert_eq!(refused["success"], false);

    let queued = rpc.call(
        "p3",
        json!({"type": "prompt", "message": "third", "streamingBehavior": "followUp"}),
    );
    assert_eq!(queued["success"], true);
    assert_eq!(queued["data"], json!({"disposition": "queued"}));

    // A session command during a run is refused as busy.
    let busy = rpc.call("st", json!({"type": "set_auto_retry", "enabled": false}));
    assert_eq!(busy["success"], false);
    assert!(busy["error"].as_str().unwrap().contains("busy"));

    open.store(true, Ordering::SeqCst);
    rpc.agent_settled();
    // Idle again: the same command is answered.
    let idle = rpc.call("st2", json!({"type": "set_auto_retry", "enabled": false}));
    assert_eq!(idle["success"], true);
    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// Unit check on the helpers the server exposes: the error envelope keeps the id.
#[test]
fn response_envelopes_keep_id() {
    let cmd: RpcCommand = parse_command(r#"{"id":"x-1","type":"abort_retry"}"#).unwrap();
    let ok = response_ok(&cmd, Value::Null);
    assert_eq!(
        ok,
        json!({"type": "response", "id": "x-1", "command": "abort_retry", "success": true})
    );
    let err = response_err(&cmd, &OxaError::new(codes::BUSY, "busy"));
    assert_eq!(err["success"], false);
    assert_eq!(err["error"], "busy");
    assert_eq!(err["id"], "x-1");
}
