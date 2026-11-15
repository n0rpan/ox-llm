//! C3 acceptance tests: Compaction orchestration.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_compact::prompt::{self, SummaryKind};
use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{
    append_entry, compact_now, idle_prefill, maybe_compact, run_loop, Queue, RunEnd, SessionState, Shared,
    Stats, Toolset,
};
use oxa_fake::{check_w1, check_w5, Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, AgentEvent, CancelToken, CommandSpec, CompactReason, Engine, Entry, EntryBody, FrozenPrefix,
    HeadlessUi, HookPoint, HookReply, Hooks, Mode, OxaError, Policy, Role, ThinkingLevel, Tool, ToolCall, Ui,
    Usage, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionHeader, SessionMeta, SessionRef, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

/// Policy that compacts whenever the context is above `soft` after a run, and keeps only the
/// newest message (`keep_tokens: 1`) so that the cut lands at the last valid point.
fn tight(soft: u32, hard: u32) -> CompactPolicy {
    CompactPolicy {
        soft,
        hard,
        keep_tokens: 1,
        summary_max: 1024,
        enabled: true,
    }
}

fn build(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    hooks: Vec<Arc<dyn Hooks>>,
    compact: CompactPolicy,
    settings: Settings,
) -> SessionState {
    build_with_store(env, Arc::new(MemStore::new()), engine, hooks, compact, settings)
}

fn build_with_store(
    env: &TestEnv,
    store: Arc<dyn SessionStore>,
    engine: Arc<dyn Engine>,
    hooks: Vec<Arc<dyn Hooks>>,
    compact: CompactPolicy,
    settings: Settings,
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
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
    });
    SessionState {
        engine,
        store,
        hooks,
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

/// A session over a scripted engine. The engine is returned for inspection of the requests.
fn state(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    compact: CompactPolicy,
) -> (SessionState, Arc<ScriptedEngine>) {
    state_settings(env, turns, compact, Settings::default())
}

fn state_settings(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    compact: CompactPolicy,
    settings: Settings,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    (build(env, engine, Vec::new(), compact, settings), eng)
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

fn run(st: &mut SessionState) -> RunEnd {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    run_loop(st, &mut sink, &mut ui).expect("run_loop")
}

/// An assistant message that calls tools (text empty).
fn calls(list: Vec<(&str, &str, &str)>) -> WireMsg {
    WireMsg {
        role: Role::Assistant,
        content: Some(String::new()),
        reasoning: None,
        tool_calls: list
            .into_iter()
            .map(|(id, name, args)| ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                args_raw: args.to_string(),
            })
            .collect(),
        tool_call_id: None,
    }
}

/// The summary request texts and kinds, for assertions.
fn kinds(reqs: &[Recorded]) -> Vec<&str> {
    reqs.iter().map(|r| r.kind.as_str()).collect()
}

/// Requests other than the idle prefill (which every compaction is followed by).
fn chat_like(reqs: &[Recorded]) -> usize {
    reqs.iter().filter(|r| r.kind != "prefill").count()
}

fn compaction_entries(st: &SessionState) -> Vec<&oxa_proto::Entry> {
    st.tree
        .entries()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Compaction { .. }))
        .collect()
}

fn ui() -> HeadlessUi {
    HeadlessUi::default()
}

/// A hook set that answers one point with a fixed reply and continues everywhere else.
struct ReplyHook {
    point: HookPoint,
    reply: HookReply,
}

impl Hooks for ReplyHook {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p == self.point {
            self.reply.clone()
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

/// A store that fails the next append after [`FailOnceStore::fail_next`]; everything else
/// delegates to [`MemStore`]. Models a disk error in a compaction step other than the summary.
struct FailOnceStore {
    inner: MemStore,
    fail_next: AtomicBool,
}

impl FailOnceStore {
    fn new() -> FailOnceStore {
        FailOnceStore {
            inner: MemStore::new(),
            fail_next: AtomicBool::new(false),
        }
    }

    fn fail_next(&self) {
        self.fail_next.store(true, Ordering::SeqCst);
    }
}

impl SessionStore for FailOnceStore {
    fn create(
        &self,
        cwd: &std::path::Path,
        id: Option<&str>,
        parent: Option<&std::path::Path>,
    ) -> Result<SessionRef, OxaError> {
        self.inner.create(cwd, id, parent)
    }
    fn append(&self, s: &SessionRef, e: &Entry) -> Result<(), OxaError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(OxaError::new(codes::IO, "disk full"));
        }
        self.inner.append(s, e)
    }
    fn load(&self, s: &SessionRef) -> Result<Vec<Entry>, OxaError> {
        self.inner.load(s)
    }
    fn header(&self, s: &SessionRef) -> Result<SessionHeader, OxaError> {
        self.inner.header(s)
    }
    fn list(&self, cwd: Option<&std::path::Path>) -> Vec<SessionMeta> {
        self.inner.list(cwd)
    }
    fn resolve(&self, id_or_path: &str) -> Result<SessionRef, OxaError> {
        self.inner.resolve(id_or_path)
    }
    fn delete(&self, s: &SessionRef) -> Result<(), OxaError> {
        self.inner.delete(s)
    }
}

/// Records the hook points that fire.
#[derive(Default)]
struct FiredHooks {
    points: Mutex<Vec<HookPoint>>,
}

impl Hooks for FiredHooks {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        self.points.lock().unwrap().push(p);
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
// Acceptance tests

/// soft limit crossed -> compaction at run end, next prompt delivered live
#[test]
fn auto_after_reply() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("first reply"),
            ScriptTurn::text("SUMMARY ONE"),
            ScriptTurn::text("second reply"),
        ],
        tight(1, u32::MAX),
    );
    say(&mut st, "first prompt");
    assert_eq!(run(&mut st), RunEnd::Stopped);

    let mut sink = VecSink::default();
    assert!(
        maybe_compact(&mut st, true, &mut sink, &mut ui()).expect("maybe_compact"),
        "soft limit crossed"
    );
    assert!(sink.0.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionStart {
            reason: CompactReason::Threshold
        }
    )));

    // The head is the summary followed by the kept reply.
    assert_eq!(
        st.transcript.wire()[0],
        WireMsg::user(prompt::summary_user_message("SUMMARY ONE"))
    );
    assert_eq!(st.transcript.wire()[1], WireMsg::assistant("first reply"));

    // The next prompt goes out live, with the head as its start.
    say(&mut st, "second prompt");
    assert_eq!(run(&mut st), RunEnd::Stopped);

    let reqs = eng.requests();
    assert_eq!(kinds(&reqs), ["chat", "compaction", "prefill", "chat"]);
    let last = reqs.last().unwrap();
    assert_eq!(
        last.messages[0],
        WireMsg::user(prompt::summary_user_message("SUMMARY ONE"))
    );
    assert_eq!(last.messages.last().unwrap(), &WireMsg::user("second prompt"));
    let rebase = [(reqs[2].seq, "compaction")];
    assert!(check_w1(&reqs, &rebase).is_empty(), "W1 across the compaction");
    assert!(check_w5(&reqs, &rebase).is_empty(), "W5 across the compaction");
    assert_eq!(compaction_entries(&st).len(), 1);
}

/// prompt kept as its own message
#[test]
fn mid_run_above_hard() {
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, vec![ScriptTurn::text("SUMMARY MID")], tight(u32::MAX, 100));
    say(&mut st, "the task");
    push(&mut st, calls(vec![("c1", "read", r#"{"path":"a.txt"}"#)]));
    push(&mut st, WireMsg::tool_result("c1", "A"));
    say(&mut st, "current prompt");
    push(&mut st, calls(vec![("c2", "read", r#"{"path":"b.txt"}"#)]));
    push(&mut st, WireMsg::tool_result("c2", "B"));
    // The context is above the hard limit: a mid-run overflow.
    st.transcript.set_usage(&Usage {
        prompt: 500,
        completion: 0,
        cached: 0,
        cached_missing: false,
    });

    let mut sink = VecSink::default();
    assert!(maybe_compact(&mut st, false, &mut sink, &mut ui()).expect("maybe_compact"));
    assert!(sink.0.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionStart {
            reason: CompactReason::Overflow
        }
    )));

    let wire = st.transcript.wire();
    let head = wire[0].content.as_deref().unwrap_or("");
    assert!(
        head.contains("SUMMARY MID") && head.contains("<read-files>\na.txt\n</read-files>"),
        "{head}"
    );
    assert_eq!(
        wire[1],
        WireMsg::user("current prompt"),
        "the prompt is its own message, not in the summary"
    );
    assert_eq!(wire.len(), 4, "prompt, its tool call and result are kept");
    assert_eq!(eng.requests()[0].kind, "compaction");
}

/// H3 + H4 + H6
#[test]
fn idle_prefill_headers_golden() {
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, vec![ScriptTurn::text("reply")], tight(u32::MAX, u32::MAX));
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let wire_before = st.transcript.wire().to_vec();
    let mut sink = VecSink::default();

    idle_prefill(&mut st, &mut sink, &mut ui());

    let reqs = eng.requests();
    let p = reqs.last().expect("prefill request");
    assert_eq!(p.kind, "prefill", "H5 kind");
    assert!(p.background, "H3 background");
    assert!(p.snapshot_keep, "H4 snapshot keep");
    assert_eq!(p.max_tokens, 0, "H6 prefill only");
    assert!(p.reply.is_none(), "H6 generates nothing");
    assert_eq!(p.messages, wire_before, "prefill sends the current wire exactly");
    assert_eq!(
        st.transcript.wire(),
        &wire_before[..],
        "prefill never pushes to the wire"
    );
}

/// /compact text appended to the instruction
#[test]
fn manual_compact_instructions() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![ScriptTurn::text("reply"), ScriptTurn::text("SUM")],
        tight(u32::MAX, u32::MAX),
    );
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);

    compact_now(
        &mut st,
        Some("keep the API"),
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut ui(),
    )
    .expect("compact");

    let reqs = eng.requests();
    let summary = reqs
        .iter()
        .find(|r| r.kind == "compaction")
        .expect("summary request");
    let instruction = summary.messages.last().expect("instruction");
    assert_eq!(instruction.role, Role::User);
    let words = prompt::summary_target_words(st.settings.summary_target());
    assert_eq!(
        instruction.content.as_deref(),
        Some(
            prompt::summary_instruction_with_target(SummaryKind::Initial, Some("keep the API"), words)
                .as_str()
        )
    );
    assert!(instruction
        .content
        .as_deref()
        .unwrap()
        .ends_with("\n\nAdditional focus: keep the API"));
}

/// SessionBeforeCompact Cancel
#[test]
fn hook_cancel() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("reply")]));
    let hook = ReplyHook {
        point: HookPoint::SessionBeforeCompact,
        reply: HookReply::Cancel("no thanks".into()),
    };
    let engine: Arc<dyn Engine> = eng.clone();
    let mut st = build(
        &env,
        engine,
        vec![Arc::new(hook)],
        tight(u32::MAX, u32::MAX),
        Settings::default(),
    );
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let before = st.transcript.wire().to_vec();
    let mut sink = VecSink::default();

    compact_now(&mut st, None, CompactReason::Manual, &mut sink, &mut ui())
        .expect("a cancel is not an error");

    assert_eq!(eng.requests().len(), 1, "no summary request after a cancel");
    assert_eq!(st.transcript.wire(), &before[..], "wire unchanged");
    assert!(compaction_entries(&st).is_empty());
    assert!(sink.0.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionEnd {
            aborted: true,
            result: None,
            ..
        }
    )));
}

/// from_hook
#[test]
fn hook_custom_summary_lands_as_head() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("reply")]));
    let hook = ReplyHook {
        point: HookPoint::SessionBeforeCompact,
        reply: HookReply::Summary {
            text: "from_hook summary".into(),
            details: json!({"owner": "ext"}),
        },
    };
    let engine: Arc<dyn Engine> = eng.clone();
    let mut st = build(
        &env,
        engine,
        vec![Arc::new(hook)],
        tight(u32::MAX, u32::MAX),
        Settings::default(),
    );
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);

    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut ui(),
    )
    .expect("compact");

    assert_eq!(
        chat_like(&eng.requests()),
        1,
        "a hook summary needs no summary request"
    );
    assert_eq!(
        st.transcript.wire()[0],
        WireMsg::user(prompt::summary_user_message("from_hook summary"))
    );
    let entries = compaction_entries(&st);
    assert_eq!(entries.len(), 1);
    match &entries[0].body {
        EntryBody::Compaction {
            from_hook, details, ..
        } => {
            assert!(*from_hook, "from_hook");
            assert_eq!(details, &json!({"owner": "ext"}));
        }
        other => panic!("not a compaction: {other:?}"),
    }
}

/// check_w5 green
#[test]
fn summary_request_is_whole_wire_plus_one() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::text("reply"),
            ScriptTurn::text("SUM"),
            ScriptTurn::text("second"),
        ],
        tight(u32::MAX, u32::MAX),
    );
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let wire_before = st.transcript.wire().to_vec();

    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut ui(),
    )
    .expect("compact");
    say(&mut st, "again");
    assert_eq!(run(&mut st), RunEnd::Stopped);

    let reqs = eng.requests();
    let summary = reqs
        .iter()
        .find(|r| r.kind == "compaction")
        .expect("summary request");
    assert_eq!(
        summary.messages.len(),
        wire_before.len() + 1,
        "whole wire plus one instruction"
    );
    assert_eq!(&summary.messages[..wire_before.len()], &wire_before[..]);
    // F12: `/compact` (Manual) is a foreground request: the user waits on it.
    assert!(!summary.thinking && !summary.background);
    let rebase_seq = reqs
        .iter()
        .find(|r| r.kind == "prefill")
        .expect("prefill after compaction")
        .seq;
    let rebases = [(rebase_seq, "compaction")];
    assert!(check_w5(&reqs, &rebases).is_empty(), "check_w5 green");
    assert!(check_w1(&reqs, &rebases).is_empty(), "check_w1 green");
}

/// readFiles / modifiedFiles
#[test]
fn compaction_entry_details_have_file_lists() {
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, vec![ScriptTurn::text("SUM")], tight(u32::MAX, u32::MAX));
    say(&mut st, "fix it");
    push(
        &mut st,
        calls(vec![
            ("r1", "read", r#"{"path":"src/a.rs"}"#),
            ("e1", "edit", r#"{"path":"src/b.rs"}"#),
        ]),
    );
    push(&mut st, WireMsg::tool_result("r1", "fn a() {}"));
    push(&mut st, WireMsg::tool_result("e1", "ok"));
    say(&mut st, "continue");
    push(&mut st, WireMsg::assistant("done"));

    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut ui(),
    )
    .expect("compact");

    let entries = compaction_entries(&st);
    assert_eq!(entries.len(), 1);
    match &entries[0].body {
        EntryBody::Compaction {
            summary,
            details,
            from_hook,
            ..
        } => {
            assert!(!*from_hook);
            assert_eq!(details["readFiles"], json!(["src/a.rs"]));
            assert_eq!(details["modifiedFiles"], json!(["src/b.rs"]));
            assert!(
                summary.contains("<read-files>\nsrc/a.rs\n</read-files>"),
                "summary carries the lists"
            );
            assert!(summary.contains("<modified-files>\nsrc/b.rs\n</modified-files>"));
        }
        other => panic!("not a compaction: {other:?}"),
    }
    assert_eq!(chat_like(&eng.requests()), 1);
}

/// CompactionStart/End fields
#[test]
fn events_sequence() {
    let env = TestEnv::new();
    let (mut st, _eng) = state(
        &env,
        vec![ScriptTurn::text("reply"), ScriptTurn::text("SUM")],
        tight(u32::MAX, u32::MAX),
    );
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let kept_id = st
        .transcript
        .origin(1)
        .expect("kept reply has an entry")
        .to_string();
    let mut sink = VecSink::default();

    compact_now(&mut st, None, CompactReason::Manual, &mut sink, &mut ui()).expect("compact");

    let names: Vec<&str> = sink.0.iter().map(AgentEvent::name).collect();
    let start = names
        .iter()
        .position(|n| *n == "compaction_start")
        .expect("start");
    let end = names.iter().position(|n| *n == "compaction_end").expect("end");
    assert!(start < end);
    let appended = sink
        .0
        .iter()
        .position(|e| matches!(e, AgentEvent::EntryAppended { entry } if matches!(entry.body, EntryBody::Compaction { .. })))
        .expect("compaction entry announced");
    assert!(
        start < appended && appended < end,
        "entry is appended between start and end"
    );
    match &sink.0[start] {
        AgentEvent::CompactionStart { reason } => assert_eq!(*reason, CompactReason::Manual),
        other => panic!("{other:?}"),
    }
    match &sink.0[end] {
        AgentEvent::CompactionEnd {
            reason,
            result: Some(r),
            aborted,
            will_retry,
            error_message,
        } => {
            assert_eq!(*reason, CompactReason::Manual);
            assert!(!aborted && !will_retry && error_message.is_none());
            assert_eq!(r.summary, "SUM");
            assert_eq!(r.first_kept_entry_id, kept_id);
            assert!(r.usage.is_some());
        }
        other => panic!("{other:?}"),
    }
}

/// A compaction step other than the summary fails (the store's append): `CompactionEnd`
/// reports the error, `session_compact_failed` fires, and the wire is unchanged.
#[test]
fn non_summary_failure_reports_the_error_and_leaves_the_wire_unchanged() {
    let env = TestEnv::new();
    let store = Arc::new(FailOnceStore::new());
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("reply"),
        ScriptTurn::text("SUM"),
    ]));
    let engine: Arc<dyn Engine> = eng.clone();
    let hooks = Arc::new(FiredHooks::default());
    let mut st = build_with_store(
        &env,
        store.clone(),
        engine,
        vec![hooks.clone()],
        tight(u32::MAX, u32::MAX),
        Settings::default(),
    );
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let before = st.transcript.wire().to_vec();
    store.fail_next();
    let mut sink = VecSink::default();

    let err = compact_now(&mut st, None, CompactReason::Manual, &mut sink, &mut ui())
        .expect_err("the store failure fails the compaction");
    assert_eq!(err.code, codes::IO);
    assert_eq!(st.transcript.wire(), &before[..], "wire unchanged");
    assert!(compaction_entries(&st).is_empty(), "no compaction entry");
    assert!(
        sink.0.iter().any(|e| matches!(
            e,
            AgentEvent::CompactionEnd {
                aborted: false,
                result: None,
                will_retry: false,
                error_message: Some(m),
                ..
            } if !m.is_empty()
        )),
        "the error is reported: {:?}",
        sink.0
    );
    assert!(
        hooks
            .points
            .lock()
            .unwrap()
            .contains(&HookPoint::SessionCompactFailed),
        "session_compact_failed fired"
    );
}

// ---------------------------------------------------------------------------------------------
// Further tests

/// #75: the `estimated_tokens_after` of a compaction includes the frozen prefix (system prompt
/// and tool definitions), so the CTX meter and the "to N tokens" notice reflect what a fresh
/// session starts with, not just the summary.
#[test]
fn estimated_tokens_after_includes_the_prefix() {
    let env = TestEnv::new();
    let (mut st, _eng) = state(
        &env,
        vec![ScriptTurn::text("reply"), ScriptTurn::text("SUM")],
        tight(u32::MAX, u32::MAX),
    );
    say(&mut st, "hi");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let prefix_bytes = st.prefix.system_text().len() + st.prefix.tools_json.len();
    let mut sink = VecSink::default();

    compact_now(&mut st, None, CompactReason::Manual, &mut sink, &mut ui()).expect("compact");

    let after = sink
        .0
        .iter()
        .find_map(|e| match e {
            AgentEvent::CompactionEnd { result: Some(r), .. } => {
                Some((r.estimated_tokens_after, r.tokens_before))
            }
            _ => None,
        })
        .expect("a CompactionEnd with a result");
    // The fake counts one token per byte: the estimate must be the prefix (system prompt and
    // tools) plus the head (summary and kept tail), not the head alone.
    let head_bytes: u32 = st
        .transcript
        .wire()
        .iter()
        .map(|m| m.content.as_deref().unwrap_or("").len() as u32)
        .sum();
    // main (#97) counts the whole request with the engine's own count when it has one, so
    // the exact number is the engine's; the estimate still covers the prefix, not just the head.
    let (after, _before) = after;
    assert!(
        after >= prefix_bytes as u32 && after > head_bytes,
        "estimated_tokens_after must include the prefix: {after} (prefix {prefix_bytes}, head {head_bytes})"
    );
}

/// The cache warming setting and the CacheWarmingDecision hook both skip the prefill.
#[test]
fn idle_prefill_can_be_switched_off() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(Vec::new()));
    let engine: Arc<dyn Engine> = eng.clone();
    let off = Settings::from_values(json!({"cacheWarming": "off"}), json!({}));
    let mut st = build(&env, engine, Vec::new(), tight(u32::MAX, u32::MAX), off);
    say(&mut st, "hi");
    idle_prefill(&mut st, &mut VecSink::default(), &mut ui());
    assert!(eng.requests().is_empty(), "cacheWarming off");

    let eng2 = Arc::new(ScriptedEngine::new(Vec::new()));
    let engine2: Arc<dyn Engine> = eng2.clone();
    let hook = ReplyHook {
        point: HookPoint::CacheWarmingDecision,
        reply: HookReply::Cancel("busy".into()),
    };
    let mut st2 = build(
        &env,
        engine2,
        vec![Arc::new(hook)],
        tight(u32::MAX, u32::MAX),
        Settings::default(),
    );
    say(&mut st2, "hi");
    idle_prefill(&mut st2, &mut VecSink::default(), &mut ui());
    assert!(eng2.requests().is_empty(), "hook cancel");
}

/// H6 capability discovery: an engine that does not advertise zero-token prefill
/// (`EngineInfo::prefill_only == false`) gets no idle prefill request at all.
#[test]
fn idle_prefill_only_when_the_engine_advertises_it() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(Vec::new()));
    let engine: Arc<dyn Engine> = eng.clone();
    let mut st = build(
        &env,
        engine,
        Vec::new(),
        tight(u32::MAX, u32::MAX),
        Settings::default(),
    );
    assert!(st.info.prefill_only, "the scripted engine advertises H6");
    st.info.prefill_only = false;
    say(&mut st, "hi");
    let mut sink = VecSink::default();
    idle_prefill(&mut st, &mut sink, &mut ui());
    assert!(eng.requests().is_empty(), "no H6 request to an engine without it");
    assert!(sink.0.is_empty(), "skipped silently");

    st.info.prefill_only = true;
    idle_prefill(&mut st, &mut VecSink::default(), &mut ui());
    assert_eq!(eng.requests().len(), 1);
    assert_eq!(eng.requests()[0].max_tokens, 0);
}

/// Nothing to summarise (a lone user message is kept whole) is a no-op with no events.
#[test]
fn nothing_to_summarise_is_a_no_op() {
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, Vec::new(), tight(u32::MAX, u32::MAX));
    say(&mut st, "only message");
    let mut sink = VecSink::default();
    compact_now(&mut st, None, CompactReason::Manual, &mut sink, &mut ui()).expect("no-op");
    assert!(eng.requests().is_empty());
    assert!(sink.0.is_empty());
    assert_eq!(st.transcript.wire(), &[WireMsg::user("only message")][..]);
}
