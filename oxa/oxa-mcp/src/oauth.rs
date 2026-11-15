//! F5: OAuth for HTTP servers (PKCE, discovery, dynamic registration, callback server, token store).
// ported from pi packages/mcp/src/oauth/*
use crate::{ClientRegistration, HttpConfig, OAuthSettings, TokenSource, PROTOCOL_VERSION};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use oxa_proto::{codes, CancelToken, OxaError, Value};
use serde_json::{json, Map};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Stored credentials of one (server name, URL) pair.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredAuth {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` = unknown.
    pub expires_at: Option<u64>,
    pub scope: Option<String>,
    /// The registered or configured client.
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token_endpoint: Option<String>,
}

/// `<agent dir>/mcp-auth.json`: mode 0600, written atomically, never through a symlink.
/// Credentials belong to a server name *and* URL.
pub struct TokenStore {
    pub(crate) path: PathBuf,
    pub(crate) lock: Mutex<()>,
}

impl TokenStore {
    pub fn open(path: &Path) -> TokenStore {
        TokenStore {
            path: path.to_path_buf(),
            lock: Mutex::new(()),
        }
    }

    /// `None` when nothing is stored (or the file is missing / unreadable).
    pub fn get(&self, server: &str, url: &str) -> Option<StoredAuth> {
        let _guard = self.guard();
        let doc = self.read_doc().ok()?;
        let entry = doc.get(server)?.as_object()?.get(url)?;
        auth_from_value(entry)
    }

    pub fn put(&self, server: &str, url: &str, auth: &StoredAuth) -> Result<(), OxaError> {
        let _guard = self.guard();
        let mut doc = self.read_doc()?;
        let entry = doc
            .entry(server.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        if let Some(by_url) = entry.as_object_mut() {
            by_url.insert(url.to_string(), auth_to_value(auth));
        }
        self.write_doc(&doc)
    }

    /// `Ok(false)` when nothing was stored.
    pub fn delete(&self, server: &str, url: &str) -> Result<bool, OxaError> {
        let _guard = self.guard();
        let mut doc = self.read_doc()?;
        let mut removed = false;
        let mut server_empty = false;
        if let Some(by_url) = doc.get_mut(server).and_then(Value::as_object_mut) {
            removed = by_url.remove(url).is_some();
            server_empty = by_url.is_empty();
        }
        if !removed {
            return Ok(false);
        }
        if server_empty {
            doc.remove(server);
        }
        self.write_doc(&doc)?;
        Ok(true)
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The whole file as a JSON object; a missing or empty file is an empty object.
    fn read_doc(&self) -> Result<Map<String, Value>, OxaError> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(OxaError::new(
                    codes::UNSAFE_PATH,
                    format!(
                        "{} is a symbolic link; refusing to read credentials through it",
                        self.path.display()
                    ),
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(e) => return Err(OxaError::io("cannot read the MCP token file", &e)),
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| OxaError::io("cannot read the MCP token file", &e))?;
        if text.trim().is_empty() {
            return Ok(Map::new());
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(doc)) => Ok(doc),
            _ => Err(OxaError::new(
                codes::INVALID,
                format!("{} is not a JSON object", self.path.display()),
            )),
        }
    }

    /// Replace the file; an empty document removes it.
    fn write_doc(&self, doc: &Map<String, Value>) -> Result<(), OxaError> {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.file_type().is_symlink() {
                return Err(OxaError::new(
                    codes::UNSAFE_PATH,
                    format!(
                        "{} is a symbolic link; refusing to write credentials through it",
                        self.path.display()
                    ),
                ));
            }
        }
        if doc.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(OxaError::io("cannot remove the MCP token file", &e)),
            };
        }
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(doc.clone()))
            .map_err(|e| OxaError::new(codes::INTERNAL, format!("cannot encode the MCP token file: {e}")))?;
        bytes.push(b'\n');
        write_private(&self.path, &bytes)
    }
}

/// Write `bytes` to a new 0600 file beside `path`, then rename it over `path`.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), OxaError> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir).map_err(|e| OxaError::io("cannot create the agent directory", &e))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mcp-auth.json".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(OxaError::io("cannot write the MCP token file", &e));
    }
    Ok(())
}

fn auth_to_value(auth: &StoredAuth) -> Value {
    let mut obj = Map::new();
    let texts: [(&str, &Option<String>); 6] = [
        ("access_token", &auth.access_token),
        ("refresh_token", &auth.refresh_token),
        ("scope", &auth.scope),
        ("client_id", &auth.client_id),
        ("client_secret", &auth.client_secret),
        ("token_endpoint", &auth.token_endpoint),
    ];
    for (key, value) in texts {
        if let Some(text) = value {
            obj.insert(key.to_string(), Value::String(text.clone()));
        }
    }
    if let Some(expires) = auth.expires_at {
        obj.insert("expires_at".to_string(), json!(expires));
    }
    Value::Object(obj)
}

fn auth_from_value(value: &Value) -> Option<StoredAuth> {
    let obj = value.as_object()?;
    let text = |key: &str| {
        obj.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(StoredAuth {
        access_token: text("access_token"),
        refresh_token: text("refresh_token"),
        expires_at: obj.get("expires_at").and_then(Value::as_u64),
        scope: text("scope"),
        client_id: text("client_id"),
        client_secret: text("client_secret"),
        token_endpoint: text("token_endpoint"),
    })
}

/// PKCE (S256): the verifier is the base64url (no padding) of `random`; the challenge is the
/// base64url of its SHA-256. Pure.
pub fn pkce_pair(random: &[u8; 32]) -> (String, String) {
    let verifier = URL_SAFE_NO_PAD.encode(random);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// 32 bytes from the OS (`getrandom`).
pub fn random_bytes() -> Result<[u8; 32], OxaError> {
    let mut buf = [0u8; 32];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut filled = 0;
        while filled < buf.len() {
            // SAFETY: the pointer and length describe the unfilled tail of `buf`, which is alive.
            let n = unsafe { libc::getrandom(buf[filled..].as_mut_ptr().cast(), buf.len() - filled, 0) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(OxaError::io("cannot read random bytes", &e));
            }
            filled += n as usize;
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .map_err(|e| OxaError::io("cannot read random bytes", &e))?;
    }
    Ok(buf)
}

/// What discovery found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthServerMeta {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    /// RFC 9207 `authorization_response_iss_parameter_supported`: the authorization response
    /// must then carry `iss`.
    pub iss_parameter_supported: bool,
}

/// Find the authorization server: protected resource metadata (RFC 9728) of the MCP URL, then
/// the authorization server metadata (RFC 8414; the issuer must match), or
/// `settings.auth_server_metadata_url` instead of discovery (HTTPS required except on
/// localhost). Errors: `mcp` with what was tried.
pub fn discover(mcp_url: &str, settings: &OAuthSettings) -> Result<AuthServerMeta, OxaError> {
    if let Some(url) = &settings.auth_server_metadata_url {
        secure_endpoint(url)?;
        let reply = get(
            url,
            &[
                ("Accept", "application/json"),
                ("MCP-Protocol-Version", PROTOCOL_VERSION),
            ],
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(mcp_error(format!(
                "HTTP {} loading authorization server metadata from {url}",
                reply.status
            )));
        }
        return parse_as_metadata(&reply.body);
    }
    let server = parse_url(mcp_url).ok_or_else(|| mcp_error(format!("invalid MCP server URL {mcp_url}")))?;
    let mut tried = Vec::new();
    let as_url = authorization_server_from_resource(mcp_url, &server, &mut tried)?
        .unwrap_or_else(|| format!("{}/", origin(&server)));
    match discover_as(&as_url, &mut tried)? {
        Some(meta) => Ok(meta),
        None => Err(mcp_error(format!(
            "no authorization server metadata found for {as_url}; tried {}",
            tried.join(", ")
        ))
        .with_hint("set oauth.authServerMetadataUrl in mcp.json")),
    }
}

/// The redirect URI: `settings.callback_url` exactly as written, else
/// `http://127.0.0.1:<callback_port or the bound port>/callback`. A `callback_url` must use
/// HTTP on `localhost`, `127.0.0.1` or `[::1]` (`Err(invalid)`).
pub fn redirect_uri(settings: &OAuthSettings, bound_port: u16) -> Result<String, OxaError> {
    match &settings.callback_url {
        Some(url) => {
            let ok = parse_url(url).is_some_and(|u| u.scheme == "http" && loopback_host(&u.host))
                && !url.contains(['?', '#']);
            if !ok {
                return Err(OxaError::new(
                    codes::INVALID,
                    "oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment",
                ));
            }
            Ok(url.clone())
        }
        None => Ok(format!(
            "http://127.0.0.1:{}/callback",
            settings.callback_port.unwrap_or(bound_port)
        )),
    }
}

/// Build the authorization URL (response_type=code, S256 challenge, state, scope, resource).
pub fn authorization_url(
    meta: &AuthServerMeta,
    client_id: &str,
    redirect: &str,
    challenge: &str,
    state: &str,
    scope: Option<&str>,
    resource: &str,
) -> String {
    let mut params: Vec<(&str, String)> = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("code_challenge", challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("redirect_uri", redirect.to_string()),
        ("state", state.to_string()),
    ];
    if let Some(scope) = scope.map(str::trim).filter(|s| !s.is_empty()) {
        params.push(("scope", scope.to_string()));
        if scope.split_whitespace().any(|s| s == "offline_access") {
            params.push(("prompt", "consent".to_string()));
        }
    }
    if !resource.is_empty() {
        params.push(("resource", resource.to_string()));
    }
    let base = meta.authorization_endpoint.split('#').next().unwrap_or("");
    let sep = if !base.contains('?') {
        "?"
    } else if base.ends_with('?') || base.ends_with('&') {
        ""
    } else {
        "&"
    };
    format!("{base}{sep}{}", encode_pairs(&params))
}

/// Extract `(code, state)` from a redirect URL (as received by the callback server or pasted
/// by the user). `Err(mcp)` with the server's `error_description` when it carries `error`.
pub fn parse_redirect(url: &str) -> Result<(String, String), OxaError> {
    let without_fragment = url.trim().split('#').next().unwrap_or("");
    let query = match without_fragment.split_once('?') {
        Some((_, query)) => query,
        None if without_fragment.contains('=') => without_fragment,
        None => "",
    };
    let pairs = query_pairs(query);
    let value = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name.as_str() == key)
            .map(|(_, v)| v.clone())
    };
    if let Some(error) = value("error") {
        let description = value("error_description").unwrap_or(error);
        return Err(mcp_error(format!(
            "the authorization server refused the sign-in: {description}"
        )));
    }
    let code = value("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| OxaError::new(codes::INVALID, "the redirect URL carries no authorization code"))?;
    Ok((code, value("state").unwrap_or_default()))
}

/// The loopback callback server.
pub struct CallbackServer {
    pub(crate) listener: std::net::TcpListener,
}

/// How often `wait` polls for a connection.
const ACCEPT_POLL: Duration = Duration::from_millis(20);
/// Longest request head the callback server reads.
const MAX_HEAD_BYTES: usize = 16 * 1024;

const CALLBACK_DONE_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>oxa</title></head><body><p>Sign-in complete. You can close this tab.</p></body></html>";
const CALLBACK_FAILED_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>oxa</title></head><body><p>The sign-in was not completed. You can close this tab and try again.</p></body></html>";

impl CallbackServer {
    /// Bind `127.0.0.1:<port>` (`0` = any free port).
    pub fn bind(port: u16) -> Result<CallbackServer, OxaError> {
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
            OxaError::io(
                &format!("cannot listen on 127.0.0.1:{port} for the sign-in callback"),
                &e,
            )
        })?;
        Ok(CallbackServer { listener })
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Wait for one request to the callback path and answer it with a small "you can close
    /// this tab" page. Returns the full request URL. `Err(timeout)` / `Err(aborted)`.
    /// The callback path is `/callback`; any other GET that carries `code` or `error` in its
    /// query (a server-specific path from `oauth.callbackUrl`) counts too. Other requests get
    /// a 404 and the wait goes on.
    pub fn wait(&self, timeout: Duration, cancel: &CancelToken) -> Result<String, OxaError> {
        let deadline = Instant::now() + timeout;
        self.listener
            .set_nonblocking(true)
            .map_err(|e| OxaError::io("cannot wait for the sign-in callback", &e))?;
        loop {
            if cancel.is_cancelled() {
                return Err(OxaError::new(codes::ABORTED, "sign-in cancelled"));
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(url) = self.answer(stream) {
                        return Ok(url);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(OxaError::io("sign-in callback", &e)),
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(OxaError::new(
                    codes::TIMEOUT,
                    "no sign-in callback arrived in time",
                ));
            }
            std::thread::sleep(ACCEPT_POLL.min(deadline - now));
        }
    }

    /// Read one request, answer it, and return its URL when it is the callback.
    fn answer(&self, stream: TcpStream) -> Option<String> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).ok()? == 0 {
            return None;
        }
        let mut total = request_line.len();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n;
                    if line == "\r\n" || line == "\n" || total > MAX_HEAD_BYTES {
                        break;
                    }
                }
            }
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("").to_string();
        let path = target.split('?').next().unwrap_or("");
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let pairs = query_pairs(query);
        let has_code = pairs.iter().any(|(k, _)| k == "code");
        let has_error = pairs.iter().any(|(k, _)| k == "error");
        let is_callback = method == "GET" && (path == "/callback" || has_code || has_error);
        let (status, body) = if !is_callback {
            ("404 Not Found", "Not found")
        } else if has_error && !has_code {
            ("200 OK", CALLBACK_FAILED_PAGE)
        } else {
            ("200 OK", CALLBACK_DONE_PAGE)
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut out = &stream;
        let _ = out.write_all(response.as_bytes());
        let _ = out.flush();
        if is_callback {
            Some(format!("http://127.0.0.1:{}{target}", self.port()))
        } else {
            None
        }
    }
}

/// How a sign-in talks to the user. Implemented by the `/mcp` selector and by `oxa mcp login`.
pub trait LoginIo {
    /// Show (and try to open) the authorization URL.
    fn open_url(&mut self, url: &str);
    /// A pasted redirect URL, if the user typed one since the last call (terminals only);
    /// polled about twice a second while waiting for the browser.
    fn pasted_redirect(&mut self) -> Option<String>;
    fn status(&mut self, text: &str);
}

/// Run a complete sign-in for one server and store the tokens: discover, obtain a client
/// (configured `clientId`, a client stored earlier, or dynamic registration under
/// `clientName`, default `oxa`), open the authorization page, wait for the callback or a
/// pasted redirect URL, check `state`, exchange the code. `timeout` bounds the whole sign-in.
/// `ClientRegistration::Cimd` is `Err(unsupported_in_oxa)`.
pub fn login(
    server: &str,
    cfg: &HttpConfig,
    store: &TokenStore,
    io: &mut dyn LoginIo,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<(), OxaError> {
    let deadline = Instant::now() + timeout;
    let default_settings = OAuthSettings::default();
    let settings = cfg.oauth.as_ref().unwrap_or(&default_settings);
    if settings.client_registration == ClientRegistration::Cimd {
        return Err(OxaError::unsupported(
            "client ID metadata documents (oauth.clientRegistration \"cimd\")",
        ));
    }
    if cancel.is_cancelled() {
        return Err(OxaError::new(codes::ABORTED, "sign-in cancelled"));
    }
    let stored = store.get(server, &cfg.url);
    let fixed_port = fixed_callback_port(settings)?;
    let callback = CallbackServer::bind(fixed_port.unwrap_or(0))?;
    let redirect = redirect_uri(settings, callback.port())?;
    let meta = discover(&cfg.url, settings)?;

    // A stored client is reused only when the redirect URI is fixed: a dynamically registered
    // client is bound to the redirect URI it was registered with.
    let stored_client = stored.as_ref().and_then(|a| a.client_id.clone());
    let (client_id, client_secret) = if let Some(id) = settings.client_id.clone() {
        (id, expand_secret(settings.client_secret.as_deref())?)
    } else if let (ClientRegistration::Auto, Some(_), Some(id)) =
        (settings.client_registration, fixed_port, stored_client)
    {
        (id, stored.as_ref().and_then(|a| a.client_secret.clone()))
    } else {
        register_client(&meta, &redirect, settings)?
    };

    // Keep the client even when this sign-in does not finish; the tokens are replaced below.
    let mut keep = stored.unwrap_or_default();
    let keep_scope = keep.scope.clone();
    keep.client_id = Some(client_id.clone());
    keep.client_secret = client_secret.clone();
    keep.token_endpoint = Some(meta.token_endpoint.clone());
    store.put(server, &cfg.url, &keep)?;

    let (verifier, challenge) = pkce_pair(&random_bytes()?);
    let state = URL_SAFE_NO_PAD.encode(random_bytes()?);
    let resource = cfg.url.split('#').next().unwrap_or(cfg.url.as_str()).to_string();
    // Step-up (pi mcp.md "If a server later requests additional scope, Pi asks for sign-in
    // again"): the scope a server asked for in its `WWW-Authenticate` challenge is kept with the
    // credentials (`OAuthTokens::step_up`) and requested together with the configured one.
    let scope = union_scope(settings.scope.as_deref(), keep_scope.as_deref());
    let auth_url = authorization_url(
        &meta,
        &client_id,
        &redirect,
        &challenge,
        &state,
        scope.as_deref(),
        &resource,
    );
    io.open_url(&auth_url);
    io.status("Waiting for the sign-in to finish in the browser, or paste the redirect URL here.");

    let (code, returned_state) = loop {
        if cancel.is_cancelled() {
            return Err(OxaError::new(codes::ABORTED, "sign-in cancelled"));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(OxaError::new(
                codes::TIMEOUT,
                format!("sign-in to {server} timed out"),
            ));
        }
        if let Some(pasted) = io.pasted_redirect() {
            if !same_redirect_target(&pasted, &redirect) {
                io.status(&format!(
                    "That URL does not belong to this sign-in: it must start with {redirect}"
                ));
                continue;
            }
            match parse_redirect(&pasted) {
                Ok(found) => {
                    check_issuer(&pasted, &meta)?;
                    break found;
                }
                Err(e) if e.code == codes::INVALID => {
                    io.status(&format!("That is not a redirect URL with a code: {}", e.message));
                }
                Err(e) => return Err(e),
            }
        }
        match callback.wait(LOGIN_POLL.min(deadline - now), cancel) {
            Ok(url) => match parse_redirect(&url) {
                Ok(found) => {
                    check_issuer(&url, &meta)?;
                    break found;
                }
                Err(e) if e.code == codes::INVALID => {
                    io.status(&format!("The callback carried no code: {}", e.message));
                }
                Err(e) => return Err(e),
            },
            Err(e) if e.code == codes::TIMEOUT => {}
            Err(e) => return Err(e),
        }
    };
    if returned_state != state {
        return Err(mcp_error(
            "the authorization response belongs to a different sign-in",
        ));
    }

    let granted = token_request(
        &meta.token_endpoint,
        &client_id,
        client_secret.as_deref(),
        vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect.clone()),
        ],
        &resource,
    )?;
    let obj = match granted {
        TokenReply::Granted(obj) => obj,
        TokenReply::Refused { error, description } => {
            return Err(mcp_error(format!(
                "the token request failed ({error}): {description}"
            )))
        }
    };
    let mut auth = StoredAuth {
        client_id: Some(client_id),
        client_secret,
        token_endpoint: Some(meta.token_endpoint.clone()),
        ..StoredAuth::default()
    };
    apply_grant(&mut auth, &obj)?;
    if auth.scope.is_none() {
        auth.scope = scope.clone();
    }
    store.put(server, &cfg.url, &auth)?;
    io.status(&format!("Signed in to {server}."));
    Ok(())
}

/// RFC 9207: the `iss` of an authorization response must name the issuer the sign-in was
/// started with; a server that advertises the parameter must send it (mix-up defence).
fn check_issuer(redirect_url: &str, meta: &AuthServerMeta) -> Result<(), OxaError> {
    let query = redirect_url
        .trim()
        .split('#')
        .next()
        .unwrap_or("")
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or_else(|| redirect_url.trim());
    let iss = query_pairs(query)
        .into_iter()
        .find(|(k, _)| k == "iss")
        .map(|(_, v)| v);
    match iss {
        Some(iss) if trim_slash(&iss) != trim_slash(&meta.issuer) => Err(mcp_error(format!(
            "the authorization response comes from issuer {iss}, but the sign-in was started with {}",
            meta.issuer
        ))),
        None if meta.iss_parameter_supported => Err(mcp_error(
            "the authorization response carries no iss although the authorization server requires it (RFC 9207)",
        )),
        _ => Ok(()),
    }
}

/// A pasted redirect URL must point at this sign-in's redirect URI (scheme, host, port and
/// path); a bare query (`code=..&state=..`) is accepted as is.
fn same_redirect_target(pasted: &str, redirect: &str) -> bool {
    let pasted = pasted.trim();
    if !pasted.contains("://") {
        return true;
    }
    let target = |u: &str| -> Option<(String, String, Option<u16>, String)> {
        let url = parse_url(u.split(['?', '#']).next().unwrap_or(""))?;
        Some((
            url.scheme,
            url.host.to_ascii_lowercase(),
            url.port,
            trim_slash(&url.path).to_string(),
        ))
    };
    match (target(pasted), target(redirect)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The scopes of `a` and `b` together (space-separated, first occurrence order).
fn union_scope(a: Option<&str>, b: Option<&str>) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for s in [a, b].into_iter().flatten().flat_map(str::split_whitespace) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    (!out.is_empty()).then(|| out.join(" "))
}

/// The `scope` of a `WWW-Authenticate: Bearer ...` challenge.
pub(crate) fn challenge_scope(header: &str) -> Option<String> {
    let at = header.find("scope=")?;
    let rest = &header[at + "scope=".len()..];
    let value = if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next().unwrap_or("")
    } else {
        rest.split([',', ' ']).next().unwrap_or("")
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The [`TokenSource`] of one server over a [`TokenStore`]: returns the stored access token,
/// refreshing it first when it expired; `refresh` exchanges the refresh token (after a 401).
pub struct OAuthTokens {
    pub(crate) server: String,
    pub(crate) url: String,
    pub(crate) store: Arc<TokenStore>,
}

/// Seconds of margin before `expires_at` at which a token counts as expired.
const EXPIRY_SKEW_S: u64 = 60;
/// How long one wait for the browser callback lasts before the pasted-URL check runs again
/// (about twice a second, as `LoginIo::pasted_redirect` promises).
const LOGIN_POLL: Duration = Duration::from_millis(250);

impl OAuthTokens {
    pub fn new(server: &str, url: &str, store: Arc<TokenStore>) -> OAuthTokens {
        OAuthTokens {
            server: server.to_string(),
            url: url.to_string(),
            store,
        }
    }
}

impl OAuthTokens {
    /// Merge `scope` into the stored credentials' scope; `drop_token` forgets the access token.
    fn merge_scope(&self, scope: &str, drop_token: bool) {
        let mut auth = self.store.get(&self.server, &self.url).unwrap_or_default();
        let merged = union_scope(auth.scope.as_deref(), Some(scope));
        if merged == auth.scope && !drop_token {
            return;
        }
        auth.scope = merged;
        if drop_token {
            auth.access_token = None;
            auth.expires_at = None;
        }
        let _ = self.store.put(&self.server, &self.url, &auth);
    }
}

impl TokenSource for OAuthTokens {
    fn step_up(&self, scope: &str) {
        self.merge_scope(scope, true);
    }
    fn remember_scope(&self, scope: &str) {
        self.merge_scope(scope, false);
    }

    fn token(&self) -> Result<Option<String>, OxaError> {
        let Some(auth) = self.store.get(&self.server, &self.url) else {
            return Ok(None);
        };
        let Some(access) = auth.access_token.clone() else {
            return Ok(None);
        };
        let expired = auth
            .expires_at
            .is_some_and(|expires| now_s().saturating_add(EXPIRY_SKEW_S) >= expires);
        if !expired {
            return Ok(Some(access));
        }
        if auth.refresh_token.is_none() {
            return Ok(None);
        }
        self.refresh()
    }

    fn refresh(&self) -> Result<Option<String>, OxaError> {
        let Some(auth) = self.store.get(&self.server, &self.url) else {
            return Ok(None);
        };
        let (Some(refresh), Some(endpoint), Some(client_id)) = (
            auth.refresh_token.clone(),
            auth.token_endpoint.clone(),
            auth.client_id.clone(),
        ) else {
            return Ok(None);
        };
        let reply = token_request(
            &endpoint,
            &client_id,
            auth.client_secret.as_deref(),
            vec![
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", refresh),
            ],
            &self.url,
        )?;
        match reply {
            TokenReply::Granted(obj) => {
                let mut next = auth;
                let access = apply_grant(&mut next, &obj)?;
                self.store.put(&self.server, &self.url, &next)?;
                Ok(Some(access))
            }
            TokenReply::Refused { error, description } => {
                if error == "invalid_grant" {
                    // The refresh token is dead: drop the tokens, keep the client.
                    let mut next = auth;
                    next.access_token = None;
                    next.refresh_token = None;
                    next.expires_at = None;
                    self.store.put(&self.server, &self.url, &next)?;
                    return Ok(None);
                }
                if matches!(error.as_str(), "server_error" | "temporarily_unavailable") {
                    return Err(mcp_error(format!("the token refresh failed: {description}")));
                }
                Ok(None)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers: URLs, HTTP, discovery, token requests.
// ---------------------------------------------------------------------------------------------

fn mcp_error(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::MCP, message)
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A parsed `http` or `https` URL (only what the OAuth code needs).
struct Url {
    scheme: String,
    /// `host[:port]` as written.
    authority: String,
    /// `host` without port; IPv6 literals keep their brackets.
    host: String,
    port: Option<u16>,
    /// Path with a leading `/` (`/` when absent).
    path: String,
}

fn parse_url(raw: &str) -> Option<Url> {
    let (scheme, rest) = raw.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = rest.split('#').next().unwrap_or("");
    let before_query = rest.split('?').next().unwrap_or("");
    let (authority, path) = match before_query.find('/') {
        Some(at) => (&before_query[..at], before_query[at..].to_string()),
        None => (before_query, "/".to_string()),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = authority[..=end].to_string();
        let after = &authority[end + 1..];
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?.parse::<u16>().ok()?)
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(port.parse::<u16>().ok()?)),
            None => (authority.to_string(), None),
        }
    };
    Some(Url {
        scheme,
        authority: authority.to_string(),
        host,
        port,
        path,
    })
}

fn origin(url: &Url) -> String {
    format!("{}://{}", url.scheme, url.authority)
}

/// The path without one trailing slash; empty for the root.
fn path_suffix(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

fn trim_slash(value: &str) -> &str {
    value.strip_suffix('/').unwrap_or(value)
}

fn loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// https, or http on a loopback host; anything else is refused.
fn secure_endpoint(raw: &str) -> Result<(), OxaError> {
    let url = parse_url(raw).ok_or_else(|| mcp_error(format!("invalid OAuth endpoint {raw}")))?;
    if url.scheme == "https" || loopback_host(&url.host) {
        Ok(())
    } else {
        Err(mcp_error(format!(
            "OAuth endpoint {raw} must use https (http only on localhost)"
        )))
    }
}

/// Unreserved characters stay; everything else is percent-encoded.
fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn encode_pairs<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                encode_component(k.as_ref()),
                encode_component(v.as_ref())
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let value = bytes
                    .get(i + 1..i + 3)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match value {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// A finished HTTP exchange: status and the body as text.
struct Reply {
    status: u16,
    body: String,
}

fn http_agent() -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)));
    // P4: offline, the sign-in requests are direct and never follow a redirect. A proxy
    // (configured or from the environment) would be a connection to its host; a redirect
    // target is not checked by the offline gate.
    if oxa_proto::offline() {
        builder = builder.proxy(None).max_redirects(0);
    }
    ureq::Agent::new_with_config(builder.build())
}

/// P4: offline, a redirect is refused before its target is used.
fn check_offline_redirect(url: &str, status: u16) -> Result<(), OxaError> {
    if oxa_proto::offline() && (300..400).contains(&status) {
        return Err(oxa_proto::offline_error(&format!(
            "the OAuth redirect from {url}"
        )));
    }
    Ok(())
}

fn unreachable_error(url: &str, e: ureq::Error) -> OxaError {
    match e {
        ureq::Error::Timeout(_) => OxaError::new(codes::TIMEOUT, format!("{url} did not answer in time")),
        other => mcp_error(format!("cannot reach {url}: {other}")),
    }
}

fn get(url: &str, headers: &[(&str, &str)]) -> Result<Reply, OxaError> {
    crate::offline::check(url)?;
    let agent = http_agent();
    let mut req = agent.get(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let mut resp = req.call().map_err(|e| unreachable_error(url, e))?;
    let status = resp.status().as_u16();
    check_offline_redirect(url, status)?;
    let bytes = resp
        .body_mut()
        .read_to_vec()
        .map_err(|e| unreachable_error(url, e))?;
    Ok(Reply {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

fn post(url: &str, content_type: &str, body: String, headers: &[(&str, String)]) -> Result<Reply, OxaError> {
    crate::offline::check(url)?;
    let agent = http_agent();
    let mut req = agent
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", content_type);
    for (name, value) in headers {
        req = req.header(*name, value.as_str());
    }
    let mut resp = req.send(body.as_bytes()).map_err(|e| unreachable_error(url, e))?;
    let status = resp.status().as_u16();
    check_offline_redirect(url, status)?;
    let bytes = resp
        .body_mut()
        .read_to_vec()
        .map_err(|e| unreachable_error(url, e))?;
    Ok(Reply {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// 404 and 4xx-like answers mean "not here": discovery tries the next candidate.
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() > 200 {
        let head: String = trimmed.chars().take(200).collect();
        format!("{head}...")
    } else {
        trimmed.to_string()
    }
}

fn parse_object(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(obj)) => Some(obj),
        _ => None,
    }
}

fn field_str(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// RFC 9728: the first authorization server named by the MCP server's protected resource
/// metadata. `Ok(None)` when the metadata is missing or names none. The metadata's `resource`
/// must be the MCP server URL (RFC 9728 section 3.3): metadata for another resource is an error,
/// never a silent fallback.
fn authorization_server_from_resource(
    mcp_url: &str,
    server: &Url,
    tried: &mut Vec<String>,
) -> Result<Option<String>, OxaError> {
    let root = origin(server);
    let mut urls = vec![format!(
        "{root}/.well-known/oauth-protected-resource{}",
        path_suffix(&server.path)
    )];
    if server.path != "/" {
        urls.push(format!("{root}/.well-known/oauth-protected-resource"));
    }
    for url in urls {
        tried.push(url.clone());
        let Ok(reply) = get(
            &url,
            &[
                ("Accept", "application/json"),
                ("MCP-Protocol-Version", PROTOCOL_VERSION),
            ],
        ) else {
            return Ok(None);
        };
        if is_discovery_miss(reply.status) {
            continue;
        }
        if !(200..300).contains(&reply.status) {
            return Ok(None);
        }
        let Some(obj) = parse_object(&reply.body) else {
            return Ok(None);
        };
        if let Some(resource) = obj.get("resource").and_then(Value::as_str) {
            let expected = mcp_url.split('#').next().unwrap_or(mcp_url);
            if trim_slash(resource) != trim_slash(expected) {
                return Err(mcp_error(format!(
                    "the protected resource metadata at {url} is for {resource}, not for the MCP server {expected}"
                )));
            }
        }
        return Ok(obj
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|servers| servers.first())
            .and_then(Value::as_str)
            .map(str::to_string));
    }
    Ok(None)
}

/// RFC 8414 / OIDC discovery of one issuer. `Ok(None)` when no candidate exists.
fn discover_as(as_url: &str, tried: &mut Vec<String>) -> Result<Option<AuthServerMeta>, OxaError> {
    let url =
        parse_url(as_url).ok_or_else(|| mcp_error(format!("invalid authorization server URL {as_url}")))?;
    let root = origin(&url);
    let suffix = path_suffix(&url.path).to_string();
    let mut urls = vec![
        format!("{root}/.well-known/oauth-authorization-server{suffix}"),
        format!("{root}/.well-known/openid-configuration{suffix}"),
    ];
    if !suffix.is_empty() {
        urls.push(format!("{root}{suffix}/.well-known/openid-configuration"));
    }
    for candidate in urls {
        tried.push(candidate.clone());
        let reply = get(
            &candidate,
            &[
                ("Accept", "application/json"),
                ("MCP-Protocol-Version", PROTOCOL_VERSION),
            ],
        )?;
        if is_discovery_miss(reply.status) {
            continue;
        }
        if !(200..300).contains(&reply.status) {
            return Err(mcp_error(format!(
                "HTTP {} loading authorization server metadata from {candidate}",
                reply.status
            )));
        }
        let meta = parse_as_metadata(&reply.body)?;
        if trim_slash(&meta.issuer) != trim_slash(as_url) {
            return Err(mcp_error(format!(
                "authorization server metadata names issuer {} but the server is {as_url}",
                meta.issuer
            )));
        }
        return Ok(Some(meta));
    }
    Ok(None)
}

fn parse_as_metadata(body: &str) -> Result<AuthServerMeta, OxaError> {
    let obj =
        parse_object(body).ok_or_else(|| mcp_error("authorization server metadata is not a JSON object"))?;
    let issuer =
        field_str(&obj, "issuer").ok_or_else(|| mcp_error("authorization server metadata has no issuer"))?;
    let authorization_endpoint = field_str(&obj, "authorization_endpoint")
        .ok_or_else(|| mcp_error("authorization server metadata has no authorization_endpoint"))?;
    let token_endpoint = field_str(&obj, "token_endpoint")
        .ok_or_else(|| mcp_error("authorization server metadata has no token_endpoint"))?;
    if let Some(methods) = obj.get("response_types_supported").and_then(Value::as_array) {
        if !methods.iter().any(|m| m.as_str() == Some("code")) {
            return Err(mcp_error(
                "the authorization server does not support authorization codes",
            ));
        }
    }
    if let Some(methods) = obj
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
    {
        if !methods.iter().any(|m| m.as_str() == Some("S256")) {
            return Err(mcp_error("the authorization server does not support PKCE S256"));
        }
    }
    let scopes_supported = obj
        .get("scopes_supported")
        .and_then(Value::as_array)
        .map(|scopes| {
            scopes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(AuthServerMeta {
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: field_str(&obj, "registration_endpoint"),
        scopes_supported,
        iss_parameter_supported: obj
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Client secrets and other OAuth values may be `${VAR}` or `!command`, as elsewhere in `mcp.json`.
fn expand_secret(raw: Option<&str>) -> Result<Option<String>, OxaError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let lookup = |key: &str| std::env::var(key).ok();
    let run = |command: &str| run_value_command(command);
    let value = crate::expand_value(raw, &lookup, &run)?;
    Ok(Some(value).filter(|v| !v.is_empty()))
}

fn run_value_command(command: &str) -> Result<String, OxaError> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| OxaError::io("cannot run a !command value", &e))?;
    if !out.status.success() {
        return Err(OxaError::new(
            codes::INVALID,
            format!("the !command value `{command}` failed"),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The port a sign-in must listen on, when the configuration fixes one.
fn fixed_callback_port(settings: &OAuthSettings) -> Result<Option<u16>, OxaError> {
    if let Some(url) = &settings.callback_url {
        let parsed = parse_url(url).ok_or_else(|| {
            OxaError::new(
                codes::INVALID,
                "oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1]",
            )
        })?;
        return Ok(Some(parsed.port.unwrap_or(80)));
    }
    Ok(settings.callback_port)
}

/// RFC 7591 dynamic registration: `(client_id, client_secret)`.
fn register_client(
    meta: &AuthServerMeta,
    redirect: &str,
    settings: &OAuthSettings,
) -> Result<(String, Option<String>), OxaError> {
    let endpoint = meta.registration_endpoint.clone().ok_or_else(|| {
        mcp_error("the authorization server does not offer dynamic client registration")
            .with_hint("set oauth.clientId in mcp.json")
    })?;
    secure_endpoint(&endpoint)?;
    let native = parse_url(redirect).is_some_and(|u| loopback_host(&u.host));
    let application_type = if native { "native" } else { "web" };
    let mut body = json!({
        "client_name": settings.client_name.clone().unwrap_or_else(|| "oxa".to_string()),
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": application_type,
    });
    if let Some(scope) = &settings.scope {
        body["scope"] = json!(scope);
    }
    let reply = post(&endpoint, "application/json", body.to_string(), &[])?;
    if !(200..300).contains(&reply.status) {
        return Err(mcp_error(format!(
            "client registration failed: HTTP {}: {}",
            reply.status,
            excerpt(&reply.body)
        )));
    }
    let obj =
        parse_object(&reply.body).ok_or_else(|| mcp_error("the registration answer is not a JSON object"))?;
    let client_id =
        field_str(&obj, "client_id").ok_or_else(|| mcp_error("the registration answer has no client_id"))?;
    let client_secret = field_str(&obj, "client_secret");
    Ok((client_id, client_secret))
}

/// The answer of a token request: tokens, or an OAuth error (`invalid_grant`, ...).
enum TokenReply {
    Granted(Map<String, Value>),
    Refused { error: String, description: String },
}

/// POST to a token endpoint. The client authenticates with HTTP Basic when it has a secret,
/// else with `client_id` in the form. `resource` is sent as RFC 8707 `resource`.
fn token_request(
    endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    mut form: Vec<(&str, String)>,
    resource: &str,
) -> Result<TokenReply, OxaError> {
    secure_endpoint(endpoint)?;
    if !resource.is_empty() {
        form.push(("resource", resource.to_string()));
    }
    let mut headers: Vec<(&str, String)> = Vec::new();
    match client_secret {
        Some(secret) => {
            let credentials = format!("{}:{}", encode_component(client_id), encode_component(secret));
            headers.push(("Authorization", format!("Basic {}", STANDARD.encode(credentials))));
        }
        None => form.push(("client_id", client_id.to_string())),
    }
    let reply = post(
        endpoint,
        "application/x-www-form-urlencoded",
        encode_pairs(&form),
        &headers,
    )?;
    // Servers may report OAuth errors with any status, so the body is checked first.
    let parsed = parse_object(&reply.body);
    if let Some(obj) = &parsed {
        if let Some(error) = field_str(obj, "error") {
            let description = field_str(obj, "error_description").unwrap_or_else(|| error.clone());
            return Ok(TokenReply::Refused { error, description });
        }
    }
    if !(200..300).contains(&reply.status) {
        return Ok(TokenReply::Refused {
            error: "server_error".to_string(),
            description: format!("HTTP {}: {}", reply.status, excerpt(&reply.body)),
        });
    }
    match parsed {
        Some(obj) => Ok(TokenReply::Granted(obj)),
        None => Err(mcp_error(
            "the token endpoint answered with something that is not JSON",
        )),
    }
}

/// Store the access token (and a new refresh token or scope, when the grant has them).
/// Returns the access token.
fn apply_grant(auth: &mut StoredAuth, obj: &Map<String, Value>) -> Result<String, OxaError> {
    let access =
        field_str(obj, "access_token").ok_or_else(|| mcp_error("the token response has no access_token"))?;
    auth.access_token = Some(access.clone());
    auth.expires_at = obj
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| now_s().saturating_add(seconds));
    if let Some(refresh) = field_str(obj, "refresh_token") {
        auth.refresh_token = Some(refresh);
    }
    if let Some(scope) = field_str(obj, "scope") {
        auth.scope = Some(scope);
    }
    Ok(access)
}
