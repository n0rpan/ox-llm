//! Wave H, lane cache (OXA-H-TINYLOOP 2): the parent's cache accounting around a subagent.
//!
//! On hardware the parent turn after a child reported `cached` = the tiny-loop driver's
//! `prompt + completion - 1` + 2. The driver's formula ignores the park: before the first child
//! the parent sends one H6 prefill over its wire, which ends with the assistant's tool-call
//! turn; H6 renders that turn CLOSED (`<|im_end|>` + `\n`: the F1 token that is never fed, plus
//! the newline), so the engine then holds `prompt + completion + 1` = F1 + 2. oxa records the
//! park's usage, so its own expectation for the parent's next request is the park's
//! `prompt_tokens`, exact. This test pins that: the expectation after the park is the park's
//! prompt (above the F1 value of the tool-call turn), and the parent's next request meets it
//! with no `cache_miss`. (The fake renders the closing as one byte, so there it is F1 + 1.)

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, AgentEvent, CommandSpec, Engine, Exposure, HeadlessUi, HookPoint, HookReply, Hooks, Host,
    HostAction, OxaError, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::{Arc, Mutex};

/// A hook set with one tool that parks the session (what `subagent` does before a child) and
/// records the park's answer and the expectation the transcript holds right after it.
#[derive(Default)]
struct Parker {
    host: Arc<Mutex<Option<Arc<dyn Host>>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

struct ParkTool {
    spec: ToolSpec,
    host: Arc<Mutex<Option<Arc<dyn Host>>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Tool for ParkTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let host = self.host.lock().unwrap().clone().expect("attached");
        let answer = host.call(HostAction::ParkSession).expect("park");
        self.seen.lock().unwrap().push(answer);
        ToolOutput::ok("the child found three files")
    }
}

impl Hooks for Parker {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(ParkTool {
            spec: ToolSpec {
                name: "delegate".to_string(),
                description: "test tool".to_string(),
                schema_json: "{}".to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Builtin,
            },
            host: self.host.clone(),
            seen: self.seen.clone(),
        })]
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
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
}

fn scenario(last: ScriptTurn) -> (Vec<oxa_fake::Recorded>, VecSink, Arc<Parker>) {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", "delegate", "{}"),
        last,
    ]));
    let engine: Arc<dyn Engine> = eng.clone();
    let hook = Arc::new(Parker::default());
    let dirs = Dirs::under(env.root());
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut s = Session::open(
        Deps {
            engine,
            store,
            tools: Vec::new(),
            hooks: vec![hook.clone()],
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings: Settings::load(&dirs, &env.work(), false),
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
        },
        Open::New,
    )
    .expect("open");
    let mut sink = VecSink::default();
    s.prompt(
        "delegate this".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");

    (eng.requests(), sink, hook)
}

#[test]
fn parent_expects_the_park_prompt_after_a_child() {
    let (reqs, sink, hook) = scenario(ScriptTurn::text("parent done"));
    let kinds: Vec<(&str, u32)> = reqs.iter().map(|r| (r.kind.as_str(), r.max_tokens)).collect();
    let call = &reqs[0];
    let park = reqs
        .iter()
        .find(|r| r.max_tokens == 0 && r.kind == "prefill")
        .unwrap_or_else(|| panic!("{kinds:?}"));
    let next = reqs
        .iter()
        .filter(|r| r.kind == "chat")
        .nth(1)
        .unwrap_or_else(|| panic!("{kinds:?}"));
    let u_call = call.usage.expect("usage");
    let f1 = u_call.prompt + u_call.completion - 1;
    let park_prompt = park.usage.expect("park usage").prompt;
    assert!(
        park_prompt > f1,
        "the park renders the tool-call turn closed: {park_prompt} > {f1}"
    );
    assert_eq!(
        hook.seen.lock().unwrap()[0]["promptTokens"],
        Value::from(park_prompt)
    );
    // The parent's next request: the engine holds the park's prompt, and oxa expected exactly it.
    assert_eq!(next.usage.expect("usage").cached, park_prompt);
    assert!(
        !sink.0.iter().any(|e| matches!(e, AgentEvent::CacheMiss { .. })),
        "no cache_miss: oxa's expectation after the park is the park's prompt"
    );
}

/// The expectation itself: the parent's next request served cold reports exactly the park's
/// `prompt_tokens` as expected (a 2-token-low expectation would show here).
#[test]
fn the_expectation_after_a_child_is_the_park_prompt() {
    let cold = ScriptTurn {
        fault: Some(Fault::Cold),
        ..ScriptTurn::text("parent done")
    };
    let (reqs, sink, _hook) = scenario(cold);
    let park_prompt = reqs
        .iter()
        .find(|r| r.max_tokens == 0)
        .and_then(|r| r.usage)
        .expect("park")
        .prompt;
    let misses: Vec<(u32, u32)> = sink
        .0
        .iter()
        .filter_map(|e| match e {
            AgentEvent::CacheMiss { expected, cached, .. } => Some((*expected, *cached)),
            _ => None,
        })
        .collect();
    assert_eq!(misses, vec![(park_prompt, 0)]);
}
