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

#[test]
fn progress_does_not_report_idle_or_finished_before_lifecycle_hooks_return() {
    #[derive(Debug, Default)]
    struct Trace(Mutex<Vec<&'static str>>);
    impl oxa_core::ProgressObserver for Trace {
        fn notify(&self, _: &oxa_session::SessionRef, progress: oxa_core::Progress<'_>) {
            match progress {
                oxa_core::Progress::Settled(_) => self.0.lock().unwrap().push("idle"),
                oxa_core::Progress::Finished(_) => self.0.lock().unwrap().push("finished"),
                _ => {}
            }
        }
    }
    impl Hooks for Trace {
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            vec![]
        }
        fn commands(&self) -> Vec<CommandSpec> {
            vec![]
        }
        fn run_command(&self, _: &str, _: &str, _: &mut dyn Ui) -> Result<(), OxaError> {
            Ok(())
        }
        fn prompt_sections(&self) -> Vec<(String, String)> {
            vec![]
        }
        fn fire(&self, point: HookPoint, _: &Value, _: &mut dyn Ui) -> HookReply {
            match point {
                HookPoint::AgentSettled => self.0.lock().unwrap().push("settled_hook"),
                HookPoint::SessionShutdown => self.0.lock().unwrap().push("shutdown_hook"),
                _ => {}
            }
            HookReply::Continue
        }
        fn shutdown(&self) {
            self.0.lock().unwrap().push("shutdown_return");
        }
    }
    let env = TestEnv::new();
    let trace = Arc::new(Trace::default());
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("done")]));
    let mut session = Session::open(deps(&env, engine, vec![trace.clone()]), Open::New).unwrap();
    session.set_progress_observer(trace.clone());
    session
        .prompt(
            "go".into(),
            vec![],
            &mut oxa_proto::VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    session.shutdown(&mut HeadlessUi::default());
    assert_eq!(
        *trace.0.lock().unwrap(),
        vec!["settled_hook", "idle", "shutdown_hook", "shutdown_return"],
        "the front end owns the final result after shutdown; hooks remain active work"
    );
}

fn deps(env: &TestEnv, engine: Arc<dyn oxa_proto::Engine>, hooks: Vec<Arc<dyn Hooks>>) -> Deps {
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
        jobs: oxa_core::fresh_jobs(),
    }
}

/// Records every event; at `AgentSettled` it asks the handle whether the session is still
/// claimed. With `abort_on_start` it aborts the run when `AgentStart` arrives.
struct Probe {
    handle: Handle,
    events: Vec<AgentEvent>,
    busy_at_settle: Vec<bool>,
    abort_on_start: bool,
    wrap_on_overrun: Option<oxa_core::WrapUpReason>,
}

impl Probe {
    fn new(handle: Handle) -> Probe {
        Probe {
            handle,
            events: Vec::new(),
            busy_at_settle: Vec::new(),
            abort_on_start: false,
            wrap_on_overrun: None,
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

    fn assert_balanced(&self) {
        let (mut turn, mut message) = (false, false);
        for event in &self.events {
            match event {
                AgentEvent::TurnStart => {
                    assert!(!turn, "a previous turn is still open");
                    turn = true;
                }
                AgentEvent::MessageStart { message: m } if m.msg.role == oxa_proto::Role::Assistant => {
                    assert!(!message, "a previous assistant message is still open");
                    message = true;
                }
                AgentEvent::MessageEnd { message: m } if m.msg.role == oxa_proto::Role::Assistant => {
                    assert!(message, "an assistant message ends without a start");
                    message = false;
                }
                AgentEvent::TurnEnd { .. } => {
                    assert!(turn, "a turn ends without a start");
                    assert!(!message, "the turn leaves an open message");
                    turn = false;
                }
                _ => {}
            }
        }
        assert!(!turn && !message, "the operation leaves an open turn/message");
    }
}

impl EventSink for Probe {
    fn emit(&mut self, e: &AgentEvent) {
        match e {
            AgentEvent::AgentSettled { .. } => self.busy_at_settle.push(self.handle.is_busy()),
            AgentEvent::AgentStart if self.abort_on_start => self.handle.abort(),
            AgentEvent::Notice { text, .. }
                if text == "thinking used the whole output budget; answered without thinking" =>
            {
                if let Some(reason) = self.wrap_on_overrun.take() {
                    self.handle.request_wrap_up(reason);
                }
            }
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
    wrap: Mutex<Option<(Handle, oxa_core::WrapUpReason, HookPoint)>>,
    wrap_skip: Mutex<usize>,
    points: Mutex<Vec<HookPoint>>,
}

impl Hooks for Trigger {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        self.points.lock().unwrap().push(p);
        let mut wrap = self.wrap.lock().unwrap();
        if wrap.as_ref().is_some_and(|(_, _, point)| *point == p) {
            let mut skip = self.wrap_skip.lock().unwrap();
            if *skip > 0 {
                *skip -= 1;
            } else if let Some((handle, reason, _)) = wrap.take() {
                handle.request_wrap_up(reason);
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

#[test]
fn wrap_up_requested_inside_before_settle_gets_a_final_turn() {
    for reason in [oxa_core::WrapUpReason::TimeLimit, oxa_core::WrapUpReason::Signal] {
        for operation in ["prompt", "queued", "command"] {
            let env = TestEnv::new();
            let hook = Arc::new(Trigger::default());
            let engine = Arc::new(ScriptedEngine::new(vec![
                ScriptTurn::text("initial"),
                ScriptTurn::text("final summary"),
            ]));
            let mut s = Session::open(deps(&env, engine.clone(), vec![hook.clone()]), Open::New).unwrap();
            *hook.wrap.lock().unwrap() = Some((s.handle(), reason, HookPoint::AgentBeforeSettle));
            let mut probe = Probe::new(s.handle());
            match operation {
                "prompt" => s
                    .prompt("go".into(), vec![], &mut probe, &mut HeadlessUi::default())
                    .unwrap(),
                "queued" => {
                    s.handle().follow_up("go".into());
                    s.run_queued(&mut probe, &mut HeadlessUi::default()).unwrap();
                }
                _ => s
                    .run_command("go", "", &mut probe, &mut HeadlessUi::default())
                    .unwrap(),
            }
            assert_eq!(
                engine.requests().len(),
                2,
                "{reason:?}/{operation}: missing final turn"
            );
            assert_eq!(*s.outcome(), oxa_core::RunOutcome::Interrupted);
            probe.assert_settled_idle(1);
            assert!(matches!(
                probe.events.last(),
                Some(AgentEvent::AgentSettled { aborted: true })
            ));
        }
    }
}

#[test]
fn wrap_up_replaces_the_normal_thinking_fallback_with_a_final_attempt() {
    check_thinking_overrun_wrap_up(1, false);
}

#[test]
fn wrap_up_after_an_exhausted_thinking_fallback_gets_a_final_attempt() {
    check_thinking_overrun_wrap_up(2, false);
}

#[test]
fn wrap_up_from_the_thinking_fallback_notice_gets_a_final_attempt() {
    check_thinking_overrun_wrap_up(1, true);
}

fn check_thinking_overrun_wrap_up(stop_request: usize, from_notice: bool) {
    use oxa_core::{RunOutcome, WrapUpReason};
    for reason in [WrapUpReason::TimeLimit, WrapUpReason::Signal] {
        for final_fails in [false, true] {
            let env = TestEnv::new();
            let hook = Arc::new(Trigger::default());
            let mut thinking = ScriptTurn::text("");
            thinking.text = None;
            thinking.reasoning = Some("considering the task".into());
            thinking.finish = oxa_proto::Finish::Length;
            let mut turns = vec![thinking.clone()];
            if stop_request == 2 {
                thinking.reasoning = None;
                turns.push(thinking);
            }
            turns.push(if final_fails {
                ScriptTurn::fault(oxa_fake::Fault::Refuse)
            } else {
                ScriptTurn::text("final summary")
            });
            turns.push(ScriptTurn::text("unexpected extra request"));
            let engine = Arc::new(ScriptedEngine::new(turns));
            let mut d = deps(&env, engine.clone(), vec![hook.clone()]);
            d.thinking = Some(oxa_proto::ThinkingLevel::Medium);
            d.settings = Settings::from_values(
                serde_json::json!({"retry":{
                    "enabled":true,"maxRetries":2,"baseDelayMs":1,"maxAgentDelayMs":5
                }}),
                serde_json::json!({}),
            );
            let mut s = Session::open(d, Open::New).unwrap();
            let mut probe = Probe::new(s.handle());
            if from_notice {
                probe.wrap_on_overrun = Some(reason);
            } else {
                *hook.wrap.lock().unwrap() = Some((s.handle(), reason, HookPoint::AfterProviderResponse));
                *hook.wrap_skip.lock().unwrap() = stop_request - 1;
            }
            s.prompt("go".into(), vec![], &mut probe, &mut HeadlessUi::default())
                .unwrap();

            let requests = engine.requests();
            assert_eq!(
                requests.len(),
                stop_request + 1,
                "{reason:?}/{final_fails}: one final attempt"
            );
            assert!(requests[0].thinking);
            let final_request = requests.last().unwrap();
            assert!(!final_request.thinking);
            assert!(final_request.max_tokens <= 1024);
            assert_ne!(
                final_request.messages, requests[0].messages,
                "final instruction replaces the normal fallback"
            );
            assert_ne!(final_request.idem_key, requests[stop_request - 1].idem_key);
            let expected = if stop_request == 2 {
                RunOutcome::Failed("thinking_overrun")
            } else if final_fails {
                RunOutcome::Failed(oxa_proto::codes::ENGINE_DOWN)
            } else {
                RunOutcome::Interrupted
            };
            assert_eq!(*s.outcome(), expected);
            if !final_fails {
                assert_eq!(
                    s.inner().stats.requests as usize,
                    requests.len(),
                    "discarded requests remain accounted"
                );
                assert!(probe.events.iter().any(|event| matches!(event,
                    AgentEvent::MessageEnd { message } if message.msg.content.as_deref() == Some("final summary"))));
            }
            assert!(!probe
                .events
                .iter()
                .any(|event| matches!(event, AgentEvent::AutoRetryStart { .. })));
            probe.assert_settled_idle(1);
            probe.assert_balanced();
        }
    }
}

#[test]
fn wrap_up_before_the_request_closes_started_events() {
    for reason in [oxa_core::WrapUpReason::TimeLimit, oxa_core::WrapUpReason::Signal] {
        for point in [HookPoint::TurnStart, HookPoint::MessageStart] {
            let env = TestEnv::new();
            let hook = Arc::new(Trigger::default());
            let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("final summary")]));
            let mut s = Session::open(deps(&env, engine.clone(), vec![hook.clone()]), Open::New).unwrap();
            *hook.wrap.lock().unwrap() = Some((s.handle(), reason, point));
            let mut probe = Probe::new(s.handle());
            s.prompt("go".into(), vec![], &mut probe, &mut HeadlessUi::default())
                .unwrap();
            let requests = engine.requests();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].max_tokens <= 1024);
            assert!(!requests[0].thinking);
            probe.assert_balanced();
            let points = hook.points.lock().unwrap();
            for (start, end) in [
                (HookPoint::TurnStart, HookPoint::TurnEnd),
                (HookPoint::MessageStart, HookPoint::MessageEnd),
            ] {
                assert_eq!(
                    points.iter().filter(|p| **p == start).count(),
                    points.iter().filter(|p| **p == end).count(),
                    "{point:?}/{start:?}"
                );
            }
        }
    }
}

#[test]
fn wrap_up_yields_overflow_recovery_before_compaction_or_resending() {
    use oxa_core::{RunOutcome, WrapUpReason};
    for reason in [WrapUpReason::TimeLimit, WrapUpReason::Signal] {
        for after_compaction in [false, true] {
            for final_fails in [false, true] {
                let env = TestEnv::new();
                let hook = Arc::new(Trigger::default());
                let mut turns = vec![
                    ScriptTurn::text("earlier answer"),
                    ScriptTurn::fault(oxa_fake::Fault::Overflow),
                ];
                if after_compaction {
                    turns.push(ScriptTurn::text("summary of the earlier exchange"));
                }
                turns.push(if final_fails {
                    ScriptTurn::fault(oxa_fake::Fault::Refuse)
                } else {
                    ScriptTurn::text("final summary")
                });
                turns.push(ScriptTurn::text("unexpected resend"));
                let engine = Arc::new(ScriptedEngine::new(turns));
                let mut d = deps(&env, engine.clone(), vec![hook.clone()]);
                d.settings = Settings::from_values(
                    serde_json::json!({
                        "compaction":{"keepRecentTokens":1},
                        "retry":{"enabled":true,"maxRetries":2,"baseDelayMs":1,"maxAgentDelayMs":5}
                    }),
                    serde_json::json!({}),
                );
                let mut s = Session::open(d, Open::New).unwrap();
                s.prompt(
                    "earlier question".into(),
                    vec![],
                    &mut oxa_proto::VecSink::default(),
                    &mut HeadlessUi::default(),
                )
                .unwrap();
                *hook.wrap.lock().unwrap() = Some((
                    s.handle(),
                    reason,
                    if after_compaction {
                        HookPoint::SessionCompact
                    } else {
                        HookPoint::BeforeProviderRequest
                    },
                ));
                let mut probe = Probe::new(s.handle());
                s.prompt("go".into(), vec![], &mut probe, &mut HeadlessUi::default())
                    .unwrap();
                let requests = engine.requests();
                assert_eq!(
                    requests.len(),
                    if after_compaction { 4 } else { 3 },
                    "{reason:?}/{after_compaction}/{final_fails}: one final attempt"
                );
                assert_eq!(
                    requests.iter().filter(|r| r.kind == "compaction").count(),
                    usize::from(after_compaction)
                );
                let final_request = requests.last().unwrap();
                assert_eq!(final_request.kind, "chat");
                assert!(final_request.max_tokens <= 1024);
                assert!(!final_request.thinking);
                assert_eq!(
                    *s.outcome(),
                    RunOutcome::Failed(oxa_proto::codes::CONTEXT_OVERFLOW)
                );
                probe.assert_settled_idle(1);
                probe.assert_balanced();
            }
        }
    }
}

#[test]
fn a_pending_wrap_up_stops_stream_replays_before_and_during_the_final_turn() {
    use oxa_core::{RunOutcome, WrapUpReason};
    for reason in [WrapUpReason::TimeLimit, WrapUpReason::Signal] {
        for point in [HookPoint::BeforeProviderRequest, HookPoint::AgentBeforeSettle] {
            let env = TestEnv::new();
            let hook = Arc::new(Trigger::default());
            let mut cut = ScriptTurn::text("PARTIAL_REPLY");
            cut.chunk = 1;
            cut.fault = Some(oxa_fake::Fault::DropAt { bytes: 5 });
            let turns = if point == HookPoint::BeforeProviderRequest {
                vec![
                    cut,
                    ScriptTurn::text("final summary"),
                    ScriptTurn::text("unexpected replay"),
                ]
            } else {
                vec![
                    ScriptTurn::text("initial"),
                    cut,
                    ScriptTurn::text("unexpected replay"),
                ]
            };
            let engine = Arc::new(ScriptedEngine::new(turns));
            let mut d = deps(&env, engine.clone(), vec![hook.clone()]);
            d.settings = Settings::from_values(
                serde_json::json!({"retry":{"enabled":true}}),
                serde_json::json!({}),
            );
            let mut s = Session::open(d, Open::New).unwrap();
            *hook.wrap.lock().unwrap() = Some((s.handle(), reason, point));
            let mut probe = Probe::new(s.handle());
            s.prompt("go".into(), vec![], &mut probe, &mut HeadlessUi::default())
                .unwrap();

            let requests = engine.requests();
            assert_eq!(requests.len(), 2, "{reason:?}/{point:?}: one final attempt");
            assert_eq!(*s.outcome(), RunOutcome::Failed(oxa_proto::codes::STREAM_BROKEN));
            assert!(requests[1].max_tokens <= 1024);
            assert!(!requests[1].thinking);
            assert_ne!(requests[0].idem_key, requests[1].idem_key);
            probe.assert_settled_idle(1);
        }
    }
}

#[test]
fn wrap_up_preserves_request_errors_before_and_during_the_final_turn() {
    use oxa_core::{RunOutcome, WrapUpReason};
    use oxa_fake::{FakeOpts, FakeServer, Fault};
    for reason in [WrapUpReason::TimeLimit, WrapUpReason::Signal] {
        for (fault, expected) in [
            (
                Fault::Http {
                    status: 429,
                    body: r#"{"error":{"code":"budget_exceeded","message":"Spent"}}"#.into(),
                },
                RunOutcome::Blocked(oxa_proto::codes::BUDGET_EXHAUSTED),
            ),
            (
                Fault::Http {
                    status: 503,
                    body: r#"{"error":{"type":"cloud_error","code":"cloud_credentials_unavailable","message":"Unavailable"}}"#
                        .into(),
                },
                RunOutcome::Blocked("cloud_credentials_unavailable"),
            ),
            (
                Fault::Http {
                    status: 503,
                    body: r#"{"error":{"type":"cloud_error","code":"cloud_ledger_unavailable","message":"Unavailable"}}"#.into(),
                },
                RunOutcome::Blocked("cloud_ledger_unavailable"),
            ),
            (
                Fault::DropAt { bytes: 1 },
                RunOutcome::Failed(oxa_proto::codes::STREAM_BROKEN),
            ),
        ] {
            for final_fails in [false, true] {
                let env = TestEnv::new();
                let hook = Arc::new(Trigger::default());
                let final_turn = if final_fails {
                    ScriptTurn::fault(Fault::Refuse)
                } else {
                    ScriptTurn::text("final summary")
                };
                let server = FakeServer::start(
                    vec![ScriptTurn::fault(fault.clone()), final_turn],
                    FakeOpts::default(),
                )
                .unwrap();
                let engine = Arc::new(
                    oxa_engine::HttpEngine::new(&server.url(), None)
                        .unwrap()
                        .with_opts(oxa_engine::StreamOpts::default(), 0),
                );
                let mut d = deps(&env, engine.clone(), vec![hook.clone()]);
                d.settings = Settings::from_values(
                    serde_json::json!({"retry":{"enabled":false}}),
                    serde_json::json!({}),
                );
                let mut s = Session::open(d, Open::New).unwrap();
                *hook.wrap.lock().unwrap() = Some((s.handle(), reason, HookPoint::BeforeProviderRequest));
                let mut probe = Probe::new(s.handle());
                s.prompt("go".into(), vec![], &mut probe, &mut HeadlessUi::default())
                    .unwrap();
                assert_eq!(
                    *s.outcome(),
                    expected,
                    "{reason:?}/{fault:?}/final_fails={final_fails}"
                );
                assert_eq!(server.requests().len(), 2, "one final attempt");
                probe.assert_settled_idle(1);
            }
        }
        let env = TestEnv::new();
        let hook = Arc::new(Trigger::default());
        let server = FakeServer::start(
            vec![
                ScriptTurn::text("initial"),
                ScriptTurn::fault(Fault::Http {
                    status: 429,
                    body: r#"{"error":{"code":"budget_exceeded","message":"Spent"}}"#.into(),
                }),
            ],
            FakeOpts::default(),
        )
        .unwrap();
        let engine = Arc::new(
            oxa_engine::HttpEngine::new(&server.url(), None)
                .unwrap()
                .with_opts(oxa_engine::StreamOpts::default(), 0),
        );
        let mut s = Session::open(deps(&env, engine.clone(), vec![hook.clone()]), Open::New).unwrap();
        *hook.wrap.lock().unwrap() = Some((s.handle(), reason, HookPoint::AgentBeforeSettle));
        let mut probe = Probe::new(s.handle());
        s.prompt("go".into(), vec![], &mut probe, &mut HeadlessUi::default())
            .unwrap();
        assert_eq!(
            *s.outcome(),
            RunOutcome::Blocked(oxa_proto::codes::BUDGET_EXHAUSTED)
        );
        assert_eq!(server.requests().len(), 2);
    }
}
