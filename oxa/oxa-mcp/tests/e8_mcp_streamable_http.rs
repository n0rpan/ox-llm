//! E8 acceptance tests: MCP streamable HTTP.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The tests talk to a tiny HTTP/1.1 server bound to 127.0.0.1 (std only, one connection per
//! request, `Connection: close`). Nothing leaves the machine.

use oxa_mcp::{
    expand_value, parse_server, parse_sse_messages, ConfigScope, HttpConfig, HttpTransport, TokenSource,
    Transport, CODE_NEEDS_AUTH,
};
use oxa_proto::{codes, OxaError, Value};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request as the test server saw it. Header names are lower-cased.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
}

/// What the test server answers.
#[derive(Clone, Debug)]
struct Canned {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

fn reply(status: u16, content_type: &str, body: &str) -> Canned {
    Canned {
        status,
        headers: vec![("Content-Type".to_string(), content_type.to_string())],
        body: body.to_string(),
    }
}

fn json_reply(status: u16, body: &Value) -> Canned {
    reply(status, "application/json", &body.to_string())
}

fn with_session(mut canned: Canned, id: &str) -> Canned {
    canned
        .headers
        .push(("Mcp-Session-Id".to_string(), id.to_string()));
    canned
}

fn read_request(stream: &TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut raw = String::new();
        reader.read_line(&mut raw).ok()?;
        let raw = raw.trim_end();
        if raw.is_empty() {
            break;
        }
        if let Some((name, value)) = raw.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn write_reply(stream: &mut TcpStream, canned: &Canned) {
    let mut out = format!(
        "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
        canned.status,
        canned.body.len()
    );
    for (name, value) in &canned.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&canned.body);
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

/// Start a server answering every request with `handler`. Returns its MCP URL and the log of
/// requests. The log entry is written before the reply, so it is complete once a call returns.
fn serve<F>(handler: F) -> (String, Arc<Mutex<Vec<Seen>>>)
where
    F: Fn(&Seen) -> Canned + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let log: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_log = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&stream) else {
                continue;
            };
            let canned = handler(&request);
            seen_log.lock().expect("log").push(request);
            write_reply(&mut stream, &canned);
        }
    });
    (format!("http://{addr}/mcp"), log)
}

fn transport(
    url: &str,
    headers: Vec<(String, String)>,
    tokens: Option<Arc<dyn TokenSource>>,
) -> HttpTransport {
    let cfg = HttpConfig {
        url: url.to_string(),
        headers: Vec::new(),
        oauth: None,
        proxy: None,
    };
    HttpTransport::new(&cfg, headers, tokens, Duration::from_secs(10))
}

const WAIT: Duration = Duration::from_millis(50);

/// A token source with a current token and what a refresh returns.
struct Tokens {
    current: Option<String>,
    fresh: Option<String>,
    refreshes: Mutex<u32>,
}

impl Tokens {
    fn new(current: Option<&str>, fresh: Option<&str>) -> Arc<Tokens> {
        Arc::new(Tokens {
            current: current.map(str::to_string),
            fresh: fresh.map(str::to_string),
            refreshes: Mutex::new(0),
        })
    }
}

impl TokenSource for Tokens {
    fn token(&self) -> Result<Option<String>, OxaError> {
        Ok(self.current.clone())
    }
    fn refresh(&self) -> Result<Option<String>, OxaError> {
        *self.refreshes.lock().expect("refreshes") += 1;
        Ok(self.fresh.clone())
    }
}

fn error_code(result: Result<(), OxaError>) -> (&'static str, String) {
    match result {
        Err(e) => (e.code, e.message),
        Ok(()) => panic!("expected an error"),
    }
}

/// plain JSON reply
#[test]
fn post_json_response() {
    let (url, log) = serve(|_| json_reply(200, &json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})));
    let mut t = transport(&url, Vec::new(), None);
    t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .expect("send");
    let got = t.recv(WAIT).expect("recv").expect("one message");
    assert_eq!(got, json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}));
    assert_eq!(t.recv(WAIT).expect("recv"), None, "queue drained");

    let seen = log.lock().expect("log");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/mcp");
    assert!(seen[0]
        .header("accept")
        .unwrap_or_default()
        .contains("text/event-stream"));
    assert_eq!(
        seen[0].header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(t.kind(), "http");
}

/// streamed reply
#[test]
fn post_sse_response() {
    let body = concat!(
        ": keep-alive comment\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\n",
        "data: \"id\":1,\"result\":{}}\n",
        "\n",
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n",
        "\n",
    );
    let (url, _log) = serve(move |_| reply(200, "text/event-stream", body));
    let mut t = transport(&url, Vec::new(), None);
    t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .expect("send");
    assert_eq!(
        t.recv(WAIT).expect("recv"),
        Some(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
    );
    assert_eq!(
        t.recv(WAIT).expect("recv"),
        Some(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}))
    );
    assert_eq!(t.recv(WAIT).expect("recv"), None);
}

/// legacy `sse` transport type -> clear error
#[test]
fn sse_type_rejected() {
    let entry = json!({"type": "sse", "url": "http://127.0.0.1:9/sse"});
    let err = parse_server("docs", &entry, ConfigScope::User).expect_err("sse must be rejected");
    assert!(err.contains("legacy SSE transport is not supported"), "{err}");
    assert!(err.contains("/mcp"), "the hint names the streamable URL: {err}");

    // The streamable types are accepted.
    let ok = json!({"type": "streamable-http", "url": "http://127.0.0.1:9/mcp"});
    assert!(parse_server("docs", &ok, ConfigScope::User).is_ok());
}

/// ${VAR} in headers
#[test]
fn headers_and_env_expansion() {
    let (url, log) = serve(|_| json_reply(202, &json!(null)));
    let env = |name: &str| -> Option<String> {
        match name {
            "TEAM" => Some("core".to_string()),
            "KEY" => Some("k-42".to_string()),
            _ => None,
        }
    };
    let run = |command: &str| -> Result<String, OxaError> { Ok(format!("{command}-out\n")) };
    let raw = [
        ("X-Team", "${TEAM}"),
        ("X-Key", "${KEY}-${MISSING}"),
        ("X-Cmd", "!echo hi"),
        ("X-Open", "a${UNCLOSED"),
    ];
    let expanded: Vec<(String, String)> = raw
        .iter()
        .map(|(name, value)| {
            let v = expand_value(value, &env, &run).expect("expand");
            (name.to_string(), v)
        })
        .collect();
    assert_eq!(expanded[0].1, "core");
    assert_eq!(expanded[1].1, "k-42-");
    assert_eq!(expanded[2].1, "echo hi-out");
    assert_eq!(expanded[3].1, "a${UNCLOSED");

    let mut t = transport(&url, expanded, None);
    t.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .expect("send");
    let seen = log.lock().expect("log");
    assert_eq!(seen[0].header("x-team").as_deref(), Some("core"));
    assert_eq!(seen[0].header("x-key").as_deref(), Some("k-42-"));
    assert_eq!(seen[0].header("x-cmd").as_deref(), Some("echo hi-out"));
}

/// Authorization header
#[test]
fn bearer_token_env_var() {
    // A configured Authorization header (expanded from an env var) is used as written; the token
    // source is not consulted.
    let (url, log) = serve(|req| {
        if req.header("authorization").as_deref() == Some("Bearer tok-env") {
            json_reply(202, &json!(null))
        } else {
            reply(401, "text/plain", "wrong token")
        }
    });
    let env = |name: &str| (name == "OXA_TEST_TOKEN").then(|| "tok-env".to_string());
    let auth = expand_value("Bearer ${OXA_TEST_TOKEN}", &env, &|_: &str| {
        Err(OxaError::new(codes::INVALID, "no commands here"))
    })
    .expect("expand");
    let tokens = Tokens::new(Some("tok-source"), None);
    let mut t = transport(
        &url,
        vec![("Authorization".to_string(), auth)],
        Some(tokens.clone()),
    );
    t.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .expect("static header is accepted");
    assert_eq!(
        log.lock().expect("log")[0].header("authorization").as_deref(),
        Some("Bearer tok-env")
    );
    assert_eq!(*tokens.refreshes.lock().expect("refreshes"), 0);

    // Without a configured header, the token source supplies the bearer token.
    let (url2, log2) = serve(|req| {
        if req.header("authorization").as_deref() == Some("Bearer tok-source") {
            json_reply(202, &json!(null))
        } else {
            reply(401, "text/plain", "no token")
        }
    });
    let mut t2 = transport(&url2, Vec::new(), Some(Tokens::new(Some("tok-source"), None)));
    t2.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .expect("token source is used");
    assert_eq!(
        log2.lock().expect("log")[0].header("authorization").as_deref(),
        Some("Bearer tok-source")
    );
}

/// Mcp-Session-Id
#[test]
fn session_id_header_kept() {
    let (url, log) = serve(|req| {
        if req.method == "DELETE" {
            return reply(200, "text/plain", "");
        }
        if req.body.contains("\"initialize\"") {
            return with_session(
                json_reply(
                    200,
                    &json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2025-11-25"}}),
                ),
                "sess-7",
            );
        }
        if req.body.contains("notifications/initialized") {
            return with_session(reply(202, "text/plain", ""), "sess-7");
        }
        json_reply(200, &json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": []}}))
    });
    let mut t = transport(&url, Vec::new(), None);
    t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
        .expect("initialize");
    assert_eq!(
        t.recv(WAIT).expect("recv").expect("initialize result")["result"]["protocolVersion"],
        "2025-11-25"
    );
    t.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .expect("initialized");
    assert_eq!(t.recv(WAIT).expect("recv"), None, "202 queues nothing");
    t.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .expect("tools/list");
    assert!(t.recv(WAIT).expect("recv").is_some());
    t.close();
    t.close(); // idempotent

    let seen = log.lock().expect("log").clone();
    assert_eq!(seen.len(), 4, "initialize, initialized, tools/list, DELETE");
    assert_eq!(
        seen[0].header("mcp-session-id"),
        None,
        "no session before the server gave one"
    );
    assert_eq!(
        seen[0].header("mcp-protocol-version"),
        None,
        "no version before initialize"
    );
    assert_eq!(seen[1].header("mcp-session-id").as_deref(), Some("sess-7"));
    assert_eq!(seen[2].header("mcp-session-id").as_deref(), Some("sess-7"));
    assert_eq!(
        seen[2].header("mcp-protocol-version").as_deref(),
        Some("2025-11-25")
    );
    assert_eq!(seen[3].method, "DELETE", "close ends the session");
    assert_eq!(seen[3].header("mcp-session-id").as_deref(), Some("sess-7"));
}

/// status mapping, 401 distinct
#[test]
fn http_error_is_mcp_error() {
    // 401 with no token source: a sign-in is needed, a distinct code.
    let (url, _) = serve(|_| reply(401, "text/plain", "unauthorized"));
    let mut t = transport(&url, Vec::new(), None);
    let (code, _) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})));
    assert_eq!(code, CODE_NEEDS_AUTH);
    assert_eq!(CODE_NEEDS_AUTH, codes::MCP_NEEDS_AUTH);
    assert_ne!(code, codes::MCP);

    // 500 is an ordinary MCP error that carries the status and a body excerpt.
    let (url, _) = serve(|_| reply(500, "text/plain", "database exploded"));
    let mut t = transport(&url, Vec::new(), None);
    let (code, message) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})));
    assert_eq!(code, codes::MCP);
    assert!(
        message.contains("500") && message.contains("database exploded"),
        "{message}"
    );

    // 403 is not an auth prompt.
    let (url, _) = serve(|_| reply(403, "text/plain", "forbidden"));
    let mut t = transport(&url, Vec::new(), None);
    let (code, _) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})));
    assert_eq!(code, codes::MCP);

    // 404 after a session was established: the session expired.
    let (url, _) = serve(|req| {
        if req.body.contains("initialize") {
            with_session(
                json_reply(200, &json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
                "old",
            )
        } else {
            reply(404, "text/plain", "no such session")
        }
    });
    let mut t = transport(&url, Vec::new(), None);
    t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .expect("initialize");
    let (code, message) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"})));
    assert_eq!(code, codes::MCP);
    assert!(message.contains("expired"), "{message}");

    // A 2xx reply with a body that is not JSON is an MCP error, not a panic.
    let (url, _) = serve(|_| reply(200, "application/json", "{not json"));
    let mut t = transport(&url, Vec::new(), None);
    let (code, _) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})));
    assert_eq!(code, codes::MCP);
}

/// A 401 with a token source: refresh once, retry with the new token; a second 401 or a refresh
/// that yields nothing is the sign-in error.
#[test]
fn token_refresh_on_401() {
    let (url, log) = serve(|req| {
        if req.header("authorization").as_deref() == Some("Bearer new") {
            json_reply(202, &json!(null))
        } else {
            reply(401, "text/plain", "expired token")
        }
    });
    let tokens = Tokens::new(Some("old"), Some("new"));
    let mut t = transport(&url, Vec::new(), Some(tokens.clone()));
    t.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .expect("retry with the refreshed token succeeds");
    let seen = log.lock().expect("log");
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].header("authorization").as_deref(), Some("Bearer old"));
    assert_eq!(seen[1].header("authorization").as_deref(), Some("Bearer new"));
    assert_eq!(*tokens.refreshes.lock().expect("refreshes"), 1);

    // Refresh yields nothing: a sign-in is needed, no retry.
    let (url, log) = serve(|_| reply(401, "text/plain", "expired token"));
    let mut t = transport(&url, Vec::new(), Some(Tokens::new(Some("old"), None)));
    let (code, _) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})));
    assert_eq!(code, CODE_NEEDS_AUTH);
    assert_eq!(log.lock().expect("log").len(), 1, "no retry without a token");
}

/// The SSE body parser: CRLF, multi-line data, comments, other fields, a final event without a
/// blank line, and a non-JSON event.
#[test]
fn sse_parser_rules() {
    let body = ": c\r\nid: 9\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\ndata:\n\n: only comment\n\ndata: {\"b\":2}";
    let messages = parse_sse_messages(body).expect("parse");
    assert_eq!(messages, vec![json!({"a": 1}), json!({"b": 2})]);

    let (code, _) = match parse_sse_messages("data: not json\n\n") {
        Err(e) => (e.code, e.message),
        Ok(v) => panic!("expected an error, got {v:?}"),
    };
    assert_eq!(code, codes::MCP);
}

/// Connection refused is an MCP error that names the server; a closed transport refuses calls.
#[test]
fn unreachable_server_and_closed_transport() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let mut t = transport(&format!("http://127.0.0.1:{port}/mcp"), Vec::new(), None);
    let (code, message) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})));
    assert_eq!(code, codes::MCP);
    assert!(message.contains("127.0.0.1"), "{message}");

    t.close();
    let (code, _) = error_code(t.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"})));
    assert_eq!(code, codes::MCP);
    assert!(t.recv(WAIT).is_err());
}

/// H (SET-050): `httpProxy` sends the MCP request through the proxy (absolute-form target).
#[test]
fn http_proxy_setting_routes_through_the_proxy() {
    let (proxy_url, log) = serve(|_| json_reply(200, &json!({"jsonrpc": "2.0", "id": 1, "result": {}})));
    let proxy = proxy_url.trim_end_matches("/mcp").to_string();
    let cfg = HttpConfig {
        url: "http://mcp-h.invalid/mcp".to_string(),
        headers: Vec::new(),
        oauth: None,
        proxy: Some(proxy),
    };
    let mut t = HttpTransport::new(&cfg, Vec::new(), None, Duration::from_secs(10));
    // The stand-in proxy does not tunnel, so the send itself fails; what matters is that the
    // request went to the proxy (ureq opens a CONNECT tunnel to the target) and not to the
    // unresolvable host directly.
    let _ = t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
    let seen = log.lock().expect("log");
    assert_eq!(seen.len(), 1, "the proxy got the request");
    assert!(
        seen[0].path.contains("mcp-h.invalid"),
        "the proxy is asked for the target: {} {}",
        seen[0].method,
        seen[0].path
    );
}
