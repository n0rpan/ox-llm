//! C10 acceptance tests: RPC server, run-control commands.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The server runs in a thread over an OS pipe, so a test can send a command while a run is
//! still in progress. A `GatedEngine` holds each model request until the test releases it (or
//! until the run is cancelled), which makes "during a run" deterministic.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, RunOutcome, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, AssistantTurn, CancelToken, ChatRequest, Decision, Engine, EngineInfo, ExecResult,
    ExecSpec, Mode, OxaError, Policy, Sandbox, StreamEvent,
};
use oxa_rpc::{parse_command, response_err, response_ok, serve_outcome, Out, RpcCommand};
use oxa_session::{FsStore, SessionStore};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A policy for sessions without tools: every access is refused.
struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Deny("no tools in this test")
    }
    fn remember(&mut self, _a: &Access) {}
}

/// A sandbox that is never reached: the sessions here run no tools.
struct NoBwrap;

impl Sandbox for NoBwrap {
    fn check(&self) -> Result<(), OxaError> {
        Err(OxaError::new(codes::NO_SANDBOX, "no sandbox in this test"))
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

/// Holds every model request until `open` is set, or until the run is cancelled. Otherwise it
/// is the scripted engine. `models` replaces the model list the engine reports (when set).
struct GatedEngine {
    inner: Arc<ScriptedEngine>,
    open: Arc<AtomicBool>,
    models: Vec<String>,
    entered: Option<Arc<AtomicBool>>,
}

impl Engine for GatedEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let mut info = self.inner.info(target)?;
        if !self.models.is_empty() {
            info.models = self.models.clone();
        }
        Ok(info)
    }

    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }

    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        if let Some(entered) = &self.entered {
            entered.store(true, Ordering::SeqCst);
        }
        while !self.open.load(Ordering::SeqCst) {
            if cancel.is_cancelled() {
                return Err(OxaError::new(
                    codes::ABORTED,
                    "aborted while the test held the request",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
        self.inner.chat(r, cancel, on)
    }
}

/// A session over `engine`, stored under the test's own agent directory.
fn open_session(env: &TestEnv, engine: Arc<dyn Engine>) -> Session {
    Session::open(session_deps(env, engine), Open::New).expect("the session opens over the engine")
}

fn session_deps(env: &TestEnv, engine: Arc<dyn Engine>) -> Deps {
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.agent_dir().join("sessions")));
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        // Exact prompt-byte fixtures select note-free queues/RPC/replay.
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn control_files_are_not_consumed_when_the_control_lane_is_off() {
    use oxa_session::send_inbox_control;

    fn control_files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| {
                        let path = entry.path();
                        if path.is_dir() {
                            control_files(&path)
                        } else {
                            usize::from(path.to_string_lossy().contains(".control-"))
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![]));
    let owner = open_session(&env, engine);
    let bound = owner.inner().sref.clone();
    let agent = Dirs::under(env.root()).agent_dir;
    let mut rpc = Rpc::start_with(owner, &env, |session, dirs, input, out| {
        oxa_rpc::serve_outcome_gated(session, dirs, input, out, false)
    });
    rpc.call("ready", json!({"type":"get_state"}));
    let packet = json!({"method":"cancel","id":"off","revision":1});
    send_inbox_control(&agent, &bound, &packet.to_string()).unwrap();
    assert_eq!(control_files(&agent), 1);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(control_files(&agent), 1, "the control file must stay unread");
    assert!(
        !rpc.records()
            .iter()
            .any(|record| record["type"] == "goal_control_receipt"),
        "no receipt without the control lane"
    );
    rpc.finish();
}

#[cfg(target_os = "linux")]
#[test]
fn file_control_receipts_report_revisioned_and_durable_owner_transitions() {
    use oxa_orchestrator::control::Request;
    use oxa_proto::{HeadlessUi, NullSink};
    use oxa_session::{send_inbox_control, Goal, GoalBudget, GoalStatus, SessionRef};

    let env = TestEnv::new();
    // The applied resume starts one continuation run: two idle replies, then the goal (no budget
    // of its own) pauses by default.
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("Nothing to do yet."),
        ScriptTurn::text("Still nothing."),
    ]));
    let mut owner = open_session(&env, engine.clone());
    let mut sink = NullSink;
    let goal = Goal::new("Review synthetic work.".into(), GoalBudget::default(), 1000).unwrap();
    owner.set_goal(Some(goal), &mut sink).unwrap();
    let bound = owner.inner().sref.clone();
    let original = owner.goal_revision();
    let agent = Dirs::under(env.root()).agent_dir;
    let mut rpc = Rpc::start(owner, &env);
    let ready = rpc.call("ready", json!({"type":"get_state"}));
    assert_eq!(ready["data"]["goalRevision"], original);
    let enqueue = |method: &str, id: &str, revision| {
        let packet = json!({"method": method, "id": id, "revision": revision});
        send_inbox_control(&agent, &bound, &packet.to_string()).unwrap();
    };
    let receipt = |rpc: &Rpc, id: &str| {
        rpc.wait_for(id, |record| {
            record["type"] == "goal_control_receipt" && record["data"]["id"] == id
        })["data"]
            .clone()
    };

    let other = SessionRef {
        id: bound.id.clone(),
        path: bound.path.with_file_name("other.jsonl"),
    };
    let misdirected = json!({"method":"cancel","id":"other","revision":original});
    send_inbox_control(&agent, &other, &misdirected.to_string()).unwrap();
    let malformed = json!({
        "method":"cancel", "id":"malformed", "revision":original, "session_file":"packet-selected.jsonl"
    });
    send_inbox_control(&agent, &bound, &malformed.to_string()).unwrap();
    let rejection = rpc.wait_for("malformed control receipt", |record| {
        record["type"] == "goal_control_receipt" && record["data"]["error"] == "invalid_request"
    });
    assert_eq!(rejection["data"], json!({"error":"invalid_request"}));

    let steer = Request::Steer {
        id: "steer".into(),
        revision: original,
        text: "Synthetic steering.".into(),
        lease: None,
    };
    send_inbox_control(&agent, &bound, &serde_json::to_string(&steer).unwrap()).unwrap();
    assert_eq!(
        receipt(&rpc, "steer"),
        json!({"id":"steer","error":"goal_steer_stale"})
    );

    enqueue("pause", "pause", original);
    let paused = receipt(&rpc, "pause");
    let paused_revision = paused["revision"].as_u64().unwrap();
    assert!(paused_revision > original);
    assert_eq!(
        paused,
        json!({"id":"pause","applied":true,"revision":paused_revision})
    );
    let state = rpc.call("after_pause", json!({"type":"get_state"}));
    assert_eq!(state["data"]["goalRevision"], paused_revision);
    enqueue("cancel", "stale", original);
    assert_eq!(
        receipt(&rpc, "stale"),
        json!({"id":"stale","error":"goal_revision_conflict"})
    );
    enqueue("resume", "resume", paused_revision);
    let resumed_revision = receipt(&rpc, "resume")["revision"].as_u64().unwrap();
    assert!(resumed_revision > paused_revision);
    // The resume ran the continuation prompt once; the run moved the goal revision on.
    rpc.agent_settled();
    assert_eq!(engine.requests().len(), 2);
    let state = rpc.call("after_resume", json!({"type":"get_state"}));
    let resumed_revision = state["data"]["goalRevision"].as_u64().unwrap();
    enqueue("cancel", "cancel", resumed_revision);
    let canceled_revision = receipt(&rpc, "cancel")["revision"].as_u64().unwrap();
    assert!(canceled_revision > resumed_revision);
    enqueue("resume", "after_cancel", canceled_revision);
    assert!(receipt(&rpc, "after_cancel")["error"].is_string());

    let (_, records) = rpc.finish();
    assert!(!records.iter().any(|record| record["data"]["id"] == "other"));
    assert_eq!(
        std::fs::read_dir(agent.join("inbox").join(&bound.id))
            .unwrap()
            .count(),
        1
    );
    let mut owner = Session::open(session_deps(&env, engine), Open::Resume(bound)).unwrap();
    assert_eq!(owner.goal_revision(), canceled_revision);
    assert_eq!(owner.goal().unwrap().status, GoalStatus::Canceled);
    owner.shutdown(&mut HeadlessUi::default());
}

/// A `Write` that appends to a shared buffer: the server's stdout in the tests.
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A running `serve` loop driven through its stdin pipe.
struct Rpc {
    input: Option<std::io::PipeWriter>,
    stdout: Arc<Mutex<Vec<u8>>>,
    thread: Option<JoinHandle<Result<RunOutcome, OxaError>>>,
    /// `agent_settled` records already consumed by [`Rpc::agent_settled`].
    settled: usize,
}

impl Rpc {
    fn start(session: Session, env: &TestEnv) -> Rpc {
        Self::start_with(session, env, serve_outcome)
    }

    fn start_with(
        session: Session,
        env: &TestEnv,
        serve: impl FnOnce(Session, Dirs, Box<dyn BufRead + Send>, Out) -> Result<RunOutcome, OxaError>
            + Send
            + 'static,
    ) -> Rpc {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let out: Out = Arc::new(Mutex::new(Box::new(SharedBuf(stdout.clone()))));
        let dirs = Dirs::under(env.root());
        let input: Box<dyn BufRead + Send> = Box::new(BufReader::new(reader));
        let thread = thread::spawn(move || serve(session, dirs, input, out));
        Rpc {
            input: Some(writer),
            stdout,
            thread: Some(thread),
            settled: 0,
        }
    }

    fn send_raw(&mut self, line: &str) {
        let w = self.input.as_mut().expect("stdin is open");
        writeln!(w, "{line}").expect("the server reads stdin");
        w.flush().expect("flush");
    }

    fn send(&mut self, command: Value) {
        self.send_raw(&command.to_string());
    }

    /// Every record written so far, in order.
    fn records(&self) -> Vec<Value> {
        let bytes = self.stdout.lock().unwrap().clone();
        bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("every stdout line is one JSON record"))
            .collect()
    }

    /// The first record that satisfies `pred`, waiting up to 20 seconds.
    fn wait_for(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            if let Some(r) = self.records().into_iter().find(|r| pred(r)) {
                return r;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!(
                    "timed out waiting for {what}; stdout so far: {:?}",
                    self.records()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn response_to(&self, id: &str) -> Value {
        self.wait_for(&format!("the response to {id}"), |r| {
            r["type"] == "response" && r["id"] == id
        })
    }

    /// Send a command with an id and wait for its response.
    fn call(&mut self, id: &str, mut command: Value) -> Value {
        command["id"] = Value::from(id);
        self.send(command);
        self.response_to(id)
    }

    /// The next `agent_settled`. The session is idle once it is written (the server releases
    /// every claim first), so the next command may follow at once.
    fn agent_settled(&mut self) -> Value {
        let n = self.settled;
        self.settled += 1;
        self.nth_settled(n)
    }

    /// The `n`th (0-based) `agent_settled` record, waiting up to 20 seconds.
    fn nth_settled(&self, n: usize) -> Value {
        let start = Instant::now();
        loop {
            let all: Vec<Value> = self
                .records()
                .into_iter()
                .filter(|r| r["type"] == "agent_settled")
                .collect();
            if let Some(r) = all.get(n) {
                return r.clone();
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "timed out waiting for agent_settled #{n}; stdout so far: {:?}",
                self.records()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Close stdin, wait for the server to finish its queued work, and return its exit code and
    /// every record it wrote.
    fn finish(self) -> (i32, Vec<Value>) {
        let (outcome, records) = self.finish_outcome();
        (outcome.exit_code(), records)
    }

    fn finish_outcome(mut self) -> (RunOutcome, Vec<Value>) {
        drop(self.input.take());
        let code = self
            .thread
            .take()
            .expect("the server thread")
            .join()
            .expect("the server thread does not panic")
            .expect("serve returns Ok");
        (code, self.records())
    }
}

fn has_settled(records: &[Value], aborted: bool) -> bool {
    records
        .iter()
        .any(|r| r["type"] == "agent_settled" && r["aborted"] == aborted)
}

#[cfg(target_os = "linux")]
mod live_steering {
    use super::*;
    use oxa_core::goal::SteeringLease;
    use oxa_proto::{Entry, EntryBody, HeadlessUi, NullSink, Role};
    use oxa_session::{send_inbox_control, Goal, GoalBudget, GoalStatus, SessionRef};

    const TEXT: &str = "Synthetic private steering text";

    #[derive(Default)]
    struct HeldRun {
        open: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }

    impl HeldRun {
        fn engine(&self, turns: Vec<ScriptTurn>) -> Arc<dyn Engine> {
            Arc::new(GatedEngine {
                inner: Arc::new(ScriptedEngine::new(turns)),
                open: self.open.clone(),
                entered: Some(self.entered.clone()),
                models: Vec::new(),
            })
        }

        fn start(&self, rpc: &mut Rpc) {
            let started = rpc.call("start", json!({"type":"prompt","message":"Start the goal"}));
            assert_eq!(started["success"], true);
            self.wait_until_entered();
        }

        fn wait_until_entered(&self) {
            let wait = Instant::now();
            while !self.entered.load(Ordering::SeqCst) {
                assert!(
                    wait.elapsed() < Duration::from_secs(20),
                    "engine must reach the gate"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }

        fn release(&self) {
            self.open.store(true, Ordering::SeqCst);
        }
    }

    impl Drop for HeldRun {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn owner(env: &TestEnv, engine: Arc<dyn Engine>, root: &str) -> Session {
        let mut deps = session_deps(env, engine);
        deps.store = Arc::new(FsStore::new(env.root().join(root)));
        deps.session_id = Some("fixture-steer".into());
        let mut session = Session::open(deps, Open::New).unwrap();
        let goal = Goal::new("Finish the synthetic task".into(), GoalBudget::default(), 10).unwrap();
        session.set_goal(Some(goal), &mut NullSink).unwrap();
        session
    }

    fn entries(env: &TestEnv, session: &SessionRef) -> Vec<Entry> {
        FsStore::new(env.root().join("first")).load(session).unwrap()
    }

    fn appended(entries: &[Entry]) -> Option<usize> {
        entries.iter().position(|entry| {
            matches!(&entry.body, EntryBody::Msg { wire, .. }
                if wire.role == Role::User && wire.content.as_deref() == Some(TEXT))
        })
    }

    fn send(env: &TestEnv, session: &SessionRef, packet: Value) {
        send_inbox_control(&Dirs::under(env.root()).agent_dir, session, &packet.to_string()).unwrap();
    }

    fn receipt(rpc: &Rpc, id: &str, field: &str) -> Value {
        rpc.wait_for(field, |record| {
            record["type"] == "goal_control_receipt"
                && record["data"]["id"] == id
                && (record["data"].get(field).is_some() || record["data"].get("error").is_some())
        })["data"]
            .clone()
    }

    fn submit(env: &TestEnv, rpc: &Rpc, session: &SessionRef, lease: Value) -> u64 {
        let revision = Goal::revision(&entries(env, session));
        send(
            env,
            session,
            json!({
                "method":"steer","id":"steer","revision":revision,"text":TEXT,"lease":lease
            }),
        );
        assert_eq!(
            receipt(rpc, "steer", "queued"),
            json!({"id":"steer","queued":true})
        );
        assert!(appended(&entries(env, session)).is_none());
        revision
    }

    #[test]
    fn managed_json_preserves_the_worker_header_and_delivers_only_lease_bound_input() {
        let env = TestEnv::new();
        let held = HeldRun::default();
        let session = owner(
            &env,
            held.engine(vec![ScriptTurn::text("first"), ScriptTurn::text("second")]),
            "first",
        );
        let binding = session.inner().sref.clone();
        let expected = binding.clone();
        let lease = SteeringLease {
            holder: "fixture/native".into(),
            generation: (3, 2),
        };
        let current = lease.clone();
        let rpc = Rpc::start_with(session, &env, move |mut session, dirs, _input, out| {
            oxa_rpc::run_json_outcome_with_steering_authority(
                &mut session,
                vec![("Start the goal".into(), vec![])],
                out,
                &mut Vec::new(),
                &dirs.agent_dir,
                Arc::new(move |requested, operation| {
                    if requested == &expected {
                        operation(Some(&current));
                    }
                }),
            )
        });
        held.wait_until_entered();
        assert_eq!(rpc.records()[0]["type"], "session");
        send(
            &env,
            &binding,
            json!({
                "method":"steer","id":"unleased","revision":Goal::revision(&entries(&env, &binding)),
                "text":"Synthetic unleased input"
            }),
        );
        assert_eq!(receipt(&rpc, "unleased", "queued")["error"], "goal_steer_stale");
        submit(
            &env,
            &rpc,
            &binding,
            json!({"holder":lease.holder,"generation":lease.generation}),
        );
        held.release();
        assert_eq!(receipt(&rpc, "steer", "appended")["appended"], true);
        rpc.finish();
        assert!(appended(&entries(&env, &binding)).is_some());
    }

    #[test]
    fn held_steer_appends_after_an_accounting_checkpoint() {
        let env = TestEnv::new();
        let held = HeldRun::default();
        let engine = held.engine(vec![
            ScriptTurn::tool("fixture-tool", "unavailable_tool", "{}"),
            ScriptTurn::text("first reply"),
            ScriptTurn::text("second reply"),
        ]);
        let session = owner(&env, engine, "first");
        let binding = session.inner().sref.clone();
        let mut rpc = Rpc::start(session, &env);
        held.start(&mut rpc);
        let submitted = submit(&env, &rpc, &binding, Value::Null);
        held.release();
        assert_eq!(
            receipt(&rpc, "steer", "appended"),
            json!({"id":"steer","appended":true})
        );
        rpc.agent_settled();
        let stored = entries(&env, &binding);
        let index = appended(&stored).expect("steer is stored as user input");
        assert!(
            Goal::revision(&stored[..index]) > submitted,
            "the budget checkpoint came first"
        );
        assert!(appended(&stored[index + 1..]).is_none());
        rpc.finish();
    }

    #[test]
    fn held_steer_returns_explicit_stale_for_a_replaced_objective() {
        let env = TestEnv::new();
        let held = HeldRun::default();
        let engine = held.engine(vec![ScriptTurn::text("first"), ScriptTurn::text("second")]);
        let mut session = owner(&env, engine, "first");
        let binding = session.inner().sref.clone();
        let replaced = session.goal_revision();
        let goal = Goal::new(
            "Review the replacement objective".into(),
            GoalBudget::default(),
            11,
        )
        .unwrap();
        session.set_goal(Some(goal), &mut NullSink).unwrap();
        assert!(session.goal_revision() > replaced);
        let mut rpc = Rpc::start(session, &env);
        held.start(&mut rpc);
        send(
            &env,
            &binding,
            json!({
                "method":"steer","id":"old-goal","revision":replaced,"text":TEXT
            }),
        );
        assert_eq!(
            receipt(&rpc, "old-goal", "queued"),
            json!({"id":"old-goal","queued":true})
        );
        held.release();
        assert_eq!(
            receipt(&rpc, "old-goal", "appended"),
            json!({"id":"old-goal","error":"goal_steer_stale"}),
        );
        rpc.agent_settled();
        assert!(appended(&entries(&env, &binding)).is_none());
        rpc.finish();
    }

    #[test]
    fn managed_rpc_recovers_a_saved_steer_with_its_live_lease() {
        let env = TestEnv::new();
        let held = HeldRun::default();
        let engine = held.engine(vec![ScriptTurn::text("first"), ScriptTurn::text("second")]);
        let mut session = owner(&env, engine, "first");
        let binding = session.inner().sref.clone();
        let lease = SteeringLease {
            holder: "fixture/first".into(),
            generation: (1, 1),
        };
        session
            .stage_lead_steer(
                &binding,
                session.goal_revision(),
                TEXT.into(),
                Some(lease.clone()),
                &mut NullSink,
            )
            .unwrap();
        let mut rpc = Rpc::start_with(session, &env, move |session, dirs, input, out| {
            oxa_rpc::serve_outcome_with_steering_authority(
                session,
                dirs,
                input,
                out,
                Arc::new(move |_, operation| operation(Some(&lease))),
            )
        });
        held.release();
        rpc.send(json!({"type":"prompt","id":"resume","message":"Continue."}));
        rpc.agent_settled();
        assert!(appended(&entries(&env, &binding)).is_some());
        rpc.finish();
    }

    #[test]
    fn held_steer_rejects_a_changed_assignment() {
        for change_holder in [false, true] {
            let env = TestEnv::new();
            let held = HeldRun::default();
            let engine = held.engine(vec![ScriptTurn::text("first"), ScriptTurn::text("second")]);
            let session = owner(&env, engine, "first");
            let binding = session.inner().sref.clone();
            let lease = SteeringLease {
                holder: "fixture/first".into(),
                generation: (1, 1),
            };
            let current = Arc::new(Mutex::new(lease.clone()));
            let source = current.clone();
            let mut rpc = Rpc::start_with(session, &env, move |session, dirs, input, out| {
                oxa_rpc::serve_outcome_with_steering_authority(
                    session,
                    dirs,
                    input,
                    out,
                    Arc::new(move |_, operation| operation(Some(&source.lock().unwrap()))),
                )
            });
            held.start(&mut rpc);
            submit(
                &env,
                &rpc,
                &binding,
                json!({"holder":lease.holder,"generation":lease.generation}),
            );
            if change_holder {
                current.lock().unwrap().holder = "fixture/next".into();
            } else {
                current.lock().unwrap().generation.1 += 1;
            }
            held.release();
            assert_eq!(
                receipt(&rpc, "steer", "appended"),
                json!({"id":"steer","error":"goal_steer_stale"})
            );
            rpc.agent_settled();
            assert!(appended(&entries(&env, &binding)).is_none());
            rpc.finish();
        }
    }

    #[test]
    fn held_steer_stays_with_its_exact_session_file() {
        let env = TestEnv::new();
        let held = HeldRun::default();
        let engine = held.engine(vec![
            ScriptTurn::text("first"),
            ScriptTurn::text("second"),
            ScriptTurn::text("third"),
        ]);
        let session = owner(&env, engine.clone(), "first");
        let original = session.inner().sref.clone();
        let mut other = owner(&env, engine, "second");
        let replacement = other.inner().sref.clone();
        assert_eq!(original.id, replacement.id);
        assert_ne!(original.path, replacement.path);
        assert_eq!(session.goal_revision(), other.goal_revision());
        other.shutdown(&mut HeadlessUi::default());
        drop(other);
        let mut rpc = Rpc::start(session, &env);
        let switched = rpc.call(
            "switch",
            json!({"type":"switch_session","sessionPath":replacement.path}),
        );
        assert_eq!(switched["success"], true, "{switched}");
        held.start(&mut rpc);
        send(
            &env,
            &original,
            json!({
                "method":"steer","id":"old","revision":Goal::revision(&entries(&env, &original)),
                "text":"Synthetic old session input"
            }),
        );
        submit(&env, &rpc, &replacement, Value::Null);
        held.release();
        assert_eq!(
            receipt(&rpc, "steer", "appended"),
            json!({"id":"steer","appended":true})
        );
        rpc.agent_settled();
        let (_, records) = rpc.finish();
        assert!(!records.iter().any(|record| record["data"]["id"] == "old"));
        assert!(appended(&entries(&env, &replacement)).is_some());
        assert_eq!(
            std::fs::read_dir(Dirs::under(env.root()).agent_dir.join("inbox").join(&original.id))
                .unwrap()
                .count(),
            1,
        );
    }

    #[test]
    fn held_steer_gets_stale_when_the_goal_finishes_before_delivery() {
        let env = TestEnv::new();
        let held = HeldRun::default();
        let engine = held.engine(vec![ScriptTurn::tool(
            "fixture-done",
            "goal_done",
            r#"{"status":"complete","evidence":"Verified fixture"}"#,
        )]);
        let session = owner(&env, engine, "first");
        let binding = session.inner().sref.clone();
        let mut rpc = Rpc::start(session, &env);
        held.start(&mut rpc);
        submit(&env, &rpc, &binding, Value::Null);
        held.release();
        assert_eq!(
            receipt(&rpc, "steer", "appended"),
            json!({"id":"steer","error":"goal_steer_stale"})
        );
        rpc.agent_settled();
        let stored = entries(&env, &binding);
        assert_eq!(Goal::latest(&stored).unwrap().unwrap().status, GoalStatus::Done);
        assert!(appended(&stored).is_none());
        rpc.finish();
    }

    #[test]
    fn aborted_run_cannot_replay_an_admitted_steer_after_terminal_cancel() {
        let env = TestEnv::new();
        let held = HeldRun::default();
        let engine = held.engine(vec![ScriptTurn::text("Independent reply")]);
        let session = owner(&env, engine, "first");
        let binding = session.inner().sref.clone();
        let mut rpc = Rpc::start(session, &env);
        held.start(&mut rpc);
        submit(&env, &rpc, &binding, Value::Null);
        let abort = rpc.call("abort", json!({"type":"abort"}));
        assert_eq!(abort["success"], true);
        assert_eq!(
            receipt(&rpc, "steer", "appended"),
            json!({"id":"steer","error":"goal_steer_stale"})
        );
        rpc.agent_settled();

        // Aborting the run only interrupts execution. Persist the distinct native goal cancel.
        let revision = Goal::revision(&entries(&env, &binding));
        send(
            &env,
            &binding,
            json!({"method":"cancel","id":"cancel","revision":revision}),
        );
        assert_eq!(receipt(&rpc, "cancel", "applied")["applied"], true);
        held.release();
        let fresh = rpc.call("fresh", json!({"type":"prompt","message":"Independent prompt"}));
        assert_eq!(fresh["success"], true);
        rpc.agent_settled();
        let stored = entries(&env, &binding);
        assert_eq!(
            Goal::latest(&stored).unwrap().unwrap().status,
            GoalStatus::Canceled
        );
        assert!(appended(&stored).is_none());
        rpc.finish();
    }
}

/// docs example request/response/events
#[test]
fn prompt_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Hello back")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call("req-2", json!({"type": "prompt", "message": "Review"}));
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["command"], "prompt");
    assert_eq!(resp["success"], true);
    assert_eq!(resp["id"], "req-2");
    assert_eq!(resp["data"], json!({"disposition": "started"}));

    rpc.agent_settled();
    let (code, records) = rpc.finish();
    assert_eq!(code, 0);

    let response_at = records.iter().position(|r| r["type"] == "response").unwrap();
    let start_at = records
        .iter()
        .position(|r| r["type"] == "agent_start")
        .expect("agent_start");
    assert!(
        response_at < start_at,
        "the response comes before the run's events"
    );

    let reply = records
        .iter()
        .find(|r| r["type"] == "message_end" && r["message"]["role"] == "assistant")
        .expect("the assistant message_end");
    assert_eq!(
        reply["message"]["content"][0],
        json!({"type": "text", "text": "Hello back"})
    );
    assert_eq!(reply["message"]["stopReason"], "stop");
    assert!(has_settled(&records, false), "agent_settled with aborted false");
}

/// during a run
#[test]
fn steer_and_follow_up_fixtures() {
    let env = TestEnv::new();
    let open = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("first"),
            ScriptTurn::text("second"),
        ])),
        open: open.clone(),
        models: Vec::new(),
        entered: None,
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "start"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");

    let steer = rpc.call("s", json!({"type": "steer", "message": "change course"}));
    assert_eq!(steer["success"], true);
    assert_eq!(steer["command"], "steer");
    assert_eq!(steer["data"], json!({"disposition": "queued"}));

    let follow = rpc.call("f", json!({"type": "follow_up", "message": "then this"}));
    assert_eq!(follow["success"], true);
    assert_eq!(follow["data"], json!({"disposition": "queued"}));

    // The run is held open: get_state answers during it, with the queued messages counted.
    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["isStreaming"], true);
    assert_eq!(state["data"]["pendingMessageCount"], 2);

    open.store(true, Ordering::SeqCst);
    rpc.agent_settled();

    let messages = rpc.call("m", json!({"type": "get_messages"}));
    let texts: Vec<&Value> = messages["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| &m["content"])
        .collect();
    assert!(texts.contains(&&json!("start")));
    assert!(
        texts.contains(&&json!("then this")),
        "the follow-up is delivered after the run: {texts:?}"
    );

    let (code, _) = rpc.finish();
    assert_eq!(code, 1);
}

/// agent_settled aborted
#[test]
fn abort_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(Vec::new())),
        open: Arc::new(AtomicBool::new(false)),
        models: Vec::new(),
        entered: None,
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "long task"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");

    let resp = rpc.call("a", json!({"type": "abort"}));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["command"], "abort");

    // The abort response comes after the run has settled.
    let records = rpc.records();
    let settled_at = records.iter().position(|r| r["type"] == "agent_settled");
    let response_at = records
        .iter()
        .position(|r| r["type"] == "response" && r["id"] == "a");
    assert!(
        settled_at.is_some() && response_at.is_some(),
        "both records are written"
    );
    assert!(
        settled_at < response_at,
        "agent_settled comes before the abort response"
    );

    let (code, records) = rpc.finish();
    assert_eq!(code, 130);
    assert!(has_settled(&records, true), "agent_settled with aborted true");
}

/// returns queued
#[test]
fn clear_queue_fixture() {
    let env = TestEnv::new();
    let open = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("done")])),
        open: open.clone(),
        models: Vec::new(),
        entered: None,
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "work"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");
    rpc.call("s", json!({"type": "steer", "message": "Change direction"}));
    rpc.call(
        "f",
        json!({"type": "follow_up", "message": "Summarize when finished"}),
    );

    let cleared = rpc.call("c", json!({"type": "clear_queue"}));
    assert_eq!(cleared["success"], true);
    assert_eq!(
        cleared["data"],
        json!({"steering": ["Change direction"], "followUp": ["Summarize when finished"]})
    );

    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["pendingMessageCount"], 0);

    open.store(true, Ordering::SeqCst);
    rpc.agent_settled();
    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// state changes
#[test]
fn new_session_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let before = rpc.call("g1", json!({"type": "get_state"}));
    let before_id = before["data"]["sessionId"].as_str().unwrap().to_string();

    let resp = rpc.call("n", json!({"type": "new_session"}));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"], json!({"cancelled": false}));

    let after = rpc.call("g2", json!({"type": "get_state"}));
    assert_ne!(
        after["data"]["sessionId"].as_str().unwrap(),
        before_id,
        "a new session id"
    );
    assert_eq!(after["data"]["messageCount"], 0);

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// RPC `new_session` ends running background jobs: the cancel fires, the id is forgotten.
#[test]
fn new_session_ends_background_jobs() {
    struct Block(Arc<AtomicBool>);
    impl Sandbox for Block {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _s: &ExecSpec, c: &CancelToken, _l: &mut dyn FnMut(&[u8])) -> ExecResult {
            while !c.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            self.0.store(true, Ordering::SeqCst);
            ExecResult::default()
        }
    }
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let jobs = oxa_core::fresh_jobs();
    let ended = Arc::new(AtomicBool::new(false));
    jobs.set_sandbox(Arc::new(Block(ended.clone())));
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.agent_dir().join("sessions")));
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: jobs.clone(),
    };
    let session = Session::open(deps, Open::New).expect("open");
    let id = jobs
        .spawn(ExecSpec {
            command: "block".into(),
            cwd: env.work(),
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        })
        .unwrap();
    assert_eq!(jobs.running(), 1);
    let mut rpc = Rpc::start(session, &env);
    let resp = rpc.call("n", json!({"type": "new_session"}));
    assert_eq!(resp["success"], true);
    assert_eq!(jobs.running(), 0);
    assert_eq!(jobs.read(&id).unwrap_err(), format!("Unknown job: {id}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ended.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the job's cancel never fired");
        thread::sleep(Duration::from_millis(5));
    }
    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// fields
#[test]
fn get_state_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(resp["success"], true);
    let data = resp["data"].as_object().expect("the state is an object");
    for key in [
        "model",
        "thinkingLevel",
        "isStreaming",
        "isCompacting",
        "steeringMode",
        "followUpMode",
        "sessionFile",
        "sessionId",
        "autoCompactionEnabled",
        "autoRetryEnabled",
        "messageCount",
        "pendingMessageCount",
        "mode",
    ] {
        assert!(data.contains_key(key), "get_state has {key}");
    }
    assert_eq!(data["model"]["id"], "fake-model");
    // pi settings.md: `defaultThinkingLevel` defaults to `medium`.
    assert_eq!(data["thinkingLevel"], "medium");
    assert_eq!(data["isStreaming"], false);
    // pi's default for both queue modes (rpc-commands.md: "one-at-a-time (default)").
    assert_eq!(data["steeringMode"], "one-at-a-time");
    assert_eq!(data["followUpMode"], "one-at-a-time");
    assert_eq!(data["autoCompactionEnabled"], true);
    assert_eq!(data["mode"], "workspace");

    rpc.finish();
}

/// message shapes
#[test]
fn get_messages_fixture() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("hi there")]));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p", json!({"type": "prompt", "message": "hello"}));
    rpc.agent_settled();

    let resp = rpc.call("m", json!({"type": "get_messages"}));
    let messages = resp["data"]["messages"].as_array().expect("a message list");
    assert_eq!(messages.len(), 2);

    let user = &messages[0];
    assert_eq!(user["role"], "user");
    assert_eq!(user["content"], "hello");
    assert!(user["timestamp"].is_u64());

    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(
        assistant["content"],
        json!([{"type": "text", "text": "hi there"}])
    );
    assert_eq!(assistant["stopReason"], "stop");
    assert_eq!(assistant["provider"], "oxe");
    assert_eq!(assistant["api"], "openai-completions");
    assert_eq!(assistant["model"], "fake-model");
    assert!(assistant["usage"].is_object());

    let (code, _) = rpc.finish();
    assert_eq!(code, 0);
}

/// set/cycle/get_available
#[test]
fn model_commands_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(Vec::new())),
        open: Arc::new(AtomicBool::new(true)),
        models: vec!["fake-model".to_string(), "second-model".to_string()],
        entered: None,
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let available = rpc.call("a", json!({"type": "get_available_models"}));
    let ids: Vec<&str> = available["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["fake-model", "second-model"]);

    let set = rpc.call(
        "s",
        json!({"type": "set_model", "provider": "oxe", "modelId": "second-model"}),
    );
    assert_eq!(set["success"], true);
    assert_eq!(set["data"]["id"], "second-model");
    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["model"]["id"], "second-model");

    let cycled = rpc.call("c", json!({"type": "cycle_model"}));
    assert_eq!(cycled["data"]["model"]["id"], "fake-model");
    assert_eq!(cycled["data"]["isScoped"], false);

    let unknown = rpc.call("u", json!({"type": "set_model", "modelId": "no-such-model"}));
    assert_eq!(unknown["success"], false);
    assert!(unknown["error"].as_str().unwrap().contains("Model not found"));

    let (code, _) = rpc.finish();
    assert_eq!(code, 1);
}

/// set/cycle/levels
#[test]
fn thinking_commands_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let set = rpc.call("s", json!({"type": "set_thinking_level", "level": "high"}));
    assert_eq!(set["success"], true);
    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["thinkingLevel"], "high");

    let cycled = rpc.call("c", json!({"type": "cycle_thinking_level"}));
    assert_eq!(cycled["data"], json!({"level": "off"}), "high wraps to off");

    let levels = rpc.call("l", json!({"type": "get_available_thinking_levels"}));
    assert_eq!(
        levels["data"]["levels"],
        json!(["off", "minimal", "low", "medium", "high"])
    );

    let bad = rpc.call("b", json!({"type": "set_thinking_level", "level": "ludicrous"}));
    assert_eq!(bad["success"], false);

    rpc.finish();
}

/// steering/follow-up modes
#[test]
fn queue_mode_commands_fixtures() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let steering = rpc.call("s", json!({"type": "set_steering_mode", "mode": "all"}));
    assert_eq!(steering["success"], true);
    let follow = rpc.call("f", json!({"type": "set_follow_up_mode", "mode": "all"}));
    assert_eq!(follow["success"], true);

    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["steeringMode"], "all");
    assert_eq!(state["data"]["followUpMode"], "all");

    let bad = rpc.call("b", json!({"type": "set_follow_up_mode", "mode": "sometimes"}));
    assert_eq!(bad["success"], false);

    rpc.finish();
}

/// oxa addition
#[test]
fn set_mode_command() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let resp = rpc.call("m", json!({"type": "set_mode", "mode": "plan"}));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["command"], "set_mode");

    let state = rpc.call("g", json!({"type": "get_state"}));
    assert_eq!(state["data"]["mode"], "plan");

    let (code, records) = {
        let changed = rpc.wait_for("mode_changed", |r| r["type"] == "mode_changed");
        assert_eq!(changed["mode"], "plan");
        rpc.finish()
    };
    assert_eq!(code, 0);
    assert!(records
        .iter()
        .any(|r| r["type"] == "mode_changed" && r["mode"] == "plan"));

    let bad_env = TestEnv::new();
    let mut rpc = Rpc::start(
        open_session(&bad_env, Arc::new(ScriptedEngine::new(Vec::new()))),
        &bad_env,
    );
    let bad = rpc.call("b", json!({"type": "set_mode", "mode": "yolo"}));
    assert_eq!(bad["success"], false);
    rpc.finish();
}

/// bad line
#[test]
fn parse_error_response_keeps_running() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.send_raw("this is not json");
    let parse = rpc.wait_for("the parse error", |r| r["command"] == "parse");
    assert_eq!(parse["type"], "response");
    assert_eq!(parse["success"], false);
    assert!(parse["error"]
        .as_str()
        .unwrap()
        .starts_with("Failed to parse command"));
    assert!(parse.get("id").is_none(), "a line with no command has no id");

    let after = rpc.call("after-bad", json!({"type": "get_state"}));
    assert_eq!(
        after["success"], true,
        "the server keeps running after a bad line"
    );

    let (code, _) = rpc.finish();
    assert_eq!(
        code, 1,
        "reader-side rejection is retained in the terminal outcome"
    );
}

/// correlation id
#[test]
fn id_echoed() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    let with_id = rpc.call("abc-7", json!({"type": "get_state"}));
    assert_eq!(with_id["id"], "abc-7");
    assert_eq!(with_id["command"], "get_state");

    rpc.send(json!({"type": "get_state"}));
    let without = rpc.wait_for("a response without id", |r| {
        r["type"] == "response" && r.get("id").is_none() && r["command"] == "get_state"
    });
    assert_eq!(without["success"], true);

    rpc.finish();
}

/// A prompt during a run needs streamingBehavior; with it the prompt is queued.
#[test]
fn prompt_while_busy_needs_streaming_behavior() {
    let env = TestEnv::new();
    let open = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(GatedEngine {
        inner: Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
        ])),
        open: open.clone(),
        models: Vec::new(),
        entered: None,
    });
    let mut rpc = Rpc::start(open_session(&env, engine), &env);

    rpc.call("p1", json!({"type": "prompt", "message": "first"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");

    let refused = rpc.call("p2", json!({"type": "prompt", "message": "second"}));
    assert_eq!(refused["success"], false);

    let queued = rpc.call(
        "p3",
        json!({"type": "prompt", "message": "third", "streamingBehavior": "followUp"}),
    );
    assert_eq!(queued["success"], true);
    assert_eq!(queued["data"], json!({"disposition": "queued"}));

    // A session command during a run is refused as busy.
    let busy = rpc.call("st", json!({"type": "set_auto_retry", "enabled": false}));
    assert_eq!(busy["success"], false);
    assert!(busy["error"].as_str().unwrap().contains("busy"));

    open.store(true, Ordering::SeqCst);
    rpc.agent_settled();
    // Idle again: the same command is answered.
    let idle = rpc.call("st2", json!({"type": "set_auto_retry", "enabled": false}));
    assert_eq!(idle["success"], true);
    let (code, _) = rpc.finish();
    assert_eq!(
        code, 1,
        "reader-side rejection is retained in the terminal outcome"
    );
}

#[test]
fn wrap_up_retains_queued_errors_without_running_later_commands() {
    for reason in [oxa_core::WrapUpReason::TimeLimit, oxa_core::WrapUpReason::Signal] {
        for fail_prompt in [false, true] {
            let env = TestEnv::new();
            let open = Arc::new(AtomicBool::new(false));
            let entered = Arc::new(AtomicBool::new(false));
            let first = if fail_prompt {
                ScriptTurn::fault(oxa_fake::Fault::Refuse)
            } else {
                ScriptTurn::text("initial")
            };
            let engine = Arc::new(GatedEngine {
                inner: Arc::new(ScriptedEngine::new(vec![
                    first,
                    ScriptTurn::text("final summary"),
                ])),
                open: open.clone(),
                models: Vec::new(),
                entered: Some(entered.clone()),
            });
            let session = open_session(&env, engine);
            let handle = session.handle();
            let mut rpc = Rpc::start(session, &env);
            assert_eq!(
                rpc.call("retry", json!({"type":"set_auto_retry","enabled":false}))["success"],
                true
            );
            assert_eq!(
                rpc.call("p", json!({"type":"prompt","message":"go"}))["success"],
                true
            );
            let wait = Instant::now();
            while !entered.load(Ordering::SeqCst) {
                assert!(
                    wait.elapsed() < Duration::from_secs(20),
                    "the request reaches the held engine"
                );
                thread::sleep(Duration::from_millis(2));
            }

            // A deferred session command sits ahead of the reader-side failure.
            // Neither a soft stop nor draining its outcomes may execute that command.
            rpc.send(json!({"id":"late","type":"prompt","message":"/name LATE-COMMAND"}));
            let rejected = rpc.call("bad", json!({"type":"follow_up"}));
            assert_eq!(rejected["success"], false);
            handle.request_wrap_up(reason);
            let input = rpc.input.take(); // stdin stays open through the terminal result
            open.store(true, Ordering::SeqCst);
            let (outcome, records) = rpc.finish_outcome();
            drop(input);

            let (expected, expected_code) = if fail_prompt {
                (RunOutcome::Failed(codes::ENGINE_DOWN), 2)
            } else {
                (RunOutcome::Failed(codes::INVALID), 1)
            };
            assert_eq!(outcome, expected, "{reason:?}/fail_prompt={fail_prompt}");
            assert_eq!(outcome.exit_code(), expected_code);
            assert!(!records
                .iter()
                .any(|record| record["type"] == "response" && record["id"] == "late"));
            assert!(records.iter().any(|record| record["type"] == "message_end"
                && record["message"]["content"][0]["text"] == "final summary"));
        }
    }
}

#[test]
fn a_wrap_up_while_the_session_thread_waits_is_served_before_the_tick() {
    // O-386: the session thread waits up to RPC_IDLE_TICK (250 ms) for a job. A wrap-up
    // requested while it waits is served at once by the session's attention waker, not at the
    // tick: the server returns without any job being sent and without stdin closing. Before
    // O-386 the tick was 20 ms, so this is the regression guard for the wake, not a
    // failing-before test.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let session = open_session(&env, engine);
    let handle = session.handle();
    let mut rpc = Rpc::start(session, &env);
    // No command is sent: the session thread is in its wait.
    let start = Instant::now();
    handle.request_wrap_up(oxa_core::WrapUpReason::Signal);
    let outcome = rpc
        .thread
        .take()
        .expect("the server thread")
        .join()
        .expect("the server thread does not panic")
        .expect("serve returns Ok");
    let waited = start.elapsed();
    assert_eq!(outcome, RunOutcome::Interrupted);
    assert!(
        waited < Duration::from_millis(100),
        "the wrap-up is served at once, not at the tick: {waited:?}"
    );
}

#[test]
fn stdin_close_drains_work_and_keeps_the_late_commands_outcome() {
    for (kind, accepted, expected_code) in [("get_state", true, 0), ("get_commands", false, 1)] {
        let env = TestEnv::new();
        let open = Arc::new(AtomicBool::new(false));
        let engine = Arc::new(GatedEngine {
            inner: Arc::new(ScriptedEngine::new(vec![ScriptTurn::text(
                "completed before exit",
            )])),
            open: open.clone(),
            models: Vec::new(),
            entered: None,
        });
        let mut rpc = Rpc::start(open_session(&env, engine), &env);
        let prompt = rpc.call("p", json!({"type": "prompt", "message": "finish this"}));
        assert_eq!(prompt["success"], true);
        rpc.wait_for("the held run", |record| record["type"] == "agent_start");

        let late = rpc.call("late", json!({"type": kind}));
        assert_eq!(late["success"], accepted, "{late}");
        if accepted {
            assert_eq!(late["data"]["isStreaming"], true);
        } else {
            assert!(late["error"].as_str().unwrap().contains("busy"));
        }
        // EOF precedes release of the accepted prompt. It must finish even when a
        // later command was rejected, and that rejection must retain its exit code.
        drop(rpc.input.take());
        open.store(true, Ordering::SeqCst);
        let (code, records) = rpc.finish();
        assert_eq!(code, expected_code, "{kind}: {records:?}");
        assert!(has_settled(&records, false));
        assert!(records.iter().any(|record| record["type"] == "message_end"
            && record["message"]["content"][0]["text"] == "completed before exit"));
    }
}

/// Unit check on the helpers the server exposes: the error envelope keeps the id.
#[test]
fn response_envelopes_keep_id() {
    let cmd: RpcCommand = parse_command(r#"{"id":"x-1","type":"abort_retry"}"#).unwrap();
    let ok = response_ok(&cmd, Value::Null);
    assert_eq!(
        ok,
        json!({"type": "response", "id": "x-1", "command": "abort_retry", "success": true})
    );
    let err = response_err(&cmd, &OxaError::new(codes::BUSY, "busy"));
    assert_eq!(err["success"], false);
    assert_eq!(err["error"], "busy");
    assert_eq!(err["id"], "x-1");
}

#[test]
fn inbox_during_a_turn_is_a_follow_up_not_an_interrupt() {
    let env = TestEnv::new();
    let open = Arc::new(AtomicBool::new(false));
    let inner = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("first"),
        ScriptTurn::text("second"),
    ]));
    let engine = Arc::new(GatedEngine {
        inner: inner.clone(),
        open: open.clone(),
        models: Vec::new(),
        entered: None,
    });
    let session = open_session(&env, engine);
    let id = session.state().session_id;
    let handle = session.handle();
    let mut rpc = Rpc::start(session, &env);
    rpc.call("p", json!({"type": "prompt", "message": "start"}));
    rpc.wait_for("agent_start", |r| r["type"] == "agent_start");
    oxa_session::send_inbox(&Dirs::under(env.root()).agent_dir, &id, "then this").unwrap();
    let start = std::time::Instant::now();
    while handle.queued().1.is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "inbox did not queue during the run"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        inner.requests().is_empty(),
        "the active request remains held, not cancelled"
    );
    open.store(true, Ordering::SeqCst);
    rpc.agent_settled();
    let (code, records) = rpc.finish();
    assert_eq!(code, 0);
    assert!(has_settled(&records, false));
    let requests = inner.requests();
    assert_eq!(requests.iter().filter(|r| r.max_tokens > 0).count(), 2);
    assert!(requests
        .last()
        .unwrap()
        .messages
        .iter()
        .any(|m| m.content.as_deref() == Some("then this")));
}
