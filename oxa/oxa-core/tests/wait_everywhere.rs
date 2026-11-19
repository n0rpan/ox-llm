//! `wait` is declared in every session (owner decision 2026-11-19), not only while a goal is
//! pursued. Offline refusal of the network probes is covered where the probes live
//! (`oxa-cli` `gh_probe::probe_refused_offline`).
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, CancelToken, EventSink, ExecResult, ExecSpec, HeadlessUi, OxaError, Sandbox, VecSink};
use oxa_sandbox::run_unsandboxed_capture;
use oxa_session::MemStore;
use oxa_tools::JobRegistry;
use std::sync::Arc;

struct Plain;
impl Sandbox for Plain {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        run_unsandboxed_capture(s, c, live, true)
    }
}

fn open(env: &TestEnv, engine: Arc<ScriptedEngine>, jobs: Arc<JobRegistry>, settings: serde_json::Value) -> Session {
    let deps = Deps {
        engine,
        store: Arc::new(MemStore::new()),
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(Plain),
        settings: Settings::from_values(settings, serde_json::json!({})),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs,
    };
    Session::open(deps, Open::New).expect("open")
}

fn plain() -> serde_json::Value {
    serde_json::json!({"oxa": {"envNote": false}})
}

#[test]
fn a_session_without_a_goal_declares_wait_and_the_opt_out_removes_it() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![]));
    let s = open(&env, engine.clone(), Arc::new(JobRegistry::new()), plain());
    assert!(s.inner().active.get("wait").is_some());
    assert!(s.inner().active.get("goal_done").is_none());
    let off = open(
        &env,
        engine,
        Arc::new(JobRegistry::new()),
        serde_json::json!({"oxa": {"envNote": false, "tools": {"wait": false}}}),
    );
    assert!(off.inner().active.get("wait").is_none());
}

#[test]
fn the_prefix_is_identical_across_two_turns() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("one"), ScriptTurn::text("two")]));
    let mut s = open(&env, engine.clone(), Arc::new(JobRegistry::new()), plain());
    for text in ["first", "second"] {
        s.prompt(text.into(), vec![], &mut VecSink::default(), &mut HeadlessUi::default())
            .unwrap();
    }
    let seen = engine.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].system, seen[1].system);
    assert_eq!(seen[0].tools_json, seen[1].tools_json);
    assert!(seen[0].tools_json.contains("\"wait\""));
}

#[test]
fn wait_until_job_done_returns_when_the_job_finishes() {
    let env = TestEnv::new();
    let jobs = Arc::new(JobRegistry::new());
    jobs.set_sandbox(Arc::new(Plain));
    let id = jobs
        .spawn(ExecSpec {
            command: "sleep 1".into(),
            cwd: env.work(),
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        })
        .unwrap();
    let args = format!(r#"{{"seconds":60,"reason":"job","until":{{"kind":"job_done","job":"{id}"}}}}"#);
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("w", "wait", &args),
        ScriptTurn::text("done"),
    ]));
    let mut s = open(&env, engine.clone(), jobs, plain());
    let start = std::time::Instant::now();
    s.prompt("go".into(), vec![], &mut VecSink::default(), &mut HeadlessUi::default())
        .unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
    let result = engine.requests()[1]
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("w"))
        .and_then(|m| m.content.clone())
        .unwrap();
    assert!(result.contains(&format!("Job {id} finished")), "{result}");
}

#[test]
fn a_typed_message_interrupts_a_wait_outside_a_goal() {
    struct TypeDuringWait(oxa_core::Handle);
    impl EventSink for TypeDuringWait {
        fn emit(&mut self, event: &AgentEvent) {
            if matches!(event, AgentEvent::ToolExecutionUpdate { tool_name, .. } if tool_name == "wait") {
                self.0.follow_up("Synthetic owner message.".into());
            }
        }
    }
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("w", "wait", r#"{"seconds":3600,"reason":"Input wake test"}"#),
        ScriptTurn::text("Input received."),
        ScriptTurn::text("Handled."),
    ]));
    let mut s = open(&env, engine.clone(), Arc::new(JobRegistry::new()), plain());
    let start = std::time::Instant::now();
    s.prompt(
        "Start".into(),
        vec![],
        &mut TypeDuringWait(s.handle()),
        &mut HeadlessUi::default(),
    )
    .unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert!(engine.requests()[1]
        .messages
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("w")
            && m.content.as_deref() == Some("Wait ended: input arrived.")));
}
