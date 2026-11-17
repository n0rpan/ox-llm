//! Dispatch native requests by catalog identity, never by switch precedence.
use oxa_proto::{codes, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent};
use std::{collections::BTreeSet, sync::Arc};

type Backend = Result<Arc<dyn Engine>, OxaError>;

pub(super) struct Routed {
    pub local: Backend,
    pub cloud: Backend,
    pub local_models: BTreeSet<String>,
    pub cloud_models: BTreeSet<String>,
    pub dynamic_go: bool,
    pub default: Option<String>,
}
impl Routed {
    pub fn into_engine(mut self, requested: Option<&str>) -> Result<Arc<dyn Engine>, OxaError> {
        if let Ok(info) = self
            .cloud
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|engine| engine.info(None))
        {
            self.cloud_models.extend(info.models);
        }
        if !self.local_models.is_disjoint(&self.cloud_models) {
            return Err(OxaError::new(
                codes::INVALID,
                "Native local and cloud model aliases must be distinct.",
            ));
        }
        // CLI model resolution needs a catalog first. If that catalog cannot
        // be discovered, retain the selected dynamic provider's saved failure
        // before the caller can replace it with a model-not-found error.
        if let Some(error) = requested.and_then(|model| self.dynamic_error(model)) {
            return Err(error.clone());
        }
        if self.local.is_err() && self.cloud.is_err() {
            return Err(self.cloud.err().unwrap());
        }
        Ok(Arc::new(self))
    }
    fn dynamic_error(&self, model: &str) -> Option<&OxaError> {
        if self.dynamic_go && model.starts_with("opencode/") && !self.local_models.contains(model) {
            self.cloud.as_ref().err()
        } else {
            None
        }
    }
    fn backend(&self, model: &str) -> Result<&dyn Engine, OxaError> {
        let backend = if self.local_models.contains(model) {
            &self.local
        } else if self.cloud_models.contains(model) {
            &self.cloud
        } else if let Some(error) = self.dynamic_error(model) {
            return Err(error.clone());
        } else {
            return Err(OxaError::new(codes::INVALID, "Unknown native model."));
        };
        backend.as_deref().map_err(Clone::clone)
    }
    fn models(&self) -> Vec<String> {
        self.local_models.union(&self.cloud_models).cloned().collect()
    }
}
impl Engine for Routed {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let models = self.models();
        if let Some(model) = target {
            let mut info = self.backend(model)?.info(Some(model))?;
            info.models = models;
            return Ok(info);
        }
        // Discovery can still describe local models offline. This metadata
        // choice never changes the backend of a resolved request or its refusal.
        let candidates = self.default.iter().chain(models.iter());
        let mut failure = None;
        for model in candidates {
            match self.info(Some(model)) {
                Ok(info) => return Ok(info),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        Err(failure.unwrap_or_else(|| OxaError::new(codes::INVALID, "No configured native models.")))
    }
    fn count_tokens(&self, _text: &str) -> Result<u32, OxaError> {
        Err(OxaError::unsupported(
            "exact token counts without a resolved model",
        ))
    }
    fn count_prompt(&self, request: &ChatRequest<'_>) -> Result<Option<u64>, OxaError> {
        self.backend(request.model)?.count_prompt(request)
    }
    fn chat(
        &self,
        request: &ChatRequest<'_>,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        self.backend(request.model)?.chat(request, cancel, on)
    }
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        // Embedded cloud has no hint-clear operation. Keep local lifecycle
        // cleanup even when the last completed request used a cloud model.
        if let Ok(local) = &self.local {
            local.clear_hint(hint)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::{EngineCaps, FrozenPrefix};
    use std::sync::Mutex;

    struct Probe(&'static str, Arc<Mutex<Vec<String>>>);
    impl Engine for Probe {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            assert!(target.is_none() || target == Some(self.0));
            Ok(EngineInfo {
                base_url: "synthetic".into(),
                models: vec![self.0.into()],
                model: self.0.into(),
                limits_for: self.0.into(),
                n_ctx: 4096,
                max_output: 64,
                queue: 0,
                compact_at: None,
                prefill_only: false,
                caps: EngineCaps {
                    remote: self.0 == "cloud",
                    ..EngineCaps::default()
                },
                thinking: None,
                images: None,
            })
        }
        fn count_tokens(&self, _: &str) -> Result<u32, OxaError> {
            panic!("unscoped count")
        }
        fn count_prompt(&self, req: &ChatRequest<'_>) -> Result<Option<u64>, OxaError> {
            assert_eq!(req.model, self.0);
            Ok(Some(if self.0 == "local" { 7 } else { 11 }))
        }
        fn chat(
            &self,
            req: &ChatRequest<'_>,
            cancel: &CancelToken,
            _: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            assert_eq!(req.model, self.0);
            self.1.lock().unwrap().push(req.model.into());
            Err(OxaError::new(
                if cancel.is_cancelled() {
                    codes::ABORTED
                } else {
                    "synthetic-refusal"
                },
                "Synthetic refusal",
            ))
        }
        fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
            self.1.lock().unwrap().push(format!("{}:{hint}", self.0));
            Ok(())
        }
    }
    fn routed(cloud: Backend, calls: Arc<Mutex<Vec<String>>>) -> Routed {
        Routed {
            local: Ok(Arc::new(Probe("local", calls))),
            cloud,
            local_models: BTreeSet::from(["local".into()]),
            cloud_models: BTreeSet::from(["cloud".into()]),
            dynamic_go: false,
            default: Some("cloud".into()),
        }
    }
    #[test]
    fn resolved_model_routes_discovery_chat_counts_and_refusal_without_fallback() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = routed(Ok(Arc::new(Probe("cloud", calls.clone()))), calls.clone())
            .into_engine(None)
            .unwrap();
        assert_eq!(engine.info(None).unwrap().model, "cloud");
        let prefix = FrozenPrefix {
            sections: vec![],
            tools_json: "[]".into(),
            sha256: [0; 32],
        };
        for (model, count) in [("local", 7), ("cloud", 11), ("local", 7)] {
            let info = engine.info(Some(model)).unwrap();
            assert_eq!(info.limits_for, model);
            assert_eq!(info.caps.remote, model == "cloud");
            assert_eq!(info.models, ["cloud", "local"]);
            let req = ChatRequest::new(&prefix, &[], model);
            assert_eq!(engine.count_prompt(&req).unwrap(), Some(count));
            assert_eq!(
                engine
                    .chat(&req, &CancelToken::new(), &mut |_| {})
                    .unwrap_err()
                    .code,
                "synthetic-refusal"
            );
        }
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_eq!(
            engine
                .chat(&ChatRequest::new(&prefix, &[], "cloud"), &cancel, &mut |_| {})
                .unwrap_err()
                .code,
            codes::ABORTED
        );
        engine.clear_hint("synthetic-session").unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["local", "cloud", "local", "cloud", "local:synthetic-session"]
        );
        assert!(engine.info(Some("unknown")).is_err());
    }
    #[test]
    fn offline_cloud_does_not_hide_local_discovery_or_fall_back_for_cloud_requests() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = routed(Err(oxa_proto::offline_error("synthetic cloud")), calls.clone())
            .into_engine(None)
            .unwrap();
        assert_eq!(engine.info(None).unwrap().model, "local");
        assert_eq!(engine.info(Some("cloud")).unwrap_err().code, codes::OFFLINE);
        assert!(calls.lock().unwrap().is_empty());
    }
    #[test]
    fn duplicate_native_aliases_refuse_before_dispatch() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut router = routed(Ok(Arc::new(Probe("cloud", calls.clone()))), calls.clone());
        router.local_models.insert("cloud".into());
        assert!(router.into_engine(None).is_err());
        assert!(calls.lock().unwrap().is_empty());
    }
    #[test]
    fn dynamic_go_retains_saved_failure_without_static_alias_or_execution() {
        let prefix = FrozenPrefix {
            sections: vec![],
            tools_json: "[]".into(),
            sha256: [0; 32],
        };
        for saved in [
            oxa_proto::offline_error("synthetic Go"),
            OxaError::new(
                "cloud_credentials_unavailable",
                "Synthetic credentials unavailable",
            ),
            OxaError::new("cloud_discovery_invalid", "Synthetic discovery invalid"),
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut router = routed(Err(saved.clone()), calls.clone());
            router.cloud_models.clear();
            router.dynamic_go = true;
            let engine = router.into_engine(None).unwrap();
            assert_eq!(engine.info(None).unwrap().model, "local");
            let req = ChatRequest::new(&prefix, &[], "opencode/synthetic");
            assert_eq!(engine.info(Some(req.model)).unwrap_err(), saved);
            assert_eq!(engine.count_prompt(&req).unwrap_err(), saved);
            assert_eq!(
                engine.chat(&req, &CancelToken::new(), &mut |_| {}).unwrap_err(),
                saved
            );
            assert!(calls.lock().unwrap().is_empty());
            let mut startup = routed(Err(saved.clone()), calls.clone());
            startup.cloud_models.clear();
            startup.dynamic_go = true;
            assert_eq!(startup.into_engine(Some(req.model)).err().unwrap(), saved);
        }
    }
    #[test]
    fn disabled_or_successfully_discovered_go_does_not_claim_unknown_models() {
        for enabled in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let cloud = if enabled {
                Ok(Arc::new(Probe("cloud", calls.clone())) as Arc<dyn Engine>)
            } else {
                Err(oxa_proto::offline_error("synthetic Go"))
            };
            let mut router = routed(cloud, calls.clone());
            router.cloud_models.clear();
            router.dynamic_go = enabled;
            let engine = router.into_engine(Some("opencode/unknown")).unwrap();
            let error = engine.info(Some("opencode/unknown")).unwrap_err();
            assert_eq!(error.code, codes::INVALID);
            assert_eq!(error.message, "Unknown native model.");
            assert!(calls.lock().unwrap().is_empty());
        }
    }
}
