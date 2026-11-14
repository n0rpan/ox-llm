//! OXA-LOCAL-FIRST R4 (lane l5-compact): the mechanical summary fallback under profile `local`
//! (a failed, cut or twice-cancelled summary, and a failed overflow summary) and the unchanged
//! `pi` behaviour, over the scripted fake engine.
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_compact::prompt;
use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{append_entry, maybe_compact, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{Fault, Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, CancelToken, CompactReason, Engine, EntryBody, Finish, FrozenPrefix, HeadlessUi, Level, Mode,
    Policy, Profile, Role, ThinkingLevel, ToolCall, Usage, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicBool;
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
    }
}

fn state(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    settings: Settings,
    profile: Profile,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let mut st = build(env, engine, policy(1), settings);
    st.stats.profile = profile;
    (st, eng)
}

fn say(st: &mut SessionState, text: &str) {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: Vec::new(),
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

/// `soft` just below the current context (a threshold compaction is due).
fn crossed(st: &mut SessionState) {
    st.compact.soft = st.transcript.context() - 1;
}

fn kinds(reqs: &[Recorded]) -> Vec<&str> {
    reqs.iter().map(|r| r.kind.as_str()).collect()
}

fn idle_off() -> Settings {
    Settings::from_values(json!({"oxa": {"compaction": {"idleSummary": false}}}), json!({}))
}

fn push(st: &mut SessionState, wire: WireMsg) {
    append_entry(
        st,
        EntryBody::Msg {
            wire,
            parts: Vec::new(),
        },
    )
    .expect("append message");
}

fn read_call(id: &str, path: &str) -> WireMsg {
    WireMsg {
        role: Role::Assistant,
        content: Some(String::new()),
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            args_raw: format!(r#"{{"path":"{path}"}}"#),
        }],
        tool_call_id: None,
    }
}

/// The history every test starts from: a first task with one `read` call, then a second
/// prompt whose run (one scripted reply) has just ended.
fn history(st: &mut SessionState) {
    say(st, "task one: fix the parser");
    push(st, read_call("c1", "src/parse.rs"));
    push(st, WireMsg::tool_result("c1", "fn parse() {}"));
    push(st, WireMsg::assistant("fixed"));
    say(st, &format!("task two: {}", "é".repeat(310)));
    run(st);
}

/// The golden of the mechanical summary for [`history`] (cut before the last reply).
fn golden() -> String {
    format!(
        "{}\n\n## User messages\n1. task one: fix the parser\n2. task two: {} [...]\n\n<read-files>\nsrc/parse.rs\n</read-files>\n\nTool results left out: 1.",
        prompt::MECHANICAL_SUMMARY_HEADER,
        "é".repeat(290)
    )
}

fn mechanical_end(events: &[AgentEvent]) -> bool {
    events.iter().any(|e| {
        matches!(e, AgentEvent::CompactionEnd { result: Some(r), .. } if r.details["mechanical"] == json!(true))
    })
}

fn warned(events: &[AgentEvent]) -> bool {
    events.iter().any(|e| {
        matches!(e, AgentEvent::Notice { level: Level::Warning, text } if text.contains("automatic record"))
    })
}

fn threshold_with(
    summary: ScriptTurn,
    profile: Profile,
) -> (SessionState, Arc<ScriptedEngine>, bool, Vec<AgentEvent>) {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![ScriptTurn::text("done two"), summary],
        idle_off(),
        profile,
    );
    history(&mut st);
    crossed(&mut st);
    let mut sink = VecSink::default();
    let done = maybe_compact(&mut st, true, &mut sink, &mut HeadlessUi::default()).expect("maybe_compact");
    (st, eng, done, sink.0)
}

#[test]
fn failed_summary_becomes_mechanical_under_local() {
    let fault = || {
        ScriptTurn::fault(Fault::Http {
            status: 400,
            body: "bad request".into(),
        })
    };
    let (st, eng, done, events) = threshold_with(fault(), Profile::Local);
    assert!(done, "the session compacts without the model");
    assert!(mechanical_end(&events) && warned(&events));
    assert_eq!(
        st.transcript.wire()[0],
        WireMsg::user(prompt::summary_user_message(&golden()))
    );
    assert_eq!(
        st.transcript.wire()[1],
        WireMsg::assistant("done two"),
        "the kept tail"
    );
    assert_eq!(kinds(&eng.requests()), ["chat", "compaction", "prefill"]);

    // pi: today's failure, the wire unchanged.
    let (st, _, done, events) = threshold_with(fault(), Profile::Pi);
    assert!(!done && !mechanical_end(&events) && !warned(&events));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionEnd {
            error_message: Some(_),
            ..
        }
    )));
    assert_eq!(st.transcript.wire()[0], WireMsg::user("task one: fix the parser"));
}

#[test]
fn cut_summary_becomes_mechanical_under_local() {
    let cut = || ScriptTurn {
        finish: Finish::Length,
        ..ScriptTurn::text("## Goal\nhalf a summ")
    };
    let (st, _, done, events) = threshold_with(cut(), Profile::Local);
    assert!(done && mechanical_end(&events));
    assert_eq!(
        st.transcript.wire()[0],
        WireMsg::user(prompt::summary_user_message(&golden()))
    );
    let (_, _, done, _) = threshold_with(cut(), Profile::Pi);
    assert!(!done, "pi: a cut summary fails as today");
}

#[test]
fn cancelled_twice_becomes_mechanical_under_local() {
    for profile in [Profile::Local, Profile::Pi] {
        let env = TestEnv::new();
        let (mut st, eng) = state(&env, vec![ScriptTurn::text("done two")], idle_off(), profile);
        history(&mut st);
        crossed(&mut st);
        let mut outcomes = Vec::new();
        for _ in 0..2 {
            // The user's input is queued: the background summary is cancelled before it is sent.
            st.shared.queue.lock().unwrap().follow_up.push("more".into());
            let mut sink = VecSink::default();
            let done = maybe_compact(&mut st, true, &mut sink, &mut HeadlessUi::default()).expect("compact");
            outcomes.push((done, mechanical_end(&sink.0)));
            st.shared.queue.lock().unwrap().follow_up.clear();
        }
        assert_eq!(kinds(&eng.requests()), ["chat"], "no summary request was sent");
        match profile {
            Profile::Local => {
                assert_eq!(outcomes, [(false, false), (true, true)]);
                assert_eq!(
                    st.transcript.wire()[0],
                    WireMsg::user(prompt::summary_user_message(&golden()))
                );
            }
            Profile::Pi => assert_eq!(outcomes, [(false, false), (false, false)]),
        }
    }
}

/// An overflow (mid-run) compaction whose summary fails no longer blocks the session.
#[test]
fn failed_overflow_summary_does_not_block_under_local() {
    for profile in [Profile::Local, Profile::Pi] {
        let env = TestEnv::new();
        let (mut st, eng) = state(
            &env,
            vec![
                ScriptTurn::fault(Fault::Overflow),
                ScriptTurn::fault(Fault::Http {
                    status: 400,
                    body: "bad request".into(),
                }),
                ScriptTurn::text("answer after the compaction"),
            ],
            idle_off(),
            profile,
        );
        say(&mut st, "task one: fix the parser");
        push(&mut st, read_call("c1", "src/parse.rs"));
        push(&mut st, WireMsg::tool_result("c1", "fn parse() {}"));
        push(&mut st, WireMsg::assistant("fixed"));
        say(&mut st, "task two");
        let mut sink = VecSink::default();
        let end = run_loop(&mut st, &mut sink, &mut HeadlessUi::default()).expect("run_loop");
        match profile {
            Profile::Local => {
                assert_eq!(end, RunEnd::Stopped, "the session continues");
                assert!(mechanical_end(&sink.0));
                let wire = st.transcript.wire();
                let head = format!(
                    "{}\n\n## User messages\n1. task one: fix the parser\n\n<read-files>\nsrc/parse.rs\n</read-files>\n\nTool results left out: 1.",
                    prompt::MECHANICAL_SUMMARY_HEADER
                );
                assert_eq!(wire[0], WireMsg::user(prompt::summary_user_message(&head)));
                assert_eq!(wire[1], WireMsg::user("task two"), "the prompt is kept");
                assert_eq!(wire[2], WireMsg::assistant("answer after the compaction"));
                assert_eq!(kinds(&eng.requests()), ["chat", "compaction", "chat"]);
            }
            Profile::Pi => assert!(matches!(end, RunEnd::Failed(_)), "pi: blocked as today"),
        }
    }
}

/// `maybe_compact` mid-run (above `hard`) with a failed summary: `Ok(true)` under local.
#[test]
fn mid_run_failure_is_mechanical_under_local() {
    let env = TestEnv::new();
    let (mut st, _) = state(
        &env,
        vec![ScriptTurn::fault(Fault::Http {
            status: 400,
            body: "bad".into(),
        })],
        idle_off(),
        Profile::Local,
    );
    say(&mut st, "task one");
    push(&mut st, WireMsg::assistant("ok"));
    say(&mut st, "task two");
    st.compact.hard = 10;
    st.transcript.set_usage(&Usage {
        prompt: 500,
        completion: 0,
        cached: 0,
        cached_missing: false,
    });
    let mut sink = VecSink::default();
    assert!(maybe_compact(&mut st, false, &mut sink, &mut HeadlessUi::default()).expect("not blocked"));
    assert!(sink.0.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionStart {
            reason: CompactReason::Overflow
        }
    )));
    assert!(mechanical_end(&sink.0));
    assert_eq!(st.transcript.wire()[1], WireMsg::user("task two"));
}

/// Self-feedback: the warning says how many assistant/tool turns the record dropped from
/// memory, and the event details carry the number (the TUI footer and the RPC/JSON events show
/// `mechanical`). The summary fails twice (a retry fails too).
#[test]
fn degraded_context_notice_counts_the_dropped_turns() {
    let fault = || {
        ScriptTurn::fault(Fault::Http {
            status: 400,
            body: "bad request".into(),
        })
    };
    let env = TestEnv::new();
    let (mut st, _) = state(
        &env,
        vec![ScriptTurn::text("done two"), fault(), fault()],
        idle_off(),
        Profile::Local,
    );
    history(&mut st);
    crossed(&mut st);
    let mut sink = VecSink::default();
    assert!(maybe_compact(&mut st, true, &mut sink, &mut HeadlessUi::default()).expect("compact"));
    let text = sink
        .0
        .iter()
        .find_map(|e| match e {
            AgentEvent::Notice {
                level: Level::Warning,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .expect("a warning notice");
    // The read call, its result and "fixed" were summarised away.
    assert!(
        text.starts_with("context compacted without a model summary: 3 assistant/tool turns were dropped from memory"),
        "{text}"
    );
    let dropped = sink.0.iter().find_map(|e| match e {
        AgentEvent::CompactionEnd { result: Some(r), .. } => Some(r.details["droppedTurns"].clone()),
        _ => None,
    });
    assert_eq!(dropped, Some(json!(3)));
}
