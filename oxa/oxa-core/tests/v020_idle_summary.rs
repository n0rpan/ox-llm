//! V020-OPTIMIZE #7 (lane l5-compact): the idle summary and the soft summary length target,
//! plus OXA-LOCAL-FIRST R4's purity check of the cut, over the scripted fake engine.
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_compact::prompt::{self, SummaryKind};
use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{append_entry, maybe_compact, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{check_w1, Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    rebase_why, AgentEvent, AssistantTurn, CancelToken, ChatRequest, CommandSpec, CompactReason, Engine,
    EngineInfo, EntryBody, FrozenPrefix, HeadlessUi, HookPoint, HookReply, Hooks, Mode, OxaError, Policy,
    StreamEvent, ThinkingLevel, Tool, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

fn policy(keep_tokens: u32) -> CompactPolicy {
    CompactPolicy {
        soft: u32::MAX - 1,
        hard: u32::MAX,
        keep_tokens,
        summary_max: 1024,
        enabled: true,
    }
}

fn idle_on() -> Settings {
    Settings::from_values(json!({"oxa": {"compaction": {"idleSummary": true}}}), json!({}))
}

fn build(env: &TestEnv, engine: Arc<dyn Engine>, compact: CompactPolicy, settings: Settings) -> SessionState {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    let sref = store.create(&cwd, None, None).expect("create session");
    let info = engine.info(None).expect("engine info");
    let mut prefix = FrozenPrefix {
        sections: vec![("preamble".to_string(), "You are a test agent.".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    };
    prefix.sha256 = Sha256::digest(prefix.hash_input()).into();
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        cancel: CancelToken::new(),
        cancel_retry: CancelToken::new(),
        busy: AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: AtomicBool::new(false),
        gate: Default::default(),
    });
    SessionState {
        goal: None,
        output_schema: None,
        engine,
        store,
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: cwd.clone(),
        all_tools: Vec::new(),
        selection: Default::default(),
        active: Toolset {
            direct: Vec::new(),
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
        mode: Mode::Workspace,
        compact,
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

fn state(env: &TestEnv, turns: Vec<ScriptTurn>, settings: Settings) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let st = build(env, engine, policy(1), settings);
    (st, eng)
}

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

/// One run; returns its events.
fn run(st: &mut SessionState) -> Vec<AgentEvent> {
    let mut sink = VecSink::default();
    assert_eq!(
        run_loop(st, &mut sink, &mut HeadlessUi::default()).expect("run_loop"),
        RunEnd::Stopped
    );
    sink.0
}

/// The run-end compaction step; returns (compacted, events).
fn settle(st: &mut SessionState) -> (bool, Vec<AgentEvent>) {
    let mut sink = VecSink::default();
    let done = maybe_compact(st, true, &mut sink, &mut HeadlessUi::default()).expect("maybe_compact");
    (done, sink.0)
}

/// `soft` such that the current context is at 85% of it (inside the idle band).
fn band(st: &mut SessionState) {
    let ctx = st.transcript.context();
    st.compact.soft = ctx * 100 / 85;
    assert!(ctx * 5 > st.compact.soft * 4 && ctx <= st.compact.soft);
}

/// `soft` just below the current context (a threshold compaction is due).
fn crossed(st: &mut SessionState) {
    st.compact.soft = st.transcript.context() - 1;
}

fn kinds(reqs: &[Recorded]) -> Vec<&str> {
    reqs.iter().map(|r| r.kind.as_str()).collect()
}

fn compactions(st: &SessionState) -> usize {
    st.tree
        .entries()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Compaction { .. }))
        .count()
}

fn misses(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::CacheMiss { .. }))
        .count()
}

fn is_compaction_event(e: &AgentEvent) -> bool {
    matches!(
        e,
        AgentEvent::CompactionStart { .. } | AgentEvent::CompactionEnd { .. }
    )
}

/// The `details` of the `CompactionEnd` result in `events`.
fn end_details(events: &[AgentEvent]) -> Value {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::CompactionEnd {
                result: Some(r),
                reason: CompactReason::Threshold,
                ..
            } => Some(r.details.clone()),
            _ => None,
        })
        .expect("a threshold CompactionEnd with a result")
}

/// A hook set that cancels the background request whenever it is armed (the user's input
/// arriving while the request runs).
struct InputArrives {
    shared: Arc<Shared>,
    armed: Arc<AtomicBool>,
}

impl Hooks for InputArrives {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p == HookPoint::BeforeProviderRequest && self.armed.load(Ordering::SeqCst) {
            self.shared.gate.bg_cancel.cancel();
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

// ---------------------------------------------------------------------------------------------
// (a) the idle summary is applied later without a request

#[test]
fn idle_summary_then_instant_compaction() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text("IDLE SUMMARY"),
            ScriptTurn::text("r2"),
        ],
        idle_on(),
    );
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    let wire1 = st.transcript.wire().to_vec();

    let (done, events) = settle(&mut st);
    assert!(!done, "no compaction below soft");
    assert!(
        !events.iter().any(is_compaction_event),
        "the idle summary is silent"
    );
    let reqs = eng.requests();
    assert_eq!(kinds(&reqs), ["chat", "compaction", "prefill"]);
    let summary = &reqs[1];
    assert!(
        summary.background && !summary.thinking,
        "background, thinking off"
    );
    assert_eq!(summary.messages[..wire1.len()], wire1[..], "the whole wire");
    assert_eq!(
        summary.messages.last().and_then(|m| m.content.clone()).as_deref(),
        Some(
            prompt::summary_instruction_with_target(
                SummaryKind::Initial,
                None,
                prompt::summary_target_words(st.settings.summary_target()),
            )
            .as_str()
        ),
        "the instruction a threshold compaction sends (the default target)"
    );
    assert_eq!(st.transcript.wire(), &wire1[..], "no rebase");
    assert_eq!(compactions(&st), 0);

    say(&mut st, "p2");
    let events = run(&mut st);
    assert_eq!(misses(&events), 0, "no cache miss after the idle summary");
    crossed(&mut st);
    let wire2 = st.transcript.wire().to_vec();

    let (done, events) = settle(&mut st);
    assert!(done, "soft crossed");
    let reqs = eng.requests();
    assert_eq!(
        kinds(&reqs),
        ["chat", "compaction", "prefill", "chat", "prefill"],
        "no summary request at compaction time"
    );
    // The idle cut (keep_tokens 1) was the last message then: r1. The tail runs from there.
    let mut expected = vec![WireMsg::user(prompt::summary_user_message("IDLE SUMMARY"))];
    expected.extend_from_slice(&wire2[1..]);
    assert_eq!(st.transcript.wire(), &expected[..]);
    assert_eq!(compactions(&st), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionStart {
            reason: CompactReason::Threshold
        }
    )));
    assert_eq!(end_details(&events)["precomputed"], json!(true));
    assert_eq!(st.stats.cache_misses, 0);
    let rebase = [(reqs[4].seq, "compaction")];
    assert!(
        check_w1(&reqs, &rebase).is_empty(),
        "W1 across the instant compaction"
    );
}

/// One idle summary per cut: a valid pending summary is not requested again.
#[test]
fn idle_summary_not_repeated_while_pending() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text("IDLE"),
            ScriptTurn::text("r2"),
            ScriptTurn::text("r3"),
        ],
        idle_on(),
    );
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    settle(&mut st);
    for p in ["p2", "p3"] {
        say(&mut st, p);
        run(&mut st);
        band(&mut st);
        let (done, _) = settle(&mut st);
        assert!(!done);
    }
    assert_eq!(
        kinds(&eng.requests()),
        ["chat", "compaction", "prefill", "chat", "chat"]
    );
}

// ---------------------------------------------------------------------------------------------
// (b) invalidation

/// After the idle summary, `change` happens; the next compaction must send its own request.
fn invalidated_by(change: impl Fn(&mut SessionState)) {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text("IDLE"),
            ScriptTurn::text("r2"),
            ScriptTurn::text("FRESH"),
        ],
        idle_on(),
    );
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    settle(&mut st);
    change(&mut st);
    say(&mut st, "p2");
    run(&mut st);
    crossed(&mut st);
    let (done, _) = settle(&mut st);
    assert!(done);
    let reqs = eng.requests();
    assert_eq!(
        kinds(&reqs),
        ["chat", "compaction", "prefill", "chat", "compaction", "prefill"]
    );
    let head = st.transcript.wire()[0].content.clone().unwrap_or_default();
    assert!(head.contains("FRESH") && !head.contains("IDLE"), "{head}");
}

#[test]
fn invalidated_by_rebase() {
    invalidated_by(|st| {
        let same = st.transcript.wire().to_vec();
        st.transcript.rebase(same, rebase_why::EXTENSION);
    });
}

#[test]
fn invalidated_by_prefix_epoch() {
    invalidated_by(|st| {
        st.prefix.sections[0].1 = "You are another test agent.".to_string();
        st.prefix.sha256 = Sha256::digest(st.prefix.hash_input()).into();
        st.stats.epochs += 1;
    });
}

#[test]
fn invalidated_by_model_switch() {
    invalidated_by(|st| st.model = "other-model".to_string());
}

#[test]
fn invalidated_by_session_switch() {
    invalidated_by(|st| {
        st.sref = st.store.create(&st.cwd, None, None).expect("new session");
    });
}

/// A pending summary whose kept tail grew above `2 * keep_tokens + soft / 5` is dropped.
#[test]
fn too_large_tail_compacts_normally() {
    let env = TestEnv::new();
    let big = "x".repeat(4000);
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text("IDLE"),
            ScriptTurn::text(&big),
            ScriptTurn::text("FRESH"),
        ],
        idle_on(),
    );
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    settle(&mut st);
    say(&mut st, "p2");
    run(&mut st);
    crossed(&mut st);
    let (done, _) = settle(&mut st);
    assert!(done);
    assert_eq!(
        kinds(&eng.requests()),
        ["chat", "compaction", "prefill", "chat", "compaction", "prefill"]
    );
    let head = st.transcript.wire()[0].content.clone().unwrap_or_default();
    assert!(head.contains("FRESH"), "{head}");
}

// ---------------------------------------------------------------------------------------------
// (c) user input cancels the idle summary

#[test]
fn input_cancels_idle_summary_then_normal_compaction() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text("r2"),
            ScriptTurn::text("NORMAL"),
        ],
        idle_on(),
    );
    let armed = Arc::new(AtomicBool::new(false));
    st.hooks.push(Arc::new(InputArrives {
        shared: st.shared.clone(),
        armed: armed.clone(),
    }));
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    armed.store(true, Ordering::SeqCst);
    let (done, events) = settle(&mut st);
    armed.store(false, Ordering::SeqCst);
    assert!(!done);
    assert!(
        !events
            .iter()
            .any(|e| is_compaction_event(e) || matches!(e, AgentEvent::Notice { .. })),
        "a cancelled idle summary is silent: {events:?}"
    );
    assert_eq!(
        kinds(&eng.requests()),
        ["chat", "compaction"],
        "sent, then cancelled"
    );

    say(&mut st, "p2");
    let events = run(&mut st);
    assert_eq!(
        misses(&events),
        0,
        "no cache miss after the cancelled idle summary"
    );
    crossed(&mut st);
    let (done, _) = settle(&mut st);
    assert!(done);
    assert_eq!(
        kinds(&eng.requests()),
        ["chat", "compaction", "chat", "compaction", "prefill"]
    );
    let head = st.transcript.wire()[0].content.clone().unwrap_or_default();
    assert!(head.contains("NORMAL"), "{head}");
}

/// Input already queued at run end: nothing is sent, nothing is shown.
#[test]
fn queued_input_skips_idle_summary() {
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, vec![ScriptTurn::text("r1")], idle_on());
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    st.shared.queue.lock().unwrap().follow_up.push("next".into());
    let (done, events) = settle(&mut st);
    assert!(!done && events.is_empty());
    assert_eq!(kinds(&eng.requests()), ["chat"]);
}

// ---------------------------------------------------------------------------------------------
// (d) setting off: today's sequence

#[test]
fn off_by_setting_is_todays_sequence() {
    let env = TestEnv::new();
    let off = Settings::from_values(json!({"oxa": {"compaction": {"idleSummary": false}}}), json!({}));
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text("r2"),
            ScriptTurn::text("SUMMARY"),
        ],
        off,
    );
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    let (done, events) = settle(&mut st);
    assert!(!done && events.is_empty());
    say(&mut st, "p2");
    run(&mut st);
    crossed(&mut st);
    let (done, events) = settle(&mut st);
    assert!(done);
    let reqs = eng.requests();
    assert_eq!(kinds(&reqs), ["chat", "chat", "compaction", "prefill"]);
    assert_eq!(
        reqs[2].messages.last().and_then(|m| m.content.clone()).as_deref(),
        Some(
            prompt::summary_instruction_with_target(
                SummaryKind::Initial,
                None,
                prompt::summary_target_words(st.settings.summary_target()),
            )
            .as_str()
        )
    );
    let details = end_details(&events);
    assert!(details.get("precomputed").is_none() && details.get("mechanical").is_none());
}

/// Explicitly off: no idle request either.
#[test]
fn off_by_setting() {
    let env = TestEnv::new();
    let off = Settings::from_values(json!({"oxa": {"compaction": {"idleSummary": false}}}), json!({}));
    let (mut st, eng) = state(&env, vec![ScriptTurn::text("r1")], off);
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    settle(&mut st);
    assert_eq!(kinds(&eng.requests()), ["chat"]);
}

// ---------------------------------------------------------------------------------------------
// Item 1: the soft length target in the request

#[test]
fn the_summary_request_carries_the_target() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![ScriptTurn::text("r1"), ScriptTurn::text("IDLE")],
        Settings::default(),
    );
    say(&mut st, "p1");
    run(&mut st);
    band(&mut st);
    settle(&mut st);
    let reqs = eng.requests();
    assert_eq!(kinds(&reqs), ["chat", "compaction", "prefill"], "idle summary on");
    assert_eq!(
        reqs[1].max_tokens, 1024,
        "the target is soft: max_tokens unchanged"
    );
    assert_eq!(
        reqs[1].messages.last().and_then(|m| m.content.clone()).as_deref(),
        Some(prompt::summary_instruction_with_target(SummaryKind::Initial, None, 550).as_str())
    );
}

// ---------------------------------------------------------------------------------------------
// Item 4 (R4 first half): the cut and the kept tail are pure functions of the wire; the summary
// request is sent with thinking off (and temperature 0 is in every request body the engine
// builder writes: oxa-engine's a2 goldens).

fn one_compaction() -> (Vec<WireMsg>, Vec<Recorded>) {
    let env = TestEnv::new();
    let off = Settings::from_values(json!({"oxa": {"compaction": {"idleSummary": false}}}), json!({}));
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("first reply with some words"),
            ScriptTurn::text("second reply"),
            ScriptTurn::text("third reply"),
            ScriptTurn::text("SUM"),
        ],
        off,
    );
    st.compact.keep_tokens = 30;
    for p in ["one", "two", "three"] {
        say(&mut st, p);
        run(&mut st);
    }
    crossed(&mut st);
    assert!(settle(&mut st).0);
    (st.transcript.wire().to_vec(), eng.requests())
}

#[test]
fn cut_and_tail_are_pure() {
    let (w1, r1) = one_compaction();
    let (w2, r2) = one_compaction();
    assert_eq!(w1, w2, "same wire bytes after compaction");
    let s1 = r1
        .iter()
        .find(|r| r.kind == "compaction")
        .expect("summary request");
    let s2 = r2
        .iter()
        .find(|r| r.kind == "compaction")
        .expect("summary request");
    assert_eq!(s1.messages, s2.messages, "same summary request bytes");
    assert!(!s1.thinking, "summary request with thinking off");
    assert!(w1.len() > 1 && w1.len() < s1.messages.len(), "a real cut");
}

// ---------------------------------------------------------------------------------------------
// Numbers: a synthetic 40-turn session, idle summary off vs on.

/// Answers every generating request on demand: a chat reply of ~300 bytes, a summary of ~200.
struct AutoEngine {
    inner: Arc<ScriptedEngine>,
    n: AtomicU32,
}

impl Engine for AutoEngine {
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
        if r.max_tokens > 0 {
            let n = self.n.fetch_add(1, Ordering::SeqCst);
            let text = if r.hints.kind == oxa_proto::TurnKind::Compaction {
                format!("SUMMARY {n} {}", "s".repeat(190))
            } else {
                format!("reply {n} {}", "r".repeat(290))
            };
            self.inner.push(ScriptTurn::text(&text));
        }
        self.inner.chat(r, cancel, on)
    }
}

#[derive(Debug, Default)]
struct Arm {
    summary_requests: Vec<u32>,
    compactions: u32,
    waited: u32,
    precomputed: u32,
    misses: u32,
    requests: usize,
}

fn forty_turns(idle: bool) -> Arm {
    let env = TestEnv::new();
    let inner = Arc::new(ScriptedEngine::new(Vec::new()));
    let engine: Arc<dyn Engine> = Arc::new(AutoEngine {
        inner: inner.clone(),
        n: AtomicU32::new(0),
    });
    let settings = Settings::from_values(json!({"oxa": {"compaction": {"idleSummary": idle}}}), json!({}));
    let mut st = build(
        &env,
        engine,
        CompactPolicy {
            soft: 4000,
            hard: 100_000,
            keep_tokens: 600,
            summary_max: 1024,
            enabled: true,
        },
        settings,
    );
    let mut arm = Arm::default();
    for i in 0..40 {
        say(&mut st, &format!("prompt {i}: {}", "p".repeat(50)));
        arm.misses += misses(&run(&mut st)) as u32;
        let before = inner.requests().len();
        let (done, events) = settle(&mut st);
        let sent: Vec<Recorded> = inner.requests()[before..].to_vec();
        let summary_sent = sent.iter().any(|r| r.kind == "compaction");
        if done {
            arm.compactions += 1;
            if summary_sent {
                arm.waited += 1;
            }
            if events.iter().any(|e| {
                matches!(e, AgentEvent::CompactionEnd { result: Some(r), .. } if r.details["precomputed"] == json!(true))
            }) {
                arm.precomputed += 1;
            }
        }
        for r in sent.iter().filter(|r| r.kind == "compaction") {
            arm.summary_requests.push(r.usage.map_or(0, |u| u.prompt));
        }
    }
    arm.requests = inner.requests().len();
    arm
}

#[test]
fn forty_turn_session_numbers() {
    let off = forty_turns(false);
    let on = forty_turns(true);
    println!("idle summary OFF: {off:?}");
    println!("idle summary ON:  {on:?}");
    let target = prompt::summary_instruction_with_target(SummaryKind::Initial, None, 550);
    let plain = prompt::summary_instruction(SummaryKind::Initial, None);
    println!(
        "summary instruction bytes: {} without target, {} with (550 words)",
        plain.len(),
        target.len()
    );
    assert_eq!(off.misses + on.misses, 0, "no cache miss in either arm");
    assert!(off.compactions > 0 && off.waited == off.compactions && off.precomputed == 0);
    assert!(on.precomputed > 0, "the idle summary was used");
    assert_eq!(on.waited + on.precomputed, on.compactions);
}
