//! CANCEL-GRACE-CPU (lane l5-cancel): a deliberate cancel by oxa tells an engine that advertises
//! the cancel notice (provisional `CANCEL_HINT` / `CANCEL_PATH`) which request ended, once, after
//! the stream was dropped. Opt-in by capability: without the advertisement nothing is sent, and
//! a connection that failed on its own sends nothing either. A failing or hanging notice never
//! delays or fails the abort. Offline: the fake engine on 127.0.0.1 only.

use oxa_engine::HttpEngine;
use oxa_fake::{CancelMode, FakeOpts, FakeServer, Fault, ScriptTurn};
use oxa_proto::{
    codes, CancelToken, ChatRequest, Engine, FrozenPrefix, Hints, OxaError, StreamEvent, TurnKind, WireMsg,
};
use std::thread;
use std::time::{Duration, Instant};

const CHAT_LOG: (&str, &str) = ("POST", "/v1/chat/completions");
const CANCEL_LOG: (&str, &str) = ("POST", "/_local/cancel");

fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), "SYS".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    }
}

fn hints<'a>(session: &'a str, key: &'a str, background: bool) -> Hints<'a> {
    Hints {
        session,
        idem_key: key,
        kind: TurnKind::Chat,
        background,
        snapshot_keep: false,
    }
}

/// A token that fires after `ms` from another thread; returns the token and the instant it fired.
fn cancel_after(ms: u64) -> (CancelToken, std::sync::Arc<std::sync::Mutex<Option<Instant>>>) {
    let token = CancelToken::new();
    let fired = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (t, f) = (token.clone(), fired.clone());
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms));
        *f.lock().unwrap() = Some(Instant::now());
        t.cancel();
    });
    (token, fired)
}

/// A fake whose one chat reply streams comments for 5 s (the client cancels long before).
fn slow_reply_server(cancel: CancelMode) -> FakeServer {
    let turn = ScriptTurn {
        fault: Some(Fault::CommentsOnly { ms: 5000 }),
        ..ScriptTurn::text("Hi")
    };
    let srv = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    srv.set_cancel(cancel);
    srv
}

/// Engine info is read first, as the core does at session start (this is what advertises).
fn engine_for(srv: &FakeServer) -> HttpEngine {
    let e = HttpEngine::new(&srv.url(), None).unwrap();
    e.info(None).unwrap();
    e
}

/// Poll until `n` notices arrived (or 2 s), then give a late duplicate 400 ms to show up.
fn settle_cancels(srv: &FakeServer, n: usize) -> usize {
    let began = Instant::now();
    while srv.cancels().len() < n && began.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(400));
    srv.cancels().len()
}

fn chat_cancelled(e: &HttpEngine, token: &CancelToken, h: Hints) -> Result<(), OxaError> {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let r = ChatRequest {
        output_schema: None,
        prefix: &p,
        messages: &msgs,
        model: "fake-model",
        max_tokens: 64,
        thinking: false,
        thinking_budget: None,
        thinking_level: None,
        preserve_thinking: true,
        sampling: None,
        hints: h,
    };
    let mut seen: Vec<StreamEvent> = Vec::new();
    e.chat(&r, token, &mut |ev| seen.push(ev)).map(|_| ())
}

fn prefill(e: &HttpEngine, token: &CancelToken, h: Hints) -> Result<(), OxaError> {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let r = ChatRequest {
        output_schema: None,
        prefix: &p,
        messages: &msgs,
        model: "fake-model",
        max_tokens: 0,
        thinking: false,
        thinking_budget: None,
        thinking_level: None,
        preserve_thinking: true,
        sampling: None,
        hints: h,
    };
    e.chat(&r, token, &mut |_| {}).map(|_| ())
}

/// A chat stream cut by the cancel token: exactly one notice, with the request's key and the
/// session, after the stream was dropped; the request log is the no-capability log plus it.
#[test]
fn cancelled_stream_sends_one_notice_with_key_and_session() {
    let srv = slow_reply_server(CancelMode::Advertised);
    let e = engine_for(&srv);
    let (token, _) = cancel_after(200);
    let began = Instant::now();
    let err = chat_cancelled(&e, &token, hints("sess-1", "key-1", false)).unwrap_err();
    assert_eq!(err.code, codes::ABORTED);
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "abort took {:?}",
        began.elapsed()
    );

    assert_eq!(settle_cancels(&srv, 1), 1, "exactly one notice");
    let c = &srv.cancels()[0];
    assert_eq!(c.idem_key, "key-1");
    assert_eq!(c.session, "sess-1");
    let header = |n: &str| c.headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    assert_eq!(header("x-oxe-session"), Some("sess-1"));
    assert_eq!(header("x-session-id"), Some("sess-1"));
    assert_eq!(
        srv.request_log().last().map(|(m, p)| (m.as_str(), p.as_str())),
        Some(CANCEL_LOG),
        "the notice is the last request"
    );
    assert_eq!(srv.hits(CANCEL_LOG.0, CANCEL_LOG.1), 1);
}

/// A prefill (`max_tokens: 0`) cancelled by the background token (user input) sends the notice
/// too, with its own key.
#[test]
fn cancelled_prefill_sends_one_notice() {
    let srv = FakeServer::start(
        Vec::new(),
        FakeOpts {
            hold_prefill_ms: 5000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    srv.set_cancel(CancelMode::Advertised);
    let e = engine_for(&srv);
    let (token, _) = cancel_after(200);
    let err = prefill(&e, &token, hints("sess-bg", "pre-key", true)).unwrap_err();
    assert_eq!(err.code, codes::ABORTED);

    assert_eq!(settle_cancels(&srv, 1), 1);
    let c = &srv.cancels()[0];
    assert_eq!((c.idem_key.as_str(), c.session.as_str()), ("pre-key", "sess-bg"));
}

/// No advertisement (the default engine): the cancel is a plain abort, the request log holds
/// no notice, and the log is exactly what the same scenario produces today.
#[test]
fn without_the_capability_nothing_is_sent() {
    let srv = slow_reply_server(CancelMode::Off);
    let e = engine_for(&srv);
    let (token, _) = cancel_after(200);
    let err = chat_cancelled(&e, &token, hints("sess-1", "key-1", false)).unwrap_err();
    assert_eq!(err.code, codes::ABORTED);

    assert_eq!(settle_cancels(&srv, 0), 0);
    assert_eq!(srv.hits(CANCEL_LOG.0, CANCEL_LOG.1), 0);
    let log = srv.request_log();
    let expected: Vec<(String, String)> = [
        ("GET", "/v1/models"),
        ("GET", "/props"),
        ("GET", "/_local/status"),
        CHAT_LOG,
    ]
    .iter()
    .map(|(m, p)| (m.to_string(), p.to_string()))
    .collect();
    assert_eq!(log, expected);
}

/// A connection the server closed mid-stream is a failure, not a cancel: no notice, even with the
/// capability advertised.
#[test]
fn server_side_close_sends_nothing() {
    let turn = ScriptTurn {
        fault: Some(Fault::DropAt { bytes: 4 }),
        ..ScriptTurn::text("Hello there")
    };
    let srv = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    srv.set_cancel(CancelMode::Advertised);
    let e = engine_for(&srv);
    let token = CancelToken::new();
    let err = chat_cancelled(&e, &token, hints("sess-1", "key-1", false)).unwrap_err();
    assert_ne!(err.code, codes::ABORTED);
    assert_eq!(settle_cancels(&srv, 0), 0);
}

/// A token that is already cancelled sends nothing: the request was never sent.
#[test]
fn cancel_before_send_sends_nothing() {
    let srv = slow_reply_server(CancelMode::Advertised);
    let e = engine_for(&srv);
    let token = CancelToken::new();
    token.cancel();
    let err = chat_cancelled(&e, &token, hints("sess-1", "key-1", false)).unwrap_err();
    assert_eq!(err.code, codes::ABORTED);
    assert_eq!(settle_cancels(&srv, 0), 0);
    assert_eq!(srv.hits(CHAT_LOG.0, CHAT_LOG.1), 0);
}

/// A notice endpoint that fails (500) or hangs (10 s) neither delays nor fails the abort: the
/// abort returns `aborted` within 3 s of the cancel, and the one notice was still received.
#[test]
fn failing_or_hanging_notice_does_not_delay_the_abort() {
    for mode in [CancelMode::Error, CancelMode::Hang] {
        let srv = slow_reply_server(mode);
        let e = engine_for(&srv);
        let (token, fired) = cancel_after(200);
        let err = chat_cancelled(&e, &token, hints("sess-1", "key-1", false)).unwrap_err();
        assert_eq!(err.code, codes::ABORTED, "{mode:?}");
        let fired = fired.lock().unwrap().expect("the token fired");
        let took = fired.elapsed();
        assert!(
            took < Duration::from_secs(3),
            "{mode:?}: abort took {took:?} after the cancel"
        );
        assert_eq!(settle_cancels(&srv, 1), 1, "{mode:?}: the notice was sent once");
    }
}

/// The controller's proposal advertisement alone: `cancel` in the top-level `capabilities` of
/// `/_local/status`, with `/props` carrying no `oxe_hints` at all. The notice is still sent.
#[test]
fn status_capability_alone_sends_the_notice() {
    let srv = FakeServer::start(
        Vec::new(),
        FakeOpts {
            hints: false,
            status_cancel: true,
            hold_prefill_ms: 5000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    srv.set_cancel(CancelMode::Advertised);
    let e = engine_for(&srv);
    let (token, _) = cancel_after(200);
    let err = prefill(&e, &token, hints("sess-st", "st-key", true)).unwrap_err();
    assert_eq!(err.code, codes::ABORTED);

    assert_eq!(settle_cancels(&srv, 1), 1);
    let c = &srv.cancels()[0];
    assert_eq!((c.idem_key.as_str(), c.session.as_str()), ("st-key", "sess-st"));
}
