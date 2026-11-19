//! Dispatch native requests by catalog identity, never by switch precedence.
use oxa_proto::{codes, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent};
use std::{collections::BTreeSet, sync::Arc};

type Backend = Result<Arc<dyn Engine>, OxaError>;

pub(super) fn is_go_model(model: &str, endpoint: &str) -> bool {
    let model = model.trim();
    let mut parts = model.split('/');
    let first = parts.next().unwrap_or_default();
    let provider = if !endpoint.is_empty() && first.eq_ignore_ascii_case(endpoint) {
        parts.next().unwrap_or_default()
    } else {
        first
    };
    provider.eq_ignore_ascii_case("opencode") && parts.next().is_some()
}

pub(super) fn resolve_in_catalog(
    pattern: &str,
    endpoint: &str,
    models: impl IntoIterator<Item = String>,
) -> Option<String> {
    let available = model_refs(endpoint, models);
    oxa_config::resolve_model(pattern, None, &available)
        .ok()
        .map(|resolved| resolved.model)
}

pub(super) fn scope_includes_go(scope: &[String], endpoint: &str, models: Vec<String>) -> bool {
    oxa_config::resolve_scoped(scope, &model_refs(endpoint, models))
        .0
        .iter()
        .any(|model| is_go_model(&model.model, ""))
}

fn model_refs(endpoint: &str, models: impl IntoIterator<Item = String>) -> Vec<oxa_config::ModelRef> {
    models
        .into_iter()
        .map(|model| oxa_config::ModelRef {
            endpoint: endpoint.to_owned(),
            model,
            thinking: None,
        })
        .collect()
}

pub(super) struct Routed {
    pub local: Backend,
    pub cloud: Backend,
    pub local_models: BTreeSet<String>,
    pub cloud_models: BTreeSet<String>,
    pub go_error: Option<OxaError>,
    pub endpoint: String,
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
        let requested_known = requested
            .and_then(|pattern| resolve_in_catalog(pattern, &self.endpoint, self.models()))
            .is_some();
        if !requested_known {
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
        if is_go_model(model, &self.endpoint) {
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

/// `true` when the catalog owns `pattern` itself: a configured alias or model name equal to it,
/// or a configured glob that matches it. A fuzzy match on a *different* name (`…-local` matching
/// `…flash`) does not make the local catalog the owner, so it may not exclude the cloud model of
/// exactly that name.
pub(super) fn catalog_owns(pattern: &str, endpoint: &str, models: impl IntoIterator<Item = String>) -> bool {
    /// The name without a `:thinking` suffix, case-folded: the resolver's own spelling rules.
    fn bare(model: &str) -> String {
        let model = model.trim().to_ascii_lowercase();
        match model.find(':') {
            Some(i) => model[..i].to_string(),
            None => model,
        }
    }
    let available = model_refs(endpoint, models);
    // `oxe/opencode/x` and `opencode/x` name the same model: the resolver returns the latter.
    let without_endpoint = |model: &str| {
        let bare = bare(model);
        match bare.strip_prefix(&format!("{}/", endpoint.to_ascii_lowercase())) {
            Some(rest) if !endpoint.is_empty() => rest.to_string(),
            _ => bare,
        }
    };
    let want = without_endpoint(pattern);
    oxa_config::resolve_scoped(&[pattern.to_string()], &available)
        .0
        .iter()
        .any(|model| without_endpoint(&model.model) == want)
}

/// `true` when every pattern of `scope` resolves in `models` (the local catalog): such a scope
/// names local models only, so it may not open the cloud gateway.
pub(super) fn scope_resolves_in_catalog(
    scope: &[String],
    endpoint: &str,
    models: impl IntoIterator<Item = String>,
) -> bool {
    let available = model_refs(endpoint, models);
    !scope.is_empty()
        && scope.iter().all(|pattern| {
            !oxa_config::resolve_scoped(std::slice::from_ref(pattern), &available)
                .0
                .is_empty()
        })
}
