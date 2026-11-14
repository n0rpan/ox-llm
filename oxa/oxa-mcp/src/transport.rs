//! Architect-owned (W0): the transport and token interfaces shared by stdio, HTTP and OAuth.
use oxa_proto::{OxaError, Value};
use std::time::Duration;

/// A bidirectional JSON-RPC message channel to one server. Implemented by
/// [`crate::StdioTransport`] (E7) and [`crate::HttpTransport`] (E8); tests use
/// [`crate::PipeTransport`].
///
/// One message = one JSON value (a request, a response or a notification); batching is not
/// used. Errors use `code = "mcp"`, except [`crate::CODE_NEEDS_AUTH`] when an HTTP server
/// answers 401 and no valid token is available, and `timeout`.
pub trait Transport: Send {
    /// Send one message. For HTTP this performs the POST; the response body (a JSON message or
    /// an SSE stream of messages) is queued for [`Transport::recv`].
    fn send(&mut self, msg: &Value) -> Result<(), OxaError>;
    /// The next incoming message, waiting at most `timeout`. `Ok(None)` = nothing arrived in
    /// time. `Err` = the connection is closed or broken (every later call fails the same way).
    fn recv(&mut self, timeout: Duration) -> Result<Option<Value>, OxaError>;
    /// Close the connection. stdio: close stdin, wait 2 s, SIGTERM the process group, wait
    /// 2 s, SIGKILL the group. HTTP: `DELETE` the session when the server gave a session id.
    /// Idempotent.
    fn close(&mut self);
    /// Last lines of the server's stderr (stdio only).
    fn stderr_tail(&self) -> String {
        String::new()
    }
    /// `"stdio"` or `"http"`.
    fn kind(&self) -> &'static str;
}

/// Supplies the bearer token of an HTTP server. Implemented by the OAuth module (F5).
pub trait TokenSource: Send + Sync {
    /// The current access token, if signed in (expired tokens are refreshed first when a
    /// refresh token exists).
    fn token(&self) -> Result<Option<String>, OxaError>;
    /// Called after a 401 with the current token: refresh and return the new token, or
    /// `Ok(None)` when a new sign-in is needed.
    fn refresh(&self) -> Result<Option<String>, OxaError>;
    /// H: a 403 `insufficient_scope` named `scope`: forget the access token and remember the
    /// scope, so the next sign-in asks for it (step-up). Default: nothing to remember.
    fn step_up(&self, _scope: &str) {}
    /// H: a 401 challenge named `scope`: remember it for the next sign-in (the token is kept;
    /// a refresh may still cure the 401). Default: nothing to remember.
    fn remember_scope(&self, _scope: &str) {}
}

/// An in-memory transport pair for tests: what one end sends, the other receives.
pub struct PipeTransport {
    pub(crate) tx: std::sync::mpsc::Sender<Value>,
    pub(crate) rx: std::sync::mpsc::Receiver<Value>,
    pub(crate) closed: bool,
}

impl PipeTransport {
    /// Two connected ends.
    pub fn pair() -> (PipeTransport, PipeTransport) {
        let (atx, brx) = std::sync::mpsc::channel();
        let (btx, arx) = std::sync::mpsc::channel();
        (
            PipeTransport {
                tx: atx,
                rx: arx,
                closed: false,
            },
            PipeTransport {
                tx: btx,
                rx: brx,
                closed: false,
            },
        )
    }
}

impl Transport for PipeTransport {
    fn send(&mut self, msg: &Value) -> Result<(), OxaError> {
        if self.closed {
            return Err(OxaError::new(oxa_proto::codes::MCP, "transport closed"));
        }
        self.tx
            .send(msg.clone())
            .map_err(|_| OxaError::new(oxa_proto::codes::MCP, "transport closed"))
    }
    fn recv(&mut self, timeout: Duration) -> Result<Option<Value>, OxaError> {
        if self.closed {
            return Err(OxaError::new(oxa_proto::codes::MCP, "transport closed"));
        }
        match self.rx.recv_timeout(timeout) {
            Ok(v) => Ok(Some(v)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(OxaError::new(oxa_proto::codes::MCP, "transport closed"))
            }
        }
    }
    fn close(&mut self) {
        self.closed = true;
    }
    fn kind(&self) -> &'static str {
        "pipe"
    }
}
