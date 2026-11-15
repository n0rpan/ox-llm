//! Wave H (lane hooks): every hook point of `HookPoint::ALL` is fired by the product, with pi's
//! payload fields (pi 1.5.2 `packages/coding-agent/src/core/extensions/types.ts`, oxa names).
//!
//! A recording `Hooks` set rides along scripted fake-engine sessions that together reach every
//! point; the test fails naming each point that never fired and each payload key that is
//! missing. Points only a front end can fire are listed in [`FRONT_END_ONLY`] with the test
//! that covers them in the owning crate.
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, CommandSpec, EntryBody, Exposure, HeadlessUi, HookPoint, HookReply, Hooks, OxaError, Role,
    Source, ThinkingLevel, Tool, ToolCall, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Points a session cannot fire by itself, with the test that covers each.
const FRONT_END_ONLY: &[(HookPoint, &str)] = &[(
    HookPoint::ProjectTrust,
    "oxa-cli/tests/h_project_trust.rs::extension_decides_trust (and oxa-parity extensions.rs::ext_008, resources.rs::sec_003)",
)];

/// The keys each point's payload must carry (pi's event fields; `type` is added by the
/// extension bus and is not part of the core payload).
fn expected_keys(p: HookPoint) -> &'static [&'static str] {
    use HookPoint::*;
    match p {
        ProjectTrust => &["cwd"],
        ResourcesDiscover => &["cwd", "reason"],
        SessionStart => &["reason"],
        SessionShutdown => &["reason"],
        SessionBeforeSwitch => &["reason"],
        SessionBeforeFork => &["entryId", "position"],
        SessionBeforeCompact => &["reason", "customInstructions", "preparation", "willRetry"],
        SessionCompact => &[
            "reason",
            "summary",
            "firstKeptEntryId",
            "fromExtension",
            "willRetry",
        ],
        SessionCompactFailed => &["reason", "errorMessage", "aborted", "willRetry", "fromExtension"],
        SessionBeforeTree => &["preparation"],
        SessionTree => &["newLeafId", "oldLeafId", "fromExtension"],
        SessionInfoChanged => &["name"],
        Input => &["text", "source"],
        UserBash => &["command", "excludeFromContext", "cwd"],
        BeforeAgentStart => &["messageType", "systemPrompt"],
        AgentStart => &[],
        AgentEnd => &["messages"],
        AgentBeforeSettle => &["entries", "continue", "outcome"],
        AgentSettled => &["aborted"],
        TurnStart => &["turnIndex", "timestamp"],
        TurnEnd => &[
            "turnIndex",
            "message",
            "toolResults",
            "messageEntryId",
            "toolResultEntryIds",
        ],
        MessageStart => &["message"],
        MessageUpdate => &["message", "assistantMessageEvent"],
        MessageEnd => &["message"],
        Context => &["messages"],
        ContextWithSystem => &["messages"],
        BeforeProviderRequest => &["messages"],
        AfterProviderResponse => &["status", "headers"],
        ProviderStreamEvent => &["provider", "api", "model", "data"],
        CacheWarmingDecision => &["wireMessages"],
        ToolCall => &["toolCallId", "toolName", "input"],
        ToolResult => &["toolCallId", "toolName", "input", "content", "isError"],
        ToolExecutionStart => &["toolCallId", "toolName", "args"],
        ToolExecutionUpdate => &["toolCallId", "toolName", "args", "partialResult"],
        ToolExecutionEnd => &["toolCallId", "toolName", "result", "isError"],
        ModelSelect => &["model", "previousModel", "source"],
        ThinkingLevelSelect => &["level", "previousLevel"],
        UiPromptStart => &["reason", "kind"],
        UiPromptEnd => &["reason", "kind"],
    }
}

/// Records every fire. Answers `user_bash` with a replacement (nothing runs) and opens one
/// dialog from the first `turn_start` (a dialog inside a hook call: `ui_prompt_*` through the
/// UI tap of the pump).
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<(HookPoint, Value)>>,
    asked: AtomicBool,
}

impl Recorder {
    fn all(&self) -> Vec<(HookPoint, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Hooks for Recorder {
    fn fire(&self, p: HookPoint, payload: &Value, ui: &mut dyn Ui) -> HookReply {
        self.seen.lock().unwrap().push((p, payload.clone()));
        match p {
            HookPoint::UserBash => HookReply::ReplaceResult(ToolOutput {
                text: "replaced".into(),
                is_error: false,
                ui: json!({"exit": 0}),
            }),
            HookPoint::TurnStart if !self.asked.swap(true, Ordering::SeqCst) => {
                let _ = ui.confirm("hook dialog", "body", None);
                HookReply::Continue
            }
            _ => HookReply::Continue,
        }
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(oxa_proto::codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// `progress` streams one partial result; `probe` reads outside the working directory, so the
/// policy asks (print-like UI: the answer is no).
struct TestTool {
    spec: ToolSpec,
}

impl TestTool {
    fn arc(name: &str) -> Arc<dyn Tool> {
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
}

impl Tool for TestTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        if self.spec.name == "probe" {
            Ok(vec![Access::Read(PathBuf::from("/outside-the-workspace/x"))])
        } else {
            Ok(Vec::new())
        }
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        live("half");
        ToolOutput {
            text: "done".into(),
            is_error: false,
            ui: Value::Null,
        }
    }
}

fn chunked(t: ScriptTurn) -> ScriptTurn {
    ScriptTurn { chunk: 2, ..t }
}

fn deps(
    env: &TestEnv,
    store: Arc<dyn SessionStore>,
    engine: Arc<ScriptedEngine>,
    rec: Arc<Recorder>,
) -> Deps {
    Deps {
        engine,
        store,
        tools: vec![TestTool::arc("progress"), TestTool::arc("probe")],
        hooks: vec![rec as Arc<dyn Hooks>],
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        // Keep only the newest message, so that a manual compaction always has a cut.
        settings: Settings::from_values(
            json!({"compaction": {"keepRecentTokens": 1}, "oxa": {"profile": "pi"}}),
            json!({}),
        ),
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

/// The id of the entry holding the user message `text`.
fn user_entry(s: &Session, text: &str) -> String {
    s.inner()
        .tree
        .entries()
        .iter()
        .find(|e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.role == Role::User && wire.content.as_deref() == Some(text)))
        .map(|e| e.id.clone())
        .unwrap_or_else(|| panic!("no user message {text:?}"))
}

/// Drive one session through every operation that fires a hook point and return the record.
fn drive() -> Vec<(HookPoint, Value)> {
    let env = TestEnv::new();
    let two_calls = ScriptTurn {
        tool_calls: vec![
            ToolCall {
                id: "c1".into(),
                name: "progress".into(),
                args_raw: "{}".into(),
            },
            ToolCall {
                id: "c2".into(),
                name: "probe".into(),
                args_raw: "{}".into(),
            },
        ],
        ..ScriptTurn::tool("c1", "progress", "{}")
    };
    let engine = Arc::new(ScriptedEngine::new(vec![
        chunked(two_calls),
        chunked(ScriptTurn::text("done one")),
        ScriptTurn::fault(Fault::Http {
            status: 400,
            body: "bad summary request".into(),
        }),
        ScriptTurn::text("the summary"),
        chunked(ScriptTurn::text("done two")),
    ]));
    let rec = Arc::new(Recorder::default());
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut s = Session::open(deps(&env, store, engine, rec.clone()), Open::New).expect("open");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();

    s.prompt("one".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt one");
    assert!(
        s.compact(None, &mut sink, &mut ui).is_err(),
        "the scripted summary fails"
    );
    s.compact(None, &mut sink, &mut ui).expect("compact");
    s.prompt("two".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt two");
    let two = user_entry(&s, "two");
    s.navigate(&two, false, None, &mut sink, &mut ui)
        .expect("navigate");
    s.user_bash("echo hi", false, &mut sink, &mut ui)
        .expect("user bash");
    s.set_name("named", &mut sink).expect("name");
    s.set_thinking(ThinkingLevel::High, &mut sink);
    s.set_model("other-model", &mut sink).expect("model");
    s.fork(&two).expect("fork");
    s.switch(Open::New, &mut sink, &mut ui).expect("switch");
    s.close(&mut ui);
    rec.all()
}

#[test]
fn every_hook_point_fires_with_pis_payload() {
    let record = drive();
    let fired: BTreeSet<&str> = record.iter().map(|(p, _)| p.as_str()).collect();
    let front: BTreeSet<&str> = FRONT_END_ONLY.iter().map(|(p, _)| p.as_str()).collect();
    let missing: Vec<&str> = HookPoint::ALL
        .iter()
        .map(|p| p.as_str())
        .filter(|p| !fired.contains(p) && !front.contains(p))
        .collect();
    assert!(missing.is_empty(), "hook points never fired: {missing:?}");

    let mut bad: Vec<String> = Vec::new();
    for p in HookPoint::ALL {
        if front.contains(p.as_str()) {
            continue;
        }
        // Every fire of the point carries the keys (not just one of them).
        for (_, payload) in record.iter().filter(|(q, _)| *q == p) {
            for key in expected_keys(p) {
                if payload.get(key).is_none() {
                    bad.push(format!("{}.{key} in {payload}", p.as_str()));
                }
            }
        }
    }
    assert!(bad.is_empty(), "payload keys missing: {bad:#?}");
}

#[test]
fn payload_values_follow_pi() {
    let record = drive();
    let first = |p: HookPoint| {
        record
            .iter()
            .find(|(q, _)| *q == p)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{} did not fire", p.as_str()))
    };
    let all = |p: HookPoint| -> Vec<Value> {
        record
            .iter()
            .filter(|(q, _)| *q == p)
            .map(|(_, v)| v.clone())
            .collect()
    };

    // agent_end carries the run's messages: the prompt, the reply, both tool results, the final
    // reply (pi's `newMessages` start with the prompts).
    let end = first(HookPoint::AgentEnd);
    let roles: Vec<&str> = end["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "tool", "assistant"]);
    assert_eq!(end["messages"][0]["content"], "one");
    assert_eq!(end["messages"][4]["content"], "done one");
    assert_eq!(end["messages"][4]["stopReason"], "stop");
    assert_eq!(end["messages"][2]["toolName"], "progress");
    // message_start: the prompt first (pi emits it for every message of the run).
    assert_eq!(first(HookPoint::MessageStart)["message"]["role"], "user");

    // turn_end: the reply and its tool results, with their entry ids.
    let turn = first(HookPoint::TurnEnd);
    assert_eq!(turn["turnIndex"], 0);
    assert_eq!(turn["toolResults"].as_array().unwrap().len(), 2);
    assert_eq!(turn["toolResultEntryIds"].as_array().unwrap().len(), 2);
    assert_eq!(turn["continue"], true);
    assert_eq!(all(HookPoint::TurnEnd)[1]["turnIndex"], 1);

    // before_agent_start: the prompt and the system text of the frozen prefix in force.
    let bas = first(HookPoint::BeforeAgentStart);
    assert_eq!(bas["prompt"], "one");
    assert_eq!(bas["messageType"], "user");
    assert!(!bas["systemPrompt"].as_str().unwrap().is_empty());

    // context_with_system: the system message first, then the wire of `context`.
    let ctx = first(HookPoint::Context);
    let cws = first(HookPoint::ContextWithSystem);
    assert_eq!(cws["messages"][0]["role"], "system");
    assert_eq!(cws["messages"][0]["content"], bas["systemPrompt"]);
    assert_eq!(
        cws["messages"].as_array().unwrap()[1..],
        ctx["messages"].as_array().unwrap()[..]
    );

    // message_update: the message so far grows with the deltas.
    let texts: Vec<String> = all(HookPoint::MessageUpdate)
        .iter()
        .filter_map(|v| v["message"]["content"].as_str().map(str::to_string))
        .collect();
    assert!(texts.iter().any(|t| t == "done one"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "do"), "{texts:?}");

    // tool execution: start, one update with the partial output, end with the result.
    let upd = first(HookPoint::ToolExecutionUpdate);
    assert_eq!(upd["toolName"], "progress");
    assert_eq!(upd["partialResult"]["content"][0]["text"], "half");
    let ends = all(HookPoint::ToolExecutionEnd);
    assert_eq!(ends[0]["result"]["content"][0]["text"], "done");
    assert_eq!(ends[1]["toolName"], "probe");
    assert_eq!(ends[1]["isError"], true, "the probe was denied");

    // ui prompts: the permission prompt of the probe and the dialog a hook opened, each a
    // start and an end.
    let titles: Vec<String> = all(HookPoint::UiPromptStart)
        .iter()
        .map(|v| v["title"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(titles.iter().any(|t| t == "hook dialog"), "{titles:?}");
    assert!(
        titles.iter().any(|t| t.contains("/outside-the-workspace/x")),
        "{titles:?}"
    );
    assert_eq!(
        all(HookPoint::UiPromptStart).len(),
        all(HookPoint::UiPromptEnd).len()
    );

    // Lifecycle order of a run: agent_end, agent_before_settle, agent_settled.
    let pos = |p: HookPoint| record.iter().position(|(q, _)| *q == p).unwrap();
    assert!(pos(HookPoint::AgentEnd) < pos(HookPoint::AgentBeforeSettle));
    assert!(pos(HookPoint::AgentBeforeSettle) < pos(HookPoint::AgentSettled));
    assert_eq!(first(HookPoint::AgentSettled)["aborted"], false);
    // session_start before resources_discover (pi's order), with their reasons.
    assert!(pos(HookPoint::SessionStart) < pos(HookPoint::ResourcesDiscover));
    assert_eq!(first(HookPoint::SessionStart)["reason"], "startup");
    let reasons: Vec<Value> = all(HookPoint::SessionStart)
        .iter()
        .map(|v| v["reason"].clone())
        .collect();
    assert!(reasons.contains(&json!("new")), "{reasons:?}");
    assert!(reasons.contains(&json!("fork")), "{reasons:?}");
    // session_shutdown: the fork, the switch to a new session, then the end.
    let down: Vec<Value> = all(HookPoint::SessionShutdown)
        .iter()
        .map(|v| v["reason"].clone())
        .collect();
    assert_eq!(down, [json!("fork"), json!("new"), json!("quit")]);

    assert_eq!(first(HookPoint::ThinkingLevelSelect)["level"], "high");
    assert_eq!(first(HookPoint::SessionInfoChanged)["name"], "named");
    assert_eq!(first(HookPoint::ModelSelect)["model"], "other-model");
    assert_eq!(first(HookPoint::SessionCompactFailed)["aborted"], false);
    assert_eq!(first(HookPoint::ProviderStreamEvent)["provider"], "oxe");
}

/// `agent_before_settle` may append a user message: the loop runs again before the session
/// settles, and `agent_settled` comes once, after it.
#[test]
fn before_settle_append_runs_again_then_settles() {
    struct Settler {
        seen: Mutex<Vec<HookPoint>>,
        once: AtomicBool,
    }
    impl Hooks for Settler {
        fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
            self.seen.lock().unwrap().push(p);
            if p == HookPoint::AgentBeforeSettle && !self.once.swap(true, Ordering::SeqCst) {
                return HookReply::Append(vec![oxa_proto::WireMsg::user("one more")]);
            }
            HookReply::Continue
        }
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            Vec::new()
        }
        fn commands(&self) -> Vec<CommandSpec> {
            Vec::new()
        }
        fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
            Err(OxaError::new(oxa_proto::codes::NOT_FOUND, name))
        }
        fn prompt_sections(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn wants(&self, p: HookPoint) -> bool {
            matches!(
                p,
                HookPoint::AgentBeforeSettle | HookPoint::AgentSettled | HookPoint::AgentEnd
            )
        }
    }
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("a"),
        ScriptTurn::text("b"),
    ]));
    let hooks = Arc::new(Settler {
        seen: Mutex::new(Vec::new()),
        once: AtomicBool::new(false),
    });
    let mut d = deps(
        &env,
        Arc::new(MemStore::new()),
        engine.clone(),
        Arc::new(Recorder::default()),
    );
    d.hooks = vec![hooks.clone() as Arc<dyn Hooks>];
    let mut s = Session::open(d, Open::New).expect("open");
    s.prompt(
        "go".into(),
        Vec::new(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    use HookPoint::*;
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [
            AgentEnd,
            AgentBeforeSettle,
            AgentEnd,
            AgentBeforeSettle,
            AgentSettled
        ]
    );
    let reqs = engine.requests();
    assert_eq!(reqs.len(), 2);
    let last = reqs[1].messages.last().unwrap();
    assert_eq!(
        last.content.as_deref(),
        Some("one more"),
        "the appended message is sent"
    );
}

/// A source that wants nothing is never called (no helper thread, no payload).
#[test]
fn sources_that_want_nothing_are_not_called() {
    struct Nothing(AtomicBool);
    impl Hooks for Nothing {
        fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
            self.0.store(true, Ordering::SeqCst);
            HookReply::Continue
        }
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            Vec::new()
        }
        fn commands(&self) -> Vec<CommandSpec> {
            Vec::new()
        }
        fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
            Err(OxaError::new(oxa_proto::codes::NOT_FOUND, name))
        }
        fn prompt_sections(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn wants(&self, _p: HookPoint) -> bool {
            false
        }
    }
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![chunked(ScriptTurn::text(
        "streamed reply",
    ))]));
    let hooks = Arc::new(Nothing(AtomicBool::new(false)));
    let mut d = deps(
        &env,
        Arc::new(MemStore::new()),
        engine,
        Arc::new(Recorder::default()),
    );
    d.hooks = vec![hooks.clone() as Arc<dyn Hooks>];
    let mut s = Session::open(d, Open::New).expect("open");
    s.prompt(
        "go".into(),
        Vec::new(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    assert!(!hooks.0.load(Ordering::SeqCst));
}

/// The front-end-only list names real points and real tests.
#[test]
fn front_end_only_points_name_their_test() {
    for (p, test) in FRONT_END_ONLY {
        assert!(HookPoint::ALL.contains(p));
        assert!(test.contains("::"), "{test}");
    }
}

/// A run a hook set triggers (`sendMessage(.., {triggerTurn: true})` while idle) fires
/// `before_agent_start` with pi's custom form: `{messageType: "custom", message}`.
#[test]
fn triggered_turn_fires_before_agent_start_with_the_message() {
    #[derive(Default)]
    struct Kicker {
        host: Mutex<Option<Arc<dyn oxa_proto::Host>>>,
        seen: Mutex<Vec<Value>>,
    }
    impl Hooks for Kicker {
        fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
            if p == HookPoint::BeforeAgentStart {
                self.seen.lock().unwrap().push(payload.clone());
            }
            HookReply::Continue
        }
        fn attach(&self, host: Arc<dyn oxa_proto::Host>) {
            *self.host.lock().unwrap() = Some(host);
        }
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            Vec::new()
        }
        fn commands(&self) -> Vec<CommandSpec> {
            vec![CommandSpec {
                name: "kick".into(),
                description: "trigger a turn".into(),
                argument_hint: None,
                source: Source::Builtin,
            }]
        }
        fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
            let host = self.host.lock().unwrap().clone().expect("attached");
            host.call(oxa_proto::HostAction::SendMessage {
                custom_type: "kick".into(),
                content: "go on".into(),
                display: true,
                details: Value::Null,
                deliver: oxa_proto::Delivery::FollowUp,
                trigger_turn: true,
            })
            .map(|_| ())
        }
        fn prompt_sections(&self) -> Vec<(String, String)> {
            Vec::new()
        }
    }
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let hooks = Arc::new(Kicker::default());
    let mut d = deps(
        &env,
        Arc::new(MemStore::new()),
        engine.clone(),
        Arc::new(Recorder::default()),
    );
    d.hooks = vec![hooks.clone() as Arc<dyn Hooks>];
    let mut s = Session::open(d, Open::New).expect("open");
    s.run_command("kick", "", &mut VecSink::default(), &mut HeadlessUi::default())
        .expect("command");
    assert_eq!(engine.requests().len(), 1, "the triggered turn ran");
    let seen = hooks.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["messageType"], "custom");
    assert_eq!(seen[0]["message"]["customType"], "kick");
    assert_eq!(seen[0]["message"]["content"], "go on");
    assert!(seen[0]["systemPrompt"].is_string());
}
