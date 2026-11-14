//! E8: the streamable HTTP transport. Owner: task E8. Signatures below are frozen; bodies are yours.
// ported from pi packages/mcp/src/transports/streamable-http.ts
use crate::{HttpConfig, TokenSource, Transport, CODE_NEEDS_AUTH};
use oxa_proto::{codes, OxaError, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Streamable HTTP: every message is a POST with `Accept: application/json, text/event-stream`;
/// the response is one JSON message or an SSE stream of messages. The legacy SSE transport is
/// not supported.
pub struct HttpTransport {
    pub(crate) url: String,
    /// Expanded static headers.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) tokens: Option<Arc<dyn TokenSource>>,
    /// `Mcp-Session-Id` once the server assigned one.
    pub(crate) session_id: Option<String>,
    pub(crate) protocol_version: Option<String>,
    pub(crate) queue: VecDeque<Value>,
    pub(crate) closed: bool,
    /// Private: one agent per connection, so the connection pool is shared by all requests.
    agent: ureq::Agent,
}

/// Longest body excerpt kept in an error message.
const EXCERPT_CHARS: usize = 200;

/// The parts of an HTTP reply the transport needs, read in full.
struct Reply {
    status: u16,
    session: Option<String>,
    /// `WWW-Authenticate` of a 401 / 403 (the step-up scope).
    challenge: Option<String>,
    content_type: String,
    body: String,
}

impl HttpTransport {
    /// Prepare a connection (no network yet). `expanded_headers` are the configured headers
    /// after `crate::expand_value`. `tokens` is used only when no `Authorization` header is
    /// configured.
    pub fn new(
        cfg: &HttpConfig,
        expanded_headers: Vec<(String, String)>,
        tokens: Option<Arc<dyn TokenSource>>,
        timeout: Duration,
    ) -> HttpTransport {
        let mut builder = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout));
        // SET-050: `httpProxy` applies to MCP HTTP (the engine connection is direct).
        if let Some(proxy) = cfg.proxy.as_deref().and_then(|p| ureq::Proxy::new(p).ok()) {
            builder = builder.proxy(Some(proxy));
        }
        let config = builder.build();
        HttpTransport {
            url: cfg.url.clone(),
            headers: expanded_headers,
            tokens,
            session_id: None,
            protocol_version: None,
            queue: VecDeque::new(),
            closed: false,
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// The token source in use: none when a static `Authorization` header is configured.
    fn token_source(&self) -> Option<Arc<dyn TokenSource>> {
        if self.has_header("authorization") {
            None
        } else {
            self.tokens.clone()
        }
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// Static headers, `Mcp-Session-Id`, `MCP-Protocol-Version` and the bearer token, applied
    /// to any request. The transport owns the two MCP headers: configured ones are ignored.
    fn decorate<M>(&self, mut req: ureq::RequestBuilder<M>, token: Option<&str>) -> ureq::RequestBuilder<M> {
        for (name, value) in &self.headers {
            let owned = ["mcp-session-id", "mcp-protocol-version"]
                .iter()
                .any(|n| name.eq_ignore_ascii_case(n));
            if !owned {
                req = req.header(name.as_str(), value.as_str());
            }
        }
        if let Some(id) = &self.session_id {
            req = req.header("Mcp-Session-Id", id.as_str());
        }
        if let Some(version) = &self.protocol_version {
            req = req.header("MCP-Protocol-Version", version.as_str());
        }
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    }

    /// One POST of `body`, read in full.
    fn post(&self, body: &[u8], token: Option<&str>) -> Result<Reply, OxaError> {
        crate::offline::check(&self.url)?;
        let mut req = self.agent.post(self.url.as_str());
        if !self.has_header("accept") {
            req = req.header("Accept", "application/json, text/event-stream");
        }
        if !self.has_header("content-type") {
            req = req.header("Content-Type", "application/json");
        }
        let req = self.decorate(req, token);
        let mut resp = req.send(body).map_err(|e| self.transport_error(e))?;
        let status = resp.status().as_u16();
        let session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| self.transport_error(e))?;
        Ok(Reply {
            status,
            session,
            challenge,
            content_type,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }

    fn transport_error(&self, e: ureq::Error) -> OxaError {
        match e {
            ureq::Error::Timeout(_) => OxaError::new(
                codes::TIMEOUT,
                format!("MCP server {} did not answer in time", self.url),
            ),
            other => OxaError::new(
                codes::MCP,
                format!("cannot reach MCP server {}: {other}", self.url),
            ),
        }
    }

    /// Queue the messages of a successful reply.
    fn enqueue(&mut self, content_type: &str, body: &str) -> Result<(), OxaError> {
        let text = body.trim();
        if text.is_empty() {
            return Ok(());
        }
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let messages = match mime.as_str() {
            "text/event-stream" => parse_sse_messages(text)?,
            "" | "application/json" => parse_json_messages(text)?,
            other => {
                return Err(OxaError::new(
                    codes::MCP,
                    format!("MCP server answered with unexpected content type \"{other}\""),
                ))
            }
        };
        for message in messages {
            // The protocol version the server agreed to, from the initialize result.
            if let Some(version) = message
                .get("result")
                .and_then(|r| r.get("protocolVersion"))
                .and_then(Value::as_str)
            {
                self.protocol_version = Some(version.to_string());
            }
            self.queue.push_back(message);
        }
        Ok(())
    }
}

/// Parse an SSE response body into the JSON messages of its `data:` fields (multi-line data
/// joined with `\n`; comments and other fields ignored; a non-JSON event is `Err(mcp)`).
pub fn parse_sse_messages(body: &str) -> Result<Vec<Value>, OxaError> {
    fn flush(data: &mut Option<String>, out: &mut Vec<Value>) -> Result<(), OxaError> {
        if let Some(text) = data.take() {
            if !text.trim().is_empty() {
                let message = serde_json::from_str::<Value>(&text).map_err(|e| {
                    OxaError::new(codes::MCP, format!("invalid SSE message from MCP server: {e}"))
                })?;
                out.push(message);
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    // Data lines of the event being read; `None` until a `data` field appears.
    let mut data: Option<String> = None;
    for raw in body.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            flush(&mut data, &mut out)?;
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let rest = &line[i + 1..];
                (&line[..i], rest.strip_prefix(' ').unwrap_or(rest))
            }
            None => (line, ""),
        };
        if field == "data" {
            match &mut data {
                Some(buf) => {
                    buf.push('\n');
                    buf.push_str(value);
                }
                None => data = Some(value.to_string()),
            }
        }
    }
    // A last event without its blank line is still delivered.
    flush(&mut data, &mut out)?;
    Ok(out)
}

/// One JSON value, or a JSON array of messages.
fn parse_json_messages(text: &str) -> Result<Vec<Value>, OxaError> {
    let value = serde_json::from_str::<Value>(text)
        .map_err(|e| OxaError::new(codes::MCP, format!("invalid JSON from MCP server: {e}")))?;
    Ok(match value {
        Value::Array(items) => items,
        single => vec![single],
    })
}

/// The first characters of a body, for an error message.
fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() > EXCERPT_CHARS {
        let head: String = trimmed.chars().take(EXCERPT_CHARS).collect();
        format!("{head}...")
    } else {
        trimmed.to_string()
    }
}

impl Transport for HttpTransport {
    /// POST the message. 200 with JSON or SSE: queue the messages. 202: nothing to queue.
    /// 401: ask `tokens.refresh()` once and retry; still 401 (or no token source) ->
    /// `Err(code = CODE_NEEDS_AUTH)`. 404 with a session id: the session expired -> `Err(mcp)`.
    /// Other non-2xx -> `Err(mcp)` with status and body excerpt. Sends `Mcp-Session-Id` and
    /// `MCP-Protocol-Version` once known.
    fn send(&mut self, msg: &Value) -> Result<(), OxaError> {
        if self.closed {
            return Err(OxaError::new(codes::MCP, "transport closed"));
        }
        let body = serde_json::to_vec(msg)
            .map_err(|e| OxaError::new(codes::INTERNAL, format!("cannot encode MCP message: {e}")))?;
        let had_session = self.session_id.is_some();
        let source = self.token_source();
        let mut token = match &source {
            Some(src) => src.token()?,
            None => None,
        };

        let mut reply = self.post(&body, token.as_deref())?;
        // Step-up (MCP authorization, RFC 6750 `insufficient_scope`): a 403 that asks for more
        // scope means a new sign-in with that scope; the scope is kept with the credentials.
        if reply.status == 403 {
            if let Some(challenge) = reply
                .challenge
                .as_deref()
                .filter(|c| c.contains("insufficient_scope"))
            {
                if let (Some(src), Some(scope)) = (&source, crate::oauth::challenge_scope(challenge)) {
                    src.step_up(&scope);
                    return Err(OxaError::new(
                        CODE_NEEDS_AUTH,
                        format!(
                            "MCP server {} needs a sign-in with more scope ({scope})",
                            self.url
                        ),
                    )
                    .with_hint("run `oxa mcp login <server>`"));
                }
            }
        }
        if reply.status == 401 {
            // A challenge may name the scope the server wants; the next sign-in asks for it.
            if let (Some(src), Some(scope)) = (
                &source,
                reply.challenge.as_deref().and_then(crate::oauth::challenge_scope),
            ) {
                src.remember_scope(&scope);
            }
            let refreshed = match &source {
                Some(src) => src.refresh()?,
                None => None,
            };
            match refreshed {
                Some(fresh) => {
                    token = Some(fresh);
                    reply = self.post(&body, token.as_deref())?;
                }
                None => {
                    return Err(OxaError::new(
                        CODE_NEEDS_AUTH,
                        format!("MCP server {} needs a sign-in", self.url),
                    )
                    .with_hint("run `oxa mcp login <server>`"))
                }
            }
            if reply.status == 401 {
                return Err(OxaError::new(
                    CODE_NEEDS_AUTH,
                    format!("MCP server {} rejected the access token", self.url),
                )
                .with_hint("run `oxa mcp login <server>`"));
            }
        }
        if let Some(id) = reply.session.take() {
            self.session_id = Some(id);
        }
        match reply.status {
            // 202 Accepted: a notification or response was taken; nothing comes back.
            202 => Ok(()),
            200..=299 => self.enqueue(&reply.content_type, &reply.body),
            404 if had_session => Err(OxaError::new(
                codes::MCP,
                format!("MCP session at {} expired (HTTP 404)", self.url),
            )
            .with_hint("restart the server with `/reload`")),
            status => Err(OxaError::new(
                codes::MCP,
                format!(
                    "MCP server {} answered HTTP {status}: {}",
                    self.url,
                    excerpt(&reply.body)
                ),
            )),
        }
    }
    /// Pop the next queued message; an empty queue is `Ok(None)` at once (responses arrive
    /// with their POST; no standing GET stream is opened).
    fn recv(&mut self, _timeout: Duration) -> Result<Option<Value>, OxaError> {
        if self.closed {
            return Err(OxaError::new(codes::MCP, "transport closed"));
        }
        Ok(self.queue.pop_front())
    }
    /// `DELETE` the session when the server gave a session id; the result is ignored.
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if self.session_id.is_some() && crate::offline::check(&self.url).is_ok() {
            let source = self.token_source();
            let token = source.and_then(|src| src.token().ok().flatten());
            let req = self.decorate(self.agent.delete(self.url.as_str()), token.as_deref());
            let _ = req.call();
        }
    }
    fn kind(&self) -> &'static str {
        "http"
    }
}
