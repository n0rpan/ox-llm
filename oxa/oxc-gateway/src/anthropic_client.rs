//! Anthropic-only streaming transport for the in-process walking skeleton.
//! The caller owns discovery, admission, request-time key reads and settlement.
use crate::{
    anthropic_request,
    anthropic_stream::{self, Completed, Field, Redactor, Usage},
    redaction,
    secrets::Key,
    sse::Framer,
};
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderValue, ACCEPT, CONTENT_TYPE},
    Url,
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, time::Duration};

pub mod models;

const MAX_BODY: usize = 64 * 1024 * 1024;

/// Shared cancellation across oxa, admission and provider I/O. Async waits wake
/// directly when cancelled; there is no relay token or polling timer.
pub type Cancellation = oxa_proto::CancelToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continue,
    Stop,
}

pub enum Event {
    /// A validated upstream attempt is about to start. Mark the budget hold as
    /// forwarded here; this is conservative, not proof that bytes arrived.
    Sending,
    /// Normalized, credential-redacted OpenAI stream data. No Done is emitted:
    /// the gateway must settle/write its ledger row before signaling success.
    Chunk(Value),
}

pub struct Chat<'a> {
    /// Host root or API base ending in `/v1`.
    pub base_url: &'a str,
    pub upstream_model: &'a str,
    pub alias: &'a str,
    pub max_output: Option<u64>,
    pub payload: &'a Value,
    pub created: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Setup,
    Endpoint,
    Request(&'static str),
    Http(u16),
    Transport,
    Protocol,
    TooLarge,
    Cancelled,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup => f.write_str("Anthropic transport is unavailable"),
            Self::Endpoint => f.write_str("Invalid Anthropic endpoint"),
            Self::Request(param) => write!(f, "Invalid Anthropic request field: {param}"),
            Self::Http(status) => write!(f, "Anthropic upstream answered HTTP {status}"),
            Self::Transport => f.write_str("Anthropic connection failed"),
            Self::Protocol => f.write_str("Invalid or incomplete Anthropic stream"),
            Self::TooLarge => f.write_str("Anthropic body exceeds the size limit"),
            Self::Cancelled => f.write_str("Anthropic request cancelled"),
        }
    }
}
impl std::error::Error for Error {}

/// Reusable connection pool. Credentials are supplied separately for each call.
#[derive(Clone)]
pub struct Client(reqwest::Client);
impl Client {
    pub fn new() -> Result<Self, Error> {
        Self::with_read_timeout(Some(Duration::from_secs(1800)))
    }
    /// A silence limit, reset by incoming bytes; None disables it. There is no
    /// total-generation deadline, so provider liveness can keep a long call alive.
    pub fn with_read_timeout(timeout: Option<Duration>) -> Result<Self, Error> {
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            // Bound connection setup while allowing long thinking pauses on a
            // live stream. There is no total-generation deadline.
            .connect_timeout(Duration::from_secs(30));
        if let Some(timeout) = timeout {
            if std::time::Instant::now().checked_add(timeout).is_none() {
                return Err(Error::Setup);
            }
            builder = builder.read_timeout(timeout);
        }
        builder.build().map(Self).map_err(|_| Error::Setup)
    }

    pub async fn stream<F>(
        &self,
        chat: Chat<'_>,
        key: &Key,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        let mut terminal_usage = None;
        self.stream_with_blocks(chat, key, cancel, on, |_| None, &mut terminal_usage)
            .await
    }

    pub(crate) async fn stream_with_blocks<F>(
        &self,
        chat: Chat<'_>,
        key: &Key,
        cancel: &Cancellation,
        on: &mut F,
        restore: impl FnMut(usize) -> Option<Vec<Value>>,
        terminal_usage: &mut Option<Usage>,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let endpoint = messages_endpoint(chat.base_url)?;
        let mut body = anthropic_request::translate_with_blocks(
            chat.payload,
            chat.upstream_model,
            chat.max_output,
            restore,
        )
        .map_err(|e| Error::Request(e.param))?;
        body["stream"] = Value::Bool(true);
        let body = serde_json::to_vec(&body).map_err(|_| Error::Protocol)?;
        if body.len() > MAX_BODY {
            return Err(Error::TooLarge);
        }
        let mut credential = HeaderValue::from_str(key.expose()).map_err(|_| Error::Setup)?;
        credential.set_sensitive(true);
        let request = self
            .0
            .post(endpoint)
            .header("x-api-key", credential)
            .header("anthropic-version", "2023-06-01")
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body)
            .build()
            .map_err(|_| Error::Endpoint)?;
        if cancel.is_cancelled() || on(Event::Sending) == Control::Stop {
            return Err(Error::Cancelled);
        }
        let cancelled = cancel.cancelled();
        tokio::pin!(cancelled);
        let response = tokio::select! {
            biased;
            _ = &mut cancelled => return Err(Error::Cancelled),
            response = self.0.execute(request) => response.map_err(|_| Error::Transport)?,
        };
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        if !response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
        {
            return Err(Error::Protocol);
        }
        let include_usage = chat.payload.pointer("/stream_options/include_usage") == Some(&Value::Bool(true));
        let redactor = KeyRedactor {
            key,
            fields: BTreeMap::new(),
        };
        let mut translated =
            anthropic_stream::Stream::with_redactor(chat.alias, chat.created, include_usage, redactor);
        let mut source = response.bytes_stream();
        let mut decoder = Framer::default();
        let mut received = 0_usize;
        loop {
            let next = tokio::select! {
                biased;
                _ = &mut cancelled => return Err(Error::Cancelled),
                next = source.next() => next,
            };
            let Some(bytes) = next else {
                return Err(Error::Protocol);
            };
            let bytes = bytes.map_err(|_| Error::Transport)?;
            for byte in bytes {
                received += 1;
                if received > MAX_BODY {
                    return Err(Error::TooLarge);
                }
                let Some(data) = decoder.push(byte) else {
                    continue;
                };
                let event: Value = serde_json::from_slice(&data).map_err(|_| Error::Protocol)?;
                let chunks = translated.event(&event).map_err(|_| Error::Protocol)?;
                if event["type"] == "message_stop" {
                    let completed = translated.finish().map_err(|_| Error::Protocol)?;
                    // Preserve trusted private metering before a callback can
                    // stop delivery of the finish/public usage chunks.
                    *terminal_usage = Some(completed.usage.ok_or(Error::Protocol)?);
                    deliver(chunks, cancel, on)?;
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    return Ok(completed);
                }
                deliver(chunks, cancel, on)?;
            }
        }
    }
}

fn messages_endpoint(base_url: &str) -> Result<Url, Error> {
    let mut endpoint = Url::parse(base_url).map_err(|_| Error::Endpoint)?;
    let loopback = match endpoint.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(Error::Endpoint);
    }
    let path = endpoint.path().trim_end_matches('/');
    let path = if path.ends_with("/v1") {
        format!("{path}/messages")
    } else {
        format!("{path}/v1/messages")
    };
    endpoint.set_path(&path);
    Ok(endpoint)
}

fn deliver<F>(chunks: Vec<Value>, cancel: &Cancellation, on: &mut F) -> Result<(), Error>
where
    F: FnMut(Event) -> Control + ?Sized,
{
    for chunk in chunks {
        if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
            return Err(Error::Cancelled);
        }
    }
    Ok(())
}

struct KeyRedactor<'a> {
    key: &'a Key,
    fields: BTreeMap<Field, redaction::Stream>,
}
impl Redactor for KeyRedactor<'_> {
    fn feed(&mut self, field: Field, fragment: &str) -> String {
        let bytes = self
            .fields
            .entry(field)
            .or_insert_with(|| redaction::Stream::new(self.key))
            .feed(fragment.as_bytes());
        String::from_utf8(bytes).expect("decoded fields retain UTF-8")
    }
    fn finish(&mut self, field: Field) -> String {
        let bytes = self
            .fields
            .remove(&field)
            .map(|mut state| state.finish())
            .unwrap_or_default();
        String::from_utf8(bytes).expect("decoded fields retain UTF-8")
    }
    fn whole(&mut self, value: &str) -> String {
        value.replace(self.key.expose(), "[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_require_https_except_for_owned_loopback_fixtures() {
        for base in [
            "http://example.invalid",
            "http://192.0.2.1/v1",
            "http://[2001:db8::1]/v1",
            "https://user:synthetic@example.invalid",
            "https://example.invalid?key=synthetic",
            "https://example.invalid#synthetic",
            "file:///synthetic",
        ] {
            assert_eq!(messages_endpoint(base), Err(Error::Endpoint));
        }
        for base in [
            "https://example.invalid",
            "https://example.invalid/v1/",
            "http://127.0.0.1/v1",
            "http://localhost/v1",
            "http://[::1]/v1",
            "http://[::ffff:127.0.0.1]/v1",
        ] {
            assert_eq!(messages_endpoint(base).unwrap().path(), "/v1/messages");
        }
    }
    #[test]
    fn sse_handles_unicode_comments_multiline_data_and_every_byte_boundary() {
        let wire = b": ping\r\nevent: content_block_delta\r\ndata: {\"type\":\r\ndata: \"ping\",\"text\":\"\xce\xbb\"}\r\n\r\n";
        let mut decoder = Framer::default();
        let events: Vec<Value> = wire
            .iter()
            .filter_map(|b| decoder.push(*b))
            .map(|data| serde_json::from_slice(&data).unwrap())
            .collect();
        assert_eq!(events, vec![serde_json::json!({"type":"ping","text":"λ"})]);
    }
    #[tokio::test]
    async fn cancellation_is_sticky_and_wakes_subscribers() {
        let cancel = Cancellation::default();
        let other = cancel.clone();
        let waiter = tokio::spawn(async move { other.cancelled().await });
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        cancel.cancelled().await;
        assert!(cancel.is_cancelled());
    }
}
