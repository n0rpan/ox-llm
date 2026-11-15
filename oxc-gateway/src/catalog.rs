//! Typed, credential-free discovery for the in-process gateway.
//!
//! Construct once from trusted user settings. No engine, runtime, credential
//! file or provider connection is needed to list models or read their limits.
use std::fmt;
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    Anthropic,
    OpenRouter,
    OpenAi,
}
impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
        }
    }
}

/// Offline wins over an enabled user setting. Configuration must select
/// Offline whenever the client's offline policy is active.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Access {
    #[default]
    Disabled,
    Offline,
    Enabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidModel,
    InvalidEndpoint,
    InvalidLimits,
    DuplicateModel,
    Disabled,
    Offline,
    UnknownModel,
}

/// Public model metadata. Transport addresses and upstream IDs stay private.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelInfo<'a> {
    pub id: &'a str,
    pub provider: Provider,
    pub context_window: u32,
    pub max_output: u32,
}

#[derive(Clone)]
pub struct Model {
    id: String,
    provider: Provider,
    upstream: String,
    endpoint: Url,
    context_window: u32,
    max_output: u32,
}
impl fmt::Debug for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.info().fmt(f)
    }
}
impl Model {
    pub fn new(
        id: impl Into<String>,
        provider: Provider,
        upstream: impl Into<String>,
        endpoint: &str,
        context_window: u32,
        max_output: u32,
    ) -> Result<Self, Error> {
        let id = id.into();
        let upstream = upstream.into();
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
            || upstream.trim().is_empty()
            || upstream.len() > 1024
            || upstream.chars().any(char::is_control)
        {
            return Err(Error::InvalidModel);
        }
        if context_window == 0 || max_output == 0 || max_output > context_window {
            return Err(Error::InvalidLimits);
        }
        let endpoint = Url::parse(endpoint).map_err(|_| Error::InvalidEndpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(Error::InvalidEndpoint);
        }
        Ok(Self {
            id,
            provider,
            upstream,
            endpoint,
            context_window,
            max_output,
        })
    }
    pub fn info(&self) -> ModelInfo<'_> {
        ModelInfo {
            id: &self.id,
            provider: self.provider,
            context_window: self.context_window,
            max_output: self.max_output,
        }
    }
    pub fn upstream(&self) -> &str {
        &self.upstream
    }
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }
}

/// Immutable catalog. Discovery borrows metadata without allocating JSON;
/// a network adapter can serialize only at its external boundary.
#[derive(Clone, Debug)]
pub struct Catalog {
    access: Access,
    models: Vec<Model>,
}
impl Catalog {
    pub fn new(access: Access, models: impl IntoIterator<Item = Model>) -> Result<Self, Error> {
        let mut models: Vec<_> = models.into_iter().collect();
        models.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        if models.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(Error::DuplicateModel);
        }
        Ok(Self { access, models })
    }
    pub fn models(&self) -> impl Iterator<Item = ModelInfo<'_>> {
        let visible = if self.access == Access::Enabled {
            self.models.as_slice()
        } else {
            &[]
        };
        visible.iter().map(Model::info)
    }
    /// Only an omitted ID selects the first model in stable ID order. A typo
    /// must never silently select a different model or provider.
    pub fn resolve(&self, id: Option<&str>) -> Result<&Model, Error> {
        match self.access {
            Access::Disabled => return Err(Error::Disabled),
            Access::Offline => return Err(Error::Offline),
            Access::Enabled => {}
        }
        let index = match id {
            Some(id) => self
                .models
                .binary_search_by(|model| model.id.as_str().cmp(id))
                .ok(),
            None => Some(0),
        };
        index
            .and_then(|index| self.models.get(index))
            .ok_or(Error::UnknownModel)
    }
}
