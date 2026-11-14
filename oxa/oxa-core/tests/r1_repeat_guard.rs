//! The repeat guard (`oxa-core/src/repeat.rs`) and opt-in model sampling
//! (`oxa-core/src/sampling.rs`). Block 1 (2026-11-14): a model under greedy decoding made 64
//! identical `read` calls of one 10-line range and never edited; with a read-only guard (Arm A)
//! the loop moved to an identical `bash` command. A scripted engine replays both loops here;
//! re-reads and re-runs after an edit, changing output, different ranges, mixed tools and a new
//! prompt must never trip it.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::repeat::{LIMIT, NOTICES_BEFORE_STOP};
use oxa_core::{append_entry, prompt, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    shown_why, AgentEvent, CancelToken, Engine, EntryBody, ExecResult, ExecSpec, FrozenPrefix, HeadlessUi,
    Mode, OxaError, Policy, Role, Sandbox, ThinkingLevel, Tool, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

/// Runs `sh -c <command>` in the test's own work directory (as `l4_budget.rs`). Test-only.
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

fn tools() -> Vec<Arc<dyn Tool>> {
    let mut t = oxa_tools::builtin_tools();
    t.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    t
}

fn state_for(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    settings: Settings,
    model: &str,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    let lines: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    std::fs::write(cwd.join("a.txt"), lines).unwrap();
    let sref = store.create(&cwd, None, None).unwrap();
    let info = engine.info(None).unwrap();
    // A fixed prefix without the cwd: the request hashes do not depend on the temp path.
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
        model: model.to_string(),
        thinking: ThinkingLevel::Medium,
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
    };
    (st, eng)
}

fn run(st: &mut SessionState, prompt_text: &str) -> (RunEnd, Vec<AgentEvent>) {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(prompt_text),
            parts: Vec::new(),
        },
    )
    .unwrap();
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let end = run_loop(st, &mut sink, &mut ui).unwrap();
    (end, sink.0)
}

fn state(env: &TestEnv, turns: Vec<ScriptTurn>, settings: Settings) -> (SessionState, Arc<ScriptedEngine>) {
    state_for(env, turns, settings, "fake-model")
}

fn read(id: &str, args: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "read", args)
}

/// The tool results on the wire, in order.
fn results(st: &SessionState) -> Vec<String> {
    st.transcript
        .wire()
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

fn is_notice(text: &str) -> bool {
    text.starts_with(prompt::REPEAT_NOTICE_HEAD)
}

const RANGE: &str = r#"{"path":"a.txt","offset":5,"limit":3}"#;

#[test]
fn identical_read_loop_gets_notices_then_stops() {
    let env = TestEnv::new();
    // The Block 1 shape: the same call again and again, far more than the guard allows.
    let turns: Vec<ScriptTurn> = (0..20).map(|i| read(&format!("c{i}"), RANGE)).collect();
    let (mut st, eng) = state(&env, turns, Settings::default());
    let (end, events) = run(&mut st, "find the bug in a.txt");
    assert_eq!(end, RunEnd::Stopped);
    let res = results(&st);
    let n_notices = NOTICES_BEFORE_STOP as usize + 1;
    assert_eq!(res.len(), LIMIT as usize + n_notices, "{res:#?}");
    // The first LIMIT calls return the content, unchanged and identical.
    for r in &res[..LIMIT as usize] {
        assert!(r.contains("line 5") && !is_notice(r), "{r}");
        assert_eq!(r, &res[0]);
    }
    // Then notices: they quote the call and say where the content is.
    for r in &res[LIMIT as usize..] {
        assert!(is_notice(r), "{r}");
        assert!(r.contains(r#"read {"path":"a.txt","offset":5,"limit":3}"#), "{r}");
        assert!(r.contains(&format!("same result {LIMIT} times in a row")), "{r}");
        assert!(!r.contains("line 5"), "{r}");
    }
    // The run ended with a shown stop message after the last notice; no further request.
    assert_eq!(eng.requests().len(), res.len());
    let shown: Vec<_> = st
        .tree
        .branch(st.tree.leaf())
        .into_iter()
        .filter_map(|e| match &e.body {
            EntryBody::Shown { text, why } => Some((text.clone(), *why)),
            _ => None,
        })
        .collect();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0].1, shown_why::NOTICE);
    assert!(shown[0]
        .0
        .starts_with("Run stopped: the model repeated the same tool call"));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Notice { text, .. } if text.starts_with("Run stopped"))));
    // W1: every request extends the one before (the notice is a new result, nothing rewritten).
    assert!(oxa_fake::check_w1(&eng.requests(), &[]).is_empty());
}

#[test]
fn the_model_can_recover_after_one_notice() {
    let env = TestEnv::new();
    let mut turns: Vec<ScriptTurn> = (0..=LIMIT).map(|i| read(&format!("c{i}"), RANGE)).collect();
    turns.push(ScriptTurn::text("the bug is on line 5"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    let (end, _) = run(&mut st, "find the bug in a.txt");
    assert_eq!(end, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.iter().filter(|r| is_notice(r)).count(), 1);
    assert!(st.transcript.wire().last().unwrap().content.as_deref() == Some("the bug is on line 5"));
}

#[test]
fn rereads_after_an_edit_never_trip_the_guard() {
    let env = TestEnv::new();
    let mut turns = Vec::new();
    for round in 0..3 {
        for i in 0..LIMIT {
            turns.push(read(&format!("r{round}-{i}"), RANGE));
        }
        // An edit outside the range: the reads after it return the same bytes as before.
        let args = format!(
            r#"{{"path":"a.txt","oldText":"line {}\n","newText":"line {} edited\n"}}"#,
            30 + round,
            30 + round
        );
        turns.push(ScriptTurn::tool(&format!("e{round}"), "edit", &args));
    }
    for i in 0..LIMIT {
        turns.push(read(&format!("last-{i}"), RANGE));
    }
    turns.push(ScriptTurn::text("done"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    let (end, _) = run(&mut st, "edit a.txt");
    assert_eq!(end, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), 4 * LIMIT as usize + 3);
    assert!(res.iter().all(|r| !is_notice(r)), "{res:#?}");
    assert!(std::fs::read_to_string(env.work().join("a.txt"))
        .unwrap()
        .contains("line 32 edited"));
}

#[test]
fn different_ranges_key_order_and_a_new_prompt() {
    let env = TestEnv::new();
    // Paging through a file: never a repeat.
    let mut turns: Vec<ScriptTurn> = (0..8)
        .map(|i| {
            read(
                &format!("p{i}"),
                &format!(r#"{{"path":"a.txt","offset":{},"limit":5}}"#, 1 + 5 * i),
            )
        })
        .collect();
    // The same call with its keys in another order IS the same call.
    turns.push(read("k0", r#"{"path":"a.txt","limit":2}"#));
    turns.push(read("k1", r#"{"limit":2,"path":"a.txt"}"#));
    turns.push(read("k2", r#"{"limit":2, "path":"a.txt"}"#));
    turns.push(read("k3", r#"{"path":"a.txt","limit":2}"#));
    turns.push(ScriptTurn::text("done"));
    // A new prompt starts afresh: the same range again is answered with its content.
    turns.push(read("n0", r#"{"path":"a.txt","limit":2}"#));
    turns.push(ScriptTurn::text("done again"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    assert_eq!(run(&mut st, "page through a.txt").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), 12);
    assert!(res[..11].iter().all(|r| !is_notice(r)), "{res:#?}");
    assert!(is_notice(&res[11]), "{}", res[11]);
    assert_eq!(run(&mut st, "read it again").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), 13);
    assert!(!is_notice(&res[12]) && res[12].contains("line 1"), "{}", res[12]);
}

#[test]
fn the_setting_turns_the_guard_off() {
    let env = TestEnv::new();
    let mut turns: Vec<ScriptTurn> = (0..8).map(|i| read(&format!("c{i}"), RANGE)).collect();
    turns.push(ScriptTurn::text("done"));
    let off = Settings::from_values(
        serde_json::json!({"oxa": {"toolRepeatGuard": false}}),
        Value::Null,
    );
    let (mut st, _eng) = state(&env, turns, off);
    assert_eq!(run(&mut st, "read").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), 8);
    assert!(res.iter().all(|r| !is_notice(r)));
}

#[test]
fn sampling_is_greedy_unless_the_model_mode_is_set() {
    let turns = || vec![read("c0", RANGE), ScriptTurn::text("done")];
    // Default: greedy (W6) on every request, also for a model the card table knows.
    let env = TestEnv::new();
    let (mut st, eng) = state_for(&env, turns(), Settings::default(), "ornith-35b-a3b-local");
    run(&mut st, "read");
    assert!(eng.requests().iter().all(|r| r.temperature == 0.0));
    // `model`: the card's values by thinking state (thinking on for the prompt reply, off for the
    // tool follow-up under the `auto` policy of profile `local`).
    let env = TestEnv::new();
    let on = Settings::from_values(
        serde_json::json!({"oxa": {"profile": "local", "sampling": {"mode": "model", "seed": 7}}}),
        Value::Null,
    );
    let (mut st, eng) = state_for(&env, turns(), on.clone(), "ornith-35b-a3b-local");
    st.stats.profile = oxa_proto::Profile::Local;
    run(&mut st, "read");
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        let want = oxa_core::sampling::card(&r.model, r.thinking)
            .unwrap()
            .temperature;
        assert_eq!(r.temperature, f64::from(want), "thinking {}", r.thinking);
    }
    // A model the table does not know stays greedy.
    let env = TestEnv::new();
    let (mut st, eng) = state_for(&env, turns(), on, "fake-model");
    run(&mut st, "read");
    assert!(eng.requests().iter().all(|r| r.temperature == 0.0));
}

#[test]
fn card_values_follow_the_qwen36_card() {
    let t = oxa_core::sampling::card("Ornith-35B-A3B", true).unwrap();
    assert_eq!(
        (t.temperature, t.top_p, t.top_k, t.presence_penalty),
        (0.6, 0.95, 20, 0.0)
    );
    let n = oxa_core::sampling::card("qwen3.6-35b-a3b", false).unwrap();
    assert_eq!(
        (n.temperature, n.top_p, n.top_k, n.presence_penalty),
        (0.7, 0.8, 20, 1.5)
    );
    assert!(oxa_core::sampling::card("llama-3", true).is_none());
}

/// Codex T2 review: `[read A, edit A, read A]` in ONE batch, after LIMIT identical reads of A on
/// the wire. The edit touches lines outside the range, so the second read returns the same bytes;
/// it follows an edit of its own batch and must not be judged a repeat. The first read of the
/// batch (before the edit) is a real repeat and gets the notice.
#[test]
fn a_read_after_an_edit_in_the_same_batch_is_not_a_repeat() {
    let env = TestEnv::new();
    let mut turns: Vec<ScriptTurn> = (0..LIMIT).map(|i| read(&format!("c{i}"), RANGE)).collect();
    let mut batch = read("b0", RANGE);
    batch.tool_calls.push(oxa_proto::ToolCall {
        id: "b1".to_string(),
        name: "edit".to_string(),
        args_raw: r#"{"path":"a.txt","oldText":"line 30\n","newText":"line 30 edited\n"}"#.to_string(),
    });
    batch.tool_calls.push(oxa_proto::ToolCall {
        id: "b2".to_string(),
        name: "read".to_string(),
        args_raw: RANGE.to_string(),
    });
    turns.push(batch);
    // After the batch the edit is on the wire: the same read again is fresh content.
    turns.push(read("after", RANGE));
    turns.push(ScriptTurn::text("done"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    assert_eq!(run(&mut st, "edit a.txt").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), LIMIT as usize + 4, "{res:#?}");
    let b = LIMIT as usize;
    assert!(is_notice(&res[b]), "the read before the edit repeats: {}", res[b]);
    assert!(!is_notice(&res[b + 1]), "edit result: {}", res[b + 1]);
    assert!(
        !is_notice(&res[b + 2]) && res[b + 2].contains("line 5"),
        "read after the edit: {}",
        res[b + 2]
    );
    assert!(
        !is_notice(&res[b + 3]) && res[b + 3].contains("line 5"),
        "next read: {}",
        res[b + 3]
    );
    assert!(std::fs::read_to_string(env.work().join("a.txt"))
        .unwrap()
        .contains("line 30 edited"));
}

fn bash(id: &str, command: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "bash", &serde_json::json!({ "command": command }).to_string())
}

/// Block 1 Arm A: the loop moved to an identical bash command (here one that fails the same way
/// each time: an error result is covered too).
#[test]
fn identical_bash_loop_gets_notices_then_stops() {
    let env = TestEnv::new();
    let turns: Vec<ScriptTurn> = (0..20)
        .map(|i| bash(&format!("b{i}"), "echo 'test result: FAILED'; exit 101"))
        .collect();
    let (mut st, eng) = state(&env, turns, Settings::default());
    let (end, _) = run(&mut st, "fix the failing test");
    assert_eq!(end, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), (LIMIT + NOTICES_BEFORE_STOP + 1) as usize, "{res:#?}");
    for r in &res[..LIMIT as usize] {
        assert!(r.contains("test result: FAILED") && !is_notice(r), "{r}");
    }
    for r in &res[LIMIT as usize..] {
        assert!(is_notice(r) && r.contains("(bash {"), "{r}");
    }
    assert_eq!(eng.requests().len(), res.len());
}

/// `cargo test` re-run after each edit: the same failing output every time, but an edit sits
/// between the runs, so the run of identical (call, result) pairs is never consecutive.
#[test]
fn test_reruns_after_edits_never_trip_the_guard() {
    let env = TestEnv::new();
    let mut turns = Vec::new();
    for i in 0..6 {
        turns.push(bash(&format!("t{i}"), "echo 'test result: FAILED'; exit 101"));
        let args = format!(
            r#"{{"path":"a.txt","oldText":"line {}\n","newText":"line {} edited\n"}}"#,
            20 + i,
            20 + i
        );
        turns.push(ScriptTurn::tool(&format!("e{i}"), "edit", &args));
    }
    turns.push(bash("t-last", "echo 'test result: FAILED'; exit 101"));
    turns.push(ScriptTurn::text("done"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    assert_eq!(run(&mut st, "fix the failing test").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), 13);
    assert!(res.iter().all(|r| !is_notice(r)), "{res:#?}");
    assert!(
        res[0].contains("test result: FAILED") && res[12] == res[0],
        "{res:#?}"
    );
}

/// A polling command whose output changes on every run is a legitimate re-run.
#[test]
fn polling_with_changing_output_never_trips_the_guard() {
    let env = TestEnv::new();
    let mut turns: Vec<ScriptTurn> = (0..8)
        .map(|i| bash(&format!("p{i}"), "echo x >> n.txt; wc -l < n.txt"))
        .collect();
    turns.push(ScriptTurn::text("done"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    assert_eq!(run(&mut st, "poll").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), 8);
    assert!(res.iter().all(|r| !is_notice(r)), "{res:#?}");
    assert!(res[7].contains('8'), "{}", res[7]);
}

/// Different tools interleaved: no consecutive identical (call, result) pair, never a repeat.
#[test]
fn mixed_tools_never_trip_the_guard() {
    let env = TestEnv::new();
    let mut turns = Vec::new();
    for i in 0..5 {
        turns.push(read(&format!("r{i}"), RANGE));
        turns.push(bash(&format!("b{i}"), "echo same"));
        turns.push(ScriptTurn::tool(
            &format!("g{i}"),
            "grep",
            r#"{"pattern":"line 1","path":"a.txt"}"#,
        ));
    }
    turns.push(ScriptTurn::text("done"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    assert_eq!(run(&mut st, "look around").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), 15);
    assert!(res.iter().all(|r| !is_notice(r)), "{res:#?}");
    assert!(res[1].contains("same") && res[2].contains("line 1"), "{res:#?}");
}

/// The same call several times inside ONE batch counts like consecutive turns.
#[test]
fn identical_calls_inside_one_batch_count() {
    let env = TestEnv::new();
    let mut batch = bash("b0", "echo same");
    for i in 1..=LIMIT {
        batch.tool_calls.push(oxa_proto::ToolCall {
            id: format!("b{i}"),
            name: "bash".to_string(),
            args_raw: r#"{"command":"echo same"}"#.to_string(),
        });
    }
    let (mut st, _eng) = state(&env, vec![batch, ScriptTurn::text("done")], Settings::default());
    assert_eq!(run(&mut st, "go").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), LIMIT as usize + 1);
    assert!(res[..LIMIT as usize].iter().all(|r| !is_notice(r)), "{res:#?}");
    assert!(is_notice(&res[LIMIT as usize]), "{}", res[LIMIT as usize]);
    assert!(res[0].contains("same"), "{}", res[0]);
}
