//! X3 regression tests: a model switch and `/reload` refresh the engine metadata and the
//! compaction policy, and the runtime auto-compaction switch takes effect. Model-free: a fake
//! controller serves two models with different limits and loads whichever model a chat request
//! names (as the local-ai controller does). With `publish`, its status carries per-model limits
//! (the controller's `model_limits`), so `Engine::info(Some(target))` describes a target that
//! is not loaded; without it, `info` describes the loaded model only (an older controller).

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{cache_bytes, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, AssistantTurn, CancelToken, ChatRequest, CompactReason, Engine, EngineInfo, HeadlessUi,
    OxaError, StreamEvent, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::{Arc, Mutex};

/// (alias, n_ctx, max_output, compact_at)
type Limits = (&'static str, u32, u32, Option<u32>);

const MOE: Limits = ("moe", 131_072, 16_384, Some(100_000));
const FLASH: Limits = ("flash", 16_384, 1_024, Some(200));

/// Two models behind one endpoint. A chat request loads the model it names. Chat itself is the
/// scripted engine's; every request is checked against the limits of the model it names, and
/// a request larger than that model's context or output limit is recorded as a violation (the
/// real engine would refuse it or the controller would cap it).
struct TwoModels {
    chat: ScriptedEngine,
    loaded: Mutex<&'static str>,
    publish: bool,
    violations: Mutex<Vec<String>>,
}

impl TwoModels {
    fn new(turns: Vec<ScriptTurn>, publish: bool) -> Arc<TwoModels> {
        Arc::new(TwoModels {
            chat: ScriptedEngine::new(turns),
            loaded: Mutex::new(MOE.0),
            publish,
            violations: Mutex::new(Vec::new()),
        })
    }
    fn load(&self, m: &'static str) {
        *self.loaded.lock().unwrap() = m;
    }
    fn limits(m: &str) -> Limits {
        if m == FLASH.0 {
            FLASH
        } else {
            MOE
        }
    }
    /// `max_tokens` of every generating chat request, in order.
    fn chat_max_tokens(&self) -> Vec<(String, u32)> {
        self.chat
            .requests()
            .iter()
            .filter(|r| r.kind == "chat")
            .map(|r| (r.model.clone(), r.max_tokens))
            .collect()
    }
    fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }
}

impl Engine for TwoModels {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let loaded = *self.loaded.lock().unwrap();
        let limits_for = match target {
            Some(t) if self.publish && (t == MOE.0 || t == FLASH.0) => t,
            _ => loaded,
        };
        let (_, n_ctx, max_output, compact_at) = Self::limits(limits_for);
        Ok(EngineInfo {
            base_url: "two-models:".to_string(),
            models: vec![MOE.0.to_string(), FLASH.0.to_string()],
            model: loaded.to_string(),
            limits_for: limits_for.to_string(),
            n_ctx,
            max_output,
            queue: 0,
            compact_at,
            prefill_only: loaded == MOE.0,
            caps: oxa_proto::EngineCaps::default(),
            thinking: None,
            images: None,
        })
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.chat.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let (model, n_ctx, max_output, _) = Self::limits(r.model);
        let prompt = cache_bytes(&r.prefix.system_text(), &r.prefix.tools_json, r.messages).len() as u64;
        if r.max_tokens > max_output {
            self.violations.lock().unwrap().push(format!(
                "{model}: max_tokens {} > output limit {max_output}",
                r.max_tokens
            ));
        }
        if prompt + u64::from(r.max_tokens) > u64::from(n_ctx) {
            self.violations.lock().unwrap().push(format!(
                "{model}: prompt {prompt} + max_tokens {} > context {n_ctx}",
                r.max_tokens
            ));
        }
        if r.max_tokens > 0 {
            self.load(if r.model == FLASH.0 { FLASH.0 } else { MOE.0 });
        }
        self.chat.chat(r, cancel, on)
    }
}

fn write_settings(env: &TestEnv, json: &str) {
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

fn open(env: &TestEnv, engine: Arc<TwoModels>) -> Session {
    open_with(env, engine, None)
}

fn open_with(env: &TestEnv, engine: Arc<TwoModels>, model: Option<&str>) -> Session {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), false);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: model.map(str::to_string),
            thinking: None,
            mode: None,
            session_id: None,
        },
        Open::New,
    )
    .expect("open")
}

fn prompt(s: &mut Session, text: &str) -> Vec<AgentEvent> {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.prompt(text.to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    sink.0
}

fn compacted(events: &[AgentEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::CompactionStart { .. }))
}

/// The reasons of the compactions that completed (a `CompactionEnd` with a result and no error).
fn compactions(events: &[AgentEvent]) -> Vec<CompactReason> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::CompactionEnd {
                reason,
                result: Some(_),
                error_message: None,
                aborted: false,
                ..
            } => Some(*reason),
            _ => None,
        })
        .collect()
}

/// The policy that `limits` and the current settings give.
fn expected(s: &Session, l: Limits) -> oxa_compact::CompactPolicy {
    let comp = s.inner().settings.compaction();
    oxa_compact::policy_from(
        l.1,
        l.2,
        comp.reserve_tokens,
        comp.keep_recent_tokens,
        comp.soft_tokens.or(l.3),
        comp.enabled,
    )
}

/// 128K MoE -> 16K Flash where Flash is not loaded yet: the switch reads Flash's published
/// limits at once, so the FIRST Flash request already fits Flash's context and output limit (the
/// fake records any request that does not), and compaction fires at Flash's `compact_at`, which
/// MoE's thresholds would never reach.
#[test]
fn switch_to_unloaded_smaller_model_fits_it_from_the_first_request() {
    let env = TestEnv::new();
    write_settings(
        &env,
        r#"{"compaction": {"reserveTokens": 1024, "keepRecentTokens": 1}}"#,
    );
    let eng = TwoModels::new(
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("SUMMARY"),
            ScriptTurn::text("three"),
        ],
        true,
    );
    let mut s = open(&env, eng.clone());
    assert_eq!(s.inner().compact, expected(&s, MOE));

    assert!(
        compactions(&prompt(&mut s, "first")).is_empty(),
        "MoE's soft limit is far away"
    );

    let mut sink = VecSink::default();
    s.set_model(FLASH.0, &mut sink).expect("set_model");
    let info = &s.inner().info;
    assert_eq!(info.model, MOE.0, "Flash is not loaded yet");
    assert_eq!(info.limits_for, FLASH.0);
    assert_eq!(
        (info.n_ctx, info.max_output, info.compact_at),
        (FLASH.1, FLASH.2, FLASH.3)
    );
    assert_eq!(s.inner().compact, expected(&s, FLASH));
    assert_eq!(s.state().n_ctx, FLASH.1);

    let ev = prompt(&mut s, "second");
    assert_eq!(
        eng.chat_max_tokens().last(),
        Some(&(FLASH.0.to_string(), FLASH.2)),
        "first Flash request"
    );
    assert_eq!(
        compactions(&ev),
        vec![CompactReason::Threshold],
        "Flash's compact_at must trigger compaction"
    );
    assert_eq!(s.inner().info.model, FLASH.0);
    assert_eq!(s.inner().compact.soft, 200);

    prompt(&mut s, "third");
    let reqs = eng.chat_max_tokens();
    assert_eq!(reqs.first(), Some(&(MOE.0.to_string(), MOE.2)));
    assert_eq!(reqs.last(), Some(&(FLASH.0.to_string(), FLASH.2)));
    assert_eq!(
        eng.violations(),
        Vec::<String>::new(),
        "no request may exceed its model's limits"
    );
}

/// `--model flash` while MoE is loaded: the session opens with Flash's limits.
#[test]
fn open_with_unloaded_target_uses_its_limits() {
    let env = TestEnv::new();
    let eng = TwoModels::new(vec![ScriptTurn::text("hi")], true);
    let mut s = open_with(&env, eng.clone(), Some(FLASH.0));
    assert_eq!(
        (s.inner().info.n_ctx, s.inner().info.max_output),
        (FLASH.1, FLASH.2)
    );
    prompt(&mut s, "hello");
    assert_eq!(eng.chat_max_tokens(), vec![(FLASH.0.to_string(), FLASH.2)]);
    assert_eq!(eng.violations(), Vec::<String>::new());
}

/// An engine without per-model limits (an older controller). The target is already loaded: the
/// switch adopts its limits at once. Switching back to the larger model while the small one is
/// loaded keeps the small (conservative) budget until the large one is loaded, then grows again.
#[test]
fn without_published_limits_switch_is_exact_when_loaded_and_conservative_otherwise() {
    let env = TestEnv::new();
    write_settings(
        &env,
        r#"{"compaction": {"reserveTokens": 1024, "keepRecentTokens": 4000, "enabled": false}}"#,
    );
    let eng = TwoModels::new(
        vec![
            ScriptTurn::text("a"),
            ScriptTurn::text("b"),
            ScriptTurn::text("c"),
        ],
        false,
    );
    let mut s = open(&env, eng.clone());
    let mut sink = VecSink::default();

    eng.load(FLASH.0);
    s.set_model(FLASH.0, &mut sink).expect("to flash");
    assert_eq!(s.inner().info.model, FLASH.0);
    assert_eq!(
        (s.inner().info.n_ctx, s.inner().info.max_output),
        (FLASH.1, FLASH.2)
    );
    assert_eq!(s.inner().info.compact_at, FLASH.3);
    assert!(!s.inner().info.prefill_only, "Flash does not advertise H6 here");
    assert_eq!(s.inner().compact, expected(&s, FLASH));
    prompt(&mut s, "x");

    s.set_model(MOE.0, &mut sink).expect("back to moe");
    assert_eq!(
        (s.inner().info.n_ctx, s.inner().info.max_output),
        (FLASH.1, FLASH.2),
        "provisional = smaller"
    );
    prompt(&mut s, "y");
    // That request loaded MoE; the refresh after the run adopts its limits.
    assert_eq!(s.inner().info.model, MOE.0);
    assert_eq!(s.inner().compact, expected(&s, MOE));
    prompt(&mut s, "z");

    let reqs: Vec<u32> = eng.chat_max_tokens().into_iter().map(|(_, m)| m).collect();
    assert_eq!(reqs, vec![FLASH.2, FLASH.2, MOE.2]);
    assert_eq!(eng.violations(), Vec::<String>::new());
}

/// `/reload` recomputes the policy from the reloaded `compaction.*` settings and the actual next
/// after-reply decision follows it.
#[test]
fn reload_applies_changed_compaction_settings() {
    let env = TestEnv::new();
    write_settings(
        &env,
        r#"{"compaction": {"reserveTokens": 1024, "keepRecentTokens": 1}}"#,
    );
    let eng = TwoModels::new(
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text("r2"),
            ScriptTurn::text("SUMMARY"),
            ScriptTurn::text("r3"),
        ],
        true,
    );
    let mut s = open(&env, eng.clone());
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert!(compactions(&prompt(&mut s, "p1")).is_empty());

    write_settings(
        &env,
        r#"{"compaction": {"reserveTokens": 2048, "keepRecentTokens": 1}, "oxa": {"compaction": {"softTokens": 50}}}"#,
    );
    s.reload(&mut sink, &mut ui).expect("reload");
    let p = s.inner().compact;
    assert_eq!(p, expected(&s, MOE));
    assert_eq!((p.soft, p.keep_tokens), (50, 1));
    assert_eq!(p.hard, MOE.1 - MOE.2 - 2048);
    assert_eq!(
        compactions(&prompt(&mut s, "p2")),
        vec![CompactReason::Threshold],
        "reloaded softTokens"
    );

    write_settings(
        &env,
        r#"{"compaction": {"reserveTokens": 1024, "keepRecentTokens": 1, "enabled": false}, "oxa": {"compaction": {"softTokens": 50}}}"#,
    );
    s.reload(&mut sink, &mut ui).expect("reload disabled");
    assert!(!s.inner().compact.enabled);
    assert!(
        !s.state().auto_compaction,
        "a changed compaction.enabled follows the setting"
    );
    let ev = prompt(&mut s, "p3");
    assert!(!compacted(&ev), "compaction disabled by reload");
}

/// The runtime switch wins over `compaction.enabled`: with the setting off, a runtime enable
/// makes a run that ends above soft (below hard) compact; an unrelated reload keeps the
/// override; a runtime disable stops it; a reload that changes the setting applies it again.
#[test]
fn runtime_auto_compaction_overrides_the_setting() {
    let env = TestEnv::new();
    // `local` turns on turn-start compaction; this test is about the runtime switch, so keep
    // the after-reply compaction the only Threshold per prompt.
    let off = r#"{"compaction": {"enabled": false, "reserveTokens": 1024, "keepRecentTokens": 1}, "oxa": {"compaction": {"softTokens": 50, "turnStart": false}}}"#;
    write_settings(&env, off);
    let eng = TwoModels::new(
        vec![
            ScriptTurn::text("a1"),
            ScriptTurn::text("a2"),
            ScriptTurn::text("SUMMARY"),
            ScriptTurn::text("a3"),
            ScriptTurn::text("SUMMARY"),
            ScriptTurn::text("a4"),
            ScriptTurn::text("a5"),
            ScriptTurn::text("SUMMARY"),
        ],
        true,
    );
    let mut s = open(&env, eng.clone());
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert!(!s.state().auto_compaction);
    let ev = prompt(&mut s, "p1");
    assert!(s.inner().transcript.context() > 50 && s.inner().transcript.context() < s.inner().compact.hard);
    assert!(compactions(&ev).is_empty(), "the setting is off");

    s.set_auto_compaction(true);
    assert!(s.inner().compact.enabled);
    assert_eq!(
        compactions(&prompt(&mut s, "p2")),
        vec![CompactReason::Threshold],
        "runtime enable"
    );

    s.reload(&mut sink, &mut ui).expect("reload unchanged");
    assert!(
        s.state().auto_compaction && s.inner().compact.enabled,
        "override survives an unrelated reload"
    );
    assert_eq!(
        compactions(&prompt(&mut s, "p3")),
        vec![CompactReason::Threshold],
        "after reload"
    );

    s.set_auto_compaction(false);
    let ev = prompt(&mut s, "p4");
    assert!(s.inner().transcript.context() > 50);
    assert!(!compacted(&ev), "runtime disable");

    // The setting changes (off -> on): the reload applies it.
    write_settings(&env, &off.replace(r#""enabled": false"#, r#""enabled": true"#));
    s.reload(&mut sink, &mut ui).expect("reload enabled");
    assert!(s.state().auto_compaction);
    assert_eq!(
        compactions(&prompt(&mut s, "p5")),
        vec![CompactReason::Threshold],
        "reloaded setting"
    );
    assert_eq!(eng.chat.remaining(), 0);
}

/// Hard-overflow safety does not depend on the switch: with auto-compaction off, a run that
/// ends above the hard limit still compacts (reason `Overflow`).
#[test]
fn hard_overflow_compacts_with_auto_compaction_off() {
    let env = TestEnv::new();
    write_settings(
        &env,
        r#"{"compaction": {"reserveTokens": 1024, "keepRecentTokens": 1}}"#,
    );
    let eng = TwoModels::new(
        vec![
            ScriptTurn::text("h1"),
            ScriptTurn::text("h2"),
            ScriptTurn::text("SUMMARY"),
        ],
        true,
    );
    eng.load(FLASH.0);
    let mut s = open(&env, eng.clone());
    s.set_auto_compaction(false);
    assert!(
        compactions(&prompt(&mut s, "small")).is_empty(),
        "above soft, auto-compaction off"
    );
    let hard = s.inner().compact.hard;
    assert_eq!(hard, FLASH.1 - FLASH.2 - 1024);
    let ev = prompt(&mut s, &"x".repeat(hard as usize));
    assert!(s.inner().stats.compactions >= 1);
    assert_eq!(compactions(&ev), vec![CompactReason::Overflow]);
}
