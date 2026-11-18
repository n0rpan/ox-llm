//! C1 regression tests: what survives compaction.
//! Compaction rewrites the transcript (the wire) only. The frozen prefix (system sections, hook
//! sections, the goal section), the queued follow-ups and the newest user prompt must still be
//! there afterwards. Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::prompt::{goal_start, GOAL_CONTINUE};
use oxa_core::{append_entry, compact_now, open_epoch, Queue, SessionState, Shared, Stats, Toolset};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    CancelToken, CommandSpec, CompactReason, Engine, EntryBody, FrozenPrefix, HookPoint, HookReply, Hooks,
    Mode, OxaError, Policy, ThinkingLevel, Tool, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{Goal, GoalBudget, MemStore, SessionStore, Transcript, Tree};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

/// Policy that keeps only the newest user message and what follows it (`keep_tokens: 1`), so
/// that a compaction cuts at the last prompt.
fn tight() -> CompactPolicy {
    CompactPolicy {
        soft: u32::MAX,
        hard: u32::MAX,
        keep_tokens: 1,
        summary_max: 1024,
        enabled: true,
    }
}

fn build(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Vec<Arc<dyn Hooks>>) -> SessionState {
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
        goal: None,
        output_schema: None,
        engine,
        store,
        hooks,
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings: Settings::default(),
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
        compact: tight(),
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

/// A session over a scripted engine with one summary reply per compaction.
fn state(env: &TestEnv, hooks: Vec<Arc<dyn Hooks>>) -> SessionState {
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("SUMMARY ONE"),
        ScriptTurn::text("SUMMARY TWO"),
    ]));
    let engine: Arc<dyn Engine> = eng;
    build(env, engine, hooks)
}

fn ui() -> oxa_proto::HeadlessUi {
    oxa_proto::HeadlessUi::default()
}

/// Append a user message the way `Session::prompt` does.
fn say(st: &mut SessionState, text: &str) {
    push(st, WireMsg::user(text));
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

/// Compact now, as a manual `/compact` does.
fn compact(st: &mut SessionState) {
    compact_now(
        st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut ui(),
    )
    .expect("compact");
}

/// How many compaction entries the session tree holds.
fn compactions(st: &SessionState) -> usize {
    st.tree
        .entries()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Compaction { .. }))
        .count()
}

/// Give the wire older history and a newest prompt, so that a compaction has something to
/// summarise (an empty wire has no cut point and the compaction is skipped).
fn seed_history(st: &mut SessionState, newest: &str) {
    say(st, "older task");
    push(st, WireMsg::assistant("older work"));
    say(st, newest);
}

/// A hook set that adds one prompt section and continues everywhere.
struct SectionHook;

impl Hooks for SectionHook {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
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
        vec![("hook-policy".to_string(), "HOOK POLICY TEXT".to_string())]
    }
}

/// The prefix section with this name, if it is there.
fn section<'a>(st: &'a SessionState, name: &str) -> Option<&'a str> {
    st.prefix
        .sections
        .iter()
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, text)| text.as_str())
}

/// Whether the wire holds this user text as a message of its own.
fn has_user(st: &SessionState, text: &str) -> bool {
    st.transcript
        .wire()
        .iter()
        .any(|m| m.content.as_deref() == Some(text))
}

// ---------------------------------------------------------------------------------------------
// Tests

/// The goal section and the goal-continue prompt both survive two compactions.
#[test]
fn goal_section_and_continue_survive_two_compactions() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    let objective = "Ship the compaction regression tests.";
    let goal = Goal::new(objective.to_string(), GoalBudget::default(), 1000).expect("goal");
    st.goal = Some(Arc::new(oxa_core::goal::GoalRuntime::new(goal)));
    open_epoch(&mut st, "goal", true, &mut VecSink::default()).expect("goal epoch");
    let expected = goal_start(objective);
    assert!(
        section(&st, "goal").is_some_and(|t| t.contains(expected.as_str())),
        "goal section in the prefix"
    );

    // The goal-continue prompt is the newest message, so the keep_tokens: 1 cut lands on it and
    // it is kept. A manual compaction moves the cut forward to a valid point, so a trailing
    // assistant reply after the prompt would be summarised (only a mid-run cut is clamped to
    // the last user message).
    say(&mut st, "the task");
    push(&mut st, WireMsg::assistant("working"));
    push(&mut st, WireMsg::user(GOAL_CONTINUE));
    compact(&mut st);
    assert!(
        section(&st, "goal").is_some_and(|t| t.contains(expected.as_str())),
        "goal section after one compaction"
    );
    assert!(
        has_user(&st, GOAL_CONTINUE),
        "goal-continue prompt after one compaction"
    );

    // The next continuation prompt is again the newest message when the second compaction runs.
    push(&mut st, WireMsg::assistant("more work"));
    push(&mut st, WireMsg::user(GOAL_CONTINUE));
    compact(&mut st);
    assert!(
        section(&st, "goal").is_some_and(|t| t.contains(expected.as_str())),
        "goal section after two compactions"
    );
    assert!(
        has_user(&st, GOAL_CONTINUE),
        "goal-continue prompt after two compactions"
    );
}

/// A queued follow-up is not touched by a compaction.
#[test]
fn queued_follow_up_survives_compaction() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    st.shared
        .queue
        .lock()
        .expect("queue")
        .follow_up
        .push("follow up later".to_string());

    seed_history(&mut st, "first prompt");
    compact(&mut st);
    assert_eq!(compactions(&st), 1, "first compaction ran");
    push(&mut st, WireMsg::assistant("more work"));
    say(&mut st, "second prompt");
    compact(&mut st);
    assert_eq!(compactions(&st), 2, "second compaction ran");

    let queue = st.shared.queue.lock().expect("queue");
    assert_eq!(
        queue.follow_up,
        vec!["follow up later".to_string()],
        "queued follow-up after two compactions"
    );
    assert!(
        !has_user(&st, "follow up later"),
        "a queued follow-up is not in the wire"
    );
}

/// A hook prompt section stays in the prefix after compaction.
#[test]
fn hook_prompt_sections_survive_compaction() {
    let env = TestEnv::new();
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(SectionHook)];
    let mut st = state(&env, hooks);
    open_epoch(&mut st, "hooks", true, &mut VecSink::default()).expect("hook epoch");
    let before = section(&st, "hook-policy").expect("hook section in the prefix");
    assert!(before.contains("HOOK POLICY TEXT"), "{before}");

    seed_history(&mut st, "newest prompt");
    compact(&mut st);
    assert_eq!(compactions(&st), 1, "compaction ran");

    let after = section(&st, "hook-policy").expect("hook section after compaction");
    assert!(after.contains("HOOK POLICY TEXT"), "{after}");
}
