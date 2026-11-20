//! Off-request-path Anthropic discovery and refusal classification.
//! No caching or request handling is performed here. Unknown wire fields are ignored.
use super::{messages_endpoint, resource_endpoint, Client, Error};
use crate::secrets::Key;
use reqwest::header::HeaderValue;
use serde_json::Value;
use std::{collections::BTreeSet, time::Duration};

const MAX_PAGES: usize = 100;
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_MODELS: usize = 8192;
const MAX_ID_BYTES: usize = 128;
const MAX_METADATA_BYTES: usize = 256;
const MAX_CAPABILITIES: usize = 128;

/// Provider metadata. Limits may be absent on older API responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub display_name: String,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub capabilities: BTreeSet<String>,
    /// Provider retirement timestamp, preserved verbatim (not deprecation time).
    pub retires_at: Option<String>,
}

impl Client {
    /// Fetch a complete catalog, or fail without exposing a partial catalog.
    /// `base_url` is a host root or API base ending in `/v1`; HTTPS is required
    /// except on loopback. `timeout` bounds the entire refresh, including pages.
    /// Redirects and proxies remain disabled by the shared transport.
    pub async fn models(&self, base_url: &str, key: &Key, timeout: Duration) -> Result<Vec<Model>, Error> {
        self.models_at(base_url, key, timeout, false).await
    }

    pub(crate) async fn models_at(
        &self,
        base_url: &str,
        key: &Key,
        timeout: Duration,
        named: bool,
    ) -> Result<Vec<Model>, Error> {
        tokio::time::timeout(timeout, self.discover_models(base_url, key, named))
            .await
            .map_err(|_| Error::Transport)?
    }

    async fn discover_models(&self, base_url: &str, key: &Key, named: bool) -> Result<Vec<Model>, Error> {
        let endpoint = if named {
            resource_endpoint(base_url, &["models"])?
        } else {
            let mut endpoint = messages_endpoint(base_url)?;
            endpoint
                .path_segments_mut()
                .map_err(|_| Error::Endpoint)?
                .pop()
                .push("models");
            endpoint
        };
        let mut credential = HeaderValue::from_str(key.expose()).map_err(|_| Error::Setup)?;
        credential.set_sensitive(true);
        let mut models = Vec::new();
        let mut remaining_bytes = MAX_CATALOG_BYTES;
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = endpoint.clone();
            url.query_pairs_mut().append_pair("limit", "100");
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("after_id", cursor);
            }
            let mut response = self
                .0
                .get(url)
                .header("x-api-key", credential.clone())
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|_| Error::Transport)?;
            if response.status().as_u16() != 200 {
                return Err(Error::Http(response.status().as_u16()));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
                if chunk.len() > MAX_PAGE_BYTES.saturating_sub(bytes.len()) || chunk.len() > remaining_bytes {
                    return Err(Error::TooLarge);
                }
                remaining_bytes -= chunk.len();
                bytes.extend_from_slice(&chunk);
            }
            let page: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
            if named && crate::redaction::contains_secret(&page, key.expose()) {
                return Err(Error::Protocol);
            }
            let data = page["data"].as_array().ok_or(Error::Protocol)?;
            if data.len() > MAX_MODELS.saturating_sub(models.len()) {
                return Err(Error::TooLarge);
            }
            for item in data {
                models.push(parse_model(item)?);
            }
            let more = page["has_more"].as_bool().ok_or(Error::Protocol)?;
            if !more {
                return Ok(models);
            }
            cursor = Some(
                page["last_id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
                    .ok_or(Error::Protocol)?
                    .to_owned(),
            );
        }
        Err(Error::Protocol)
    }
}

fn parse_model(item: &Value) -> Result<Model, Error> {
    let optional_limit = |field: &str| match &item[field] {
        Value::Null => Ok(None),
        value => value.as_u64().map(Some).ok_or(Error::Protocol),
    };
    let mut capabilities = BTreeSet::new();
    if !item["capabilities"].is_null() {
        let fields = item["capabilities"].as_object().ok_or(Error::Protocol)?;
        if fields.len() > MAX_CAPABILITIES {
            return Err(Error::TooLarge);
        }
        for (name, feature) in fields {
            if name.len() > MAX_METADATA_BYTES {
                return Err(Error::Protocol);
            }
            if feature["supported"].as_bool().ok_or(Error::Protocol)? {
                capabilities.insert(name.clone());
            }
        }
    }
    let text = |field: &str, maximum: usize| {
        item[field]
            .as_str()
            .filter(|text| text.len() <= maximum)
            .map(str::to_owned)
            .ok_or(Error::Protocol)
    };
    let retires_at = match &item["retires_at"] {
        Value::Null => None,
        _ => Some(text("retires_at", MAX_METADATA_BYTES)?),
    };
    Ok(Model {
        id: text("id", MAX_ID_BYTES)?,
        display_name: text("display_name", MAX_METADATA_BYTES)?,
        max_input_tokens: optional_limit("max_input_tokens")?,
        max_output_tokens: optional_limit("max_tokens")?,
        capabilities,
        retires_at,
    })
}

/// Safe refusal categories: never retain provider messages or raw headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    OutOfCredit,
    RateLimited { retry_after: Option<Duration> },
    Overloaded,
}
impl Refusal {
    pub fn is_retryable(self) -> bool {
        !matches!(self, Self::OutOfCredit)
    }
}

/// Classify only matching HTTP status/native error-type pairs. Anthropic's
/// `retry-after` is seconds; invalid values are discarded, never echoed.
/// Unrecognized or malformed bodies are left to the caller's generic HTTP error.
pub fn classify_refusal(status: u16, body: &[u8], retry_after: Option<&str>) -> Option<Refusal> {
    let body: Value = serde_json::from_slice(body).ok()?;
    let error = &body["error"];
    match (status, error["type"].as_str()?) {
        (400, "invalid_request_error")
            if error["message"]
                .as_str()?
                .to_ascii_lowercase()
                .contains("credit balance is too low") =>
        {
            Some(Refusal::OutOfCredit)
        }
        (429, "rate_limit_error") => Some(Refusal::RateLimited {
            retry_after: retry_after
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
        }),
        (529, "overloaded_error") => Some(Refusal::Overloaded),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets;
    use serde_json::json;
    use std::{
        fs::{self, OpenOptions},
        io::{Read, Write},
        net::TcpListener,
        os::unix::fs::OpenOptionsExt,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };

    const SECRET: &str = "synthetic-discovery-credential";

    fn key() -> Key {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "gateway-anthropic-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        write!(file, "{}", json!({"anthropic":{"api_key":SECRET}})).unwrap();
        let key = secrets::read(&path, "anthropic").unwrap();
        fs::remove_file(path).unwrap();
        key
    }

    fn mock(replies: Vec<(u16, String, Duration)>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let mut requests = vec![];
            for (status, body, delay) in replies {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = vec![];
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                requests.push(String::from_utf8(request).unwrap());
                thread::sleep(delay);
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            requests
        });
        (base, worker)
    }

    fn page(data: Value, more: bool, last: Value) -> String {
        json!({"data":data,"has_more":more,"last_id":last,"future_field":true}).to_string()
    }

    #[test]
    fn catalog_parser_refuses_oversized_model_ids() {
        let id = "m".repeat(129);
        let model = json!({"id":id,"display_name":"Synthetic"});
        assert!(matches!(parse_model(&model), Err(Error::Protocol)));
    }

    #[tokio::test]
    async fn discovers_two_pages_and_optional_fields() {
        let first = page(
            json!([{"id":"model/a?b","display_name":"Synthetic A","created_at":"2026-01-01T00:00:00Z", "max_input_tokens":200000,"max_tokens":64000,"capabilities":{"tools":{"supported":true,"future":42},"vision":{"supported":false},"thinking":{"supported":true}},"deprecated_at":"2026-11-01","retires_at":"2027-01-01"}]),
            true,
            json!("model/a?b"),
        );
        let second = page(
            json!([{"id":"model-b","display_name":"Synthetic B"}]),
            false,
            Value::Null,
        );
        let (base, worker) = mock(vec![(200, first, Duration::ZERO), (200, second, Duration::ZERO)]);
        let models = Client::new()
            .unwrap()
            .models(&format!("{base}/v1/"), &key(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "model/a?b");
        assert_eq!(models[0].display_name, "Synthetic A");
        assert_eq!(models[0].max_input_tokens, Some(200000));
        assert_eq!(models[0].max_output_tokens, Some(64000));
        assert_eq!(
            models[0].capabilities,
            ["thinking".to_owned(), "tools".to_owned()].into()
        );
        assert_eq!(models[0].retires_at.as_deref(), Some("2027-01-01"));
        assert_eq!(models[1].max_input_tokens, None);
        assert_eq!(models[1].max_output_tokens, None);
        assert!(models[1].capabilities.is_empty());
        assert_eq!(models[1].retires_at, None);
        let requests = worker.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/models?limit=100 HTTP/1.1"));
        assert!(requests[1].contains("after_id=model%2Fa%3Fb"));
        for request in requests {
            assert!(request.contains(&format!("x-api-key: {SECRET}\r\n")));
            assert!(request.contains("anthropic-version: 2023-06-01\r\n"));
        }
    }

    #[tokio::test]
    async fn failures_are_bounded_and_secret_free() {
        for (status, body, delay, expected) in [
            (200, format!("{{{SECRET}"), Duration::ZERO, Error::Protocol),
            (401, SECRET.to_owned(), Duration::ZERO, Error::Http(401)),
            (200, "{}".into(), Duration::from_millis(150), Error::Transport),
            (
                200,
                page(json!([]), true, Value::Null),
                Duration::ZERO,
                Error::Protocol,
            ),
        ] {
            let (base, worker) = mock(vec![(status, body, delay)]);
            let key = key();
            let error = Client::new()
                .unwrap()
                .models(&base, &key, Duration::from_millis(75))
                .await
                .unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{key:?} {error:?} {error}").contains(SECRET));
            worker.join().unwrap();
        }
    }

    #[tokio::test]
    async fn caps_pages_without_returning_a_partial_catalog() {
        let body = page(json!([]), true, json!("cursor"));
        let (base, worker) = mock(
            (0..MAX_PAGES)
                .map(|_| (200, body.clone(), Duration::ZERO))
                .collect(),
        );
        assert_eq!(
            Client::new()
                .unwrap()
                .models(&base, &key(), Duration::from_secs(5))
                .await
                .unwrap_err(),
            Error::Protocol
        );
        assert_eq!(worker.join().unwrap().len(), MAX_PAGES);
    }

    #[test]
    fn classifies_refusals_without_retaining_messages_or_headers() {
        let body = |kind: &str, message: &str| json!({"error":{"type":kind,"message":message}}).to_string();
        let credit = body(
            "invalid_request_error",
            &format!("Your credit balance is too low to access the Anthropic API. {SECRET}"),
        );
        assert_eq!(
            classify_refusal(400, credit.as_bytes(), None),
            Some(Refusal::OutOfCredit)
        );
        assert!(!Refusal::OutOfCredit.is_retryable());
        let rate = body("rate_limit_error", SECRET);
        assert_eq!(
            classify_refusal(429, rate.as_bytes(), Some("12")),
            Some(Refusal::RateLimited {
                retry_after: Some(Duration::from_secs(12))
            })
        );
        assert_eq!(
            classify_refusal(429, rate.as_bytes(), Some(SECRET)),
            Some(Refusal::RateLimited { retry_after: None })
        );
        assert!(Refusal::RateLimited { retry_after: None }.is_retryable());
        assert_eq!(
            classify_refusal(529, body("overloaded_error", SECRET).as_bytes(), None),
            Some(Refusal::Overloaded)
        );
        assert!(Refusal::Overloaded.is_retryable());
        for (status, body) in [
            (400, body("invalid_request_error", "Invalid model")),
            (401, credit),
            (429, body("overloaded_error", SECRET)),
            (529, "not JSON".into()),
        ] {
            assert_eq!(classify_refusal(status, body.as_bytes(), None), None);
        }
        let refusal = classify_refusal(429, rate.as_bytes(), Some(SECRET)).unwrap();
        assert!(!format!("{refusal:?}").contains(SECRET));
    }
}
