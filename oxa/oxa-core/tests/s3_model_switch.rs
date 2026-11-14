//! S3 / O-145 / ENG-004: the model switch door (`session::switch_model`) against the HTTP fake
//! controller with two models: a large "moe" and a small "flash" whose cap is scaled to the
//! session's prefix so the tests stay fast. Every chat request goes through [`Recorder`], which
//! records a violation for any request addressed to "flash" whose prompt plus `max_tokens`
//! does not fit Flash's context (the real engine would refuse it or cut the reply).

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, OverCap, Session, ToolSelection};
use oxa_fake::{cache_bytes, FakeModel, FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, AgentEvent, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, HeadlessUi, HookPoint,
    HookReply, Hooks, Host, HostAction, OxaError, StreamEvent, Tool, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::{Arc, Mutex};

const FLASH: &str = "flash";
const MOE: &str = "moe";
/// Flash's output limit (published by the controller row).
const FLASH_OUT: u32 = 256;
/// Bytes of each large user prompt (one token = one byte on the fake).
const BIG: usize = 3000;

/// Delegates to the HTTP engine and checks every request addressed to Flash.
struct Recorder {
    inner: oxa_engine::HttpEngine,
    flash_cap: u32,
    /// `(model, kind, prompt tokens)` of every chat request, in order.
    log: Mutex<Vec<(String, String, u64)>>,
    violations: Mutex<Vec<String>>,
}

impl Recorder {
    fn new(url: &str, flash_cap: u32) -> Arc<Recorder> {
        Arc::new(Recorder {
            inner: oxa_engine::HttpEngine::new(url, None).expect("engine"),
            flash_cap,
            log: Mutex::new(Vec::new()),
            violations: Mutex::new(Vec::new()),
        })
    }
    fn log(&self) -> Vec<(String, String, u64)> {
        self.log.lock().unwrap().clone()
    }
    fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }
}

impl Engine for Recorder {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let prompt = cache_bytes(&r.prefix.system_text(), &r.prefix.tools_json, r.messages).len() as u64;
        self.log
            .lock()
            .unwrap()
            .push((r.model.to_string(), r.hints.kind.as_str().to_string(), prompt));
        if r.model == FLASH && prompt + u64::from(r.max_tokens) > u64::from(self.flash_cap) {
            self.violations.lock().unwrap().push(format!(
                "flash request ({}) prompt {prompt} + max_tokens {} > cap {}",
                r.hints.kind.as_str(),
                r.max_tokens,
                self.flash_cap
            ));
        }
        self.inner.chat(r, cancel, on)
    }
}

/// Records the hook points it sees and when its tools are read (the epoch reads them).
#[derive(Default)]
struct Probe {
    log: Mutex<Vec<String>>,
    host: Mutex<Option<Arc<dyn Host>>>,
}

impl Hooks for Probe {
    fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p == HookPoint::ModelSelect {
            self.log.lock().unwrap().push(format!("model_select {payload}"));
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.log.lock().unwrap().push("tools".to_string());
        Vec::new()
    }
    fn commands(&self) -> Vec<oxa_proto::CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
}

fn write_settings(env: &TestEnv) {
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let settings: serde_json::Value = serde_json::from_str(
        r#"{"compaction": {"reserveTokens": 256, "keepRecentTokens": 200}, "retry": {"enabled": false}}"#,
    )
    .unwrap();
    std::fs::write(path, settings.to_string()).unwrap();
}

fn deps(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Vec<Arc<dyn Hooks>>) -> Deps {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), false);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs,
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: Some(MOE.to_string()),
        thinking: None,
        mode: None,
        session_id: None,
    }
}

/// The bytes of the session's frozen prefix (what every request carries before the wire).
fn prefix_len(env: &TestEnv) -> u32 {
    let eng: Arc<dyn Engine> = Arc::new(oxa_fake::ScriptedEngine::new(Vec::new()));
    let s = Session::open(deps(env, eng, Vec::new()), Open::Ephemeral).expect("open");
    let p = &s.inner().prefix;
    cache_bytes(&p.system_text(), &p.tools_json, &[]).len() as u32
}

struct Rig {
    _server: FakeServer,
    eng: Arc<Recorder>,
    session: Session,
    flash_cap: u32,
    probe: Arc<Probe>,
}

/// A fake controller with both models (`publish_flash = false`: Flash has no `model_limits`
/// row, an engine that does not publish the target's limits) and a session on "moe".
fn rig(env: &TestEnv, script: Vec<ScriptTurn>, publish_flash: bool) -> Rig {
    write_settings(env);
    let flash_cap = prefix_len(env) + 4000;
    let mut models = vec![FakeModel::moe(MOE, 1 << 20)];
    if publish_flash {
        models.push(FakeModel::flash(FLASH, flash_cap, Some(FLASH_OUT)));
    }
    let server = FakeServer::start(
        script,
        FakeOpts {
            model: MOE.to_string(),
            models,
            ..FakeOpts::default()
        },
    )
    .expect("fake");
    let eng = Recorder::new(&server.url(), flash_cap);
    let probe = Arc::new(Probe::default());
    let session = Session::open(deps(env, eng.clone(), vec![probe.clone()]), Open::New).expect("open");
    Rig {
        _server: server,
        eng,
        session,
        flash_cap,
        probe,
    }
}

fn prompt(s: &mut Session, text: &str) -> Vec<AgentEvent> {
    let mut sink = VecSink::default();
    s.prompt(
        text.to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    sink.0
}

/// Two large exchanges on "moe": the conversation is far above Flash's hard limit.
fn big_conversation(s: &mut Session) {
    prompt(s, &"a".repeat(BIG));
    prompt(s, &"b".repeat(BIG));
}

fn notices(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn script_big(extra: Vec<ScriptTurn>) -> Vec<ScriptTurn> {
    let mut turns = vec![ScriptTurn::text("one"), ScriptTurn::text("two")];
    turns.extend(extra);
    turns
}

/// (a) O-145: above Flash's limit, `CompactFirst` compacts on the SOURCE model (the summary
/// request is addressed to "moe", in the foreground) BEFORE anything reaches Flash, and the
/// first Flash request fits Flash.
#[test]
fn compact_first_summarises_on_source_before_any_target_request() {
    let env = TestEnv::new();
    let mut r = rig(
        &env,
        script_big(vec![
            ScriptTurn::text("SUMMARY"),
            ScriptTurn::text("hi from flash"),
        ]),
        true,
    );
    big_conversation(&mut r.session);
    let plan = r.session.plan_model_switch(FLASH);
    assert!(plan.over_cap, "{plan:?}");
    assert_eq!(plan.target_ctx, Some(r.flash_cap));
    assert!(plan.context_tokens > plan.target_hard.unwrap());

    let mut sink = VecSink::default();
    r.session
        .set_model_with(
            FLASH,
            OverCap::CompactFirst,
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .expect("switch");
    assert_eq!(r.session.state().model, FLASH);
    assert!(sink
        .0
        .iter()
        .any(|e| matches!(e, AgentEvent::CompactionEnd { result: Some(_), .. })));
    prompt(&mut r.session, "hello");

    let log = r.eng.log();
    let summary = log
        .iter()
        .position(|(_, k, _)| k == "compaction")
        .expect("a summary request");
    assert_eq!(log[summary].0, MOE, "the summary goes to the source model");
    let first_flash = log
        .iter()
        .position(|(m, _, _)| m == FLASH)
        .expect("a flash request");
    assert!(summary < first_flash, "{log:?}");
    assert!(
        !log[..first_flash]
            .iter()
            .any(|(m, k, _)| m == MOE && k == "prefill"),
        "no idle prefill on the source after the pre-switch compaction: {log:?}"
    );
    assert_eq!(r.eng.violations(), Vec::<String>::new());
    // F12: the pre-switch summary is a foreground request.
    let reqs = r._server.requests();
    let sum = reqs.iter().find(|q| q.kind == "compaction").unwrap();
    assert!(!sum.background);
}

/// The recorder really trips: an engine that does not publish Flash's limits leaves oxa with
/// the provisional budget (no over-cap guard is possible), and the first Flash request is
/// recorded as oversized. Also a hand-made oversized Flash request is caught directly.
#[test]
fn recorder_catches_an_oversized_target_request() {
    let env = TestEnv::new();
    let mut r = rig(&env, script_big(vec![ScriptTurn::text("x")]), false);
    big_conversation(&mut r.session);
    let plan = r.session.plan_model_switch(FLASH);
    assert!(
        !plan.over_cap && plan.target_ctx.is_none(),
        "unknown limits: {plan:?}"
    );
    let mut sink = VecSink::default();
    r.session
        .set_model_with(
            FLASH,
            OverCap::CompactFirst,
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .expect("switch");
    prompt(&mut r.session, "hello");
    assert_eq!(r.eng.violations().len(), 1, "{:?}", r.eng.violations());

    let st = r.session.inner();
    let msgs = vec![WireMsg::user("z".repeat(r.flash_cap as usize))];
    let req = ChatRequest {
        prefix: &st.prefix,
        messages: &msgs,
        model: FLASH,
        max_tokens: 1,
        thinking: false,
        preserve_thinking: false,
        hints: oxa_proto::Hints {
            session: "s",
            idem_key: "",
            kind: oxa_proto::TurnKind::Chat,
            background: false,
            snapshot_keep: false,
        },
    };
    let _ = r.eng.chat(&req, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.eng.violations().len(), 2);
}

/// (b) `Refuse` over the cap changes nothing and sends nothing; the error carries both numbers.
#[test]
fn refuse_changes_nothing_and_sends_nothing() {
    let env = TestEnv::new();
    let mut r = rig(&env, script_big(Vec::new()), true);
    big_conversation(&mut r.session);
    let plan = r.session.plan_model_switch(FLASH);
    let requests = r.eng.log().len();
    let entries = r.session.inner().tree.entries().len();
    let mut sink = VecSink::default();
    let err = r
        .session
        .set_model_with(FLASH, OverCap::Refuse, &mut sink, &mut HeadlessUi::default())
        .expect_err("refused");
    assert_eq!(err.code, codes::INVALID);
    assert!(
        err.message.contains(&plan.context_tokens.to_string()),
        "{}",
        err.message
    );
    assert!(
        err.message.contains(&plan.target_hard.unwrap().to_string()),
        "{}",
        err.message
    );
    assert_eq!(r.session.state().model, MOE);
    assert_eq!(r.eng.log().len(), requests, "nothing sent");
    assert_eq!(
        r.session.inner().tree.entries().len(),
        entries,
        "nothing appended"
    );
    assert!(sink.0.is_empty(), "no event: {:?}", sink.0);
}

/// (c) OXA-S3-EXT-SETMODEL: an extension's `oxa.setModel` (`HostAction::SetModel`) goes through
/// the same door end to end: a real session, a hook set's host, the settled serve.
#[test]
fn extension_set_model_compacts_first_through_the_host() {
    let env = TestEnv::new();
    let mut r = rig(
        &env,
        script_big(vec![
            ScriptTurn::text("SUMMARY"),
            ScriptTurn::text("hi from flash"),
        ]),
        true,
    );
    big_conversation(&mut r.session);
    let host = r.probe.host.lock().unwrap().clone().expect("attached");
    assert_eq!(
        host.call(HostAction::SetModel(FLASH.to_string())),
        Ok(Value::Null)
    );
    let mut sink = VecSink::default();
    r.session
        .serve_host(&mut sink, &mut HeadlessUi::default())
        .expect("serve");
    assert_eq!(r.session.state().model, FLASH);
    prompt(&mut r.session, "hello");
    let log = r.eng.log();
    let summary = log
        .iter()
        .position(|(_, k, _)| k == "compaction")
        .expect("summary");
    let first_flash = log.iter().position(|(m, _, _)| m == FLASH).expect("flash");
    assert_eq!(log[summary].0, MOE);
    assert!(summary < first_flash);
    assert_eq!(r.eng.violations(), Vec::<String>::new());
}

/// (e) Not over the cap: one notice names the target, the tokens it prefills cold and the
/// estimated seconds; the epoch event carries the same number.
#[test]
fn switch_prints_one_cost_notice() {
    let env = TestEnv::new();
    let mut r = rig(&env, vec![ScriptTurn::text("one")], true);
    prompt(&mut r.session, "small");
    let plan = r.session.plan_model_switch(FLASH);
    assert!(!plan.over_cap);
    let mut sink = VecSink::default();
    r.session.set_model(FLASH, &mut sink).expect("switch");
    let costs: Vec<String> = notices(&sink.0)
        .into_iter()
        .filter(|n| n.contains("tokens"))
        .collect();
    assert_eq!(costs.len(), 1, "{costs:?}");
    let n = &costs[0];
    assert!(n.contains(FLASH), "{n}");
    assert!(n.contains(&plan.cold_tokens.to_string()), "{n}");
    assert!(n.contains(&format!("~{} s", plan.est_secs)), "{n}");
    let epoch = sink.0.iter().find_map(|e| match e {
        AgentEvent::PrefixEpoch { reprefill_tokens, .. } => Some(*reprefill_tokens),
        _ => None,
    });
    assert_eq!(epoch, Some(plan.cold_tokens));
}

/// (f) The `ModelSelect` hook point fires with `{model, previousModel}` after the limits are
/// refreshed and before the epoch reads the hook sets' tools.
#[test]
fn model_select_hook_fires_before_the_epoch() {
    let env = TestEnv::new();
    let mut r = rig(&env, vec![ScriptTurn::text("one")], true);
    r.probe.log.lock().unwrap().clear();
    let mut sink = VecSink::default();
    r.session.set_model(FLASH, &mut sink).expect("switch");
    let log = r.probe.log.lock().unwrap().clone();
    let at = log
        .iter()
        .position(|l| l.starts_with("model_select"))
        .expect("fired");
    let payload: Value = serde_json::from_str(log[at].trim_start_matches("model_select ")).unwrap();
    assert_eq!(payload["model"], FLASH);
    assert_eq!(payload["previousModel"], MOE);
    assert!(
        log[at + 1..].iter().any(|l| l == "tools"),
        "epoch after the hook: {log:?}"
    );
    assert!(!log[..at].iter().any(|l| l == "tools"), "{log:?}");
}
