//! Wave D D4b: the status bar's location segment learns where a model runs from the engine's
//! model info (`EngineInfo::caps.remote` / `remote_provider`, published by the controller's
//! `/v1/models` capability row). This test proves the bridge: a remote model's provider name
//! reaches `State::model_remote` / `model_provider` for anthropic, openai and openrouter, and a
//! local model stays `local`. The status bar itself renders those fields in oxa-interactive
//! (`d6_footer_header_title.rs`); here we pin the data it reads.
//! Isolation: use `oxa_fake::TestEnv`; never touch the real home.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::TestEnv;
use oxa_perm::ModePolicy;
use oxa_proto::{
    AssistantTurn, CancelToken, ChatRequest, Engine, EngineCaps, EngineInfo, OxaError, StreamEvent,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use std::sync::Arc;

/// An engine that reports a fixed `EngineInfo` (the remote/local capability of the model).
struct FixedEngine {
    info: EngineInfo,
}

impl Engine for FixedEngine {
    fn info(&self, _target: Option<&str>) -> Result<EngineInfo, OxaError> {
        Ok(self.info.clone())
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
        Err(OxaError::new(oxa_proto::codes::INTERNAL, "unused"))
    }
}

fn info_for(remote: bool, provider: Option<&'static str>) -> EngineInfo {
    EngineInfo {
        base_url: "fixed:".to_string(),
        models: vec!["m".to_string()],
        model: "m".to_string(),
        limits_for: "m".to_string(),
        n_ctx: 8192,
        max_output: 1024,
        queue: 0,
        compact_at: None,
        prefill_only: false,
        caps: EngineCaps {
            remote,
            remote_provider: provider,
            ..EngineCaps::default()
        },
        thinking: None,
        images: None,
    }
}

fn deps(env: &TestEnv, engine: Arc<dyn Engine>) -> Deps {
    Deps {
        engine,
        store: Arc::new(MemStore::new()),
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    }
}

/// The status bar's location segment: a remote model names its provider, a local one says local.
#[test]
fn state_reports_the_model_location_for_every_provider() {
    let env = TestEnv::new();
    for (remote, provider, want_remote, want_provider) in [
        (true, Some("anthropic"), true, Some("anthropic")),
        (true, Some("openai"), true, Some("openai")),
        (true, Some("openrouter"), true, Some("openrouter")),
        (false, None, false, None),
    ] {
        let engine = Arc::new(FixedEngine {
            info: info_for(remote, provider),
        });
        let s = Session::open(deps(&env, engine), Open::New).expect("open");
        let st = s.state();
        assert_eq!(st.model_remote, want_remote, "remote flag for {provider:?}");
        assert_eq!(st.model_provider, want_provider, "provider name for {provider:?}");
    }
}
