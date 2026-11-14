//! Wave H (lane run): `AgentSettled` is the last thing an operation does. Every busy claim is
//! released before the record is emitted, on every exit path, so a client that acts on the
//! record (the RPC contract: "wait for agent_settled") never meets a busy session.
//!
//! Deterministic: the probe sink calls back into the session's `Handle` at the moment it
//! receives `AgentSettled` and records what it saw; no sleeps, no timing.
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Handle, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, CommandSpec, Delivery, EventSink, HeadlessUi, HookPoint, HookReply, Hooks, Host, HostAction,
    OxaError, Tool, Ui, Value,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use std::sync::{Arc, Mutex};

fn deps(env: &TestEnv, engine: Arc<ScriptedEngine>, hooks: Vec<Arc<dyn Hooks>>) -> Deps {
    Deps {
        engine,
        store: Arc::new(MemStore::new()),
        tools: Vec::new(),
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::default(),
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

/// Records every event; at `AgentSettled` it asks the handle whether the session is still
/// claimed. With `abort_on_start` it aborts the run when `AgentStart` arrives.
struct Probe {
    handle: Handle,
    events: Vec<AgentEvent>,
    busy_at_settle: Vec<bool>,
    abort_on_start: bool,
}

impl Probe {
    fn new(handle: Handle) -> Probe {
        Probe {
            handle,
            events: Vec::new(),
            busy_at_settle: Vec::new(),
            abort_on_start: false,
        }
    }

    /// Exactly `n` settle records, each seen with the session idle, and the last event is one.
    fn assert_settled_idle(&self, n: usize) {
        assert_eq!(self.busy_at_settle.len(), n, "settle records: {:?}", self.events);
        assert!(
            self.busy_at_settle.iter().all(|b| !b),
            "the session was still claimed when AgentSettled was emitted: {:?}",
            self.busy_at_settle
        );
        if n > 0 {
            assert!(
                matches!(self.events.last(), Some(AgentEvent::AgentSettled { .. })),
                "AgentSettled is the last event: {:?}",
                self.events.last()
            );
        }
    }
}

impl EventSink for Probe {
    fn emit(&mut self, e: &AgentEvent) {
        match e {
            AgentEvent::AgentSettled { .. } => self.busy_at_settle.push(self.handle.is_busy()),
            AgentEvent::AgentStart if self.abort_on_start => self.handle.abort(),
            _ => {}
        }
        self.events.push(e.clone());
    }
}

fn trigger(text: &str) -> HostAction {
    HostAction::SendMessage {
        custom_type: "test".into(),
        content: text.into(),
        display: true,
        details: Value::Null,
        deliver: Delivery::FollowUp,
        trigger_turn: true,
    }
}

/// A hook set with one command, `go`, that asks the host for a triggered turn.
#[derive(Default)]
struct Trigger {
    host: Mutex<Option<Arc<dyn Host>>>,
}

impl Hooks for Trigger {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
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
        let host = self.host.lock().unwrap().clone().expect("attached");
        host.call(trigger("from the command")).map(|_| ())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
}

fn open(env: &TestEnv, turns: Vec<ScriptTurn>, hooks: Vec<Arc<dyn Hooks>>) -> Session {
    let engine = Arc::new(ScriptedEngine::new(turns));
    Session::open(deps(env, engine, hooks), Open::New).expect("open")
}

/// A normal prompt: the claim is gone when the closing record is emitted.
#[test]
fn prompt_settles_after_release() {
    let env = TestEnv::new();
    let mut s = open(&env, vec![ScriptTurn::text("hi")], Vec::new());
    let mut probe = Probe::new(s.handle());
    s.prompt("hello".into(), Vec::new(), &mut probe, &mut HeadlessUi::default())
        .expect("prompt");
    probe.assert_settled_idle(1);
    // Repeated prompts: each one settles idle (no state left over between them).
    for i in 0..20 {
        let mut s = open(&env, vec![ScriptTurn::text("ok")], Vec::new());
        let mut probe = Probe::new(s.handle());
        s.prompt(
            format!("p{i}"),
            Vec::new(),
            &mut probe,
            &mut HeadlessUi::default(),
        )
        .expect("prompt");
        probe.assert_settled_idle(1);
    }
}

/// A failed run (the engine has no turn): settled idle all the same.
#[test]
fn failed_prompt_settles_after_release() {
    let env = TestEnv::new();
    let mut s = open(&env, Vec::new(), Vec::new());
    let mut probe = Probe::new(s.handle());
    let _ = s.prompt("hello".into(), Vec::new(), &mut probe, &mut HeadlessUi::default());
    probe.assert_settled_idle(1);
}

/// An aborted run: settled idle, with `aborted: true`.
#[test]
fn aborted_prompt_settles_after_release() {
    let env = TestEnv::new();
    let mut s = open(&env, vec![ScriptTurn::text("never shown")], Vec::new());
    let mut probe = Probe::new(s.handle());
    probe.abort_on_start = true;
    let _ = s.prompt("hello".into(), Vec::new(), &mut probe, &mut HeadlessUi::default());
    probe.assert_settled_idle(1);
    assert!(
        matches!(
            probe.events.last(),
            Some(AgentEvent::AgentSettled { aborted: true })
        ),
        "{:?}",
        probe.events.last()
    );
}

/// Queued input run by `run_queued` (after a navigation or `/compact`).
#[test]
fn run_queued_settles_after_release() {
    let env = TestEnv::new();
    let mut s = open(&env, vec![ScriptTurn::text("ok")], Vec::new());
    s.handle().follow_up("later".into());
    let mut probe = Probe::new(s.handle());
    s.run_queued(&mut probe, &mut HeadlessUi::default())
        .expect("run queued");
    probe.assert_settled_idle(1);
}

/// A command handler that triggers a turn; and the same through the idle serve.
#[test]
fn triggered_turns_settle_after_release() {
    let env = TestEnv::new();
    let hook = Arc::new(Trigger::default());
    let mut s = open(
        &env,
        vec![ScriptTurn::text("one"), ScriptTurn::text("two")],
        vec![hook.clone()],
    );
    let mut probe = Probe::new(s.handle());
    s.run_command("go", "", &mut probe, &mut HeadlessUi::default())
        .expect("command");
    probe.assert_settled_idle(1);

    let host = hook.host.lock().unwrap().clone().expect("attached");
    host.call(trigger("idle")).expect("accepted");
    let mut probe = Probe::new(s.handle());
    s.serve_host(&mut probe, &mut HeadlessUi::default())
        .expect("idle serve");
    probe.assert_settled_idle(1);
}

/// `Handle::abort_next` (an RPC prompt answered `started` and aborted before its run claimed
/// the session): the next run starts cancelled and settles aborted; the mark is consumed.
#[test]
fn abort_next_aborts_the_next_run() {
    let env = TestEnv::new();
    let mut s = open(
        &env,
        vec![ScriptTurn::text("never"), ScriptTurn::text("later")],
        Vec::new(),
    );
    s.handle().abort_next();
    let mut probe = Probe::new(s.handle());
    let _ = s.prompt("go".into(), Vec::new(), &mut probe, &mut HeadlessUi::default());
    probe.assert_settled_idle(1);
    assert!(
        matches!(
            probe.events.last(),
            Some(AgentEvent::AgentSettled { aborted: true })
        ),
        "{:?}",
        probe.events.last()
    );
    // Consumed: the following prompt runs normally.
    let mut probe = Probe::new(s.handle());
    s.prompt("again".into(), Vec::new(), &mut probe, &mut HeadlessUi::default())
        .expect("prompt");
    assert!(matches!(
        probe.events.last(),
        Some(AgentEvent::AgentSettled { aborted: false })
    ));
    // Withdrawn before a claim: nothing is aborted.
    s.handle().abort_next();
    s.handle().clear_abort_next();
    let mut probe = Probe::new(s.handle());
    let _ = s.prompt("third".into(), Vec::new(), &mut probe, &mut HeadlessUi::default());
    assert!(matches!(
        probe.events.last(),
        Some(AgentEvent::AgentSettled { aborted: false })
    ));
}
