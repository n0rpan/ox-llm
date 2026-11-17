//! Offline mode is a hard error for a remote model on every path that selects or uses one:
//! `/model` and RPC `set_model` (`Session::set_model`), the host's `SetModel` (hooks, extensions)
//! all end in `switch_model`; a chat request is refused when the selected model is remote.
//! The engine here is a fake that does NOT refuse by itself (the refusal in `parse_info_for` is
//! the engine client's; callers must not depend on it) plus one that does and whose error the
//! callers used to swallow. Process policy is global: the tests take one lock.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::TestEnv;
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, set_net_policy, AssistantTurn, CancelToken, ChatRequest, Engine, EngineCaps, EngineInfo,
    HeadlessUi, OxaError, StreamEvent, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

static POLICY: Mutex<()> = Mutex::new(());

struct Policy;
impl Drop for Policy {
    fn drop(&mut self) {
        set_net_policy(false, false);
    }
}

/// `cloud-m` is remote. `refuses` makes the engine itself fail with the offline error (the real
/// engine client's behaviour) instead of describing the model.
struct Fake {
    refuses: bool,
    chats: AtomicU32,
}

impl Engine for Fake {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let m = target.unwrap_or("local-m");
        let remote = m == "cloud-m";
        if remote && self.refuses && oxa_proto::offline() {
            return Err(oxa_proto::offline_error("the cloud model cloud-m"));
        }
        Ok(EngineInfo {
            base_url: "fixed:".to_string(),
            models: vec!["local-m".to_string(), "cloud-m".to_string()],
            model: m.to_string(),
            limits_for: m.to_string(),
            n_ctx: 200_000,
            max_output: 1024,
            queue: 0,
            compact_at: None,
            prefill_only: false,
            caps: EngineCaps {
                remote,
                remote_provider: remote.then_some("openrouter"),
                ..EngineCaps::default()
            },
            thinking: None,
            images: None,
        })
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        Ok(text.len() as u32)
    }
    fn chat(
        &self,
        _r: &ChatRequest,
        _cancel: &CancelToken,
        _on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        self.chats.fetch_add(1, Ordering::SeqCst);
        Err(OxaError::new(codes::INTERNAL, "the fake engine does not answer"))
    }
}

fn session(env: &TestEnv, refuses: bool) -> (Session, Arc<Fake>) {
    let engine = Arc::new(Fake {
        refuses,
        chats: AtomicU32::new(0),
    });
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        jobs: oxa_core::fresh_jobs(),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: Some("local-m".to_string()),
        thinking: None,
        mode: None,
        session_id: None,
    };
    (Session::open(deps, Open::New).expect("open"), engine)
}

#[test]
fn failed_remote_request_leaves_session_cost_unknown_after_local_switch() {
    let _lock = POLICY.lock().unwrap_or_else(|e| e.into_inner());
    let _reset = Policy;
    set_net_policy(false, false);
    let env = TestEnv::new();
    let (mut s, engine) = session(&env, false);
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert!(!s.stats().cost_unknown);
    s.set_model("cloud-m", &mut sink).expect("cloud switch");
    // Selecting a cloud model alone does not incur a charge.
    assert!(!s.stats().cost_unknown);
    let _ = s.prompt("Synthetic request".into(), Vec::new(), &mut sink, &mut ui);
    assert!(engine.chats.load(Ordering::SeqCst) > 0);
    assert!(s.stats().cost_unknown, "failed remote calls can still be billed");
    s.set_model("local-m", &mut sink).expect("local switch");
    assert!(
        s.stats().cost_unknown,
        "switching models cannot erase unknown cost"
    );
}

#[test]
fn offline_switch_to_a_remote_model_is_a_hard_error() {
    let _lock = POLICY.lock().unwrap_or_else(|e| e.into_inner());
    let _reset = Policy;
    let env = TestEnv::new();
    for refuses in [false, true] {
        let (mut s, _) = session(&env, refuses);
        set_net_policy(true, false);
        let mut sink = VecSink::default();
        let err = s.set_model("cloud-m", &mut sink).expect_err("must refuse");
        assert_eq!(err.code, codes::OFFLINE, "refuses={refuses}: {err}");
        assert_eq!(s.state().model, "local-m", "the model is unchanged");
        // a local target still works offline
        s.set_model("local-m", &mut sink).expect("local switch");
        // and online the remote switch works
        set_net_policy(false, false);
        s.set_model("cloud-m", &mut sink).expect("online switch");
        assert!(s.state().model_remote);
    }
}

#[test]
fn offline_chat_on_a_remote_model_is_refused_before_the_engine() {
    let _lock = POLICY.lock().unwrap_or_else(|e| e.into_inner());
    let _reset = Policy;
    let env = TestEnv::new();
    for refuses in [false, true] {
        let (mut s, engine) = session(&env, refuses);
        let mut sink = VecSink::default();
        s.set_model("cloud-m", &mut sink).expect("online switch");
        set_net_policy(true, false);
        let mut ui = HeadlessUi::default();
        let before = engine.chats.load(Ordering::SeqCst);
        let result = s.prompt("hello".to_string(), Vec::new(), &mut sink, &mut ui);
        let text = format!("{result:?} {:?}", sink.0);
        assert!(text.contains("offline mode is on"), "refuses={refuses}: {text}");
        assert_eq!(engine.chats.load(Ordering::SeqCst), before, "nothing was sent");
        set_net_policy(false, false);
    }
}
