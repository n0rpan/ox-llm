//! Wave H, lane cache, A5 (tiny mechanics A/B): invariants as tests.
//! (a) over a scripted session with tools, a boot preload, threshold compactions, idle
//!     prefills and a park (what a subagent call does), oxa sends nothing but main turns,
//!     summaries and the documented prefills: no title, tag or naming request;
//! (b) every background or side request carries an explicit bounded `max_tokens` and the
//!     background marker where it must (`oxa_fake::check_discipline`, generic over the
//!     recorded requests, with a negative control);
//! (c) a byte budget for the frozen prefix (system + tools) of a default session.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{check_discipline, Recorded, ScriptTurn, ScriptedEngine, TestEnv, SIDE_MAX_TOKENS};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, CommandSpec, Engine, Exposure, HeadlessUi, HookPoint, HookReply, Hooks, Host, HostAction,
    OxaError, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::{Arc, Mutex};

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: "test tool".to_string(),
        schema_json: "{}".to_string(),
        class: ToolClass::Read,
        exposure: Exposure::Direct,
        source: Source::Builtin,
    }
}

/// `delegate` parks the session through the host (as `subagent` does before a child).
#[derive(Default)]
struct Parker {
    host: Arc<Mutex<Option<Arc<dyn Host>>>>,
}

struct ParkTool {
    spec: ToolSpec,
    host: Arc<Mutex<Option<Arc<dyn Host>>>>,
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
        host.call(HostAction::ParkSession).expect("park");
        ToolOutput::ok("delegated")
    }
}

impl Hooks for Parker {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(ParkTool {
            spec: spec("delegate"),
            host: self.host.clone(),
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

struct Echo(ToolSpec);

impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        ToolOutput::ok("file contents")
    }
}

fn open(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Arc<dyn Hooks>>,
    settings: &str,
) -> Session {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, settings).unwrap();
    let settings = Settings::load(&dirs, &env.work(), false);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools,
            hooks,
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open")
}

#[test]
fn only_main_turns_summaries_and_documented_prefills() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", "peek", "{}"),
        ScriptTurn::text("read it"),
        ScriptTurn::text("SUMMARY ONE"),
        ScriptTurn::tool("c2", "delegate", "{}"),
        ScriptTurn::text("delegated it"),
        ScriptTurn::text("SUMMARY TWO"),
    ]));
    let engine: Arc<dyn Engine> = eng.clone();
    let mut s = open(
        &env,
        engine,
        vec![Arc::new(Echo(spec("peek")))],
        vec![Arc::new(Parker::default())],
        r#"{"compaction": {"keepRecentTokens": 1}, "oxa": {"compaction": {"softTokens": 10}}}"#,
    );
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert!(s.preload_prefix(&mut sink, &mut ui), "the boot preload ran");
    s.prompt("look at the file".to_string(), Vec::new(), &mut sink, &mut ui)
        .unwrap();
    s.prompt("hand it off".to_string(), Vec::new(), &mut sink, &mut ui)
        .unwrap();
    assert_eq!(
        eng.remaining(),
        0,
        "every scripted reply was asked for, and no other"
    );

    let reqs = eng.requests();
    let count = |k: &str| reqs.iter().filter(|r| r.kind == k).count();
    let kinds: Vec<(&str, u32, bool)> = reqs
        .iter()
        .map(|r| (r.kind.as_str(), r.max_tokens, r.background))
        .collect();
    assert_eq!(count("chat"), 4, "the four main turns only: {kinds:?}");
    // `local` turns on turn-start compaction, so the second prompt compacts once more.
    assert_eq!(count("compaction"), 3, "{kinds:?}");
    assert_eq!(
        reqs.len(),
        count("chat") + count("compaction") + count("prefill"),
        "{kinds:?}"
    );
    assert!(count("prefill") >= 2, "boot preload and park at least: {kinds:?}");
    let v = check_discipline(&reqs);
    assert!(v.is_empty(), "{v:?}\n{kinds:?}");
}

/// The negative control: each rule of `check_discipline` catches its own break.
#[test]
fn discipline_negative_controls() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let engine: Arc<dyn Engine> = eng.clone();
    let mut s = open(&env, engine, Vec::new(), Vec::new(), "{}");
    s.prompt(
        "hi".to_string(),
        Vec::new(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .unwrap();
    let base: Recorded = eng
        .requests()
        .into_iter()
        .find(|r| r.kind == "chat")
        .expect("chat");
    assert!(check_discipline(std::slice::from_ref(&base)).is_empty());
    let bad = |f: &dyn Fn(&mut Recorded)| {
        let mut r = base.clone();
        f(&mut r);
        check_discipline(&[r])
    };
    assert_eq!(bad(&|r| r.kind = "title".to_string())[0].rule, "A5a");
    assert_eq!(bad(&|r| r.background = true)[0].rule, "A5a");
    assert_eq!(
        bad(&|r| {
            r.kind = "prefill".to_string();
            r.background = true;
            r.max_tokens = 5;
        })[0]
            .rule,
        "A5b"
    );
    assert_eq!(
        bad(&|r| {
            r.kind = "compaction".to_string();
            r.max_tokens = SIDE_MAX_TOKENS + 1;
        })[0]
            .rule,
        "A5b"
    );
    assert_eq!(
        bad(&|r| {
            r.kind = "subagent".to_string();
            r.background = false;
        })[0]
            .rule,
        "A5b"
    );
}

/// The frozen prefix of a default session (the seven built-in tools, default settings and
/// resources, no user skills or context files): measured 7114 bytes on 2026-11-13 (system 2141,
/// tools 4973), budget ~10% above. The binary adds the identity block, the built-in skills index
/// and (O-146) the subagent tool on top. Raise the budget only
/// deliberately: every cold or post-rewrite prefill pays these bytes (the A/B: oxa 2.0K tokens
/// against another client's 5.6K).
const PREFIX_BUDGET_BYTES: usize = 7_800;

#[test]
fn default_prefix_stays_within_its_byte_budget() {
    let env = TestEnv::new();
    let engine: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(Vec::new()));
    let s = open(&env, engine, oxa_tools::builtin_tools(), Vec::new(), "{}");
    let p = &s.inner().prefix;
    let bytes = p.system_text().len() + p.tools_json.len();
    assert!(
        bytes <= PREFIX_BUDGET_BYTES,
        "the default frozen prefix is {bytes} bytes (system {} + tools {}), budget {PREFIX_BUDGET_BYTES}: \
         raise the budget only deliberately",
        p.system_text().len(),
        p.tools_json.len()
    );
}
