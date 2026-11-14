//! oxa <-> real server contract (fit review F5). Copied by `tools/oxa-contract/run.sh` into a
//! scratch copy of an oxa checkout as `oxa-core/tests/zz_stub_contract.rs` and run there, so it
//! uses oxa's own client code (body builder, SSE reader, reply assembly, error mapping, transcript
//! cache accounting) against the REAL `oxe-server --engine stub` (direct) and the real lab
//! controller in front of it (queue on). Where the fake must behave like the server, the same
//! request also goes to `oxa_fake::FakeServer` and the observable outcome is compared.
//!
//! Environment (set by run.sh):
//! - `OXA_CONTRACT_ENGINE_URL` (required): the stub engine, `http://127.0.0.1:<port>`.
//! - `OXA_CONTRACT_CTL_URL` (optional): the controller with `LOCAL_AI_QUEUE=1` in front of it.
//! - `OXA_CONTRACT_MODE`: `engine` | `controller` | `both` (which servers the scenarios target).
//! - `OXA_CONTRACT_N_CTX`: the engine's context size.
//!
//! The stub runs with a slow prefill (`--stub-prefill-us`) and a short keepalive
//! (`OXE_SSE_KEEPALIVE_SECS`), so prefill comments and queueing are observable without a model.
//! Run with `--test-threads=1` (the slow requests share one engine). Every scenario first checks
//! that the server really produced the situation (a setup check), so a pass is never vacuous.
//! `/*OXA_CHAT_REQUEST_EXTRA*/` is replaced by run.sh with fields newer oxa revisions require.

use oxa_engine::{build_body, build_headers, post_sse, Flow, HttpEngine, SseEvent, StreamOpts, Url};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
use oxa_proto::{
    codes, AssistantTurn, CancelToken, ChatRequest, Engine, Finish, FrozenPrefix, Hints, OxaError, StreamEvent,
    TurnKind, WireMsg,
};
use oxa_session::Transcript;
use serde_json::Value;
use std::time::Duration;

const SYSTEM: &str = "You are a contract test agent. Answer briefly.";

fn engine_url() -> String {
    std::env::var("OXA_CONTRACT_ENGINE_URL").expect("OXA_CONTRACT_ENGINE_URL (run tools/oxa-contract/run.sh)")
}

fn ctl_url() -> Option<String> {
    std::env::var("OXA_CONTRACT_CTL_URL").ok().filter(|s| !s.is_empty())
}

fn n_ctx() -> u32 {
    std::env::var("OXA_CONTRACT_N_CTX").ok().and_then(|s| s.parse().ok()).unwrap_or(32768)
}

/// `OXA_CONTRACT_MODE`: `engine` (direct to oxe-server), `controller` (through the lab controller
/// with the queue on) or `both` (default when a controller URL is given).
fn mode() -> String {
    std::env::var("OXA_CONTRACT_MODE").unwrap_or_else(|_| if ctl_url().is_some() { "both" } else { "engine" }.to_string())
}

/// (name, base URL) of every real server under test in this mode.
fn targets() -> Vec<(&'static str, String)> {
    let mut v = Vec::new();
    if mode() != "controller" {
        v.push(("engine", engine_url()));
    }
    if mode() != "engine" {
        v.push(("controller", ctl_url().expect("OXA_CONTRACT_CTL_URL is required in controller mode")));
    }
    v
}

fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), SYSTEM.to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    }
}

fn hints(session: &str) -> Hints<'_> {
    Hints {
        session,
        idem_key: "",
        kind: TurnKind::Chat,
        background: false,
        snapshot_keep: false,
    }
}

fn model_of(url: &str) -> String {
    let e = HttpEngine::new(url, None).expect("url");
    e.info(None).map(|i| i.model).unwrap_or_else(|_| "local-default".to_string())
}

/// Words that tokenize to roughly one token each; `seed` keeps prompts of different calls apart
/// (no cache reuse between them).
fn words(n: usize, seed: &str) -> String {
    let base = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet"];
    let mut s = String::with_capacity(n * 7 + seed.len());
    s.push_str(seed);
    for i in 0..n {
        s.push(' ');
        s.push_str(base[(i * 7 + seed.len()) % base.len()]);
    }
    s
}

fn chat(
    url: &str,
    model: &str,
    session: &str,
    msgs: &[WireMsg],
    max_tokens: u32,
    events: &mut Vec<StreamEvent>,
) -> Result<AssistantTurn, OxaError> {
    let p = prefix();
    let eng = HttpEngine::new(url, None)?;
    let r = ChatRequest {
        prefix: &p,
        messages: msgs,
        model,
        max_tokens,
        thinking: false,
        /*OXA_CHAT_REQUEST_EXTRA*/
        hints: hints(session),
    };
    eng.chat(&r, &CancelToken::new(), &mut |e| events.push(e))
}

/// What the server sent for one request, read with oxa's SSE reader (setup checks).
#[derive(Debug, Default)]
struct Raw {
    status: u16,
    comments: Vec<String>,
    data: Vec<Value>,
    done: bool,
    error_body: Option<String>,
    /// post_sse's own error (a stream that ended without [DONE], e.g. after an error event).
    failure: Option<String>,
}

fn raw(url: &str, model: &str, session: &str, msgs: &[WireMsg], max_tokens: u32) -> Raw {
    let p = prefix();
    let r = ChatRequest {
        prefix: &p,
        messages: msgs,
        model,
        max_tokens,
        thinking: false,
        /*OXA_CHAT_REQUEST_EXTRA*/
        hints: hints(session),
    };
    let mut body = Vec::new();
    build_body(&r, &mut body);
    let key = format!("contract-{session}-{}", std::process::id());
    let h = Hints { idem_key: &key, ..hints(session) };
    let headers = build_headers(&h);
    let mut out = Raw::default();
    let res = post_sse(
        &Url::parse(url).expect("url"),
        "/v1/chat/completions",
        &headers,
        &body,
        &StreamOpts::default(),
        &CancelToken::new(),
        &mut |ev| {
            match ev {
                SseEvent::Comment(c) => out.comments.push(c),
                SseEvent::Data(d) => out.data.push(serde_json::from_str(&d).unwrap_or(Value::String(d))),
                SseEvent::Done => out.done = true,
            }
            Flow::Continue
        },
    );
    match res {
        Ok(end) => {
            out.status = end.status;
            out.error_body = end.error_body;
        }
        // the head was read (an error event ends a 200 stream without [DONE]): the status is 200
        Err(e) => {
            out.status = if out.data.is_empty() && out.comments.is_empty() { 0 } else { 200 };
            out.failure = Some(e.to_string());
        }
    }
    out
}

/// The kind of each data chunk: role / content / finish:<reason> / usage / error.
fn shape(r: &Raw) -> Vec<String> {
    let mut v = Vec::new();
    for d in &r.data {
        if d.get("error").is_some() {
            v.push("error".to_string());
            continue;
        }
        let ch = d.pointer("/choices/0");
        if let Some(c) = ch {
            if c.pointer("/delta/role").is_some() {
                v.push("role".to_string());
            }
            if c.pointer("/delta/content").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
                v.push("content".to_string());
            }
            if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
                v.push(format!("finish:{f}"));
            }
        }
        if d.get("usage").is_some_and(|u| !u.is_null()) {
            v.push(if ch.is_none() { "usage".to_string() } else { "usage-in-choice-chunk".to_string() });
        }
    }
    v.dedup();
    if r.done {
        v.push("[DONE]".to_string());
    }
    v
}

fn fake(n_ctx: u32) -> FakeServer {
    let turns = (0..8).map(|i| ScriptTurn::text(&format!("fake reply {i}"))).collect();
    FakeServer::start(turns, FakeOpts { n_ctx, ..FakeOpts::default() }).expect("fake server")
}

// ---------------------------------------------------------------------------------------------

/// F1: a plain continuation (the previous reply sent back unchanged plus one user message) must
/// never be a cache miss by oxa's own rule (`usage.cached >= Transcript::expected_cache()`, the
/// comparison of the agent loop). The real engine keeps every fed token; the last generated id
/// was never fed, so cached = prompt + completion - 1.
#[test]
fn f1_plain_continuation_is_not_a_cache_miss() {
    for (name, url) in targets() {
        let model = model_of(&url);
        let session = format!("f1-{name}");
        let mut t = Transcript::default();
        t.push(WireMsg::user(words(40, &format!("f1 {name} first question"))));
        let mut misses = Vec::new();
        for turn in 0..3 {
            let expected = t.expected_cache();
            let mut ev = Vec::new();
            let reply = chat(&url, &model, &session, t.wire(), 32, &mut ev)
                .unwrap_or_else(|e| panic!("{name} turn {turn}: {e}"));
            if turn > 0 && reply.usage.cached < expected {
                misses.push((turn, expected, reply.usage.cached, reply.usage.prompt));
            }
            t.push(reply.msg.clone());
            t.set_usage(&reply.usage);
            t.push(WireMsg::user(format!("f1 follow-up {turn}")));
        }
        assert!(
            misses.is_empty(),
            "{name}: oxa reports a cache miss on a perfect continuation (turn, expected, cached, prompt): {misses:?}"
        );
    }
}

/// F2: the engine's `: prefill <done>/<total> <secs>s` keepalive must reach oxa as
/// `StreamEvent::Prefill`.
#[test]
fn f2_prefill_progress_reaches_oxa() {
    for (name, url) in targets() {
        let model = model_of(&url);
        let msgs = [WireMsg::user(words(6000, &format!("f2 setup {name}")))];
        let r = raw(&url, &model, &format!("f2s-{name}"), &msgs, 4);
        let prefill: Vec<&String> = r.comments.iter().filter(|c| c.starts_with("prefill ")).collect();
        assert!(!prefill.is_empty(), "{name} setup: the server sent no prefill comment ({:?})", r.comments);

        let msgs = [WireMsg::user(words(6000, &format!("f2 oxa {name}")))];
        let mut ev = Vec::new();
        chat(&url, &model, &format!("f2-{name}"), &msgs, 4, &mut ev).unwrap_or_else(|e| panic!("{name}: {e}"));
        let seen = ev.iter().filter(|e| matches!(e, StreamEvent::Prefill { .. })).count();
        assert!(seen > 0, "{name}: the server sent prefill comments like {:?} but oxa produced no Prefill event", prefill[0]);
    }
}

/// F3: through the controller with the queue on, an engine error after the early SSE head arrives as
/// `event: error` + an OpenAI error object. oxa must map it exactly as the same error sent as a
/// status: a context overflow is `context_overflow` (compaction), not `stream_broken` (retried).
#[test]
fn f3_error_after_early_headers_keeps_its_meaning() {
    let big = |tag: &str| [WireMsg::user(words(n_ctx() as usize + 4000, tag))];
    // direct: the status form
    if mode() != "controller" {
        let url = engine_url();
        let model = model_of(&url);
        let err = chat(&url, &model, "f3-engine", &big("f3 engine"), 16, &mut Vec::new()).expect_err("overflow");
        assert_eq!(err.code, codes::CONTEXT_OVERFLOW, "engine status form: {err}");
    }
    if mode() == "engine" {
        return;
    }
    let ctl = ctl_url().expect("controller URL");
    let model = model_of(&ctl);
    let busy = |tag: String| {
        let (ctl, model) = (ctl.clone(), model.clone());
        std::thread::spawn(move || {
            let msgs = [WireMsg::user(words(8000, &tag))];
            raw(&ctl, &model, &tag, &msgs, 4)
        })
    };
    // setup check: the overflow really comes as 200 + an in-stream error after queueing
    let a = busy("f3 busy one".to_string());
    std::thread::sleep(Duration::from_millis(400));
    let r = raw(&ctl, &model, "f3-setup", &big("f3 setup"), 16);
    a.join().unwrap();
    let in_stream = r.status == 200
        && r.comments.iter().any(|c| c.starts_with("queued"))
        && r.data.iter().any(|d| d.pointer("/error/type").and_then(Value::as_str) == Some("exceed_context_size_error"));
    assert!(in_stream, "setup: expected 200, a queued comment and an in-stream exceed_context_size_error; got {} {:?} {:?} {:?}", r.status, r.comments, shape(&r), r.error_body);

    let a = busy("f3 busy two".to_string());
    std::thread::sleep(Duration::from_millis(400));
    let err = chat(&ctl, &model, "f3-ctl", &big("f3 ctl"), 16, &mut Vec::new()).expect_err("overflow");
    a.join().unwrap();
    assert_eq!(err.code, codes::CONTEXT_OVERFLOW, "controller in-stream form: oxa mapped it to {}: {err}", err.code);
}

/// F6: the engine refuses only a prompt that does not fit (`prompt >= n_ctx`); a `max_tokens` beyond
/// the remaining context is not refused (it generates until the context is full and ends with
/// `length`). The fake must answer the same request the same way.
#[test]
fn f6_max_tokens_beyond_the_remaining_context_is_not_refused() {
    let msgs = [WireMsg::user("f6 short question")];
    let mut outcomes = Vec::new();
    for (name, url) in targets() {
        let model = model_of(&url);
        let r = chat(&url, &model, &format!("f6-{name}"), &msgs, n_ctx(), &mut Vec::new());
        assert!(r.is_ok(), "{name}: {:?}", r.err());
        outcomes.push((name, "ok".to_string()));
    }
    let f = fake(n_ctx());
    let r = chat(&f.url(), "fake-model", "f6-fake", &msgs, n_ctx(), &mut Vec::new());
    let fake_outcome = match &r {
        Ok(_) => "ok".to_string(),
        Err(e) => e.code.to_string(),
    };
    assert_eq!(fake_outcome, "ok", "the fake refuses what the server accepts (server: {outcomes:?}; fake: {r:?})");
}

/// F7: H6 (`max_tokens: 0`) reply: finish `length`, and the fake's stream has the server's shape
/// (role chunk, finish chunk, a separate usage chunk, [DONE]).
#[test]
fn f7_h6_reply_matches_the_server() {
    let msgs = [WireMsg::user("f7 prefill only")];
    let (name, url) = targets().remove(0);
    let model = model_of(&url);
    let mut ev = Vec::new();
    let t = chat(&url, &model, "f7-server", &msgs, 0, &mut ev).unwrap_or_else(|e| panic!("h6 on the {name}: {e}"));
    assert_eq!(t.finish, Finish::Length, "{name} H6 finish");
    assert_eq!(t.usage.completion, 0);
    let server = shape(&raw(&url, &model, "f7-server-raw", &msgs, 0));

    let f = fake(n_ctx());
    let ft = chat(&f.url(), "fake-model", "f7-fake", &msgs, 0, &mut Vec::new()).expect("h6 on the fake");
    let fake_shape = shape(&raw(&f.url(), "fake-model", "f7-fake-raw", &msgs, 0));
    assert_eq!(
        (ft.finish, fake_shape.clone()),
        (t.finish, server.clone()),
        "fake H6 (finish, stream shape) differs from the engine's"
    );
}

/// F9 (OpenAI v1): a generating request that ends with an assistant message gets a NEW assistant
/// turn (it is history), on the server and on the fake.
#[test]
fn f9_trailing_assistant_gets_a_new_turn() {
    let msgs = [WireMsg::user("f9 question"), WireMsg::assistant("f9 earlier answer")];
    for (name, url) in targets() {
        let model = model_of(&url);
        let t = chat(&url, &model, &format!("f9-{name}"), &msgs, 16, &mut Vec::new())
            .unwrap_or_else(|e| panic!("{name}: a trailing assistant message is refused: {e}"));
        assert!(t.msg.content.as_deref().is_some_and(|c| !c.is_empty()), "{name}: no new turn: {t:?}");
    }
    let f = fake(n_ctx());
    let r = chat(&f.url(), "fake-model", "f9-fake", &msgs, 16, &mut Vec::new());
    assert!(r.is_ok(), "the fake refuses a trailing assistant message the server accepts: {:?}", r.err());
}

/// F10: every keepalive comment of the server follows the documented grammar:
/// `queued [pos=<n>] wait_s=<s>` (key=value fields) or `prefill <done>/<total> ...`.
#[test]
fn f10_comment_grammar() {
    let mut all = Vec::new();
    for (name, url) in targets() {
        let model = model_of(&url);
        let busy = {
            let (url, model) = (url.clone(), model.clone());
            std::thread::spawn(move || raw(&url, &model, "f10-busy", &[WireMsg::user(words(6000, "f10 busy"))], 4))
        };
        std::thread::sleep(Duration::from_millis(300));
        let r = raw(&url, &model, &format!("f10-{name}"), &[WireMsg::user(words(3000, &format!("f10 {name}")))], 4);
        let b = busy.join().unwrap();
        all.extend(r.comments.into_iter().chain(b.comments).map(|c| (name, c)));
    }
    assert!(all.iter().any(|(_, c)| c.starts_with("queued")), "setup: no queued comment seen: {all:?}");
    for (name, c) in &all {
        let mut it = c.split_whitespace();
        let ok = match it.next() {
            Some("queued") => {
                let fields: Vec<(&str, &str)> = it.filter_map(|f| f.split_once('=')).collect();
                fields.iter().any(|(k, v)| *k == "wait_s" && v.parse::<u64>().is_ok())
                    && fields.iter().all(|(k, v)| *k != "pos" || v.parse::<u32>().is_ok_and(|n| n >= 1))
                    && c.split_whitespace().skip(1).all(|f| f.contains('='))
            }
            Some("prefill") => it.next().and_then(|f| f.split_once('/')).is_some_and(|(d, t)| d.parse::<u64>().is_ok() && t.parse::<u64>().is_ok()),
            _ => true, // other comments are free
        };
        assert!(ok, "{name}: comment {c:?} does not follow the documented grammar");
    }
}

/// Info: oxa reads the context size and the H6 capability from the real server, direct and through
/// the controller (F4/F14 neighbourhood).
#[test]
fn info_reads_limits_and_capabilities() {
    for (name, url) in targets() {
        let i = HttpEngine::new(&url, None).unwrap().info(None).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(i.n_ctx, n_ctx(), "{name}: n_ctx");
        assert!(i.prefill_only, "{name}: the server supports H6 (max_tokens 0) but oxa's info says it does not");
    }
}
