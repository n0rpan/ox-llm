//! OXA-LOCAL-FIRST L5 (`oxa.compaction.turnStart`): a prompt that finds the context above the
//! soft limit is compacted before it is sent. The incident: a turn aborted by a big tool result
//! left the result uncounted (`Transcript::context` is the last reply's usage), so the next task
//! started with the whole result in the wire. Over the scripted fake engine; isolation through
//! `oxa_fake::TestEnv`; deterministic, no sleeps.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    tool_errors, AgentEvent, Engine, Exposure, HeadlessUi, Source, Tool, ToolCall, ToolClass, ToolCtx,
    ToolOutput, ToolSpec, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::sync::Arc;

/// `read`: returns `bytes` bytes; with `cancel` it cancels the run while it returns (the user's
/// abort lands during the tool, so the run ends after the result is recorded).
struct Read {
    spec: ToolSpec,
    bytes: usize,
    cancel: bool,
}

impl Tool for Read {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<oxa_proto::Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        if self.cancel {
            cx.cancel.cancel();
        }
        if cx.cancel.is_cancelled() && self.bytes == 0 {
            return ToolOutput::error(tool_errors::ABORTED);
        }
        ToolOutput {
            text: "r".repeat(self.bytes),
            is_error: false,
            ui: Value::Null,
        }
    }
}

fn read_tool(bytes: usize, cancel: bool) -> Arc<dyn Tool> {
    Arc::new(Read {
        spec: ToolSpec {
            name: "read".into(),
            description: "test read".into(),
            schema_json: "{}".into(),
            class: ToolClass::Read,
            exposure: Exposure::Direct,
            source: Source::Builtin,
        },
        bytes,
        cancel,
    })
}

/// The settings of a case: `oxa.compaction.softTokens` and `oxa.compaction.turnStart` (each
/// left out when `None`).
fn cfg(soft: Option<u32>, turn_start: Option<bool>) -> Settings {
    let mut compaction = json!({});
    if let Some(s) = soft {
        compaction["softTokens"] = json!(s);
    }
    if let Some(t) = turn_start {
        compaction["turnStart"] = json!(t);
    }
    Settings::from_values(
        // The explicit cap keeps the fake tool's big result whole: a small soft point would
        // otherwise bound it to a quarter of that point (l4-budget), and these tests are about a
        // context that is already too large.
        json!({"oxa": {"compaction": compaction, "toolOutputCap": 16384}}),
        json!({}),
    )
}

fn open(env: &TestEnv, eng: Arc<ScriptedEngine>, tools: Vec<Arc<dyn Tool>>, settings: Settings) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine: Arc<dyn Engine> = eng;
    let deps = Deps {
        engine,
        store,
        tools,
        hooks: Vec::new(),
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
    };
    Session::open(deps, Open::New).expect("open")
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        args_raw: "{\"path\": \"big.txt\"}".into(),
    }
}

/// A reply that calls `read` once.
fn read_turn(id: &str) -> ScriptTurn {
    let mut t = ScriptTurn::tool(id, "read", "{\"path\": \"big.txt\"}");
    t.tool_calls = vec![call(id)];
    t
}

/// Requests that are sent to answer (not prefills): (kind, background, wire bytes).
fn sent(eng: &ScriptedEngine) -> Vec<Recorded> {
    eng.requests().into_iter().filter(|r| r.max_tokens != 0).collect()
}

fn kinds(eng: &ScriptedEngine) -> Vec<String> {
    sent(eng).into_iter().map(|r| r.kind).collect()
}

fn wire_bytes(r: &Recorded) -> usize {
    r.messages.iter().map(|m| m.to_json().len()).sum()
}

/// Context after the aborted turn of the incident: the usage of the reply that called `read`
/// (the result itself is not counted). Measured with no soft limit.
fn measured_abort_context(env: &TestEnv, read_bytes: usize) -> u32 {
    let eng = Arc::new(ScriptedEngine::new(vec![read_turn("c1")]));
    let mut s = open(env, eng, vec![read_tool(read_bytes, true)], cfg(None, None));
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    s.inner().transcript.context()
}

/// The incident: turn 1 reads a big file and is aborted; the next prompt is sent with the
/// result still in the wire. Returns the session and the engine after the second prompt.
fn incident(
    env: &TestEnv,
    soft: u32,
    turn_start: Option<bool>,
    read_bytes: usize,
    script: Vec<ScriptTurn>,
) -> (Session, Arc<ScriptedEngine>, VecSink) {
    let eng = Arc::new(ScriptedEngine::new(script));
    let mut s = open(
        env,
        eng.clone(),
        vec![read_tool(read_bytes, true)],
        cfg(Some(soft), turn_start),
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look".into(), Vec::new(), &mut sink, &mut ui)
        .expect("first prompt");
    assert!(sink
        .0
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true })));
    let mut sink2 = VecSink::default();
    s.prompt("next task".into(), Vec::new(), &mut sink2, &mut ui)
        .expect("second prompt");
    (s, eng, sink2)
}

const BIG: usize = 60_000;

/// (a) The regression: the first request of the new prompt is the compaction summary request
/// (foreground), and the answer's wire is the compacted head plus the new user message.
#[test]
fn aborted_big_read_is_compacted_before_the_next_prompt() {
    let env = TestEnv::new();
    let soft = measured_abort_context(&env, BIG) + 1000;
    let script = vec![
        read_turn("c1"),
        ScriptTurn::text("summary of the aborted read"),
        ScriptTurn::text("answer"),
    ];
    let (_s, eng, sink) = incident(&env, soft, None, BIG, script);
    let reqs = sent(&eng);
    assert_eq!(kinds(&eng), vec!["chat", "compaction", "chat"]);
    assert!(!reqs[1].background, "the summary of a turn start is foreground");
    let answer = &reqs[2];
    assert!(
        wire_bytes(answer) < 5_000,
        "the answer sees the compacted head, not the {BIG}-byte read ({} bytes)",
        wire_bytes(answer)
    );
    assert_eq!(
        answer.messages.last().and_then(|m| m.content.as_deref()),
        Some("next task")
    );
    assert!(sink
        .0
        .iter()
        .any(|e| matches!(e, AgentEvent::CompactionEnd { aborted: false, .. })));
}

/// (b) Context below the soft limit at turn start (the result is counted but the limit is
/// far): no extra request.
#[test]
fn below_soft_at_turn_start_sends_no_summary() {
    let env = TestEnv::new();
    let script = vec![read_turn("c1"), ScriptTurn::text("answer")];
    let (_s, eng, _sink) = incident(&env, 500_000, None, BIG, script);
    assert_eq!(kinds(&eng), vec!["chat", "chat"]);
}

/// (d) The explicit setting decides: on compacts at turn start, off does not.
#[test]
fn explicit_setting_wins() {
    let env = TestEnv::new();
    let soft = measured_abort_context(&env, BIG) + 1000;
    let script = || {
        vec![
            read_turn("c1"),
            ScriptTurn::text("summary"),
            ScriptTurn::text("answer"),
        ]
    };
    let (_s, on, _) = incident(&env, soft, Some(true), BIG, script());
    assert_eq!(kinds(&on), vec!["chat", "compaction", "chat"]);

    let (_s, off, _) = incident(
        &env,
        soft,
        Some(false),
        BIG,
        vec![read_turn("c1"), ScriptTurn::text("answer")],
    );
    assert_eq!(kinds(&off), vec!["chat", "chat", "compaction"]);
}

/// (e) Auto-compaction switched off at runtime: no compaction at turn start.
#[test]
fn runtime_auto_compaction_off_disables_it() {
    let env = TestEnv::new();
    let soft = measured_abort_context(&env, BIG) + 1000;
    let eng = Arc::new(ScriptedEngine::new(vec![
        read_turn("c1"),
        ScriptTurn::text("answer"),
    ]));
    let mut s = open(
        &env,
        eng.clone(),
        vec![read_tool(BIG, true)],
        cfg(Some(soft), None),
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("look".into(), Vec::new(), &mut sink, &mut ui)
        .expect("first prompt");
    s.set_auto_compaction(false);
    s.prompt("next task".into(), Vec::new(), &mut sink, &mut ui)
        .expect("second prompt");
    assert_eq!(kinds(&eng), vec!["chat", "chat"]);
}

/// (f) A valid pending idle summary is applied at turn start with no summary request. The
/// idle summary is made after a stopped run whose context is just below the soft limit; the
/// aborted turn after it then pushes the context over the limit.
#[test]
fn pending_idle_summary_is_applied_without_a_request() {
    let env = TestEnv::new();
    let stopped_reply = "y".repeat(4_100);
    // The context after the stopped run (the idle summary is on by default). The soft limit
    // sits just above it: the idle summary is made, and the aborted read's result (about 1200
    // tokens by the estimate) takes the context over the limit.
    let soft = {
        let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text(&stopped_reply)]));
        let mut s = open(&env, eng, Vec::new(), cfg(None, None));
        let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
        s.prompt("first".into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        s.inner().transcript.context() + 500
    };
    let script = vec![
        ScriptTurn::text(&stopped_reply),
        ScriptTurn::text("idle summary"),
        read_turn("c2"),
        ScriptTurn::text("answer"),
    ];
    let eng = Arc::new(ScriptedEngine::new(script));
    let mut s = open(
        &env,
        eng.clone(),
        vec![read_tool(3_600, true)],
        cfg(Some(soft), None),
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("first".into(), Vec::new(), &mut sink, &mut ui)
        .expect("first prompt");
    assert_eq!(
        kinds(&eng),
        vec!["chat", "compaction"],
        "the idle summary ran after the stopped run"
    );
    s.prompt("second".into(), Vec::new(), &mut sink, &mut ui)
        .expect("aborted prompt");
    let mut sink3 = VecSink::default();
    s.prompt("third".into(), Vec::new(), &mut sink3, &mut ui)
        .expect("third prompt");
    assert_eq!(kinds(&eng), vec!["chat", "compaction", "chat", "chat"]);
    let precomputed = sink3.0.iter().any(|e| match e {
        AgentEvent::CompactionEnd { result: Some(r), .. } => r.details["precomputed"] == json!(true),
        _ => false,
    });
    assert!(
        precomputed,
        "the turn-start compaction used the pending summary: {:?}",
        sink3.0
    );
}
