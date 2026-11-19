//! B3 acceptance tests: Agent loop.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{append_entry, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, miss_cause, shown_why, tool_errors, Access, AgentEvent, AssistantTurn, CancelToken, ChatRequest,
    CommandSpec, Engine, EngineInfo, Entry, EntryBody, Exposure, Finish, FrozenPrefix, HeadlessUi, HookPoint,
    HookReply, Hooks, Mode, OxaError, Policy, Source, StreamEvent, ThinkingLevel, Tool, ToolCall, ToolClass,
    ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionHeader, SessionMeta, SessionRef, SessionStore, Transcript, Tree};
use oxa_tools::ReadTool;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// Harness

/// A tool whose access, body and recorded arguments the test controls.
type AccessFn = Box<dyn Fn(&Value, &ToolCtx) -> Result<Vec<Access>, String> + Send + Sync>;
type BodyFn = Box<dyn Fn(&Value, &ToolCtx) -> ToolOutput + Send + Sync>;

struct Scripted {
    spec: ToolSpec,
    access: AccessFn,
    body: BodyFn,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Tool for Scripted {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        (self.access)(args, cx)
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        self.seen.lock().unwrap().push(args.clone());
        (self.body)(args, cx)
    }
}

fn spec(name: &str, class: ToolClass) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: "test tool".to_string(),
        schema_json: "{}".to_string(),
        class,
        exposure: Exposure::Direct,
        source: Source::Builtin,
    }
}

fn no_access() -> AccessFn {
    Box::new(|_args: &Value, _cx: &ToolCtx| Ok(Vec::new()))
}

fn tool_with(
    name: &str,
    class: ToolClass,
    access: AccessFn,
    body: BodyFn,
) -> (Arc<dyn Tool>, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let t: Arc<dyn Tool> = Arc::new(Scripted {
        spec: spec(name, class),
        access,
        body,
        seen: Arc::clone(&seen),
    });
    (t, seen)
}

/// An echo of argument `tag`, read class, no access.
fn echo(name: &str) -> (Arc<dyn Tool>, Arc<Mutex<Vec<Value>>>) {
    tool_with(
        name,
        ToolClass::Read,
        no_access(),
        Box::new(|a: &Value, _c: &ToolCtx| ToolOutput::ok(a["tag"].as_str().unwrap_or("").to_string())),
    )
}

fn build(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    store: Arc<dyn SessionStore>,
    tools: Vec<Arc<dyn Tool>>,
    mode: Mode,
    hooks: Vec<Arc<dyn Hooks>>,
) -> SessionState {
    let cwd = env.work();
    let sref = store.create(&cwd, None, None).expect("create session");
    let info = engine.info(None).expect("engine info");
    let sections = vec![
        ("preamble".to_string(), "You are a test agent.".to_string()),
        ("cwd".to_string(), cwd.display().to_string()),
    ];
    let mut prefix = FrozenPrefix {
        sections,
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    };
    prefix.sha256 = Sha256::digest(prefix.hash_input()).into();
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        cancel: CancelToken::new(),
        cancel_retry: CancelToken::new(),
        wait_wake: CancelToken::new(),
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
        wake: Default::default(),
    });
    SessionState {
        goal: None,
        output_schema: None,
        engine,
        store,
        hooks,
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings: Settings::default(),
        dirs: Dirs::under(env.root()),
        cwd: cwd.clone(),
        all_tools: tools.clone(),
        selection: Default::default(),
        active: Toolset {
            direct: tools,
            indirect: Vec::new(),
        },
        resources: Resources::default(),
        sref,
        tree: Tree::new(Vec::new()),
        transcript: Transcript::default(),
        prefix,
        info,
        model: "fake-model".to_string(),
        thinking: ThinkingLevel::Off,
        mode,
        compact: CompactPolicy {
            soft: u32::MAX,
            hard: u32::MAX,
            keep_tokens: 4000,
            summary_max: 1024,
            enabled: true,
        },
        shared,
        next_turn: Vec::new(),
        last_usage: None,
        stats: Stats::default(),
        spill_dir: env.tmp(),
        tool_env: Vec::new(),
        tune: Default::default(),
        auto_compaction: true,
        auto_retry: true,
        host_rx: None,
        host_deferred: Vec::new(),
        compact_memo: Default::default(),
        jobs: oxa_core::fresh_jobs(),
    }
}

/// A session over a fresh scripted engine. Returns the state and the engine for inspection.
fn scripted_state(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    tools: Vec<Arc<dyn Tool>>,
    mode: Mode,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    (build(env, engine, store, tools, mode, Vec::new()), eng)
}

/// Append the user message the way `Session::prompt` does.
fn say(st: &mut SessionState, text: &str) {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("append user");
}

fn run(st: &mut SessionState) -> (RunEnd, Vec<AgentEvent>) {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let end = run_loop(st, &mut sink, &mut ui).expect("run_loop");
    (end, sink.0)
}

/// W1 on the recorded requests, by the invariant checker (no rebase in these tests).
fn w1_holds(reqs: &[Recorded]) {
    let broken = oxa_fake::check_w1(reqs, &[]);
    assert!(broken.is_empty(), "W1 violations: {broken:?}");
}

fn turn_calls(calls: Vec<(&str, &str, &str)>) -> ScriptTurn {
    ScriptTurn {
        reasoning: None,
        text: None,
        tool_calls: calls
            .into_iter()
            .map(|(id, name, args)| ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                args_raw: args.to_string(),
            })
            .collect(),
        finish: Finish::ToolCalls,
        completion_tokens: None,
        chunk: 0,
        fault: None,
    }
}

fn tool_text(msg: &WireMsg) -> String {
    msg.content.clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Engine wrappers and stores used by individual tests

/// Cancels the run as soon as the first stream increment has been delivered.
struct CancelMidStream {
    inner: Arc<ScriptedEngine>,
    token: CancelToken,
}

impl Engine for CancelMidStream {
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
        let token = self.token.clone();
        let mut fired = false;
        let mut wrapped = |ev: StreamEvent| {
            on(ev);
            if !fired {
                fired = true;
                token.cancel();
            }
        };
        self.inner.chat(r, cancel, &mut wrapped)
    }
}

/// Reports `cached = 0` for the second request, which is a cache miss by the engine's count.
struct MissOnSecond {
    inner: Arc<ScriptedEngine>,
    calls: AtomicUsize,
}

impl Engine for MissOnSecond {
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
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let mut turn = self.inner.chat(r, cancel, on)?;
        if n == 2 {
            turn.usage.cached = 0;
        }
        Ok(turn)
    }
}

/// A store whose appends always fail; everything else delegates.
struct FailAppend {
    inner: Arc<MemStore>,
}

impl SessionStore for FailAppend {
    fn create(&self, cwd: &Path, id: Option<&str>, parent: Option<&Path>) -> Result<SessionRef, OxaError> {
        self.inner.create(cwd, id, parent)
    }
    fn append(&self, _s: &SessionRef, _e: &Entry) -> Result<(), OxaError> {
        Err(OxaError::new(codes::IO, "disk full"))
    }
    fn load(&self, s: &SessionRef) -> Result<Vec<Entry>, OxaError> {
        self.inner.load(s)
    }
    fn header(&self, s: &SessionRef) -> Result<SessionHeader, OxaError> {
        self.inner.header(s)
    }
    fn list(&self, cwd: Option<&Path>) -> Vec<SessionMeta> {
        self.inner.list(cwd)
    }
    fn resolve(&self, id_or_path: &str) -> Result<SessionRef, OxaError> {
        self.inner.resolve(id_or_path)
    }
    fn delete(&self, s: &SessionRef) -> Result<(), OxaError> {
        self.inner.delete(s)
    }
}

/// Returns one reply for one hook point (once), `Continue` everywhere else.
struct OneShot {
    point: HookPoint,
    reply: Mutex<Option<HookReply>>,
}

impl Hooks for OneShot {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p == self.point {
            self.reply.lock().unwrap().take().unwrap_or(HookReply::Continue)
        } else {
            HookReply::Continue
        }
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

/// Rewrites the arguments of `echo` calls and blocks calls to `blocked`.
struct ToolHook;

impl Hooks for ToolHook {
    fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p != HookPoint::ToolCall {
            return HookReply::Continue;
        }
        match payload["toolName"].as_str() {
            Some("echo") => HookReply::ReplaceArgs(r#"{"tag":"replaced"}"#.to_string()),
            Some("blocked") => HookReply::Block("blocked by hook".to_string()),
            _ => HookReply::Continue,
        }
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

// ---------------------------------------------------------------------------------------------
// Acceptance tests

/// read call -> result -> final text; wire checked with oxa_fake::check_w1
#[test]
fn tool_loop_scripted() {
    let env = TestEnv::new();
    env.write("work/a.txt", "hello\n");
    let turns = vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "a.txt"}"#),
        ScriptTurn::text("Done.\n"),
    ];
    let (mut st, eng) = scripted_state(
        &env,
        turns,
        vec![Arc::new(ReadTool::new()) as Arc<dyn Tool>],
        Mode::Workspace,
    );
    say(&mut st, "read a.txt");

    let (end, events) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);

    let reqs = eng.requests();
    assert_eq!(reqs.len(), 2, "one request per reply");
    w1_holds(&reqs);
    assert!(
        reqs[1].messages.iter().any(|m| tool_text(m).contains("hello")),
        "read result reaches the second request"
    );

    let wire = st.transcript.wire();
    assert_eq!(
        wire.len(),
        4,
        "user, assistant with the call, tool result, final text"
    );
    assert_eq!(
        wire[3].content.as_deref(),
        Some("Done.\n"),
        "final text is untrimmed (W2)"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. })));
    assert!(matches!(events.first(), Some(AgentEvent::AgentStart)));
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

/// policy Deny and user Deny produce the fixed texts and the loop continues
#[test]
fn deny_path() {
    let env = TestEnv::new();
    let runs = Arc::new(AtomicUsize::new(0));

    // Plan mode: the table denies writes outright.
    let r1 = Arc::clone(&runs);
    let writer = tool_with(
        "writer",
        ToolClass::Write,
        Box::new(|_a: &Value, cx: &ToolCtx| Ok(vec![Access::Write(cx.cwd.join("out.txt"))])),
        Box::new(move |_a: &Value, _c: &ToolCtx| {
            r1.fetch_add(1, Ordering::SeqCst);
            ToolOutput::ok("written")
        }),
    )
    .0;
    let turns = vec![
        turn_calls(vec![("w1", "writer", "{}")]),
        ScriptTurn::text("understood"),
    ];
    let (mut st, eng) = scripted_state(&env, turns, vec![writer.clone()], Mode::Plan);
    say(&mut st, "write");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped, "a denial does not stop the loop");
    let denied = tool_text(&st.transcript.wire()[2]);
    assert_eq!(
        denied,
        format!(
            "{}{}",
            tool_errors::DENIED_PREFIX,
            oxa_perm::deny_reasons::PLAN_WRITE
        )
    );
    assert_eq!(eng.requests().len(), 2);
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a denied tool never runs");

    // Ask mode asks even for writes inside cwd; headless UI answers Deny.
    let (mut st2, eng2) = scripted_state(
        &env,
        vec![turn_calls(vec![("w2", "writer", "{}")]), ScriptTurn::text("ok")],
        vec![writer],
        Mode::Ask,
    );
    say(&mut st2, "write again");
    let (end2, _) = run(&mut st2);
    assert_eq!(end2, RunEnd::Stopped);
    // #554: nobody was asked, so the text must not claim that "the user declined"; it names the
    // action and says why it was refused.
    let unasked = tool_text(&st2.transcript.wire()[2]);
    assert_ne!(unasked, tool_errors::DENIED_BY_USER);
    assert_eq!(
        unasked,
        format!(
            "Permission denied: write {} needs approval, but this run cannot ask (non-interactive), so it was refused; nobody declined it.",
            st2.cwd.join("out.txt").display()
        ),
        "Ask mode must not promise approval-free writes or bash commands"
    );
    assert_eq!(eng2.requests().len(), 2);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

/// Shown entry stored, wire unchanged (W2)
#[test]
fn abort_mid_stream_nothing_on_wire() {
    let env = TestEnv::new();
    let mut partial = ScriptTurn::text("partial reply");
    partial.chunk = 4;
    let eng = Arc::new(ScriptedEngine::new(vec![partial]));
    let placeholder: Arc<dyn Engine> = eng.clone();
    let mut st = build(
        &env,
        placeholder,
        Arc::new(MemStore::new()),
        Vec::new(),
        Mode::Workspace,
        Vec::new(),
    );
    // The wrapper cancels the state's own token, the one the loop checks.
    st.engine = Arc::new(CancelMidStream {
        inner: Arc::clone(&eng),
        token: st.shared.cancel.clone(),
    });
    say(&mut st, "hello");

    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Aborted);
    assert_eq!(
        st.transcript.wire().len(),
        1,
        "nothing but the user message is on the wire"
    );
    let last = st.tree.entries().last().expect("an entry was stored");
    match &last.body {
        EntryBody::Shown { text, why } => {
            assert_eq!(*why, shown_why::ABORTED);
            assert_eq!(text, "partial reply");
        }
        other => panic!("expected Shown, got {other:?}"),
    }
    assert!(!st
        .transcript
        .wire()
        .iter()
        .any(|m| m.role == oxa_proto::Role::Assistant));
}

/// #456: a stream cut at byte N after output is replayed (partial dropped from the wire, kept
/// as `Shown { cut }`) and the run completes with the full reply.
#[test]
fn stream_cut_mid_output_is_replayed_and_completes() {
    let env = TestEnv::new();
    let mut cut = ScriptTurn::text("a longer reply that is cut");
    cut.chunk = 1;
    cut.fault = Some(oxa_fake::Fault::DropAt { bytes: 5 });
    let eng = Arc::new(ScriptedEngine::new(vec![cut, ScriptTurn::text("full reply")]));
    let mut st = build(
        &env,
        eng.clone(),
        Arc::new(MemStore::new()),
        Vec::new(),
        Mode::Workspace,
        Vec::new(),
    );
    say(&mut st, "hello");

    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 2, "replayed once");
    let wire = st.transcript.wire();
    assert_eq!(wire.len(), 2);
    assert_eq!(wire[1].content.as_deref(), Some("full reply"));
    assert!(st
        .tree
        .entries()
        .iter()
        .any(|e| matches!(&e.body, EntryBody::Shown { why, .. } if *why == shown_why::CUT)));
}

/// #877: a replayed request is still the same foreground run. The cut reply is persisted as
/// `Shown { cut }`, which ends the foreground signal; the run must re-arm it before it resends,
/// or input typed during the replay is taken for a new prompt.
#[test]
fn a_replayed_request_is_still_foreground_work() {
    let env = TestEnv::new();
    let mut cut = ScriptTurn::text("a longer reply that is cut");
    cut.chunk = 1;
    cut.fault = Some(oxa_fake::Fault::DropAt { bytes: 5 });
    let eng = Arc::new(ScriptedEngine::new(vec![cut, ScriptTurn::text("full reply")]));
    let mut st = build(
        &env,
        eng.clone(),
        Arc::new(MemStore::new()),
        Vec::new(),
        Mode::Workspace,
        Vec::new(),
    );
    say(&mut st, "hello");
    /// Records the foreground signal as each request's message opens.
    struct RunningAtStart(Box<dyn Fn() -> bool>, Arc<Mutex<Vec<bool>>>);
    impl oxa_proto::EventSink for RunningAtStart {
        fn emit(&mut self, event: &AgentEvent) {
            if matches!(event, AgentEvent::MessageStart { .. }) {
                self.1.lock().unwrap().push((self.0)());
            }
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let running = {
        let shared = st.shared.clone();
        Box::new(move || shared.gate.running.load(Ordering::SeqCst)) as Box<dyn Fn() -> bool>
    };
    let mut sink = RunningAtStart(running, seen.clone());
    let mut ui = HeadlessUi::default();
    let end = run_loop(&mut st, &mut sink, &mut ui).expect("run_loop");
    assert_eq!(end, RunEnd::Stopped);
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 2,
        "the cut request and its replay each open a message: {seen:?}"
    );
    assert!(
        seen.iter().all(|running| *running),
        "no request of the run opens while the session reads as idle: {seen:?}"
    );
}

/// results pushed in call order
#[test]
fn parallel_reads_in_call_order() {
    let env = TestEnv::new();
    // The first call is slow, the second fast: they run in parallel, results stay in call order.
    let slow = tool_with(
        "slow",
        ToolClass::Read,
        no_access(),
        Box::new(|a: &Value, _c: &ToolCtx| {
            std::thread::sleep(Duration::from_millis(a["ms"].as_u64().unwrap_or(0)));
            ToolOutput::ok(a["tag"].as_str().unwrap_or("").to_string())
        }),
    )
    .0;
    let turns = vec![
        turn_calls(vec![
            ("call_a", "slow", r#"{"tag":"first","ms":200}"#),
            ("call_b", "slow", r#"{"tag":"second","ms":0}"#),
        ]),
        ScriptTurn::text("done"),
    ];
    let (mut st, eng) = scripted_state(&env, turns, vec![slow], Mode::Workspace);
    say(&mut st, "go");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);

    let wire = st.transcript.wire();
    assert_eq!(wire[2].tool_call_id.as_deref(), Some("call_a"));
    assert_eq!(tool_text(&wire[2]), "first");
    assert_eq!(wire[3].tool_call_id.as_deref(), Some("call_b"));
    assert_eq!(tool_text(&wire[3]), "second");
    w1_holds(&eng.requests());
}

/// A local length cut continues after an append-only instruction and an advertised prefill.
#[test]
fn length_continue_once() {
    let env = TestEnv::new();
    let mut first = ScriptTurn::text("part one");
    first.finish = Finish::Length;
    let turns = vec![first, ScriptTurn::text("completed task")];
    let (mut st, eng) = scripted_state(&env, turns, Vec::new(), Mode::Workspace);
    say(&mut st, "write a lot");
    let (end, events) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 3, "chat, prefill, continuation");
    assert_eq!(eng.remaining(), 0);
    let wire = st.transcript.wire();
    assert_eq!(wire[1].content.as_deref(), Some("part one"));
    assert_eq!(wire.last().unwrap().content.as_deref(), Some("completed task"));
    assert!(!st.stats.output_truncated);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::MessageEnd { message } if message.stop == Some(oxa_proto::StopReason::Length))));
    for r in eng.requests() {
        assert!(
            !(r.max_tokens > 0 && r.messages.last().map(|m| m.role) == Some(oxa_proto::Role::Assistant)),
            "a generating request needs an explicit continuation instruction"
        );
    }
    w1_holds(&eng.requests());
}

/// A completed cloud reply cut at the limit is kept as progress and regenerated off-wire.
#[test]
fn remote_length_recovery_drops_the_cut_reply() {
    let env = TestEnv::new();
    let mut cut = ScriptTurn::text("UNFINISHED_PARTIAL");
    cut.finish = Finish::Length;
    let eng = Arc::new(ScriptedEngine::new(vec![cut, ScriptTurn::text("complete answer")]).with_remote());
    let mut st = build(
        &env,
        eng.clone(),
        Arc::new(MemStore::new()),
        vec![],
        Mode::Workspace,
        vec![],
    );
    say(&mut st, "complete the task");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    let requests = eng.requests();
    assert_eq!(requests.len(), 2, "cloud recovery sends no local prefill");
    assert_ne!(
        requests[0].idem_key, requests[1].idem_key,
        "a completed cut response needs a fresh request"
    );
    assert!(!requests[1]
        .messages
        .iter()
        .any(|m| m.content.as_deref() == Some("UNFINISHED_PARTIAL")));
    assert!(!st.stats.output_truncated);
    assert!(st.tree.entries().iter().any(|e| matches!(&e.body, EntryBody::Shown { text, why } if text == "UNFINISHED_PARTIAL" && *why == shown_why::CUT)));
}

/// The existing byte-N replay and new length recovery complete one run together.
#[test]
fn a_stream_cut_then_length_cut_still_completes() {
    for remote in [false, true] {
        let env = TestEnv::new();
        let mut broken = ScriptTurn::text("stream fragment");
        broken.chunk = 1;
        broken.fault = Some(oxa_fake::Fault::DropAt { bytes: 5 });
        let mut cut = ScriptTurn::text("length fragment");
        cut.finish = Finish::Length;
        let scripted = ScriptedEngine::new(vec![broken, cut, ScriptTurn::text("complete answer")]);
        let eng = Arc::new(if remote { scripted.with_remote() } else { scripted });
        let mut st = build(
            &env,
            eng.clone(),
            Arc::new(MemStore::new()),
            vec![],
            Mode::Workspace,
            vec![],
        );
        say(&mut st, "finish the task");
        let (end, events) = run(&mut st);
        assert_eq!(end, RunEnd::Stopped);
        let requests = eng.requests();
        assert_eq!(requests.len(), if remote { 3 } else { 4 });
        assert_eq!(
            requests[0].idem_key, requests[1].idem_key,
            "transport replay keeps its key"
        );
        assert_ne!(requests[1].idem_key, requests.last().unwrap().idem_key);
        assert_eq!(
            st.transcript.wire().last().unwrap().content.as_deref(),
            Some("complete answer")
        );
        assert!(!st.stats.output_truncated);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::AgentEnd { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn length_recovery_skips_unavailable_or_disabled_prefill() {
    for advertised in [false, true] {
        let env = TestEnv::new();
        let mut cut = ScriptTurn::text("part one");
        cut.finish = Finish::Length;
        let server = oxa_fake::FakeServer::start(
            vec![cut, ScriptTurn::text("done")],
            oxa_fake::FakeOpts {
                hints: advertised,
                ..oxa_fake::FakeOpts::default()
            },
        )
        .unwrap();
        let engine = Arc::new(oxa_engine::HttpEngine::new(&server.url(), None).unwrap());
        let mut st = build(
            &env,
            engine,
            Arc::new(MemStore::new()),
            vec![],
            Mode::Workspace,
            vec![],
        );
        assert_eq!(st.info.prefill_only, advertised);
        st.settings = Settings::from_values(
            serde_json::json!({"cacheWarming":if advertised { "off" } else { "idle" }}),
            serde_json::json!({}),
        );
        say(&mut st, "finish the task");
        let (end, _) = run(&mut st);
        assert_eq!(end, RunEnd::Stopped);
        assert_eq!(server.requests().len(), 2);
        assert!(server.requests().iter().all(|r| r.max_tokens > 0));
        assert!(!st.stats.output_truncated);
    }
}

#[test]
fn disabled_length_recovery_keeps_the_cut_outcome() {
    for setting in ["disabled", "zero", "runtime"] {
        let env = TestEnv::new();
        let mut cut = ScriptTurn::text("part one");
        cut.finish = Finish::Length;
        let (mut st, eng) = scripted_state(
            &env,
            vec![cut, ScriptTurn::text("not requested")],
            vec![],
            Mode::Workspace,
        );
        st.settings = Settings::from_values(
            serde_json::json!({"retry":{"enabled":setting!="disabled","maxRetries":if setting=="zero" {0} else {2}}}),
            serde_json::json!({}),
        );
        st.auto_retry = setting != "runtime";
        say(&mut st, "finish the task");
        let (end, _) = run(&mut st);
        assert_eq!(end, RunEnd::Stopped);
        assert_eq!(eng.requests().len(), 1);
        assert!(st.stats.output_truncated);
    }
}

/// A refusal (`content_filter`) keeps the streamed text, shows a one-line notice and does not retry.
#[test]
fn content_filter_keeps_text_notices_and_does_not_retry() {
    let env = TestEnv::new();
    let mut first = ScriptTurn::text("I can't help with that");
    first.finish = Finish::ContentFilter;
    let turns = vec![first, ScriptTurn::text("never requested")];
    let (mut st, eng) = scripted_state(&env, turns, Vec::new(), Mode::Workspace);
    say(&mut st, "do the thing");
    let (end, events) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 1, "no retry");
    let wire = st.transcript.wire();
    assert_eq!(
        wire.last().unwrap().content.as_deref(),
        Some("I can't help with that")
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { text, .. } if text == "The model declined to answer this request.")));
}

/// F9 (engine `chat::chat_inputs`): a generating request ending in an assistant message is
/// history plus a NEW assistant turn, on the stub engine as on the real one; the trailing
/// message is held like any earlier turn (cached = prompt - 1, the `\n` after the trailing
/// message is the one new byte). An H6 prefill of the same history is accepted as before.
#[test]
fn scripted_engine_answers_trailing_assistant_like_the_real_engine() {
    let eng = ScriptedEngine::new(vec![ScriptTurn::text("cut"), ScriptTurn::text("x")]);
    let prefix = FrozenPrefix {
        sections: vec![("p".to_string(), "s".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    };
    let first = vec![WireMsg::user("hi")];
    let msgs = vec![WireMsg::user("hi"), WireMsg::assistant("cut")];
    let hints = oxa_proto::Hints {
        session: "s",
        idem_key: "",
        kind: oxa_proto::TurnKind::Chat,
        background: false,
        snapshot_keep: false,
    };
    fn req<'a>(
        prefix: &'a FrozenPrefix,
        messages: &'a [WireMsg],
        hints: oxa_proto::Hints<'a>,
        max_tokens: u32,
    ) -> ChatRequest<'a> {
        ChatRequest {
            output_schema: None,
            prefix,
            messages,
            model: "fake-model",
            max_tokens,
            thinking: false,
            thinking_budget: None,
            thinking_level: None,
            preserve_thinking: true,
            sampling: None,
            hints,
        }
    }
    eng.chat(
        &req(&prefix, &first, hints, 4096),
        &CancelToken::new(),
        &mut |_| {},
    )
    .expect("turn 1");
    let t = eng
        .chat(
            &req(&prefix, &msgs, hints, 4096),
            &CancelToken::new(),
            &mut |_| {},
        )
        .expect("a trailing assistant message is history, not a 400");
    assert_eq!(t.msg.content.as_deref(), Some("x"), "a new turn");
    assert_eq!(t.usage.cached, t.usage.prompt - 1);
    assert_eq!(eng.remaining(), 0);
    eng.chat(&req(&prefix, &msgs, hints, 0), &CancelToken::new(), &mut |_| {})
        .expect("H6 prefill of a finished history");
}

/// X2, pi parity: tool calls of a reply cut by the output limit are not run; each gets the
/// fixed truncation error and the loop goes on (the wire then ends in tool messages, which the
/// engine accepts).
#[test]
fn length_stop_with_tool_calls_fails_them_and_continues() {
    let env = TestEnv::new();
    let (echo_tool, seen) = echo("read_like");
    let mut cut = turn_calls(vec![
        ("c1", "read_like", r#"{"tag":"a"}"#),
        ("c2", "read_like", r#"{"tag":"#),
    ]);
    cut.finish = Finish::Length;
    let (mut st, eng) = scripted_state(
        &env,
        vec![cut, ScriptTurn::text("retried")],
        vec![echo_tool],
        Mode::Workspace,
    );
    say(&mut st, "go");
    let (end, events) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    assert!(
        seen.lock().unwrap().is_empty(),
        "no call of a truncated reply runs"
    );
    let wire = st.transcript.wire();
    assert_eq!(wire.len(), 5, "user, cut reply, two tool errors, final reply");
    for (i, name) in [(2, "read_like"), (3, "read_like")] {
        assert_eq!(
            tool_text(&wire[i]),
            tool_errors::TRUNCATED.replace("{name}", name)
        );
    }
    let ends: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { result, .. } => Some(result.is_error),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![true, true]);
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].messages.last().unwrap().role, oxa_proto::Role::Tool);
    w1_holds(&reqs);
}

/// Length recovery uses supported HTTP requests, without implicit assistant prefill.
#[test]
fn length_stop_over_http_sends_no_unsupported_request() {
    let env = TestEnv::new();
    let mut cut = ScriptTurn::text("part one");
    cut.finish = Finish::Length;
    let server = oxa_fake::FakeServer::start(
        vec![
            cut,
            ScriptTurn::text("completed task"),
            ScriptTurn::text("a new turn"),
        ],
        oxa_fake::FakeOpts::default(),
    )
    .unwrap();
    let engine: Arc<dyn Engine> = Arc::new(oxa_engine::HttpEngine::new(&server.url(), None).unwrap());
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut st = build(
        &env,
        engine.clone(),
        store,
        Vec::new(),
        Mode::Workspace,
        Vec::new(),
    );
    say(&mut st, "write a lot");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].max_tokens, 0);
    assert_eq!(requests[1].kind, "prefill");
    assert_eq!(requests[2].messages.last().unwrap().role, oxa_proto::Role::User);
    assert_eq!(
        requests[2].reply.as_ref().unwrap().content.as_deref(),
        Some("completed task")
    );

    // F9 remains supported: trailing assistant history starts a new turn, not an implicit prefill.
    let wire = st.transcript.wire().to_vec();
    assert_eq!(wire.last().unwrap().role, oxa_proto::Role::Assistant);
    let hints = oxa_proto::Hints {
        session: "s",
        idem_key: "",
        kind: oxa_proto::TurnKind::Chat,
        background: false,
        snapshot_keep: false,
    };
    let req = |max_tokens| ChatRequest {
        output_schema: None,
        prefix: &st.prefix,
        messages: &wire,
        model: "fake-model",
        max_tokens,
        thinking: false,
        thinking_budget: None,
        thinking_level: None,
        preserve_thinking: true,
        sampling: None,
        hints,
    };
    // F9: the engine answers it as a NEW turn (history + reply), not a continuation, not a 400.
    let t = engine
        .chat(&req(st.info.max_output), &CancelToken::new(), &mut |_| {})
        .expect("a trailing assistant message is history");
    assert_eq!(t.msg.content.as_deref(), Some("a new turn"));
    // The same history as an H6 prefill is fine.
    engine
        .chat(&req(0), &CancelToken::new(), &mut |_| {})
        .expect("H6 prefill after a reply");
}

/// tool_errors texts
#[test]
fn unknown_tool_and_bad_json_fixed_errors() {
    let env = TestEnv::new();
    let (echo_tool, _) = echo("read_like");
    let turns = vec![
        turn_calls(vec![
            ("c1", "nope", "{}"),
            ("c2", "read_like", "[1, 2]"),
            ("c3", "read_like", r#"{"path": "#),
        ]),
        ScriptTurn::text("ok"),
    ];
    let (mut st, _eng) = scripted_state(&env, turns, vec![echo_tool], Mode::Workspace);
    say(&mut st, "try");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    let wire = st.transcript.wire();
    assert_eq!(tool_text(&wire[2]), "Tool nope not found");
    assert_eq!(tool_text(&wire[3]), tool_errors::BAD_JSON);
    assert_eq!(tool_text(&wire[4]), tool_errors::BAD_JSON);
}

/// as a user message
#[test]
fn steering_delivered_after_tool_batch() {
    let env = TestEnv::new();
    env.write("work/a.txt", "hello\n");
    let turns = vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "a.txt"}"#),
        ScriptTurn::text("ok"),
    ];
    let (mut st, eng) = scripted_state(
        &env,
        turns,
        vec![Arc::new(ReadTool::new()) as Arc<dyn Tool>],
        Mode::Workspace,
    );
    say(&mut st, "read it");
    st.shared
        .queue
        .lock()
        .unwrap()
        .steering
        .push("also check b.txt".to_string());

    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    let reqs = eng.requests();
    let second = &reqs[1].messages;
    assert_eq!(second.len(), 4, "user, call, tool result, steering");
    assert_eq!(second[3].role, oxa_proto::Role::User);
    assert_eq!(second[3].content.as_deref(), Some("also check b.txt"));
    assert!(st.shared.queue.lock().unwrap().steering.is_empty());
    w1_holds(&reqs);
}

/// cached < expected emits CacheMiss
#[test]
fn cache_miss_event_with_cause() {
    let env = TestEnv::new();
    env.write("work/a.txt", "hello\n");
    let inner = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "a.txt"}"#),
        ScriptTurn::text("done"),
    ]));
    let engine: Arc<dyn Engine> = Arc::new(MissOnSecond {
        inner,
        calls: AtomicUsize::new(0),
    });
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut st = build(
        &env,
        engine,
        store,
        vec![Arc::new(ReadTool::new())],
        Mode::Workspace,
        Vec::new(),
    );
    say(&mut st, "read");
    let (end, events) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    let misses: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::CacheMiss {
                expected,
                cached,
                cause,
            } => Some((*expected, *cached, *cause)),
            _ => None,
        })
        .collect();
    assert_eq!(misses.len(), 1, "exactly the second reply misses");
    let (expected, cached, cause) = misses[0];
    assert!(expected > 0);
    assert_eq!(cached, 0);
    assert_eq!(
        cause,
        miss_cause::ENGINE_EVICTED,
        "no rebase happened and it was not the first request: the engine lost the state \
         (`unknown` must never occur, plan sec. 6.6)"
    );
}

/// over-cap output is capped with a spill
#[test]
fn tool_result_goes_through_shape() {
    let env = TestEnv::new();
    let big = tool_with(
        "big",
        ToolClass::Read,
        no_access(),
        Box::new(|_a: &Value, _c: &ToolCtx| ToolOutput::ok("line of output\n".repeat(10_000))),
    )
    .0;
    let (mut st, _eng) = scripted_state(
        &env,
        vec![turn_calls(vec![("b1", "big", "{}")]), ScriptTurn::text("ok")],
        vec![big],
        Mode::Workspace,
    );
    say(&mut st, "big");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);

    let text = tool_text(&st.transcript.wire()[2]);
    let cap = st.settings.tool_output_cap();
    assert!(text.len() <= cap, "capped at {cap} bytes, got {}", text.len());
    assert!(text.contains("[output truncated:"), "the mark is in the result");
    let spills = std::fs::read_dir(env.tmp()).expect("tmp dir").count();
    assert!(spills >= 1, "the full output was spilled");
}

/// wire keeps the original args_raw
#[test]
fn hook_tool_call_block_and_replace_args() {
    let env = TestEnv::new();
    let (echo_tool, seen) = echo("echo");
    let (blocked_tool, blocked_seen) = echo("blocked");
    let original = r#"{ "tag" :  "original" }"#;
    let turns = vec![
        turn_calls(vec![("e1", "echo", original), ("b1", "blocked", "{}")]),
        ScriptTurn::text("ok"),
    ];
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(ToolHook)];
    let mut st = build(
        &env,
        engine,
        Arc::new(MemStore::new()),
        vec![echo_tool, blocked_tool],
        Mode::Workspace,
        hooks,
    );
    say(&mut st, "go");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);

    let wire = st.transcript.wire();
    assert_eq!(
        tool_text(&wire[2]),
        "replaced",
        "execution saw the replaced arguments"
    );
    assert_eq!(seen.lock().unwrap()[0]["tag"], "replaced");
    assert_eq!(tool_text(&wire[3]), "blocked by hook");
    assert!(
        blocked_seen.lock().unwrap().is_empty(),
        "a blocked call never runs"
    );

    // The assistant message on the wire carries the args exactly as streamed (W2).
    let reqs = eng.requests();
    let echoed = reqs[1].messages[1].tool_calls[0].args_raw.clone();
    assert_eq!(echoed, original);
    w1_holds(&reqs);
}

/// Notice + unchanged wire
#[test]
fn guard_refuses_non_prefix_context() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let engine: Arc<dyn Engine> = eng.clone();
    let hook: Arc<dyn Hooks> = Arc::new(OneShot {
        point: HookPoint::Context,
        reply: Mutex::new(Some(HookReply::Rebase(vec![WireMsg::user(
            "replaced everything",
        )]))),
    });
    let mut st = build(
        &env,
        engine,
        Arc::new(MemStore::new()),
        Vec::new(),
        Mode::Workspace,
        vec![hook],
    );
    say(&mut st, "hi");
    let before = st.transcript.wire().to_vec();

    let (end, events) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    let reqs = eng.requests();
    assert_eq!(
        reqs[0].messages[0],
        WireMsg::user("hi"),
        "the request went out unchanged"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Notice { text, .. } if text.contains(codes::GUARD_REFUSED))));
    assert_eq!(
        st.transcript.wire()[..before.len()],
        before[..],
        "wire unchanged (W1)"
    );
}

/// store error
#[test]
fn append_entry_failure_leaves_state() {
    let env = TestEnv::new();
    let mem = Arc::new(MemStore::new());
    let store: Arc<dyn SessionStore> = Arc::new(FailAppend {
        inner: Arc::clone(&mem),
    });
    let eng: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut st = build(&env, eng, store, Vec::new(), Mode::Workspace, Vec::new());
    let entries_before = st.tree.entries().len();
    let wire_before = st.transcript.wire().to_vec();

    let err = append_entry(
        &mut st,
        EntryBody::Msg {
            wire: WireMsg::user("lost"),
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect_err("the store refuses the append");
    assert_eq!(err.code, codes::IO);
    assert_eq!(st.tree.entries().len(), entries_before, "tree unchanged");
    assert_eq!(st.transcript.wire(), wire_before.as_slice(), "wire unchanged");
    assert!(st.tree.leaf().is_empty() || st.tree.get(st.tree.leaf()).is_some());
}

// ---------------------------------------------------------------------------------------------
// Extra coverage

/// A successful append is stored, folded and becomes the leaf.
#[test]
fn append_entry_persists_and_folds() {
    let env = TestEnv::new();
    let mem = Arc::new(MemStore::new());
    let store: Arc<dyn SessionStore> = mem.clone();
    let eng: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut st = build(&env, eng, store, Vec::new(), Mode::Workspace, Vec::new());
    let e1 = append_entry(
        &mut st,
        EntryBody::Msg {
            wire: WireMsg::user("one"),
            parts: Vec::new(),
            tool: None,
        },
    )
    .unwrap();
    let e2 = append_entry(
        &mut st,
        EntryBody::Msg {
            wire: WireMsg::assistant("two"),
            parts: Vec::new(),
            tool: None,
        },
    )
    .unwrap();
    assert_eq!(e2.parent.as_deref(), Some(e1.id.as_str()));
    assert_eq!(st.tree.leaf(), e2.id);
    assert_eq!(st.transcript.wire().len(), 2);
    let stored = mem.load(&st.sref).unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].body, e2.body);
}

/// A hook may extend the wire with plain user messages; they are persisted as entries.
#[test]
fn guard_extension_append_is_persisted() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let engine: Arc<dyn Engine> = eng.clone();
    let hook: Arc<dyn Hooks> = Arc::new(OneShot {
        point: HookPoint::Context,
        reply: Mutex::new(Some(HookReply::Rebase(vec![
            WireMsg::user("hi"),
            WireMsg::user("note from a hook"),
        ]))),
    });
    let mem = Arc::new(MemStore::new());
    let mut st = build(&env, engine, mem.clone(), Vec::new(), Mode::Workspace, vec![hook]);
    say(&mut st, "hi");
    let (end, _) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    let reqs = eng.requests();
    assert_eq!(
        reqs[0].messages.len(),
        2,
        "the extension message is on the request"
    );
    assert_eq!(reqs[0].messages[1], WireMsg::user("note from a hook"));
    assert!(mem.load(&st.sref).unwrap().iter().any(
        |e| matches!(&e.body, EntryBody::CustomMessage { content, .. } if content == "note from a hook")
    ));
    assert_eq!(st.transcript.wire().len(), 3, "user, hook note, reply");
}

/// Exhausted length recovery is bounded and the run emits exactly one `AgentEnd`.
#[test]
fn length_continue_then_stop_emits_one_agent_end() {
    let env = TestEnv::new();
    let mut only = ScriptTurn::text("cut");
    only.finish = Finish::Length;
    let (mut st, eng) = scripted_state(
        &env,
        vec![only.clone(), only, ScriptTurn::text("never asked")],
        Vec::new(),
        Mode::Workspace,
    );
    st.settings = Settings::from_values(
        serde_json::json!({"retry":{"enabled":true,"maxRetries":1}}),
        serde_json::json!({}),
    );
    say(&mut st, "x");
    let (end, events) = run(&mut st);
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 3, "two generations and one prefill");
    assert_eq!(eng.remaining(), 1);
    assert!(st.stats.output_truncated);
    let ends = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        .count();
    assert_eq!(ends, 1);
}

/// C7 carry-over: write-class calls hold their `Access::Write` paths in the process-wide
/// `MutationQueue` while they run, so two sessions writing the same file never overlap; a
/// write on another path does not wait.
#[test]
fn write_calls_are_serialised_per_path_across_sessions() {
    use std::sync::atomic::AtomicBool;
    let env = TestEnv::new();
    let live = Arc::new(AtomicUsize::new(0));
    let overlap = Arc::new(AtomicBool::new(false));
    let writer = |file: &'static str| {
        let (live, overlap) = (Arc::clone(&live), Arc::clone(&overlap));
        tool_with(
            "writer",
            ToolClass::Write,
            Box::new(move |_a: &Value, cx: &ToolCtx| Ok(vec![Access::Write(cx.cwd.join(file))])),
            Box::new(move |_a: &Value, _c: &ToolCtx| {
                if live.fetch_add(1, Ordering::SeqCst) > 0 {
                    overlap.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(60));
                live.fetch_sub(1, Ordering::SeqCst);
                ToolOutput::ok("written".to_string())
            }),
        )
        .0
    };
    let call = vec![ToolCall {
        id: "w".to_string(),
        name: "writer".to_string(),
        args_raw: "{}".to_string(),
    }];
    let run_pair = |a: &'static str, b: &'static str| {
        let tools = [writer(a), writer(b)];
        std::thread::scope(|s| {
            for tool in tools {
                let (env, call) = (&env, &call);
                s.spawn(move || {
                    let (mut st, _) = scripted_state(env, Vec::new(), vec![tool], Mode::Workspace);
                    let out = oxa_core::execute_calls(
                        &mut st,
                        call,
                        &mut VecSink::default(),
                        &mut HeadlessUi::default(),
                    );
                    assert_eq!(out[0].content.as_deref(), Some("written"));
                });
            }
        });
    };
    run_pair("same.txt", "same.txt");
    assert!(
        !overlap.load(Ordering::SeqCst),
        "two writes on one path ran at once"
    );

    // Different paths may overlap; nothing to assert beyond termination (no deadlock).
    run_pair("one.txt", "two.txt");
}

/// X4: the write lock identity is the canonical path, computed before the paths are sorted and
/// deduplicated. One call that names a file twice (the file and a symlink to it, `./f` and `f`)
/// must not wait for itself, and two concurrent calls that name the same files through
/// different aliases (raw order a-link, b versus b, z; canonical b, z for both) must run one
/// after the other without deadlock. Bounded: every wait has a timeout.
#[cfg(unix)]
#[test]
fn write_locks_use_canonical_paths_for_aliases() {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    let env = TestEnv::new();
    let cwd = env.work();
    std::fs::write(cwd.join("z.txt"), "z").unwrap();
    std::fs::write(cwd.join("b.txt"), "b").unwrap();
    std::os::unix::fs::symlink(cwd.join("z.txt"), cwd.join("a-link")).unwrap();
    std::fs::create_dir(cwd.join("dir")).unwrap();
    std::os::unix::fs::symlink(cwd.join("dir"), cwd.join("dlink")).unwrap();

    let live = Arc::new(AtomicUsize::new(0));
    let overlap = Arc::new(AtomicBool::new(false));
    let writer = |files: &'static [&'static str]| {
        let (live, overlap) = (Arc::clone(&live), Arc::clone(&overlap));
        tool_with(
            "writer",
            ToolClass::Write,
            Box::new(move |_a: &Value, cx: &ToolCtx| {
                Ok(files.iter().map(|f| Access::Write(cx.cwd.join(f))).collect())
            }),
            Box::new(move |_a: &Value, _c: &ToolCtx| {
                if live.fetch_add(1, Ordering::SeqCst) > 0 {
                    overlap.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(20));
                live.fetch_sub(1, Ordering::SeqCst);
                ToolOutput::ok("written".to_string())
            }),
        )
        .0
    };
    let call = vec![ToolCall {
        id: "w".to_string(),
        name: "writer".to_string(),
        args_raw: "{}".to_string(),
    }];
    // Run each tool in its own session on its own thread; all must finish within the bound.
    let run_all = |tools: Vec<Arc<dyn Tool>>| {
        let (tx, rx) = mpsc::channel();
        for tool in tools {
            let (mut st, _) = scripted_state(&env, Vec::new(), vec![tool], Mode::Workspace);
            let (tx, call) = (tx.clone(), call.clone());
            std::thread::spawn(move || {
                let out = oxa_core::execute_calls(
                    &mut st,
                    &call,
                    &mut VecSink::default(),
                    &mut HeadlessUi::default(),
                );
                tx.send(out[0].content.clone()).unwrap();
            });
        }
        drop(tx);
        let mut done = 0;
        while let Ok(content) = rx.recv_timeout(Duration::from_secs(10)) {
            assert_eq!(content.as_deref(), Some("written"));
            done += 1;
        }
        done
    };

    // One call, the same file three times (symlink, plain, `./`): no self-deadlock.
    assert_eq!(
        run_all(vec![writer(&["a-link", "z.txt", "./z.txt"])]),
        1,
        "a call waited for itself"
    );
    // A new file through a directory symlink and through `dir/./`: one key.
    assert_eq!(
        run_all(vec![writer(&[
            "dlink/new.txt",
            "dir/./new.txt",
            "dir/x/../new.txt"
        ])]),
        1
    );

    // Opposite raw orders of the same canonical set, many times: never a deadlock, never overlap.
    for _ in 0..10 {
        let done = run_all(vec![
            writer(&["a-link", "b.txt"]),
            writer(&["./b.txt", "z.txt"]),
            writer(&["dlink/../b.txt", "a-link"]),
        ]);
        assert_eq!(done, 3, "aliased calls deadlocked");
    }
    // A not-yet-existing file named through two aliases is one lock as well.
    for _ in 0..5 {
        assert_eq!(
            run_all(vec![writer(&["dlink/new.txt"]), writer(&["./dir/new.txt"])]),
            2
        );
    }
    assert!(
        !overlap.load(Ordering::SeqCst),
        "two calls on aliases of one file ran at once"
    );

    // Keys: aliases map to one identity; the order is the identities' order.
    use oxa_core::MutationQueue;
    let keys = MutationQueue::keys([
        cwd.join("a-link").as_path(),
        cwd.join("./b.txt").as_path(),
        cwd.join("z.txt").as_path(),
    ]);
    let real = |f: &str| std::fs::canonicalize(cwd.join(f)).unwrap();
    assert_eq!(keys, vec![real("b.txt"), real("z.txt")]);
    assert_eq!(
        MutationQueue::key(&cwd.join("dlink/new.txt")),
        real("dir").join("new.txt")
    );
}

/// Exercise the real run-loop request construction, not just the settings accessor.
#[test]
fn optional_thinking_budget_flows_through_the_policy_only_for_supported_local_requests() {
    struct CaptureBudget {
        inner: ScriptedEngine,
        cap: bool,
        remote: bool,
        seen: Mutex<Vec<Option<u64>>>,
    }
    impl Engine for CaptureBudget {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            let mut info = self.inner.info(target)?;
            info.caps.thinking_budget = self.cap;
            info.caps.remote = self.remote;
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
            self.seen.lock().unwrap().push(r.thinking_budget);
            self.inner.chat(r, cancel, on)
        }
    }
    for (budget, cap, remote, thinking, want) in [
        (Value::Null, true, false, true, None),
        (Value::from(512), true, false, true, Some(512)),
        (Value::from(0), true, false, true, Some(0)),
        (Value::from(512), false, false, true, None),
        (Value::from(512), true, true, true, None),
        (Value::from(512), true, false, false, None),
    ] {
        let env = TestEnv::new();
        let engine = Arc::new(CaptureBudget {
            inner: ScriptedEngine::new(vec![ScriptTurn::text("answer")]),
            cap,
            remote,
            seen: Mutex::new(Vec::new()),
        });
        let mut st = build(
            &env,
            engine.clone(),
            Arc::new(MemStore::new()),
            Vec::new(),
            Mode::Workspace,
            Vec::new(),
        );
        st.settings = Settings::from_values(
            serde_json::json!({"oxa":{"thinking":{"budget":budget,"policy":"fixed"}}}),
            serde_json::json!({}),
        );
        st.thinking = if thinking {
            ThinkingLevel::Medium
        } else {
            ThinkingLevel::Off
        };
        say(&mut st, "Reply with a short answer.");
        assert_eq!(run(&mut st).0, RunEnd::Stopped);
        assert_eq!(
            *engine.seen.lock().unwrap(),
            vec![want],
            "cap={cap}, remote={remote}, thinking={thinking}"
        );
    }
}
