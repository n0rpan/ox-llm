//! C1 acceptance tests: Session object, queues, Handle.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{Deps, Open, Queue, Session, Shared, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, AgentEvent, CommandSpec, Decision, EntryBody, HeadlessUi, HookPoint, HookReply, Hooks, Mode,
    OxaError, Part, ThinkingLevel, Tool, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Deps over a scripted engine with no turns, the given store and the test's own directories.
fn deps(env: &TestEnv, store: Arc<dyn SessionStore>) -> Deps {
    deps_with(env, store, Arc::new(ScriptedEngine::new(Vec::new())), Vec::new())
}

fn deps_with(
    env: &TestEnv,
    store: Arc<dyn SessionStore>,
    engine: Arc<ScriptedEngine>,
    hooks: Vec<Arc<dyn Hooks>>,
) -> Deps {
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        // Exact prompt-byte fixtures select note-free queues/RPC/replay.
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    }
}

fn settled_count(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AgentSettled { .. }))
        .count()
}

/// steering after the tool batch, follow-up at stop
#[test]
fn steering_vs_follow_up_order() {
    let sh = Shared::new(QueueMode::All, QueueMode::All);
    {
        let mut q = sh.queue.lock().unwrap();
        q.steering.push("s1".to_string());
        q.follow_up.push("f1".to_string());
    }
    // The tool batch ends: only steering is delivered, the follow-up waits for the stop.
    assert_eq!(sh.take_steering(), vec!["s1"]);
    assert!(sh.take_steering().is_empty());
    assert_eq!(sh.queue.lock().unwrap().follow_up, vec!["f1".to_string()]);
    // The agent would stop: the follow-up is delivered.
    assert_eq!(sh.take_follow_up(), vec!["f1"]);
}

/// all queued messages delivered at once
#[test]
fn mode_all() {
    let sh = Shared::new(QueueMode::All, QueueMode::All);
    sh.queue.lock().unwrap().follow_up = vec!["a".into(), "b".into(), "c".into()];
    assert_eq!(sh.take_follow_up(), vec!["a", "b", "c"]);
    assert!(sh.take_follow_up().is_empty());
}

/// one per turn
#[test]
fn mode_one_at_a_time() {
    let sh = Shared::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
    sh.queue.lock().unwrap().follow_up = vec!["a".into(), "b".into()];
    assert_eq!(sh.take_follow_up(), vec!["a"]);
    assert_eq!(sh.take_follow_up(), vec!["b"]);
    assert!(sh.take_follow_up().is_empty());
}

/// steering first
#[test]
fn dequeue_returns_all() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let s = Session::open(deps(&env, store), Open::New).expect("open");
    let h = s.handle();
    h.follow_up("f1".to_string());
    h.steer("s1".to_string());
    assert_eq!(h.dequeue(), vec!["s1", "f1"]);
    assert_eq!(h.queued(), (Vec::new(), Vec::new()));
}

/// queue intact after abort
#[test]
fn abort_returns_queued_text() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let s = Session::open(deps(&env, store), Open::New).expect("open");
    let h = s.handle();
    h.steer("s1".to_string());
    h.follow_up("f1".to_string());
    h.abort();
    assert_eq!(h.queued(), (vec!["s1".to_string()], vec!["f1".to_string()]));
    assert_eq!(h.dequeue(), vec!["s1", "f1"]);
}

/// AgentSettled closes every prompt
#[test]
fn settle_event_last() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut s = Session::open(deps(&env, store), Open::New).expect("open");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    // The scripted engine has no turns, so the request fails; the prompt still settles.
    let _ = s.prompt("hello".to_string(), Vec::new(), &mut sink, &mut ui);
    assert_eq!(settled_count(&sink.0), 1);
    assert!(matches!(sink.0.last(), Some(AgentEvent::AgentSettled { .. })));
}

/// W3
#[test]
fn open_new_writes_prefix_entry_first() {
    let env = TestEnv::new();
    let store = Arc::new(MemStore::new());
    let s = Session::open(deps(&env, store.clone()), Open::New).expect("open");
    let sref = s.inner().sref.clone();
    let entries = store.load(&sref).expect("load");
    assert!(!entries.is_empty());
    assert!(
        matches!(entries[0].body, EntryBody::Prefix(_)),
        "first entry must be the prefix"
    );
    assert!(entries[0].parent.is_none());
}

/// from entries
#[test]
fn open_continue_and_resume_restore_model_thinking_mode() {
    let env = TestEnv::new();
    let store = Arc::new(MemStore::new());
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let mut first = Session::open(deps(&env, store.clone()), Open::New).expect("open");
    first.set_model("other-model", &mut sink).expect("set_model");
    first.set_thinking(ThinkingLevel::High, &mut sink);
    first.set_mode(Mode::Plan, &mut sink);
    let sref = first.inner().sref.clone();
    first.shutdown(&mut ui);
    drop(first);

    let cont = Session::open(deps(&env, store.clone()), Open::Continue).expect("continue");
    let st = cont.state();
    assert_eq!(st.session_id, sref.id);
    assert_eq!(st.model, "other-model");
    assert_eq!(st.thinking, ThinkingLevel::High);
    assert_eq!(st.mode, Mode::Plan);

    let resumed = Session::open(deps(&env, store.clone()), Open::Resume(sref)).expect("resume");
    let st = resumed.state();
    assert_eq!(st.model, "other-model");
    assert_eq!(st.thinking, ThinkingLevel::High);
    assert_eq!(st.mode, Mode::Plan);
}

/// MemStore
#[test]
fn ephemeral_writes_nothing() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let s = Session::open(deps(&env, store.clone()), Open::Ephemeral).expect("open");
    assert!(s.state().session_file.is_none());
    // Nothing reached the store that was offered, nor the disk.
    assert!(store.list(None).is_empty());
    let sessions = env.agent_dir().join("sessions");
    let on_disk = std::fs::read_dir(&sessions).map(|d| d.count()).unwrap_or(0);
    assert_eq!(on_disk, 0);
}

/// W3
#[test]
fn set_mode_never_touches_wire() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut s = Session::open(deps(&env, store), Open::New).expect("open");
    let wire_before = s.inner().transcript.wire().to_vec();
    let prefix_before = s.inner().prefix.sha256;
    let mut sink = VecSink::default();
    s.set_mode(Mode::Ask, &mut sink);
    assert_eq!(s.inner().transcript.wire(), wire_before.as_slice());
    assert_eq!(s.inner().prefix.sha256, prefix_before);
    assert_eq!(s.state().mode, Mode::Ask);
}

/// A hook set that talks to the session through the host it was attached to, the way an
/// extension handler does: from another thread's point of view (`Hooks::fire` runs beside the
/// session thread, which serves the host).
#[derive(Default)]
struct HostUser {
    host: std::sync::Mutex<Option<Arc<dyn oxa_proto::Host>>>,
    /// Results of the blocking `GetState` query made inside `agent_end`.
    states: std::sync::Mutex<Vec<Result<oxa_proto::Value, oxa_proto::OxaError>>>,
    attached: std::sync::atomic::AtomicUsize,
}

impl oxa_proto::Hooks for HostUser {
    fn fire(
        &self,
        p: oxa_proto::HookPoint,
        _payload: &oxa_proto::Value,
        _ui: &mut dyn oxa_proto::Ui,
    ) -> oxa_proto::HookReply {
        if p == oxa_proto::HookPoint::AgentEnd {
            let host = self
                .host
                .lock()
                .unwrap()
                .clone()
                .expect("attached before the run");
            // Fire-and-forget: queued for the next prompt.
            host.call(oxa_proto::HostAction::SendUserMessage {
                text: "queued by the hook".to_string(),
                deliver: oxa_proto::Delivery::NextTurn,
            })
            .expect("accepted");
            // Blocking: answered only if the session thread serves the host while this hook
            // point fires (E-INT 2).
            self.states
                .lock()
                .unwrap()
                .push(host.call(oxa_proto::HostAction::GetState));
        }
        oxa_proto::HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn oxa_proto::Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<oxa_proto::CommandSpec> {
        Vec::new()
    }
    fn run_command(
        &self,
        name: &str,
        _args: &str,
        _ui: &mut dyn oxa_proto::Ui,
    ) -> Result<(), oxa_proto::OxaError> {
        Err(oxa_proto::OxaError::new(oxa_proto::codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn oxa_proto::Host>) {
        *self.host.lock().unwrap() = Some(host);
        self.attached.fetch_add(1, Ordering::SeqCst);
    }
}

/// Delivery::NextTurn
///
/// The joined path (E-INT 1 + 2): a real `Session` attaches a `CoreHost` to its hook sets and
/// serves it. A hook queues a `NextTurn` message and makes a blocking query during a run; the
/// message is appended with the next prompt, before the prompt's own message. The whole
/// scenario runs under a hard time limit, so a session that does not serve its host fails
/// here instead of hanging.
#[test]
fn next_turn_messages_appended_with_prompt() {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let env = TestEnv::new();
        let hook = Arc::new(HostUser::default());
        let mut d = deps(&env, Arc::new(MemStore::new()));
        d.engine = Arc::new(ScriptedEngine::new(vec![
            oxa_fake::ScriptTurn::text("one"),
            oxa_fake::ScriptTurn::text("two"),
        ]));
        d.hooks = vec![hook.clone()];
        let mut s = Session::open(d, Open::New).expect("open");
        assert_eq!(hook.attached.load(Ordering::SeqCst), 1, "attached once at open");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();

        s.prompt("first".to_string(), Vec::new(), &mut sink, &mut ui)
            .expect("first prompt");
        let users = |s: &Session| -> Vec<String> {
            s.inner()
                .transcript
                .wire()
                .iter()
                .filter(|m| m.role == oxa_proto::Role::User)
                .filter_map(|m| m.content.clone())
                .collect()
        };
        assert_eq!(
            users(&s),
            ["first"],
            "the queued message waits for the next prompt"
        );
        assert_eq!(s.state().pending_message_count, 1);
        {
            let states = hook.states.lock().unwrap();
            assert_eq!(states.len(), 1);
            let state = states[0].as_ref().expect("the blocking query was answered");
            assert_eq!(state["isStreaming"], serde_json::json!(true), "answered mid-run");
        }

        s.prompt("second".to_string(), Vec::new(), &mut sink, &mut ui)
            .expect("second prompt");
        assert_eq!(
            users(&s),
            ["first", "queued by the hook", "second"],
            "appended with the prompt, before its own message"
        );
        // The second run queued another one, for a third prompt.
        assert_eq!(s.state().pending_message_count, 1);
        s.shutdown(&mut ui);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the session must serve its host: the scenario did not finish in 30 s");
}

/// wire text
#[test]
fn image_part_adds_placeholder_line() {
    let env = TestEnv::new();
    let store = Arc::new(MemStore::new());
    let mut s = Session::open(deps(&env, store.clone()), Open::New).expect("open");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let part = Part::Image {
        path: PathBuf::from("/somewhere/pic.png"),
        mime: "image/png".to_string(),
        sha256: [0u8; 32],
        w: 4,
        h: 3,
    };
    let _ = s.prompt("look".to_string(), vec![part.clone()], &mut sink, &mut ui);
    let sref = s.inner().sref.clone();
    let entries = store.load(&sref).expect("load");
    let user = entries
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::Msg { wire, parts, .. } if !parts.is_empty() => Some((wire.clone(), parts.clone())),
            _ => None,
        })
        .expect("the user message is stored");
    assert_eq!(
        user.0.content.as_deref(),
        Some("look\n[image pic.png 4x3: not visible to this model]")
    );
    assert_eq!(user.1, vec![part], "the attachment is kept as shown data");
}

/// Keeps `Queue` in the public surface exercised by the tests.
#[test]
fn queue_struct_starts_empty() {
    let q = Queue::default();
    assert!(q.steering.is_empty() && q.follow_up.is_empty());
    let sh = Shared::new(QueueMode::All, QueueMode::All);
    assert!(!sh.busy.load(Ordering::SeqCst));
}

/// #522: a new session points the policy at its spill directory, so the full output of a
/// capped result reads without a question (an unattended run cannot answer one).
#[test]
fn opening_a_session_makes_its_spill_directory_readable() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let s = Session::open(deps(&env, store), Open::New).expect("open");
    let st = s.inner();
    let read = |p: PathBuf| st.policy.decide(Mode::Workspace, &Access::Read(p), &st.cwd);
    assert_eq!(read(st.spill_dir.join("0123.txt")), Decision::Allow);
    assert_eq!(
        read(st.dirs.tmp_dir.join("other-session/0123.txt")),
        Decision::Ask,
        "outside the cwd and not this session's"
    );
}

fn guarded_deps(env: &TestEnv, store: Arc<dyn SessionStore>) -> Deps {
    let mut d = deps(env, store);
    // Production keeps spills under the denied agent directory, unlike Dirs::under.
    d.dirs.tmp_dir = d.dirs.agent_dir.join("tmp");
    d.policy = Box::new(ModePolicy::new().with_guard(oxa_perm::PathGuard::new(
        &d.dirs.home,
        &d.dirs.agent_dir,
        &d.dirs.tmp_dir,
    )));
    d
}

fn assert_readable_spill(s: &Session, path: &std::path::Path) {
    let st = s.inner();
    for mode in Mode::ALL {
        assert_eq!(
            st.policy.decide(mode, &Access::Read(path.to_path_buf()), &st.cwd),
            Decision::Allow,
            "{mode:?}: {}",
            path.display()
        );
    }
    assert!(
        !st.policy.path_filter().expect("guard").hides(path),
        "a readable spill must also be visible to directory-walking tools"
    );
}

/// #555: --fork copies tool results verbatim, including their original spill paths.
#[test]
fn forked_and_resumed_sessions_can_read_inherited_spills_only() {
    let env = TestEnv::new();
    let store = Arc::new(MemStore::new());
    let source = Session::open(guarded_deps(&env, store.clone()), Open::New).expect("source");
    let full = "synthetic tool output\n".repeat(200);
    let shaped = oxa_tools::shape(full.as_bytes(), 512, false, &source.inner().spill_dir);
    let spill = std::fs::read_dir(&source.inner().spill_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(shaped.contains(&spill.display().to_string()));
    let src = source.inner().sref.clone();
    store
        .append(
            &src,
            &oxa_proto::Entry {
                id: "deadbeef".into(),
                parent: Some(source.inner().tree.leaf().to_string()),
                ts_ms: 0,
                body: EntryBody::Msg {
                    wire: oxa_proto::WireMsg::tool_result("call", shaped.clone()),
                    parts: Vec::new(),
                    tool: None,
                },
            },
        )
        .unwrap();
    let fork = Session::open(guarded_deps(&env, store.clone()), Open::Fork(src)).expect("fork");
    assert!(fork
        .inner()
        .transcript
        .wire()
        .iter()
        .any(|w| w.content.as_deref() == Some(&shaped)));
    assert_readable_spill(&fork, &spill);
    assert_eq!(std::fs::read_to_string(&spill).unwrap(), full);
    // A fork of a fork, including a fresh-process resume, keeps the original output readable.
    let nested = Session::open(
        guarded_deps(&env, store.clone()),
        Open::Fork(fork.inner().sref.clone()),
    )
    .expect("nested fork");
    let mut resumed = Session::open(
        guarded_deps(&env, store.clone()),
        Open::Resume(nested.inner().sref.clone()),
    )
    .expect("resume");
    assert_readable_spill(&resumed, &spill);
    let st = resumed.inner();
    for mode in Mode::ALL {
        for access in [
            Access::Read(st.dirs.tmp_dir.join("unrelated/out.txt")),
            Access::Write(spill.clone()),
            Access::Read(st.dirs.agent_dir.join("settings.json")),
        ] {
            assert_eq!(
                st.policy.decide(mode, &access, &st.cwd),
                Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
            );
        }
    }
    // Session switches revoke the inherited exceptions, even in the same process.
    resumed
        .switch(Open::New, &mut VecSink::default(), &mut HeadlessUi::default())
        .unwrap();
    let st = resumed.inner();
    assert_eq!(
        st.policy.decide(Mode::Workspace, &Access::Read(spill), &st.cwd),
        Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
    );
}

/// /clone and /fork switch without going through Session::open.
#[test]
fn live_clone_and_fork_rebind_spill_policy_and_walk_filter() {
    let env = TestEnv::new();
    let store = Arc::new(MemStore::new());
    let mut s = Session::open(guarded_deps(&env, store), Open::New).unwrap();
    let old = s.inner().spill_dir.clone();
    s.clone_session().expect("clone");
    assert_ne!(s.inner().spill_dir, old, "clone writes its own spill files");
    assert_readable_spill(&s, &s.inner().spill_dir.join("out.txt"));
    assert_readable_spill(&s, &old.join("inherited.txt"));
    // The scripted engine has no turn, but prompt still persists a user message to fork before.
    let _ = s.prompt(
        "fork here".into(),
        Vec::new(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    );
    let user = s
        .inner()
        .tree
        .entries()
        .iter()
        .find(|e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.role == oxa_proto::Role::User))
        .unwrap()
        .id
        .clone();
    let before = s.inner().spill_dir.clone();
    s.fork(&user).expect("live fork");
    assert_ne!(s.inner().spill_dir, before, "fork writes its own spill files");
    assert_readable_spill(&s, &s.inner().spill_dir.join("out.txt"));
    assert_readable_spill(&s, &old.join("inherited.txt"));
}

/// #877: a `!cmd` is foreground work while it holds the session. Without the foreground signal
/// the front end reads a long command as idle and takes input typed during it for a new run.
#[test]
fn a_user_bash_command_is_foreground_work_until_it_ends() {
    use oxa_proto::EventSink;
    use std::sync::atomic::AtomicBool;
    struct RunningAtAppend(oxa_core::Handle, Arc<AtomicBool>);
    impl EventSink for RunningAtAppend {
        fn emit(&mut self, event: &AgentEvent) {
            if matches!(event, AgentEvent::EntryAppended { .. }) {
                self.1.store(self.0.is_running(), Ordering::SeqCst);
            }
        }
    }
    let env = TestEnv::new();
    let mut session = Session::open(deps(&env, Arc::new(MemStore::new())), Open::New).unwrap();
    let handle = session.handle();
    let seen = Arc::new(AtomicBool::new(false));
    session
        .user_bash(
            "printf expected > bash-started",
            false,
            &mut RunningAtAppend(handle.clone(), seen.clone()),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert!(
        seen.load(Ordering::SeqCst),
        "the session is foreground work while the command holds it"
    );
    assert!(!handle.is_running(), "the signal ends with the command");
    assert!(!handle.is_busy());
}

<<<<<<< HEAD
/// #1693: `/tree` branch-summary generation emits no agent activity events, but it is still
=======
/// #877: `/tree` branch-summary generation emits no agent activity events, but it is still
>>>>>>> origin/agent/yolo-s11
/// foreground work and must remain cancellable from the interactive front end.
#[test]
fn a_branch_summary_is_foreground_work_until_navigation_ends() {
    struct RunningAtTreeHook {
        handle: Mutex<Option<oxa_core::Handle>>,
        seen: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Hooks for RunningAtTreeHook {
        fn fire(&self, point: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
            if point == HookPoint::SessionBeforeTree {
                self.seen.store(
                    self.handle.lock().unwrap().as_ref().unwrap().is_running(),
                    Ordering::SeqCst,
                );
            }
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
            Vec::new()
        }
    }

    let env = TestEnv::new();
    let hook = Arc::new(RunningAtTreeHook {
        handle: Mutex::new(None),
        seen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("first reply"),
        ScriptTurn::text("second reply"),
        ScriptTurn::text("branch summary"),
    ]));
    let mut session = Session::open(
        deps_with(&env, Arc::new(MemStore::new()), engine, vec![hook.clone()]),
        Open::New,
    )
    .unwrap();
    let handle = session.handle();
    *hook.handle.lock().unwrap() = Some(handle.clone());
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    session
        .prompt("one".into(), Vec::new(), &mut sink, &mut ui)
        .unwrap();
    session
        .prompt("two".into(), Vec::new(), &mut sink, &mut ui)
        .unwrap();
    let target = session
        .inner()
        .tree
        .entries()
        .iter()
        .find(|entry| {
            matches!(&entry.body, EntryBody::Msg { wire, .. } if wire.content.as_deref() == Some("first reply"))
        })
        .unwrap()
        .id
        .clone();

<<<<<<< HEAD
    let navigation_events = sink.0.len();
=======
>>>>>>> origin/agent/yolo-s11
    session
        .navigate(&target, true, None, &mut sink, &mut ui)
        .expect("navigate with a branch summary");

    assert!(
        hook.seen.load(Ordering::SeqCst),
        "branch-summary navigation is foreground while it holds the session"
    );
<<<<<<< HEAD
    assert!(
        !sink.0[navigation_events..].iter().any(|event| matches!(
            event,
            AgentEvent::AgentStart | AgentEvent::TurnStart | AgentEvent::MessageStart { .. }
        )),
        "branch-summary navigation must not rely on activity events"
    );
=======
>>>>>>> origin/agent/yolo-s11
    assert!(!handle.is_running(), "the signal ends with navigation");
    assert!(!handle.is_busy());
}

/// #744: queued RPC bash jobs must not run after a headless SIGINT, even on a later claim.
#[test]
fn interrupt_before_user_bash_claim_is_sticky_and_does_not_execute() {
    let env = TestEnv::new();
    let mut session = Session::open(deps(&env, Arc::new(MemStore::new())), Open::New).unwrap();
    let handle = session.handle();
    handle.interrupt();
    for _ in 0..2 {
        let _ = session.user_bash(
            "printf unwanted > bash-started",
            false,
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        );
        assert!(!env.work().join("bash-started").exists(), "interrupted bash ran");
        assert!(handle.interrupted());
        assert!(!handle.is_busy());
    }
}

/// An ordinary idle abort must still leave the next interactive !cmd usable.
#[test]
fn idle_abort_does_not_prevent_user_bash() {
    let env = TestEnv::new();
    let mut session = Session::open(deps(&env, Arc::new(MemStore::new())), Open::New).unwrap();
    let handle = session.handle();
    handle.abort();
    session
        .user_bash(
            "printf expected > bash-started",
            false,
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(env.work().join("bash-started")).unwrap(),
        "expected"
    );
    assert!(!handle.interrupted());
    assert!(!handle.is_busy());
}

/// #699: a headless SIGINT before the worker claims a prompt must not be reset away.
#[test]
fn interrupt_before_claim_is_sticky_and_does_not_generate() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![oxa_fake::ScriptTurn::text("unwanted")]));
    let mut dependencies = deps(&env, Arc::new(MemStore::new()));
    dependencies.engine = engine.clone();
    let mut session = Session::open(dependencies, Open::New).unwrap();
    let handle = session.handle();
    handle.interrupt();
    for _ in 0..2 {
        let _ = session.prompt(
            "synthetic".into(),
            vec![],
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        );
        assert!(handle.interrupted());
        assert_eq!(session.take_outcome(), oxa_core::RunOutcome::Interrupted);
        // ScriptedEngine records even a request whose token was already cancelled,
        // but must not consume a scripted generation turn.
        assert_eq!(engine.remaining(), 1);
        assert!(!handle.is_busy());
    }
}

#[test]
fn session_switch_and_clone_do_not_inherit_another_inbox() {
    let env = TestEnv::new();
    let mut session = Session::open(deps(&env, Arc::new(MemStore::new())), Open::New).unwrap();
    let original = session.inner().sref.clone();
    let handle = session.handle();
    handle.follow_up_for(&original.id, "admitted before switch".into());
    session
        .switch(Open::New, &mut VecSink::default(), &mut HeadlessUi::default())
        .unwrap();
    handle.follow_up_for(&original.id, "late old callback".into());
    assert!(!handle.has_inbox_input());
    assert!(handle.queued().1.is_empty());
    session.clone_session().unwrap();
    assert!(handle.queued().1.is_empty());
    session
        .switch(
            Open::Resume(original),
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert!(handle.has_inbox_input());
    assert_eq!(handle.dequeue(), ["admitted before switch", "late old callback"]);
}
