//! C11 acceptance tests: RPC session commands.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The server runs in a thread over an OS pipe, as in the C10 tests. Each test drives the
//! commands through stdin and reads the records from stdout.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, Mode, OxaError, Policy, Sandbox,
};
use oxa_rpc::{serve, Out};
use oxa_session::{FsStore, SessionStore};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
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

/// A sandbox that is never reached: the sessions here run no tools, and `!cmd` is unsandboxed.
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

/// A session over `engine`, stored under the test's own agent directory.
fn open_session_with(env: &TestEnv, engine: Arc<dyn Engine>, settings: Settings) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.agent_dir().join("sessions")));
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        // This fixture compares exact prompt/prefill bytes, with notes explicitly off.
        settings: settings.with_override("oxa.envNote", serde_json::json!(false)),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, Open::New).expect("the session opens over the engine")
}

fn open_session(env: &TestEnv, engine: Arc<dyn Engine>) -> Session {
    // Background cache warming would spend scripted turns; keep the scripts exact.
    open_session_with(
        env,
        engine,
        Settings::from_values(json!({"cacheWarming": "off"}), json!({})),
    )
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

    fn send(&mut self, command: Value) {
        let w = self.input.as_mut().expect("stdin is open");
        writeln!(w, "{command}").expect("the server reads stdin");
        w.flush().expect("flush");
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
    fn agent_settled(&mut self) {
        let n = self.settled;
        self.settled += 1;
        let start = Instant::now();
        while self
            .records()
            .iter()
            .filter(|r| r["type"] == "agent_settled")
            .count()
            <= n
        {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "timed out waiting for agent_settled #{n}; stdout so far: {:?}",
                self.records()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Send a prompt, wait for its response and for the run to settle.
    fn prompt(&mut self, id: &str, message: &str) {
        let resp = self.call(id, json!({"type": "prompt", "message": message}));
        assert_eq!(resp["success"], true, "prompt accepted: {resp}");
        self.agent_settled();
    }

    fn state(&mut self, id: &str) -> Value {
        let resp = self.call(id, json!({"type": "get_state"}));
        assert_eq!(resp["success"], true, "get_state: {resp}");
        resp["data"].clone()
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

/// result fields
#[test]
fn compact_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("The parser reads tokens."),
        ScriptTurn::text("The lexer produces tokens."),
        ScriptTurn::text("Summary: the parser and the lexer were discussed."),
    ]));
    // A tiny keep budget makes the cut fall before the last reply, so there is something to summarise.
    let settings = Settings::from_values(
        json!({"cacheWarming": "off", "compaction": {"keepRecentTokens": 1}}),
        json!({}),
    );
    let mut rpc = Rpc::start(open_session_with(&env, engine, settings), &env);
    rpc.prompt("p1", "Explain the parser");
    rpc.prompt("p2", "Explain the lexer");

    let resp = rpc.call(
        "c1",
        json!({"type": "compact", "customInstructions": "Keep it short"}),
    );
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["command"], "compact");
    assert_eq!(resp["success"], true, "compact: {resp}");
    let data = &resp["data"];
    assert_eq!(
        data["summary"],
        "Summary: the parser and the lexer were discussed."
    );
    assert!(
        data["firstKeptEntryId"].as_str().is_some_and(|s| !s.is_empty()),
        "firstKeptEntryId: {data}"
    );
    assert!(data["tokensBefore"].is_u64(), "tokensBefore: {data}");
    assert!(
        data["estimatedTokensAfter"].is_u64(),
        "estimatedTokensAfter: {data}"
    );
    assert!(
        data["usage"]["totalTokens"].is_u64(),
        "usage of the summary request: {data}"
    );
    assert!(data.get("details").is_some(), "details: {data}");

    let end = rpc.wait_for("compaction_end", |r| r["type"] == "compaction_end");
    assert_eq!(end["reason"], "manual");
    assert_eq!(end["aborted"], false);

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// bash_execution_update events
#[test]
fn bash_fixture_and_updates() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call(
        "b-1",
        json!({"type": "bash", "command": "printf 'alpha\\nbeta\\n'"}),
    );
    assert_eq!(resp["command"], "bash");
    assert_eq!(resp["success"], true, "bash: {resp}");
    assert_eq!(resp["data"]["output"], "alpha\nbeta\n");
    assert_eq!(resp["data"]["exitCode"], 0);
    assert_eq!(resp["data"]["cancelled"], false);
    assert_eq!(resp["data"]["truncated"], false);
    assert!(resp["data"].get("fullOutputPath").is_none());

    // The streamed chunks carry the request id and arrive before the response.
    let records = rpc.records();
    let response_at = records
        .iter()
        .position(|r| r["type"] == "response" && r["id"] == "b-1")
        .unwrap();
    let updates: Vec<(usize, &Value)> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r["type"] == "bash_execution_update")
        .collect();
    assert!(!updates.is_empty(), "bash_execution_update records: {records:?}");
    for (at, u) in &updates {
        assert_eq!(u["id"], "b-1", "every update names its request: {u}");
        assert!(*at < response_at, "updates come before the response");
    }
    let streamed: String = updates
        .iter()
        .map(|(_, u)| u["delta"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(streamed, "alpha\nbeta\n");

    // A failing command reports its exit code and is not cancelled.
    let failed = rpc.call("b-2", json!({"type": "bash", "command": "exit 3"}));
    assert_eq!(failed["success"], true);
    assert_eq!(failed["data"]["exitCode"], 3);
    assert_eq!(failed["data"]["cancelled"], false);

    // `!!cmd` runs too; it stays out of the model context.
    let hidden = rpc.call(
        "b-3",
        json!({"type": "bash", "command": "echo hidden", "excludeFromContext": true}),
    );
    assert_eq!(hidden["success"], true);
    assert_eq!(hidden["data"]["output"], "hidden\n");

    // Nothing is running, so abort_bash is a successful no-op.
    let abort = rpc.call("b-4", json!({"type": "abort_bash"}));
    assert_eq!(abort["success"], true);
    assert_eq!(abort["command"], "abort_bash");

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// get_session_stats
#[test]
fn stats_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Stats reply.")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);
    rpc.prompt("p1", "Count me");

    let resp = rpc.call("s1", json!({"type": "get_session_stats"}));
    assert_eq!(resp["command"], "get_session_stats");
    assert_eq!(resp["success"], true, "stats: {resp}");
    let data = &resp["data"];
    assert_eq!(data["userMessages"], 1);
    assert_eq!(data["assistantMessages"], 1);
    assert_eq!(data["toolCalls"], 0);
    assert_eq!(data["toolResults"], 0);
    assert_eq!(data["totalMessages"], 2);
    assert_eq!(data["cost"], 0);
    let tokens = &data["tokens"];
    let sum = ["input", "output", "cacheRead", "cacheWrite"]
        .iter()
        .map(|k| tokens[*k].as_u64().expect("a token count"))
        .sum::<u64>();
    assert_eq!(
        tokens["total"].as_u64(),
        Some(sum),
        "total is the sum of the parts: {tokens}"
    );
    assert!(
        data["contextUsage"]["contextWindow"]
            .as_u64()
            .is_some_and(|n| n > 0),
        "{data}"
    );
    assert!(data["contextUsage"]["tokens"].is_u64(), "{data}");

    let state = rpc.state("st");
    assert_eq!(data["sessionId"], state["sessionId"]);
    assert!(
        data["sessionFile"]
            .as_str()
            .is_some_and(|p| p.ends_with(".jsonl")),
        "{data}"
    );

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// abort_bash cancels a running bash (answered on the reader, ahead of the busy check)
#[test]
fn abort_bash_cancels_a_running_bash() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let started = std::time::Instant::now();
    let mut cmd = json!({"type": "bash", "command": "echo up; sleep 30"});
    cmd["id"] = json!("long");
    rpc.send(cmd);
    rpc.wait_for("the first bash chunk", |r| r["type"] == "bash_execution_update");
    let abort = rpc.call("ab", json!({"type": "abort_bash"}));
    assert_eq!(abort["success"], true, "abort_bash: {abort}");
    let resp = rpc.response_to("long");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the sleep was cut short"
    );
    assert_eq!(resp["success"], true, "bash: {resp}");
    assert_eq!(resp["data"]["cancelled"], true);
}

/// path returned
#[test]
fn export_html_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Export me.")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);
    rpc.prompt("p1", "Export this");

    let target = env.work().join("session-export.html");
    let target_text = target.to_string_lossy().into_owned();
    let resp = rpc.call("e1", json!({"type": "export_html", "outputPath": target_text}));
    assert_eq!(resp["command"], "export_html");
    assert_eq!(resp["success"], true, "export_html: {resp}");
    assert_eq!(resp["data"]["path"], target_text);
    let html = std::fs::read_to_string(&target).expect("the export file exists");
    assert!(html.contains("<html"), "an HTML document was written");

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// session changes
#[test]
fn switch_fork_clone_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Noted.")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);
    rpc.prompt("p1", "Remember the fork point");

    let original = rpc.state("s0");
    let original_id = original["sessionId"].clone();
    let original_file = original["sessionFile"]
        .as_str()
        .expect("a session file")
        .to_string();

    let fork_messages = rpc.call("fm", json!({"type": "get_fork_messages"}));
    let entry_id = fork_messages["data"]["messages"][0]["entryId"]
        .as_str()
        .expect("a user entry")
        .to_string();
    assert_eq!(
        fork_messages["data"]["messages"][0]["text"],
        "Remember the fork point"
    );

    let fork = rpc.call("f1", json!({"type": "fork", "entryId": entry_id}));
    assert_eq!(fork["success"], true, "fork: {fork}");
    assert_eq!(fork["data"]["text"], "Remember the fork point");
    assert_eq!(fork["data"]["cancelled"], false);
    let forked = rpc.state("s1");
    assert_ne!(forked["sessionId"], original_id, "the fork is a new session");

    let clone = rpc.call("c1", json!({"type": "clone"}));
    assert_eq!(clone["success"], true, "clone: {clone}");
    assert_eq!(clone["data"]["cancelled"], false);
    let cloned = rpc.state("s2");
    assert_ne!(
        cloned["sessionId"], forked["sessionId"],
        "the clone is a new session"
    );

    let switch = rpc.call(
        "w1",
        json!({"type": "switch_session", "sessionPath": original_file}),
    );
    assert_eq!(switch["success"], true, "switch_session: {switch}");
    assert_eq!(switch["data"]["cancelled"], false);
    let back = rpc.state("s3");
    assert_eq!(
        back["sessionId"], original_id,
        "switched back to the original session"
    );

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// get_entries / get_tree / get_fork_messages
#[test]
fn entries_and_tree_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Hi tree.")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);
    rpc.prompt("p1", "Hello tree");

    let all = rpc.call("e1", json!({"type": "get_entries"}));
    assert_eq!(all["success"], true, "get_entries: {all}");
    let entries = all["data"]["entries"]
        .as_array()
        .expect("an entries array")
        .clone();
    assert!(
        entries.len() >= 3,
        "the prefix, the user and the assistant entries: {entries:?}"
    );
    let last_id = entries.last().unwrap()["id"].clone();
    assert_eq!(
        all["data"]["leafId"], last_id,
        "the leaf is the last entry of the branch"
    );
    let user = entries
        .iter()
        .find(|e| e["message"]["role"] == "user")
        .expect("the user message entry")
        .clone();
    assert_eq!(user["message"]["content"], "Hello tree");

    // `since` returns only the entries strictly after the cursor.
    let since = rpc.call("e2", json!({"type": "get_entries", "since": entries[0]["id"]}));
    assert_eq!(since["success"], true);
    let tail = since["data"]["entries"].as_array().unwrap();
    assert_eq!(tail.len(), entries.len() - 1);
    assert_eq!(tail[0]["id"], entries[1]["id"]);

    let tree = rpc.call("t1", json!({"type": "get_tree"}));
    assert_eq!(tree["success"], true, "get_tree: {tree}");
    assert_eq!(tree["data"]["leafId"], last_id);
    let roots = tree["data"]["tree"].as_array().expect("a tree array");
    assert_eq!(roots.len(), 1, "one root for a linear session");
    assert_eq!(roots[0]["entry"]["id"], entries[0]["id"]);
    assert_eq!(roots[0]["children"][0]["entry"]["id"], entries[1]["id"]);

    let forks = rpc.call("m1", json!({"type": "get_fork_messages"}));
    let messages = forks["data"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["entryId"], user["id"]);
    assert_eq!(messages[0]["text"], "Hello tree");

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// get_last_assistant_text / set_session_name
#[test]
fn last_text_and_name_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("The final answer.")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let none = rpc.call("t0", json!({"type": "get_last_assistant_text"}));
    assert_eq!(none["success"], true);
    assert_eq!(none["data"]["text"], Value::Null, "no assistant text yet");

    rpc.prompt("p1", "Say it");
    let text = rpc.call("t1", json!({"type": "get_last_assistant_text"}));
    assert_eq!(text["data"]["text"], "The final answer.");

    let named = rpc.call(
        "n1",
        json!({"type": "set_session_name", "name": "  my feature  "}),
    );
    assert_eq!(named["success"], true, "set_session_name: {named}");
    assert!(named.get("data").is_none(), "no data field: {named}");
    let state = rpc.state("st");
    assert_eq!(state["sessionName"], "my feature", "the name is trimmed");

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// shape
#[test]
fn get_commands_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call("c1", json!({"type": "get_commands"}));
    assert_eq!(resp["command"], "get_commands");
    assert_eq!(resp["success"], true, "get_commands: {resp}");
    let commands = resp["data"]["commands"].as_array().expect("a commands array");
    for c in commands {
        assert!(
            c["name"]
                .as_str()
                .is_some_and(|n| !n.is_empty() && !n.starts_with('/')),
            "name: {c}"
        );
        assert!(c["description"].is_string(), "description: {c}");
        let source = c["source"].as_str().expect("a source");
        assert!(["extension", "prompt", "skill"].contains(&source), "source: {c}");
    }

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// success false + error
#[test]
fn error_shapes() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let cases = [
        (
            json!({"type": "get_entries", "since": "nope0000"}),
            "get_entries",
            "Entry not found",
        ),
        (
            json!({"type": "fork", "entryId": "nope0000"}),
            "fork",
            "Invalid entry ID for forking",
        ),
        (
            json!({"type": "set_session_name", "name": "   "}),
            "set_session_name",
            "cannot be empty",
        ),
        (json!({"type": "bash"}), "bash", "command"),
        (
            json!({"type": "switch_session", "sessionPath": "/no/such/session.jsonl"}),
            "switch_session",
            "",
        ),
    ];
    for (n, (command, name, needle)) in cases.into_iter().enumerate() {
        let id = format!("err-{n}");
        let mut command = command;
        command["id"] = Value::from(id.as_str());
        rpc.send(command);
        let resp = rpc.response_to(&id);
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], name);
        assert_eq!(resp["success"], false, "{name} fails: {resp}");
        let message = resp["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} has an error string: {resp}"));
        assert!(message.contains(needle), "{name}: {message}");
        assert!(resp.get("data").is_none(), "a failure has no data: {resp}");
    }

    let (code, _) = rpc.finish();
    assert_eq!(code, 1);
}

/// exit code 0
#[test]
fn shutdown_on_stdin_close() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Last words.")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    // Queue a run and close stdin straight away: the queued work finishes, then the exit is 0.
    let resp = rpc.call("p1", json!({"type": "prompt", "message": "Bye"}));
    assert_eq!(resp["success"], true);
    rpc.send(json!({"type": "get_state", "id": "late"}));

    let (code, records) = rpc.finish();
    assert_eq!(code, 0, "exit code 0 after stdin closes");
    assert!(has_settled(&records, false), "the queued run settled before exit");
    // This read is allowed while busy, so it cannot add a rejected-command outcome.
    let late = records
        .iter()
        .find(|r| r["type"] == "response" && r["id"] == "late")
        .expect("the late state read is answered before exit");
    assert_eq!(late["success"], true);
}
