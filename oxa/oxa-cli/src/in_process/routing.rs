//! Dispatch native requests by catalog identity, never by switch precedence.
use oxa_proto::{codes, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent};
use std::{collections::BTreeSet, sync::Arc};

type Backend = Result<Arc<dyn Engine>, OxaError>;

pub(super) fn is_go_model(model: &str) -> bool {
    model
        .trim()
        .split_once('/')
        .is_some_and(|(provider, _)| provider.eq_ignore_ascii_case("opencode"))
}

pub(super) struct Routed {
    pub local: Backend,
    pub cloud: Backend,
    pub local_models: BTreeSet<String>,
    pub cloud_models: BTreeSet<String>,
    pub go_error: Option<OxaError>,
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
            self.cloud_models = info.models.into_iter().collect();
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
        let requested_local = requested.is_some_and(|pattern| {
            // Preflight only catalog IDs, without opening either backend. The
            // caller remains responsible for endpoint/provider resolution.
            let available: Vec<_> = self
                .models()
                .into_iter()
                .map(|model| oxa_config::ModelRef {
                    endpoint: String::new(),
                    model,
                    thinking: None,
                })
                .collect();
            oxa_config::resolve_model(pattern, None, &available)
                .is_ok_and(|resolved| self.local_models.contains(&resolved.model))
        });
        if !requested_local {
            if let Some(error) = requested.and_then(|model| self.dynamic_error(model)) {
                return Err(error.clone());
            }
        }
        if self.local.is_err() && self.cloud.is_err() {
            return Err(self.cloud.err().unwrap());
        }
        Ok(Arc::new(self))
    }
    fn dynamic_error(&self, model: &str) -> Option<&OxaError> {
        if is_go_model(model) && !self.local_models.contains(model) {
            self.go_error.as_ref().or_else(|| self.cloud.as_ref().err())
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
    fn local_memory_status(&self, model: &str) -> Option<(u32, &'static str, u64)> {
        if model.is_empty() {
            self.local.as_ref().ok()?.local_memory_status("")
        } else {
            self.backend(model).ok()?.local_memory_status(model)
        }
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
