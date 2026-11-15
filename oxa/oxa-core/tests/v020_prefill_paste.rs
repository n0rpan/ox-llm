//! V020-OPTIMIZE #13 (`oxa.prefill.paste`): the draft prefill. While the user composes a large
//! message, `Session::prefill_draft` sends one background zero-token request over
//! `wire + [the user message the prompt would append]`; the prompt sent with the same draft
//! then finds it cached. Timing only: nothing is pushed, stored or recorded, and a lost prefill
//! is never a cache miss.
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, AgentEvent, CancelToken, ChatRequest, Engine, EngineInfo, EntryBody, HeadlessUi, OxaError, Part,
    StreamEvent, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The scripted engine behind a switch: `prefill_only` as advertised, and prefill requests that
/// block until cancelled (`block`), each one reported on `entered` and logged.
struct Gated {
    inner: Arc<ScriptedEngine>,
    prefill_only: bool,
    block: bool,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    log: Mutex<Vec<String>>,
}

impl Gated {
    fn new(inner: Arc<ScriptedEngine>) -> Gated {
        Gated {
            inner,
            prefill_only: true,
            block: false,
            entered: Mutex::new(None),
            log: Mutex::new(Vec::new()),
        }
    }
}

impl Engine for Gated {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let mut info = self.inner.info(target)?;
        info.prefill_only = self.prefill_only;
        Ok(info)
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
        self.log.lock().unwrap().push(r.hints.kind.as_str().to_string());
        self.inner.chat(r, cancel, on)
    }
}

fn paste_on() -> Settings {
    Settings::from_values(json!({"oxa": {"prefill": {"paste": true}}}), json!({}))
}

fn open(env: &TestEnv, engine: Arc<dyn Engine>, settings: Settings) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        // This fixture compares exact prompt/prefill bytes, with notes explicitly off.
        settings: settings.with_override("oxa.envNote", serde_json::json!(false)),
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

/// A pasted block of `n` bytes (several lines, like a real paste).
fn paste(n: usize) -> String {
    let line = "let value = compute(input, 42); // pasted line\n";
    line.repeat(n / line.len() + 1)[..n].to_string()
}

fn never() -> bool {
    false
}

fn misses(sink: &VecSink) -> usize {
    sink.0
        .iter()
        .filter(|e| matches!(e, AgentEvent::CacheMiss { .. }))
        .count()
}

fn prefills(reqs: &[Recorded]) -> Vec<&Recorded> {
    reqs.iter().filter(|r| r.max_tokens == 0).collect()
}

/// Golden: chat (turn 1) -> draft prefill (wire + the would-be user message, H6, H3, H4,
/// `prefill`) -> chat (turn 2) whose prompt repeats the prefilled message byte for byte and
/// whose `cached` covers it; no `cache_miss`, nothing recorded for the prefill.
#[test]
fn draft_prefill_then_enter_finds_it_cached() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("first answer"),
        ScriptTurn::text("second answer"),
    ]));
    let mut s = open(&env, eng.clone(), paste_on());
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("hello".into(), Vec::new(), &mut sink, &mut ui)
        .expect("turn 1");
    let entries_before = s.inner().tree.entries().len();
    let stats_before = s.stats();

    let draft = format!("Please review this:\n{}", paste(3000));
    assert!(
        s.prefill_draft(&draft, &[], &never, &mut sink, &mut ui),
        "prefilled"
    );
    // Nothing stored, nothing counted.
    assert_eq!(s.inner().tree.entries().len(), entries_before);
    assert_eq!(s.stats().requests, stats_before.requests);
    assert_eq!(s.stats().last_snap_pos, stats_before.last_snap_pos);

    s.prompt(draft.clone(), Vec::new(), &mut sink, &mut ui)
        .expect("turn 2");
    let reqs = eng.requests();
    let kinds: Vec<(&str, u32)> = reqs.iter().map(|r| (r.kind.as_str(), r.max_tokens)).collect();
    assert_eq!(kinds.len(), 3, "chat, prefill, chat: {kinds:?}");
    let (pre, chat) = (&reqs[1], &reqs[2]);
    assert_eq!(pre.kind, "prefill");
    assert_eq!(pre.max_tokens, 0);
    assert!(pre.background, "H3 background");
    assert!(!pre.snapshot_keep, "no H4: a draft is speculative");
    assert_eq!(reqs[0].kind, "chat");
    assert_eq!(chat.kind, "chat");
    // The prefilled prompt is the real prompt up to and including the user message.
    assert_eq!(pre.system, chat.system);
    assert_eq!(pre.tools_json, chat.tools_json);
    assert_eq!(pre.messages, chat.messages, "byte-identical messages");
    assert_eq!(pre.messages.last(), Some(&WireMsg::user(draft.clone())));
    // The real request finds the whole prefill cached; the expectation was only the wire.
    let pre_prompt = pre.usage.expect("prefill usage").prompt;
    let cached = chat.usage.expect("chat usage").cached;
    assert!(
        cached >= pre_prompt,
        "cached {cached} covers the prefill ({pre_prompt})"
    );
    assert_eq!(misses(&sink), 0, "more cached than expected is silent");
    assert_eq!(s.stats().cache_misses, 0);
    // No usage entry of kind `prefill` in the session.
    let prefill_usage = s
        .inner()
        .tree
        .entries()
        .iter()
        .any(|e| matches!(&e.body, EntryBody::Usage { kind, .. } if kind == "prefill"));
    assert!(!prefill_usage, "the draft prefill records nothing");
}

/// The prefilled user message equals the prompt's, also with image parts (placeholder lines)
/// on a fresh session (the first request expects a cold start: silent either way).
#[test]
fn prefilled_message_bytes_equal_the_prompt_with_parts() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let mut s = open(&env, eng.clone(), paste_on());
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    let parts = vec![
        Part::Image {
            path: env.work().join("shot.png"),
            mime: "image/png".into(),
            sha256: [7u8; 32],
            w: 640,
            h: 480,
        },
        Part::Text(paste(1200)),
    ];
    let draft = paste(1500);
    assert!(s.prefill_draft(&draft, &parts, &never, &mut sink, &mut ui));
    s.prompt(draft, parts, &mut sink, &mut ui).expect("prompt");
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].messages, reqs[1].messages);
    assert_eq!(misses(&sink), 0);
}

/// An edit before the request starts (the front end's generation moved): no request. An edit
/// while it runs (user input aborts it): it is cancelled, and the prompt goes out at once with
/// nothing recorded and no cache miss.
#[test]
fn edit_cancels_the_draft_prefill() {
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let mut gated = Gated::new(scripted.clone());
    gated.block = true;
    let (tx, rx) = mpsc::channel();
    *gated.entered.lock().unwrap() = Some(tx);
    let gated = Arc::new(gated);
    let mut s = open(&env, gated.clone(), paste_on());
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    let draft = paste(4000);

    // Stale before the request: nothing is sent.
    assert!(!s.prefill_draft(&draft, &[], &|| true, &mut sink, &mut ui));
    assert!(gated.log.lock().unwrap().is_empty());

    // Stale while it runs: the front end aborts it (`Loop::input` -> `Handle::abort`).
    let handle = s.handle();
    let aborter = std::thread::spawn(move || {
        rx.recv_timeout(Duration::from_secs(10)).expect("prefill started");
        handle.abort();
    });
    let started = Instant::now();
    assert!(
        !s.prefill_draft(&draft, &[], &never, &mut sink, &mut ui),
        "cancelled"
    );
    aborter.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "cancel latency only");
    let notices = sink
        .0
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice { text, .. } if text.contains("draft prefill")))
        .count();
    assert_eq!(notices, 0, "a cancel is silent");

    // The prompt goes out normally (the abort does not leak into it).
    s.prompt(
        format!("{draft} and one more line"),
        Vec::new(),
        &mut sink,
        &mut ui,
    )
    .expect("prompt");
    assert_eq!(
        *gated.log.lock().unwrap(),
        vec!["prefill-start", "prefill-cancelled", "chat"]
    );
    assert_eq!(misses(&sink), 0);
    assert_eq!(s.stats().requests, 1);
}

/// Setting off (profile `pi`, unset): no request.
#[test]
fn setting_off_sends_nothing() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(Vec::new()));
    let pi = Settings::from_values(json!({"oxa": {"profile": "pi"}}), json!({}));
    let mut s = open(&env, eng.clone(), pi);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    assert!(!s.prefill_draft(&paste(5000), &[], &never, &mut sink, &mut ui));
    let off = Settings::from_values(json!({"oxa": {"prefill": {"paste": false}}}), json!({}));
    let mut s2 = open(&env, eng.clone(), off);
    assert!(!s2.prefill_draft(&paste(5000), &[], &never, &mut sink, &mut ui));
    let warm_off = Settings::from_values(
        json!({"oxa": {"prefill": {"paste": true}}, "cacheWarming": "off"}),
        json!({}),
    );
    let mut s3 = open(&env, eng.clone(), warm_off);
    assert!(!s3.prefill_draft(&paste(5000), &[], &never, &mut sink, &mut ui));
    assert!(eng.requests().is_empty());
}

/// Under the threshold: no request; at the threshold: one.
#[test]
fn below_threshold_sends_nothing() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut s = open(&env, eng.clone(), paste_on());
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    let min = Session::DRAFT_PREFILL_MIN_BYTES;
    assert!(!s.prefill_draft(&paste(min - 1), &[], &never, &mut sink, &mut ui));
    assert!(eng.requests().is_empty());
    assert!(s.prefill_draft(&paste(min), &[], &never, &mut sink, &mut ui));
    assert_eq!(prefills(&eng.requests()).len(), 1);
}

/// An engine that does not advertise zero-token prefill: no request.
#[test]
fn engine_without_prefill_only_sends_nothing() {
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut gated = Gated::new(scripted.clone());
    gated.prefill_only = false;
    let gated = Arc::new(gated);
    let mut s = open(&env, gated.clone(), paste_on());
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    assert!(!s.prefill_draft(&paste(5000), &[], &never, &mut sink, &mut ui));
    assert!(gated.log.lock().unwrap().is_empty());
    assert!(scripted.requests().is_empty());
}

/// Commands and `!` lines are not prompts: no request.
#[test]
fn commands_are_not_prefilled() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut s = open(&env, eng.clone(), paste_on());
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    let body = paste(3000);
    assert!(!s.prefill_draft(&format!("!cat {body}"), &[], &never, &mut sink, &mut ui));
    assert!(!s.prefill_draft(&format!("/compact {body}"), &[], &never, &mut sink, &mut ui));
    assert!(eng.requests().is_empty());
}

// ---- l5-prefill-fix ----

use oxa_proto::{CommandSpec, HookPoint, HookReply, Hooks, Tool, Ui, Value};

/// The scripted engine reporting another base URL (where the engine "is").
struct At {
    inner: Arc<ScriptedEngine>,
    base_url: &'static str,
}

impl Engine for At {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let mut info = self.inner.info(target)?;
        info.base_url = self.base_url.to_string();
        Ok(info)
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
        self.inner.chat(r, cancel, on)
    }
}

/// Privacy: a draft never submitted is sent only to an engine on this machine.
#[test]
fn draft_prefill_only_to_a_loopback_engine() {
    let env = TestEnv::new();
    for (url, sent) in [
        ("http://127.0.0.1:8080", true),
        ("http://127.3.2.1:8080/v1", true),
        ("http://localhost:8080", true),
        ("http://[::1]:8080", true),
        ("http://10.0.0.5:8080", false),
        ("http://192.168.1.20:8080", false),
        ("https://engine.example.com", false),
    ] {
        let scripted = Arc::new(ScriptedEngine::new(Vec::new()));
        let at = Arc::new(At {
            inner: scripted.clone(),
            base_url: url,
        });
        let mut s = open(&env, at, paste_on());
        let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
        assert_eq!(
            s.prefill_draft(&paste(3000), &[], &never, &mut sink, &mut ui),
            sent,
            "{url}"
        );
        assert_eq!(prefills(&scripted.requests()).len(), usize::from(sent), "{url}");
    }
}

/// A hook set that wants exactly one point.
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

/// A hook that can change or block the prompt (`input`, `before_agent_start`, `context`,
/// `before_provider_request`) suppresses the draft prefill.
#[test]
fn prompt_hooks_suppress_the_draft_prefill() {
    let env = TestEnv::new();
    for point in [
        HookPoint::Input,
        HookPoint::BeforeAgentStart,
        HookPoint::Context,
        HookPoint::ContextWithSystem,
        HookPoint::BeforeProviderRequest,
    ] {
        let eng = Arc::new(ScriptedEngine::new(Vec::new()));
        let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
        let deps = Deps {
            engine: eng.clone(),
            store,
            tools: Vec::new(),
            hooks: vec![Arc::new(WantsOne(point))],
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings: paste_on(),
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
        let mut s = Session::open(deps, Open::New).expect("open");
        let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
        assert!(
            !s.prefill_draft(&paste(3000), &[], &never, &mut sink, &mut ui),
            "{point:?}"
        );
        assert!(prefills(&eng.requests()).is_empty(), "{point:?}");
    }
}

/// `Handle::cancel_background` (what the interactive loop sends on a key or a command)
/// ends a running draft prefill; the prompt the worker starts right after it is NOT cancelled
/// (the bug: `abort()` there also cancelled the user's run once it had claimed the session).
#[test]
fn cancel_background_ends_the_draft_prefill_but_not_the_next_prompt() {
    let env = TestEnv::new();
    let scripted = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let mut gated = Gated::new(scripted.clone());
    gated.block = true;
    let (tx, rx) = mpsc::channel();
    *gated.entered.lock().unwrap() = Some(tx);
    let gated = Arc::new(gated);
    let mut s = open(&env, gated.clone(), paste_on());
    let handle = s.handle();
    let draft = paste(4000);
    // The worker: the draft prefill, then the queued prompt, back to back.
    let worker = std::thread::spawn(move || {
        let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
        let pre = s.prefill_draft(&draft, &[], &never, &mut sink, &mut ui);
        let run = s.prompt(draft, Vec::new(), &mut sink, &mut ui);
        (pre, run.is_ok(), sink, s.stats().requests)
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("prefill started");
    handle.cancel_background();
    // A second cancel lands while the prompt may already hold the session: still harmless.
    handle.cancel_background();
    let (pre, ok, sink, requests) = worker.join().unwrap();
    assert!(!pre, "the prefill was cancelled");
    assert!(ok, "the prompt ran");
    assert!(
        !sink
            .0
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true })),
        "the prompt was not aborted"
    );
    assert_eq!(requests, 1);
    assert_eq!(
        *gated.log.lock().unwrap(),
        vec!["prefill-start", "prefill-cancelled", "chat"]
    );
}

/// An engine whose chat requests wait for a release (reported on `entered`).
struct HeldChat {
    inner: Arc<ScriptedEngine>,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Engine for HeldChat {
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
        if r.max_tokens != 0 {
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release.lock().unwrap().take() {
                let _ = rx.recv_timeout(Duration::from_secs(10));
            }
            if cancel.is_cancelled() {
                return Err(OxaError::new(codes::ABORTED, "aborted"));
            }
        }
        self.inner.chat(r, cancel, on)
    }
}

/// The lowest level of the race: a background cancel that lands while the user's prompt is
/// in flight leaves it running; `abort` at the same point still aborts it.
#[test]
fn background_cancel_during_a_prompt_spares_it_abort_does_not() {
    for abort in [false, true] {
        let env = TestEnv::new();
        let scripted = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
        let (tx, rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let held = Arc::new(HeldChat {
            inner: scripted.clone(),
            entered: Mutex::new(Some(tx)),
            release: Mutex::new(Some(release_rx)),
        });
        let mut s = open(&env, held, paste_on());
        let handle = s.handle();
        let worker = std::thread::spawn(move || {
            let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
            let _ = s.prompt("hello".into(), Vec::new(), &mut sink, &mut ui);
            sink
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("chat started");
        if abort {
            handle.abort();
        } else {
            handle.cancel_background();
        }
        release_tx.send(()).unwrap();
        let sink = worker.join().unwrap();
        let aborted = sink
            .0
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true }));
        assert_eq!(aborted, abort, "abort {abort}");
    }
}
