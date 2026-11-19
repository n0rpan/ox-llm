//! R1 (wave H): resume after a crash mid-tool. A full session runs one tool loop
//! (two parallel reads), then a second prompt; its file is cut at each of five points, the
//! session is resumed and a prompt sent. Asserted: the request messages exactly (the request
//! golden of this test: the system prompt carries the temp cwd, so the messages are compared
//! byte for byte instead of a hash), and that every byte already on disk is still there.

use oxa_config::{Dirs, Settings};
use oxa_core::{goal::GoalControl, prompt, Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{codes, EntryBody, Finish, HeadlessUi, OxaError, Role, ToolCall, VecSink, WireMsg};
use oxa_session::{FsStore, Goal, GoalBudget, GoalStatus, SessionRef};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn deps(env: &TestEnv, engine: Arc<ScriptedEngine>) -> Deps {
    Deps {
        engine,
        store: Arc::new(FsStore::new(env.root().join("sessions"))),
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(oxa_perm::ModePolicy::new()),
        sandbox: Arc::new(oxa_sandbox::NoSandbox),
        // Exact prompt-byte fixtures select note-free queues/RPC/replay.
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: oxa_resources::Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    }
}

fn two_reads() -> ScriptTurn {
    ScriptTurn {
        tool_calls: vec![
            ToolCall {
                id: "c1".into(),
                name: "read".into(),
                args_raw: r#"{"path":"a.txt"}"#.into(),
            },
            ToolCall {
                id: "c2".into(),
                name: "read".into(),
                args_raw: r#"{"path":"b.txt"}"#.into(),
            },
        ],
        finish: Finish::ToolCalls,
        text: None,
        ..ScriptTurn::text("")
    }
}

/// The complete session: prompt "go" (two reads, then "done"), prompt "next" ("ok").
fn full_session(env: &TestEnv) -> SessionRef {
    std::fs::write(env.work().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(env.work().join("b.txt"), "beta\n").unwrap();
    let eng = Arc::new(ScriptedEngine::new(vec![
        two_reads(),
        ScriptTurn::text("done"),
        ScriptTurn::text("ok"),
    ]));
    let mut s = Session::open(deps(env, eng), Open::New).unwrap();
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("go".into(), Vec::new(), &mut sink, &mut ui).unwrap();
    s.prompt("next".into(), Vec::new(), &mut sink, &mut ui).unwrap();
    let st = s.inner();
    st.sref.clone()
}

/// Byte offset just after the first line matching `pred`.
fn after_line(bytes: &[u8], pred: impl Fn(&str) -> bool) -> usize {
    let mut at = 0;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        at += line.len();
        if pred(std::str::from_utf8(line).unwrap()) {
            return at;
        }
    }
    panic!("no such line");
}

fn is_assistant_with_calls(l: &str) -> bool {
    l.contains("\"role\":\"assistant\"") && l.contains("\"tool_calls\"")
}
fn is_result(id: &str) -> impl Fn(&str) -> bool + '_ {
    move |l: &str| l.contains("\"role\":\"toolResult\"") && l.contains(&format!("\"toolCallId\":\"{id}\""))
}
fn is_user(text: &str) -> impl Fn(&str) -> bool + '_ {
    move |l: &str| l.contains("\"role\":\"user\"") && l.contains(&format!("\"content\":\"{text}\""))
}

/// Cut the file to `len` bytes, resume, prompt "continue"; returns the messages of the request
/// and checks that the cut bytes are still the start of the file.
fn resume_after_cut(env: &TestEnv, sref: &SessionRef, len: usize) -> Vec<WireMsg> {
    let path: &Path = &sref.path;
    let all = std::fs::read(path).unwrap();
    let kept = all[..len].to_vec();
    std::fs::write(path, &kept).unwrap();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("resumed")]));
    let mut s = Session::open(deps(env, eng.clone()), Open::Resume(sref.clone())).unwrap();
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("continue".into(), Vec::new(), &mut sink, &mut ui)
        .unwrap();
    let now = std::fs::read(path).unwrap();
    assert!(now.starts_with(&kept), "nothing already on disk was rewritten");
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 1);
    // The resumed request is well-formed: it ends with the new user message, never with the
    // interrupted assistant message.
    assert_eq!(
        reqs[0].messages.last().map(|m| m.role),
        Some(oxa_proto::Role::User)
    );
    reqs[0].messages.clone()
}

fn interrupted(id: &str) -> WireMsg {
    WireMsg::tool_result(id, prompt::TOOL_INTERRUPTED)
}

/// The two read results as the read tool returns them.
fn real(id: &str, text: &str) -> WireMsg {
    WireMsg::tool_result(id, text)
}

fn calls_msg() -> WireMsg {
    two_reads().message()
}

fn check(case: &str, cut: impl Fn(&[u8]) -> usize, expected: Vec<WireMsg>) {
    let env = TestEnv::new();
    let sref = full_session(&env);
    let bytes = std::fs::read(&sref.path).unwrap();
    let got = resume_after_cut(&env, &sref, cut(&bytes));
    assert_eq!(got, expected, "{case}");
    // Every call is answered and results directly follow their assistant message.
    let at = got.iter().position(|m| !m.tool_calls.is_empty()).unwrap();
    let ids: Vec<_> = got[at + 1..at + 3]
        .iter()
        .map(|m| m.tool_call_id.clone().unwrap())
        .collect();
    assert_eq!(ids, ["c1", "c2"], "{case}");
    assert!(got[at + 1..at + 3].iter().all(|m| m.role == Role::Tool));
}

#[test]
fn cut_after_the_assistant_message() {
    check(
        "after the assistant message",
        |b| after_line(b, is_assistant_with_calls),
        vec![
            WireMsg::user("go"),
            calls_msg(),
            interrupted("c1"),
            interrupted("c2"),
            WireMsg::user("continue"),
        ],
    );
}

#[test]
fn cut_between_two_results_of_a_parallel_batch() {
    check(
        "between the results",
        |b| after_line(b, is_result("c1")),
        vec![
            WireMsg::user("go"),
            calls_msg(),
            real("c1", "alpha\n"),
            interrupted("c2"),
            WireMsg::user("continue"),
        ],
    );
}

#[test]
fn cut_mid_line_torn_write() {
    check(
        "torn second result",
        |b| after_line(b, is_result("c2")) - 7,
        vec![
            WireMsg::user("go"),
            calls_msg(),
            real("c1", "alpha\n"),
            interrupted("c2"),
            WireMsg::user("continue"),
        ],
    );
}

#[test]
fn cut_after_all_results() {
    check(
        "after all results",
        |b| after_line(b, is_result("c2")),
        vec![
            WireMsg::user("go"),
            calls_msg(),
            real("c1", "alpha\n"),
            real("c2", "beta\n"),
            WireMsg::user("continue"),
        ],
    );
}

#[test]
fn cut_after_the_next_user_message() {
    check(
        "after the next user message",
        |b| after_line(b, is_user("next")),
        vec![
            WireMsg::user("go"),
            calls_msg(),
            real("c1", "alpha\n"),
            real("c2", "beta\n"),
            WireMsg::assistant("done"),
            WireMsg::user("next"),
            WireMsg::user("continue"),
        ],
    );
}

#[test]
fn a_second_resume_appends_nothing_more() {
    let env = TestEnv::new();
    let sref = full_session(&env);
    let bytes = std::fs::read(&sref.path).unwrap();
    let cut = after_line(&bytes, is_assistant_with_calls);
    std::fs::write(&sref.path, &bytes[..cut]).unwrap();
    let eng = Arc::new(ScriptedEngine::new(Vec::new()));
    drop(Session::open(deps(&env, eng.clone()), Open::Resume(sref.clone())).unwrap());
    let once = std::fs::read(&sref.path).unwrap();
    drop(Session::open(deps(&env, eng), Open::Resume(sref.clone())).unwrap());
    assert_eq!(std::fs::read(&sref.path).unwrap(), once);
}

/// Counts sync calls and can inject failures at the durability and prefix boundaries.
#[derive(Debug, Default)]
struct CountingStore {
    inner: oxa_session::MemStore,
    syncs: AtomicUsize,
    goal_revision_at_sync: AtomicU64,
    confirmed_goal_revision: AtomicU64,
    fail_sync: AtomicBool,
    fail_prefix: AtomicBool,
}

impl oxa_session::SessionStore for CountingStore {
    fn create(
        &self,
        cwd: &Path,
        id: Option<&str>,
        parent: Option<&Path>,
    ) -> Result<SessionRef, oxa_proto::OxaError> {
        self.inner.create(cwd, id, parent)
    }
    fn append(&self, s: &SessionRef, e: &oxa_proto::Entry) -> Result<(), oxa_proto::OxaError> {
        if self.fail_prefix.load(Ordering::SeqCst) && matches!(e.body, EntryBody::Prefix(_)) {
            return Err(OxaError::new(codes::IO, "synthetic prefix append failure"));
        }
        self.inner.append(s, e)
    }
    fn load(&self, s: &SessionRef) -> Result<Vec<oxa_proto::Entry>, oxa_proto::OxaError> {
        self.inner.load(s)
    }
    fn header(&self, s: &SessionRef) -> Result<oxa_session::SessionHeader, oxa_proto::OxaError> {
        self.inner.header(s)
    }
    fn list(&self, cwd: Option<&Path>) -> Vec<oxa_session::SessionMeta> {
        self.inner.list(cwd)
    }
    fn resolve(&self, id_or_path: &str) -> Result<SessionRef, oxa_proto::OxaError> {
        self.inner.resolve(id_or_path)
    }
    fn delete(&self, s: &SessionRef) -> Result<(), oxa_proto::OxaError> {
        self.inner.delete(s)
    }
    fn sync(&self, s: &SessionRef) -> Result<(), oxa_proto::OxaError> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
        let revision = Goal::revision(&self.inner.load(s)?);
        self.goal_revision_at_sync.store(revision, Ordering::SeqCst);
        if self.fail_sync.load(Ordering::SeqCst) {
            return Err(OxaError::new(codes::IO, "synthetic sync failure"));
        }
        self.confirmed_goal_revision.store(revision, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn the_session_file_is_synced_once_per_turn_and_fs_sync_works() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(env.work().join("b.txt"), "beta\n").unwrap();
    let store = Arc::new(CountingStore::default());
    let eng = Arc::new(ScriptedEngine::new(vec![two_reads(), ScriptTurn::text("done")]));
    let mut d = deps(&env, eng);
    d.store = store.clone();
    let mut s = Session::open(d, Open::New).unwrap();
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("go".into(), Vec::new(), &mut sink, &mut ui).unwrap();
    // Two turns (the tool batch, the final reply): two syncs, not one per entry.
    assert_eq!(store.syncs.load(Ordering::SeqCst), 2);
    let sref = full_session(&env);
    let fs = FsStore::new(env.root().join("sessions"));
    oxa_session::SessionStore::sync(&fs, &sref).unwrap();
}

#[derive(Debug)]
struct GoalCommits {
    store: Arc<CountingStore>,
    seen: Mutex<Vec<(SessionRef, u64, u64)>>,
    statuses: Mutex<Vec<String>>,
}

impl oxa_core::ProgressObserver for GoalCommits {
    fn notify(&self, session: &SessionRef, progress: oxa_core::Progress<'_>) {
        if let oxa_core::Progress::GoalControlCommitted { revision } = progress {
            let confirmed = self.store.confirmed_goal_revision.load(Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap()
                .push((session.clone(), revision, confirmed));
        }
        if let oxa_core::Progress::GoalStatus { status, .. } = progress {
            self.statuses.lock().unwrap().push(status.into());
        }
    }
}

#[test]
fn goal_control_syncs_before_ack_and_distinguishes_partial_failures() {
    for (fail_sync, fail_prefix, expected_error) in [
        (false, false, None),
        (true, false, Some("goal_sync_failed")),
        (false, true, Some("goal_applied_prefix_failed")),
    ] {
        let env = TestEnv::new();
        let store = Arc::new(CountingStore::default());
        let mut deps = deps(&env, Arc::new(ScriptedEngine::new(vec![])));
        deps.store = store.clone();
        let mut session = Session::open(deps, Open::New).unwrap();
        let mut sink = VecSink::default();
        let goal = Goal::new("Synthetic objective".into(), GoalBudget::default(), 1).unwrap();
        session.set_goal(Some(goal), &mut sink).unwrap();
        let bound = session.inner().sref.clone();
        let before = session.goal_revision();
        let commits = Arc::new(GoalCommits {
            store: store.clone(),
            seen: Mutex::new(Vec::new()),
            statuses: Mutex::new(Vec::new()),
        });
        session.set_progress_observer(commits.clone());
        commits.statuses.lock().unwrap().clear();
        store.fail_sync.store(fail_sync, Ordering::SeqCst);
        store.fail_prefix.store(fail_prefix, Ordering::SeqCst);

        let receipt = session.control_goal(&bound, before, GoalControl::Cancel, &mut sink);
        let after = session.goal_revision();
        assert!(after > before);
        assert_eq!(session.goal().unwrap().status, GoalStatus::Canceled);
        assert_eq!(store.syncs.load(Ordering::SeqCst), 1);
        assert_eq!(store.goal_revision_at_sync.load(Ordering::SeqCst), after);
        match expected_error {
            Some(code) => assert_eq!(receipt.unwrap_err().code, code),
            None => assert_eq!(receipt.unwrap(), after),
        }
        let retry = session.control_goal(&bound, before, GoalControl::Cancel, &mut sink);
        assert_eq!(retry.unwrap_err().code, "goal_revision_conflict");
        assert_eq!(session.goal_revision(), after);
        assert_eq!(store.syncs.load(Ordering::SeqCst), 1);
        let expected = if fail_sync {
            vec![]
        } else {
            vec![(bound, after, after)]
        };
        assert_eq!(*commits.seen.lock().unwrap(), expected);
        let expected_statuses: Vec<String> = if fail_sync {
            vec![]
        } else {
            vec!["canceled".into()]
        };
        assert_eq!(*commits.statuses.lock().unwrap(), expected_statuses);
        session.shutdown(&mut HeadlessUi::default());
    }
}
