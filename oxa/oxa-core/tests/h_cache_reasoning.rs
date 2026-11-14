//! Wave H lane `harden` (pre-flight B6): the cache-miss rule with reasoning tokens.
//!
//! The engine's accounting (oxe `server/src/api.rs`, `generate.rs`, `echo.rs`): every generated
//! token is in `completion_tokens`, the reasoning included; the state after a turn is the prompt
//! plus every generated token but the last; `cached_tokens` is always reported. oxa echoes the
//! reasoning back (W2), so with a template that keeps it the next request continues the state
//! exactly (`prompt + completion - 1`). A template told to drop the reasoning of earlier turns
//! (`preserve_thinking: false`) re-renders the history at the next user message from the first
//! assistant turn of the previous user turn; oxa's expectation then stops there. A server that
//! does not report `cached_tokens` yields no miss (and no hidden "0 cached") but one notice.
//!
//! oxa-fake models both template behaviours (`ScriptedEngine::with_thinking_template`).
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, Exposure, HeadlessUi, Source, ThinkingLevel, Tool, ToolCall, ToolClass, ToolCtx, ToolOutput,
    ToolSpec, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::sync::Arc;

struct Fine(ToolSpec);

impl Tool for Fine {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<oxa_proto::Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        ToolOutput {
            text: "fine".into(),
            is_error: false,
            ui: Value::Null,
        }
    }
}

fn fine() -> Arc<dyn Tool> {
    Arc::new(Fine(ToolSpec {
        name: "fine".into(),
        description: "test tool".into(),
        schema_json: "{}".into(),
        class: ToolClass::Read,
        exposure: Exposure::Direct,
        source: Source::Builtin,
    }))
}

fn open(env: &TestEnv, engine: Arc<ScriptedEngine>, settings: Settings) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let deps = Deps {
        engine,
        store,
        tools: vec![fine()],
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: Some(ThinkingLevel::Medium),
        mode: None,
        session_id: None,
    };
    Session::open(deps, Open::New).expect("open")
}

fn thinking_text(reasoning: &str, text: &str) -> ScriptTurn {
    ScriptTurn {
        reasoning: Some(reasoning.to_string()),
        ..ScriptTurn::text(text)
    }
}

fn thinking_tool(reasoning: &str, id: &str) -> ScriptTurn {
    ScriptTurn {
        reasoning: Some(reasoning.to_string()),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "fine".into(),
            args_raw: "{}".into(),
        }],
        ..ScriptTurn::tool(id, "fine", "{}")
    }
}

/// Two user turns, each with a tool loop, every reply thinking.
fn script() -> Vec<ScriptTurn> {
    vec![
        thinking_tool("first I look", "c1"),
        thinking_text("the tool said fine", "answer one"),
        thinking_tool("again a look", "c2"),
        thinking_text("fine again", "answer two"),
        thinking_text("short", "answer three"),
    ]
}

fn drive(s: &mut Session, sink: &mut VecSink) {
    let mut ui = HeadlessUi::default();
    for p in ["one", "two", "three"] {
        s.prompt(p.into(), Vec::new(), sink, &mut ui).expect("prompt");
    }
}

fn misses(sink: &VecSink) -> Vec<&AgentEvent> {
    sink.0
        .iter()
        .filter(|e| matches!(e, AgentEvent::CacheMiss { .. }))
        .collect()
}

/// Each request's `cached` against `prompt + completion - 1` of the one before.
fn exact_continuations(engine: &ScriptedEngine) -> Vec<bool> {
    let reqs = engine.requests();
    reqs.windows(2)
        .map(|w| {
            let (a, b) = (w[0].usage.expect("usage"), w[1].usage.expect("usage"));
            b.cached == a.prompt + a.completion - 1
        })
        .collect()
}

#[test]
fn thinking_on_and_preserved_continues_exactly() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(script()).with_thinking_template(Some(true), false));
    let mut s = open(&env, engine.clone(), Settings::default());
    let mut sink = VecSink::default();
    drive(&mut s, &mut sink);
    assert!(misses(&sink).is_empty(), "{:?}", misses(&sink));
    assert!(exact_continuations(&engine).iter().all(|&ok| ok));
    // The reasoning is part of `completion` (the engine counts every generated token).
    let first = &engine.requests()[0];
    let reply = first.reply.as_ref().expect("reply");
    assert!(first.usage.expect("usage").completion as usize > reply.reasoning.as_deref().unwrap_or("").len());
}

#[test]
fn thinking_on_and_not_preserved_raises_no_false_miss() {
    // Flash: the capabilities say the model does not keep reasoning, and its template is told
    // to drop it: each new user message re-renders the previous user turn without reasoning.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(script()).with_thinking_template(Some(false), true));
    let mut s = open(&env, engine.clone(), Settings::default());
    let mut sink = VecSink::default();
    drive(&mut s, &mut sink);
    assert!(misses(&sink).is_empty(), "no false miss: {:?}", misses(&sink));
    let exact = exact_continuations(&engine);
    // Inside a tool loop the reasoning stays (exact); at the new user messages it is dropped.
    assert!(exact.iter().any(|&ok| ok), "{exact:?}");
    assert!(
        exact.iter().any(|&ok| !ok),
        "the fake dropped reasoning somewhere: {exact:?}"
    );
}

#[test]
fn not_preserved_capability_with_a_keeping_template_continues_exactly() {
    // What oxa sends to Flash today: no `preserve_thinking` key, the template default keeps
    // the reasoning oxa echoes back.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(script()).with_thinking_template(Some(false), false));
    let mut s = open(&env, engine.clone(), Settings::default());
    let mut sink = VecSink::default();
    drive(&mut s, &mut sink);
    assert!(misses(&sink).is_empty(), "{:?}", misses(&sink));
    assert!(exact_continuations(&engine).iter().all(|&ok| ok));
}

#[test]
fn a_real_loss_is_still_reported_when_reasoning_is_not_preserved() {
    // Negative control: the bound must not hide an engine that lost the session.
    let env = TestEnv::new();
    let mut turns = script();
    turns[4] = ScriptTurn {
        fault: Some(Fault::Cold),
        ..thinking_text("short", "answer three")
    };
    let engine = Arc::new(ScriptedEngine::new(turns).with_thinking_template(Some(false), true));
    let mut s = open(&env, engine.clone(), Settings::default());
    let mut sink = VecSink::default();
    drive(&mut s, &mut sink);
    assert_eq!(misses(&sink).len(), 1, "{:?}", misses(&sink));
}

#[test]
fn thinking_toggled_per_turn_raises_no_miss() {
    // `oxa.thinking.policy = auto`: thinking on for the reply to a prompt, off for the tool
    // follow-ups. The toggle changes only the opener after the history.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        thinking_tool("first I look", "c1"),
        ScriptTurn::text("answer one"),
        thinking_text("plain", "answer two"),
    ]));
    let settings = Settings::from_values(json!({"oxa": {"thinking": {"policy": "auto"}}}), json!({}));
    let mut s = open(&env, engine.clone(), settings);
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    for p in ["one", "two"] {
        s.prompt(p.into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
    }
    let flags: Vec<bool> = engine.requests().iter().map(|r| r.thinking).collect();
    assert_eq!(flags, vec![true, false, true], "auto toggles per turn");
    assert!(misses(&sink).is_empty(), "{:?}", misses(&sink));
    assert!(exact_continuations(&engine).iter().all(|&ok| ok));
}

#[test]
fn missing_cached_tokens_is_neither_a_miss_nor_hidden() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(script()).without_cached_tokens());
    let mut s = open(&env, engine.clone(), Settings::default());
    let mut sink = VecSink::default();
    drive(&mut s, &mut sink);
    assert!(misses(&sink).is_empty(), "{:?}", misses(&sink));
    let notices: Vec<&String> = sink
        .0
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    let unreported = notices
        .iter()
        .filter(|t| t.contains("does not report cached"))
        .count();
    assert_eq!(unreported, 1, "one notice per session: {notices:?}");
    assert!(
        !notices.iter().any(|t| t.to_lowercase().contains("reload")),
        "no false engine-reload notice: {notices:?}"
    );
}
