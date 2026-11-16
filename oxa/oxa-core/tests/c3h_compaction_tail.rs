//! Wave H, lane cache: the kept tail of a compaction (A1: oversized tool results are elided, so
//! one long read does not make the next task compact again) and cuts on messages rewritten by a
//! declared extension rebase (OXA-REBASE-NONE-ORIGIN). Isolation: `TestEnv`; the scripted engine.

use oxa_compact::prompt;
use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{append_entry, compact_now, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, CancelToken, CompactReason, Engine, EntryBody, Exposure, FrozenPrefix, HeadlessUi, Hooks, Mode,
    Policy, Role, Source, ThinkingLevel, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value, VecSink,
    WireMsg,
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
    });
    SessionState {
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

/// A read-class tool that returns `size` bytes of text.
struct Big {
    spec: ToolSpec,
    size: usize,
}

impl Tool for Big {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let line = "2026-11-13 12:00:00 INFO worker heartbeat ok\n";
        ToolOutput::ok(line.repeat(self.size / line.len() + 1)[..self.size].to_string())
    }
}

fn big_tool(size: usize) -> Arc<dyn Tool> {
    Arc::new(Big {
        spec: ToolSpec {
            name: "readlog".to_string(),
            description: "test tool".to_string(),
            schema_json: "{}".to_string(),
            class: ToolClass::Read,
            exposure: Exposure::Direct,
            source: Source::Builtin,
        },
        size,
    })
}

fn compactions(st: &SessionState) -> usize {
    st.tree
        .entries()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Compaction { .. }))
        .count()
}

/// The fold of the stored branch reproduces the live wire (resume, W1).
fn assert_replays(st: &SessionState) {
    let branch: Vec<_> = st.tree.branch(st.tree.leaf()).into_iter().cloned().collect();
    assert_eq!(
        Transcript::project(&branch).wire(),
        st.transcript.wire(),
        "resume replays the same wire"
    );
}

/// A1 (tiny mechanics A/B): a long read in one task overflows the hard limit mid-run; before
/// the fix the kept tail (the current prompt onwards) kept the 9 KB result, the post-compaction
/// prompt stayed near the trigger and the NEXT small task compacted again. Now the result is
/// elided in the kept tail (the summary request saw all of it): exactly one compaction, and
/// the next prompts are far below the trigger.
#[test]
fn one_long_read_compacts_once() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("hello, what next?"),
        ScriptTurn::tool("c1", "readlog", "{}"),
        ScriptTurn::text("the log shows only heartbeats"),
        ScriptTurn::text("SUMMARY: the user asked to read the log; it holds heartbeats only"),
        ScriptTurn::text("small answer"),
    ]));
    let engine: Arc<dyn Engine> = eng.clone();
    let policy = CompactPolicy {
        soft: 6000,
        hard: 8000,
        keep_tokens: 4000,
        summary_max: 1024,
        enabled: true,
    };
    let mut st = build(&env, engine, vec![big_tool(9000)], policy, Settings::default());
    say(&mut st, "hello");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    say(&mut st, "read the big log");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let mut sink = VecSink::default();
    oxa_core::maybe_compact(&mut st, true, &mut sink, &mut HeadlessUi::default()).expect("compact");
    assert_eq!(compactions(&st), 1, "the long read compacts once");

    let reqs = eng.requests();
    let summary = reqs
        .iter()
        .find(|r| r.kind == "compaction")
        .expect("a summary request");
    assert!(
        summary
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.as_deref().is_some_and(|c| c.len() == 9000)),
        "the summary request sees the whole result"
    );
    let wire = st.transcript.wire();
    let tool = wire
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("the call keeps a result");
    assert_eq!(
        tool.content.as_deref(),
        Some(prompt::elided_tool_result(9000).as_str())
    );
    assert!(
        wire.iter()
            .any(|m| m.content.as_deref() == Some("read the big log")),
        "the task is kept"
    );
    assert_replays(&st);

    say(&mut st, "a small task");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    oxa_core::maybe_compact(&mut st, true, &mut sink, &mut HeadlessUi::default()).expect("no compact");
    assert_eq!(compactions(&st), 1, "the next small task does not compact again");
    let last = eng.requests().into_iter().rfind(|r| r.kind == "chat").unwrap();
    let prompt_tokens = last.usage.expect("usage").prompt;
    assert!(
        prompt_tokens < policy.soft / 3,
        "post-compaction prompt {prompt_tokens} is far below the trigger"
    );
}

/// A tool result up to `ELIDE_MIN_TOKENS` (and up to half of `keepRecentTokens`) stays verbatim.
#[test]
fn small_kept_results_stay_verbatim() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", "readlog", "{}"),
        ScriptTurn::text("done"),
        ScriptTurn::text("SUMMARY"),
    ]));
    let engine: Arc<dyn Engine> = eng.clone();
    let policy = CompactPolicy {
        soft: 1,
        hard: u32::MAX,
        keep_tokens: 4000,
        summary_max: 1024,
        enabled: true,
    };
    let mut st = build(&env, engine, vec![big_tool(900)], policy, Settings::default());
    say(&mut st, "read");
    run(&mut st);
    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("compact");
    let tool = st
        .transcript
        .wire()
        .iter()
        .find(|m| m.role == Role::Tool)
        .cloned()
        .expect("kept");
    assert_eq!(tool.content.map(|c| c.len()), Some(900));
}

/// OXA-REBASE-NONE-ORIGIN: after a declared extension rebase that rewrote some messages (they
/// have no entry origin), a compaction cut before, on and after a rewritten message works, and
/// a resume replays the same wire.
#[test]
fn cuts_around_rewritten_messages() {
    // keep 34: the cut lands on `u2` (origin intact), before the rewritten reply; keep 16: on the
    // rewritten reply itself; keep 1: on `tail reply`, after it.
    for (keep, first_kept) in [(34u32, "u2"), (16, "rewritten reply two"), (1, "tail reply")] {
        let env = TestEnv::new();
        let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("SUMMARY")]));
        let engine: Arc<dyn Engine> = eng.clone();
        let policy = CompactPolicy {
            soft: u32::MAX,
            hard: u32::MAX,
            keep_tokens: keep,
            summary_max: 1024,
            enabled: true,
        };
        let mut st = build(&env, engine, Vec::new(), policy, Settings::default());
        say(&mut st, "u1");
        push(&mut st, WireMsg::assistant("a1 original"));
        say(&mut st, "u2");
        push(&mut st, WireMsg::assistant("a2 original"));
        // The extension rewrites the replies (positions 1 and 3 lose their origins).
        let rewritten = vec![
            WireMsg::user("u1"),
            WireMsg::assistant("a1 rewritten"),
            WireMsg::user("u2"),
            WireMsg::assistant("rewritten reply two"),
        ];
        append_entry(
            &mut st,
            EntryBody::Custom {
                custom_type: Transcript::EXTENSION_REBASE_ENTRY.to_string(),
                data: serde_json::json!({ "messages": Transcript::encode_rebase(&rewritten) }),
            },
        )
        .expect("declared rebase");
        assert!(st.transcript.origin(3).is_none());
        say(&mut st, "tail");
        push(&mut st, WireMsg::assistant("tail reply"));
        let before = st.transcript.wire().to_vec();
        compact_now(
            &mut st,
            None,
            CompactReason::Manual,
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap_or_else(|e| panic!("keep {keep}: compaction failed: {e:?}"));
        let wire = st.transcript.wire();
        assert_eq!(
            wire[0],
            WireMsg::user(prompt::summary_user_message("SUMMARY")),
            "keep {keep}"
        );
        let kept = &wire[1..];
        assert!(
            before.ends_with(kept),
            "keep {keep}: the tail is a suffix of the old wire"
        );
        assert_eq!(
            kept[0].content.as_deref(),
            Some(first_kept),
            "keep {keep}: {kept:?}"
        );
        assert_replays(&st);
    }
}

/// The cut lands exactly on a rewritten (origin-less) message: its tail is stored by value.
#[test]
fn cut_on_a_rewritten_message_is_stored_by_value() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("SUMMARY")]));
    let engine: Arc<dyn Engine> = eng.clone();
    let policy = CompactPolicy {
        soft: u32::MAX,
        hard: u32::MAX,
        keep_tokens: 1,
        summary_max: 1024,
        enabled: true,
    };
    let mut st = build(&env, engine, Vec::new(), policy, Settings::default());
    say(&mut st, "u1");
    push(&mut st, WireMsg::assistant("a1"));
    say(&mut st, "u2");
    push(&mut st, WireMsg::assistant("a2"));
    let rewritten = vec![
        WireMsg::user("u1"),
        WireMsg::assistant("a1"),
        WireMsg::user("u2"),
        WireMsg::assistant("a2 rewritten by the extension"),
    ];
    append_entry(
        &mut st,
        EntryBody::Custom {
            custom_type: Transcript::EXTENSION_REBASE_ENTRY.to_string(),
            data: serde_json::json!({ "messages": Transcript::encode_rebase(&rewritten) }),
        },
    )
    .expect("declared rebase");
    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("a cut on an origin-less message works");
    assert_eq!(
        st.transcript.wire()[1..].to_vec(),
        vec![WireMsg::assistant("a2 rewritten by the extension")]
    );
    let details = st
        .tree
        .entries()
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::Compaction {
                details, first_kept, ..
            } => Some((details.clone(), first_kept.clone())),
            _ => None,
        })
        .expect("compaction entry");
    assert_eq!(details.1, "");
    assert!(details.0.get("keptMessages").is_some(), "{:?}", details.0);
    assert_replays(&st);
}
