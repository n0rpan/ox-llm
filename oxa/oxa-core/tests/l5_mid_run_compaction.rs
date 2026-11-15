//! OXA-LOCAL-FIRST L5 mid-run (`oxa.compaction.midRun`, O-188): between two tool steps of a
//! run, a context past the soft limit is compacted before the next model request, keeping the
//! unanswered tool batch verbatim. The regression: a long multi-tool run with big reads stays
//! under the cap without an aborted turn. Over the scripted fake engine (one token per byte);
//! isolation through `oxa_fake::TestEnv`; deterministic, no sleeps.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Handle, Open, Session, ToolSelection};
use oxa_fake::{Fault, Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, CommandSpec, CompactReason, Engine, Exposure, HeadlessUi, HookPoint, HookReply, Hooks,
    OxaError, Role, Source, Tool, ToolCall, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink,
    WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Bytes of each file the scripted run reads. With `softTokens` set, one tool
/// result is bounded to a quarter of the compaction point (at least 2048 bytes, L4): the shape
/// of a real run, where each read is bounded and the context grows step by step. A bounded
/// result is still above the A1 elision threshold (1000 tokens), so an elided last batch shows.
const FILE: usize = 6_000;

/// Steps of the regression run.
const STEPS: usize = 8;

/// `read`: returns `FILE` bytes that differ per path (each step reads a different file).
struct Read {
    spec: ToolSpec,
}

impl Tool for Read {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<oxa_proto::Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = args["path"].as_str().unwrap_or("none").to_string();
        let fill = char::from(b'a' + (path.len() % 26) as u8);
        let mut text = format!("{path}\n");
        text.push_str(&fill.to_string().repeat(FILE - text.len()));
        ToolOutput {
            text,
            is_error: false,
            ui: Value::Null,
        }
    }
}

fn read_tool() -> Arc<dyn Tool> {
    Arc::new(Read {
        spec: ToolSpec {
            name: "read".into(),
            description: "test read".into(),
            schema_json: "{}".into(),
            class: ToolClass::Read,
            exposure: Exposure::Direct,
            source: Source::Builtin,
        },
    })
}

/// A hook set that records every point it sees and answers `SessionBeforeCompact` with
/// `before` (`Continue` = no opinion).
struct Recorder {
    before: HookReply,
    seen: Mutex<Vec<(HookPoint, Value)>>,
}

impl Hooks for Recorder {
    fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        self.seen.lock().unwrap().push((p, payload.clone()));
        if p == HookPoint::SessionBeforeCompact {
            self.before.clone()
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

/// The settings of a case: `oxa.compaction.softTokens` and `oxa.compaction.midRun` (left out
/// when `None`).
fn cfg(soft: u32, mid_run: Option<bool>) -> Settings {
    let mut compaction = json!({ "softTokens": soft });
    if let Some(m) = mid_run {
        compaction["midRun"] = json!(m);
    }
    Settings::from_values(json!({"oxa": {"compaction": compaction}}), json!({}))
}

fn open(env: &TestEnv, eng: Arc<ScriptedEngine>, hooks: Vec<Arc<dyn Hooks>>, settings: Settings) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine: Arc<dyn Engine> = eng;
    let deps = Deps {
        engine,
        store,
        tools: vec![read_tool()],
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
    };
    Session::open(deps, Open::New).expect("open")
}

/// A reply that reads file `k`.
fn step(k: usize) -> ScriptTurn {
    let args = format!("{{\"path\": \"src/file{k}.rs\"}}");
    let mut t = ScriptTurn::tool(&format!("c{k}"), "read", &args);
    t.tool_calls = vec![ToolCall {
        id: format!("c{k}"),
        name: "read".into(),
        args_raw: args,
    }];
    t
}

/// Requests sent to answer (not zero-token prefills).
fn sent(eng: &ScriptedEngine) -> Vec<Recorded> {
    eng.requests().into_iter().filter(|r| r.max_tokens != 0).collect()
}

fn kinds(eng: &ScriptedEngine) -> Vec<String> {
    sent(eng).into_iter().map(|r| r.kind).collect()
}

fn wire_bytes(r: &Recorded) -> usize {
    r.messages.iter().map(|m| m.to_json().len()).sum()
}

/// The unanswered batch at the end of a wire: from the last assistant message with tool calls.
fn last_batch(wire: &[WireMsg]) -> Vec<WireMsg> {
    let start = wire
        .iter()
        .rposition(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .expect("a tool batch");
    wire[start..].to_vec()
}

/// Every tool result has its call in the preceding assistant message, and every call of an
/// assistant message is answered by the tool results right after it.
fn assert_paired(wire: &[WireMsg]) {
    let mut open: Vec<String> = Vec::new();
    for m in wire {
        match m.role {
            Role::Tool => {
                let id = m.tool_call_id.clone().expect("tool result id");
                let at = open.iter().position(|o| *o == id);
                assert!(at.is_some(), "orphan tool result {id}");
                open.remove(at.unwrap_or_default());
            }
            _ => {
                assert!(open.is_empty(), "unanswered calls {open:?}");
                open = m.tool_calls.iter().map(|c| c.id.clone()).collect();
            }
        }
    }
    assert!(open.is_empty(), "unanswered calls {open:?}");
}

/// One prompt over `script`; returns the engine and the events.
fn run(soft: u32, mid_run: Option<bool>, script: Vec<ScriptTurn>) -> (Arc<ScriptedEngine>, Vec<AgentEvent>) {
    run_in(&TestEnv::new(), soft, mid_run, script)
}

/// [`run`] in a given environment (two runs in one environment share the system prompt's
/// working directory, so their requests can be compared byte for byte).
fn run_in(
    env: &TestEnv,
    soft: u32,
    mid_run: Option<bool>,
    script: Vec<ScriptTurn>,
) -> (Arc<ScriptedEngine>, Vec<AgentEvent>) {
    run_env(env, soft, mid_run, script, Vec::new(), true)
}

fn run_with(
    soft: u32,
    mid_run: Option<bool>,
    script: Vec<ScriptTurn>,
    hooks: Vec<Arc<dyn Hooks>>,
    auto: bool,
) -> (Arc<ScriptedEngine>, Vec<AgentEvent>) {
    run_env(&TestEnv::new(), soft, mid_run, script, hooks, auto)
}

fn run_env(
    env: &TestEnv,
    soft: u32,
    mid_run: Option<bool>,
    script: Vec<ScriptTurn>,
    hooks: Vec<Arc<dyn Hooks>>,
    auto: bool,
) -> (Arc<ScriptedEngine>, Vec<AgentEvent>) {
    let eng = Arc::new(ScriptedEngine::new(script));
    let mut s = open(env, eng.clone(), hooks, cfg(soft, mid_run));
    s.set_auto_compaction(auto);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("read the files".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    (eng, sink.0)
}

const SOFT: u32 = 2_000;

/// The `STEPS`-step script: `summaries` lists after which steps a summary request is
/// expected; `end_summary` adds the after-reply summary of the stopped run.
fn steps(summaries: &[usize], end_summary: bool) -> Vec<ScriptTurn> {
    let mut script = Vec::new();
    for k in 1..=STEPS {
        script.push(step(k));
        if summaries.contains(&k) {
            script.push(ScriptTurn::text(&format!("summary after step {k}")));
        }
    }
    script.push(ScriptTurn::text("all files read"));
    if end_summary {
        script.push(ScriptTurn::text("summary of the run"));
    }
    script
}

fn chats(eng: &ScriptedEngine) -> Vec<Recorded> {
    sent(eng).into_iter().filter(|r| r.kind == "chat").collect()
}

fn settled_cleanly(events: &[AgentEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: false }))
        && !events
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true }))
}

/// The regression of the ruling (O-188): a small soft limit, one prompt whose
/// run reads six different large files. Between tool steps the context is compacted, every
/// chat request stays small, the batch the model has not answered is kept verbatim, and the
/// run ends with its answer without an aborted turn.
#[test]
fn long_multi_tool_run_stays_under_the_cap() {
    // Without the mid-run compaction (setting off): six steps, then the answer, then the
    // after-reply compaction of today.
    let (off, off_events) = run(SOFT, Some(false), steps(&[], true));
    let mut plain = vec!["chat"; STEPS + 1];
    plain.push("compaction");
    assert_eq!(kinds(&off), plain);
    assert!(settled_cleanly(&off_events));
    let off_max = chats(&off).iter().map(wire_bytes).max().unwrap_or(0);

    // With it (the default): step 1 has no earlier step to fold; every later step folds
    // the previous batch before the next request. After the answer the kept tail fits
    // `keepRecentTokens`, so the after-reply check has nothing to cut.
    let folds: Vec<usize> = (2..=STEPS).collect();
    let (on, events) = run(SOFT, None, steps(&folds, false));
    let mut expected = vec!["chat", "chat"];
    for _ in 2..=STEPS {
        expected.extend(["compaction", "chat"]);
    }
    assert_eq!(kinds(&on), expected);

    // (i) Summary requests between tool steps, foreground, reason threshold.
    let reqs = sent(&on);
    for r in reqs.iter().filter(|r| r.kind == "compaction") {
        assert!(!r.background, "a mid-run summary is foreground");
    }
    let mid_ends: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::CompactionEnd {
                    reason: CompactReason::Threshold,
                    aborted: false,
                    result: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        mid_ends.len(),
        STEPS - 1,
        "a mid-run compaction per step after the first"
    );

    // (ii) Every chat request stays below the bound; the uncompacted run exceeds it.
    const BOUND: usize = 5_000;
    let on_max = chats(&on).iter().map(wire_bytes).max().unwrap_or(0);
    eprintln!("largest chat request wire: {on_max} bytes with, {off_max} bytes without");
    assert!(on_max < BOUND, "largest chat request {on_max} bytes");
    assert!(
        off_max > 3 * BOUND,
        "the uncompacted run grows to {off_max} bytes"
    );

    // (iii) Each chat request right after a summary request ends with the batch the summary
    // request's wire ended with, byte for byte, with valid pairing.
    let mut checked = 0;
    for w in reqs.windows(2) {
        if w[0].kind == "compaction" && w[1].kind == "chat" {
            let wire = &w[0].messages[..w[0].messages.len() - 1];
            let batch = last_batch(wire);
            assert!(
                w[1].messages.ends_with(&batch),
                "the unanswered batch is kept verbatim"
            );
            let kept = batch
                .last()
                .and_then(|m| m.content.as_deref())
                .map_or(0, str::len);
            assert!(kept >= 1_500, "the last tool result is not elided ({kept} bytes)");
            assert_paired(&w[1].messages);
            checked += 1;
        }
    }
    assert_eq!(checked, STEPS - 1);

    // (iv) The answer is produced and the run ends cleanly; (v) no aborted turn.
    assert!(settled_cleanly(&events));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::AgentEnd { messages, .. }
            if messages.iter().any(|m| m.msg.content.as_deref() == Some("all files read"))
    )));
}

/// 3b: no thrash. A tiny soft limit and three big steps: one summary request per step that
/// adds an earlier step to fold (none after step 1, which has nothing before its batch but the
/// prompt), never two summary requests in a row, never twice over the same wire.
#[test]
fn tiny_soft_point_never_thrashes() {
    let mut script = vec![
        step(1),
        step(2),
        ScriptTurn::text("s2"),
        step(3),
        ScriptTurn::text("s3"),
    ];
    script.push(ScriptTurn::text("done"));
    let (eng, _) = run(1, None, script);
    let k = kinds(&eng);
    assert_eq!(
        k,
        vec!["chat", "chat", "compaction", "chat", "compaction", "chat"]
    );
    for w in k.windows(2) {
        assert!(!(w[0] == "compaction" && w[1] == "compaction"), "{k:?}");
    }
    let summaries: Vec<_> = sent(&eng)
        .into_iter()
        .filter(|r| r.kind == "compaction")
        .collect();
    for w in summaries.windows(2) {
        assert_ne!(w[0].messages, w[1].messages);
    }
}

/// 3b: one tool step past a tiny soft limit: nothing older than the batch but the prompt, so
/// no summary request is sent mid-run.
#[test]
fn a_single_step_has_nothing_to_fold() {
    let (eng, _) = run(1, None, vec![step(1), ScriptTurn::text("done")]);
    // (After the answer the whole wire fits `keepRecentTokens`: nothing to cut either.)
    assert_eq!(kinds(&eng), vec!["chat", "chat"]);
}

/// 3d: a failed summary becomes the mechanical record, and the next request
/// still ends with the unanswered batch verbatim.
#[test]
fn failed_summary_is_mechanical_and_keeps_the_batch() {
    let script = vec![
        step(1),
        step(2),
        ScriptTurn::fault(Fault::Http {
            status: 400,
            body: "bad request".into(),
        }),
        ScriptTurn::text("done"),
    ];
    let (eng, events) = run(SOFT, None, script);
    assert_eq!(kinds(&eng), vec!["chat", "chat", "compaction", "chat"]);
    let mechanical = events.iter().any(|e| match e {
        AgentEvent::CompactionEnd { result: Some(r), .. } => r.details["mechanical"] == json!(true),
        _ => false,
    });
    assert!(mechanical, "the mechanical summary applied");
    let reqs = sent(&eng);
    let batch = last_batch(&reqs[2].messages[..reqs[2].messages.len() - 1]);
    assert!(reqs[3].messages.ends_with(&batch));
    assert_paired(&reqs[3].messages);
    assert!(wire_bytes(&reqs[3]) + 1_500 < wire_bytes(&reqs[2]));
    assert!(settled_cleanly(&events));
}

/// 3e: the hooks fire as for any threshold compaction; a hook that cancels leaves the wire
/// unchanged and the run continues (no summary request).
#[test]
fn hooks_fire_and_a_cancel_leaves_the_run_going() {
    let rec = Arc::new(Recorder {
        before: HookReply::Continue,
        seen: Mutex::new(Vec::new()),
    });
    let script = vec![
        step(1),
        step(2),
        ScriptTurn::text("s2"),
        ScriptTurn::text("done"),
        ScriptTurn::text("end summary"),
    ];
    let (_eng, _) = run_with(SOFT, None, script, vec![rec.clone()], true);
    let seen = rec.seen.lock().unwrap().clone();
    let reasons = |p: HookPoint| {
        seen.iter()
            .filter(|(q, _)| *q == p)
            .map(|(_, v)| v["reason"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(reasons(HookPoint::SessionBeforeCompact)[0], json!("threshold"));
    assert_eq!(reasons(HookPoint::SessionCompact)[0], json!("threshold"));

    let cancel = Arc::new(Recorder {
        before: HookReply::Cancel("not now".into()),
        seen: Mutex::new(Vec::new()),
    });
    let script = vec![step(1), step(2), step(3), ScriptTurn::text("done")];
    let (eng, events) = run_with(SOFT, None, script, vec![cancel.clone()], true);
    assert_eq!(kinds(&eng), vec!["chat", "chat", "chat", "chat"]);
    let reqs = sent(&eng);
    // The wire only grew: each request extends the previous one.
    for w in reqs.windows(2) {
        assert!(w[1].messages.starts_with(&w[0].messages));
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::CompactionEnd { aborted: true, .. })));
    assert!(settled_cleanly(&events));
}

/// The explicit setting wins: on compacts mid-run, off does not.
#[test]
fn explicit_setting_wins() {
    let (on, _) = run(
        SOFT,
        Some(true),
        vec![
            step(1),
            step(2),
            ScriptTurn::text("s2"),
            ScriptTurn::text("done"),
            ScriptTurn::text("end"),
        ],
    );
    assert_eq!(kinds(&on), vec!["chat", "chat", "compaction", "chat"]);
    let (off, _) = run(
        SOFT,
        Some(false),
        vec![
            step(1),
            step(2),
            ScriptTurn::text("s2"),
            ScriptTurn::text("done"),
            ScriptTurn::text("end"),
        ],
    );
    assert_eq!(kinds(&off), vec!["chat", "chat", "chat", "compaction"]);
}

/// Auto-compaction switched off at runtime: no mid-run compaction (and none after the run).
#[test]
fn runtime_auto_compaction_off_disables_it() {
    let script = vec![step(1), step(2), step(3), ScriptTurn::text("done")];
    let (eng, events) = run_with(SOFT, None, script, Vec::new(), false);
    assert_eq!(kinds(&eng), vec!["chat", "chat", "chat", "chat"]);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::CompactionStart { .. })));
}

/// 3a with a result far above the elision threshold (an explicit `oxa.toolOutputCap` wins over
/// the byte budget): the last batch is kept whole and in the kept tail, while the earlier
/// batch is summarised.
#[test]
fn oversized_last_batch_is_kept_whole() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        step(1),
        step(2),
        ScriptTurn::text("s2"),
        ScriptTurn::text("done"),
        ScriptTurn::text("end summary"),
    ]));
    let settings = Settings::from_values(
        json!({"oxa": {"toolOutputCap": 16384,
            "compaction": {"softTokens": SOFT}}, "compaction": {"keepRecentTokens": 2000}}),
        json!({}),
    );
    let mut s = open(&env, eng.clone(), Vec::new(), settings);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("read".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    // (With this small keep the after-reply compaction then folds the answered batch.)
    assert_eq!(
        kinds(&eng),
        vec!["chat", "chat", "compaction", "chat", "compaction"]
    );
    let reqs = sent(&eng);
    let batch = last_batch(&reqs[2].messages[..reqs[2].messages.len() - 1]);
    assert_eq!(
        batch.last().and_then(|m| m.content.as_deref()).map(str::len),
        Some(FILE),
        "the {FILE}-byte result is above max(keep / 2, 1000) tokens and still kept whole"
    );
    assert!(reqs[3].messages.ends_with(&batch));
    assert_eq!(
        reqs[3].messages.len(),
        1 + batch.len(),
        "summary head + the batch"
    );
    assert_paired(&reqs[3].messages);
}

/// A sink that aborts the run (the front end's `Handle::abort`) when a threshold compaction
/// starts, i.e. while the mid-run compaction and its summary request are in progress.
struct AbortOnCompaction {
    handle: Handle,
    events: Vec<AgentEvent>,
}

impl oxa_proto::EventSink for AbortOnCompaction {
    fn emit(&mut self, e: &AgentEvent) {
        if matches!(
            e,
            AgentEvent::CompactionStart {
                reason: CompactReason::Threshold,
                ..
            }
        ) {
            self.handle.abort();
        }
        self.events.push(e.clone());
    }
}

/// A user abort during the mid-run summary ends the run: the summary request reaches the
/// engine cancelled and is refused, the wire is not rebased, no further chat request is sent
/// and the run settles as aborted.
#[test]
fn abort_during_the_summary_ends_the_run() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        step(1),
        step(2),
        ScriptTurn::text("summary never used"),
        step(3),
        ScriptTurn::text("done"),
    ]));
    let mut s = open(&env, eng.clone(), Vec::new(), cfg(SOFT, None));
    let mut sink = AbortOnCompaction {
        handle: s.handle(),
        events: Vec::new(),
    };
    let mut ui = HeadlessUi::default();
    let _ = s.prompt("read the files".into(), Vec::new(), &mut sink, &mut ui);
    let all: Vec<String> = eng.requests().into_iter().map(|r| r.kind).collect();
    assert_eq!(
        all.iter().filter(|k| *k == "chat").count(),
        2,
        "no chat request after the abort: {all:?}"
    );
    assert_eq!(all.last().map(String::as_str), Some("compaction"), "{all:?}");
    let events = sink.events;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionEnd {
            reason: CompactReason::Threshold,
            result: None,
            ..
        }
    )));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: false })));
}

/// The hard-limit check is kept as the backstop: a hook cancels every threshold compaction
/// (the mid-run one included), the context passes the hard limit after a tool step, and the
/// overflow compaction between the tool steps still runs (it folds the earlier prompt's turn,
/// the current prompt's turn is kept), so the next request fits and the run ends with its
/// answer, no aborted turn.
#[test]
fn hard_limit_backstop_still_compacts() {
    struct CancelThreshold;
    impl Hooks for CancelThreshold {
        fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
            if p == HookPoint::SessionBeforeCompact && payload["reason"] == json!("threshold") {
                HookReply::Cancel("not the soft one".into())
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
    const N_CTX: u32 = 13_000;
    let settings = |reserve: u32| {
        Settings::from_values(
            json!({"oxa": {"toolOutputCap": 16384,
                "compaction": {"softTokens": SOFT}},
                "compaction": {"reserveTokens": reserve}}),
            json!({}),
        )
    };
    let prompt = |s: &mut Session, text: &str| -> Vec<AgentEvent> {
        let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
        s.prompt(text.into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        sink.0
    };
    let env = TestEnv::new();
    // Probe (same engine limits): the prompt of the request after one read.
    let probe = Arc::new(ScriptedEngine::new(vec![step(1), ScriptTurn::text("x")]).with_limits(N_CTX, 1_000));
    {
        let mut s = open(&env, probe.clone(), Vec::new(), settings(1_000));
        prompt(&mut s, "read the files");
    }
    let one = sent(&probe)[1].usage.expect("usage").prompt;
    // The hard limit sits between one read and two reads in the context.
    let hard = one + 3_000;
    let eng = Arc::new(
        ScriptedEngine::new(vec![
            step(1),
            ScriptTurn::text("first read"),
            step(2),
            step(3),
            ScriptTurn::text("overflow summary"),
            ScriptTurn::text("done"),
        ])
        .with_limits(N_CTX, 1_000),
    );
    let mut s = open(
        &env,
        eng.clone(),
        vec![Arc::new(CancelThreshold)],
        settings(N_CTX - 1_000 - hard),
    );
    assert_eq!(s.inner().compact.hard, hard);
    prompt(&mut s, "read the files");
    let events = prompt(&mut s, "read two more");
    let k = kinds(&eng);
    assert_eq!(k, ["chat", "chat", "chat", "chat", "compaction", "chat"], "{k:?}");
    // The soft attempts were cancelled by the hook (no request), the overflow one compacted.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionEnd {
            reason: CompactReason::Threshold,
            aborted: true,
            ..
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionEnd {
            reason: CompactReason::Overflow,
            aborted: false,
            result: Some(_),
            ..
        }
    )));
    let reqs = sent(&eng);
    assert!(
        reqs[3].usage.expect("usage").prompt > hard,
        "the context passed the hard limit"
    );
    let after = reqs[5].usage.expect("the request after the backstop is answered");
    assert!(after.prompt < N_CTX);
    assert_paired(&reqs[5].messages);
    assert!(settled_cleanly(&events));
}
