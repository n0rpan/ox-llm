//! Typed, credential-free discovery for the in-process gateway.
//!
//! Construct once from trusted user settings. No engine, runtime, credential
//! file or provider connection is needed to list models or read their limits.
use std::fmt;
use url::{Host, Url};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    Anthropic,
    OpenRouter,
    OpenAi,
    ChatGpt,
    OpenCode,
}
impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::ChatGpt => "chatgpt",
            Self::OpenCode => "opencode",
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
    pub provider_name: &'a str,
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
    #[cfg(feature = "anthropic-gateway")]
    configured_route: Option<ConfiguredRoute>,
}

/// Provider identity and budget pool for a route from trusted user configuration.
/// The credential selector stays private to the gateway.
#[cfg(feature = "anthropic-gateway")]
#[derive(Clone)]
pub struct ConfiguredRoute {
    provider: String,
    pool: String,
    key: String,
}

#[cfg(feature = "anthropic-gateway")]
impl ConfiguredRoute {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn pool(&self) -> &str {
        &self.pool
    }
}

#[cfg(feature = "anthropic-gateway")]
fn is_builtin(value: &str) -> bool {
    [
        Provider::Anthropic,
        Provider::OpenRouter,
        Provider::OpenAi,
        Provider::ChatGpt,
        Provider::OpenCode,
    ]
    .iter()
    .any(|provider| value.eq_ignore_ascii_case(provider.as_str()))
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
        let loopback = match endpoint.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
            Some(Host::Domain(host)) => host == "localhost",
            None => false,
        };
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
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
            #[cfg(feature = "anthropic-gateway")]
            configured_route: None,
        })
    }

    /// Attach an HTTPS API route. Built-in credential entries stay bound to
    /// the built-in route configuration.
    #[cfg(feature = "anthropic-gateway")]
    pub fn with_configured_route(mut self, provider: &str, pool: &str, key: &str) -> Result<Self, Error> {
        if self.endpoint.scheme() != "https" {
            return Err(Error::InvalidEndpoint);
        }
        if !matches!(
            self.provider,
            Provider::Anthropic | Provider::OpenAi | Provider::OpenRouter
        ) || ![provider, pool, key]
            .into_iter()
            .all(oxa_config::valid_cloud_name)
            || is_builtin(provider)
            || is_builtin(key)
        {
            return Err(Error::InvalidModel);
        }
        self.configured_route = Some(ConfiguredRoute {
            provider: provider.into(),
            pool: pool.into(),
            key: key.into(),
        });
        Ok(self)
    }

    #[cfg(feature = "anthropic-gateway")]
    pub fn configured_route(&self) -> Option<&ConfiguredRoute> {
        self.configured_route.as_ref()
    }

    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn credential_name(&self) -> &str {
        self.configured_route()
            .map_or(self.provider.as_str(), |route| &route.key)
    }

    pub(crate) fn provider_name(&self) -> &str {
        #[cfg(feature = "anthropic-gateway")]
        if let Some(route) = self.configured_route() {
            return route.provider();
        }
        self.provider.as_str()
    }

    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn for_offer(
        &self,
        id: &str,
        upstream: &str,
        context: u32,
        output: u32,
    ) -> Result<Self, Error> {
        let mut offer = Self::new(
            id,
            self.provider,
            upstream,
            self.endpoint.as_str(),
            context,
            output,
        )?;
        offer.configured_route.clone_from(&self.configured_route);
        Ok(offer)
    }

    #[cfg(all(test, feature = "anthropic-gateway"))]
    pub(crate) fn set_mock_base(&mut self, base: &str) {
        assert!(base.starts_with("http://127.0.0.1:"));
        self.endpoint = Self::new(&self.id, self.provider, &self.upstream, base, 1, 1)
            .unwrap()
            .endpoint;
    }

    pub fn info(&self) -> ModelInfo<'_> {
        ModelInfo {
            id: &self.id,
            provider: self.provider,
            provider_name: self.provider_name(),
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
    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn configured(&self, id: &str) -> Option<&Model> {
        self.models
            .binary_search_by(|model| model.id.as_str().cmp(id))
            .ok()
            .map(|index| &self.models[index])
    }
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
