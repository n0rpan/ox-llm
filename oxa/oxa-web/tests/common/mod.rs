//! Test helpers: a tiny HTTP/1.1 mock of the Brave API on 127.0.0.1 (std only, one connection
//! per request, `Connection: close`), as the oxa-mcp tests do. Nothing leaves the machine and
//! no real key is used (O-170).
#![allow(dead_code)]

use oxa_proto::{CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, ToolCtx};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// An obviously fake key.
pub const FAKE_KEY: &str = "test-key-not-real";

/// One request as the mock saw it. Header names lower-cased.
#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
    pub fn param(&self, name: &str) -> Option<String> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }
}

/// What the mock answers.
#[derive(Clone, Debug)]
pub struct Canned {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Wait this long before answering.
    pub delay: Duration,
}

pub fn ok(body: &str) -> Canned {
    status(200, body)
}

pub fn status(status: u16, body: &str) -> Canned {
    Canned {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: body.to_string(),
        delay: Duration::ZERO,
    }
}

type Handler = dyn Fn(&Seen, usize) -> Canned + Send + Sync;

pub struct Mock {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    in_flight: Arc<AtomicUsize>,
    pub max_in_flight: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let h = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'?');
                out.push(h);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Mock {
    /// Start the mock; `handler(seen, n)` answers request number `n` (0-based).
    pub fn start(handler: impl Fn(&Seen, usize) -> Canned + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let count = Arc::new(AtomicUsize::new(0));
        {
            let (seen, in_flight, max_in_flight, stop) = (
                seen.clone(),
                in_flight.clone(),
                max_in_flight.clone(),
                stop.clone(),
            );
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let (seen, in_flight, max_in_flight, handler, count) = (
                        seen.clone(),
                        in_flight.clone(),
                        max_in_flight.clone(),
                        handler.clone(),
                        count.clone(),
                    );
                    std::thread::spawn(move || {
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        max_in_flight.fetch_max(now, Ordering::SeqCst);
                        serve(conn, &seen, &*handler, &count);
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            });
        }
        Mock {
            url,
            seen,
            in_flight,
            max_in_flight,
            stop,
        }
    }
    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    pub fn hits(&self, path: &str) -> usize {
        self.requests().iter().filter(|s| s.path == path).count()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
    }
}

fn serve(conn: TcpStream, seen: &Mutex<Vec<Seen>>, handler: &Handler, count: &AtomicUsize) {
    let mut reader = BufReader::new(conn.try_clone().expect("clone"));
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "content-length" {
                length = v.trim().parse().unwrap_or(0);
            }
            headers.push((k, v.trim().to_string()));
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let (path, qs) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let s = Seen {
        path: path.to_string(),
        query,
        headers,
    };
    seen.lock().unwrap().push(s.clone());
    let n = count.fetch_add(1, Ordering::SeqCst);
    let c = handler(&s, n);
    std::thread::sleep(c.delay);
    let mut out = conn;
    let mut head = format!(
        "HTTP/1.1 {} X\r\nConnection: close\r\nContent-Length: {}\r\n",
        c.status,
        c.body.len()
    );
    for (k, v) in &c.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(c.body.as_bytes());
    let _ = out.flush();
}

/// LLM Context answer with `n` sources of `snippets` each.
pub fn llm_body(n: usize, snippet: &str) -> String {
    let generic: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            serde_json::json!({
                "url": format!("https://example.com/{i}"),
                "title": format!("Title {i}"),
                "snippets": [format!("{snippet} {i}"), format!("second {i}")],
            })
        })
        .collect();
    let mut sources = serde_json::Map::new();
    for i in 0..n {
        sources.insert(
            format!("https://example.com/{i}"),
            serde_json::json!({"title": format!("Title {i}"), "hostname": "example.com", "age": ["Monday, May 6, 2024", "2024-05-06", "380 days ago"]}),
        );
    }
    serde_json::json!({"grounding": {"generic": generic, "poi": null, "map": []}, "sources": sources})
        .to_string()
}

/// Web search answer with `n` results.
pub fn web_body(n: usize) -> String {
    let results: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            serde_json::json!({
                "title": format!("Result <strong>{i}</strong>"),
                "url": format!("https://example.org/{i}"),
                "description": format!("Desc &amp; <strong>bold</strong> {i}"),
                "extra_snippets": [format!("extra {i}")],
                "age": "2 days ago",
                "page_age": "2024-05-06T10:00:00",
            })
        })
        .collect();
    serde_json::json!({"type": "search", "web": {"type": "search", "results": results}}).to_string()
}

pub const NOT_IN_PLAN: &str = r#"{"type":"ErrorResponse","error":{"id":"d3b5f6a0-1111-2222-3333-444455556666","status":400,"code":"OPTION_NOT_IN_PLAN","detail":"The option is not included in the plan.","meta":{}}}"#;

/// A sandbox the web tool never uses.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

pub fn ctx<'a>(cwd: &'a Path, sb: &'a NoSandbox, mode: Mode, cancel: &'a CancelToken) -> ToolCtx<'a> {
    ToolCtx {
        checked: &[],
        cwd,
        spill_dir: cwd,
        sandbox: sb,
        mode,
        cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    }
}

/// Fast production-shaped options against `url`.
pub fn opts(url: &str) -> oxa_web::BraveOpts {
    oxa_web::BraveOpts {
        base_url: url.to_string(),
        timeout: Duration::from_secs(5),
        backoff_base: Duration::from_millis(20),
        max_wait: Duration::from_secs(2),
        isolation: true,
        ..oxa_web::BraveOpts::default()
    }
}

pub fn key() -> oxa_web::ApiKey {
    oxa_web::ApiKey::new(FAKE_KEY).unwrap()
}
