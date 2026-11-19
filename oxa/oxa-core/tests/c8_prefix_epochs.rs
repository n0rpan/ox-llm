//! C8 acceptance tests: Prefix epochs.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{
    append_entry, open_epoch, reload, replay_prefix, run_loop, Queue, RunEnd, SessionState, Shared, Stats,
    Toolset,
};
use oxa_fake::{check_w3, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::ThinkingLevel;
use oxa_proto::{
    codes, AgentEvent, CancelToken, CommandSpec, Engine, EntryBody, FrozenPrefix, HeadlessUi, HookReply,
    Hooks, Mode, OxaError, Ui, Usage, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

/// A hook set whose prompt section the test can change between runs.
struct SectionHook {
    section: Mutex<String>,
}

impl SectionHook {
    fn new(text: &str) -> Arc<SectionHook> {
        Arc::new(SectionHook {
            section: Mutex::new(text.to_string()),
        })
    }
    fn set(&self, text: &str) {
        *self.section.lock().unwrap() = text.to_string();
    }
}

impl Hooks for SectionHook {
    fn fire(&self, _p: oxa_proto::HookPoint, _payload: &oxa_proto::Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn oxa_proto::Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(
            codes::NOT_FOUND,
            format!("unknown command: /{name}"),
        ))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        vec![("notes".to_string(), self.section.lock().unwrap().clone())]
    }
}

/// A session state over a `MemStore` and a scripted engine, in the test's work directory.
fn fixture(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Vec<Arc<dyn Hooks>>) -> SessionState {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    let sref = store.create(&cwd, None, None).expect("create session");
    let info = engine.info(None).expect("engine info");
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        cancel: CancelToken::new(),
        cancel_retry: CancelToken::new(),
        wait_wake: CancelToken::new(),
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
    });
    SessionState {
        goal: None,
        output_schema: None,
        engine,
        store,
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::default(),
        dirs: Dirs::under(env.root()),
        cwd: cwd.clone(),
        all_tools: Vec::new(),
        selection: Default::default(),
        active: Toolset::default(),
        resources: Resources::default(),
        sref,
        tree: Tree::new(Vec::new()),
        transcript: Transcript::default(),
        prefix: FrozenPrefix {
            sections: Vec::new(),
            tools_json: "[]".to_string(),
            sha256: [0u8; 32],
        },
        info,
        model: "fake-model".to_string(),
        thinking: ThinkingLevel::Off,
        mode: Mode::Workspace,
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

fn no_engine() -> Arc<dyn Engine> {
    Arc::new(ScriptedEngine::new(Vec::new()))
}

/// Number of `Prefix` entries in the session tree.
fn prefix_entries(st: &SessionState) -> usize {
    st.tree
        .entries()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Prefix(_)))
        .count()
}

fn epoch_events(sink: &VecSink) -> Vec<(String, u32)> {
    sink.0
        .iter()
        .filter_map(|e| match e {
            AgentEvent::PrefixEpoch {
                reason,
                reprefill_tokens,
                ..
            } => Some((reason.clone(), *reprefill_tokens)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Tests

/// returns false, no entry
#[test]
fn reload_without_change_no_epoch() {
    let env = TestEnv::new();
    let mut st = fixture(&env, no_engine(), Vec::new());
    let mut ui = HeadlessUi { notes: Vec::new() };
    let mut sink = VecSink(Vec::new());

    // The first reload reads the resources and opens the first epoch.
    assert!(
        reload(&mut st, &mut sink, &mut ui).expect("first reload"),
        "first reload opens an epoch"
    );
    let entries = prefix_entries(&st);
    let sha = st.prefix.sha256;

    let mut again = VecSink(Vec::new());
    let opened = reload(&mut st, &mut again, &mut ui).expect("second reload");
    assert!(!opened, "nothing changed: no epoch");
    assert_eq!(prefix_entries(&st), entries, "no Prefix entry appended");
    assert_eq!(st.prefix.sha256, sha, "prefix bytes unchanged");
    assert!(epoch_events(&again).is_empty(), "no PrefixEpoch event");
}

/// PrefixEpoch event with cost
#[test]
fn change_is_one_prefix_entry_and_notice() {
    let env = TestEnv::new();
    let mut st = fixture(&env, no_engine(), Vec::new());
    let mut ui = HeadlessUi { notes: Vec::new() };
    env.write("work/AGENTS.md", "project rules v1\n");
    let mut sink = VecSink(Vec::new());
    assert!(reload(&mut st, &mut sink, &mut ui).expect("first epoch"));
    let entries = prefix_entries(&st);
    let rebases = st.transcript.rebases().len();

    // A reply has been received: the context to prefill again is prompt + completion.
    st.transcript.set_usage(&Usage {
        prompt: 500,
        completion: 20,
        cached: 0,
        cached_missing: false,
        unreported: false,
    });
    env.write("work/AGENTS.md", "project rules v2\n");
    let mut sink = VecSink(Vec::new());
    assert!(reload(&mut st, &mut sink, &mut ui).expect("changed reload"));

    assert_eq!(prefix_entries(&st), entries + 1, "exactly one Prefix entry");
    assert_eq!(
        st.transcript.rebases().len(),
        rebases + 1,
        "exactly one epoch rebase"
    );
    assert_eq!(st.transcript.rebases().last().map(|r| r.why), Some("epoch"));
    assert_eq!(
        epoch_events(&sink),
        vec![("reload".to_string(), 520)],
        "PrefixEpoch carries the re-prefill cost"
    );
    assert!(
        sink.0.iter().any(|e| matches!(e, AgentEvent::Notice { .. })),
        "a Notice accompanies the epoch"
    );
    assert!(st.prefix.system_text().contains("project rules v2"));
}

/// changed AGENTS.md not picked up until /reload
#[test]
fn resume_replays_stored_bytes() {
    let env = TestEnv::new();
    let mut st = fixture(&env, no_engine(), Vec::new());
    let mut ui = HeadlessUi { notes: Vec::new() };
    env.write("work/AGENTS.md", "v1 rules\n");
    let mut sink = VecSink(Vec::new());
    reload(&mut st, &mut sink, &mut ui).expect("epoch with v1");

    // The file changes on disk; the session is resumed from its entries.
    env.write("work/AGENTS.md", "v2 rules\n");
    let branch = st.tree.branch(st.tree.leaf());
    let replayed = replay_prefix(&branch).expect("stored prefix");
    assert_eq!(replayed, st.prefix, "resume replays the stored bytes");
    assert!(replayed.system_text().contains("v1 rules"));
    assert!(
        !replayed.system_text().contains("v2 rules"),
        "changed file not picked up on resume"
    );

    // The transcript fold reproduces the same prefix.
    let mut t = Transcript::default();
    for e in &branch {
        t.apply(e);
    }
    assert_eq!(t.prefix(), Some(&st.prefix));

    // Only /reload picks the change up.
    assert!(reload(&mut st, &mut VecSink(Vec::new()), &mut ui).expect("reload"));
    assert!(st.prefix.system_text().contains("v2 rules"));
}

/// busy
#[test]
fn epoch_refused_during_run() {
    let env = TestEnv::new();
    let mut st = fixture(&env, no_engine(), Vec::new());
    let mut ui = HeadlessUi { notes: Vec::new() };
    let mut sink = VecSink(Vec::new());
    reload(&mut st, &mut sink, &mut ui).expect("first epoch");
    let entries = prefix_entries(&st);
    env.write("work/AGENTS.md", "changed while running\n");

    st.shared.busy.store(true, Ordering::SeqCst);
    let e = open_epoch(&mut st, "manual", true, &mut VecSink(Vec::new())).expect_err("busy");
    assert_eq!(e.code, codes::BUSY);
    let e = reload(&mut st, &mut VecSink(Vec::new()), &mut ui).expect_err("busy");
    assert_eq!(e.code, codes::BUSY);
    assert_eq!(prefix_entries(&st), entries, "a refused epoch writes nothing");

    st.shared.busy.store(false, Ordering::SeqCst);
    assert!(reload(&mut st, &mut VecSink(Vec::new()), &mut ui).expect("idle again"));
}

/// via Session::set_model
///
/// `Session::set_model` (C1) changes `st.model` and opens an epoch with `force = true`; the epoch
/// door itself is what is checked here. The model name is not in the prefix bytes (W3).
#[test]
fn model_switch_is_epoch() {
    let env = TestEnv::new();
    let mut st = fixture(&env, no_engine(), Vec::new());
    let mut ui = HeadlessUi { notes: Vec::new() };
    reload(&mut st, &mut VecSink(Vec::new()), &mut ui).expect("first epoch");
    let entries = prefix_entries(&st);
    let sha = st.prefix.sha256;

    st.model = "other-model".to_string();
    let mut sink = VecSink(Vec::new());
    assert!(open_epoch(&mut st, "model", true, &mut sink).expect("model epoch"));
    assert_eq!(prefix_entries(&st), entries + 1);
    assert_eq!(st.prefix.sha256, sha, "same bytes, new epoch");
    assert_eq!(epoch_events(&sink), vec![("model".to_string(), 0)]);
    assert_eq!(st.transcript.rebases().last().map(|r| r.why), Some("epoch"));

    // Without force and without a change, nothing happens.
    assert!(!open_epoch(&mut st, "again", false, &mut VecSink(Vec::new())).expect("no change"));
}

/// check_w3 green over a run with hooks
#[test]
fn w3_constant_within_epoch() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("first reply"),
        ScriptTurn::text("second reply"),
    ]));
    let hook = SectionHook::new("notes v1");
    let mut st = fixture(&env, engine.clone(), vec![hook.clone() as Arc<dyn Hooks>]);
    let mut ui = HeadlessUi { notes: Vec::new() };
    open_epoch(&mut st, "start", true, &mut VecSink(Vec::new())).expect("first epoch");
    let sha = st.prefix.sha256;

    let mut sink = VecSink(Vec::new());
    append_entry(
        &mut st,
        EntryBody::Msg {
            wire: WireMsg::user("hello"),
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("user");
    assert_eq!(
        run_loop(&mut st, &mut sink, &mut ui).expect("run 1"),
        RunEnd::Stopped
    );

    // The hook changes its section during the epoch: the prefix must not follow until an epoch opens.
    hook.set("notes v2, changed mid-epoch");
    append_entry(
        &mut st,
        EntryBody::Msg {
            wire: WireMsg::user("again"),
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("user");
    assert_eq!(
        run_loop(&mut st, &mut sink, &mut ui).expect("run 2"),
        RunEnd::Stopped
    );

    let reqs = engine.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[0].system, reqs[1].system,
        "system text constant within the epoch"
    );
    assert!(reqs[0].system.contains("notes v1"));
    assert_eq!(st.prefix.sha256, sha);
    let violations = check_w3(&reqs, &[]);
    assert!(violations.is_empty(), "W3 violations: {}", violations.len());
}
