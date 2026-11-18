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
    state_with(env, turns, settings, model, tools())
}

fn state_with(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    settings: Settings,
    model: &str,
    tools: Vec<Arc<dyn Tool>>,
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
        wait_wake: CancelToken::new(),
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
    });
    let st = SessionState {
        goal: None,
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
        jobs: oxa_core::fresh_jobs(),
    };
    (st, eng)
}

fn run(st: &mut SessionState, prompt_text: &str) -> (RunEnd, Vec<AgentEvent>) {
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
    // Giving up is not a finished task: the run ends as failed with its own error class.
    assert!(
        matches!(&end, RunEnd::Failed(e) if e.code == "repeat_guard"),
        "{end:?}"
    );
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
    // tool follow-up under the default `auto` policy).
    let env = TestEnv::new();
    let on = Settings::from_values(
        serde_json::json!({"oxa": {"sampling": {"mode": "model", "seed": 7}}}),
        Value::Null,
    );
    let (mut st, eng) = state_for(&env, turns(), on.clone(), "ornith-35b-a3b-local");
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
    assert!(
        matches!(&end, RunEnd::Failed(e) if e.code == "repeat_guard"),
        "{end:?}"
    );
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

/// Codex focused note on the PR: `[other, same x(LIMIT+1)]` in ONE batch. The different call
/// before the run must end the walk without discarding the identical suffix already counted, so
/// the last `same` gets the notice (it used to pass because the walk returned Pass on `other`).
#[test]
fn identical_suffix_after_a_different_call_in_one_batch_counts() {
    let env = TestEnv::new();
    let mut batch = bash("b0", "echo other");
    for i in 1..=LIMIT + 1 {
        batch.tool_calls.push(oxa_proto::ToolCall {
            id: format!("b{i}"),
            name: "bash".to_string(),
            args_raw: r#"{"command":"echo same"}"#.to_string(),
        });
    }
    let (mut st, _eng) = state(&env, vec![batch, ScriptTurn::text("done")], Settings::default());
    assert_eq!(run(&mut st, "go").0, RunEnd::Stopped);
    let res = results(&st);
    assert_eq!(res.len(), LIMIT as usize + 2, "{res:#?}");
    assert!(res[..=LIMIT as usize].iter().all(|r| !is_notice(r)), "{res:#?}");
    assert!(is_notice(&res[LIMIT as usize + 1]), "{}", res[LIMIT as usize + 1]);
}

/// #150 (1): a background job that prints nothing and runs on until cancelled.
struct Hold;
impl Sandbox for Hold {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        while !c.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ExecResult {
            output: Vec::new(),
            exit: None,
            timed_out: false,
        }
    }
}

/// #150 (1): a job's identical polls (same state, same output) are a stuck loop and trip the
/// guard exactly like any identical call, with the same notices and stop.
#[test]
fn identical_job_polls_trip_the_guard() {
    let env = TestEnv::new();
    let jobs = Arc::new(oxa_tools::JobRegistry::new());
    jobs.set_sandbox(Arc::new(Hold));
    let mut t = oxa_tools::builtin_tools_with_shell_and_jobs(oxa_tools::ShellConfig::default(), jobs.clone());
    t.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    let mut turns = vec![ScriptTurn::tool(
        "s",
        "bash",
        r#"{"command":"sleep 100","background":true}"#,
    )];
    turns.extend((0..20).map(|i| ScriptTurn::tool(&format!("p{i}"), "bash", r#"{"job":"job-1"}"#)));
    let (mut st, _eng) = state_with(&env, turns, Settings::default(), "fake-model", t);
    let (end, _) = run(&mut st, "run it in the background and poll it");
    jobs.kill_all();
    assert!(
        matches!(&end, RunEnd::Failed(e) if e.code == "repeat_guard"),
        "{end:?}"
    );
    let res = results(&st);
    let polls = &res[1..];
    assert_eq!(
        res.len(),
        1 + (LIMIT + NOTICES_BEFORE_STOP + 1) as usize,
        "{res:#?}"
    );
    assert!(res[0].starts_with("Started background job job-1"), "{}", res[0]);
    for r in &polls[..LIMIT as usize] {
        assert!(r.starts_with("Job job-1: running") && !is_notice(r), "{r}");
    }
    for r in &polls[LIMIT as usize..] {
        assert!(is_notice(r) && r.contains("(bash {"), "{r}");
    }
}

fn job_tools(sb: Arc<dyn Sandbox>) -> (Vec<Arc<dyn Tool>>, Arc<oxa_tools::JobRegistry>) {
    let jobs = Arc::new(oxa_tools::JobRegistry::new());
    jobs.set_sandbox(sb);
    let mut t = oxa_tools::builtin_tools_with_shell_and_jobs(oxa_tools::ShellConfig::default(), jobs.clone());
    t.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    (t, jobs)
}

fn poll_run(cmd: &str, poll: &str, sb: Arc<dyn Sandbox>) -> Vec<String> {
    let env = TestEnv::new();
    let (t, jobs) = job_tools(sb);
    let mut turns = vec![ScriptTurn::tool(
        "s",
        "bash",
        &format!(r#"{{"command":"{cmd}","background":true}}"#),
    )];
    turns.extend((0..LIMIT + 3).map(|i| ScriptTurn::tool(&format!("p{i}"), "bash", poll)));
    let (mut st, _eng) = state_with(&env, turns, Settings::default(), "fake-model", t);
    run(&mut st, "poll it");
    jobs.kill_all();
    results(&st)
}

/// #439: blocking polls (`wait_s`) of a job that is still running are not repeats.
#[test]
fn waiting_polls_of_a_running_job_are_not_repeats() {
    let res = poll_run("sleep 100", r#"{"job":"job-1","wait_s":1}"#, Arc::new(Hold));
    assert!(res.len() > LIMIT as usize + 2, "{res:#?}");
    for r in &res[1..] {
        assert!(r.starts_with("Job job-1: running") && !is_notice(r), "{r}");
    }
}

/// Codex finding on #441: a finished job whose OUTPUT has a line `Still running.` is no exemption.
#[test]
fn output_that_says_still_running_is_no_exemption() {
    let res = poll_run(
        "printf 'x\\nStill running.\\n'",
        r#"{"job":"job-1","wait_s":10}"#,
        Arc::new(ShSandbox),
    );
    let polls = &res[1..];
    for r in &polls[..LIMIT as usize] {
        assert!(r.starts_with("Job job-1: exited 0") && !is_notice(r), "{r}");
    }
    assert!(polls[LIMIT as usize..].iter().all(|r| is_notice(r)), "{res:#?}");
}

/// Review on #441: `wait_s` 0 or null does not wait (parse_wait), so it is no exemption (#150).
#[test]
fn zero_or_null_wait_polls_still_trip_the_guard() {
    for poll in [
        r#"{"job":"job-1","wait_s":0}"#,
        r#"{"job":"job-1","wait_s":null}"#,
        r#"{"job":"job-1","wait_s":0.001}"#,
    ] {
        let res = poll_run("sleep 100", poll, Arc::new(Hold));
        assert!(res[1..].iter().any(|r| is_notice(r)), "{poll}: {res:#?}");
    }
}

/// A repeat of an unchanged range is still a repeat when other read-only calls (another range)
/// sit between the copies: the content is still in context (#463). An edit in between breaks it.
#[test]
fn identical_reads_interleaved_with_other_reads_trip_the_guard() {
    let env = TestEnv::new();
    let other = r#"{"path":"a.txt","offset":20,"limit":3}"#;
    let mut turns = Vec::new();
    for i in 0..=LIMIT {
        turns.push(read(&format!("a{i}"), RANGE));
        turns.push(read(&format!("b{i}"), other));
    }
    turns.push(ScriptTurn::text("done"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    run(&mut st, "look");
    let res = results(&st);
    assert!(is_notice(&res[2 * LIMIT as usize]), "{res:#?}");
}

fn call(id: &str, name: &str, args: &str) -> oxa_proto::ToolCall {
    oxa_proto::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args_raw: args.to_string(),
    }
}

/// Codex P2 on #471: interleaved reads inside ONE batch `[A, B, A, B, A, B, A]` are counted the
/// same as when spread across turns: the fourth read of A gets the notice.
#[test]
fn identical_reads_interleaved_in_one_batch_trip_the_guard() {
    let env = TestEnv::new();
    let other = r#"{"path":"a.txt","offset":20,"limit":3}"#;
    let mut batch = read("x0", RANGE);
    for i in 0..LIMIT {
        batch.tool_calls.push(call(&format!("o{i}"), "read", other));
        batch.tool_calls.push(call(&format!("r{i}"), "read", RANGE));
    }
    let turns = vec![batch, ScriptTurn::text("done")];
    let (mut st, _eng) = state(&env, turns, Settings::default());
    run(&mut st, "look");
    let res = results(&st);
    assert_eq!(res.len(), 2 * LIMIT as usize + 1, "{res:#?}");
    assert!(!is_notice(&res[2 * LIMIT as usize - 2]), "{res:#?}");
    assert!(is_notice(&res[2 * LIMIT as usize]), "{res:#?}");
}

/// An edit between identical reads (in the batch) ends the run even with read-only calls around it.
#[test]
fn an_edit_between_interleaved_batch_reads_breaks_the_run() {
    let env = TestEnv::new();
    let other = r#"{"path":"a.txt","offset":20,"limit":3}"#;
    let edit = r#"{"path":"a.txt","oldText":"line 30\n","newText":"line 30 edited\n"}"#;
    let mut batch = read("x0", RANGE);
    for (i, (n, a)) in [
        ("read", other),
        ("read", RANGE),
        ("read", RANGE),
        ("edit", edit),
        ("read", other),
        ("read", RANGE),
    ]
    .iter()
    .enumerate()
    {
        batch.tool_calls.push(call(&format!("y{i}"), n, a));
    }
    let turns = vec![batch, ScriptTurn::text("done")];
    let (mut st, _eng) = state(&env, turns, Settings::default());
    run(&mut st, "edit");
    let res = results(&st);
    assert!(res.iter().all(|r| !is_notice(r)), "{res:#?}");
}

const BAD_EDIT: &str = r#"{"path":"a.txt","oldText":"no such text\n","newText":"x\n"}"#;
const BAD_WRITE: &str = r#"{"path":"a.txt/child","content":"draft"}"#;

/// PR #563: failed writers still count as repeats themselves, including their notices,
/// even though they do not interrupt another call's repeat sequence.
#[test]
fn identical_failed_writers_in_one_batch_get_notices_then_stop() {
    for (name, args) in [("edit", BAD_EDIT), ("write", BAD_WRITE)] {
        let env = TestEnv::new();
        let count = LIMIT + NOTICES_BEFORE_STOP + 1;
        let mut batch = ScriptTurn::tool("w0", name, args);
        for i in 1..count {
            batch.tool_calls.push(call(&format!("w{i}"), name, args));
        }
        let (mut st, eng) = state(
            &env,
            vec![batch, ScriptTurn::text("not reached")],
            Settings::default(),
        );
        let end = run(&mut st, "fix a.txt").0;
        assert!(
            matches!(&end, RunEnd::Failed(e) if e.code == "repeat_guard"),
            "{name}: {end:?}"
        );
        let res = results(&st);
        assert_eq!(res.len(), count as usize, "{name}: {res:#?}");
        assert!(
            res[..LIMIT as usize].iter().all(|r| !is_notice(r)),
            "{name}: {res:#?}"
        );
        assert!(
            res[LIMIT as usize..].iter().all(|r| is_notice(r)),
            "{name}: {res:#?}"
        );
        assert_eq!(
            eng.requests().len(),
            1,
            "{name}: no request after the stop notice"
        );
        assert_eq!(
            std::fs::read_to_string(env.work().join("a.txt"))
                .unwrap()
                .lines()
                .count(),
            40
        );
    }
}

/// A command still breaks the run (it may change files), but must not hide the
/// identical failed writer suffix inside each batch, including later batches.
#[test]
fn identical_failed_writer_suffixes_after_commands_get_notices_in_each_batch() {
    for (name, args) in [("edit", BAD_EDIT), ("write", BAD_WRITE)] {
        let env = TestEnv::new();
        let mut turns = Vec::new();
        for i in 0..2 {
            let mut batch = bash(&format!("b{i}"), "echo other");
            for j in 0..=LIMIT {
                batch.tool_calls.push(call(&format!("w{i}-{j}"), name, args));
            }
            turns.push(batch);
        }
        turns.push(ScriptTurn::text("done"));
        let (mut st, eng) = state(&env, turns, Settings::default());
        assert_eq!(run(&mut st, "fix a.txt").0, RunEnd::Stopped);
        let res = results(&st);
        for batch in res.as_chunks::<{ LIMIT as usize + 2 }>().0 {
            assert!(
                batch[..=LIMIT as usize].iter().all(|r| !is_notice(r)),
                "{name}: {batch:#?}"
            );
            assert!(is_notice(&batch[LIMIT as usize + 1]), "{name}: {batch:#?}");
        }
        assert_eq!(res.len(), 2 * (LIMIT as usize + 2), "{name}: {res:#?}");
        assert_eq!(eng.requests().len(), 3, "a command still resets the notice count");
    }
}

/// #507: an `edit` that failed changed nothing, so it does not end a run of identical reads:
/// `read, edit (fails), read, edit (fails), ...` is a loop and gets the notice. A successful
/// edit still ends the run (`rereads_after_an_edit_never_trip_the_guard`).
#[test]
fn failed_edits_between_identical_reads_do_not_break_the_run() {
    let env = TestEnv::new();
    let mut turns = Vec::new();
    for i in 0..=LIMIT {
        turns.push(read(&format!("r{i}"), RANGE));
        turns.push(ScriptTurn::tool(&format!("e{i}"), "edit", BAD_EDIT));
    }
    turns.push(ScriptTurn::text("done"));
    let (mut st, _eng) = state(&env, turns, Settings::default());
    run(&mut st, "fix a.txt");
    let res = results(&st);
    assert!(res[1].starts_with("Could not find"), "{res:#?}");
    assert!(is_notice(&res[2 * LIMIT as usize]), "{res:#?}");
    assert!(!is_notice(&res[2 * LIMIT as usize - 2]), "{res:#?}");
}

/// The same inside one batch: the failed edits are not in the way.
#[test]
fn failed_edits_between_identical_reads_in_one_batch_do_not_break_the_run() {
    let env = TestEnv::new();
    let mut batch = read("x0", RANGE);
    for i in 0..LIMIT {
        batch.tool_calls.push(call(&format!("e{i}"), "edit", BAD_EDIT));
        batch.tool_calls.push(call(&format!("r{i}"), "read", RANGE));
    }
    let turns = vec![batch, ScriptTurn::text("done")];
    let (mut st, _eng) = state(&env, turns, Settings::default());
    run(&mut st, "fix a.txt");
    let res = results(&st);
    assert_eq!(res.len(), 2 * LIMIT as usize + 1, "{res:#?}");
    assert!(is_notice(&res[2 * LIMIT as usize]), "{res:#?}");
}

fn turn_limit_settings(n: u32) -> Settings {
    Settings::from_values(serde_json::json!({"oxa": {"maxTurns": n}}), Value::Null)
}

/// #507: `oxa.maxTurns` ends a run that keeps calling tools, however the calls vary (so no
/// repeat guard can see them). Unset (the default) there is no limit.
#[test]
fn max_turns_ends_a_run_that_keeps_calling_tools() {
    let env = TestEnv::new();
    let turns: Vec<ScriptTurn> = (0..20)
        .map(|i| {
            read(
                &format!("c{i}"),
                &format!(r#"{{"path":"a.txt","offset":{},"limit":2}}"#, i + 1),
            )
        })
        .collect();
    let (mut st, eng) = state(&env, turns, turn_limit_settings(5));
    let (end, events) = run(&mut st, "look around");
    match end {
        RunEnd::Failed(e) => assert_eq!(e.code, "turn_limit", "{e}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(eng.requests().len(), 5);
    assert_eq!(results(&st).len(), 5);
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Notice { text, .. } if text.contains("oxa.maxTurns"))));
    let shown = st
        .tree
        .branch(st.tree.leaf())
        .into_iter()
        .filter(|e| matches!(&e.body, EntryBody::Shown { why, .. } if *why == shown_why::FAILED))
        .count();
    assert_eq!(shown, 1);
}

/// A run that ends within the limit is not touched; the limit counts per run, not per session.
#[test]
fn max_turns_leaves_a_run_within_the_limit_alone() {
    let env = TestEnv::new();
    let mut turns: Vec<ScriptTurn> = (0..2)
        .map(|i| {
            read(
                &format!("c{i}"),
                &format!(r#"{{"path":"a.txt","offset":{},"limit":2}}"#, i + 1),
            )
        })
        .collect();
    turns.push(ScriptTurn::text("one"));
    turns.push(read("d0", RANGE));
    turns.push(ScriptTurn::text("two"));
    let (mut st, _eng) = state(&env, turns, turn_limit_settings(3));
    assert_eq!(run(&mut st, "first").0, RunEnd::Stopped);
    assert_eq!(run(&mut st, "second").0, RunEnd::Stopped);
}

#[test]
fn advisor_returns_to_the_same_worker_and_does_not_repeat_the_call() {
    use oxa_core::advice::{AdviceContext, AdviceDispatcher, BoundAdvisor};
    use oxa_session::{
        advice::{AdviceGrant, AdvicePolicy, AdviceTier, AdviceTrigger, AdvisorRoute},
        SessionRef,
    };
    use std::{
        num::{NonZeroU16, NonZeroU32},
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Advisor(AtomicUsize);
    impl AdviceDispatcher for Advisor {
        fn authorized(&self, _: &SessionRef, _: u64) -> bool {
            true
        }

        fn call(
            &self,
            grant: &AdviceGrant,
            context: &AdviceContext,
            cancel: &CancelToken,
        ) -> Result<String, OxaError> {
            assert_eq!(grant.advisor.pool, "fixture-advisor");
            assert_eq!(grant.max_output.get(), 64);
            assert_eq!(context.objective, "Review the synthetic file.");
            assert!(context.delta.contains("read"));
            assert!(context.delta.contains("line 5"));
            assert!(context.failing_evidence.contains("same tool call"));
            assert!(!cancel.is_cancelled());
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("Use the current contents to write the patch.".into())
        }
    }

    for retry_same in [false, true] {
        let env = TestEnv::new();
        let stop = LIMIT + NOTICES_BEFORE_STOP + 1;
        let mut turns: Vec<_> = (0..stop).map(|i| read(&format!("c{i}"), RANGE)).collect();
        if retry_same {
            turns.extend((0..stop).map(|i| {
                read(
                    &format!("retry{i}"),
                    r#"{ "limit":3, "offset":5, "path":"a.txt" }"#,
                )
            }));
        } else {
            turns.push(ScriptTurn::text("Continued on the original worker."));
        }
        let (mut st, worker) = state(&env, turns, Settings::default());
        let advisor = Arc::new(Advisor(AtomicUsize::new(0)));
        st.shared.bind_advisor(BoundAdvisor {
            session: st.sref.clone(),
            lease_identity: [1; 32],
            objective: "Review the synthetic file.".into(),
            worker: AdviceTier::Bulk,
            done_checks_after: NonZeroU32::MIN,
            policy: AdvicePolicy {
                triggers: vec![AdviceTrigger::RepeatGuard],
                max_calls: NonZeroU16::new(2).unwrap(),
                failures_before_escalation: NonZeroU16::new(1).unwrap(),
                max_output: NonZeroU32::new(64).unwrap(),
            },
            route: AdvisorRoute {
                tier: AdviceTier::Main,
                model: "fixture-main".into(),
                pool: "fixture-advisor".into(),
            },
            dispatcher: advisor.clone(),
            scrub: Arc::new(str::to_owned),
        });
        let (end, _) = run(&mut st, "find the bug in a.txt");
        if retry_same {
            assert!(matches!(end, RunEnd::Failed(error) if error.code == "advisor_escalation_required"));
        } else {
            assert_eq!(end, RunEnd::Stopped);
        }
        assert_eq!(advisor.0.load(Ordering::SeqCst), 1);
        let requests = worker.requests();
        assert!(requests.iter().all(|request| request.model == "fake-model"));
        assert!(requests
            .iter()
            .flat_map(|request| &request.messages)
            .any(|message| message
                .content
                .as_deref()
                .is_some_and(|text| text.contains("write the patch"))));
    }
}
