//! Post-wave-E fixes (lane L4): triggered turns, `waitForIdle` inside a run, the persisted
//! declared rebase. Extension-free: a native `Hooks` drives the session's `CoreHost`.
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, AgentEvent, CommandSpec, Delivery, EntryBody, HeadlessUi, HookPoint, HookReply, Hooks, Host,
    HostAction, OxaError, Role, Tool, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{FsStore, MemStore, SessionStore};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn deps(
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
    }
}

/// Run `f` on its own thread; fail if it has not finished within `secs`.
fn bounded(secs: u64, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(()) => worker.join().expect("scenario thread"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // The scenario panicked: surface its message.
            if let Err(p) = worker.join() {
                std::panic::resume_unwind(p);
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("the scenario did not finish in {secs} s (hang)"),
    }
}

fn trigger(text: &str, deliver: Delivery) -> HostAction {
    HostAction::SendMessage {
        custom_type: "test".into(),
        content: text.into(),
        display: true,
        details: Value::Null,
        deliver,
        trigger_turn: true,
    }
}

type Action = Box<dyn Fn(&Arc<dyn Host>) + Send + Sync>;

/// A hook set that keeps its host and runs scripted actions at hook points and in one command.
#[derive(Default)]
struct Scripted {
    host: Mutex<Option<Arc<dyn Host>>>,
    at: Mutex<Vec<(HookPoint, Action)>>,
    command: Mutex<Option<Action>>,
    /// The rebase this hook set declares at `Context` (see `rebase_first`).
    rebase: bool,
}

impl Scripted {
    fn host(&self) -> Arc<dyn Host> {
        self.host.lock().unwrap().clone().expect("attached")
    }
    fn on(&self, p: HookPoint, f: impl Fn(&Arc<dyn Host>) + Send + Sync + 'static) {
        self.at.lock().unwrap().push((p, Box::new(f)));
    }
}

/// The wire of the rebase scenario before and after the declared rebase.
fn rebase_before() -> Vec<WireMsg> {
    vec![
        WireMsg::user("a"),
        ScriptTurn::text("one").message(),
        WireMsg::user("first"),
    ]
}
fn rebase_after() -> Vec<WireMsg> {
    vec![
        WireMsg::user("a"),
        ScriptTurn::text("one").message(),
        WireMsg::user("FIRST"),
    ]
}

impl Hooks for Scripted {
    fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        let host = self.host.lock().unwrap().clone();
        if let Some(host) = host {
            for (point, f) in self.at.lock().unwrap().iter() {
                if *point == p {
                    f(&host);
                }
            }
        }
        if self.rebase && p == HookPoint::Context {
            let last = payload["messages"]
                .as_array()
                .and_then(|m| m.last())
                .and_then(|m| m["content"].as_str())
                .unwrap_or_default()
                .to_string();
            if last == "first" {
                return HookReply::DeclaredRebase {
                    messages: rebase_after(),
                    origin: "rebaser.js".into(),
                };
            }
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: "go".into(),
            description: String::new(),
            argument_hint: None,
            source: oxa_proto::Source::Extension(String::new()),
        }]
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        if let Some(f) = self.command.lock().unwrap().as_ref() {
            f(&self.host());
        }
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
}

fn settled(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AgentSettled { .. }))
        .count()
}

fn users(s: &Session) -> Vec<String> {
    s.inner()
        .transcript
        .wire()
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content.clone())
        .collect()
}

/// Item 1: `sendMessage(.., {triggerTurn: true})` made while the session is idle starts a run
/// at the next idle serve (the interactive idle tick): the message is a custom message entry,
/// the model is called once, the run settles with `AgentSettled`.
#[test]
fn trigger_turn_runs_from_the_idle_serve() {
    bounded(30, || {
        let env = TestEnv::new();
        let hook = Arc::new(Scripted::default());
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("pong")]));
        let d = deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            vec![hook.clone()],
        );
        let mut s = Session::open(d, Open::New).expect("open");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();

        hook.host()
            .call(trigger("ping", Delivery::Steer))
            .expect("accepted");
        assert!(
            engine.requests().is_empty(),
            "nothing runs before the session is served"
        );
        s.serve_host(&mut sink, &mut ui).expect("idle serve");

        let reqs = engine.requests();
        assert_eq!(reqs.len(), 1, "one run");
        assert_eq!(reqs[0].messages, vec![WireMsg::user("ping")]);
        assert_eq!(s.inner().transcript.wire().len(), 2, "the reply is on the wire");
        assert!(s
            .inner()
            .tree
            .branch(s.inner().tree.leaf())
            .iter()
            .any(|e| matches!(
                &e.body,
                EntryBody::CustomMessage { custom_type, display: true, .. } if custom_type == "test"
            )));
        assert_eq!(settled(&sink.0), 1);
        assert_eq!(s.state().pending_message_count, 0, "not left in a queue");

        // Nothing pending: a further idle serve runs nothing.
        s.serve_host(&mut sink, &mut ui).expect("idle serve");
        assert_eq!(engine.requests().len(), 1);
        assert_eq!(settled(&sink.0), 1);
    });
}

/// Item 1: a command handler that triggers a turn (the usual pi pattern) gets the turn after
/// the handler returned, in the same `run_command` call (print / JSON / RPC run it this way).
#[test]
fn trigger_turn_from_a_command_runs_after_the_handler() {
    bounded(30, || {
        let env = TestEnv::new();
        let hook = Arc::new(Scripted::default());
        *hook.command.lock().unwrap() = Some(Box::new(|h: &Arc<dyn Host>| {
            h.call(trigger("from the command", Delivery::FollowUp))
                .expect("accepted");
        }));
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("done")]));
        let d = deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            vec![hook.clone()],
        );
        let mut s = Session::open(d, Open::New).expect("open");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        s.run_command("go", "", &mut sink, &mut ui).expect("command");
        assert_eq!(engine.requests().len(), 1);
        assert_eq!(users(&s), ["from the command"]);
        assert_eq!(settled(&sink.0), 1);
    });
}

/// Item 1: during a run a triggering message is delivered per `deliver` (here a follow-up):
/// it reaches the model once, inside the same prompt; no second, separate run starts.
#[test]
fn trigger_turn_during_a_run_is_delivered_not_doubled() {
    bounded(30, || {
        let env = TestEnv::new();
        let hook = Arc::new(Scripted::default());
        let once = std::sync::atomic::AtomicBool::new(false);
        hook.on(HookPoint::AgentStart, move |h| {
            if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                h.call(trigger("later", Delivery::FollowUp)).expect("accepted");
            }
        });
        let engine = Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
        ]));
        let d = deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            vec![hook.clone()],
        );
        let mut s = Session::open(d, Open::New).expect("open");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        s.prompt("hi".into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        assert_eq!(
            engine.requests().len(),
            2,
            "the prompt's run and the delivered message"
        );
        assert_eq!(users(&s), ["hi", "later"]);
        assert_eq!(settled(&sink.0), 1, "one settlement");
        s.serve_host(&mut sink, &mut ui).expect("idle serve");
        assert_eq!(engine.requests().len(), 2, "nothing left to run");
    });
}

/// Item 9: a handler the run waits for (here `input`) cannot wait for the run to end; its
/// `WaitForIdle` is refused at once with `busy` and the prompt completes.
#[test]
fn wait_for_idle_from_a_run_handler_is_refused() {
    bounded(20, || {
        let env = TestEnv::new();
        let hook = Arc::new(Scripted::default());
        let got: Arc<Mutex<Vec<Result<Value, OxaError>>>> = Arc::default();
        let g = got.clone();
        hook.on(HookPoint::Input, move |h| {
            g.lock().unwrap().push(h.call(HostAction::WaitForIdle));
        });
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
        let d = deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            vec![hook.clone()],
        );
        let mut s = Session::open(d, Open::New).expect("open");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        s.prompt("hi".into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].as_ref().expect_err("refused").code, codes::BUSY);
    });
}

/// Item 9: a command handler (no run in progress) waits for idle and is answered at once; a
/// waiter outside the run (a background thread) is released at settlement, also after an abort.
#[test]
fn wait_for_idle_outside_the_busy_scope_resolves() {
    bounded(20, || {
        let env = TestEnv::new();
        let hook = Arc::new(Scripted::default());
        let got: Arc<Mutex<Vec<Result<Value, OxaError>>>> = Arc::default();
        let g = got.clone();
        *hook.command.lock().unwrap() = Some(Box::new(move |h: &Arc<dyn Host>| {
            g.lock().unwrap().push(h.call(HostAction::WaitForIdle));
        }));
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        hook.on(HookPoint::AgentStart, move |h| {
            let waiter = h.clone();
            let tx = tx.lock().unwrap().clone();
            std::thread::spawn(move || {
                let _ = tx.send(waiter.call(HostAction::WaitForIdle));
            });
            // Abort the run: the background waiter must still be released.
            let _ = h.call(HostAction::Abort);
        });
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
        let d = deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            vec![hook.clone()],
        );
        let mut s = Session::open(d, Open::New).expect("open");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        s.run_command("go", "", &mut sink, &mut ui).expect("command");
        assert_eq!(got.lock().unwrap().as_slice(), &[Ok(Value::Null)]);

        s.prompt("hi".into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        let r = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the waiter was released");
        assert_eq!(r, Ok(Value::Null));
    });
}

/// Item 4: a declared rebase is stored with the rebased wire; a resumed session folds it to
/// the identical wire and chain, and its first request carries exactly the bytes the session
/// before the restart sends.
#[test]
fn declared_rebase_resumes_exactly() {
    bounded(30, || {
        let env = TestEnv::new();
        let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.root().join("sessions")));
        let hook = Arc::new(Scripted {
            rebase: true,
            ..Scripted::default()
        });
        let engine = Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("three"),
        ]));
        let d = deps(&env, store.clone(), engine.clone(), vec![hook.clone()]);
        let mut a = Session::open(d, Open::New).expect("open");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        a.prompt("a".into(), Vec::new(), &mut sink, &mut ui).expect("a");
        let origins_before: Vec<Option<String>> = (0..2)
            .map(|i| a.inner().transcript.origin(i).map(String::from))
            .collect();
        a.prompt("first".into(), Vec::new(), &mut sink, &mut ui)
            .expect("first");
        let sent = engine.requests()[1].messages.clone();
        assert_eq!(sent, rebase_after(), "the rebased wire was sent");
        assert_ne!(sent, rebase_before());
        let t = &a.inner().transcript;
        assert_eq!(
            t.rebases().last().map(|r| r.why),
            Some(oxa_proto::rebase_why::EXTENSION)
        );
        // Byte-identical messages keep their origins; the replaced one has none.
        for (i, o) in origins_before.iter().enumerate() {
            assert_eq!(t.origin(i), o.as_deref(), "origin {i} kept");
        }
        assert_eq!(t.origin(2), None);

        // Resume from the file without the rebasing extension.
        let sref = a.inner().sref.clone();
        let engine_b = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("three")]));
        let mut b = Session::open(
            deps(&env, store.clone(), engine_b.clone(), Vec::new()),
            Open::Resume(sref),
        )
        .expect("resume");
        assert_eq!(b.inner().transcript.wire(), a.inner().transcript.wire());
        assert_eq!(b.inner().transcript.chain(), a.inner().transcript.chain());
        assert_eq!(
            b.inner().transcript.rebases().last().map(|r| r.why),
            Some(oxa_proto::rebase_why::EXTENSION)
        );

        b.prompt("next".into(), Vec::new(), &mut sink, &mut ui)
            .expect("b next");
        a.prompt("next".into(), Vec::new(), &mut sink, &mut ui)
            .expect("a next");
        let ra = engine.requests().last().cloned().expect("a request");
        let rb = engine_b.requests().last().cloned().expect("b request");
        assert_eq!(rb.messages, ra.messages, "same messages after the restart");
        assert_eq!(rb.system, ra.system);
        assert_eq!(rb.tools_json, ra.tools_json);
    });
}
