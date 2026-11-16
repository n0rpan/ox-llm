//! Off-request-path OpenAI API and ChatGPT subscription model discovery.
//! Opt-in through `upstream-openai,anthropic-client`; no cache or catalog wiring.
use crate::secrets::Key;
use std::{fmt, time::Duration};

const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Endpoint,
    Setup,
    Transport,
    Http(u16),
    Protocol,
    TooLarge,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpenAI model discovery failed: {self:?}")
    }
}
impl std::error::Error for Error {}

/// API discovery supplies no context limits or prices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiModel {
    pub id: String,
    pub shutdown_date: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningLevel {
    pub effort: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionModel {
    pub slug: String,
    pub context_window: u64,
    pub auto_compact_token_limit: Option<u64>,
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
    pub input_modalities: Vec<String>,
    /// Preserved, not filtered: the caller decides how to display hidden models.
    pub visibility: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionModels {
    Modified {
        models: Vec<SubscriptionModel>,
        etag: Option<String>,
    },
    /// Keep the caller's cached models. An absent ETag preserves its validator.
    NotModified { etag: Option<String> },
}

/// Stateless discovery pool. The injected base is the API root (`.../v1`) or
/// subscription root; both methods append `models`. No credentials are retained.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    timeout: Duration,
}
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpenAIModelsClient([configured])")
    }
}
impl Client {
    /// HTTPS only, except HTTP loopback mocks. Redirects and environment proxies
    /// are disabled. `timeout` bounds the entire GET, including the response body.
    pub fn new(base: &str, timeout: Duration) -> Result<Self, Error> {
        let mut base = reqwest::Url::parse(base).map_err(|_| Error::Endpoint)?;
        let loopback = match base.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
            Some(url::Host::Domain(host)) => host == "localhost",
            None => false,
        };
        if !(base.scheme() == "https" || base.scheme() == "http" && loopback)
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || timeout.is_zero()
        {
            return Err(Error::Endpoint);
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let endpoint = base.join("models").map_err(|_| Error::Endpoint)?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Setup)?;
        Ok(Self {
            http,
            endpoint,
            timeout,
        })
    }

    /// The base must include `/v1` for the OpenAI API. No limits or prices are
    /// inferred from an id; overlay/fallback policy belongs to the caller.
    pub async fn api_models(&self, key: &Key) -> Result<Vec<ApiModel>, Error> {
        let (_, body) = self.get(key, None, None).await?;
        let response: serde_json::Value =
            serde_json::from_slice(&body.ok_or(Error::Protocol)?).map_err(|_| Error::Protocol)?;
        response["data"]
            .as_array()
            .ok_or(Error::Protocol)?
            .iter()
            .map(|model| {
                Ok(ApiModel {
                    id: text(&model["id"])?.into(),
                    shutdown_date: match &model["shutdown_date"] {
                        serde_json::Value::Null => None,
                        value => Some(text(value)?.into()),
                    },
                })
            })
            .collect()
    }

    /// Query the subscription's injected base. ETags are opaque, including weak
    /// validators and quotes; the caller owns storage and passes its prior ETag.
    /// A 304 is accepted only for a conditional request, never as an empty list.
    pub async fn subscription_models(
        &self,
        key: &Key,
        client_version: &str,
        etag: Option<&str>,
    ) -> Result<SubscriptionModels, Error> {
        if client_version.is_empty() {
            return Err(Error::Setup);
        }
        let (etag, body) = self.get(key, Some(client_version), etag).await?;
        let Some(body) = body else {
            return Ok(SubscriptionModels::NotModified { etag });
        };
        let response: serde_json::Value = serde_json::from_slice(&body).map_err(|_| Error::Protocol)?;
        let models = response["models"]
            .as_array()
            .ok_or(Error::Protocol)?
            .iter()
            .map(subscription_model)
            .collect::<Result<_, _>>()?;
        Ok(SubscriptionModels::Modified { models, etag })
    }

    async fn get(
        &self,
        key: &Key,
        client_version: Option<&str>,
        etag: Option<&str>,
    ) -> Result<(Option<String>, Option<Vec<u8>>), Error> {
        tokio::time::timeout(self.timeout, self.get_inner(key, client_version, etag))
            .await
            .map_err(|_| Error::Transport)?
    }

    async fn get_inner(
        &self,
        key: &Key,
        client_version: Option<&str>,
        etag: Option<&str>,
    ) -> Result<(Option<String>, Option<Vec<u8>>), Error> {
        use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, ETAG, IF_NONE_MATCH};
        let mut endpoint = self.endpoint.clone();
        if let Some(version) = client_version {
            endpoint.query_pairs_mut().append_pair("client_version", version);
        }
        let mut bearer =
            HeaderValue::from_str(&format!("Bearer {}", key.expose())).map_err(|_| Error::Setup)?;
        bearer.set_sensitive(true);
        let mut request = self
            .http
            .get(endpoint)
            .header(AUTHORIZATION, bearer)
            .header(ACCEPT, "application/json");
        if let Some(etag) = etag {
            if etag.is_empty() {
                return Err(Error::Setup);
            }
            request = request.header(
                IF_NONE_MATCH,
                HeaderValue::from_str(etag).map_err(|_| Error::Setup)?,
            );
        }
        let mut response = request.send().await.map_err(|_| Error::Transport)?;
        let status = response.status().as_u16();
        if status != 200 && status != 304 {
            // Do not parse, log, or return upstream error bodies or reqwest errors:
            // they may contain credentials or sensitive endpoint information.
            return Err(Error::Http(status));
        }
        if status == 304 && client_version.is_none() {
            return Err(Error::Http(status));
        }
        if status == 304 && etag.is_none() {
            return Err(Error::Protocol);
        }
        let returned_etag = response
            .headers()
            .get(ETAG)
            .map(|value| value.to_str().map(str::to_owned).map_err(|_| Error::Protocol))
            .transpose()?;
        if status == 304 {
            return Ok((returned_etag, None));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BODY as u64)
        {
            return Err(Error::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if chunk.len() > MAX_BODY - body.len() {
                return Err(Error::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((returned_etag, Some(body)))
    }
}

fn text(value: &serde_json::Value) -> Result<&str, Error> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or(Error::Protocol)
}

fn subscription_model(model: &serde_json::Value) -> Result<SubscriptionModel, Error> {
    let levels = model["supported_reasoning_levels"]
        .as_array()
        .ok_or(Error::Protocol)?
        .iter()
        .map(|level| {
            Ok(ReasoningLevel {
                effort: text(&level["effort"])?.into(),
                description: level["description"].as_str().ok_or(Error::Protocol)?.into(),
            })
        })
        .collect::<Result<_, Error>>()?;
    let modalities = model["input_modalities"]
        .as_array()
        .ok_or(Error::Protocol)?
        .iter()
        .map(|value| text(value).map(str::to_owned))
        .collect::<Result<_, _>>()?;
    Ok(SubscriptionModel {
        slug: text(&model["slug"])?.into(),
        context_window: model["context_window"]
            .as_u64()
            .filter(|size| *size > 0)
            .ok_or(Error::Protocol)?,
        auto_compact_token_limit: match &model["auto_compact_token_limit"] {
            serde_json::Value::Null => None,
            value => Some(value.as_u64().ok_or(Error::Protocol)?),
        },
        supported_reasoning_levels: levels,
        input_modalities: modalities,
        visibility: text(&model["visibility"])?.into(),
    })
}

#[cfg(test)]
mod tests;
