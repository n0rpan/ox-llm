//! C4 acceptance tests: Tree navigation, fork, clone.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_compact::prompt::BRANCH_SUMMARY_PREAMBLE;
use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{
    append_entry, clone_current, fork_at, fork_from, navigate, run_loop, ForkPosition, Queue, RunEnd,
    SessionState, Shared, Stats, Toolset,
};
use oxa_fake::{check_w1, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, rebase_why, CancelToken, CommandSpec, Engine, Entry, EntryBody, EventSink, FrozenPrefix,
    HeadlessUi, HookPoint, HookReply, Hooks, Mode, OxaError, Policy, Role, ThinkingLevel, Tool, Ui, Value,
    VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionRef, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

/// A session over a scripted engine and an in-memory store. Returns the state, the engine and
/// the store for inspection.
fn state_with(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    hooks: Vec<Arc<dyn Hooks>>,
) -> (SessionState, Arc<ScriptedEngine>, Arc<MemStore>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let mem = Arc::new(MemStore::new());
    let store: Arc<dyn SessionStore> = mem.clone();
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
        wait_wake: CancelToken::new(),
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
        wake: Default::default(),
    });
    let st = SessionState {
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
    };
    (st, eng, mem)
}

/// Append a user message the way `Session::prompt` does; returns its id.
fn say(st: &mut SessionState, text: &str) -> String {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("append user")
    .id
}

/// Append an assistant message (as a completed reply); returns its id.
fn reply(st: &mut SessionState, text: &str) -> String {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::assistant(text),
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("append assistant")
    .id
}

/// Id of the message of `role` with `text` on the whole tree.
fn find_msg(st: &SessionState, role: Role, text: &str) -> String {
    st.tree
        .entries()
        .iter()
        .find(|e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.role == role && wire.content.as_deref() == Some(text)))
        .map(|e| e.id.clone())
        .expect("message on the tree")
}

/// The parent of the active leaf (the navigation's new leaf, one usage entry below it).
fn leaf_parent(st: &SessionState) -> Option<String> {
    st.tree.get(st.tree.leaf()).and_then(|e| e.parent.clone())
}

fn go(st: &mut SessionState, target: &str, summarize: bool) -> Result<Option<String>, OxaError> {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    navigate(st, target, summarize, None, &mut sink, &mut ui)
}

fn run(st: &mut SessionState) {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let end = run_loop(st, &mut sink, &mut ui).expect("run_loop");
    assert_eq!(end, RunEnd::Stopped);
}

/// A hook set that answers `SessionBeforeTree` with a fixed reply and continues otherwise.
struct TreeHook {
    reply: HookReply,
}

impl Hooks for TreeHook {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p == HookPoint::SessionBeforeTree {
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

/// The rebase logs written after index `from`.
fn rebases_since(st: &SessionState, from: usize) -> Vec<String> {
    st.transcript.rebases()[from..]
        .iter()
        .map(|r| r.why.to_string())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Acceptance

/// exactly one RebaseLog with leaf_move
#[test]
fn leaf_move_is_one_rebase() {
    let env = TestEnv::new();
    let (mut st, _eng, _store) = state_with(&env, Vec::new(), Vec::new());
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    let _a2 = reply(&mut st, "four");

    let before = st.transcript.rebases().len();
    let editor = go(&mut st, &a1, false).expect("navigate");
    assert_eq!(editor, None, "an assistant target gives no editor text");
    assert_eq!(
        rebases_since(&st, before),
        vec![rebase_why::LEAF_MOVE.to_string()]
    );
    assert_eq!(
        leaf_parent(&st),
        Some(a1.clone()),
        "the new branch continues at the target"
    );
    assert_eq!(
        st.transcript.wire().to_vec(),
        vec![WireMsg::user("one"), WireMsg::assistant("two")]
    );
}

/// with and without summarize
#[test]
fn optional_branch_summary_entry() {
    let env = TestEnv::new();

    // Without summarize: no summary entry and no summary request.
    let (mut st, eng, _store) = state_with(&env, Vec::new(), Vec::new());
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    let _a2 = reply(&mut st, "four");
    go(&mut st, &a1, false).expect("navigate without summary");
    assert!(!st
        .tree
        .entries()
        .iter()
        .any(|e| matches!(e.body, EntryBody::BranchSummary { .. })));
    assert!(eng.requests().iter().all(|r| r.kind != "branch_summary"));

    // With summarize: one summary entry at the new leaf, from the old leaf, on the wire.
    let (mut st, eng, _store) = state_with(
        &env,
        vec![ScriptTurn::text("the summary of the abandoned part")],
        Vec::new(),
    );
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    let a2 = reply(&mut st, "four");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    navigate(&mut st, &a1, true, Some("focus on errors"), &mut sink, &mut ui).expect("navigate with summary");

    let summaries: Vec<&Entry> = st
        .tree
        .entries()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::BranchSummary { .. }))
        .collect();
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].parent.as_deref(),
        Some(a1.as_str()),
        "the summary sits at the new leaf"
    );
    match &summaries[0].body {
        EntryBody::BranchSummary {
            from,
            summary,
            from_hook,
            ..
        } => {
            assert_eq!(from, &a2);
            assert!(summary.contains("the summary of the abandoned part"));
            assert!(summary.contains(BRANCH_SUMMARY_PREAMBLE));
            assert!(!from_hook);
        }
        other => panic!("not a branch summary: {other:?}"),
    }
    assert_eq!(
        st.transcript.wire().len(),
        3,
        "the summary is the last message after the target branch"
    );
    assert!(st.transcript.wire()[2]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("the summary of the abandoned part"));

    let reqs = eng.requests();
    let bs: Vec<_> = reqs.iter().filter(|r| r.kind == "branch_summary").collect();
    assert_eq!(bs.len(), 1, "one summary request");
    assert_eq!(bs[0].messages.len(), 5, "the old wire (4) plus one instruction");
    assert!(bs[0].messages[4]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("focus on errors"));
    assert!(!bs[0].thinking, "summaries run with thinking off");
}

/// entries appended
#[test]
fn model_and_thinking_recorded_on_new_branch() {
    let env = TestEnv::new();
    let (mut st, _eng, _store) = state_with(&env, Vec::new(), Vec::new());
    append_entry(&mut st, EntryBody::ModelChange("m1".to_string())).expect("model record");
    append_entry(&mut st, EntryBody::ThinkingLevelChange(ThinkingLevel::Off)).expect("thinking record");
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    let _a2 = reply(&mut st, "four");

    // The session moved on to another model and level; the target branch recorded the old ones.
    st.model = "m2".to_string();
    st.thinking = ThinkingLevel::Medium;
    go(&mut st, &a1, false).expect("navigate");

    let branch: Vec<Entry> = st.tree.branch(st.tree.leaf()).into_iter().cloned().collect();
    assert!(branch
        .iter()
        .any(|e| matches!(&e.body, EntryBody::ModelChange(m) if m == "m2")));
    assert!(branch
        .iter()
        .any(|e| matches!(&e.body, EntryBody::ThinkingLevelChange(t) if *t == ThinkingLevel::Medium)));
    // The current values stay in force.
    assert_eq!(st.model, "m2");
    assert_eq!(st.thinking, ThinkingLevel::Medium);
}

/// prefill request after the move
#[test]
fn park_prefill_issued() {
    let env = TestEnv::new();
    let (mut st, eng, _store) = state_with(&env, Vec::new(), Vec::new());
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    let _a2 = reply(&mut st, "four");
    go(&mut st, &a1, false).expect("navigate");

    let reqs = eng.requests();
    let pre: Vec<_> = reqs.iter().filter(|r| r.kind == "prefill").collect();
    assert_eq!(pre.len(), 1, "one prefill after the move");
    assert_eq!(pre[0].max_tokens, 0);
    assert!(pre[0].background && pre[0].snapshot_keep);
    assert_eq!(pre[0].messages.len(), 2, "prefill covers the wire after the move");
    assert!(
        matches!(&st.tree.get(st.tree.leaf()).expect("leaf").body, EntryBody::Usage { kind, .. } if kind == "prefill"),
        "the prefill usage is recorded under the new branch"
    );
}

/// user-message target: editor text returned, leaf moves to its parent
#[test]
fn user_message_target_refills_editor() {
    let env = TestEnv::new();
    let (mut st, _eng, _store) = state_with(&env, Vec::new(), Vec::new());
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let u2 = say(&mut st, "three");
    let _a2 = reply(&mut st, "four");

    let editor = go(&mut st, &u2, false).expect("navigate to a user message");
    assert_eq!(editor, Some("three".to_string()));
    assert_eq!(
        leaf_parent(&st),
        Some(a1),
        "the leaf moved to the parent of the user message"
    );
    assert_eq!(st.transcript.wire().len(), 2);
}

/// hook replies
#[test]
fn hook_before_tree_cancel_and_summary() {
    let env = TestEnv::new();

    // Cancel: nothing moves, nothing is requested.
    let cancel: Arc<dyn Hooks> = Arc::new(TreeHook {
        reply: HookReply::Cancel("not now".to_string()),
    });
    let (mut st, eng, _store) = state_with(&env, vec![ScriptTurn::text("never used")], vec![cancel]);
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    let a2 = reply(&mut st, "four");
    let rebases_before = st.transcript.rebases().len();
    let err = go(&mut st, &a1, true).expect_err("cancelled");
    assert_eq!(err.code, codes::ABORTED);
    assert_eq!(st.tree.leaf(), a2);
    assert_eq!(st.transcript.rebases().len(), rebases_before);
    assert!(eng.requests().is_empty(), "a cancelled navigation sends nothing");

    // Summary: the hook's text is stored as the summary, no engine summary request is made.
    let hook_summary: Arc<dyn Hooks> = Arc::new(TreeHook {
        reply: HookReply::Summary {
            text: "from the hook".to_string(),
            details: json!({"readFiles": []}),
        },
    });
    let (mut st, eng, _store) = state_with(&env, Vec::new(), vec![hook_summary]);
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    let _a2 = reply(&mut st, "four");
    go(&mut st, &a1, true).expect("navigate with hook summary");
    let stored: Vec<&EntryBody> = st
        .tree
        .entries()
        .iter()
        .map(|e| &e.body)
        .filter(|b| matches!(b, EntryBody::BranchSummary { .. }))
        .collect();
    assert_eq!(stored.len(), 1);
    match stored[0] {
        EntryBody::BranchSummary {
            summary, from_hook, ..
        } => {
            assert!(*from_hook);
            assert!(summary.contains("from the hook"));
            assert!(
                !summary.contains(BRANCH_SUMMARY_PREAMBLE),
                "hook text is stored as given"
            );
        }
        _ => unreachable!(),
    }
    assert!(eng.requests().iter().all(|r| r.kind != "branch_summary"));
}

/// #703: a changed fork head invalidates active usage, but a clone keeps it.
#[test]
fn fork_invalidates_context_usage_but_clone_preserves_it() {
    let env = TestEnv::new();
    let (mut st, _eng, _store) = state_with(
        &env,
        vec![ScriptTurn::text("two"), ScriptTurn::text("four")],
        Vec::new(),
    );
    say(&mut st, "one");
    run(&mut st);
    let second = say(&mut st, "three");
    run(&mut st);
    let before = st.stats.clone();
    assert!(before.context_tokens > 0);
    assert!(st.last_usage.is_some());
    let mut ui = HeadlessUi::default();
    clone_current(&mut st, &mut ui).expect("clone");
    assert_eq!(st.stats.context_tokens, before.context_tokens);
    assert!(st.last_usage.is_some(), "clone has the same active context");

    fork_at(&mut st, &second, &mut ui).expect("fork before second prompt");
    assert_eq!(st.transcript.wire().len(), 2, "first exchange is retained");
    assert_eq!(st.stats.context_tokens, st.transcript.context());
    assert_eq!(
        st.stats.context_tokens, 0,
        "context is unknown until the next request"
    );
    assert!(
        st.last_usage.is_none(),
        "old head's cache usage is no longer applicable"
    );
    assert_eq!(st.stats.prompt_tokens, before.prompt_tokens);
    assert_eq!(st.stats.completion_tokens, before.completion_tokens);
}

/// W3 across fork
#[test]
fn fork_keeps_prefix_bytes() {
    let env = TestEnv::new();
    let (mut st, eng, _store) = state_with(
        &env,
        vec![
            ScriptTurn::text("two"),
            ScriptTurn::text("four"),
            ScriptTurn::text("six"),
        ],
        Vec::new(),
    );
    let old = st.sref.clone();
    say(&mut st, "one");
    run(&mut st);
    say(&mut st, "three");
    run(&mut st);
    let u2 = find_msg(&st, Role::User, "three");
    let u2_parent = st.tree.get(&u2).and_then(|e| e.parent.clone()).expect("parent");
    let old_wire = st.transcript.wire().to_vec();
    let before = st.transcript.rebases().len();

    // pi's fork (position `before`, its default): the new session ends at the PARENT of the
    // chosen user message and the message's text comes back for the editor.
    let mut ui = HeadlessUi::default();
    let forked = fork_from(&mut st, &u2, ForkPosition::Before, &mut ui).expect("fork");
    assert_eq!(forked.text.as_deref(), Some("three"));
    let fork = forked.sref;
    assert_ne!(fork.id, old.id);
    assert_eq!(st.sref, fork);
    assert_eq!(
        st.tree.leaf(),
        u2_parent,
        "the leaf is the parent of the chosen user message"
    );
    assert!(
        st.tree.get(&u2).is_none(),
        "the chosen message is not in the fork"
    );
    assert_eq!(
        st.transcript.wire().to_vec(),
        old_wire[..2].to_vec(),
        "the wire up to the fork point is unchanged"
    );
    assert_eq!(
        rebases_since(&st, before),
        vec![rebase_why::LEAF_MOVE.to_string()]
    );

    say(&mut st, "five");
    run(&mut st);
    let reqs = eng.requests();
    let (first, second, forked) = (&reqs[0], &reqs[1], &reqs[2]);
    assert_eq!(
        forked.system, first.system,
        "the system prefix bytes are the same after the fork"
    );
    assert_eq!(forked.tools_json, first.tools_json);
    assert_eq!(
        forked.messages,
        vec![
            WireMsg::user("one"),
            WireMsg::assistant("two"),
            WireMsg::user("five")
        ]
    );
    assert_eq!(
        forked.messages[..2],
        second.messages[..2],
        "the shared prefix of the wire is the same bytes"
    );
}

/// new file, same wire
#[test]
fn clone_switches_session() {
    let env = TestEnv::new();
    let (mut st, eng, mem) = state_with(&env, vec![ScriptTurn::text("five-reply")], Vec::new());
    let old = st.sref.clone();
    say(&mut st, "one");
    reply(&mut st, "two");
    say(&mut st, "three");
    reply(&mut st, "four");
    let old_leaf = st.tree.leaf().to_string();
    let old_wire = st.transcript.wire().to_vec();

    let mut ui = HeadlessUi::default();
    let clone = clone_current(&mut st, &mut ui).expect("clone");
    assert_ne!(clone.id, old.id);
    assert_ne!(clone.path, old.path, "a new file");
    assert_eq!(st.sref, clone);
    assert_eq!(st.tree.leaf(), old_leaf);
    assert_eq!(st.transcript.wire().to_vec(), old_wire, "the same wire");
    assert!(st.transcript.rebases().is_empty(), "a clone is not a rebase");
    let loaded = mem.load(&clone).expect("load clone");
    assert_eq!(loaded.len(), 4);
    assert!(mem.header(&clone).expect("header").parent_session.is_some());

    // The next request extends the same wire in the new session.
    say(&mut st, "five");
    run(&mut st);
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].messages[..4].to_vec(), old_wire);
    assert_eq!(reqs[0].session, clone.id);
}

/// check_w1 green with the mark
#[test]
fn move_back_to_earlier_leaf_w1_marked() {
    let env = TestEnv::new();
    let (mut st, eng, _store) = state_with(
        &env,
        vec![
            ScriptTurn::text("two"),
            ScriptTurn::text("four"),
            ScriptTurn::text("six"),
        ],
        Vec::new(),
    );
    say(&mut st, "one");
    run(&mut st);
    say(&mut st, "three");
    run(&mut st);
    let a1 = find_msg(&st, Role::Assistant, "two");

    // Request 2 is the idle prefill after the move; request 3 is the next chat turn.
    go(&mut st, &a1, false).expect("move back");
    say(&mut st, "five");
    run(&mut st);

    let reqs = eng.requests();
    assert_eq!(reqs.len(), 4);
    let marked = check_w1(&reqs, &[(2, rebase_why::LEAF_MOVE)]);
    assert!(marked.is_empty(), "W1 violations with the mark: {marked:?}");
    assert!(
        !check_w1(&reqs, &[]).is_empty(),
        "the unmarked move is a W1 violation"
    );
}

// ---------------------------------------------------------------------------------------------
// Extra coverage

/// `cacheWarming: off` skips the park prefill; the navigation still happens.
#[test]
fn park_prefill_skipped_when_cache_warming_off() {
    let env = TestEnv::new();
    let (mut st, eng, _store) = state_with(&env, Vec::new(), Vec::new());
    st.settings = Settings::from_values(json!({"cacheWarming": "off"}), json!({}));
    let _u1 = say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let _u2 = say(&mut st, "three");
    reply(&mut st, "four");
    go(&mut st, &a1, false).expect("navigate");
    assert!(eng.requests().iter().all(|r| r.kind != "prefill"));
    assert_eq!(
        st.tree.leaf(),
        a1,
        "with no prefill the leaf is the target itself"
    );
}

/// Navigating to the current leaf does nothing; an unknown target is not found.
#[test]
fn noop_at_leaf_and_unknown_target() {
    let env = TestEnv::new();
    let (mut st, eng, _store) = state_with(&env, Vec::new(), Vec::new());
    say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let rebases = st.transcript.rebases().len();
    assert_eq!(go(&mut st, &a1, true).expect("no-op"), None);
    assert_eq!(st.transcript.rebases().len(), rebases);
    assert!(eng.requests().is_empty());

    let err = go(&mut st, "deadbeef", false).expect_err("unknown");
    assert_eq!(err.code, codes::NOT_FOUND);
    let mut ui = HeadlessUi::default();
    let err = fork_at(&mut st, "deadbeef", &mut ui).expect_err("unknown fork point");
    assert_eq!(err.code, codes::NOT_FOUND);
}

/// TREE-004 / RPC-031, pi `AgentSessionRuntime.fork`: the default position (`before`) accepts
/// only a user message (`Invalid entry ID for forking` otherwise); `fork_at` is that default.
/// `at` keeps the chosen entry as the leaf.
#[test]
fn fork_before_needs_a_user_message_and_at_keeps_the_entry() {
    let env = TestEnv::new();
    let (mut st, _eng, _store) = state_with(&env, Vec::new(), Vec::new());
    say(&mut st, "one");
    let a1 = reply(&mut st, "two");
    let u2 = say(&mut st, "three");
    reply(&mut st, "four");
    let mut ui = HeadlessUi::default();
    let err = fork_at(&mut st, &a1, &mut ui).expect_err("an assistant message is no fork point");
    assert_eq!(err.code, codes::INVALID);
    assert!(err.message.contains("Invalid entry ID for forking"), "{err:?}");

    let sref = fork_at(&mut st, &u2, &mut ui).expect("fork before `three`");
    assert_eq!(st.sref, sref);
    assert_eq!(st.tree.leaf(), a1);
    assert_eq!(
        st.transcript.wire().to_vec(),
        vec![WireMsg::user("one"), WireMsg::assistant("two")]
    );

    let at = fork_from(&mut st, &a1, ForkPosition::At, &mut ui).expect("fork at");
    assert_eq!(at.text, None);
    assert_eq!(st.tree.leaf(), a1);
}

/// W4: the fork an extension asks for (`fork_at`, the path `HostAction::Fork` takes) names the new
/// session as the owner of the jobs started from there, so their wake lands in it.
#[test]
fn extension_fork_rebinds_the_job_owner() {
    let env = TestEnv::new();
    let (mut st, _eng, _store) = state_with(&env, Vec::new(), Vec::new());
    say(&mut st, "one");
    let u2 = say(&mut st, "two");
    reply(&mut st, "three");
    let mut ui = HeadlessUi::default();
    let sref = fork_at(&mut st, &u2, &mut ui).expect("fork before `two`");
    assert_eq!(st.jobs.owner(), sref.id);
}

/// pi: `clone` is `fork(leaf, {position: "at"})`, so `session_before_fork` fires (with the
/// position) and may cancel it; `fork` sends `position: "before"`.
#[test]
fn clone_and_fork_fire_before_fork_with_position() {
    struct ForkHook {
        seen: Arc<Mutex<Vec<Value>>>,
        cancel: bool,
    }
    impl Hooks for ForkHook {
        fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
            if p != HookPoint::SessionBeforeFork {
                return HookReply::Continue;
            }
            self.seen.lock().unwrap().push(payload.clone());
            if self.cancel {
                HookReply::Cancel("no".to_string())
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
    let env = TestEnv::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<dyn Hooks> = Arc::new(ForkHook {
        seen: seen.clone(),
        cancel: true,
    });
    let (mut st, _eng, _store) = state_with(&env, Vec::new(), vec![hook]);
    say(&mut st, "one");
    reply(&mut st, "two");
    let u2 = say(&mut st, "three");
    let leaf = reply(&mut st, "four");
    let old = st.sref.clone();
    let mut ui = HeadlessUi::default();
    let err = clone_current(&mut st, &mut ui).expect_err("the hook cancels the clone");
    assert_eq!(err.code, codes::ABORTED);
    let err = fork_at(&mut st, &u2, &mut ui).expect_err("the hook cancels the fork");
    assert_eq!(err.code, codes::ABORTED);
    assert_eq!(st.sref, old, "nothing switched");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            json!({"entryId": leaf, "position": "at"}),
            json!({"entryId": u2, "position": "before"}),
        ]
    );
}

/// Silence the unused-import lint for the event sink trait used only through `&mut dyn`.
#[allow(dead_code)]
fn _sink_type(_: &mut dyn EventSink) {}

/// Keep the `SessionRef` type in the public surface of the test (fork / clone return it).
#[allow(dead_code)]
fn _ref_type(_: &SessionRef) {}

/// SET-029: `branchSummary.reserveTokens` bounds the summary's output:
/// `min(max_output, reserve * 4 / 5)`.
#[test]
fn branch_summary_budget_from_its_reserve_setting() {
    let env = TestEnv::new();
    for (reserve, want) in [(Some(2000u32), 1600u32), (None, 16384 * 4 / 5)] {
        let (mut st, eng, _store) = state_with(&env, vec![ScriptTurn::text("summary")], Vec::new());
        if let Some(r) = reserve {
            st.settings = Settings::from_values(json!({"branchSummary": {"reserveTokens": r}}), json!({}));
        }
        let _u1 = say(&mut st, "one");
        let a1 = reply(&mut st, "two");
        let _u2 = say(&mut st, "three");
        reply(&mut st, "four");
        go(&mut st, &a1, true).expect("navigate with summary");
        let req = eng
            .requests()
            .into_iter()
            .find(|r| r.kind == "branch_summary")
            .expect("a summary request");
        assert_eq!(req.max_tokens, want.min(st.info.max_output), "{reserve:?}");
    }
}

fn replay_reply(st: &mut SessionState, text: &str, tag: u8) -> String {
    let mut wire = WireMsg::assistant(text);
    wire.provider_history = Some(
        oxa_proto::ProviderHistory::new(
            [tag; 32],
            [tag + 1; 32],
            vec![json!({"type":"reasoning","encrypted_content":format!("synthetic-replay-{tag}")})],
        )
        .unwrap(),
    );
    append_entry(
        st,
        EntryBody::Msg {
            wire,
            parts: vec![],
            tool: None,
        },
    )
    .unwrap()
    .id
}

fn replay_navigation_state(file_backed: bool) -> (TestEnv, SessionState, String) {
    let env = TestEnv::new();
    let (mut st, _, _) = state_with(&env, vec![], vec![]);
    if file_backed {
        let store = Arc::new(oxa_session::FsStore::new(env.root().join("replay-sessions")));
        st.sref = store.create(&env.work(), None, None).unwrap();
        st.store = store;
    }
    say(&mut st, "common question");
    let common = replay_reply(&mut st, "common answer", 1);
    say(&mut st, "branch A question");
    replay_reply(&mut st, "branch A answer", 3);
    say(&mut st, "longer branch A question");
    let target = replay_reply(&mut st, "longer branch A answer", 5);
    go(&mut st, &common, false).unwrap();
    say(&mut st, "branch B question");
    replay_reply(&mut st, "branch B answer", 7);
    (env, st, target)
}

fn persisted_replay_target(st: &SessionState, target: &str) -> Vec<WireMsg> {
    let tree = Tree::new(st.store.load(&st.sref).unwrap());
    let branch: Vec<_> = tree.branch(target).into_iter().cloned().collect();
    let wire = Transcript::project(&branch).wire().to_vec();
    assert_eq!(wire.iter().filter(|m| m.provider_history.is_some()).count(), 3);
    assert!(
        wire.len() > st.transcript.wire().len(),
        "also cover destination carriers beyond outgoing length"
    );
    wire
}

#[test]
fn divergent_leaf_restores_the_persisted_destination_replay_exactly() {
    for file_backed in [true, false] {
        let (_env, mut st, target) = replay_navigation_state(file_backed);
        let expected = persisted_replay_target(&st, &target);
        go(&mut st, &target, false).unwrap();
        assert_eq!(
            st.transcript.wire(),
            expected.as_slice(),
            "leaf move must match direct destination projection"
        );
    }
}

#[test]
fn switch_file_restores_the_persisted_destination_replay_exactly() {
    for file_backed in [true, false] {
        let (_env, mut st, target) = replay_navigation_state(file_backed);
        let source = st.sref.clone();
        let expected = persisted_replay_target(&st, &target);
        let forked = fork_from(&mut st, &target, ForkPosition::At, &mut HeadlessUi::default()).unwrap();
        assert_ne!(forked.sref, source);
        assert_eq!(st.sref, forked.sref);
        assert_eq!(
            st.transcript.wire(),
            expected.as_slice(),
            "switch_file must match direct destination projection"
        );
        let tree = Tree::new(st.store.load(&st.sref).unwrap());
        let branch: Vec<_> = tree.branch(tree.leaf()).into_iter().cloned().collect();
        assert_eq!(st.transcript.wire(), Transcript::project(&branch).wire());
    }
}
