//! Wave H lane `harden` (pre-flight A1/A2/B9): the release binary unwinds, and a panic at a
//! boundary where third-party code runs is contained there. Each test injects a panic at one
//! boundary and asserts the user sees a clear error and the session goes on.
//!
//! Boundaries covered here: a tool (serial path, serial path with a host, parallel batch), a
//! hook set's `fire` / `attach` / command handler, and a panic that escapes a run (injected in
//! the front end's event sink, i.e. a bug in oxa itself): the busy flag is released and the
//! unanswered call is closed so the next request is well-formed.
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, tool_errors, AgentEvent, CommandSpec, EventSink, Exposure, HeadlessUi, HookPoint, HookReply,
    Hooks, Host, OxaError, Role, Source, Tool, ToolCall, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value,
    VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// `boom` panics; `fine` answers "fine".
struct TestTool {
    spec: ToolSpec,
}

fn tool(name: &str) -> Arc<dyn Tool> {
    Arc::new(TestTool {
        spec: ToolSpec {
            name: name.to_string(),
            description: "test tool".to_string(),
            schema_json: "{}".to_string(),
            class: ToolClass::Read,
            exposure: Exposure::Direct,
            source: Source::Builtin,
        },
    })
}

impl Tool for TestTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<oxa_proto::Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        if self.spec.name == "boom" {
            panic!("injected tool panic");
        }
        ToolOutput {
            images: Vec::new(),
            text: "fine".into(),
            is_error: false,
            ui: Value::Null,
        }
    }
}

/// A hook set that panics where it is told to.
#[derive(Default)]
struct PanicHooks {
    on_turn_start: AtomicBool,
    on_attach: AtomicBool,
    fired: AtomicUsize,
}

impl Hooks for PanicHooks {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        self.fired.fetch_add(1, Ordering::SeqCst);
        if p == HookPoint::TurnStart && self.on_turn_start.swap(false, Ordering::SeqCst) {
            panic!("injected hook panic");
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: "explode".into(),
            description: "panics".into(),
            argument_hint: None,
            source: Source::Extension("panic-hooks.ts".into()),
        }]
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        panic!("injected command panic");
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn name(&self) -> &str {
        "panic-hooks.ts"
    }
    fn attach(&self, _host: Arc<dyn Host>) {
        if self.on_attach.swap(false, Ordering::SeqCst) {
            panic!("injected attach panic");
        }
    }
}

fn open(env: &TestEnv, engine: Arc<ScriptedEngine>, hooks: Vec<Arc<dyn Hooks>>) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let deps = Deps {
        engine,
        store,
        tools: vec![tool("boom"), tool("fine")],
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
    };
    Session::open(deps, Open::New).expect("open")
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args_raw: "{}".into(),
    }
}

/// The tool results of the last recorded request, as (call id, text).
fn results(engine: &ScriptedEngine) -> Vec<(String, String)> {
    let reqs = engine.requests();
    let last = reqs.last().expect("a request");
    last.messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| {
            (
                m.tool_call_id.clone().unwrap_or_default(),
                m.content.clone().unwrap_or_default(),
            )
        })
        .collect()
}

fn panicked_text() -> String {
    tool_errors::PANICKED
        .replace("{name}", "boom")
        .replace("{error}", "injected tool panic")
}

fn assert_idle_and_usable(s: &mut Session, engine: &ScriptedEngine, sink: &mut VecSink) {
    assert!(
        !s.handle().is_busy(),
        "a contained panic must not leave the session busy"
    );
    engine.push(ScriptTurn::text("still here"));
    let mut ui = HeadlessUi::default();
    s.prompt("again".into(), Vec::new(), sink, &mut ui)
        .expect("the session goes on after the panic");
}

#[test]
fn a_panicking_tool_is_an_error_result_and_the_turn_goes_on() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", "boom", "{}"),
        ScriptTurn::text("after the panic"),
    ]));
    let mut sink = VecSink(Vec::new());
    let mut s = open(&env, engine.clone(), Vec::new());
    let mut ui = HeadlessUi::default();
    s.prompt("go".into(), Vec::new(), &mut sink, &mut ui)
        .expect("the run completes");
    assert_eq!(results(&engine), vec![("c1".to_string(), panicked_text())]);
    let ended = sink.0.iter().any(|e| {
        matches!(e, AgentEvent::ToolExecutionEnd { tool_call_id, result, .. } if tool_call_id == "c1" && result.is_error)
    });
    assert!(ended, "the call ends as an error: {:?}", sink.0);
    assert_idle_and_usable(&mut s, &engine, &mut sink);
}

#[test]
fn a_panicking_tool_in_a_parallel_batch_fails_alone() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let two = ScriptTurn {
        tool_calls: vec![call("c1", "fine"), call("c2", "boom"), call("c3", "fine")],
        ..ScriptTurn::tool("c1", "fine", "{}")
    };
    let engine = Arc::new(ScriptedEngine::new(vec![two, ScriptTurn::text("done")]));
    let mut sink = VecSink(Vec::new());
    let mut s = open(&env, engine.clone(), Vec::new());
    let mut ui = HeadlessUi::default();
    s.prompt("go".into(), Vec::new(), &mut sink, &mut ui)
        .expect("the run completes");
    assert_eq!(
        results(&engine),
        vec![
            ("c1".to_string(), "fine".to_string()),
            ("c2".to_string(), panicked_text()),
            ("c3".to_string(), "fine".to_string()),
        ]
    );
    assert_idle_and_usable(&mut s, &engine, &mut sink);
}

#[test]
fn a_panicking_tool_beside_a_hook_set_is_contained_on_the_helper_thread() {
    // With a hook set the tool runs on a helper thread while the session thread serves the
    // host: the panic is contained on that thread.
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", "boom", "{}"),
        ScriptTurn::text("after the panic"),
    ]));
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(PanicHooks::default())];
    let mut sink = VecSink(Vec::new());
    let mut s = open(&env, engine.clone(), hooks);
    let mut ui = HeadlessUi::default();
    s.prompt("go".into(), Vec::new(), &mut sink, &mut ui)
        .expect("the run completes");
    assert_eq!(results(&engine), vec![("c1".to_string(), panicked_text())]);
    assert_idle_and_usable(&mut s, &engine, &mut sink);
}

fn extension_errors(sink: &VecSink) -> Vec<(String, String, String)> {
    sink.0
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ExtensionError {
                extension_path,
                event,
                error,
            } => Some((extension_path.clone(), event.clone(), error.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_panicking_hook_set_is_an_extension_error_and_the_run_goes_on() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("answer")]));
    let hooks = Arc::new(PanicHooks::default());
    hooks.on_turn_start.store(true, Ordering::SeqCst);
    hooks.on_attach.store(true, Ordering::SeqCst);
    let mut sink = VecSink(Vec::new());
    let mut s = open(&env, engine.clone(), vec![hooks.clone() as Arc<dyn Hooks>]);
    let mut ui = HeadlessUi::default();
    s.prompt("go".into(), Vec::new(), &mut sink, &mut ui)
        .expect("the run completes despite the hook panic");
    let errors = extension_errors(&sink);
    assert!(
        errors.iter().any(|(path, event, error)| path == "panic-hooks.ts"
            && event == HookPoint::TurnStart.as_str()
            && error.contains("injected hook panic")),
        "one extension_error names the hook set, the point and the panic: {errors:?}"
    );
    assert!(hooks.fired.load(Ordering::SeqCst) > 1, "later points still fire");
    let reqs = engine.requests();
    assert_eq!(reqs.len(), 1, "the model request went out");
    assert_idle_and_usable(&mut s, &engine, &mut sink);
}

#[test]
fn a_panicking_command_handler_fails_that_command_only() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(PanicHooks::default())];
    let mut sink = VecSink(Vec::new());
    let mut s = open(&env, engine.clone(), hooks);
    let mut ui = HeadlessUi::default();
    let e = s
        .run_command("explode", "", &mut sink, &mut ui)
        .expect_err("the command fails");
    assert_eq!(e.code, codes::INTERNAL);
    assert!(e.message.contains("injected command panic"), "{e}");
    assert_idle_and_usable(&mut s, &engine, &mut sink);
}

/// Panics on the first `tool_execution_start` it sees: a bug in oxa itself, mid-turn, after
/// the assistant message with the call is stored and before its result exists.
struct PanicSink {
    inner: VecSink,
    armed: bool,
}

impl EventSink for PanicSink {
    fn emit(&mut self, e: &AgentEvent) {
        if self.armed && matches!(e, AgentEvent::ToolExecutionStart { .. }) {
            self.armed = false;
            panic!("injected sink panic");
        }
        self.inner.emit(e);
    }
}

#[test]
fn a_panic_that_escapes_a_run_releases_the_session_and_closes_the_call() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::tool("c1", "fine", "{}")]));
    let mut s = open(&env, engine.clone(), Vec::new());
    let mut sink = PanicSink {
        inner: VecSink(Vec::new()),
        armed: true,
    };
    let mut ui = HeadlessUi::default();
    let e = s
        .prompt("go".into(), Vec::new(), &mut sink, &mut ui)
        .expect_err("the run reports the panic");
    assert_eq!(e.code, codes::INTERNAL);
    assert!(
        e.message.contains("injected sink panic") && e.message.contains("session continues"),
        "{e}"
    );
    assert!(!s.handle().is_busy(), "busy is released");
    // The next request carries the interrupted result for the unanswered call (well-formed).
    engine.push(ScriptTurn::text("recovered"));
    s.prompt("again".into(), Vec::new(), &mut sink, &mut ui)
        .expect("the session goes on");
    assert_eq!(
        results(&engine),
        vec![("c1".to_string(), oxa_core::prompt::TOOL_INTERRUPTED.to_string())]
    );
}
