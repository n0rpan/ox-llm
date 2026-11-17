//! The Brave Search provider: the only connect site of `oxa-web` (reviewed in
//! `ci/check_net_surface.py`: user-configured key only, off when offline).
//!
//! Primary endpoint `GET /res/v1/llm/context` (LLM Context: `grounding.generic[]` +
//! `sources{}`); on HTTP 400 `OPTION_NOT_IN_PLAN` the process falls back to
//! `GET /res/v1/web/search` (`web.results[]`) for good: the decision is cached in
//! [`BraveShared`], LLM Context is never asked again, and [`FALLBACK_NOTICE`] is shown once.
//!
//! Limits: 30 s per request; 429 and 5xx are retried (at most 3 attempts) with bounded
//! exponential backoff, honouring `Retry-After` / `X-RateLimit-Reset`; requests are serialised
//! process-wide (the plan allows 50 per second, a parallel batch must not burst); every wait
//! and request is cancellable. No redirects are followed (the key header stays with the host).
use crate::key::ApiKey;
use crate::shape::{normalise, strip_html, Found};
use crate::tool::Query;
use oxa_proto::{tool_errors, CancelToken, Value};
use oxa_tools::descriptions::web_search as text;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// The real host.
pub const DEFAULT_BASE_URL: &str = "https://api.search.brave.com";
/// The one-time user notice of the fallback (and the status line).
pub const FALLBACK_NOTICE: &str = "Brave plan lacks LLM Context; using web search";
const LLM_PATH: &str = "/res/v1/llm/context";
const WEB_PATH: &str = "/res/v1/web/search";
/// `count` of the web search endpoint when the model gives none.
const WEB_DEFAULT_COUNT: u32 = 10;
/// The largest response body read.
const MAX_BODY: u64 = 8 * 1024 * 1024;
/// Longest provider error detail quoted in an error text.
const DETAIL_CHARS: usize = 200;

/// Which endpoint answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    LlmContext,
    WebSearch,
}

/// The provider's knobs. `Default` = the real host and the production limits.
#[derive(Debug, Clone)]
pub struct BraveOpts {
    /// Scheme + host (+ port), no path. Tests pass a loopback mock.
    pub base_url: String,
    /// Per request (connect + answer).
    pub timeout: Duration,
    /// The first backoff wait; doubled per attempt.
    pub backoff_base: Duration,
    /// The longest single wait (also caps `Retry-After`).
    pub max_wait: Duration,
    pub max_attempts: u32,
    /// LLM Context `maximum_number_of_tokens` (clamped to 1024..32768).
    pub max_tokens: u32,
    /// `OXA_TEST_ISOLATION=1`: only a loopback base URL is used.
    pub isolation: bool,
}

impl Default for BraveOpts {
    fn default() -> BraveOpts {
        BraveOpts {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: Duration::from_secs(30),
            backoff_base: Duration::from_secs(1),
            max_wait: Duration::from_secs(10),
            max_attempts: 3,
            max_tokens: crate::DEFAULT_MAX_TOKENS,
            isolation: false,
        }
    }
}

/// What outlives one provider instance (a new instance is built per epoch, when the key may
/// have changed): the fallback decision, the notice, the request lock and counters.
#[derive(Debug, Default)]
pub struct BraveShared {
    fallback: AtomicBool,
    notice_pending: AtomicBool,
    serial: Mutex<()>,
    requests: AtomicUsize,
}

impl BraveShared {
    pub fn new() -> Arc<BraveShared> {
        Arc::new(BraveShared::default())
    }
    /// The endpoint the next search uses.
    pub fn endpoint(&self) -> Endpoint {
        if self.fallback.load(Ordering::SeqCst) {
            Endpoint::WebSearch
        } else {
            Endpoint::LlmContext
        }
    }
    /// Requests sent by this process (all endpoints, all attempts).
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// Brave Search.
pub struct Brave {
    key: ApiKey,
    opts: BraveOpts,
    shared: Arc<BraveShared>,
    agent: ureq::Agent,
}

/// One answer, read in full.
struct Reply {
    status: u16,
    wait: Option<Duration>,
    body: Vec<u8>,
}

impl std::fmt::Debug for Brave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Brave")
            .field("key", &self.key)
            .field("base_url", &self.opts.base_url)
            .field("endpoint", &self.shared.endpoint())
            .finish()
    }
}

impl Brave {
    pub fn new(key: ApiKey, opts: BraveOpts) -> Brave {
        Brave::with_shared(key, opts, BraveShared::new())
    }
    pub fn with_shared(key: ApiKey, mut opts: BraveOpts, shared: Arc<BraveShared>) -> Brave {
        opts.max_tokens = opts
            .max_tokens
            .clamp(crate::MIN_MAX_TOKENS, crate::MAX_MAX_TOKENS);
        opts.base_url = opts.base_url.trim_end_matches('/').to_string();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(opts.timeout))
            .max_redirects(0)
            .build();
        Brave {
            key,
            opts,
            shared,
            agent: ureq::Agent::new_with_config(config),
        }
    }
    pub fn shared(&self) -> &Arc<BraveShared> {
        &self.shared
    }

    fn host_of_base(&self) -> String {
        oxa_proto::url_host(&self.opts.base_url).unwrap_or_default()
    }

    /// One GET, on a helper thread so that a cancel returns at once (the abandoned request
    /// ends by itself within the timeout).
    fn get(&self, endpoint: Endpoint, q: &Query, cancel: &CancelToken) -> Result<Reply, String> {
        let host = self.host_of_base();
        oxa_proto::check_connect(text::NAME, &host).map_err(|e| e.to_string())?;
        let url = format!(
            "{}{}",
            self.opts.base_url,
            match endpoint {
                Endpoint::LlmContext => LLM_PATH,
                Endpoint::WebSearch => WEB_PATH,
            }
        );
        let mut req = self
            .agent
            .get(url.as_str())
            .header("Accept", "application/json")
            .header("X-Subscription-Token", self.key.expose())
            .query("q", &q.q);
        match endpoint {
            Endpoint::LlmContext => {
                req = req.query("maximum_number_of_tokens", self.opts.max_tokens.to_string());
                if let Some(c) = q.count {
                    req = req.query("maximum_number_of_urls", c.to_string());
                }
            }
            Endpoint::WebSearch => {
                req = req.query("count", q.count.unwrap_or(WEB_DEFAULT_COUNT).to_string());
            }
        }
        for (k, v) in [
            ("country", &q.country),
            ("search_lang", &q.search_lang),
            ("freshness", &q.freshness),
        ] {
            if let Some(v) = v {
                req = req.query(k, v);
            }
        }
        self.shared.requests.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = req.call().and_then(|mut resp| {
                let status = resp.status().as_u16();
                let header = |name: &str| {
                    resp.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                let wait = header("retry-after")
                    .or_else(|| header("x-ratelimit-reset"))
                    .and_then(|v| first_seconds(&v));
                let body = resp.body_mut().with_config().limit(MAX_BODY).read_to_vec()?;
                Ok(Reply { status, wait, body })
            });
            let _ = tx.send(result);
        });
        loop {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(reply)) => return Ok(reply),
                Ok(Err(ureq::Error::Timeout(_))) => return Err(text::TIMEOUT.to_string()),
                Ok(Err(_)) => return Err(text::UNREACHABLE.to_string()),
                Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() => {
                    return Err(tool_errors::ABORTED.to_string())
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(text::UNREACHABLE.to_string()),
            }
        }
    }

    /// [`Brave::get`] with the retry rule. 401/403 = the key error; other statuses are returned.
    fn get_retrying(&self, endpoint: Endpoint, q: &Query, cancel: &CancelToken) -> Result<Reply, String> {
        let attempts = self.opts.max_attempts.max(1);
        for attempt in 1..=attempts {
            let reply = self.get(endpoint, q, cancel)?;
            let retryable = reply.status == 429 || (500..600).contains(&reply.status);
            if reply.status == 401 || reply.status == 403 {
                return Err(text::KEY_INVALID.to_string());
            }
            if !retryable {
                return Ok(reply);
            }
            if attempt == attempts {
                return Err(if reply.status == 429 {
                    text::RATE_LIMITED.to_string()
                } else {
                    text::SERVER_ERROR.replace("{status}", &reply.status.to_string())
                });
            }
            let backoff = retry_backoff(self.opts.backoff_base, attempt);
            let wait = reply.wait.unwrap_or(backoff).min(self.opts.max_wait);
            sleep_cancellable(wait, cancel)?;
        }
        Err(text::UNREACHABLE.to_string())
    }
}

fn retry_backoff(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(1_u32.checked_shl(attempt - 1).unwrap_or(u32::MAX))
}

/// The first whole number of seconds in a header value (`Retry-After: 2`,
/// `X-RateLimit-Reset: 1, 1419704`).
fn first_seconds(v: &str) -> Option<Duration> {
    let first = v.split(',').next()?.trim();
    first.parse::<u64>().ok().map(Duration::from_secs)
}

fn sleep_cancellable(d: Duration, cancel: &CancelToken) -> Result<(), String> {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if cancel.is_cancelled() {
            return Err(tool_errors::ABORTED.to_string());
        }
        std::thread::sleep((end - Instant::now()).min(Duration::from_millis(25)));
    }
    Ok(())
}

fn parse_json(body: &[u8]) -> Result<Value, String> {
    match serde_json::from_slice::<Value>(body) {
        Ok(v @ Value::Object(_)) => Ok(v),
        _ => Err(text::MALFORMED.to_string()),
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `true` for `YYYY-MM-DD...`.
fn dated(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10 && b[4] == b'-' && b[7] == b'-' && b[..4].iter().all(u8::is_ascii_digit)
}

/// The provider's date of a page: the `YYYY-MM-DD` entry of LLM Context's `age` list (or
/// the first entry), never a relative age that changes with the day.
fn age_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Array(items) => {
            let strs: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            strs.iter()
                .find(|s| dated(s))
                .map(|s| s[..10].to_string())
                .or_else(|| strs.first().filter(|s| !s.contains("ago")).map(|s| s.to_string()))
        }
        Value::String(s) if dated(s) => Some(s[..10].to_string()),
        _ => None,
    }
}

/// LLM Context: `grounding.generic[] {url, title, snippets[]}` + `sources{url: {title, age}}`.
fn parse_llm(v: &Value) -> Vec<Found> {
    let sources = v.get("sources");
    v.pointer("/grounding/generic")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|g| {
                    let url = str_of(g, "url").to_string();
                    let src = sources.and_then(|s| s.get(&url));
                    let mut title = str_of(g, "title").to_string();
                    if title.trim().is_empty() {
                        title = src.map(|s| str_of(s, "title").to_string()).unwrap_or_default();
                    }
                    let snippets = g
                        .get("snippets")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .map(|s| match s {
                                    Value::String(t) => t.clone(),
                                    other => other.to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Found {
                        title,
                        url,
                        age: age_of(src.and_then(|s| s.get("age"))),
                        snippets,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Web search: `web.results[] {title, url, description, extra_snippets?, page_age?}`; the
/// HTML of `title` / `description` / snippets is stripped.
fn parse_web(v: &Value) -> Vec<Found> {
    v.pointer("/web/results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|r| {
                    let mut snippets = vec![strip_html(str_of(r, "description"))];
                    if let Some(extra) = r.get("extra_snippets").and_then(Value::as_array) {
                        snippets.extend(extra.iter().filter_map(Value::as_str).map(strip_html));
                    }
                    Found {
                        title: strip_html(str_of(r, "title")),
                        url: str_of(r, "url").to_string(),
                        age: age_of(r.get("page_age")),
                        snippets,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The provider's error code and detail of an error body.
fn error_of(body: &[u8]) -> (String, String) {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = v
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let detail = normalise(v.pointer("/error/detail").and_then(Value::as_str).unwrap_or(""));
    let detail: String = detail.chars().take(DETAIL_CHARS).collect();
    (code, detail)
}

fn http_error(reply: &Reply) -> String {
    let (_, detail) = error_of(&reply.body);
    text::HTTP_ERROR
        .replace("{status}", &reply.status.to_string())
        .replace(
            "{detail}",
            if detail.is_empty() {
                text::NO_DETAIL
            } else {
                &detail
            },
        )
}

impl Brave {
    /// The host this provider connects to.
    pub fn host(&self) -> String {
        self.host_of_base()
    }

    /// Run one search, honoring cancellation.
    pub fn search(&self, q: &Query, cancel: &CancelToken) -> Result<Vec<Found>, String> {
        if self.opts.isolation && !oxa_proto::is_loopback_host(&self.host_of_base()) {
            return Err(text::TEST_REAL_HOST.to_string());
        }
        // One request at a time per process (all instances share the lock).
        let _serial = self.shared.serial.lock().unwrap_or_else(|p| p.into_inner());
        if self.shared.endpoint() == Endpoint::LlmContext {
            let reply = self.get_retrying(Endpoint::LlmContext, q, cancel)?;
            if reply.status == 200 {
                return parse_json(&reply.body).map(|v| parse_llm(&v));
            }
            let (code, _) = error_of(&reply.body);
            if reply.status != 400 || code != "OPTION_NOT_IN_PLAN" {
                return Err(http_error(&reply));
            }
            // The plan lacks LLM Context: web search from now on, said once.
            if !self.shared.fallback.swap(true, Ordering::SeqCst) {
                self.shared.notice_pending.store(true, Ordering::SeqCst);
            }
        }
        let reply = self.get_retrying(Endpoint::WebSearch, q, cancel)?;
        if reply.status != 200 {
            return Err(http_error(&reply));
        }
        parse_json(&reply.body).map(|v| parse_web(&v))
    }

    /// Take the one-time fallback notice, if pending.
    pub fn take_notice(&self) -> Option<String> {
        self.shared
            .notice_pending
            .swap(false, Ordering::SeqCst)
            .then(|| FALLBACK_NOTICE.to_string())
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn retry_backoff_saturates_at_large_attempt_counts() {
        let base = Duration::from_nanos(1);
        assert_eq!(retry_backoff(base, 1), base);
        assert_eq!(retry_backoff(base, 32), Duration::from_nanos(1 << 31));
        assert_eq!(retry_backoff(base, 33), Duration::from_nanos(u32::MAX.into()));
        assert_eq!(
            retry_backoff(base, u32::MAX),
            Duration::from_nanos(u32::MAX.into())
        );
    }
}
