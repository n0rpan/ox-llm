//! Wave H (lane cli): the settings that oxa-core consumes at session open and while it runs:
//! `defaultModel`, `defaultThinkingLevel`, `modelThinkingLevels`, `codemode.mode: "only"`,
//! `retry.provider.*`, the shell tool environment (ENV-002/003), `shellCommandPrefix` /
//! `shellPath` for `!cmd`.
//! Isolation: `oxa_fake::TestEnv` and the in-process `ScriptedEngine`; nothing touches the
//! real home or the network.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Fault, Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, Access, AssistantTurn, CancelToken, ChatRequest, CommandSpec, Engine, EngineInfo, ExecResult,
    ExecSpec, Exposure, HeadlessUi, HookPoint, HookReply, Hooks, OxaError, Sandbox, Source, StreamEvent,
    ThinkingLevel, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_session::{FsStore, MemStore, SessionStore};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// The scripted engine with two models: `fake-model` (loaded) and `beta`.
struct TwoModels(Arc<ScriptedEngine>);

impl Engine for TwoModels {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let mut info = self.0.info(None)?;
        info.models = vec![info.model.clone(), "beta".to_string()];
        if let Some(t) = target {
            info.limits_for = t.to_string();
        }
        Ok(info)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.0.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        self.0.chat(r, cancel, on)
    }
}

/// A sandbox that records every spec it is asked to run and answers `ok`.
#[derive(Default)]
struct Capture(Mutex<Vec<ExecSpec>>);

impl Sandbox for Capture {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.0.lock().unwrap().push(s.clone());
        ExecResult {
            output: b"ok".to_vec(),
            exit: Some(0),
            timed_out: false,
        }
    }
}

/// A do-nothing tool with a name (a stand-in for the codemode tool of the hook set).
struct Named(ToolSpec);

impl Tool for Named {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        ToolOutput::ok("")
    }
}

fn named(name: &str) -> Arc<dyn Tool> {
    Arc::new(Named(ToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        schema_json: "{}".to_string(),
        class: ToolClass::Read,
        exposure: Exposure::Direct,
        source: Source::Extension("/x/ext.js".into()),
    }))
}

/// A hook set that only offers tools (as an extension would).
struct ToolSource(Vec<Arc<dyn Tool>>);

impl Hooks for ToolSource {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.clone()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

struct Opened {
    session: Session,
    engine: Arc<ScriptedEngine>,
    sandbox: Arc<Capture>,
}

fn open_with(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    settings: Value,
    extra_tools: Vec<Arc<dyn Tool>>,
    model: Option<&str>,
    thinking: Option<ThinkingLevel>,
    persistent: bool,
) -> Opened {
    let engine = Arc::new(ScriptedEngine::new(turns));
    let sandbox = Arc::new(Capture::default());
    let dirs = Dirs::under(env.root());
    let store: Arc<dyn SessionStore> = if persistent {
        Arc::new(FsStore::new(dirs.sessions_dir.clone()))
    } else {
        Arc::new(MemStore::new())
    };
    let hooks: Vec<Arc<dyn Hooks>> = if extra_tools.is_empty() {
        Vec::new()
    } else {
        vec![Arc::new(ToolSource(extra_tools))]
    };
    let deps = Deps {
        engine: Arc::new(TwoModels(engine.clone())),
        store,
        tools: oxa_tools::builtin_tools(),
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: sandbox.clone(),
        settings: Settings::from_values(settings, json!({})),
        dirs,
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: model.map(str::to_string),
        thinking,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    let session =
        Session::open(deps, if persistent { Open::New } else { Open::Ephemeral }).expect("open session");
    Opened {
        session,
        engine,
        sandbox,
    }
}

fn open(env: &TestEnv, turns: Vec<ScriptTurn>, settings: Value) -> Opened {
    open_with(env, turns, settings, Vec::new(), None, None, false)
}

fn prompt(o: &mut Opened, text: &str) {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    o.session
        .prompt(text.to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
}

fn first(o: &Opened) -> Recorded {
    o.engine.requests().into_iter().next().expect("one request")
}

fn tool_names(r: &Recorded) -> Vec<String> {
    let v: Value = serde_json::from_str(&r.tools_json).unwrap_or(Value::Null);
    let mut names: Vec<String> = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    t.pointer("/function/name")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn env_of(spec: &ExecSpec, key: &str) -> Option<String> {
    spec.env
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

#[test]
fn default_model_picks_an_engine_model_and_falls_back_when_unknown() {
    let env = TestEnv::new();
    let mut o = open(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "defaultModel": "beta" }),
    );
    prompt(&mut o, "hi");
    assert_eq!(
        first(&o).model,
        "beta",
        "defaultModel names the second engine model"
    );

    let env = TestEnv::new();
    let mut o = open(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "defaultModel": "nosuch" }),
    );
    prompt(&mut o, "hi");
    assert_eq!(
        first(&o).model,
        "fake-model",
        "an unknown defaultModel falls back"
    );

    // The CLI choice wins over the setting.
    let env = TestEnv::new();
    let mut o = open_with(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "defaultModel": "beta" }),
        Vec::new(),
        Some("fake-model"),
        None,
        false,
    );
    prompt(&mut o, "hi");
    assert_eq!(first(&o).model, "fake-model");
}

#[test]
fn startup_thinking_follows_the_settings_and_the_cli_wins() {
    // Default `medium`: thinking on the wire (W6).
    let env = TestEnv::new();
    let mut o = open(&env, vec![ScriptTurn::text("ok")], json!({}));
    prompt(&mut o, "hi");
    assert!(first(&o).thinking);
    assert_eq!(o.session.state().thinking, ThinkingLevel::Medium);

    let env = TestEnv::new();
    let mut o = open(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "defaultThinkingLevel": "off" }),
    );
    prompt(&mut o, "hi");
    assert!(!first(&o).thinking);

    // The per-model level of the model in use wins over the default.
    let env = TestEnv::new();
    let mut o = open(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "defaultModel": "beta", "defaultThinkingLevel": "off", "modelThinkingLevels": { "beta": "high" } }),
    );
    prompt(&mut o, "hi");
    assert!(first(&o).thinking);
    assert_eq!(o.session.state().thinking, ThinkingLevel::High);

    // `--thinking off` wins over every setting.
    let env = TestEnv::new();
    let mut o = open_with(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "defaultThinkingLevel": "high" }),
        Vec::new(),
        None,
        Some(ThinkingLevel::Off),
        false,
    );
    prompt(&mut o, "hi");
    assert!(!first(&o).thinking);
}

#[test]
fn codemode_only_hides_the_other_declared_tools() {
    let env = TestEnv::new();
    let mut o = open_with(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "codemode": { "mode": "only" } }),
        vec![named("codemode"), named("ext_tool")],
        None,
        None,
        false,
    );
    prompt(&mut o, "hi");
    assert_eq!(tool_names(&first(&o)), ["codemode"]);

    // `on` (the default) keeps them declared.
    let env = TestEnv::new();
    let mut o = open_with(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({}),
        vec![named("codemode"), named("ext_tool")],
        None,
        None,
        false,
    );
    prompt(&mut o, "hi");
    let names = tool_names(&first(&o));
    assert!(names.contains(&"read".to_string()) && names.contains(&"ext_tool".to_string()));

    // `only` without a declared codemode tool changes nothing.
    let env = TestEnv::new();
    let mut o = open(
        &env,
        vec![ScriptTurn::text("ok")],
        json!({ "codemode": { "mode": "only" } }),
    );
    prompt(&mut o, "hi");
    assert!(tool_names(&first(&o)).contains(&"read".to_string()));
}

fn http_503() -> ScriptTurn {
    ScriptTurn::fault(Fault::Http {
        status: 503,
        body: json!({ "error": { "message": "busy", "code": 503 } }).to_string(),
    })
}

#[test]
fn provider_retries_replay_an_http_failure_before_any_data() {
    let env = TestEnv::new();
    let mut o = open(
        &env,
        vec![http_503(), http_503(), ScriptTurn::text("ok")],
        json!({ "retry": { "maxRetries": 0, "baseDelayMs": 1, "provider": { "maxRetries": 2 } } }),
    );
    prompt(&mut o, "hi");
    assert_eq!(
        o.engine.requests().len(),
        3,
        "two provider retries after the first attempt"
    );

    // Without provider retries (and no agent retries) the first failure is final.
    let env = TestEnv::new();
    let mut o = open(
        &env,
        vec![http_503(), ScriptTurn::text("ok")],
        json!({ "retry": { "maxRetries": 0, "baseDelayMs": 1 } }),
    );
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let _ = o.session.prompt("hi".to_string(), Vec::new(), &mut sink, &mut ui);
    assert_eq!(o.engine.requests().len(), 1);

    // A cut after the first data byte is never replayed by the provider layer either.
    let env = TestEnv::new();
    let mut cut = ScriptTurn::text("a longer reply that is cut");
    cut.chunk = 1;
    cut.fault = Some(Fault::DropAt { bytes: 5 });
    let mut o = open(
        &env,
        vec![cut, ScriptTurn::text("ok")],
        json!({ "retry": { "maxRetries": 0, "baseDelayMs": 1, "provider": { "maxRetries": 2 } } }),
    );
    let _ = o.session.prompt("hi".to_string(), Vec::new(), &mut sink, &mut ui);
    assert_eq!(o.engine.requests().len(), 1);
}

#[test]
fn shell_tools_see_the_session_environment_and_user_bash_does_not() {
    let env = TestEnv::new();
    let call = ScriptTurn::tool("c1", "bash", r#"{"command":"echo hi"}"#);
    let mut o = open_with(
        &env,
        vec![call, ScriptTurn::text("done")],
        json!({ "defaultThinkingLevel": "high", "shellCommandPrefix": "set -e" }),
        Vec::new(),
        None,
        None,
        true,
    );
    prompt(&mut o, "run it");
    let specs = o.sandbox.0.lock().unwrap().clone();
    let tool = specs.first().expect("the bash tool ran");
    let state = o.session.state();
    assert_eq!(
        env_of(tool, "OXA_SESSION_ID").as_deref(),
        Some(state.session_id.as_str())
    );
    let file = env_of(tool, "OXA_SESSION_FILE").expect("a persistent session has a file");
    assert_eq!(Some(std::path::PathBuf::from(&file)), state.session_file);
    assert_eq!(env_of(tool, "OXA_PROVIDER").as_deref(), Some("oxe"));
    assert_eq!(env_of(tool, "OXA_MODEL").as_deref(), Some("fake-model"));
    assert_eq!(env_of(tool, "OXA_REASONING_LEVEL").as_deref(), Some("high"));
    // The tools of `Deps` are the plain built-ins here: the prefix is the CLI's wiring
    // (`builtin_tools_with_shell`), so the tool's command is unchanged.
    assert_eq!(tool.command, "echo hi");

    // A thinking change is seen by the next command (pi: resolved when each command starts).
    let mut sink = VecSink::default();
    o.session.set_thinking(ThinkingLevel::Low, &mut sink);
    o.engine
        .push(ScriptTurn::tool("c2", "bash", r#"{"command":"echo again"}"#));
    o.engine.push(ScriptTurn::text("done"));
    prompt(&mut o, "again");
    let specs = o.sandbox.0.lock().unwrap().clone();
    assert_eq!(env_of(&specs[1], "OXA_REASONING_LEVEL").as_deref(), Some("low"));
}

#[test]
fn ephemeral_sessions_have_no_session_file_variable() {
    let env = TestEnv::new();
    let call = ScriptTurn::tool("c1", "bash", r#"{"command":"echo hi"}"#);
    let mut o = open(&env, vec![call, ScriptTurn::text("done")], json!({}));
    prompt(&mut o, "run it");
    let specs = o.sandbox.0.lock().unwrap().clone();
    assert_eq!(env_of(&specs[0], "OXA_SESSION_FILE"), None);
    assert!(env_of(&specs[0], "OXA_SESSION_ID").is_some());
}

#[test]
fn user_bash_gets_the_prefix_but_not_the_session_variables() {
    let env = TestEnv::new();
    let mut o = open_with(
        &env,
        Vec::new(),
        json!({ "shellCommandPrefix": "export HPFX=prefixed" }),
        Vec::new(),
        None,
        None,
        true,
    );
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    o.session
        .user_bash("echo \"$HPFX:${OXA_MODEL-unset}\"", true, &mut sink, &mut ui)
        .expect("user bash");
    let st = o.session.inner();
    let output = st
        .tree
        .branch(st.tree.leaf())
        .into_iter()
        .find_map(|e| match &e.body {
            oxa_proto::EntryBody::BashExecution { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a bash execution entry");
    assert!(output.contains("prefixed:unset"), "{output}");
}

#[test]
fn oxa_mode_is_the_startup_permission_mode() {
    let env = TestEnv::new();
    let o = open(&env, Vec::new(), json!({ "oxa": { "mode": "plan" } }));
    assert_eq!(o.session.state().mode, oxa_proto::Mode::Plan);
    let env = TestEnv::new();
    let o = open(&env, Vec::new(), json!({}));
    assert_eq!(o.session.state().mode, oxa_proto::Mode::Workspace);
}
