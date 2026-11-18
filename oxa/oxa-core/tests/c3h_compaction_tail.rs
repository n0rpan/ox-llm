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
        wake: Default::default(),
    });
    SessionState {
        goal: None,
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

#[derive(Default)]
struct ReplayHook(Mutex<Vec<Value>>);
impl Hooks for ReplayHook {
    fn fire(
        &self,
        _: oxa_proto::HookPoint,
        payload: &Value,
        _: &mut dyn oxa_proto::Ui,
    ) -> oxa_proto::HookReply {
        self.0.lock().unwrap().push(payload.clone());
        oxa_proto::HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
    fn commands(&self) -> Vec<oxa_proto::CommandSpec> {
        vec![]
    }
    fn run_command(&self, _: &str, _: &str, _: &mut dyn oxa_proto::Ui) -> Result<(), oxa_proto::OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        vec![]
    }
}

#[test]
fn resumed_private_replay_is_absent_from_hooks_and_engine_wire_then_dropped_on_compaction() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![ScriptTurn::text("next answer"), ScriptTurn::text("SUMMARY")],
        CompactPolicy {
            soft: u32::MAX,
            hard: u32::MAX,
            keep_tokens: 1,
            summary_max: 64,
            enabled: false,
        },
    );
    let hooks = Arc::new(ReplayHook::default());
    st.hooks.push(hooks.clone());
    say(&mut st, "previous question");
    let mut reply = WireMsg::assistant("previous answer");
    reply.provider_history = Some(
        oxa_proto::ProviderHistory::new(
            [1; 32],
            [2; 32],
            vec![serde_json::json!({
        "type":"reasoning","encrypted_content":"synthetic-private-replay"})],
        )
        .unwrap(),
    );
    let e = oxa_proto::Entry {
        id: "a".into(),
        parent: None,
        ts_ms: 1,
        body: EntryBody::Msg {
            wire: reply,
            parts: vec![],
            tool: None,
        },
    };
    let mut bytes = vec![];
    oxa_session::encode_entry(&e, &mut bytes);
    let EntryBody::Msg { wire, .. } = oxa_session::decode_entry(&bytes).unwrap().body else {
        panic!("message")
    };
    assert!(wire.provider_history.is_some());
    push(&mut st, wire);
    say(&mut st, "next question");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    assert!(!hooks.0.lock().unwrap().is_empty());
    for value in hooks.0.lock().unwrap().iter() {
        assert!(!value.to_string().contains("synthetic-private-replay"));
    }
    for request in eng.requests() {
        for message in request.messages {
            assert!(!String::from_utf8_lossy(&message.to_json()).contains("synthetic-private-replay"));
        }
    }
    compact_now(
        &mut st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .unwrap();
    assert_eq!(compactions(&st), 1);
    assert!(st.transcript.wire().iter().all(|m| m.provider_history.is_none()));
    assert_replays(&st);
}

/// Models the native latest-tool boundary, not the provider's MAC implementation.
/// The real provider binding controls remain in the engine/session suites.
struct NativeBoundary(Arc<ScriptedEngine>);
impl Engine for NativeBoundary {
    fn info(&self, target: Option<&str>) -> Result<oxa_proto::EngineInfo, oxa_proto::OxaError> {
        self.0.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, oxa_proto::OxaError> {
        self.0.count_tokens(text)
    }
    fn chat(
        &self,
        r: &oxa_proto::ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(oxa_proto::StreamEvent),
    ) -> Result<oxa_proto::AssistantTurn, oxa_proto::OxaError> {
        let fresh = r.messages.iter().rposition(|m| m.role == Role::User).unwrap_or(0);
        if let Some(call) = r.messages[fresh..]
            .iter()
            .rev()
            .find(|m| !m.tool_calls.is_empty())
        {
            assert!(
                call.provider_history.is_some(),
                "latest native tool call has no replay authority and no fresh user boundary"
            );
        }
        let mut turn = self.0.chat(r, cancel, on)?;
        if !turn.msg.tool_calls.is_empty() {
            turn.msg.provider_history = Some(
                oxa_proto::ProviderHistory::new(
                    [1; 32],
                    [2; 32],
                    vec![serde_json::json!({"type":"function_call","id":"synthetic-native-id"})],
                )
                .unwrap(),
            );
        }
        Ok(turn)
    }
}

#[test]
fn native_tool_auto_compaction_establishes_a_durable_fresh_turn() {
    // Execute the persisted, unfinished continuation in another process: no in-memory
    // carrier cache, and no new user prompt to conceal a missing compaction boundary.
    if let Some(path) = std::env::var_os("OXA_TEST_NATIVE_COMPACT_SESSION") {
        let env = TestEnv::new();
        let store = oxa_session::FsStore::new(std::path::PathBuf::from(&path).parent().unwrap().into());
        let entries = store
            .load(&oxa_session::SessionRef {
                path: path.into(),
                id: "synthetic".into(),
            })
            .unwrap();
        let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("resumed answer")]));
        let mut st = build(
            &env,
            Arc::new(NativeBoundary(eng.clone())),
            vec![],
            CompactPolicy {
                soft: u32::MAX,
                hard: u32::MAX,
                keep_tokens: 1,
                summary_max: 64,
                enabled: false,
            },
            Settings::default(),
        );
        st.transcript = Transcript::project(&entries);
        st.tree = Tree::new(entries);
        assert_eq!(st.transcript.wire().last().unwrap().role, Role::User);
        assert!(
            st.transcript.origin(st.transcript.wire().len() - 1).is_none(),
            "the synthetic continuation must not alias the summary's entry origin"
        );
        assert!(st.transcript.wire().iter().all(|m| m.provider_history.is_none()));
        assert_eq!(run(&mut st), RunEnd::Stopped);
        assert_eq!(eng.remaining(), 0);
        return;
    }
    let env = TestEnv::new();
    for native in [false, true] {
        let eng = Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("ready"),
            ScriptTurn::tool("c1", "readlog", "{}"),
            ScriptTurn::text("SUMMARY: inspect the log and report its contents"),
            ScriptTurn::text("the log contains heartbeats"),
        ]));
        let engine: Arc<dyn Engine> = if native {
            Arc::new(NativeBoundary(eng.clone()))
        } else {
            eng.clone()
        };
        let mut st = build(
            &env,
            engine,
            vec![big_tool(6000)],
            CompactPolicy {
                soft: 2000,
                hard: 100_000,
                keep_tokens: 1,
                summary_max: 64,
                enabled: true,
            },
            Settings::from_values(
                serde_json::json!({"oxa":{"compaction":{"midRun":true}}}),
                serde_json::json!({}),
            ),
        );
        say(&mut st, "get ready");
        assert_eq!(run(&mut st), RunEnd::Stopped);
        say(&mut st, "inspect the log and report its contents");
        assert_eq!(run(&mut st), RunEnd::Stopped);
        assert_eq!(compactions(&st), 1);
        assert_eq!(eng.remaining(), 0);
        let requests = eng.requests();
        let summary = requests.iter().position(|r| r.kind == "compaction").unwrap();
        let continuation = &requests[summary + 1].messages;
        assert_eq!(
            continuation.last().unwrap().role,
            if native { Role::User } else { Role::Tool }
        );
        assert!(continuation
            .iter()
            .any(|m| m.role == Role::Tool && m.content.as_ref().unwrap().len() == 6000));
        assert!(continuation.iter().all(|m| m.provider_history.is_none()));
        assert_replays(&st);
        if native {
            let end = st
                .tree
                .entries()
                .iter()
                .position(|e| matches!(e.body, EntryBody::Compaction { .. }))
                .unwrap();
            let store = oxa_session::FsStore::new(env.root().join("saved"));
            let sref = store.create(&env.work(), Some("synthetic"), None).unwrap();
            for entry in &st.tree.entries()[..=end] {
                store.append(&sref, entry).unwrap();
            }
            store.sync(&sref).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "native_tool_auto_compaction_establishes_a_durable_fresh_turn",
                ])
                .env_clear()
                .envs(env.envs("http://127.0.0.1:9"))
                .env("OXA_TEST_NATIVE_COMPACT_SESSION", &sref.path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn unchanged_native_tool_continuation_keeps_its_authority_without_a_new_prompt() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", "readlog", "{}"),
        ScriptTurn::text("done"),
    ]));
    let mut st = build(
        &env,
        Arc::new(NativeBoundary(eng.clone())),
        vec![big_tool(100)],
        CompactPolicy {
            soft: u32::MAX,
            hard: u32::MAX,
            keep_tokens: 1,
            summary_max: 64,
            enabled: true,
        },
        Settings::default(),
    );
    say(&mut st, "inspect the log");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    assert_eq!(compactions(&st), 0);
    let requests = eng.requests();
    assert_eq!(requests.len(), 2);
    let continuation = &requests[1].messages;
    assert_eq!(continuation.last().unwrap().role, Role::Tool);
    assert!(continuation
        .iter()
        .find(|m| !m.tool_calls.is_empty())
        .unwrap()
        .provider_history
        .is_some());
    assert_replays(&st);
}
