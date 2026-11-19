//! Dispatch native requests by catalog identity, never by switch precedence.
use oxa_proto::{
    codes, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent, ThinkingLevel,
};
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
    /// The models the owner's scope admitted. Empty means no scope restricted the catalog;
    /// otherwise a model outside it is not this run's to spend.
    pub admitted: Vec<String>,
    /// The gateway was admitted by the owner's Go switch alone: only its Go models may be
    /// reached, not another configured provider's metered alias.
    pub go_only: bool,
    /// The aliases the owner configured in `oxa.cloud.models`. Each one carries its own provider,
    /// so a configured alias is not a Go model whatever its name spells.
    pub metered: BTreeSet<String>,
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
    /// `true` when the owner scope that admitted this gateway names `model`. The effective
    /// settings may replace the scope with the project's, so selection must stay inside the
    /// admitted one: another provider's alias is not reached on a Go admission's authority.
    fn admitted(&self, model: &str) -> bool {
        if self.go_only && (self.metered.contains(model) || !is_go_model(model, &self.endpoint)) {
            return false;
        }
        self.admitted.is_empty()
            || oxa_config::resolve_scoped(
                &self.admitted,
                &model_refs(&self.endpoint, self.cloud_models.iter().cloned()),
            )
            .0
            .iter()
            .any(|model_ref| own_name(&model_ref.model, &self.endpoint) == own_name(model, &self.endpoint))
    }
    fn backend(&self, model: &str) -> Result<&dyn Engine, OxaError> {
        let backend = if self.local_models.contains(model) {
            &self.local
        } else if self.cloud_models.contains(model) && self.admitted(model) {
            &self.cloud
        } else if let Some(error) = self.dynamic_error(model) {
            return Err(error.clone());
        } else {
            return Err(OxaError::new(codes::INVALID, "Unknown native model."));
        };
        backend.as_deref().map_err(Clone::clone)
    }
    fn models(&self) -> Vec<String> {
        self.local_models
            .iter()
            .chain(self.cloud_models.iter().filter(|model| self.admitted(model)))
            .cloned()
            .collect()
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
    let available = model_refs(endpoint, models);
    let want = own_name(pattern, endpoint);
    // `oxe/opencode/x` and `opencode/x` name the same model: the resolver returns the latter.
    oxa_config::resolve_scoped(&[pattern.to_string()], &available)
        .0
        .iter()
        .any(|model| own_name(&model.model, endpoint) == want)
}

/// The name without a `:thinking` suffix or an endpoint prefix, case-folded.
fn own_name(model: &str, endpoint: &str) -> String {
    let normalized = model.trim().to_ascii_lowercase();
    // Model IDs may contain literal colons. Only the final, recognized thinking level is
    // selection metadata and may be removed when comparing catalog ownership.
    let bare = normalized
        .rsplit_once(':')
        .filter(|(_, suffix)| ThinkingLevel::parse(suffix).is_some())
        .map_or(normalized.as_str(), |(name, _)| name)
        .to_string();
    match bare.strip_prefix(&format!("{}/", endpoint.to_ascii_lowercase())) {
        Some(rest) if !endpoint.is_empty() => rest.to_string(),
        _ => bare,
    }
}

/// `true` when every entry of `scope` is owned by `models`: a concrete name must be an exact
/// model of that catalog (a fuzzy neighbour is not), while a wildcard may match one. A local
/// alias named `opencode/x-local` therefore cannot hide a requested `opencode/x`.
pub(super) fn scope_owns_in_catalog(
    scope: &[String],
    endpoint: &str,
    models: impl IntoIterator<Item = String> + Clone,
) -> bool {
    !scope.is_empty()
        && scope.iter().all(|pattern| {
            let pattern = pattern.trim();
            if pattern.contains(['*', '?']) {
                // A Go namespace wildcard can cover both a configured local alias and models
                // that are discoverable only from Go. One local hit cannot own that mixed scope.
                if is_go_model(pattern, endpoint) {
                    return false;
                }
                !oxa_config::resolve_scoped(&[pattern.to_string()], &model_refs(endpoint, models.clone()))
                    .0
                    .is_empty()
            } else {
                catalog_owns(pattern, endpoint, models.clone())
            }
        })
}
