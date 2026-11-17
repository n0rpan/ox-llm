//! #141 over a real `HttpEngine`: the looping text, the finish chunk and `[DONE]` can arrive in
//! one read, so the stream ends cleanly after the guard fired. The run must still fail as a
//! repetition loop, the reply must stay Shown-only, and the next request must be healthy.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, EntryBody, HeadlessUi, OxaError,
    StreamEvent, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Info and token counts from the script; chat over real HTTP.
struct Http {
    inner: ScriptedEngine,
    http: oxa_engine::HttpEngine,
}

impl Engine for Http {
    fn info(&self, t: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(t)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
        self.inner.count_prompt(r)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        self.http.chat(r, cancel, on)
    }
}

fn read_request(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return String::new(),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// One coalesced SSE response: content, finish + usage and `[DONE]` in a single write.
fn coalesced(text: &str) -> Vec<u8> {
    coalesced_field("content", text)
}

/// Same, with the text in the given delta field (`reasoning_content` for thinking).
fn coalesced_field(field: &str, text: &str) -> Vec<u8> {
    let content = json!({"choices":[{"index":0,"delta":{field:text},"finish_reason":null}]});
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n".to_vec();
    for e in [content.to_string().as_str(), finish, "[DONE]"] {
        out.extend_from_slice(format!("data: {e}\n\n").as_bytes());
    }
    out
}

/// Serves one response per connection, in order; records each request.
fn server(replies: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for reply in replies {
            let Ok((mut s, _)) = listener.accept() else { return };
            let req = read_request(&mut s);
            log.lock().unwrap().push(req);
            let _ = s.write_all(&reply);
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn open(env: &TestEnv, url: &str) -> Session {
    let http = oxa_engine::HttpEngine::new(url, None).unwrap();
    let engine: Arc<dyn Engine> = Arc::new(Http {
        inner: ScriptedEngine::new(Vec::<ScriptTurn>::new()),
        http,
    });
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(json!({}), json!({})),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, Open::New).expect("open")
}

fn shown(s: &Session) -> Vec<String> {
    s.inner()
        .tree
        .entries()
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Shown { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn assistant_texts(s: &Session) -> Vec<String> {
    s.inner()
        .tree
        .entries()
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Msg { wire, .. } if wire.role == oxa_proto::Role::Assistant => wire.content.clone(),
            _ => None,
        })
        .collect()
}

#[test]
fn coalesced_looping_reply_fails_shown_only_and_next_request_is_healthy() {
    let env = TestEnv::new();
    let para: String = (0..40).map(|i| format!("word{} ", (i * 7) % 97)).collect();
    let looping = format!("Start.\n\n{}", para.repeat(8));
    let (url, seen) = server(vec![coalesced(&looping), coalesced("all good")]);
    let mut s = open(&env, &url);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());

    // `prompt` settles Ok for a failed run; the failure is the Shown entry below.
    s.prompt("hi".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt settles");
    assert!(
        !s.inner().shared.cancel.is_cancelled(),
        "the guard-owned cancel flag is cleared"
    );
    let sh = shown(&s);
    assert_eq!(sh.len(), 1);
    assert!(sh[0].contains("\n\nResponse stopped: the model fell into a repetition loop"));
    assert!(assistant_texts(&s).is_empty(), "never persisted as history");

    s.prompt("again".into(), Vec::new(), &mut sink, &mut ui)
        .expect("healthy next request");
    assert_eq!(assistant_texts(&s), vec!["all good".to_string()]);
    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(!reqs[1].contains("word0 word7"), "the loop is not sent back");
    assert!(!reqs[1].contains("repetition loop"));
}

#[test]
fn coalesced_ordinary_reply_is_unaffected() {
    let env = TestEnv::new();
    let (url, _seen) = server(vec![coalesced("A normal short answer.")]);
    let mut s = open(&env, &url);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("hi".into(), Vec::new(), &mut sink, &mut ui).expect("ok");
    assert!(shown(&s).is_empty());
    assert_eq!(assistant_texts(&s), vec!["A normal short answer.".to_string()]);
}

/// #513: a model that loops inside its reasoning is stopped like one that loops in its answer,
/// instead of streaming until the output limit.
#[test]
fn looping_reasoning_is_stopped_shown_only_and_next_request_is_healthy() {
    let env = TestEnv::new();
    let para: String = (0..40).map(|i| format!("word{} ", (i * 7) % 97)).collect();
    let looping = format!("Let me think.\n\n{}", para.repeat(8));
    let (url, seen) = server(vec![
        coalesced_field("reasoning_content", &looping),
        coalesced("all good"),
    ]);
    let mut s = open(&env, &url);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());

    s.prompt("hi".into(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt settles");
    assert!(!s.inner().shared.cancel.is_cancelled());
    let sh = shown(&s);
    assert_eq!(sh.len(), 1, "{sh:?}");
    assert!(sh[0].contains("Response stopped: the model fell into a repetition loop"));
    assert!(assistant_texts(&s).is_empty());

    s.prompt("again".into(), Vec::new(), &mut sink, &mut ui)
        .expect("healthy next request");
    assert_eq!(assistant_texts(&s), vec!["all good".to_string()]);
    assert_eq!(seen.lock().unwrap().len(), 2, "no retry of the looping request");
}
