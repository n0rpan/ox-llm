//! parity-local (O-167): under profile `local` a failed model summary is replaced by the
//! mechanical one (R4). An extension still learns that the model summary failed: the
//! compaction happened, so `session_compact` fires with `mechanical: true` and `summaryError`
//! (the failure's text); `session_compact_failed` does not fire (it means the wire is
//! unchanged). Under `pi` the failure fires `session_compact_failed` as before and the
//! `session_compact` payload of a normal compaction has no new key.
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{append_entry, maybe_compact, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    CancelToken, CommandSpec, Engine, EntryBody, FrozenPrefix, HeadlessUi, HookPoint, HookReply, Hooks, Mode,
    OxaError, Policy, Profile, ThinkingLevel, Tool, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<(HookPoint, Value)>>,
}

impl Recorder {
    fn of(&self, p: HookPoint) -> Vec<Value> {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .filter(|(q, _)| *q == p)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

impl Hooks for Recorder {
    fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        self.seen.lock().unwrap().push((p, payload.clone()));
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(oxa_proto::codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

fn build(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Arc<Recorder>, profile: Profile) -> SessionState {
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
    let mut st = SessionState {
        engine,
        store,
        hooks: vec![hooks as Arc<dyn Hooks>],
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(json!({"oxa": {"compaction": {"idleSummary": false}}}), json!({})),
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
        compact: CompactPolicy {
            soft: u32::MAX - 1,
            hard: u32::MAX,
            keep_tokens: 1,
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
    };
    st.stats.profile = profile;
    st
}

fn push(st: &mut SessionState, wire: WireMsg) {
    append_entry(
        st,
        EntryBody::Msg {
            wire,
            parts: Vec::new(),
        },
    )
    .expect("append");
}

/// A threshold compaction whose summary request is answered by `summary`; returns the hooks
/// seen and whether it compacted.
fn compact_with(summary: ScriptTurn, profile: Profile) -> (Arc<Recorder>, bool) {
    let env = TestEnv::new();
    let rec = Arc::new(Recorder::default());
    let engine: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("done two"), summary]));
    let mut st = build(&env, engine, rec.clone(), profile);
    push(&mut st, WireMsg::user("task one: fix the parser"));
    push(&mut st, WireMsg::assistant("done one"));
    push(&mut st, WireMsg::user("task two"));
    let mut sink = VecSink::default();
    assert_eq!(
        run_loop(&mut st, &mut sink, &mut HeadlessUi::default()).expect("run_loop"),
        RunEnd::Stopped
    );
    st.compact.soft = st.transcript.context() - 1;
    let done = maybe_compact(&mut st, true, &mut sink, &mut HeadlessUi::default()).expect("maybe_compact");
    (rec, done)
}

fn fault() -> ScriptTurn {
    ScriptTurn::fault(Fault::Http {
        status: 400,
        body: "bad request".into(),
    })
}

#[test]
fn local_mechanical_fallback_fires_session_compact_with_the_failure() {
    let (rec, done) = compact_with(fault(), Profile::Local);
    assert!(done, "the session compacts without the model");
    assert!(
        rec.of(HookPoint::SessionCompactFailed).is_empty(),
        "the wire was compacted: session_compact_failed must not fire"
    );
    let compact = rec.of(HookPoint::SessionCompact);
    assert_eq!(compact.len(), 1, "session_compact fires once");
    assert_eq!(compact[0]["mechanical"], json!(true));
    let err = compact[0]["summaryError"]
        .as_str()
        .expect("summaryError is the failure text");
    assert!(!err.is_empty());
    assert_eq!(compact[0]["reason"], json!("threshold"));
}

#[test]
fn pi_failure_still_fires_session_compact_failed() {
    let (rec, done) = compact_with(fault(), Profile::Pi);
    assert!(!done);
    assert!(rec.of(HookPoint::SessionCompact).is_empty());
    assert_eq!(rec.of(HookPoint::SessionCompactFailed).len(), 1);
}

#[test]
fn a_model_summary_adds_no_key_under_either_profile() {
    for profile in [Profile::Pi, Profile::Local] {
        let (rec, done) = compact_with(ScriptTurn::text("## Goal\nsummary"), profile);
        assert!(done, "{profile:?}");
        let compact = rec.of(HookPoint::SessionCompact);
        assert_eq!(compact.len(), 1, "{profile:?}");
        assert!(compact[0].get("mechanical").is_none(), "{profile:?}");
        assert!(compact[0].get("summaryError").is_none(), "{profile:?}");
    }
}
