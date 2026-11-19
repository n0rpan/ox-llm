//! C6 acceptance tests: !cmd and !!cmd.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{append_entry, run_loop, user_bash, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, AgentEvent, CancelToken, CommandSpec, Engine, Entry, EntryBody, FrozenPrefix, HeadlessUi,
    HookPoint, HookReply, Hooks, Mode, OxaError, Policy, ThinkingLevel, Tool, ToolOutput, Ui, Value, VecSink,
    WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{bash_text, MemStore, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

fn build(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    store: Arc<dyn SessionStore>,
    hooks: Vec<Arc<dyn Hooks>>,
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
        wait_wake: CancelToken::new(),
        busy: AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: AtomicBool::new(false),
        gate: Default::default(),
        wake: Default::default(),
    });
    let tools: Vec<Arc<dyn Tool>> = Vec::new();
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
        spill_dir: env.tmp().join("spill"),
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

/// A session over a fresh scripted engine, with `turns` as its replies.
fn scripted_state(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    hooks: Vec<Arc<dyn Hooks>>,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    (build(env, engine, store, hooks), eng)
}

/// Append the user message the way `Session::prompt` does.
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

fn run(st: &mut SessionState) -> RunEnd {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    run_loop(st, &mut sink, &mut ui).expect("run_loop")
}

fn bang(st: &mut SessionState, cmd: &str, excluded: bool) -> VecSink {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    user_bash(st, cmd, excluded, &mut sink, &mut ui).expect("user_bash");
    sink
}

/// The `BashExecution` entries stored in the session, in order.
fn stored_bash(st: &SessionState) -> Vec<(String, String, Option<i32>, bool)> {
    st.store
        .load(&st.sref)
        .expect("load")
        .into_iter()
        .filter_map(|e: Entry| match e.body {
            EntryBody::BashExecution {
                command,
                output,
                exit,
                excluded,
            } => Some((command, output, exit, excluded)),
            _ => None,
        })
        .collect()
}

/// W1 on the recorded requests, by the invariant checker (no rebase in these tests).
fn w1_holds(reqs: &[Recorded]) {
    let broken = oxa_fake::check_w1(reqs, &[]);
    assert!(broken.is_empty(), "W1 violations: {broken:?}");
}

/// Reads `command` from the payload and answers with a fixed reply for each named command.
struct PerCommand {
    replies: Vec<(String, HookReply)>,
    payloads: Mutex<Vec<Value>>,
}

impl Hooks for PerCommand {
    fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p != HookPoint::UserBash {
            return HookReply::Continue;
        }
        self.payloads.lock().unwrap().push(payload.clone());
        let cmd = payload["command"].as_str().unwrap_or("");
        self.replies
            .iter()
            .find(|(c, _)| c == cmd)
            .map(|(_, r)| r.clone())
            .unwrap_or(HookReply::Continue)
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

/// W1 green
#[test]
fn bang_output_enters_context_at_next_prompt_as_append() {
    let env = TestEnv::new();
    let (mut st, eng) = scripted_state(
        &env,
        vec![ScriptTurn::text("a1"), ScriptTurn::text("a2")],
        Vec::new(),
    );

    // A first exchange, so the wire is not empty when the bang lands.
    say(&mut st, "q1");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let before = st.transcript.wire().to_vec();

    bang(&mut st, "echo hello", false);
    let after = st.transcript.wire().to_vec();
    let expected = bash_text("echo hello", "hello\n", Some(0));
    assert_eq!(after.len(), before.len() + 1, "one user message appended");
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "previous wire is a prefix (W1)"
    );
    assert_eq!(after[before.len()], WireMsg::user(expected.clone()));

    say(&mut st, "q2");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let reqs = eng.requests();
    w1_holds(&reqs);
    // The request after the bang carries the bash text before the next user message.
    let sent = &reqs[1].messages;
    assert_eq!(sent.len(), before.len() + 2);
    assert_eq!(sent[before.len()], WireMsg::user(expected));
    assert_eq!(sent[before.len() + 1], WireMsg::user("q2"));
}

/// excluded
#[test]
fn double_bang_never_in_context() {
    let env = TestEnv::new();
    let (mut st, eng) = scripted_state(&env, vec![ScriptTurn::text("a1")], Vec::new());

    bang(&mut st, "echo secret-marker", true);
    assert!(st.transcript.wire().is_empty(), "!! never enters the wire");

    let stored = stored_bash(&st);
    assert_eq!(stored.len(), 1, "the entry is still recorded");
    assert!(stored[0].3, "recorded as excluded");
    assert!(stored[0].1.contains("secret-marker"));

    say(&mut st, "q");
    assert_eq!(run(&mut st), RunEnd::Stopped);
    let reqs = eng.requests();
    w1_holds(&reqs);
    assert_eq!(reqs[0].messages, vec![WireMsg::user("q")]);
    for m in &reqs[0].messages {
        assert!(!m.content.clone().unwrap_or_default().contains("secret-marker"));
    }
}

/// pi's notice
#[test]
fn truncation_notice_and_full_output_file() {
    let env = TestEnv::new();
    let (mut st, _eng) = scripted_state(&env, Vec::new(), Vec::new());

    // 20000 lines: well over pi's 2000-line limit.
    bang(&mut st, "seq 1 20000", false);
    let stored = stored_bash(&st);
    assert_eq!(stored.len(), 1);
    let output = &stored[0].1;

    let marker = "[Output truncated. Full output: ";
    let at = output.find(marker).expect("pi's truncation notice");
    let rest = &output[at + marker.len()..];
    let path_str = &rest[..rest.find(']').expect("notice closes")];
    let path = Path::new(path_str);
    assert!(
        path.starts_with(st.spill_dir.as_path()),
        "full output saved under the session tmp dir"
    );

    let full = std::fs::read_to_string(path).expect("full output file");
    assert!(
        full.starts_with("1\n2\n3\n"),
        "full file holds the whole output from the start"
    );
    assert!(full.ends_with("19999\n20000\n"));
    assert_eq!(full.lines().count(), 20000);

    // The entry keeps the tail: the last line of the output is the last number.
    let kept: Vec<&str> = output[..at].trim_end_matches('\n').lines().collect();
    assert!(kept.len() <= 2000, "at most pi's line limit is kept");
    assert_eq!(kept.last().copied(), Some("20000"));
}

/// events
#[test]
fn streams_bash_execution_update() {
    let env = TestEnv::new();
    let (mut st, _eng) = scripted_state(&env, Vec::new(), Vec::new());

    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    user_bash(&mut st, "printf 'a\\nb\\n'", false, &mut sink, &mut ui).expect("user_bash");

    let streamed: String = sink
        .0
        .iter()
        .filter_map(|e| match e {
            AgentEvent::BashExecutionUpdate { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, "a\nb\n");
    assert!(
        sink.0
            .iter()
            .any(|e| matches!(e, AgentEvent::EntryAppended { .. })),
        "the stored entry is announced"
    );
}

/// replies
#[test]
fn hook_user_bash_block_and_replace() {
    let env = TestEnv::new();
    let marker = env.work().join("ran-marker");
    let blocked_cmd = format!("touch {}", marker.display());
    let hook = Arc::new(PerCommand {
        replies: vec![
            (
                blocked_cmd.clone(),
                HookReply::Block("blocked by policy".to_string()),
            ),
            (
                "echo original".to_string(),
                HookReply::ReplaceResult(ToolOutput {
                    images: Vec::new(),
                    text: "replaced output".to_string(),
                    is_error: false,
                    ui: json!({ "exit": 3 }),
                }),
            ),
        ],
        payloads: Mutex::new(Vec::new()),
    });
    let (mut st, _eng) = scripted_state(&env, Vec::new(), vec![hook.clone() as Arc<dyn Hooks>]);

    // Block: nothing runs; the reason is the output and there is no exit code.
    bang(&mut st, &blocked_cmd, false);
    assert!(!marker.exists(), "a blocked command never runs");
    // Replace: the hook's output and exit code are stored, the command never runs.
    bang(&mut st, "echo original", true);
    // No reply: the command runs for real.
    bang(&mut st, "echo real", true);

    let stored = stored_bash(&st);
    assert_eq!(stored.len(), 3);
    assert_eq!(
        stored[0],
        (blocked_cmd, "blocked by policy".to_string(), None, false)
    );
    assert_eq!(
        stored[1],
        (
            "echo original".to_string(),
            "replaced output".to_string(),
            Some(3),
            true
        )
    );
    assert_eq!(
        stored[2],
        ("echo real".to_string(), "real\n".to_string(), Some(0), true)
    );

    let payloads = hook.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 3, "the hook sees every !cmd");
    assert_eq!(payloads[1]["excludeFromContext"], json!(true));
    assert_eq!(payloads[0]["excludeFromContext"], json!(false));
}

/// during a run
#[test]
fn busy_is_refused() {
    let env = TestEnv::new();
    let (mut st, _eng) = scripted_state(&env, Vec::new(), Vec::new());

    st.shared.busy.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let err = user_bash(&mut st, "echo nope", false, &mut sink, &mut ui).expect_err("refused while busy");
    assert_eq!(err.code, codes::BUSY);
    assert!(st.transcript.wire().is_empty());
    assert!(stored_bash(&st).is_empty(), "nothing is recorded");
    assert!(sink.0.is_empty(), "nothing is streamed");
    assert!(
        st.shared.busy.load(std::sync::atomic::Ordering::SeqCst),
        "the other run keeps the flag"
    );

    st.shared.busy.store(false, std::sync::atomic::Ordering::SeqCst);
    bang(&mut st, "echo ok", false);
    assert_eq!(stored_bash(&st).len(), 1, "allowed again once idle");
    assert!(
        !st.shared.busy.load(std::sync::atomic::Ordering::SeqCst),
        "the flag is released after the run"
    );
}
