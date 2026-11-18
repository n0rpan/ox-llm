//! R4: a failed model summary is replaced by the mechanical one. An extension still learns that
//! the model summary failed: the compaction happened, so `session_compact` fires with
//! `mechanical: true` and `summaryError` (the failure's text); `session_compact_failed` does
//! not fire (it means the wire is unchanged).
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{
    append_entry, compact_now, maybe_compact, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset,
};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, AssistantTurn, CancelToken, CommandSpec, CompactReason, Engine, EngineInfo, EntryBody,
    FrozenPrefix, HeadlessUi, HookPoint, HookReply, Hooks, Mode, OxaError, Policy, StreamEvent,
    ThinkingLevel, Tool, Ui, Value, VecSink, WireMsg,
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

fn build(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Arc<Recorder>) -> SessionState {
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
        wait_wake: CancelToken::new(),
        busy: AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: AtomicBool::new(false),
        gate: Default::default(),
    });
    let st = SessionState {
        goal: None,
        output_schema: None,
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
        jobs: oxa_core::fresh_jobs(),
    };
    st
}

fn push(st: &mut SessionState, wire: WireMsg) {
    append_entry(
        st,
        EntryBody::Msg {
            wire,
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("append");
}

/// A threshold compaction whose summary request is answered by `summary`; returns the hooks
/// seen, whether it compacted, and the reported `estimated_tokens_after` (when it compacted).
fn compact_with(summary: ScriptTurn) -> (Arc<Recorder>, bool, Option<u32>) {
    let env = TestEnv::new();
    let rec = Arc::new(Recorder::default());
    let engine: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("done two"), summary]));
    let mut st = build(&env, engine, rec.clone());
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
    let after = sink.0.iter().find_map(|event| match event {
        AgentEvent::CompactionEnd {
            result: Some(result), ..
        } => Some(result.estimated_tokens_after),
        _ => None,
    });
    if let Some(after) = after {
        assert!(after > 0, "rebased context still contains the prefix and summary");
        assert_eq!(st.stats.context_tokens, after);
    }
    (rec, done, after)
}

fn fault() -> ScriptTurn {
    ScriptTurn::fault(Fault::Http {
        status: 400,
        body: "bad request".into(),
    })
}

#[test]
fn local_mechanical_fallback_fires_session_compact_with_the_failure() {
    let (rec, done, after) = compact_with(fault());
    assert!(done, "the session compacts without the model");
    assert!(after.is_some(), "a compaction reports its estimated tokens after");
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
fn a_model_summary_adds_no_key() {
    let (rec, done, after) = compact_with(ScriptTurn::text("## Goal\nsummary"));
    assert!(done);
    assert!(after.is_some());
    let compact = rec.of(HookPoint::SessionCompact);
    assert_eq!(compact.len(), 1);
    assert!(compact[0].get("mechanical").is_none());
    assert!(compact[0].get("summaryError").is_none());
}

/// An engine that cannot count a prompt exactly (`count_prompt` -> `Ok(None)`), so the
/// post-compaction estimate falls back to the local tokenizer.
struct NoCount(Arc<dyn Engine>);

impl Engine for NoCount {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.0.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.0.count_tokens(text)
    }
    fn chat(
        &self,
        r: &oxa_proto::ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        self.0.chat(r, cancel, on)
    }
    fn count_prompt(&self, _r: &oxa_proto::ChatRequest) -> Result<Option<u64>, OxaError> {
        Ok(None)
    }
}

/// A manual compaction after a branch rebase (which zeroes the transcript usage, so
/// `tokens_before` is 0) with `count_prompt` unavailable must still report a nonzero
/// `estimated_tokens_after`: the clamp to `tokens_before` must not force it to 0.
#[test]
fn manual_compaction_after_a_rebase_without_usage_reports_a_nonzero_after() {
    let env = TestEnv::new();
    let rec = Arc::new(Recorder::default());
    let inner: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("done one"),
        ScriptTurn::text("## Goal\nsummary"),
    ]));
    let engine: Arc<dyn Engine> = Arc::new(NoCount(inner));
    let mut st = build(&env, engine, rec.clone());
    push(&mut st, WireMsg::user("task one: fix the parser"));
    push(&mut st, WireMsg::assistant("done one"));
    push(&mut st, WireMsg::user("task two"));
    let mut sink = VecSink::default();
    assert_eq!(
        run_loop(&mut st, &mut sink, &mut HeadlessUi::default()).expect("run_loop"),
        RunEnd::Stopped
    );
    // A branch rebase drops the usage: the context is unknown (0) until the next reply.
    let head = st.transcript.wire().to_vec();
    st.transcript.rebase(head, oxa_proto::rebase_why::LEAF_MOVE);
    assert_eq!(st.transcript.context(), 0, "a rebase zeroes the usage");

    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("compact_now");
    let after = sink
        .0
        .iter()
        .find_map(|event| match event {
            AgentEvent::CompactionEnd {
                result: Some(result), ..
            } => Some(result.estimated_tokens_after),
            _ => None,
        })
        .expect("compaction result");
    assert!(
        after > 0,
        "the estimate is not clamped to the zeroed tokens_before"
    );
    assert_eq!(st.stats.context_tokens, after);
}

/// With `count_prompt` unavailable the post-compaction count is the local estimate (message
/// content only, no template framing), which under-counts the engine's own usage. On a
/// threshold compaction the estimate must still be smaller than the context before compaction:
/// the summary replaces the early turns.
#[test]
fn an_estimated_after_is_smaller_than_the_context_before_compaction() {
    let env = TestEnv::new();
    let rec = Arc::new(Recorder::default());
    let inner: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("done one"),
        ScriptTurn::text("## Goal\nsummary"),
    ]));
    let engine: Arc<dyn Engine> = Arc::new(NoCount(inner));
    let mut st = build(&env, engine, rec.clone());
    push(&mut st, WireMsg::user("task one: fix the parser"));
    push(&mut st, WireMsg::assistant("done one"));
    push(&mut st, WireMsg::user("task two"));
    let mut sink = VecSink::default();
    assert_eq!(
        run_loop(&mut st, &mut sink, &mut HeadlessUi::default()).expect("run_loop"),
        RunEnd::Stopped
    );
    let tokens_before = st.transcript.context();
    assert!(tokens_before > 0, "the usage is known before compaction");
    st.compact.soft = tokens_before - 1;
    let done = maybe_compact(&mut st, true, &mut sink, &mut HeadlessUi::default()).expect("maybe_compact");
    assert!(done, "the session compacts at the threshold");
    let after = sink
        .0
        .iter()
        .find_map(|event| match event {
            AgentEvent::CompactionEnd {
                result: Some(result), ..
            } => Some(result.estimated_tokens_after),
            _ => None,
        })
        .expect("compaction result");
    assert!(after > 0, "the estimate is nonzero");
    assert!(after < tokens_before, "compaction shrinks the context");
    assert_eq!(st.stats.context_tokens, after);
}
