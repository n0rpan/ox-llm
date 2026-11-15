//! Anthropic-only streaming transport for the in-process walking skeleton.
//! The caller owns discovery, admission, request-time key reads and settlement.
use crate::{
    anthropic_request,
    anthropic_stream::{self, Completed, Field, Redactor},
    redaction,
    secrets::Key,
};
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderValue, ACCEPT, CONTENT_TYPE},
    Url,
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, time::Duration};
use tokio::sync::watch;

const MAX_BODY: usize = 64 * 1024 * 1024;

/// One logical request's cancellation. Cancellation is sticky and wakes reads.
#[derive(Clone)]
pub struct Cancellation(watch::Sender<bool>);
impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}
impl Cancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

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
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            // Match the current native Messages client's per-operation timeout;
            // an active stream has no total-generation deadline.
            .connect_timeout(Duration::from_secs(1800))
            .read_timeout(Duration::from_secs(1800))
            .build()
            .map(Self)
            .map_err(|_| Error::Setup)
    }

    pub async fn stream(
        &self,
        chat: Chat<'_>,
        key: &Key,
        cancel: &Cancellation,
        on: &mut dyn FnMut(Event) -> Control,
    ) -> Result<Completed, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut endpoint = Url::parse(chat.base_url).map_err(|_| Error::Endpoint)?;
        if !["http", "https"].contains(&endpoint.scheme())
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
        let mut body = anthropic_request::translate(chat.payload, chat.upstream_model, chat.max_output)
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
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
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
        let mut decoder = Decoder::default();
        let mut received = 0_usize;
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
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
                let Some(event) = decoder.push(byte)? else {
                    continue;
                };
                let chunks = translated.event(&event).map_err(|_| Error::Protocol)?;
                for chunk in chunks {
                    if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                        return Err(Error::Cancelled);
                    }
                }
                if event["type"] == "message_stop" {
                    let completed = translated.finish().map_err(|_| Error::Protocol)?;
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    if completed.usage.is_none() {
                        return Err(Error::Protocol);
                    }
                    return Ok(completed);
                }
            }
        }
    }
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

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
}
impl Decoder {
    fn push(&mut self, byte: u8) -> Result<Option<Value>, Error> {
        if byte == b'\n' && self.after_cr {
            self.after_cr = false;
            return Ok(None);
        }
        self.after_cr = byte == b'\r';
        if !matches!(byte, b'\r' | b'\n') {
            self.line.push(byte);
            return Ok(None);
        }
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.data.is_empty() {
                return Ok(None);
            }
            let data = std::mem::take(&mut self.data);
            return serde_json::from_slice(&data[..data.len() - 1])
                .map(Some)
                .map_err(|_| Error::Protocol);
        }
        let data = if line == b"data" {
            Some(&b""[..])
        } else {
            line.strip_prefix(b"data:")
        };
        if let Some(data) = data {
            self.data
                .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            self.data.push(b'\n');
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sse_handles_unicode_comments_multiline_data_and_every_byte_boundary() {
        let wire = b": ping\r\nevent: content_block_delta\r\ndata: {\"type\":\r\ndata: \"ping\",\"text\":\"\xce\xbb\"}\r\n\r\n";
        let mut decoder = Decoder::default();
        let events: Vec<_> = wire.iter().filter_map(|b| decoder.push(*b).unwrap()).collect();
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
