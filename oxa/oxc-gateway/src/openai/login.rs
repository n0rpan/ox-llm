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
    Storage,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Setup => "ChatGPT sign-in unavailable; loopback port must be free",
            Self::Callback => "Invalid ChatGPT sign-in callback",
            Self::Expired => "ChatGPT sign-in timed out",
            Self::Cancelled => "ChatGPT sign-in cancelled",
            Self::Exchange => "ChatGPT sign-in exchange failed",
            Self::Storage => "ChatGPT credential store refused the operation",
        })
    }
}
impl std::error::Error for Error {}

/// Contains state/verifier and callback bytes; intentionally no Debug/Clone.
pub struct Login {
    listener: TcpListener,
    pending: Option<(TcpStream, Vec<u8>)>,
    state: String,
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
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455)).map_err(|_| Error::Setup)?;
        let mut random = [0; 96];
        getrandom::fill(&mut random).map_err(|_| Error::Setup)?;
        Self::start(listener, ISSUER.into(), REDIRECT.into(), &random, DEADLINE)
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
            ("client_id", CLIENT_ID),
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
    pub fn finish(
        &mut self,
        code: Code,
        path: &Path,
        replace: bool,
        expected: &[&str],
        cancelled: &AtomicBool,
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
            if chatgpt_tokens::try_install(path, &record, replace, expected)
                .map_err(|_| Error::Storage)?
                .is_some()
            {
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
                ("client_id", CLIENT_ID),
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
