//! Adaptive thinking on tool follow-ups (`oxa-core/src/think.rs`,
//! `oxa.thinking.toolFollowups: auto`). Evidence (Ornith, a long-session bug task, greedy):
//! thinking off on every follow-up looped and did not solve it; thinking on every follow-up
//! solved it but made a pure large-read task too slow. A scripted engine replays the shapes:
//! a failing check turns thinking on for the next reply, an edit or a passing check turns it off
//! again, a repeat notice or calls without progress turn it on, plain reads keep it off, the
//! user's level `off` and the setting win, and no sent byte changes across a toggle.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{append_entry, prompt, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{cache_bytes, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, AgentEvent, Answer, CancelToken, Engine, EntryBody, ExecResult, ExecSpec, FrozenPrefix, Level,
    Mode, OxaError, Placement, Policy, Role, Sandbox, ThinkingLevel, Tool, Ui, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

/// Runs `sh -c <command>` in the test's own work directory (as `r1_repeat_guard.rs`).
struct ShSandbox;
impl Sandbox for ShSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&s.command)
            .current_dir(&s.cwd)
            .output()
            .expect("sh runs");
        let mut output = out.stdout;
        output.extend_from_slice(&out.stderr);
        ExecResult {
            output,
            exit: out.status.code(),
            timed_out: false,
        }
    }
}

/// A headless UI that records the footer status calls.
#[derive(Default)]
struct StatusUi {
    status: Vec<(String, Option<String>)>,
}
impl Ui for StatusUi {
    fn select(&mut self, _: &str, _: &[String], _: Option<u32>) -> Option<usize> {
        None
    }
    fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
        None
    }
    fn input(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, _: &str, _: Level) {}
    fn set_status(&mut self, key: &str, text: Option<&str>) {
        self.status.push((key.to_string(), text.map(str::to_string)));
    }
    fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: Placement) {}
    fn set_title(&mut self, _: &str) {}
    fn set_editor_text(&mut self, _: &str) {}
    fn ask_access(&mut self, _: &Access) -> Answer {
        Answer::Deny
    }
}

fn tools() -> Vec<Arc<dyn Tool>> {
    let mut t = oxa_tools::builtin_tools();
    t.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    t
}

/// Policy `auto` plus `extra` (merged into the `oxa` object of the user layer).
fn settings(extra: serde_json::Value) -> Settings {
    let mut oxa = json!({"thinking": {"policy": "auto"}});
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            if k == "thinking" {
                for (tk, tv) in v.as_object().unwrap() {
                    oxa["thinking"][tk] = tv.clone();
                }
            } else {
                oxa[k] = v.clone();
            }
        }
    }
    Settings::from_values(json!({ "oxa": oxa }), json!({}))
}

fn state(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    settings: Settings,
    level: ThinkingLevel,
) -> (SessionState, Arc<ScriptedEngine>) {
    state_with(env, ScriptedEngine::new(turns), settings, level)
}

fn state_with(
    env: &TestEnv,
    eng: ScriptedEngine,
    settings: Settings,
    level: ThinkingLevel,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(eng);
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    let lines: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    std::fs::write(cwd.join("a.txt"), lines).unwrap();
    let sref = store.create(&cwd, None, None).unwrap();
    let info = engine.info(None).unwrap();
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
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
    });
    let tools = tools();
    let st = SessionState {
        output_schema: None,
        engine,
        store,
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(ShSandbox),
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
        thinking: level,
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
    (st, eng)
}

struct Ran {
    end: RunEnd,
    events: Vec<AgentEvent>,
    status: Vec<(String, Option<String>)>,
}

fn run(st: &mut SessionState, prompt_text: &str) -> Ran {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(prompt_text),
            parts: Vec::new(),
            tool: None,
        },
    )
    .unwrap();
    let mut sink = VecSink::default();
    let mut ui = StatusUi::default();
    let end = run_loop(st, &mut sink, &mut ui).unwrap();
    Ran {
        end,
        events: sink.0,
        status: ui.status,
    }
}

/// `enable_thinking` of every request so far.
fn thinking(eng: &ScriptedEngine) -> Vec<bool> {
    eng.requests().iter().map(|r| r.thinking).collect()
}

/// The escalation notices of a run.
fn notices(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice {
                level: Level::Info,
                text,
            } if text.starts_with(oxa_core::THINKING_STATUS_ON) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn bash(id: &str, command: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "bash", &json!({ "command": command }).to_string())
}

/// A `cargo test` stand-in (a shell function, so no toolchain is needed) that fails.
fn failing_test(id: &str) -> ScriptTurn {
    bash(
        id,
        "cargo() { echo 'test result: FAILED. 10 passed; 1 failed'; return 101; }; cargo test",
    )
}

/// The same check passing.
fn passing_test(id: &str) -> ScriptTurn {
    bash(
        id,
        "cargo() { echo 'test result: ok. 11 passed; 0 failed'; }; cargo test",
    )
}

/// Piped through `tail`: exits 0 but reports the failure.
fn piped_failing_test(id: &str) -> ScriptTurn {
    bash(
        id,
        "cargo() { echo 'test result: FAILED. 10 passed; 1 failed'; return 101; }; cargo test 2>&1 | tail -5",
    )
}

fn edit(id: &str, n: u32) -> ScriptTurn {
    let args = format!(r#"{{"path":"a.txt","oldText":"line {n}\n","newText":"line {n} fixed\n"}}"#);
    ScriptTurn::tool(id, "edit", &args)
}

fn read_range(id: &str, offset: u32) -> ScriptTurn {
    ScriptTurn::tool(
        id,
        "read",
        &format!(r#"{{"path":"a.txt","offset":{offset},"limit":3}}"#),
    )
}

/// A reply that thinks: the reasoning goes back on the wire (preserve_thinking).
fn with_reasoning(mut t: ScriptTurn, r: &str) -> ScriptTurn {
    t.reasoning = Some(r.to_string());
    t
}

#[test]
fn a_failing_test_turns_thinking_on_until_an_edit() {
    let env = TestEnv::new();
    let turns = vec![
        failing_test("t0"),
        bash("c0", "grep -n 'line 27' a.txt"),
        edit("e0", 27),
        read_range("r0", 1),
        ScriptTurn::text("done"),
    ];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    let ran = run(&mut st, "Fix the failing test in a.txt");
    assert_eq!(ran.end, RunEnd::Stopped);
    // prompt; after the failure (on); after the grep (still on: no progress yet); after the
    // edit (off); after the read (off).
    assert_eq!(thinking(&eng), vec![true, true, true, false, false]);
    let n = notices(&ran.events);
    assert_eq!(n.len(), 1, "{n:?}");
    assert!(n[0].contains("a tool call failed"), "{n:?}");
    // The footer: on at the failure, off at the edit (and the run's end clears nothing more).
    let on = Some(oxa_core::THINKING_STATUS_ON.to_string());
    assert_eq!(
        ran.status,
        vec![
            (oxa_core::THINKING_STATUS_KEY.to_string(), on),
            (oxa_core::THINKING_STATUS_KEY.to_string(), None)
        ]
    );
    assert_eq!(st.stats.escalation, Default::default(), "the run's end resets it");
}

#[test]
fn after_a_passing_test_it_is_off() {
    let env = TestEnv::new();
    let turns = vec![
        failing_test("t0"),
        passing_test("t1"),
        read_range("r0", 1),
        ScriptTurn::text("done"),
    ];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    run(&mut st, "debug the test");
    assert_eq!(thinking(&eng), vec![true, true, false, false]);
}

#[test]
fn a_piped_failing_test_counts_under_a_fix_prompt_only() {
    // `cargo test | tail` exits 0; under a fix/debug/implement prompt the output decides.
    let env = TestEnv::new();
    let turns = vec![piped_failing_test("t0"), ScriptTurn::text("done")];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    run(&mut st, "fix the bug");
    assert_eq!(thinking(&eng), vec![true, true]);

    // A question: the same output is just output, and a check that exits 0 is a pass.
    let env = TestEnv::new();
    let turns = vec![piped_failing_test("t0"), ScriptTurn::text("done")];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    run(&mut st, "what does the test output say?");
    assert_eq!(thinking(&eng), vec![true, false]);
}

#[test]
fn a_repeat_notice_turns_it_on() {
    let env = TestEnv::new();
    let range = r#"{"path":"a.txt","offset":5,"limit":3}"#;
    let mut turns: Vec<ScriptTurn> = (0..4)
        .map(|i| ScriptTurn::tool(&format!("r{i}"), "read", range))
        .collect();
    turns.push(ScriptTurn::text("done"));
    // A high fruitless limit: only the notice can turn it on here.
    let (mut st, eng) = state(
        &env,
        turns,
        settings(json!({"thinking": {"escalation": {"fruitlessCalls": 50}}})),
        ThinkingLevel::Medium,
    );
    let ran = run(&mut st, "show me lines 5 to 7");
    let results: Vec<String> = st
        .transcript
        .wire()
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(results[3].starts_with(prompt::REPEAT_NOTICE_HEAD), "{results:#?}");
    // The three identical reads keep it off; the notice turns it on.
    assert_eq!(thinking(&eng), vec![true, false, false, false, true]);
    let n = notices(&ran.events);
    assert!(n.len() == 1 && n[0].contains("repeated"), "{n:?}");
}

#[test]
fn fruitless_calls_turn_it_on() {
    // Six commands without an edit since the prompt (the default limit).
    let env = TestEnv::new();
    let mut turns: Vec<ScriptTurn> = (0..7)
        .map(|i| bash(&format!("b{i}"), &format!("echo step {i}")))
        .collect();
    turns.push(ScriptTurn::text("done"));
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    let ran = run(&mut st, "fix the gguf bug");
    let got = thinking(&eng);
    assert_eq!(got, vec![true, false, false, false, false, false, true, true]);
    let n = notices(&ran.events);
    assert!(n.len() == 1 && n[0].contains("without progress"), "{n:?}");

    // Identical re-reads count too (the Block 1 shape), a lower limit applies at once, and
    // `0` turns this ground off.
    let reread = |n: usize| {
        let mut t: Vec<ScriptTurn> = (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    read_range(&format!("r{i}"), 1)
                } else {
                    read_range(&format!("r{i}"), 9)
                }
            })
            .collect();
        t.push(ScriptTurn::text("done"));
        t
    };
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        reread(5),
        settings(json!({"thinking": {"escalation": {"fruitlessCalls": 2}}})),
        ThinkingLevel::Medium,
    );
    run(&mut st, "fix it");
    // r0, r1 new; r2 and r3 repeat earlier reads: on after the second repeat.
    assert_eq!(thinking(&eng), vec![true, false, false, false, true, true]);
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        reread(5),
        settings(json!({"thinking": {"escalation": {"fruitlessCalls": 0}}})),
        ThinkingLevel::Medium,
    );
    run(&mut st, "fix it");
    assert_eq!(thinking(&eng), vec![true, false, false, false, false, false]);
}

#[test]
fn plain_successful_reads_stay_off() {
    // Paging through a large file: every read is new information, never escalated, also
    // under a fix prompt and far past the fruitless limit.
    let env = TestEnv::new();
    let mut turns: Vec<ScriptTurn> = (0..12).map(|i| read_range(&format!("r{i}"), 1 + 3 * i)).collect();
    turns.push(ScriptTurn::text("done"));
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    let ran = run(&mut st, "fix the bug somewhere in a.txt");
    let got = thinking(&eng);
    assert!(got[0] && got[1..].iter().all(|t| !t), "{got:?}");
    assert!(notices(&ran.events).is_empty());
    assert!(ran.status.is_empty(), "{:?}", ran.status);
}

#[test]
fn outside_a_fix_task_a_clean_step_releases_it() {
    // A question: a failed command thinks once, the next clean step of new calls is off again,
    // so a read-heavy task stays fast.
    let env = TestEnv::new();
    let turns = vec![
        bash("b0", "ls missing-dir"),
        read_range("r0", 1),
        read_range("r1", 4),
        ScriptTurn::text("done"),
    ];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    run(&mut st, "what is in a.txt?");
    assert_eq!(thinking(&eng), vec![true, true, false, false]);
}

#[test]
fn a_new_prompt_starts_from_off() {
    let env = TestEnv::new();
    let turns = vec![
        failing_test("t0"),
        ScriptTurn::text("I could not fix it"),
        read_range("r0", 1),
        ScriptTurn::text("done"),
    ];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    let first = run(&mut st, "fix the test");
    // The run ended while on: the footer is cleared at its end.
    assert_eq!(
        first.status.last(),
        Some(&(oxa_core::THINKING_STATUS_KEY.to_string(), None))
    );
    run(&mut st, "show me the top of a.txt");
    assert_eq!(thinking(&eng), vec![true, true, true, false]);
}

#[test]
fn the_user_override_wins() {
    let script = || vec![failing_test("t0"), read_range("r0", 1), ScriptTurn::text("done")];
    // The level `off` (Shift+Tab): nothing turns thinking on, also not the prompt.
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, script(), settings(json!({})), ThinkingLevel::Off);
    let ran = run(&mut st, "fix the test");
    assert_eq!(thinking(&eng), vec![false, false, false]);
    assert!(notices(&ran.events).is_empty());
    // `toolFollowups: off`: the follow-ups never think.
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        script(),
        settings(json!({"thinking": {"toolFollowups": "off"}})),
        ThinkingLevel::High,
    );
    run(&mut st, "fix the test");
    assert_eq!(thinking(&eng), vec![true, false, false]);
    // `toolFollowups: on`: they always do.
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        script(),
        settings(json!({"thinking": {"toolFollowups": "on"}})),
        ThinkingLevel::High,
    );
    run(&mut st, "fix the test");
    assert_eq!(thinking(&eng), vec![true, true, true]);
    // Policy `fixed` (pi): the level on every request.
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        script(),
        Settings::from_values(json!({"oxa": {"thinking": {"policy": "fixed"}}}), json!({})),
        ThinkingLevel::High,
    );
    run(&mut st, "fix the test");
    assert_eq!(thinking(&eng), vec![true, true, true]);
}

#[test]
fn the_history_stays_a_byte_prefix_across_an_escalation() {
    // Off, on (with reasoning in the reply), off again: every request's bytes start with the
    // whole previous request (the fake's cache model), and no cache miss is reported.
    let env = TestEnv::new();
    let turns = vec![
        with_reasoning(read_range("r0", 1), "look first"),
        failing_test("t0"),
        with_reasoning(edit("e0", 27), "the test fails because line 27 is wrong"),
        passing_test("t1"),
        read_range("r1", 4),
        ScriptTurn::text("fixed"),
    ];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    let ran = run(&mut st, "fix the failing test");
    assert_eq!(thinking(&eng), vec![true, false, true, false, false, false]);
    let reqs = eng.requests();
    for w in reqs.windows(2) {
        let a = cache_bytes(&w[0].system, &w[0].tools_json, &w[0].messages);
        let b = cache_bytes(&w[1].system, &w[1].tools_json, &w[1].messages);
        assert!(
            b.starts_with(&a),
            "request {} does not extend request {}",
            w[1].seq,
            w[0].seq
        );
        let cached = w[1].usage.expect("usage").cached as usize;
        assert!(
            cached >= a.len(),
            "request {}: cached {cached} < {}",
            w[1].seq,
            a.len()
        );
    }
    // The reasoning of the escalated reply is on the wire exactly as received.
    assert!(reqs[3]
        .messages
        .iter()
        .any(|m| m.reasoning.as_deref() == Some("the test fails because line 27 is wrong")));
    assert!(!ran
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::CacheMiss { .. })));
}

#[test]
fn searching_for_test_commands_never_counts_as_a_pass() {
    // Codex T2 review of 5b03ba5: a successful search or read whose command or output merely
    // mentions a test runner must not release the escalation; only an executed runner with a
    // pass summary does.
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("tests")).unwrap();
    std::fs::write(
        env.work().join("tests/run.sh"),
        "#!/bin/sh\ncargo test\n# test result: ok. 11 passed\n",
    )
    .unwrap();
    std::fs::write(env.work().join("README.md"), "Run `cargo test` and pytest.\n").unwrap();
    let turns = vec![
        failing_test("t0"),
        bash("g0", "grep -rn \"cargo test\" ."),
        bash("g1", "rg 'cargo test' README.md || grep -n pytest README.md"),
        ScriptTurn::tool("r0", "read", r#"{"path":"tests/run.sh"}"#),
        bash("l0", "ls tests/"),
        bash("e0", "echo 'test result: ok. 11 passed; 0 failed'"),
        passing_test("t1"),
        ScriptTurn::text("done"),
    ];
    let (mut st, eng) = state(&env, turns, settings(json!({})), ThinkingLevel::Medium);
    run(&mut st, "fix the failing test");
    // Every follow-up after the failure keeps thinking until the real passing run.
    assert_eq!(
        thinking(&eng),
        vec![true, true, true, true, true, true, true, false]
    );
}

// ---- #184: thinking that uses the whole output budget (finish=length, reasoning only) ----

const OVERRUN_NOTICE: &str = "thinking used the whole output budget; answered without thinking";

/// A reply that thought until the output limit and never answered.
fn overrun() -> ScriptTurn {
    let mut t = ScriptTurn::text("");
    t.text = None;
    t.reasoning = Some("hmm ".repeat(50));
    t.finish = oxa_proto::Finish::Length;
    t
}

fn notice_texts(events: &[AgentEvent], level: Level) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice { level: l, text } if *l == level => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn answered(st: &SessionState, text: &str) -> bool {
    st.transcript
        .wire()
        .iter()
        .any(|m| m.role == Role::Assistant && m.content.as_deref() == Some(text))
}

#[test]
fn overrun_retries_once_without_thinking() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![overrun(), ScriptTurn::text("the answer")],
        settings(json!({})),
        ThinkingLevel::Medium,
    );
    let r = run(&mut st, "explain a.txt");
    assert!(matches!(r.end, RunEnd::Stopped));
    assert_eq!(thinking(&eng), vec![true, false]);
    let reqs = eng.requests();
    assert_eq!(
        reqs[0].messages, reqs[1].messages,
        "same messages, so the prefix is cached"
    );
    assert!(answered(&st, "the answer"));
    assert_eq!(
        notice_texts(&r.events, Level::Info)
            .iter()
            .filter(|t| *t == OVERRUN_NOTICE)
            .count(),
        1
    );
    // The reasoning-only reply never reaches the wire.
    assert!(!st.transcript.wire().iter().any(|m| m.reasoning.is_some()));
}

#[test]
fn overrun_twice_is_an_error_not_a_loop() {
    let env = TestEnv::new();
    let mut again = overrun();
    again.reasoning = None;
    let (mut st, eng) = state(
        &env,
        vec![overrun(), again, ScriptTurn::text("never asked")],
        settings(json!({})),
        ThinkingLevel::Medium,
    );
    let r = run(&mut st, "explain a.txt");
    assert!(
        matches!(&r.end, RunEnd::Failed(error) if error.code == "thinking_overrun"),
        "{:?}",
        r.end
    );
    assert_eq!(eng.requests().len(), 2, "no further retry");
    assert_eq!(notice_texts(&r.events, Level::Error).len(), 1);
}

#[test]
fn remote_model_is_unchanged() {
    let env = TestEnv::new();
    let (mut st, eng) = state_with(
        &env,
        ScriptedEngine::new(vec![overrun(), ScriptTurn::text("never asked")]).with_remote(),
        settings(json!({})),
        ThinkingLevel::Medium,
    );
    let r = run(&mut st, "explain a.txt");
    assert!(matches!(r.end, RunEnd::Stopped));
    assert_eq!(eng.requests().len(), 1);
    assert!(notice_texts(&r.events, Level::Info)
        .iter()
        .all(|t| t != OVERRUN_NOTICE));
}

#[test]
fn overrun_stops_later_escalation_in_the_task() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![overrun(), failing_test("c1"), ScriptTurn::text("done")],
        settings(json!({})),
        ThinkingLevel::Medium,
    );
    // The retry (no thinking) answers with a failing call; the failure would escalate.
    let r = run(&mut st, "fix the failing test");
    assert!(matches!(r.end, RunEnd::Stopped));
    assert_eq!(thinking(&eng), vec![true, false, false]);
    assert!(notices(&r.events).is_empty());
}

#[test]
fn overrun_after_an_escalation_clears_the_footer() {
    let env = TestEnv::new();
    let (mut st, _eng) = state(
        &env,
        vec![failing_test("c1"), overrun(), ScriptTurn::text("done")],
        settings(json!({})),
        ThinkingLevel::Medium,
    );
    let r = run(&mut st, "fix the failing test");
    assert!(matches!(r.end, RunEnd::Stopped), "{:?}", r.end);
    let key_status: Vec<_> = r.status.iter().filter(|(k, _)| k == "thinking").collect();
    assert!(
        key_status.iter().any(|(_, t)| t.is_some()),
        "the test escalated: {:?}",
        r.status
    );
    // Cleared right at the overrun, not only at the end of the run.
    let first_overrun_notice = r
        .events
        .iter()
        .position(|e| matches!(e, AgentEvent::Notice { text, .. } if text == OVERRUN_NOTICE))
        .unwrap();
    assert!(first_overrun_notice > 0);
    assert_eq!(key_status.last().unwrap().1, None);
}

#[test]
fn the_discarded_overrun_request_is_counted() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![overrun(), ScriptTurn::text("the answer")],
        settings(json!({})),
        ThinkingLevel::Medium,
    );
    run(&mut st, "explain a.txt");
    assert_eq!(eng.requests().len(), 2);
    assert_eq!(st.stats.requests, 2, "both requests are in the stats");
    assert_eq!(st.stats.assistant_messages, 1, "only the answer is a message");
}

#[test]
fn both_requests_are_counted_when_the_retry_overruns_too() {
    let env = TestEnv::new();
    let mut again = overrun();
    again.reasoning = None;
    let (mut st, _eng) = state(
        &env,
        vec![overrun(), again],
        settings(json!({})),
        ThinkingLevel::Medium,
    );
    let r = run(&mut st, "explain a.txt");
    assert!(matches!(r.end, RunEnd::Failed(_)));
    assert_eq!(st.stats.requests, 2);
}
