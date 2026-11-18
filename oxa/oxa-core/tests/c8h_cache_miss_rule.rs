//! Wave H, lane cache (OXA-H-TINYLOOP 1): the cache-miss rule. A reply whose `cached` is below
//! the expectation raises `cache_miss` whatever the server advertises: the first request after
//! a compaction, a fork, a resume (an engine restart), a model switch (epoch) and after the boot
//! preload; a true cold start stays silent. The rule itself: `agent_loop::expected_cached`.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{
    append_entry, boot_preload, compact_now, fork_at, run_loop, Queue, RunEnd, SessionState, Shared, Stats,
    Toolset,
};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    miss_cause, AgentEvent, CancelToken, CompactReason, Engine, EntryBody, FrozenPrefix, HeadlessUi, Hooks,
    Mode, Policy, Role, ThinkingLevel, Tool, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

fn build(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    tools: Vec<Arc<dyn Tool>>,
    compact: CompactPolicy,
    settings: Settings,
) -> SessionState {
    let hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
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
        settings,
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

/// A session over a scripted engine. The engine is returned for inspection of the requests.
#[allow(dead_code)]
fn state(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    compact: CompactPolicy,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    (build(env, engine, Vec::new(), compact, Settings::default()), eng)
}

/// Append a user message the way `Session::prompt` does.
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

/// Append one wire message of any role as a `Msg` entry.
#[allow(dead_code)]
fn push(st: &mut SessionState, wire: WireMsg) {
    append_entry(
        st,
        EntryBody::Msg {
            wire,
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("append message");
}

#[allow(dead_code)]
fn run(st: &mut SessionState) -> RunEnd {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    run_loop(st, &mut sink, &mut ui).expect("run_loop")
}

fn loose() -> CompactPolicy {
    CompactPolicy {
        soft: u32::MAX,
        hard: u32::MAX,
        keep_tokens: 1,
        summary_max: 1024,
        enabled: true,
    }
}

fn session(env: &TestEnv, turns: Vec<ScriptTurn>, warming: bool) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let settings = if warming {
        Settings::default()
    } else {
        Settings::from_values(serde_json::json!({"cacheWarming": "off"}), serde_json::json!({}))
    };
    (build(env, engine, Vec::new(), loose(), settings), eng)
}

/// Run the loop on `text`; the `cache_miss` events as (expected, cached, cause).
fn turn(st: &mut SessionState, text: &str) -> Vec<(u32, u32, &'static str)> {
    say(st, text);
    let mut sink = VecSink::default();
    assert_eq!(
        run_loop(st, &mut sink, &mut HeadlessUi::default()).expect("run"),
        RunEnd::Stopped
    );
    misses(&sink)
}

fn misses(sink: &VecSink) -> Vec<(u32, u32, &'static str)> {
    sink.0
        .iter()
        .filter_map(|e| match e {
            AgentEvent::CacheMiss {
                expected,
                cached,
                cause,
            } => Some((*expected, *cached, *cause)),
            _ => None,
        })
        .collect()
}

fn cold(text: &str) -> ScriptTurn {
    ScriptTurn {
        fault: Some(Fault::Cold),
        ..ScriptTurn::text(text)
    }
}

/// The anchor the rule expects after a rebase: system text + tools, by the engine's count.
fn anchor(st: &SessionState) -> u32 {
    (st.prefix.system_text().len() + st.prefix.tools_json.len()) as u32
}

#[test]
fn true_cold_start_is_silent_and_warm_turns_hit() {
    let env = TestEnv::new();
    let (mut st, _eng) = session(&env, vec![ScriptTurn::text("a"), ScriptTurn::text("b")], false);
    assert!(
        turn(&mut st, "one").is_empty(),
        "a new session's first request expects nothing"
    );
    assert!(turn(&mut st, "two").is_empty(), "a pure append hits");
}

/// Real hardware (tiny loop): a server without a prefix cache served the first request after a
/// compaction cold (0 of 3563) and oxa raised nothing (the rebase zeroed the expectation).
#[test]
fn cold_after_compaction_is_reported() {
    let env = TestEnv::new();
    let (mut st, _eng) = session(
        &env,
        vec![
            ScriptTurn::text("a"),
            ScriptTurn::text("b"),
            ScriptTurn::text("SUMMARY"),
            cold("c"),
        ],
        false,
    );
    turn(&mut st, "one");
    turn(&mut st, "two");
    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("compact");
    let m = turn(&mut st, "three");
    assert_eq!(m, vec![(anchor(&st), 0, miss_cause::COMPACTION)]);
}

#[test]
fn warm_after_compaction_is_silent() {
    let env = TestEnv::new();
    let (mut st, _eng) = session(
        &env,
        vec![
            ScriptTurn::text("a"),
            ScriptTurn::text("b"),
            ScriptTurn::text("SUMMARY"),
            ScriptTurn::text("c"),
            ScriptTurn::text("d"),
        ],
        false,
    );
    turn(&mut st, "one");
    turn(&mut st, "two");
    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("compact");
    assert!(
        turn(&mut st, "three").is_empty(),
        "the anchor (and more) was reused"
    );
    // A2 (OXE-POSTCOMPACT-MISS): the first PURE append after the post-compaction turn continues
    // the live state exactly (C3); an engine that misses it here is reported, not expected.
    let expected = st.transcript.expected_cache();
    assert!(expected > anchor(&st));
    assert!(
        turn(&mut st, "four").is_empty(),
        "a pure append after the post-compaction turn hits"
    );
}

/// The idle prefill after a compaction is checked too (it is the first request over the new
/// head when the engine advertises H6).
#[test]
fn cold_idle_prefill_after_compaction_is_reported() {
    let env = TestEnv::new();
    let (mut st, eng) = session(
        &env,
        vec![
            ScriptTurn::text("a"),
            ScriptTurn::text("b"),
            ScriptTurn::text("SUMMARY"),
        ],
        true,
    );
    turn(&mut st, "one");
    turn(&mut st, "two");
    // The engine loses everything between the summary and the prefill (a restart).
    struct Restart(Arc<ScriptedEngine>, VecSink);
    impl oxa_proto::EventSink for Restart {
        fn emit(&mut self, e: &AgentEvent) {
            if matches!(e, AgentEvent::CompactionEnd { .. }) {
                self.0.restart();
            }
            self.1.emit(e);
        }
    }
    let mut tap = Restart(eng.clone(), VecSink::default());
    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut tap,
        &mut HeadlessUi::default(),
    )
    .expect("compact");
    let sink = tap.1;
    assert!(
        eng.requests().iter().any(|r| r.kind == "prefill"),
        "an idle prefill was sent"
    );
    assert_eq!(misses(&sink), vec![(anchor(&st), 0, miss_cause::COMPACTION)]);
}

#[test]
fn fork_reuses_the_shared_anchor_and_a_cold_fork_is_reported() {
    for (last, want_miss) in [(ScriptTurn::text("c"), false), (cold("c"), true)] {
        let env = TestEnv::new();
        let (mut st, _eng) = session(
            &env,
            vec![ScriptTurn::text("a"), ScriptTurn::text("b"), last],
            false,
        );
        turn(&mut st, "one");
        turn(&mut st, "two");
        let u2 = st
            .tree
            .entries()
            .iter()
            .find(|e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.role == Role::User && wire.content.as_deref() == Some("two")))
            .map(|e| e.id.clone())
            .expect("u2");
        fork_at(&mut st, &u2, &mut HeadlessUi::default()).expect("fork");
        let m = turn(&mut st, "three");
        if want_miss {
            assert_eq!(m, vec![(anchor(&st), 0, miss_cause::LEAF_MOVE)]);
        } else {
            assert!(
                m.is_empty(),
                "a fork is a new session id, but the engine shares the anchor: {m:?}"
            );
        }
    }
}

/// A resumed session (the transcript folded from its entries, a new process: no request sent
/// yet) expects what its last request left; an engine that lost it is a `cold_start` miss.
#[test]
fn resume_on_a_restarted_engine_is_reported() {
    let env = TestEnv::new();
    let (mut st, eng) = session(&env, vec![ScriptTurn::text("a"), ScriptTurn::text("b")], false);
    turn(&mut st, "one");
    let expected = st.transcript.expected_cache();
    // The "new process": the same entries folded again, no request yet; the engine restarted.
    let branch: Vec<_> = st.tree.branch(st.tree.leaf()).into_iter().cloned().collect();
    st.transcript = Transcript::project(&branch);
    st.stats = Stats::default();
    eng.restart();
    let m = turn(&mut st, "two");
    assert_eq!(m, vec![(expected, 0, miss_cause::COLD_START)]);
}

/// A model switch is a new epoch (a rebase): the anchor is expected.
#[test]
fn cold_after_a_new_epoch_is_reported() {
    let env = TestEnv::new();
    let (mut st, _eng) = session(&env, vec![ScriptTurn::text("a"), cold("b")], false);
    // A session starts with its epoch entry (`start_new`); the switch appends the next one.
    let prefix = st.prefix.clone();
    append_entry(&mut st, EntryBody::Prefix(prefix.clone())).expect("first epoch");
    turn(&mut st, "one");
    append_entry(&mut st, EntryBody::Prefix(prefix)).expect("epoch");
    let m = turn(&mut st, "two");
    assert_eq!(m, vec![(anchor(&st), 0, miss_cause::EPOCH)]);
}

/// The boot preload sets the first prompt's expectation (the preloaded prefix).
#[test]
fn first_prompt_after_the_boot_preload_is_checked() {
    let env = TestEnv::new();
    let (mut st, eng) = session(&env, vec![ScriptTurn::text("a")], true);
    assert!(boot_preload(
        &mut st,
        &mut VecSink::default(),
        &mut HeadlessUi::default()
    ));
    let preload = eng.requests()[0].usage.expect("usage").prompt;
    eng.restart();
    let m = turn(&mut st, "one");
    assert_eq!(m, vec![(preload, 0, miss_cause::COLD_START)]);
}

/// A2 (OXE-POSTCOMPACT-MISS) over HTTP: the strict expectation holds on the correct engine (the
/// fake's default), and the engine as measured today (`rewrite_resume_block`: the append after
/// a rewrite resumes block-granular) is reported as a miss on that append, not hidden.
#[test]
fn post_compaction_append_over_http() {
    for (mode, want) in [(None, false), (Some(256usize), true)] {
        let env = TestEnv::new();
        let srv = oxa_fake::FakeServer::start(
            vec![
                ScriptTurn::text(&"a".repeat(300)),
                ScriptTurn::text(&"b".repeat(300)),
                ScriptTurn::text("SUMMARY"),
                ScriptTurn::text("c"),
                ScriptTurn::text("d"),
            ],
            oxa_fake::FakeOpts {
                rewrite_resume_block: mode,
                ..oxa_fake::FakeOpts::default()
            },
        )
        .unwrap();
        let engine: Arc<dyn Engine> = Arc::new(oxa_engine::HttpEngine::new(&srv.url(), None).unwrap());
        let settings =
            Settings::from_values(serde_json::json!({"cacheWarming": "off"}), serde_json::json!({}));
        let mut st = build(&env, engine, Vec::new(), loose(), settings);
        turn(&mut st, "one");
        turn(&mut st, "two");
        compact_now(
            &mut st,
            None,
            CompactReason::Manual,
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .expect("compact");
        turn(&mut st, "three");
        let expected = st.transcript.expected_cache();
        let m = turn(&mut st, "four");
        let last = srv.requests().last().cloned().unwrap();
        if want {
            assert_eq!(m.len(), 1, "{m:?}");
            assert_eq!(m[0].0, expected);
            assert_eq!(m[0].2, miss_cause::ENGINE_EVICTED);
            assert_eq!(last.usage.unwrap().cached % 256, 0, "block-granular");
        } else {
            assert!(m.is_empty(), "the correct engine continues the live state: {m:?}");
            assert_eq!(last.usage.unwrap().cached, expected);
        }
    }
}
