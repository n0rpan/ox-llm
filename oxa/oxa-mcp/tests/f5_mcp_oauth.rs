//! F5 acceptance tests: MCP OAuth.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The authorization server and the MCP endpoint are tiny HTTP/1.1 servers bound to 127.0.0.1
//! (std only, one connection per request, `Connection: close`). The "browser" is a thread that
//! GETs the redirect URI, as a browser would after the user approved. Nothing leaves the machine.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use oxa_fake::TestEnv;
use oxa_mcp::{
    authorization_url, discover, login, parse_redirect, pkce_pair, random_bytes, redirect_uri,
    run_value_command, AccessGate, AuthServerMeta, CallbackServer, ConfigScope, HttpConfig, HttpTransport,
    LoginIo, ManagerOpts, McpConfig, McpManager, OAuthSettings, OAuthTokens, ServerConfig, ServerKind,
    StoredAuth, TokenSource, TokenStore, Transport, AUTH_FILE, CODE_NEEDS_AUTH,
};
use oxa_proto::{codes, Access, CancelToken, Exposure, OxaError, Value};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------------------------
// A tiny HTTP server for the tests.
// ---------------------------------------------------------------------------------------------

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
    /// The body as an `application/x-www-form-urlencoded` map.
    fn form(&self) -> HashMap<String, String> {
        self.body
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (unpercent(k), unpercent(v)))
            .collect()
    }
}

#[derive(Clone, Debug)]
struct Answer {
    status: u16,
    body: String,
}

fn answer(status: u16, body: Value) -> Answer {
    Answer {
        status,
        body: body.to_string(),
    }
}

fn empty(status: u16) -> Answer {
    Answer {
        status,
        body: String::new(),
    }
}

fn serve_one(stream: TcpStream, handler: &dyn Fn(&Seen) -> Answer, seen: &Mutex<Vec<Seen>>) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let request = Seen {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    seen.lock().unwrap().push(request.clone());
    let reply = handler(&request);
    let head = format!(
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.body.len()
    );
    let _ = writer.write_all(head.as_bytes());
    let _ = writer.write_all(reply.body.as_bytes());
}

/// A server on 127.0.0.1. `build` receives the base URL (`http://127.0.0.1:<port>`).
struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    fn start<H, B>(build: B) -> Mock
    where
        B: FnOnce(String) -> H,
        H: Fn(&Seen) -> Answer + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base = format!("http://{}", listener.local_addr().expect("local address"));
        let handler: Arc<dyn Fn(&Seen) -> Answer + Send + Sync> = Arc::new(build(base.clone()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (accept_handler, accept_seen) = (handler.clone(), seen.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (h, s) = (accept_handler.clone(), accept_seen.clone());
                thread::spawn(move || serve_one(stream, &*h, &s));
            }
        });
        Mock { base, seen }
    }

    fn requests(&self, method: &str, path: &str) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .cloned()
            .collect()
    }
}

fn unpercent(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).expect("hex escape"));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("utf-8")
}

fn query_params(url: &str) -> HashMap<String, String> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (unpercent(k), unpercent(v)))
        .collect()
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Authorization server metadata of the mock issuer `base`.
fn metadata(base: &str, registration: bool) -> Value {
    let mut meta = json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["mcp"],
    });
    if registration {
        meta["registration_endpoint"] = json!(format!("{base}/register"));
    }
    meta
}

/// The token endpoint of the sign-in tests: the code must be the one the "browser" sent, and
/// its PKCE verifier must match the challenge of the authorization request.
fn token_reply(req: &Seen, challenge: &str) -> Answer {
    let form = req.form();
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let derived = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if form.get("grant_type").map(String::as_str) == Some("authorization_code")
        && form.get("code").map(String::as_str) == Some("the-code")
        && derived == challenge
    {
        answer(
            200,
            json!({"access_token": "at-1", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "rt-1", "scope": "mcp"}),
        )
    } else {
        answer(
            400,
            json!({"error": "invalid_grant", "error_description": "bad code or verifier"}),
        )
    }
}

/// What "the user approves in the browser" looks like: GET the redirect URI with code and state.
fn browse(auth_url: &str) {
    let params = query_params(auth_url);
    let url = format!(
        "{}?code=the-code&state={}",
        params["redirect_uri"], params["state"]
    );
    if let Ok(mut reply) = ureq::get(&url).call() {
        let _ = reply.body_mut().read_to_string();
    }
}

/// The user interface of the tests: optionally acts as the browser, optionally pastes the
/// redirect URL on the third poll, and records what the login showed.
struct TestIo {
    browser: bool,
    paste: bool,
    opened: Option<String>,
    challenge: Arc<Mutex<String>>,
    statuses: Vec<String>,
    polls: u32,
}

impl TestIo {
    fn new(browser: bool, paste: bool, challenge: Arc<Mutex<String>>) -> TestIo {
        TestIo {
            browser,
            paste,
            opened: None,
            challenge,
            statuses: Vec::new(),
            polls: 0,
        }
    }
}

impl LoginIo for TestIo {
    fn open_url(&mut self, url: &str) {
        *self.challenge.lock().unwrap() = query_params(url)
            .get("code_challenge")
            .cloned()
            .unwrap_or_default();
        self.opened = Some(url.to_string());
        if self.browser {
            let url = url.to_string();
            thread::spawn(move || browse(&url));
        }
    }
    fn pasted_redirect(&mut self) -> Option<String> {
        if !self.paste {
            return None;
        }
        self.polls += 1;
        if self.polls < 3 {
            return None;
        }
        let params = query_params(self.opened.as_deref()?);
        Some(format!(
            "{}?code=the-code&state={}",
            params["redirect_uri"], params["state"]
        ))
    }
    fn status(&mut self, text: &str) {
        self.statuses.push(text.to_string());
    }
}

/// The `!command` runner of a sign-in whose gate allows everything.
fn run_allowed(command: &str) -> Result<String, OxaError> {
    let dir = std::env::temp_dir();
    run_value_command(&|_: &Access| true, &dir, &dir, command)
}

fn http_config(url: &str, settings: OAuthSettings) -> HttpConfig {
    HttpConfig {
        url: url.to_string(),
        headers: Vec::new(),
        oauth: Some(settings),
        proxy: None,
    }
}

fn sample_auth(access: &str) -> StoredAuth {
    StoredAuth {
        access_token: Some(access.to_string()),
        refresh_token: Some("refresh".to_string()),
        expires_at: Some(now_s() + 3600),
        scope: Some("mcp".to_string()),
        client_id: Some("client".to_string()),
        client_secret: None,
        token_endpoint: Some("https://auth.example/token".to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Acceptance tests.
// ---------------------------------------------------------------------------------------------

/// verifier and challenge
#[test]
fn pkce() {
    // RFC 7636 appendix B: the verifier is the base64url of the 32 random bytes, the challenge
    // the base64url of its SHA-256.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let random: [u8; 32] = URL_SAFE_NO_PAD
        .decode(verifier)
        .expect("decodable verifier")
        .try_into()
        .expect("32 bytes");
    let (v, c) = pkce_pair(&random);
    assert_eq!(v, verifier);
    assert_eq!(c, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");

    // Fresh OS randomness: a 43-character verifier whose challenge is its SHA-256.
    let fresh = random_bytes().expect("random bytes");
    assert_ne!(fresh, [0u8; 32]);
    let (v2, c2) = pkce_pair(&fresh);
    assert_eq!(v2.len(), 43);
    assert_eq!(c2, URL_SAFE_NO_PAD.encode(Sha256::digest(v2.as_bytes())));
    assert_ne!(random_bytes().expect("random bytes"), fresh);
}

/// against a local test server
#[test]
fn dynamic_registration() {
    let env = TestEnv::new();
    let challenge = Arc::new(Mutex::new(String::new()));
    let handler_challenge = challenge.clone();
    let mock = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, true)),
            ("POST", "/register") => {
                let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
                let redirect = body["redirect_uris"][0].as_str().unwrap_or("");
                let ok = body["client_name"] == "oxa"
                    && body["application_type"] == "native"
                    && redirect.starts_with("http://127.0.0.1:")
                    && redirect.ends_with("/callback");
                if ok {
                    answer(201, json!({"client_id": "dcr-client-1"}))
                } else {
                    answer(400, json!({"error": "invalid_client_metadata"}))
                }
            }
            ("POST", "/token") => token_reply(req, &handler_challenge.lock().unwrap()),
            _ => empty(404),
        }
    });
    let store = TokenStore::open(&env.agent_dir().join(AUTH_FILE));
    let url = format!("{}/mcp", mock.base);
    let cfg = http_config(&url, OAuthSettings::default());
    let mut io = TestIo::new(true, false, challenge.clone());
    login(
        "docs",
        &cfg,
        &store,
        &mut io,
        Duration::from_secs(20),
        &CancelToken::new(),
        &run_allowed,
    )
    .expect("sign-in with dynamic registration");

    assert_eq!(mock.requests("POST", "/register").len(), 1, "one registration");
    let opened = io.opened.clone().expect("the browser was asked");
    let params = query_params(&opened);
    assert_eq!(params["client_id"], "dcr-client-1");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["resource"], url);

    let stored = store.get("docs", &url).expect("tokens stored");
    assert_eq!(stored.access_token.as_deref(), Some("at-1"));
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-1"));
    assert_eq!(stored.client_id.as_deref(), Some("dcr-client-1"));
    assert_eq!(
        stored.token_endpoint.as_deref(),
        Some(format!("{}/token", mock.base).as_str())
    );
    assert!(stored.expires_at.is_some_and(|t| t > now_s()));
    assert_eq!(stored.scope.as_deref(), Some("mcp"));
}

/// client id from mcp.json
#[test]
fn configured_client() {
    let env = TestEnv::new();
    let challenge = Arc::new(Mutex::new(String::new()));
    let handler_challenge = challenge.clone();
    let expected_auth = format!("Basic {}", STANDARD.encode("configured-id:s3cret"));
    let mock = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, true)),
            ("POST", "/token") => {
                if req.header("authorization").as_deref() != Some(expected_auth.as_str()) {
                    return answer(401, json!({"error": "invalid_client"}));
                }
                token_reply(req, &handler_challenge.lock().unwrap())
            }
            _ => empty(404),
        }
    });
    let settings = OAuthSettings {
        client_id: Some("configured-id".to_string()),
        // `!command` values are expanded like every other mcp.json value.
        client_secret: Some("!printf s3cret".to_string()),
        ..OAuthSettings::default()
    };
    let store = TokenStore::open(&env.agent_dir().join(AUTH_FILE));
    let url = format!("{}/mcp", mock.base);
    let cfg = http_config(&url, settings);
    let mut io = TestIo::new(true, false, challenge.clone());
    login(
        "cfg",
        &cfg,
        &store,
        &mut io,
        Duration::from_secs(20),
        &CancelToken::new(),
        &run_allowed,
    )
    .expect("sign-in with the configured client");

    assert!(
        mock.requests("POST", "/register").is_empty(),
        "no dynamic registration"
    );
    let params = query_params(&io.opened.clone().expect("opened"));
    assert_eq!(params["client_id"], "configured-id");
    let stored = store.get("cfg", &url).expect("tokens stored");
    assert_eq!(stored.client_id.as_deref(), Some("configured-id"));
    assert_eq!(stored.client_secret.as_deref(), Some("s3cret"));
    assert_eq!(stored.access_token.as_deref(), Some("at-1"));
}

/// A `!command` client secret runs through the caller's `run_value_command`: the gate is asked
/// for `Access::Exec`, and a denied command stops the sign-in before any code exchange.
#[test]
fn configured_client_secret_command_asks_the_gate() {
    let env = TestEnv::new();
    let challenge = Arc::new(Mutex::new(String::new()));
    let mock = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, true)),
            _ => empty(404),
        }
    });
    let settings = OAuthSettings {
        client_id: Some("configured-id".to_string()),
        client_secret: Some("!printf s3cret".to_string()),
        ..OAuthSettings::default()
    };
    let store = TokenStore::open(&env.agent_dir().join(AUTH_FILE));
    let url = format!("{}/mcp", mock.base);
    let cfg = http_config(&url, settings);
    let asked = Arc::new(Mutex::new(Vec::<Access>::new()));
    let seen = asked.clone();
    let gate = move |a: &Access| {
        seen.lock().unwrap().push(a.clone());
        false
    };
    let (home, work) = (env.home(), env.work());
    let run = |c: &str| run_value_command(&gate, &home, &work, c);
    let mut io = TestIo::new(true, false, challenge);
    let err = login(
        "cfg",
        &cfg,
        &store,
        &mut io,
        Duration::from_secs(20),
        &CancelToken::new(),
        &run,
    )
    .expect_err("a denied !command stops the sign-in");

    assert_eq!(err.code, codes::DENIED, "{err:?}");
    assert_eq!(
        *asked.lock().unwrap(),
        vec![Access::Exec {
            command: "printf s3cret".to_string(),
            net: true,
        }]
    );
    assert!(io.opened.is_none(), "the browser was not asked");
    assert!(mock.requests("POST", "/token").is_empty(), "no code exchange");
    assert!(mock.requests("POST", "/register").is_empty(), "no registration");
}

/// `McpManager::login` expands a `!command` client secret with the manager's own runner: its
/// gate is asked, and a denied command never runs and stops the sign-in.
#[test]
fn manager_login_secret_command_asks_the_manager_gate() {
    let env = TestEnv::new();
    let challenge = Arc::new(Mutex::new(String::new()));
    let mock = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, true)),
            _ => empty(404),
        }
    });
    let marker = env.work().join("secret-command-ran");
    let command = format!("touch {}", marker.display());
    let settings = OAuthSettings {
        client_id: Some("configured-id".to_string()),
        client_secret: Some(format!("!{command}")),
        ..OAuthSettings::default()
    };
    let url = format!("{}/mcp", mock.base);
    let server = ServerConfig {
        name: "cfg".to_string(),
        kind: ServerKind::Http(http_config(&url, settings)),
        enabled: false,
        exposure: Exposure::Codemode,
        tool_exposure: Vec::new(),
        description: None,
        timeout_s: 10,
        defined_in: ConfigScope::User,
        project_override: false,
        trust_annotations: false,
    };
    let asked = Arc::new(Mutex::new(Vec::<Access>::new()));
    let seen = asked.clone();
    let gate: AccessGate = Arc::new(move |a: &Access| {
        seen.lock().unwrap().push(a.clone());
        !matches!(a, Access::Exec { .. })
    });
    let manager = McpManager::new(ManagerOpts {
        config: McpConfig {
            servers: vec![server],
            auto_enable_codemode: None,
            diagnostics: Vec::new(),
        },
        cwd: env.work(),
        agent_dir: env.agent_dir(),
        home: env.home(),
        trusted: false,
        gate,
        client_version: "0.0.0-test".into(),
    });
    let mut io = TestIo::new(true, false, challenge);
    let err = manager
        .login("cfg", &mut io, Duration::from_secs(20), &CancelToken::new())
        .expect_err("a denied !command stops the sign-in");

    assert_eq!(err.code, codes::DENIED, "{err:?}");
    assert!(
        asked
            .lock()
            .unwrap()
            .contains(&Access::Exec { command, net: true }),
        "the gate was asked for the command"
    );
    assert!(!marker.exists(), "the denied command did not run");
    assert!(io.opened.is_none(), "the browser was not asked");
    assert!(mock.requests("POST", "/token").is_empty(), "no code exchange");
}

/// loopback redirect
#[test]
fn callback_server() {
    let cancel = CancelToken::new();
    let server = CallbackServer::bind(0).expect("bind loopback");
    let port = server.port();
    assert_ne!(port, 0);
    let client = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        // A favicon request first: it gets a 404 and the wait goes on.
        let _ = ureq::get(&format!("http://127.0.0.1:{port}/favicon.ico")).call();
        let mut reply = ureq::get(&format!("http://127.0.0.1:{port}/callback?code=abc&state=xyz"))
            .call()
            .expect("callback answered");
        reply.body_mut().read_to_string().expect("page body")
    });
    let url = server
        .wait(Duration::from_secs(5), &cancel)
        .expect("callback arrives");
    assert!(
        url.starts_with(&format!("http://127.0.0.1:{port}/callback?")),
        "{url}"
    );
    assert_eq!(
        parse_redirect(&url).expect("parsed"),
        ("abc".to_string(), "xyz".to_string())
    );
    let page = client.join().expect("client thread");
    assert!(page.contains("You can close this tab"), "{page}");

    // Nothing arrives: a timeout, not a hang.
    let quiet = CallbackServer::bind(0).expect("bind loopback");
    let err = quiet
        .wait(Duration::from_millis(150), &cancel)
        .expect_err("no callback");
    assert_eq!(err.code, codes::TIMEOUT);

    // Cancelled: aborted at once.
    let stopped = CancelToken::new();
    stopped.cancel();
    let err = quiet
        .wait(Duration::from_secs(5), &stopped)
        .expect_err("cancelled");
    assert_eq!(err.code, codes::ABORTED);
}

/// storage
#[test]
fn token_file_0600() {
    let env = TestEnv::new();
    let path = env.agent_dir().join(AUTH_FILE);
    let store = TokenStore::open(&path);
    let auth = sample_auth("secret-token");
    store
        .put("svc", "https://mcp.example/mcp", &auth)
        .expect("stored");

    let mode = std::fs::metadata(&path).expect("file").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "token file mode");
    assert_eq!(store.get("svc", "https://mcp.example/mcp"), Some(auth.clone()));
    assert_eq!(store.get("svc", "https://other.example/mcp"), None);
    assert_eq!(store.get("other", "https://mcp.example/mcp"), None);

    // A second URL under the same name keeps the first; the directory holds no temporary files.
    let other = sample_auth("second-token");
    store
        .put("svc", "https://other.example/mcp", &other)
        .expect("stored");
    assert_eq!(store.get("svc", "https://other.example/mcp"), Some(other));
    assert_eq!(store.get("svc", "https://mcp.example/mcp"), Some(auth));
    assert_eq!(std::fs::read_dir(env.agent_dir()).expect("dir").count(), 1);

    // A symbolic link is never followed.
    let target = env.write("elsewhere.json", "{}");
    let link = env.agent_dir().join("linked.json");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let err = TokenStore::open(&link)
        .put("svc", "https://mcp.example/mcp", &sample_auth("x"))
        .expect_err("symlink refused");
    assert_eq!(err.code, codes::UNSAFE_PATH);
    assert_eq!(std::fs::read_to_string(&target).expect("target"), "{}");
}

/// one refresh then retry
#[test]
fn refresh_on_401() {
    let env = TestEnv::new();
    // `reject_all`: the MCP endpoint refuses every token, so only one refresh may happen.
    let make = |reject_all: bool| {
        Mock::start(move |_base: String| {
            move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
                ("POST", "/mcp") => {
                    if !reject_all && req.header("authorization").as_deref() == Some("Bearer new-access") {
                        answer(200, json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
                    } else {
                        empty(401)
                    }
                }
                ("POST", "/token") => {
                    let form = req.form();
                    let refreshed = form.get("grant_type").map(String::as_str) == Some("refresh_token");
                    if refreshed && form.get("refresh_token").map(String::as_str) == Some("rt-1") {
                        answer(
                            200,
                            json!({"access_token": "new-access", "expires_in": 3600, "refresh_token": "rt-2"}),
                        )
                    } else {
                        answer(400, json!({"error": "invalid_grant"}))
                    }
                }
                _ => empty(404),
            }
        })
    };
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    let store_path = env.agent_dir().join(AUTH_FILE);

    // Expired access token, valid refresh token: the 401 is answered with one refresh and a retry.
    let mock = make(false);
    let url = format!("{}/mcp", mock.base);
    let store = Arc::new(TokenStore::open(&store_path));
    let stale = StoredAuth {
        access_token: Some("old-access".to_string()),
        refresh_token: Some("rt-1".to_string()),
        expires_at: Some(now_s() + 3600),
        client_id: Some("cid".to_string()),
        token_endpoint: Some(format!("{}/token", mock.base)),
        ..StoredAuth::default()
    };
    store.put("svc", &url, &stale).expect("stored");
    let tokens: Arc<dyn TokenSource> = Arc::new(OAuthTokens::new("svc", &url, store.clone()));
    let cfg = http_config(&url, OAuthSettings::default());
    let mut transport = HttpTransport::new(&cfg, Vec::new(), Some(tokens), Duration::from_secs(10));
    transport.send(&ping).expect("retried with the refreshed token");
    assert!(transport.recv(Duration::from_secs(1)).expect("recv").is_some());
    let bearers: Vec<String> = mock
        .requests("POST", "/mcp")
        .iter()
        .filter_map(|r| r.header("authorization"))
        .collect();
    assert_eq!(bearers, vec!["Bearer old-access", "Bearer new-access"]);
    assert_eq!(mock.requests("POST", "/token").len(), 1, "exactly one refresh");
    let refreshed = store.get("svc", &url).expect("stored");
    assert_eq!(refreshed.access_token.as_deref(), Some("new-access"));
    assert_eq!(refreshed.refresh_token.as_deref(), Some("rt-2"));

    // The endpoint keeps refusing: one refresh, one retry, then a sign-in is needed.
    let mock = make(true);
    let url = format!("{}/mcp", mock.base);
    let stale = StoredAuth {
        token_endpoint: Some(format!("{}/token", mock.base)),
        ..stale
    };
    store.put("svc", &url, &stale).expect("stored");
    let tokens: Arc<dyn TokenSource> = Arc::new(OAuthTokens::new("svc", &url, store.clone()));
    let cfg = http_config(&url, OAuthSettings::default());
    let mut transport = HttpTransport::new(&cfg, Vec::new(), Some(tokens), Duration::from_secs(10));
    let err = transport.send(&ping).expect_err("still refused");
    assert_eq!(err.code, CODE_NEEDS_AUTH);
    assert_eq!(
        mock.requests("POST", "/mcp").len(),
        2,
        "one attempt and one retry"
    );
    assert_eq!(mock.requests("POST", "/token").len(), 1, "one refresh");

    // A refresh token the server no longer accepts: the tokens go, the client stays.
    let mock = make(false);
    let url = format!("{}/mcp", mock.base);
    let dead = StoredAuth {
        refresh_token: Some("dead".to_string()),
        token_endpoint: Some(format!("{}/token", mock.base)),
        ..stale
    };
    store.put("svc", &url, &dead).expect("stored");
    let tokens: Arc<dyn TokenSource> = Arc::new(OAuthTokens::new("svc", &url, store.clone()));
    let cfg = http_config(&url, OAuthSettings::default());
    let mut transport = HttpTransport::new(&cfg, Vec::new(), Some(tokens), Duration::from_secs(10));
    let err = transport.send(&ping).expect_err("sign-in needed");
    assert_eq!(err.code, CODE_NEEDS_AUTH);
    let kept = store.get("svc", &url).expect("client kept");
    assert_eq!(kept.access_token, None);
    assert_eq!(kept.refresh_token, None);
    assert_eq!(kept.client_id.as_deref(), Some("cid"));
}

/// terminal paste path
#[test]
fn pasted_redirect_url() {
    let env = TestEnv::new();
    let challenge = Arc::new(Mutex::new(String::new()));
    let handler_challenge = challenge.clone();
    let mock = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, false)),
            ("POST", "/token") => token_reply(req, &handler_challenge.lock().unwrap()),
            _ => empty(404),
        }
    });
    let settings = OAuthSettings {
        client_id: Some("paste-client".to_string()),
        ..OAuthSettings::default()
    };
    let store = TokenStore::open(&env.agent_dir().join(AUTH_FILE));
    let url = format!("{}/mcp", mock.base);
    let cfg = http_config(&url, settings);
    // No browser: the user pastes the redirect URL into the terminal.
    let mut io = TestIo::new(false, true, challenge.clone());
    login(
        "paste",
        &cfg,
        &store,
        &mut io,
        Duration::from_secs(20),
        &CancelToken::new(),
        &run_allowed,
    )
    .expect("sign-in with the pasted redirect");
    assert_eq!(
        store.get("paste", &url).expect("stored").access_token.as_deref(),
        Some("at-1")
    );
    assert!(
        io.statuses.iter().any(|s| s.contains("Signed in")),
        "{:?}",
        io.statuses
    );

    // Parsing a pasted URL: a server error is reported with its description.
    let err = parse_redirect("http://127.0.0.1:1/callback?error=access_denied&error_description=no%20thanks")
        .expect_err("refused");
    assert_eq!(err.code, codes::MCP);
    assert!(err.message.contains("no thanks"), "{}", err.message);
}

/// credentials removed
#[test]
fn logout_deletes_tokens() {
    let env = TestEnv::new();
    let path = env.agent_dir().join(AUTH_FILE);
    let store = TokenStore::open(&path);
    store
        .put("svc", "https://a.example/mcp", &sample_auth("at-a"))
        .expect("stored");
    store
        .put("svc", "https://b.example/mcp", &sample_auth("at-b"))
        .expect("stored");

    assert!(store.delete("svc", "https://a.example/mcp").expect("deleted"));
    assert_eq!(store.get("svc", "https://a.example/mcp"), None);
    assert!(
        store.get("svc", "https://b.example/mcp").is_some(),
        "other URL kept"
    );
    assert!(!std::fs::read_to_string(&path).expect("file").contains("at-a"));
    assert!(!store
        .delete("svc", "https://a.example/mcp")
        .expect("second delete"));

    assert!(store.delete("svc", "https://b.example/mcp").expect("deleted"));
    assert_eq!(store.get("svc", "https://b.example/mcp"), None);
    assert!(!path.exists(), "no credentials left, no file left");
}

// ---------------------------------------------------------------------------------------------
// Further checks: URLs, discovery, the redirect rules.
// ---------------------------------------------------------------------------------------------

#[test]
fn discovery_finds_the_issuer_and_refuses_a_mismatch() {
    let good = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, true)),
            _ => empty(404),
        }
    });
    let meta = discover(&format!("{}/mcp", good.base), &OAuthSettings::default()).expect("discovered");
    assert_eq!(meta.issuer, good.base);
    assert_eq!(meta.token_endpoint, format!("{}/token", good.base));
    assert_eq!(
        meta.registration_endpoint,
        Some(format!("{}/register", good.base))
    );
    assert_eq!(meta.scopes_supported, vec!["mcp".to_string()]);

    let wrong = Mock::start(move |_base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => {
                answer(200, metadata("https://elsewhere.example", true))
            }
            _ => empty(404),
        }
    });
    let err =
        discover(&format!("{}/mcp", wrong.base), &OAuthSettings::default()).expect_err("issuer mismatch");
    assert_eq!(err.code, codes::MCP);
    assert!(err.message.contains("issuer"), "{}", err.message);

    // Nothing at all: an mcp error that names what was tried.
    let none = Mock::start(|_base: String| move |_req: &Seen| empty(404));
    let err = discover(&format!("{}/mcp", none.base), &OAuthSettings::default()).expect_err("nothing found");
    assert_eq!(err.code, codes::MCP);
    assert!(
        err.message.contains("/.well-known/oauth-authorization-server"),
        "{}",
        err.message
    );
}

#[test]
fn authorization_url_carries_pkce_state_scope_and_resource() {
    let meta = AuthServerMeta {
        authorization_endpoint: "https://auth.example/authorize".to_string(),
        ..AuthServerMeta::default()
    };
    let url = authorization_url(
        &meta,
        "cid",
        "http://127.0.0.1:4000/callback",
        "challenge-value",
        "state-value",
        Some("mcp offline_access"),
        "https://mcp.example/mcp",
    );
    assert!(url.starts_with("https://auth.example/authorize?"), "{url}");
    let params = query_params(&url);
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "cid");
    assert_eq!(params["code_challenge"], "challenge-value");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["redirect_uri"], "http://127.0.0.1:4000/callback");
    assert_eq!(params["state"], "state-value");
    assert_eq!(params["scope"], "mcp offline_access");
    assert_eq!(params["prompt"], "consent");
    assert_eq!(params["resource"], "https://mcp.example/mcp");
    assert!(!url.contains('#'));
}

#[test]
fn redirect_uri_rules() {
    let plain = OAuthSettings::default();
    assert_eq!(
        redirect_uri(&plain, 5555).expect("uri"),
        "http://127.0.0.1:5555/callback"
    );

    let fixed = OAuthSettings {
        callback_port: Some(7777),
        ..OAuthSettings::default()
    };
    assert_eq!(
        redirect_uri(&fixed, 5555).expect("uri"),
        "http://127.0.0.1:7777/callback"
    );

    let local = OAuthSettings {
        callback_url: Some("http://localhost:9000/cb".to_string()),
        ..OAuthSettings::default()
    };
    assert_eq!(redirect_uri(&local, 1).expect("uri"), "http://localhost:9000/cb");

    let remote = OAuthSettings {
        callback_url: Some("https://example.com/cb".to_string()),
        ..OAuthSettings::default()
    };
    assert_eq!(
        redirect_uri(&remote, 1).expect_err("not loopback").code,
        codes::INVALID
    );
}

#[test]
fn parse_redirect_outcomes() {
    let (code, state) = parse_redirect("http://127.0.0.1:1/callback?code=a%2Fb&state=s+1").expect("parsed");
    assert_eq!((code.as_str(), state.as_str()), ("a/b", "s 1"));
    let (code, state) = parse_redirect("code=c1&state=s1").expect("query only");
    assert_eq!((code.as_str(), state.as_str()), ("c1", "s1"));
    assert_eq!(
        parse_redirect("http://127.0.0.1:1/callback?state=s1")
            .expect_err("no code")
            .code,
        codes::INVALID
    );
}

// ---------------------------------------------------------------------------------------------
// Wave H (lane cli): the OAuth gaps of the FG checkpoint, each against a loopback mock.
// ---------------------------------------------------------------------------------------------

/// A login UI whose "browser" appends `extra` to the redirect (for example `&iss=...`), or which
/// pastes the given URLs in order (one per poll) instead of using a browser.
struct ScriptedIo {
    extra: Option<String>,
    pastes: Vec<String>,
    opened: Option<String>,
    challenge: Arc<Mutex<String>>,
    statuses: Vec<String>,
}

impl ScriptedIo {
    fn browser(extra: &str, challenge: Arc<Mutex<String>>) -> ScriptedIo {
        ScriptedIo {
            extra: Some(extra.to_string()),
            pastes: Vec::new(),
            opened: None,
            challenge,
            statuses: Vec::new(),
        }
    }
    fn pasting(pastes: Vec<String>, challenge: Arc<Mutex<String>>) -> ScriptedIo {
        ScriptedIo {
            extra: None,
            pastes,
            opened: None,
            challenge,
            statuses: Vec::new(),
        }
    }
}

impl LoginIo for ScriptedIo {
    fn open_url(&mut self, url: &str) {
        let params = query_params(url);
        *self.challenge.lock().unwrap() = params.get("code_challenge").cloned().unwrap_or_default();
        self.opened = Some(url.to_string());
        if let Some(extra) = self.extra.clone() {
            let target = format!(
                "{}?code=the-code&state={}{extra}",
                params["redirect_uri"], params["state"]
            );
            thread::spawn(move || {
                if let Ok(mut reply) = ureq::get(&target).call() {
                    let _ = reply.body_mut().read_to_string();
                }
            });
        }
    }
    fn pasted_redirect(&mut self) -> Option<String> {
        let opened = self.opened.clone()?;
        if self.pastes.is_empty() {
            return None;
        }
        let params = query_params(&opened);
        let next = self.pastes.remove(0);
        Some(
            next.replace("{redirect}", &params["redirect_uri"])
                .replace("{state}", &params["state"]),
        )
    }
    fn status(&mut self, text: &str) {
        self.statuses.push(text.to_string());
    }
}

fn as_mock(iss_supported: bool) -> (Mock, Arc<Mutex<String>>) {
    let challenge = Arc::new(Mutex::new(String::new()));
    let handler_challenge = challenge.clone();
    let mock = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => {
                let mut meta = metadata(&base, false);
                if iss_supported {
                    meta["authorization_response_iss_parameter_supported"] = json!(true);
                }
                answer(200, meta)
            }
            ("POST", "/token") => token_reply(req, &handler_challenge.lock().unwrap()),
            _ => empty(404),
        }
    });
    (mock, challenge)
}

fn client_settings() -> OAuthSettings {
    OAuthSettings {
        client_id: Some("h-client".to_string()),
        scope: Some("mcp".to_string()),
        ..OAuthSettings::default()
    }
}

/// RFC 9207: a response from another issuer is refused; a server that advertises the
/// parameter must send it; the right issuer signs in.
#[test]
fn authorization_response_issuer_is_checked() {
    for (iss_supported, extra, ok) in [
        (false, "&iss=https%3A%2F%2Fevil.example".to_string(), false),
        (true, String::new(), false),
        (true, "&iss={base}".to_string(), true),
        (false, String::new(), true),
    ] {
        let env = TestEnv::new();
        let (mock, challenge) = as_mock(iss_supported);
        let extra = extra.replace("{base}", &mock.base);
        let store = TokenStore::open(&env.agent_dir().join(AUTH_FILE));
        let url = format!("{}/mcp", mock.base);
        let mut io = ScriptedIo::browser(&extra, challenge);
        let result = login(
            "iss",
            &http_config(&url, client_settings()),
            &store,
            &mut io,
            Duration::from_secs(20),
            &CancelToken::new(),
            &run_allowed,
        );
        assert_eq!(
            result.is_ok(),
            ok,
            "iss_supported {iss_supported}, extra {extra:?}: {result:?}"
        );
        if !ok {
            let e = result.unwrap_err();
            assert_eq!(e.code, codes::MCP);
            assert!(e.message.contains("iss"), "{}", e.message);
            assert!(mock.requests("POST", "/token").is_empty(), "no code exchange");
        }
    }
}

/// RFC 9728 section 3.3: protected resource metadata for another resource is refused.
#[test]
fn resource_metadata_must_name_the_server() {
    let mock = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", p) if p.starts_with("/.well-known/oauth-protected-resource") => answer(
                200,
                json!({"resource": format!("{base}/other"), "authorization_servers": [base]}),
            ),
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, false)),
            _ => empty(404),
        }
    });
    let err = discover(&format!("{}/mcp", mock.base), &OAuthSettings::default()).expect_err("mismatch");
    assert_eq!(err.code, codes::MCP);
    assert!(err.message.contains("/other"), "{}", err.message);

    let good = Mock::start(move |base: String| {
        move |req: &Seen| match (req.method.as_str(), req.path.as_str()) {
            ("GET", p) if p.starts_with("/.well-known/oauth-protected-resource") => answer(
                200,
                json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}),
            ),
            ("GET", "/.well-known/oauth-authorization-server") => answer(200, metadata(&base, false)),
            _ => empty(404),
        }
    });
    let meta = discover(&format!("{}/mcp", good.base), &OAuthSettings::default()).expect("matching resource");
    assert_eq!(meta.issuer, good.base);
}

/// A pasted URL for another path is refused with a message; the right one completes.
#[test]
fn pasted_redirect_must_match_the_redirect_uri() {
    let env = TestEnv::new();
    let (mock, challenge) = as_mock(false);
    let store = TokenStore::open(&env.agent_dir().join(AUTH_FILE));
    let url = format!("{}/mcp", mock.base);
    let mut io = ScriptedIo::pasting(
        vec![
            "http://127.0.0.1:1/elsewhere?code=the-code&state={state}".to_string(),
            "{redirect}?code=the-code&state={state}".to_string(),
        ],
        challenge,
    );
    login(
        "paste-h",
        &http_config(&url, client_settings()),
        &store,
        &mut io,
        Duration::from_secs(20),
        &CancelToken::new(),
        &run_allowed,
    )
    .expect("the matching paste signs in");
    assert!(
        io.statuses
            .iter()
            .any(|s| s.contains("does not belong to this sign-in")),
        "{:?}",
        io.statuses
    );
    assert_eq!(
        mock.requests("POST", "/token").len(),
        1,
        "only the matching paste is exchanged"
    );
}

/// Step-up: a 403 `insufficient_scope` drops the token, keeps the asked scope, and the next
/// sign-in requests it together with the configured scope.
#[test]
fn insufficient_scope_steps_up_the_next_sign_in() {
    let env = TestEnv::new();
    // The MCP endpoint answers every POST with the step-up challenge (the `Mock` above sends
    // no custom headers, so this one is a raw listener).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let mcp_url = format!("http://{}/mcp", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut length = 0usize;
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let mut out = stream;
            let _ = out.write_all(
                b"HTTP/1.1 403 Forbidden\r\nWWW-Authenticate: Bearer error=\"insufficient_scope\", scope=\"mcp admin\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    let store = Arc::new(TokenStore::open(&env.agent_dir().join(AUTH_FILE)));
    store.put("step", &mcp_url, &sample_auth("old-token")).unwrap();
    let tokens: Arc<dyn TokenSource> = Arc::new(OAuthTokens::new("step", &mcp_url, store.clone()));
    let cfg = http_config(&mcp_url, client_settings());
    let mut t = HttpTransport::new(&cfg, Vec::new(), Some(tokens), Duration::from_secs(5));
    let err = t
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .expect_err("insufficient scope");
    assert_eq!(err.code, CODE_NEEDS_AUTH);
    let kept = store.get("step", &mcp_url).expect("still stored");
    assert_eq!(kept.access_token, None, "the token is dropped");
    assert_eq!(kept.scope.as_deref(), Some("mcp admin"));

    // The next sign-in asks for both scopes.
    let (mock, challenge) = as_mock(false);
    let as_cfg = HttpConfig {
        oauth: Some(OAuthSettings {
            auth_server_metadata_url: Some(format!("{}/.well-known/oauth-authorization-server", mock.base)),
            ..client_settings()
        }),
        ..http_config(&mcp_url, client_settings())
    };
    let mut io = TestIo::new(true, false, challenge);
    login(
        "step",
        &as_cfg,
        &store,
        &mut io,
        Duration::from_secs(20),
        &CancelToken::new(),
        &run_allowed,
    )
    .expect("step-up sign-in");
    let params = query_params(io.opened.as_deref().expect("opened"));
    assert_eq!(params["scope"], "mcp admin");
}
