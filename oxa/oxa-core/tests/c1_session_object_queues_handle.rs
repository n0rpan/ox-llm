//! C1 acceptance tests: Session object, queues, Handle.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{Deps, Open, Queue, Session, Shared, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, EntryBody, HeadlessUi, Mode, Part, ThinkingLevel, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Deps over a scripted engine with no turns, the given store and the test's own directories.
fn deps(env: &TestEnv, store: Arc<dyn SessionStore>) -> Deps {
    Deps {
        engine: Arc::new(ScriptedEngine::new(Vec::new())),
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
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
