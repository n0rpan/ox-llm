//! #1844 PR 2: fork-style subagents on a local model.
//!
//! A local child must not cost the parent its cache. This drives the real
//! [`oxa_core::Session::adopt_fork`] and the host's fork head over the scripted engine, which
//! holds the same prefix-cache model as the HTTP fake: a request is served from what the engine
//! holds for its session, and at least the frozen anchor (system text + tools) when any session
//! holds the same anchor.
//!
//! The child's request carries the parent's frozen prefix and the parent's wire with the task
//! appended, so the engine continues the parent's state instead of evicting it; the parent's own
//! next turn still reuses its context. (`builtin:subagent`'s decision to fork, and to park
//! nothing, is tested in `e11_builtin_subagent.rs`.)

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, CommandSpec, Engine, Exposure, HeadlessUi, HookPoint, HookReply, Hooks, Host, HostAction,
    OxaError, Role, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::{Arc, Mutex};

/// A hook set with one tool that asks the session for its fork head, as `subagent` does before a
/// local child, and keeps the answer.
#[derive(Default)]
struct Seeder {
    host: Arc<Mutex<Option<Arc<dyn Host>>>>,
    seed: Arc<Mutex<Option<Value>>>,
}

struct SeedTool {
    spec: ToolSpec,
    host: Arc<Mutex<Option<Arc<dyn Host>>>>,
    seed: Arc<Mutex<Option<Value>>>,
}

impl Tool for SeedTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let host = self.host.lock().unwrap().clone().expect("attached");
        let seed = host.call(HostAction::ForkSeed).expect("the session's head");
        *self.seed.lock().unwrap() = Some(seed);
        ToolOutput::ok("delegated")
    }
}

impl Hooks for Seeder {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(SeedTool {
            spec: ToolSpec {
                name: "delegate".to_string(),
                description: "test tool".to_string(),
                schema_json: "{}".to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Builtin,
            },
            host: self.host.clone(),
            seed: self.seed.clone(),
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

fn deps(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Vec<Arc<dyn Hooks>>) -> Deps {
    let dirs = Dirs::under(env.root());
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks,
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
        jobs: oxa_core::fresh_jobs(),
    }
}

#[test]
fn a_forked_child_continues_the_parents_prefix() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", "delegate", "{}"),
        ScriptTurn::text("parent done"),
        ScriptTurn::text("child answer"),
        ScriptTurn::text("parent again"),
    ]));
    let seeder = Arc::new(Seeder::default());
    let mut parent =
        Session::open(deps(&env, eng.clone(), vec![seeder.clone()]), Open::New).expect("open the parent");
    let mut sink = VecSink::default();
    parent
        .prompt(
            "delegate this".to_string(),
            Vec::new(),
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .expect("the parent's prompt");
    let seed = oxa_core::ForkSeed::from_host(
        &seeder
            .seed
            .lock()
            .unwrap()
            .clone()
            .expect("the tool asked for the head"),
    )
    .expect("a fork seed");
    assert!(
        seed.entries.contains("\"type\":\"message\""),
        "the head holds the session's entries: {}",
        seed.entries
    );

    // The child is a session of its own (the child binary, `--no-session`), seeded with the
    // parent's head before its first prompt.
    let mut child = Session::open(deps(&env, eng.clone(), Vec::new()), Open::New).expect("open the child");
    child
        .adopt_fork(&seed, &mut sink)
        .expect("adopt the parent's head");
    child
        .prompt(
            "Task: find the files".to_string(),
            Vec::new(),
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .expect("the child's prompt");

    let reqs: Vec<Recorded> = eng.requests();
    assert_eq!(reqs.len(), 3, "the parent's two turns and the child's");
    let (parent_req, child_req) = (&reqs[0], &reqs[2]);

    // The child continues the parent's prompt: the same frozen prefix, the parent's wire first.
    assert_eq!(child_req.system, parent_req.system, "the parent's system text");
    assert_eq!(
        child_req.tools_json, parent_req.tools_json,
        "the parent's tool list"
    );
    assert_eq!(
        child_req.messages[..parent_req.messages.len()],
        parent_req.messages[..],
        "the parent's wire is the child's first messages"
    );
    assert!(
        child_req.messages.len() > parent_req.messages.len(),
        "the task is appended to it"
    );
    let task = child_req.messages.last().expect("a last message");
    assert_eq!(task.role, Role::User);
    assert!(
        task.content
            .as_deref()
            .unwrap_or("")
            .contains("Task: find the files"),
        "{:?}",
        task.content
    );
    // The engine's prefix cache holds the parent's anchor, so the child's first request is not
    // cold (before the fork it prefilled its own system text and tools from zero).
    let anchor = parent_req.system.len() + parent_req.tools_json.len() + 2;
    let child_cached = child_req.usage.expect("the child's usage").cached as usize;
    assert!(
        child_cached >= anchor,
        "the child reuses the parent's frozen anchor ({child_cached} < {anchor})"
    );

    // The parent's next turn, after the child, reuses the context it had: the child took
    // nothing away from it.
    parent
        .prompt(
            "one more thing".to_string(),
            Vec::new(),
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .expect("the parent's next prompt");
    let reqs: Vec<Recorded> = eng.requests();
    let next = reqs.last().expect("the parent's next request");
    let before = reqs[1].usage.expect("the parent's usage before the child");
    let cached = next.usage.expect("the parent's next usage").cached;
    assert!(
        cached >= before.prompt,
        "the parent's next turn reuses its whole context ({cached} < {})",
        before.prompt
    );
}
