//! Wave H (lane run): ordering guarantees of the RPC server.
//!
//! * A command a client sends the moment it reads `agent_settled` (or a response) is accepted:
//!   every claim is released before the record is written. The tests pipeline the next command
//!   from inside the server's own stdout write, which is the tightest a client can be, and
//!   repeat it many times.
//! * A `get_state` sent after a response (or pipelined behind a command) reflects that command.
//! * Stdin EOF while an untimed dialog waits for an answer: the dialog resolves as cancelled,
//!   later dialogs resolve at once, and `serve` returns.
//!
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, Answer, AssistantTurn, CancelToken, ChatRequest, CommandSpec, Decision, Engine,
    EngineInfo, ExecResult, ExecSpec, HookPoint, HookReply, Hooks, Mode, OxaError, Policy, Sandbox,
    StreamEvent, Tool, Ui, Value,
};
use oxa_rpc::{serve, Out};
use serde_json::json;
use std::io::{BufRead, BufReader, PipeWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Deny("no tools in this test")
    }
    fn remember(&mut self, _a: &Access) {}
}

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

fn open_session(env: &TestEnv, turns: Vec<ScriptTurn>, hooks: Vec<Arc<dyn Hooks>>) -> Session {
    open_with(env, Arc::new(ScriptedEngine::new(turns)), hooks)
}

fn open_with(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Vec<Arc<dyn Hooks>>) -> Session {
    let deps = Deps {
        engine,
        store: Arc::new(oxa_session::MemStore::new()),
        tools: Vec::new(),
        hooks,
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        settings: Settings::default(),
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
    Session::open(deps, Open::New).expect("open")
}

/// What the scripted client sends back when it sees a record.
type Reaction = Box<dyn FnMut(&Value) -> Vec<Value> + Send>;

/// The server's stdout. Every record is kept; `react` sees each one as it is written and the
/// commands it returns go into the server's stdin before the write returns.
struct Reactive {
    lines: Arc<Mutex<Vec<Value>>>,
    pending: Vec<u8>,
    input: Arc<Mutex<Option<PipeWriter>>>,
    react: Reaction,
}

impl Write for Reactive {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(data);
        while let Some(i) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=i).collect();
            let v: Value = serde_json::from_slice(&line[..line.len() - 1]).expect("one JSON record per line");
            for cmd in (self.react)(&v) {
                if let Some(w) = self.input.lock().unwrap().as_mut() {
                    writeln!(w, "{cmd}").expect("write a command");
                    w.flush().expect("flush");
                }
            }
            self.lines.lock().unwrap().push(v);
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Served {
    lines: Arc<Mutex<Vec<Value>>>,
    input: Arc<Mutex<Option<PipeWriter>>>,
    done: mpsc::Receiver<Result<i32, OxaError>>,
}

impl Served {
    fn start(session: Session, env: &TestEnv, react: Reaction) -> Served {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let input = Arc::new(Mutex::new(Some(writer)));
        let out: Out = Arc::new(Mutex::new(Box::new(Reactive {
            lines: lines.clone(),
            pending: Vec::new(),
            input: input.clone(),
            react,
        })));
        let dirs = Dirs::under(env.root());
        let stdin: Box<dyn BufRead + Send> = Box::new(BufReader::new(reader));
        let (tx, done) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(serve(session, dirs, stdin, out));
        });
        Served { lines, input, done }
    }

    fn send(&self, cmd: Value) {
        let mut guard = self.input.lock().unwrap();
        let w = guard.as_mut().expect("stdin open");
        writeln!(w, "{cmd}").expect("write");
        w.flush().expect("flush");
    }

    fn lines(&self) -> Vec<Value> {
        self.lines.lock().unwrap().clone()
    }

    /// Wait until `pred` holds for the records so far (at most 30 s).
    fn wait(&self, what: &str, pred: impl Fn(&[Value]) -> bool) {
        let start = Instant::now();
        while !pred(&self.lines()) {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}: {:?}",
                self.lines()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Close stdin; `serve` must return within `secs`.
    fn close(self, secs: u64, expected_code: i32) -> Vec<Value> {
        self.input.lock().unwrap().take();
        match self.done.recv_timeout(Duration::from_secs(secs)) {
            Ok(r) => assert_eq!(r.expect("serve returns Ok"), expected_code),
            Err(_) => panic!(
                "serve did not return {secs} s after stdin closed: {:?}",
                self.lines()
            ),
        }
        self.lines()
    }
}

fn response<'a>(lines: &'a [Value], id: &str) -> Option<&'a Value> {
    lines.iter().find(|v| v["type"] == "response" && v["id"] == id)
}

const ROUNDS: usize = 60;

/// The next prompt goes into stdin from inside the write of `agent_settled`: it must be
/// accepted (`started`), never refused as busy. Repeated `ROUNDS` times.
#[test]
fn prompt_pipelined_on_agent_settled_is_accepted() {
    let env = TestEnv::new();
    let turns = (0..=ROUNDS).map(|i| ScriptTurn::text(&format!("r{i}"))).collect();
    let session = open_session(&env, turns, Vec::new());
    let mut n = 0;
    let react: Reaction = Box::new(move |v| {
        if v["type"] == "agent_settled" && n < ROUNDS {
            n += 1;
            return vec![json!({"id": format!("p{n}"), "type": "prompt", "message": format!("q{n}")})];
        }
        Vec::new()
    });
    let s = Served::start(session, &env, react);
    s.send(json!({"id": "p0", "type": "prompt", "message": "q0"}));
    s.wait("every run to settle", |l| {
        l.iter().filter(|v| v["type"] == "agent_settled").count() == ROUNDS + 1
    });
    let lines = s.close(10, 0);
    for i in 0..=ROUNDS {
        let r = response(&lines, &format!("p{i}")).unwrap_or_else(|| panic!("no response to p{i}"));
        assert_eq!(r["success"], true, "p{i} refused: {r}");
        assert_eq!(r["data"]["disposition"], "started", "{r}");
    }
    assert!(
        lines
            .iter()
            .all(|v| v["type"] != "agent_settled" || v["aborted"] == false),
        "no run aborted"
    );
}

/// Session commands pipelined on `agent_settled` are accepted, and a `get_state` pipelined on
/// their response reports their effect.
#[test]
fn commands_and_get_state_pipelined_on_records_see_fresh_state() {
    let env = TestEnv::new();
    let turns = (0..=ROUNDS).map(|i| ScriptTurn::text(&format!("r{i}"))).collect();
    let session = open_session(&env, turns, Vec::new());
    let mut n = 0;
    let react: Reaction = Box::new(move |v| {
        if v["type"] == "agent_settled" && n < ROUNDS {
            n += 1;
            return vec![
                json!({"id": format!("name{n}"), "type": "set_session_name", "name": format!("s{n}")}),
                json!({"id": format!("piped{n}"), "type": "get_state"}),
            ];
        }
        if v["type"] == "response" && v["command"] == "set_session_name" {
            let i = v["id"]
                .as_str()
                .unwrap_or("")
                .trim_start_matches("name")
                .to_string();
            return vec![
                json!({"id": format!("after{i}"), "type": "get_state"}),
                json!({"id": format!("p{i}"), "type": "prompt", "message": format!("q{i}")}),
            ];
        }
        Vec::new()
    });
    let s = Served::start(session, &env, react);
    s.send(json!({"id": "p0", "type": "prompt", "message": "q0"}));
    s.wait("every run to settle", |l| {
        l.iter().filter(|v| v["type"] == "agent_settled").count() == ROUNDS + 1
    });
    let lines = s.close(10, 0);
    for i in 1..=ROUNDS {
        let name = response(&lines, &format!("name{i}")).expect("name response");
        assert_eq!(
            name["success"], true,
            "set_session_name after agent_settled refused: {name}"
        );
        for id in [format!("piped{i}"), format!("after{i}")] {
            let st = response(&lines, &id).unwrap_or_else(|| panic!("no response to {id}"));
            assert_eq!(st["data"]["sessionName"], format!("s{i}"), "{id}: {st}");
            assert_eq!(st["data"]["isStreaming"], false, "{id}: {st}");
        }
        let p = response(&lines, &format!("p{i}")).expect("prompt response");
        assert_eq!(p["data"]["disposition"], "started", "{p}");
    }
}

/// Holds every request until the run is cancelled (then `aborted`); after 5 s it answers.
struct HoldUntilCancel(ScriptedEngine);

impl Engine for HoldUntilCancel {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.0.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.0.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if cancel.is_cancelled() {
                return Err(OxaError::new(codes::ABORTED, "aborted"));
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.0.chat(r, cancel, on)
    }
}

/// `abort` sent the moment the prompt is answered `started` (before or after its run claimed
/// the session, whichever the threads give): that run is aborted, and the abort is answered
/// after `agent_settled` (aborted: true). The window before the claim is covered
/// deterministically by oxa-core's `abort_next_aborts_the_next_run`.
#[test]
fn abort_right_after_started_aborts_that_run() {
    for _ in 0..20 {
        let env = TestEnv::new();
        let engine = Arc::new(HoldUntilCancel(ScriptedEngine::new(vec![ScriptTurn::text(
            "never",
        )])));
        let session = open_with(&env, engine, Vec::new());
        let react: Reaction = Box::new(|v| {
            if v["type"] == "response" && v["id"] == "p" {
                return vec![json!({"id": "a", "type": "abort"})];
            }
            Vec::new()
        });
        let s = Served::start(session, &env, react);
        s.send(json!({"id": "p", "type": "prompt", "message": "go"}));
        s.wait("the abort response", |l| response(l, "a").is_some());
        let lines = s.close(10, 130);
        let settled = lines
            .iter()
            .position(|v| v["type"] == "agent_settled")
            .expect("agent_settled");
        let abort = lines
            .iter()
            .position(|v| v["type"] == "response" && v["id"] == "a")
            .expect("abort response");
        assert!(
            settled < abort,
            "agent_settled before the abort response: {lines:?}"
        );
        assert_eq!(lines[settled]["aborted"], true, "{lines:?}");
    }
}

/// Records which dialog results a command saw.
#[derive(Default)]
struct Dialogs {
    seen: Mutex<Vec<String>>,
}

impl Hooks for Dialogs {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: "ask".into(),
            description: String::new(),
            argument_hint: None,
            source: oxa_proto::Source::Extension(String::new()),
        }]
    }
    fn run_command(&self, _name: &str, _args: &str, ui: &mut dyn Ui) -> Result<(), OxaError> {
        let mut seen = Vec::new();
        seen.push(format!("input:{:?}", ui.input("Name?", "")));
        seen.push(format!("editor:{:?}", ui.editor("Text?", "")));
        seen.push(format!("confirm:{:?}", ui.confirm("Sure?", "", None)));
        seen.push(format!(
            "select:{:?}",
            ui.select("Pick", &["a".into(), "b".into()], None)
        ));
        let access = Access::Read(PathBuf::from("/x"));
        seen.push(format!("access:{}", ui.ask_access(&access) == Answer::Deny));
        *self.seen.lock().unwrap() = seen;
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Stdin EOF while an untimed dialog waits: it resolves as cancelled, the following dialogs
/// (and the permission ask) resolve at once, and `serve` returns.
#[test]
fn eof_cancels_waiting_dialogs_and_serve_returns() {
    let env = TestEnv::new();
    let hooks = Arc::new(Dialogs::default());
    let session = open_session(&env, Vec::new(), vec![hooks.clone()]);
    let s = Served::start(session, &env, Box::new(|_| Vec::new()));
    s.send(json!({"id": "p", "type": "prompt", "message": "/ask"}));
    s.wait("the input dialog", |l| {
        l.iter()
            .any(|v| v["type"] == "extension_ui_request" && v["method"] == "input")
    });
    let lines = s.close(10, 0);
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        vec![
            "input:None",
            "editor:None",
            "confirm:None",
            "select:None",
            "access:true"
        ]
    );
    let r = response(&lines, "p").expect("the command is answered before exit");
    assert_eq!(r["data"]["disposition"], "handled", "{r}");
}

/// Where [`Blocker`] holds the session thread.
#[derive(Clone, Copy, PartialEq)]
enum Hold {
    /// In `Hooks::commands` (a `get_commands` job): the accepted prompt waits behind it and
    /// has not claimed the session.
    BeforeClaim,
    /// In the `input` hook: the prompt's run has claimed the session, no request was sent.
    AfterClaim,
}

/// Holds the session thread once at `at`: reports `entered`, then waits for `release`.
struct Blocker {
    at: Hold,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Blocker {
    fn hold(&self) {
        let entered = self.entered.lock().unwrap().take();
        if let Some(tx) = entered {
            let _ = tx.send(());
            let rx = self.release.lock().unwrap().take().expect("release channel");
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }
}

impl Hooks for Blocker {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if self.at == Hold::AfterClaim && p == HookPoint::Input {
            self.hold();
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// A `Hooks` for `BeforeClaim`: blocks in `commands`, which `get_commands` calls on the
/// session thread without claiming the session.
struct CommandsBlocker(Blocker);

impl Hooks for CommandsBlocker {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        if *ARMED.lock().unwrap() {
            *ARMED.lock().unwrap() = false;
            self.0.hold();
        }
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Armed by the `BeforeClaim` scenario right before it sends `get_commands`.
static ARMED: Mutex<bool> = Mutex::new(false);

/// Records whether the engine was asked for a reply (and holds it until cancelled).
struct Probe {
    inner: HoldUntilCancel,
    entered: Mutex<Option<mpsc::Sender<()>>>,
}

impl Engine for Probe {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
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
        if let Some(tx) = self.entered.lock().unwrap().take() {
            let _ = tx.send(());
        }
        self.inner.chat(r, cancel, on)
    }
}

/// `abort` delivered at each point between "prompt accepted" and the first engine byte:
/// (1) before the run claimed the session (the session thread is busy with an earlier job),
/// (2) after the claim and before any request (inside the `input` hook), (3) while the request
/// waits for its first byte. Each time the run ends aborted, `agent_settled {aborted: true}`
/// is written, and the abort response follows it. Deterministic: each point is held open by
/// the test until the abort has been read.
#[test]
fn abort_at_each_point_before_the_first_byte_settles_aborted() {
    for point in 1..=3 {
        let env = TestEnv::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let blocker = |at| Blocker {
            at,
            entered: Mutex::new(Some(entered_tx.clone())),
            release: Mutex::new(Some(release_rx)),
        };
        let mut hooks: Vec<Arc<dyn Hooks>> = Vec::new();
        let mut engine_entered = None;
        match point {
            1 => hooks.push(Arc::new(CommandsBlocker(blocker(Hold::BeforeClaim)))),
            2 => hooks.push(Arc::new(blocker(Hold::AfterClaim))),
            _ => engine_entered = Some(entered_tx.clone()),
        }
        let engine = Arc::new(Probe {
            inner: HoldUntilCancel(ScriptedEngine::new(vec![ScriptTurn::text("never")])),
            entered: Mutex::new(engine_entered),
        });
        let session = open_with(&env, engine, hooks);
        let s = Served::start(session, &env, Box::new(|_| Vec::new()));
        if point == 1 {
            // The server is up (its start-up snapshot is taken) before the hold is armed.
            s.send(json!({"id": "up", "type": "get_state"}));
            s.wait("the server", |l| response(l, "up").is_some());
            *ARMED.lock().unwrap() = true;
            s.send(json!({"id": "gc", "type": "get_commands"}));
            entered_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("held in get_commands");
            s.send(json!({"id": "p", "type": "prompt", "message": "go"}));
        } else {
            s.send(json!({"id": "p", "type": "prompt", "message": "go"}));
            entered_rx.recv_timeout(Duration::from_secs(10)).expect("held");
        }
        s.wait("the prompt response", |l| response(l, "p").is_some());
        s.send(json!({"id": "a", "type": "abort"}));
        // The reader handles lines in order: once this get_state (answered by the reader, a
        // run is claimed) is answered, the abort has been handled. Then the hold opens.
        s.send(json!({"id": "st", "type": "get_state"}));
        s.wait("the get_state answer", |l| response(l, "st").is_some());
        let _ = release_tx.send(());
        s.wait("the abort response", |l| response(l, "a").is_some());
        let lines = s.close(10, 130);
        let started = response(&lines, "p").expect("prompt response");
        assert_eq!(
            started["data"]["disposition"], "started",
            "point {point}: {started}"
        );
        let settled: Vec<&Value> = lines.iter().filter(|v| v["type"] == "agent_settled").collect();
        assert_eq!(settled.len(), 1, "point {point}: {lines:?}");
        assert_eq!(settled[0]["aborted"], true, "point {point}: {lines:?}");
        let at_settled = lines.iter().position(|v| v["type"] == "agent_settled").unwrap();
        let at_abort = lines
            .iter()
            .position(|v| v["type"] == "response" && v["id"] == "a")
            .unwrap();
        assert!(at_settled < at_abort, "point {point}: {lines:?}");
    }
}

#[test]
fn inbox_wakes_an_idle_rpc_session_without_stdin_input() {
    let env = TestEnv::new();
    let session = open_session(&env, vec![ScriptTurn::text("received")], Vec::new());
    let id = session.state().session_id;
    let inbox = Dirs::under(env.root()).agent_dir.join("inbox").join(id);
    std::fs::create_dir_all(&inbox).unwrap();
    // Ready before the watcher starts: startup must scan as well as subscribe.
    std::fs::write(inbox.join("001.msg"), "task finished, red").unwrap();
    let served = Served::start(session, &env, Box::new(|_| Vec::new()));
    let start = Instant::now();
    while !served.lines().iter().any(|v| v["type"] == "agent_settled")
        && start.elapsed() < Duration::from_secs(3)
    {
        thread::sleep(Duration::from_millis(5));
    }
    let lines = served.close(3, 0);
    assert!(
        lines.iter().any(|v| v["type"] == "agent_settled"),
        "inbox did not wake RPC: {lines:?}"
    );
    assert!(
        !inbox.join("001.msg").exists(),
        "delivered message must be consumed"
    );
}

#[test]
fn inbox_tracks_rpc_session_switches_and_leaves_the_old_inbox_alone() {
    let env = TestEnv::new();
    let session = open_session(&env, vec![ScriptTurn::text("new session received")], Vec::new());
    let old_id = session.state().session_id;
    let served = Served::start(session, &env, Box::new(|_| Vec::new()));
    served.send(json!({"type": "new_session", "id": "new"}));
    served.wait("new session", |lines| response(lines, "new").is_some());
    served.send(json!({"type": "get_state", "id": "state"}));
    served.wait("new state", |lines| response(lines, "state").is_some());
    let lines = served.lines();
    let new_id = response(&lines, "state").unwrap()["data"]["sessionId"]
        .as_str()
        .unwrap();
    assert_ne!(old_id, new_id);
    let agent = Dirs::under(env.root()).agent_dir;
    oxa_session::send_inbox(&agent, &old_id, "for the previous session").unwrap();
    oxa_session::send_inbox(&agent, new_id, "for the active session").unwrap();
    served.wait("new session inbox run", |lines| {
        lines.iter().any(|v| v["type"] == "agent_settled")
    });
    served.close(3, 0);
    assert_eq!(
        std::fs::read_dir(agent.join("inbox").join(old_id))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_dir(agent.join("inbox").join(new_id))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn inbox_arriving_during_session_switch_stays_with_its_destination() {
    struct DuringSwitch {
        agent: PathBuf,
        old_id: Mutex<String>,
    }
    impl Hooks for DuringSwitch {
        fn fire(&self, point: HookPoint, _: &Value, _: &mut dyn Ui) -> HookReply {
            if point == HookPoint::SessionBeforeSwitch {
                let old_id = self.old_id.lock().unwrap();
                oxa_session::send_inbox(&self.agent, &old_id, "old session only").unwrap();
                // Give the live watcher time to see the file while the switch is in progress.
                let deadline = Instant::now() + Duration::from_millis(300);
                while Instant::now() < deadline {
                    if std::fs::read_dir(self.agent.join("inbox").join(&*old_id))
                        .unwrap()
                        .count()
                        == 0
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            HookReply::Continue
        }
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            Vec::new()
        }
        fn commands(&self) -> Vec<CommandSpec> {
            Vec::new()
        }
        fn run_command(&self, _: &str, _: &str, _: &mut dyn Ui) -> Result<(), OxaError> {
            Ok(())
        }
        fn prompt_sections(&self) -> Vec<(String, String)> {
            Vec::new()
        }
    }
    let env = TestEnv::new();
    let agent = Dirs::under(env.root()).agent_dir;
    let hook = Arc::new(DuringSwitch {
        agent: agent.clone(),
        old_id: Mutex::new(String::new()),
    });
    let session = open_session(&env, vec![ScriptTurn::text("new reply")], vec![hook.clone()]);
    let old_id = session.state().session_id;
    *hook.old_id.lock().unwrap() = old_id.clone();
    let served = Served::start(session, &env, Box::new(|_| Vec::new()));
    served.send(json!({"type": "new_session", "id": "new"}));
    served.wait("switch response", |lines| response(lines, "new").is_some());
    served.send(json!({"type": "get_state", "id": "state"}));
    served.wait("new state", |lines| response(lines, "state").is_some());
    served.send(json!({"type": "prompt", "id": "prompt", "message": "new session only"}));
    served.wait("new prompt settles", |lines| {
        lines.iter().any(|v| v["type"] == "agent_settled")
    });
    served.send(json!({"type": "get_messages", "id": "messages"}));
    served.wait("messages", |lines| response(lines, "messages").is_some());
    let lines = served.close(3, 0);
    assert!(!response(&lines, "messages")
        .unwrap()
        .to_string()
        .contains("old session only"));
    assert_eq!(
        std::fs::read_dir(agent.join("inbox").join(old_id))
            .unwrap()
            .count(),
        1,
        "a switch must not consume the old session's pending text"
    );
}

#[test]
fn inbox_rebinding_rearms_admitted_messages_without_files() {
    let env = TestEnv::new();
    let session = open_session(&env, vec![ScriptTurn::text("received")], Vec::new());
    let handle = session.handle();
    handle.follow_up_for(&session.state().session_id, "already admitted".into());
    let served = Served::start(session, &env, Box::new(|_| Vec::new()));
    served.wait("admitted inbox run", |lines| {
        lines.iter().any(|v| v["type"] == "agent_settled")
    });
    served.close(3, 0);
    assert!(!handle.has_inbox_input());
}

fn write_oversized_fixture(dir: &Path, during_write: impl FnOnce()) {
    let pending = dir.join("oversized.pending");
    let mut file = std::fs::File::create(&pending).unwrap();
    during_write();
    file.write_all(&vec![b'x'; 256 * 1024 + 1]).unwrap();
    drop(file);
    std::fs::rename(pending, dir.join("oversized.msg")).unwrap();
}

#[test]
fn oversized_fixture_is_not_published_until_its_payload_is_complete() {
    let env = TestEnv::new();
    let agent = Dirs::under(env.root()).agent_dir;
    let id = "fixture-publication";
    let dir = agent.join("inbox").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    // Both paths exist before startup scanning; this sorts after the fixture.
    std::fs::write(dir.join("zz-scan.msg"), "scan barrier").unwrap();
    write_oversized_fixture(&dir, || {
        let (tx, rx) = mpsc::channel();
        let watch = oxa_session::Inbox::watch(&agent, id, move |text| tx.send(text).is_ok()).unwrap();
        let delivered = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(watch);
        assert_eq!(
            delivered, "scan barrier",
            "startup scan consumed an incomplete fixture"
        );
        assert!(
            rx.try_recv().is_err(),
            "only the complete barrier may be delivered"
        );
    });
    assert_eq!(
        std::fs::metadata(dir.join("oversized.msg")).unwrap().len(),
        256 * 1024 + 1
    );
}

#[test]
fn inbox_backpressure_retains_excess_files_while_model_is_held_then_drains() {
    struct HeldFirst {
        inner: ScriptedEngine,
        hold: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    }
    impl Engine for HeldFirst {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            self.inner.info(target)
        }
        fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
            self.inner.count_tokens(text)
        }
        fn chat(
            &self,
            request: &ChatRequest,
            cancel: &CancelToken,
            on: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            if let Some((entered, release)) = self.hold.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            self.inner.chat(request, cancel, on)
        }
    }
    let env = TestEnv::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let engine = Arc::new(HeldFirst {
        inner: ScriptedEngine::new((0..100).map(|_| ScriptTurn::text("reply")).collect()),
        hold: Mutex::new(Some((entered_tx, release_rx))),
    });
    let session = open_with(&env, engine, Vec::new());
    let id = session.state().session_id;
    let handle = session.handle();
    let served = Served::start(session, &env, Box::new(|_| Vec::new()));
    served.send(json!({"type": "prompt", "id": "held", "message": "hold model"}));
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let agent = Dirs::under(env.root()).agent_dir;
    let dir = agent.join("inbox").join(&id);
    // An external writer can bypass send's size validation: still never read it all.
    write_oversized_fixture(&dir, || {});
    for n in 0..70 {
        oxa_session::send_inbox(&agent, &id, &format!("backlog {n}")).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while handle.queued().1.len() < 64 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(handle.queued().1.len(), 64);
    // A barrier file after the excess backlog confirms that a scan encountered pressure.
    oxa_session::send_inbox(&agent, &id, "barrier").unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        handle.queued().1.len(),
        64,
        "no unbounded admissions during a held model"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 8);
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_dir(&dir).unwrap().count() != 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "capacity release must retry disk backlog"
    );
    served.wait("backlog drained", |_| {
        !handle.has_inbox_input() && !handle.is_busy()
    });
    served.send(json!({"type": "get_messages", "id": "messages"}));
    served.wait("messages", |lines| response(lines, "messages").is_some());
    let lines = served.close(3, 0);
    let messages = response(&lines, "messages").unwrap().to_string();
    for n in 0..70 {
        assert!(messages.contains(&format!("backlog {n}")), "missing backlog {n}");
    }
    assert!(messages.contains("barrier"));
    assert!(dir.join("oversized.msg").exists());
}
