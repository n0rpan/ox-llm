//! OXA-LOCAL-FIRST F1 (`oxa.prefill.tools`): prefill while tools run. A reply with several
//! tool calls: while a slow call runs, the results of the finished calls before it are final, so
//! one background zero-token request over `wire + those results` warms the engine's cache. The
//! follow-up request is byte-identical to today's; nothing is recorded.
//! Isolation: every file lives under `oxa_fake::TestEnv`. The tests share the process-wide
//! prefill delay (`Session::set_tool_prefill_after_for_tests`), so they run one at a time.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, tool_errors, AgentEvent, CancelToken, ChatRequest, CommandSpec, Engine, EngineInfo, Exposure,
    HeadlessUi, HookPoint, HookReply, Hooks, OxaError, Role, Source, StreamEvent, Tool, ToolCall, ToolClass,
    ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

/// One test at a time, with the prefill delay at `after`.
fn serial(after: Duration) -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    Session::set_tool_prefill_after_for_tests(after);
    g
}

const SHORT: Duration = Duration::from_millis(30);

/// The scripted engine plus an order log; prefill requests block until cancelled when
/// `block` is set (reported on `entered`).
struct Logged {
    inner: Arc<ScriptedEngine>,
    block: bool,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    log: Mutex<Vec<String>>,
}

impl Logged {
    fn new(inner: Arc<ScriptedEngine>, block: bool) -> Arc<Logged> {
        Arc::new(Logged {
            inner,
            block,
            entered: Mutex::new(None),
            log: Mutex::new(Vec::new()),
        })
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Engine for Logged {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
        self.inner.count_prompt(r)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<oxa_proto::AssistantTurn, OxaError> {
        if r.max_tokens == 0 && self.block {
            self.log.lock().unwrap().push("prefill-start".into());
            if let Some(tx) = self.entered.lock().unwrap().as_ref() {
                let _ = tx.send(());
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while !cancel.is_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.log.lock().unwrap().push("prefill-cancelled".into());
            return Err(OxaError::new(codes::ABORTED, "aborted"));
        }
        let res = self.inner.chat(r, cancel, on);
        self.log.lock().unwrap().push(r.hints.kind.as_str().to_string());
        res
    }
}

/// How the slow tool decides to return.
type Until = Arc<dyn Fn(&ToolCtx) + Send + Sync>;

/// `read`: a fast read returning `bytes` bytes. `slow`: an exec call that returns when `until`
/// does (or at once when cancelled).
struct TestTool {
    spec: ToolSpec,
    bytes: usize,
    until: Option<Until>,
}

impl Tool for TestTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<oxa_proto::Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        if let Some(until) = &self.until {
            until(cx);
            if cx.cancel.is_cancelled() {
                return ToolOutput::error(tool_errors::ABORTED);
            }
            return ToolOutput {
                text: "slow done".into(),
                is_error: false,
                ui: Value::Null,
            };
        }
        let tag = args.get("tag").and_then(Value::as_str).unwrap_or("x");
        let line = format!("{tag}: fn main() {{ println!(\"hello\"); }}\n");
        ToolOutput {
            text: line.repeat(self.bytes / line.len() + 1)[..self.bytes].to_string(),
            is_error: false,
            ui: Value::Null,
        }
    }
}

fn spec(name: &str, class: ToolClass) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "test tool".into(),
        schema_json: "{}".into(),
        class,
        exposure: Exposure::Direct,
        source: Source::Builtin,
    }
}

fn tools(read_bytes: usize, until: Until) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(TestTool {
            spec: spec("read", ToolClass::Read),
            bytes: read_bytes,
            until: None,
        }),
        Arc::new(TestTool {
            spec: spec("slow", ToolClass::Exec),
            bytes: 0,
            until: Some(until),
        }),
    ]
}

/// Waits until `pred` holds or the run is cancelled (bounded).
fn wait_for(pred: impl Fn() -> bool + Send + Sync + 'static) -> Until {
    Arc::new(move |cx: &ToolCtx| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pred() && !cx.cancel.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

/// Runs for `d` (or until cancelled).
fn runs_for(d: Duration) -> Until {
    Arc::new(move |cx: &ToolCtx| {
        let deadline = Instant::now() + d;
        while !cx.cancel.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

/// A hook set that wants nothing: the session gets a host, so tools run on a helper thread
/// while the session thread serves it.
struct Quiet;

impl Hooks for Quiet {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
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
    fn name(&self) -> &str {
        "quiet"
    }
    fn wants(&self, _p: HookPoint) -> bool {
        false
    }
}

fn tools_on(on: bool) -> Settings {
    Settings::from_values(json!({"oxa": {"prefill": {"tools": on}}}), json!({}))
}

fn open(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    tools: Vec<Arc<dyn Tool>>,
    settings: Settings,
    host: bool,
) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let hooks: Vec<Arc<dyn Hooks>> = if host { vec![Arc::new(Quiet)] } else { Vec::new() };
    let deps = Deps {
        engine,
        store,
        tools,
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, Open::New).expect("open")
}

fn call(id: &str, name: &str, tag: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args_raw: format!("{{\"tag\": \"{tag}\"}}"),
    }
}

/// One reply with `calls`, then a final text reply.
fn script(calls: Vec<ToolCall>) -> Vec<ScriptTurn> {
    let mut t = ScriptTurn::tool("unused", "read", "{}");
    t.tool_calls = calls;
    vec![t, ScriptTurn::text("all done")]
}

fn reads_then_slow() -> Vec<ToolCall> {
    vec![
        call("c1", "read", "a"),
        call("c2", "read", "b"),
        call("c3", "slow", "s"),
    ]
}

fn has_prefill(eng: &ScriptedEngine) -> bool {
    eng.requests().iter().any(|r| r.max_tokens == 0)
}

fn misses(sink: &VecSink) -> usize {
    sink.0
        .iter()
        .filter(|e| matches!(e, AgentEvent::CacheMiss { .. }))
        .count()
}

/// (kind, zero-token prefill).
fn shape(r: &Recorded) -> (&str, bool) {
    (r.kind.as_str(), r.max_tokens == 0)
}

/// Independent sessions compared below must see the same date/minute, even across midnight.
fn fixed_time<R>(run: impl FnOnce() -> R) -> R {
    oxa_core::envnote::with_time_for_tests(
        oxa_session::LocalTime {
            date: "2026-01-02".into(),
            weekday: "Friday",
            hm: "03:04".into(),
            zone: "UTC".into(),
            offset_min: 0,
        },
        run,
    )
}

/// Runs the 3-call scenario; the slow call returns once a prefill was recorded (feature on) or
/// after a while (off). Returns the requests and the events.
fn scenario(env: &TestEnv, on: bool, host: bool) -> (Vec<Recorded>, VecSink) {
    let eng = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let until = if on {
        let e = eng.clone();
        wait_for(move || has_prefill(&e))
    } else {
        runs_for(SHORT * 4)
    };
    let mut s = open(env, eng.clone(), tools(1500, until), tools_on(on), host);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    fixed_time(|| {
        s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
    });
    assert_eq!(s.stats().cache_misses, 0);
    (eng.requests(), sink)
}

/// Golden: chat (3 calls) -> prefill (wire + the first two results, H6/H3/H4, `prefill`) ->
/// chat follow-up, whose bytes are exactly today's and whose `cached` covers the prefill; no
/// `cache_miss`. Same with a host (the tool runs on a helper thread).
#[test]
fn prefill_while_slow_tool_runs_golden() {
    let _g = serial(SHORT);
    for host in [false, true] {
        let (reqs, sink) = scenario(&TestEnv::new(), true, host);
        let seq: Vec<(&str, bool)> = reqs.iter().map(shape).collect();
        assert_eq!(
            seq,
            vec![("chat", false), ("prefill", true), ("chat", false)],
            "host {host}"
        );
        let (first, pre, follow) = (&reqs[0], &reqs[1], &reqs[2]);
        assert!(pre.background && pre.snapshot_keep, "H3 + H4 as the idle prefill");
        assert_eq!(pre.session, follow.session);
        assert_eq!(
            (&pre.system, &pre.tools_json),
            (&follow.system, &follow.tools_json)
        );
        // wire (user, assistant tool calls) + the two finished results.
        assert_eq!(pre.messages.len(), first.messages.len() + 3);
        assert_eq!(pre.messages[..], follow.messages[..pre.messages.len()]);
        assert_eq!(pre.messages[pre.messages.len() - 3].role, Role::Assistant);
        let ids: Vec<_> = pre.messages[pre.messages.len() - 2..]
            .iter()
            .map(|m| m.tool_call_id.clone().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["c1", "c2"]);
        assert_eq!(follow.messages.len(), pre.messages.len() + 1);
        assert_eq!(
            follow.messages.last(),
            Some(&WireMsg::tool_result("c3".to_string(), "slow done".to_string()))
        );
        let (pre_prompt, cached) = (pre.usage.unwrap().prompt, follow.usage.unwrap().cached);
        assert!(
            cached >= pre_prompt,
            "cached {cached} covers the prefill {pre_prompt}"
        );
        assert_eq!(misses(&sink), 0, "more cached than expected is silent");
    }
}

/// The scenario really uses the pinned clock, not wall time. Keep notes enabled so the
/// byte-identity test below still compares the full environment block.
#[test]
fn scenario_env_note_uses_fixed_clock() {
    let _g = serial(SHORT);
    let (reqs, _) = scenario(&TestEnv::new(), true, false);
    let user = reqs[0].messages.iter().find(|m| m.role == Role::User).unwrap();
    assert!(
        user.content
            .as_deref()
            .unwrap()
            .contains("<env>\nDate: Friday 2026-01-02, local time 03:04 UTC (UTC)"),
        "scenario must use the fixed environment-note clock: {user:?}"
    );
}

/// The follow-up request is byte-identical with the feature on and off; off sends today's
/// sequence exactly (chat, chat).
#[test]
fn follow_up_bytes_identical_on_and_off() {
    let _g = serial(SHORT);
    // One environment: the prefix names the working directory.
    let env = TestEnv::new();
    let (on, _) = scenario(&env, true, false);
    let (off, _) = scenario(&env, false, false);
    let seq: Vec<(&str, bool)> = off.iter().map(shape).collect();
    assert_eq!(
        seq,
        vec![("chat", false), ("chat", false)],
        "feature off: today's sequence"
    );
    let (a, b) = (on.last().unwrap(), off.last().unwrap());
    assert_eq!(a.messages, b.messages);
    assert_eq!(a.system, b.system);
    assert_eq!(a.tools_json, b.tools_json);
    assert_eq!(
        (a.max_tokens, a.thinking, &a.kind),
        (b.max_tokens, b.thinking, &b.kind)
    );
    assert_eq!(on[0].messages, off[0].messages);
}

/// The slow call finishes while the prefill is in flight: the prefill is cancelled and joined
/// before the follow-up is sent (never two requests at once).
#[test]
fn prefill_cancelled_and_joined_before_follow_up() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let logged = Logged::new(scripted.clone(), true);
    let (tx, rx) = mpsc::channel();
    *logged.entered.lock().unwrap() = Some(tx);
    let rx = Mutex::new(rx);
    let until: Until = Arc::new(move |_cx: &ToolCtx| {
        let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
    });
    let mut s = open(&env, logged.clone(), tools(1500, until), tools_on(true), false);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    assert_eq!(
        logged.log(),
        vec!["chat", "prefill-start", "prefill-cancelled", "chat"]
    );
    let notices = sink
        .0
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice { text, .. } if text.contains("prefill")))
        .count();
    assert_eq!(notices, 0, "a cancel is silent");
    assert_eq!(misses(&sink), 0);
    // The follow-up is today's.
    let reqs = scripted.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].messages.len(), 5);
}

/// The slow call finishes before the delay: nothing is sent.
#[test]
fn slow_call_done_before_delay_sends_nothing() {
    let _g = serial(Duration::from_secs(30));
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let started = Instant::now();
    let mut s = open(
        &env,
        eng.clone(),
        tools(1500, runs_for(SHORT)),
        tools_on(true),
        false,
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the waiting helper ends at once"
    );
    let reqs = eng.requests();
    let seq: Vec<(&str, bool)> = reqs.iter().map(shape).collect();
    assert_eq!(seq, vec![("chat", false), ("chat", false)]);
}

/// The user aborts while the slow call runs and the prefill is in flight: both end, nothing
/// follows.
#[test]
fn abort_during_slow_tool_cancels_both() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let logged = Logged::new(scripted.clone(), true);
    let (tx, rx) = mpsc::channel();
    *logged.entered.lock().unwrap() = Some(tx);
    let mut s = open(
        &env,
        logged.clone(),
        tools(1500, wait_for(|| false)),
        tools_on(true),
        false,
    );
    let handle = s.handle();
    let aborter = std::thread::spawn(move || {
        rx.recv_timeout(Duration::from_secs(10)).expect("prefill started");
        handle.abort();
    });
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    let started = Instant::now();
    let _ = s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui);
    aborter.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(logged.log(), vec!["chat", "prefill-start", "prefill-cancelled"]);
    assert!(sink
        .0
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true })));
}

/// The slow call is first: nothing before it is final, no prefill.
#[test]
fn slow_call_first_sends_no_prefill() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let calls = vec![
        call("c1", "slow", "s"),
        call("c2", "read", "a"),
        call("c3", "read", "b"),
    ];
    let eng = Arc::new(ScriptedEngine::new(script(calls)));
    let mut s = open(
        &env,
        eng.clone(),
        tools(3000, runs_for(SHORT * 6)),
        tools_on(true),
        false,
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    let reqs = eng.requests();
    let seq: Vec<(&str, bool)> = reqs.iter().map(shape).collect();
    assert_eq!(seq, vec![("chat", false), ("chat", false)]);
}

/// The finished results are under the threshold: no prefill.
#[test]
fn small_results_send_no_prefill() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let small = (Session::TOOL_PREFILL_MIN_BYTES - 1) / 2;
    let mut s = open(
        &env,
        eng.clone(),
        tools(small, runs_for(SHORT * 6)),
        tools_on(true),
        false,
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    let reqs = eng.requests();
    let seq: Vec<(&str, bool)> = reqs.iter().map(shape).collect();
    assert_eq!(seq, vec![("chat", false), ("chat", false)]);
}

// ---- l5-prefill-fix ----

/// Hook sets that want exactly one point (and answer `Continue`).
struct WantsOne(HookPoint);

impl Hooks for WantsOne {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
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
    fn name(&self) -> &str {
        "wants-one"
    }
    fn wants(&self, p: HookPoint) -> bool {
        p == self.0
    }
}

fn open_with(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    tools: Vec<Arc<dyn Tool>>,
    settings: Settings,
    hooks: Vec<Arc<dyn Hooks>>,
) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let deps = Deps {
        engine,
        store,
        tools,
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, Open::New).expect("open")
}

/// The OFF arm: `oxa.prefill.tools` off: no extra request at all, however long the slow call
/// runs past the delay.
#[test]
fn off_arm_prefill_tools_off_sends_no_prefill() {
    let _g = serial(Duration::from_millis(1));
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let settings = Settings::from_values(json!({"oxa": {"prefill": {"tools": false}}}), json!({}));
    let mut s = open(
        &env,
        eng.clone(),
        tools(3000, runs_for(SHORT * 4)),
        settings,
        false,
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    let reqs = eng.requests();
    let seq: Vec<(&str, bool)> = reqs.iter().map(shape).collect();
    assert_eq!(seq, vec![("chat", false), ("chat", false)]);
}

/// The prefill records nothing: the session's entries, the usage entries' kinds, the request
/// count and the cache expectation are the same with the feature on and off.
#[test]
fn tool_prefill_records_nothing() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let run = |on: bool| {
        let eng = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
        let until = if on {
            let e = eng.clone();
            wait_for(move || has_prefill(&e))
        } else {
            runs_for(SHORT * 4)
        };
        let mut s = open(&env, eng.clone(), tools(1500, until), tools_on(on), false);
        let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
        fixed_time(|| {
            s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
                .expect("prompt");
        });
        assert_eq!(has_prefill(&eng), on);
        let kinds: Vec<String> = s
            .inner()
            .tree
            .entries()
            .iter()
            .map(|e| match &e.body {
                oxa_proto::EntryBody::Usage { kind, .. } => format!("usage:{kind}"),
                _ => "entry".to_string(),
            })
            .collect();
        let st = s.stats();
        (
            kinds,
            st.requests,
            st.cache_misses,
            s.inner().transcript.expected_cache(),
        )
    };
    let on = run(true);
    let off = run(false);
    assert_eq!(on, off);
}

/// A `tool_result` hook can replace a result: no prefill (the bytes are not known yet).
#[test]
fn tool_result_hook_suppresses_the_prefill() {
    let _g = serial(Duration::from_millis(1));
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(WantsOne(HookPoint::ToolResult))];
    let mut s = open_with(
        &env,
        eng.clone(),
        tools(3000, runs_for(SHORT * 4)),
        tools_on(true),
        hooks,
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    assert!(!has_prefill(&eng));
}

/// The user aborts while the prefill is in flight and the slow call does NOT see the cancel
/// at once: the prefill still ends at once (it watches the user's abort itself).
#[test]
fn abort_cancels_an_in_flight_prefill_at_once() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let logged = Logged::new(scripted.clone(), true);
    let (tx, rx) = mpsc::channel();
    *logged.entered.lock().unwrap() = Some(tx);
    // Ignores the cancel until the prefill has ended (or 10 s).
    let seen = logged.clone();
    let until: Until = Arc::new(move |_cx: &ToolCtx| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen.log().iter().any(|l| l == "prefill-cancelled") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut s = open(&env, logged.clone(), tools(1500, until), tools_on(true), false);
    let handle = s.handle();
    let aborter = std::thread::spawn(move || {
        rx.recv_timeout(Duration::from_secs(10)).expect("prefill started");
        handle.abort();
    });
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    let started = Instant::now();
    let _ = s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui);
    aborter.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the prefill ended on the abort, not after the tool"
    );
    assert_eq!(logged.log(), vec!["chat", "prefill-start", "prefill-cancelled"]);
}

/// An engine whose zero-token request ignores the cancel until released.
struct Stuck {
    inner: Arc<ScriptedEngine>,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
    log: Mutex<Vec<String>>,
}

impl Engine for Stuck {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
        self.inner.count_prompt(r)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<oxa_proto::AssistantTurn, OxaError> {
        if r.max_tokens == 0 {
            self.log.lock().unwrap().push("prefill-start".into());
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release.lock().unwrap().take() {
                let _ = rx.recv_timeout(Duration::from_secs(20));
            }
            self.log.lock().unwrap().push("prefill-end".into());
            return Err(OxaError::new(codes::ABORTED, "aborted"));
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("{}-start", r.hints.kind.as_str()));
        self.inner.chat(r, cancel, on)
    }
}

/// Runs the 3-call scenario against [`Stuck`]: the slow call returns once the prefill is in
/// flight, and the prefill ends `release` later whatever the cancel. Returns the order log.
fn stuck_run(release: Duration) -> Vec<String> {
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let (tx, rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let stuck = Arc::new(Stuck {
        inner: scripted.clone(),
        entered: Mutex::new(Some(tx)),
        release: Mutex::new(Some(release_rx)),
        log: Mutex::new(Vec::new()),
    });
    let rx = Mutex::new(rx);
    let until: Until = Arc::new(move |_cx: &ToolCtx| {
        let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
        let release_tx = release_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(release);
            let _ = release_tx.send(());
        });
    });
    let mut s = open(&env, stuck.clone(), tools(1500, until), tools_on(true), false);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    let log = stuck.log.lock().unwrap().clone();
    log
}

/// A prefill that outlives the bounded join after its call stays registered: the follow-up
/// request settles it again and is sent only after it ended (never two requests in flight).
/// Takes ~4.5 s: the join bound is 3 s; released after the first bounded join (3 s), before
/// the second one ends (6 s).
#[test]
fn prefill_outliving_the_join_bound_is_settled_by_the_next_request() {
    let _g = serial(SHORT);
    assert_eq!(
        stuck_run(Duration::from_millis(4500)),
        vec!["chat-start", "prefill-start", "prefill-end", "chat-start"]
    );
}

/// Released only after every bounded join (finish 3 s, guard 3 s, request 3 s) ran out: the
/// follow-up still waits for it (deferred, abortable) and is never sent alongside it.
/// Takes ~11 s.
#[test]
fn prefill_outliving_every_join_bound_still_precedes_the_follow_up() {
    let _g = serial(SHORT);
    assert_eq!(
        stuck_run(Duration::from_millis(11_000)),
        vec!["chat-start", "prefill-start", "prefill-end", "chat-start"]
    );
}

/// The prefill goes to a REAL `HttpEngine` whose server accepts and never reads, so its write
/// blocks on full socket buffers; every other request goes to the script.
struct HttpPrefill {
    inner: Arc<ScriptedEngine>,
    http: oxa_engine::HttpEngine,
    log: Mutex<Vec<String>>,
}

impl Engine for HttpPrefill {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
        self.inner.count_prompt(r)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<oxa_proto::AssistantTurn, OxaError> {
        if r.max_tokens == 0 {
            self.log.lock().unwrap().push("prefill-start".into());
            // Larger than any loopback socket buffer pair.
            let mut msgs = r.messages.to_vec();
            msgs.push(WireMsg::user(" ".repeat(32 << 20)));
            let big = ChatRequest {
                messages: &msgs,
                ..*r
            };
            let res = self.http.chat(&big, cancel, on);
            self.log.lock().unwrap().push("prefill-end".into());
            return res;
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("{}-start", r.hints.kind.as_str()));
        self.inner.chat(r, cancel, on)
    }
}

/// Real transport: the tool-result prefill's write is blocked by an engine that stopped
/// reading; the cancel when the call returns ends it within a poll, and the follow-up is sent
/// only after it ended, promptly (not after the silence limit or the join bounds).
#[test]
fn http_prefill_blocked_writing_ends_on_cancel_before_the_follow_up() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Reports each accepted connection (the client writes right after its connect).
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(90);
        listener.set_nonblocking(true).unwrap();
        while Instant::now() < deadline {
            if let Ok((c, _)) = listener.accept() {
                held.push(c);
                let _ = tx.send(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let http = oxa_engine::HttpEngine::new(&format!("http://127.0.0.1:{port}"), None)
        .unwrap()
        .with_opts(
            oxa_engine::StreamOpts {
                connect_timeout_ms: 2_000,
                silence_limit_ms: 60_000,
            },
            0,
        );
    let eng = Arc::new(HttpPrefill {
        inner: Arc::new(ScriptedEngine::new(script(reads_then_slow()))),
        http,
        log: Mutex::new(Vec::new()),
    });
    let returned = Arc::new(Mutex::new(None));
    let (rx, ret) = (Mutex::new(rx), returned.clone());
    // Returns once the prefill is connected and its write has had time to block.
    let until: Until = Arc::new(move |_cx: &ToolCtx| {
        let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(500));
        *ret.lock().unwrap() = Some(Instant::now());
    });
    let mut s = open(&env, eng.clone(), tools(1500, until), tools_on(true), false);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    let after_call = returned.lock().unwrap().expect("slow call ran").elapsed();
    assert_eq!(
        *eng.log.lock().unwrap(),
        vec!["chat-start", "prefill-start", "prefill-end", "chat-start"]
    );
    assert!(
        after_call < Duration::from_secs(2),
        "the follow-up waited {after_call:?} after the call returned"
    );
}

#[test]
fn run_token_budget_counts_new_tokens_from_tool_prefill() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let (baseline, _) = scenario(&env, true, true);
    let spent: u64 = baseline[..2]
        .iter()
        .map(|r| {
            let u = r.usage.unwrap();
            u64::from(u.prompt.saturating_sub(u.cached)) + u64::from(u.completion)
        })
        .sum();
    let eng = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let e = eng.clone();
    let mut session = open(
        &env,
        eng.clone(),
        tools(1500, wait_for(move || has_prefill(&e))),
        tools_on(true),
        true,
    );
    session.handle().set_token_budget(Some(spent - 1));
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    session
        .prompt("look around".into(), Vec::new(), &mut sink, &mut ui)
        .unwrap();
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[1].max_tokens, 0);
    assert!(
        reqs[2].max_tokens <= 1024,
        "the prefill must consume the run budget before the follow-up"
    );
    assert_eq!(
        session.handle().wrap_up_reason(),
        Some(oxa_core::WrapUpReason::TokenLimit)
    );
    let total: u64 = reqs
        .iter()
        .map(|r| {
            let u = r.usage.unwrap();
            u64::from(u.prompt.saturating_sub(u.cached)) + u64::from(u.completion)
        })
        .sum();
    assert!(total > spent, "includes final-turn overspend");
    assert_eq!(session.handle().run_tokens_used(), total);
}

#[test]
fn advancing_tool_prefill_counts_as_progress_while_a_quiet_tool_runs() {
    struct Prefilling(Arc<ScriptedEngine>);
    impl Engine for Prefilling {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            self.0.info(target)
        }
        fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
            self.0.count_tokens(text)
        }
        fn count_prompt(&self, request: &ChatRequest) -> Result<Option<u64>, OxaError> {
            self.0.count_prompt(request)
        }
        fn chat(
            &self,
            request: &ChatRequest,
            cancel: &CancelToken,
            on: &mut dyn FnMut(StreamEvent),
        ) -> Result<oxa_proto::AssistantTurn, OxaError> {
            if request.max_tokens != 0 {
                return self.0.chat(request, cancel, on);
            }
            for done in [0, 1, 1, 0, 2] {
                on(StreamEvent::Prefill {
                    done,
                    total: 2,
                    secs: 0,
                });
                on(StreamEvent::Queued {
                    pos: None,
                    wait_s: done,
                });
            }
            // Keep the real scripted request/result/usage, with only the progress script replaced.
            self.0.chat(request, cancel, &mut |_| {})
        }
    }
    #[derive(Debug, Default)]
    struct Trace(Mutex<(bool, Vec<oxa_session::SessionRef>)>);
    impl oxa_core::ProgressObserver for Trace {
        fn notify(&self, session: &oxa_session::SessionRef, progress: oxa_core::Progress<'_>) {
            let mut state = self.0.lock().unwrap();
            match progress {
                oxa_core::Progress::RequestStart {
                    kind: oxa_proto::TurnKind::Prefill,
                    ..
                } => state.0 = true,
                oxa_core::Progress::RequestEnd { .. } => state.0 = false,
                oxa_core::Progress::Advanced if state.0 => state.1.push(session.clone()),
                _ => {}
            }
        }
    }
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let inner = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let waiter = inner.clone();
    let mut session = open(
        &env,
        Arc::new(Prefilling(inner.clone())),
        tools(1500, wait_for(move || has_prefill(&waiter))),
        tools_on(true),
        true,
    );
    let trace = Arc::new(Trace::default());
    session.set_progress_observer(trace.clone());
    session
        .prompt(
            "look around".into(),
            vec![],
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert_eq!(inner.requests().len(), 3, "chat, overlapping prefill, follow-up");
    let state = trace.0.lock().unwrap();
    assert_eq!(
        state.1.len(),
        2,
        "only increasing prefill counts advance the stall clock"
    );
    assert!(state
        .1
        .iter()
        .all(|reference| reference.id == inner.requests()[1].session));
}

#[test]
fn cancelled_sent_prefill_marks_goal_usage_unknown_before_another_generation() {
    let _g = serial(SHORT);
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(script(reads_then_slow())));
    let logged = Logged::new(scripted.clone(), true);
    let (tx, rx) = mpsc::channel();
    *logged.entered.lock().unwrap() = Some(tx);
    let rx = Mutex::new(rx);
    let until: Until = Arc::new(move |_cx: &ToolCtx| {
        rx.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let mut session = open(&env, logged.clone(), tools(1500, until), tools_on(true), false);
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    session
        .set_goal(
            Some(
                oxa_session::Goal::new(
                    "Synthetic bounded goal".into(),
                    oxa_session::GoalBudget {
                        max_tokens: Some(100000),
                        ..Default::default()
                    },
                    0,
                )
                .unwrap(),
            ),
            &mut sink,
        )
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut ui)
        .unwrap();
    assert_eq!(
        scripted.requests().len(),
        1,
        "An uncertain prefill must stop the next generation"
    );
    let goal = session.goal().unwrap();
    assert_eq!(goal.unknown_requests, 1);
    assert_eq!(goal.status, oxa_session::GoalStatus::Blocked);
}
