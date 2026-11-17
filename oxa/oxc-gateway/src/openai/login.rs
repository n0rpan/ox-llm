//! Explicit owner sign-in only. Fixed issuer/public client/redirect in production;
//! test-only peers inject those values without reading any existing credentials.
use super::chatgpt_tokens::{self, TokenRecord};
use futures_util::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// Public experimental identity authorized in #252; never a client secret.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const REDIRECT: &str = "http://localhost:1455/auth/callback";
const MAX_BYTES: usize = 64 * 1024;
const MAX_CALLBACK: usize = 8 * 1024;
const DEADLINE: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Setup,
    Callback,
    Expired,
    Cancelled,
    Exchange,
    Storage(chatgpt_tokens::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Setup => "ChatGPT sign-in unavailable; loopback port must be free",
            Self::Callback => "Invalid ChatGPT sign-in callback",
            Self::Expired => "ChatGPT sign-in timed out",
            Self::Cancelled => "ChatGPT sign-in cancelled",
            Self::Exchange => "ChatGPT sign-in exchange failed",
            Self::Storage(error) => return write!(f, "ChatGPT credential store operation failed: {error}"),
        })
    }
}
impl std::error::Error for Error {}

/// Contains state/verifier and callback bytes; intentionally no Debug/Clone.
pub struct Login {
    listener: TcpListener,
    pending: Option<(TcpStream, Vec<u8>)>,
    state: String,
    client_id: String,
    verifier: String,
    issuer: String,
    redirect: String,
    deadline: Instant,
    exchanged: bool,
}
/// An authorization code is usable only by its originating Login; never Debug.
pub struct Code {
    value: String,
    state: String,
}
impl Login {
    /// Bind BEFORE presenting the authorization URL. No browser is launched.
    pub fn new() -> Result<Self, Error> {
        Self::with_client_id(CLIENT_ID)
    }
    /// Same public-identity override contract as the production refresh factory.
    pub fn with_client_id(client_id: &str) -> Result<Self, Error> {
        if client_id.is_empty()
            || client_id.len() > 128
            || !client_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(Error::Setup);
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455)).map_err(|_| Error::Setup)?;
        let mut random = [0; 96];
        getrandom::fill(&mut random).map_err(|_| Error::Setup)?;
        Self::start(listener, ISSUER.into(), REDIRECT.into(), &random, DEADLINE).map(|mut flow| {
            flow.client_id = client_id.into();
            flow
        })
    }
    fn start(
        listener: TcpListener,
        issuer: String,
        redirect: String,
        random: &[u8; 96],
        timeout: Duration,
    ) -> Result<Self, Error> {
        listener.set_nonblocking(true).map_err(|_| Error::Setup)?;
        Ok(Self {
            listener,
            pending: None,
            verifier: base64(&random[..64]),
            state: base64(&random[64..]),
            client_id: CLIENT_ID.into(),
            issuer,
            redirect,
            deadline: Instant::now().checked_add(timeout).ok_or(Error::Setup)?,
            exchanged: false,
        })
    }
    pub fn authorization_url(&self) -> String {
        let mut url = url::Url::parse(&format!("{}/oauth/authorize", self.issuer)).expect("fixed issuer");
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", &self.redirect),
            ("scope", "openid profile email offline_access"),
            (
                "code_challenge",
                &base64(&Sha256::digest(self.verifier.as_bytes())),
            ),
            ("code_challenge_method", "S256"),
            ("state", &self.state),
            ("id_token_add_organizations", "true"),
            ("originator", "oxc"),
        ]);
        url.into()
    }
    pub fn check(&self, cancelled: &AtomicBool) -> Result<(), Error> {
        check(self.deadline, cancelled)
    }
    /// Owner-pasted URL uses exactly the same redirect and query checks. The CLI
    /// reads it without echo; never accept a bare code or a different authority.
    pub fn pasted_callback(&self, value: &str, cancelled: &AtomicBool) -> Result<Code, Error> {
        self.check(cancelled)?;
        self.code(value.trim())
    }
    fn code(&self, value: &str) -> Result<Code, Error> {
        if value.len() > MAX_CALLBACK
            || !value.starts_with(&format!("{}?", self.redirect))
            || value.contains('#')
        {
            return Err(Error::Callback);
        }
        let url = url::Url::parse(value).map_err(|_| Error::Callback)?;
        let mut pairs = BTreeMap::new();
        for (key, value) in url.query_pairs() {
            if pairs.insert(key.into_owned(), value.into_owned()).is_some() {
                return Err(Error::Callback);
            }
        }
        if pairs.contains_key("error") || pairs.get("state") != Some(&self.state) {
            return Err(Error::Callback);
        }
        let value = pairs
            .remove("code")
            .filter(|s| !s.is_empty())
            .ok_or(Error::Callback)?;
        Ok(Code {
            value,
            state: self.state.clone(),
        })
    }
    /// Nonblocking bounded HTTP callback. No callback, query, header or code is
    /// ever logged. Invalid traffic refuses this flow without any token request.
    pub fn poll_callback(&mut self, cancelled: &AtomicBool) -> Result<Option<Code>, Error> {
        self.check(cancelled)?;
        if self.pending.is_none() {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        return Err(Error::Callback);
                    }
                    stream.set_nonblocking(true).map_err(|_| Error::Callback)?;
                    self.pending = Some((stream, Vec::new()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(_) => return Err(Error::Callback),
            }
        }
        let (stream, bytes) = self.pending.as_mut().ok_or(Error::Callback)?;
        let mut data = [0; 1024];
        loop {
            match stream.read(&mut data) {
                Ok(0) => return Err(Error::Callback),
                Ok(n) => {
                    bytes.extend_from_slice(&data[..n]);
                    if bytes.len() > MAX_CALLBACK {
                        return Err(Error::Callback);
                    }
                    if bytes.windows(4).any(|p| p == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(_) => return Err(Error::Callback),
            }
        }
        let (_, bytes) = self.pending.as_ref().ok_or(Error::Callback)?;
        let result = self.request_code(bytes);
        if let Some((mut stream, _)) = self.pending.take() {
            let reply: &[u8] = if result.is_ok() {
                b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\nConnection: close\r\nContent-Type: text/plain\r\n\r\nCallback received. Return to oxa."
            } else {
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = stream.write_all(reply);
        }
        result.map(Some)
    }
    fn request_code(&self, bytes: &[u8]) -> Result<Code, Error> {
        let text = std::str::from_utf8(bytes).map_err(|_| Error::Callback)?;
        let (head, body) = text.split_once("\r\n\r\n").ok_or(Error::Callback)?;
        if !body.is_empty() {
            return Err(Error::Callback);
        }
        let mut lines = head.split("\r\n");
        let request: Vec<_> = lines.next().ok_or(Error::Callback)?.split(' ').collect();
        let ["GET", target, "HTTP/1.1"] = request.as_slice() else {
            return Err(Error::Callback);
        };
        if !target.starts_with("/auth/callback?") {
            return Err(Error::Callback);
        }
        let mut headers = BTreeMap::new();
        for line in lines {
            let (name, value) = line.split_once(':').ok_or(Error::Callback)?;
            if name.is_empty()
                || name.bytes().any(|b| !b.is_ascii_alphanumeric() && b != b'-')
                || headers.insert(name.to_ascii_lowercase(), value.trim()).is_some()
            {
                return Err(Error::Callback);
            }
        }
        let redirect = url::Url::parse(&self.redirect).map_err(|_| Error::Setup)?;
        let authority = format!("localhost:{}", redirect.port().ok_or(Error::Setup)?);
        if headers.get("host") != Some(&authority.as_str())
            || headers.contains_key("transfer-encoding")
            || headers.get("content-length").is_some_and(|v| *v != "0")
        {
            return Err(Error::Callback);
        }
        self.code(&format!("http://{authority}{target}"))
    }
    /// Exactly one exchange, then publish through the existing token module.
    /// Whole-operation deadline includes headers/body and refresh-lock waiting.
    /// Any publication error is terminal, even if rename already made the record
    /// visible and its directory sync failed. Never exchange the code again.
    pub fn finish(
        &mut self,
        code: Code,
        path: &Path,
        replace: bool,
        expected: &[&str],
        cancelled: &AtomicBool,
    ) -> Result<(), Error> {
        self.finish_install(code, cancelled, |record| {
            chatgpt_tokens::try_install(path, record, replace, expected)
        })
    }
    fn finish_install(
        &mut self,
        code: Code,
        cancelled: &AtomicBool,
        mut install: impl FnMut(&TokenRecord) -> Result<Option<()>, chatgpt_tokens::Error>,
    ) -> Result<(), Error> {
        self.check(cancelled)?;
        if self.exchanged || code.state != self.state {
            return Err(Error::Callback);
        }
        self.exchanged = true;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Setup)?;
        let record = runtime.block_on(self.exchange(code, cancelled))?;
        loop {
            self.check(cancelled)?;
            // Retry only nonblocking lock contention (None). Any store error,
            // including uncertain durability AFTER publication, is terminal.
            if install(&record).map_err(Error::Storage)?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    async fn exchange(&self, code: Code, cancelled: &AtomicBool) -> Result<TokenRecord, Error> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| Error::Setup)?;
        let operation = async {
            let form = [
                ("grant_type", "authorization_code"),
                ("code", &code.value),
                ("redirect_uri", &self.redirect),
                ("client_id", self.client_id.as_str()),
                ("code_verifier", &self.verifier),
            ];
            let response = client
                .post(format!("{}/oauth/token", self.issuer))
                .form(&form)
                .send()
                .await
                .map_err(|_| Error::Exchange)?;
            if !response.status().is_success()
                || response.content_length().is_some_and(|n| n > MAX_BYTES as u64)
                || !response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|h| h.to_str().ok())
                    .is_some_and(|v| {
                        v.split(';')
                            .next()
                            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
                    })
            {
                return Err(Error::Exchange);
            }
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(next) = stream.next().await {
                let data = next.map_err(|_| Error::Exchange)?;
                if data.len() > MAX_BYTES - bytes.len() {
                    return Err(Error::Exchange);
                }
                bytes.extend_from_slice(&data);
            }
            let reply: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Exchange)?;
            if reply.get("error").is_some() {
                return Err(Error::Exchange);
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| Error::Exchange)?
                .as_secs_f64();
            chatgpt_tokens::login_record(&reply, now).map_err(|_| Error::Exchange)
        };
        tokio::pin!(operation);
        loop {
            self.check(cancelled)?;
            tokio::select! {
                result = &mut operation => { self.check(cancelled)?; return result; }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }
}
fn check(deadline: Instant, cancelled: &AtomicBool) -> Result<(), Error> {
    if cancelled.load(Ordering::SeqCst) {
        return Err(Error::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Error::Expired);
    }
    Ok(())
}
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let mut value = u32::from(chunk[0]) << 16;
        if chunk.len() > 1 {
            value |= u32::from(chunk[1]) << 8;
        }
        if chunk.len() > 2 {
            value |= u32::from(chunk[2]);
        }
        for shift in (0..4).rev().take(chunk.len() + 1) {
            result.push(ALPHABET[((value >> (shift * 6)) & 63) as usize] as char);
        }
    }
    result
}

#[cfg(test)]
use crate::mock_openai as mock;
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::PathBuf,
        sync::{atomic::AtomicU64, Arc},
    };
    struct Home(PathBuf);
    impl Home {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "native-login-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("chatgpt-auth.json")
        }
        fn old(&self) -> Vec<u8> {
            let value = json!({"access_token":"synthetic-old-access","refresh_token":"synthetic-old-refresh","account_id":"synthetic-account","expires_at":2_000_000_000});
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.path())
                .unwrap();
            f.write_all(value.to_string().as_bytes()).unwrap();
            fs::read(self.path()).unwrap()
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn flow(issuer: &str, timeout: Duration) -> Login {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let redirect = format!(
            "http://localhost:{}/auth/callback",
            listener.local_addr().unwrap().port()
        );
        Login::start(listener, issuer.into(), redirect, &[7; 96], timeout).unwrap()
    }
    fn callback(flow: &Login) -> String {
        format!(
            "{}?state={}&code=synthetic-authorization-code",
            flow.redirect, flow.state
        )
    }
    fn reply() -> Value {
        let claims = json!({"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-account"}});
        json!({"access_token":"synthetic-new-access","refresh_token":"synthetic-new-refresh","id_token":format!("synthetic.{}.unsigned",base64(claims.to_string().as_bytes())),"expires_in":3600})
    }
    #[test]
    fn pkce_vector_randomness_and_public_fixed_contract() {
        assert_eq!(base64(b"f"), "Zg");
        assert_eq!(base64(b"fo"), "Zm8");
        assert_eq!(base64(b"foo"), "Zm9v");
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let first = Login::new().unwrap();
        let url = url::Url::parse(&first.authorization_url()).unwrap();
        let params: BTreeMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(url.origin().ascii_serialization(), ISSUER);
        assert_eq!(params["redirect_uri"], REDIRECT);
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(
            params["code_challenge"],
            base64(&Sha256::digest(first.verifier.as_bytes()))
        );
        let state = first.state.clone();
        let verifier = first.verifier.clone();
        assert!(
            matches!(Login::new(), Err(Error::Setup)),
            "occupied fixed port cannot choose another redirect"
        );
        drop(first);
        // Other unit controls spawn child processes concurrently. A fork can
        // briefly retain the first CLOEXEC listener until exec; keep the second
        // entropy check on an ephemeral test listener rather than racing a
        // second bind of the global production port.
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut random = [0; 96];
        getrandom::fill(&mut random).unwrap();
        let second = Login::start(listener, ISSUER.into(), REDIRECT.into(), &random, DEADLINE).unwrap();
        assert_ne!(state, second.state);
        assert_ne!(verifier, second.verifier);
        for id in ["", "bad id", "bad/id", "λ", &"x".repeat(129)] {
            assert!(matches!(Login::with_client_id(id), Err(Error::Setup)));
        }
    }
    #[test]
    fn complete_fake_pkce_loopback_and_paste_publish_one_existing_record() {
        for paste in [false, true] {
            let home = Home::new();
            let peer = mock::MockOpenAi::start(vec![mock::Reply::json(reply())]);
            let mut flow = flow(&peer.base_url(), Duration::from_secs(2));
            let stop = AtomicBool::new(false);
            let code = if paste {
                flow.pasted_callback(&callback(&flow), &stop).unwrap()
            } else {
                let addr = flow.listener.local_addr().unwrap();
                let mut socket = TcpStream::connect(addr).unwrap();
                let request=format!("GET /auth/callback?state={}&code=synthetic-authorization-code HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",flow.state,addr.port());
                let (first, last) = request.split_at(request.len() / 2);
                socket.write_all(first.as_bytes()).unwrap();
                assert!(flow.poll_callback(&stop).unwrap().is_none());
                socket.write_all(last.as_bytes()).unwrap();
                let code = loop {
                    if let Some(code) = flow.poll_callback(&stop).unwrap() {
                        break code;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                };
                let mut answer = String::new();
                socket.read_to_string(&mut answer).unwrap();
                assert!(answer.ends_with("Callback received. Return to oxa."));
                assert!(!answer.contains("synthetic-authorization-code"));
                code
            };
            flow.finish(code, &home.path(), false, &["synthetic-account"], &stop)
                .unwrap();
            let record = chatgpt_tokens::read(&home.path()).unwrap();
            assert_eq!(record.access_token(), "synthetic-new-access");
            assert_eq!(record.account_id(), "synthetic-account");
            assert_eq!(
                fs::metadata(home.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let raw = fs::read_to_string(home.path()).unwrap();
            assert!(!raw.contains("id_token"));
            let requests = peer.wait_for_requests(1);
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].body["code_verifier"], flow.verifier);
            assert_eq!(requests[0].body["grant_type"], "authorization_code");
            assert_eq!(requests[0].body["redirect_uri"], flow.redirect);
            assert_eq!(requests[0].body["client_id"], CLIENT_ID);
            let code = flow.pasted_callback(&callback(&flow), &stop).unwrap();
            assert_eq!(
                flow.finish(code, &home.path(), true, &[], &stop),
                Err(Error::Callback)
            );
        }
    }
    #[test]
    fn callbacks_reject_wrong_state_duplicates_paths_hosts_and_provider_errors_without_exchange() {
        let flow = flow("http://127.0.0.1:9", Duration::from_secs(1));
        let stop = AtomicBool::new(false);
        for bad in [
            format!("{}?code=x", flow.redirect),
            format!("{}?state=wrong&code=x", flow.redirect),
            format!(
                "{}?state={}&state={}&code=x",
                flow.redirect, flow.state, flow.state
            ),
            format!("{}?state={}&code=x&code=y", flow.redirect, flow.state),
            format!(
                "{}?state={}&code=x&error=synthetic-error",
                flow.redirect, flow.state
            ),
            format!("{}?state={}&code=x#fragment", flow.redirect, flow.state),
            callback(&flow).replace("localhost", "127.0.0.1"),
            callback(&flow).replace("/auth/callback", "/other/../auth/callback"),
        ] {
            assert!(matches!(flow.pasted_callback(&bad, &stop), Err(Error::Callback)));
        }
        let port = flow.listener.local_addr().unwrap().port();
        for (method, path, host) in [
            ("POST", "/auth/callback", "localhost"),
            ("GET", "/other", "localhost"),
            ("GET", "/auth/callback", "example.invalid"),
        ] {
            let request = format!(
                "{method} {path}?state={}&code=x HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n",
                flow.state
            );
            assert!(matches!(
                flow.request_code(request.as_bytes()),
                Err(Error::Callback)
            ));
        }
        let duplicate=format!("GET /auth/callback?state={}&code=x HTTP/1.1\r\nHost: localhost:{port}\r\nHost: localhost:{port}\r\n\r\n",flow.state);
        assert!(matches!(
            flow.request_code(duplicate.as_bytes()),
            Err(Error::Callback)
        ));
        stop.store(true, Ordering::SeqCst);
        assert!(matches!(
            flow.pasted_callback(&callback(&flow), &stop),
            Err(Error::Cancelled)
        ));
    }
    #[test]
    fn bounded_exchange_errors_and_account_refusal_preserve_previous_bytes() {
        let oversized = vec![b'x'; MAX_BYTES + 1];
        for response in [
            mock::Reply::raw(
                302,
                "application/json",
                b"{}".to_vec(),
                vec![("Location", "http://127.0.0.1:9/unreachable".into())],
            ),
            mock::Reply::raw(200, "application/json", b"not json".to_vec(), vec![]),
            mock::Reply::raw(200, "application/json", oversized, vec![]),
            mock::Reply::json(
                json!({"error":"synthetic-new-access","refresh_token":"synthetic-new-refresh"}),
            ),
            mock::Reply::json(
                json!({"access_token":"synthetic-new-access","refresh_token":"synthetic-new-refresh","expires_in":3600}),
            ),
            mock::Reply::raw(401, "application/json", reply().to_string().into_bytes(), vec![]),
        ] {
            let home = Home::new();
            let previous = home.old();
            let peer = mock::MockOpenAi::start(vec![response]);
            let mut flow = flow(&peer.base_url(), Duration::from_secs(2));
            let stop = AtomicBool::new(false);
            let code = flow.pasted_callback(&callback(&flow), &stop).unwrap();
            let error = flow.finish(code, &home.path(), true, &[], &stop).unwrap_err();
            assert_eq!(error, Error::Exchange);
            for value in [
                "synthetic-new-access",
                "synthetic-new-refresh",
                "synthetic-authorization-code",
            ] {
                assert!(!format!("{error:?} {error}").contains(value));
            }
            assert_eq!(fs::read(home.path()).unwrap(), previous);
            assert_eq!(peer.requests().len(), 1);
        }
        let home = Home::new();
        let previous = home.old();
        let peer = mock::MockOpenAi::start(vec![mock::Reply::json(reply())]);
        let mut flow = flow(&peer.base_url(), Duration::from_secs(2));
        let stop = AtomicBool::new(false);
        let code = flow.pasted_callback(&callback(&flow), &stop).unwrap();
        assert_eq!(
            flow.finish(code, &home.path(), true, &["other-synthetic-account"], &stop),
            Err(Error::Storage(chatgpt_tokens::Error::Account))
        );
        assert_eq!(fs::read(home.path()).unwrap(), previous);
    }
    #[test]
    fn held_exchange_cancel_deadline_and_store_lock_deadline_never_publish() {
        for cancel in [false, true] {
            let home = Home::new();
            let previous = home.old();
            let (response, gate) = mock::Reply::json(reply()).gated();
            let peer = mock::MockOpenAi::start(vec![response]);
            let mut flow = flow(&peer.base_url(), Duration::from_millis(150));
            let stop = Arc::new(AtomicBool::new(false));
            let code = flow.pasted_callback(&callback(&flow), &stop).unwrap();
            let other = stop.clone();
            let worker = std::thread::spawn(move || {
                if cancel {
                    std::thread::sleep(Duration::from_millis(30));
                    other.store(true, Ordering::SeqCst);
                }
            });
            let start = Instant::now();
            let error = flow.finish(code, &home.path(), true, &[], &stop).unwrap_err();
            worker.join().unwrap();
            assert_eq!(error, if cancel { Error::Cancelled } else { Error::Expired });
            assert!(start.elapsed() < Duration::from_secs(1));
            assert_eq!(fs::read(home.path()).unwrap(), previous);
            drop(gate);
        }
        let home = Home::new();
        let previous = home.old();
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(home.0.join("chatgpt-auth.lock"))
            .unwrap();
        let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).unwrap();
        let peer = mock::MockOpenAi::start(vec![mock::Reply::json(reply())]);
        let mut flow = flow(&peer.base_url(), Duration::from_millis(150));
        let stop = AtomicBool::new(false);
        let code = flow.pasted_callback(&callback(&flow), &stop).unwrap();
        assert_eq!(
            flow.finish(code, &home.path(), true, &[], &stop),
            Err(Error::Expired)
        );
        assert_eq!(fs::read(home.path()).unwrap(), previous);
        drop(lock);
    }
    #[test]
    fn publication_boundary_errors_are_fixed_and_never_repeat_the_exchange() {
        use std::os::unix::fs::MetadataExt;
        for replace in [false, true] {
            for after_rename in [false, true] {
                let home = Home::new();
                let previous = replace.then(|| home.old());
                let peer = mock::MockOpenAi::start(vec![mock::Reply::json(reply())]);
                let mut login = flow(&peer.base_url(), Duration::from_secs(2));
                let stop = AtomicBool::new(false);
                let code = login.pasted_callback(&callback(&login), &stop).unwrap();
                let mut installs = 0;
                let mut syncs = 0;
                let mut expected_record = Value::Null;
                let error = login
                    .finish_install(code, &stop, |record| {
                        installs += 1;
                        expected_record = json!({"access_token":record.access_token(),"refresh_token":record.refresh_token(),
                            "account_id":record.account_id(),"expires_at":record.expires_at()});
                        chatgpt_tokens::try_install_sync(
                            &home.path(),
                            record,
                            replace,
                            &["synthetic-account"],
                            |file| {
                                syncs += 1;
                                if after_rename && syncs == 1 {
                                    file.sync_all()
                                } else {
                                    Err(std::io::Error::other("synthetic private fault detail"))
                                }
                            },
                        )
                    })
                    .unwrap_err();
                assert_eq!(installs, 1);
                assert_eq!(syncs, if after_rename { 2 } else { 1 });
                assert_eq!(error, Error::Storage(chatgpt_tokens::Error::Unavailable));
                assert_eq!(
                    error.to_string(),
                    "ChatGPT credential store operation failed: token file unavailable"
                );
                assert_eq!(format!("{error:?}"), "Storage(Unavailable)");
                if after_rename {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&fs::read(home.path()).unwrap()).unwrap(),
                        expected_record
                    );
                    let record = chatgpt_tokens::read(&home.path()).unwrap();
                    assert_eq!(record.access_token(), "synthetic-new-access");
                    assert_eq!(record.refresh_token(), "synthetic-new-refresh");
                    assert_eq!(record.account_id(), "synthetic-account");
                    let meta = fs::metadata(home.path()).unwrap();
                    assert_eq!(meta.mode() & 0o777, 0o600);
                    assert_eq!(meta.nlink(), 1);
                    if let Some(previous) = &previous {
                        assert_ne!(fs::read(home.path()).unwrap(), *previous);
                    }
                } else if let Some(previous) = previous {
                    assert_eq!(fs::read(home.path()).unwrap(), previous);
                } else {
                    assert!(!home.path().exists());
                }
                assert!(fs::read_dir(&home.0).unwrap().all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")));
                let lock = home.0.join("chatgpt-auth.lock");
                let inode = fs::metadata(&lock).unwrap().ino();
                let state_after_error = fs::read(home.path()).ok();
                // The same flow refuses BEFORE both transport and installation,
                // even when the first error happened after committed rename.
                let code = login.pasted_callback(&callback(&login), &stop).unwrap();
                assert_eq!(
                    login.finish(code, &home.path(), replace, &[], &stop),
                    Err(Error::Callback)
                );
                assert_eq!(peer.wait_for_requests(1).len(), 1);
                assert_eq!(peer.requests().len(), 1);
                assert_eq!(fs::read(home.path()).ok(), state_after_error);
                assert_eq!(fs::metadata(lock).unwrap().ino(), inode);
            }
        }
    }
}
