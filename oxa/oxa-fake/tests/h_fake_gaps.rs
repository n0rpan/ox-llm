//! Wave H: the fake engine gaps the PARITY suite needed (ENG-001, ENG-002, ENG-004, RPC-015,
//! SET-004, SET-051), engine F9 (a trailing assistant message is history), the `cold` fault and
//! `/apply-template` (exact prompt token counts). Raw HTTP on 127.0.0.1 only.

use oxa_fake::{cache_bytes, FakeModel, FakeOpts, FakeServer, Fault, ScriptTurn};
use oxa_proto::WireMsg;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn connect(url: &str) -> TcpStream {
    let s = TcpStream::connect(url.trim_start_matches("http://")).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s
}

fn send(s: &mut TcpStream, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
}

/// (status, body text: the raw bytes after the head, chunk framing included).
fn exchange(url: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String) {
    let mut s = connect(url);
    send(&mut s, method, path, headers, body);
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .map(|x| x.1.to_string())
        .unwrap_or_default();
    (status, body)
}

fn json_body(text: &str) -> Value {
    serde_json::from_str(text).expect("JSON")
}

fn chat_body(model: &str, messages: &[WireMsg], max_tokens: u32, extra: &str) -> Vec<u8> {
    let mut msgs = vec![json!({"role": "system", "content": "sys"})];
    for m in messages {
        msgs.push(serde_json::from_slice(&m.to_json()).unwrap());
    }
    format!(
        r#"{{"model":"{model}","messages":{},"tools":[],"max_tokens":{max_tokens},"stream":true{extra}}}"#,
        Value::Array(msgs)
    )
    .into_bytes()
}

/// The `usage` object of an SSE reply body.
fn usage_of(body: &str) -> Value {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find_map(|v| v.get("usage").cloned())
        .expect("a usage chunk")
}

fn two_models() -> FakeOpts {
    FakeOpts {
        model: "big".to_string(),
        models: vec![
            FakeModel::moe("big", 131072),
            FakeModel::flash("small", 4096, Some(512)),
        ],
        ..FakeOpts::default()
    }
}

#[test]
fn v1_models_lists_every_model_limits_row() {
    let srv = FakeServer::start(Vec::new(), two_models()).unwrap();
    let (status, body) = exchange(&srv.url(), "GET", "/v1/models", &[], b"");
    assert_eq!(status, 200);
    let ids: Vec<String> = json_body(&body)["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["big", "small"]);

    let one = FakeServer::start(Vec::new(), FakeOpts::default()).unwrap();
    let (_, body) = exchange(&one.url(), "GET", "/v1/models", &[], b"");
    assert_eq!(json_body(&body)["data"][0]["id"], json!("fake-model"));
}

#[test]
fn a_request_uses_the_context_of_its_model() {
    let srv = FakeServer::start(vec![ScriptTurn::text("ok")], two_models()).unwrap();
    let big = WireMsg::user("x".repeat(5000));
    let (status, body) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &[],
        &chat_body("small", std::slice::from_ref(&big), 8, ""),
    );
    assert_eq!(status, 400, "over the small model's 4096: {body}");
    assert!(body.contains("exceed_context_size_error"));
    let (status, _) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &[],
        &chat_body("big", &[big], 8, ""),
    );
    assert_eq!(status, 200);
}

#[test]
fn in_flight_counter_and_timings() {
    let srv = FakeServer::start(
        vec![
            ScriptTurn::fault(Fault::HoldHeaders { ms: 300 }),
            ScriptTurn::fault(Fault::HoldHeaders { ms: 300 }),
            ScriptTurn::text("c"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let url = srv.url();
    let body = chat_body("fake-model", &[WireMsg::user("hi")], 8, "");
    let (u2, b2) = (url.clone(), body.clone());
    let t = std::thread::spawn(move || exchange(&u2, "POST", "/v1/chat/completions", &[], &b2));
    exchange(&url, "POST", "/v1/chat/completions", &[], &body);
    t.join().unwrap();
    assert_eq!(
        srv.max_in_flight(),
        2,
        "two overlapping requests are seen as such"
    );
    assert_eq!(srv.in_flight(), 0);

    let solo = FakeServer::start(
        vec![ScriptTurn::text("a"), ScriptTurn::text("b")],
        FakeOpts::default(),
    )
    .unwrap();
    exchange(&solo.url(), "POST", "/v1/chat/completions", &[], &body);
    exchange(&solo.url(), "POST", "/v1/chat/completions", &[], &body);
    assert_eq!(solo.max_in_flight(), 1);
    let t = solo.timings();
    assert_eq!(t.len(), 2);
    assert!(t[0].end_ms <= t[1].start_ms, "{t:?}");
    assert_eq!(t[0].seq, Some(0));
    assert_eq!(t[1].max_tokens, Some(8));
    assert!(!t[0].client_closed);
}

/// ENG-002: an H6 request held by `hold_prefill_ms` ends as soon as the client closes it.
#[test]
fn held_prefill_notices_a_client_cancel() {
    let srv = FakeServer::start(
        Vec::new(),
        FakeOpts {
            hold_prefill_ms: 5000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut s = connect(&srv.url());
    send(
        &mut s,
        "POST",
        "/v1/chat/completions",
        &[],
        &chat_body("fake-model", &[WireMsg::user("hi")], 0, ""),
    );
    std::thread::sleep(Duration::from_millis(200));
    let began = Instant::now();
    drop(s);
    while srv.timings().is_empty() && began.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let t = srv.timings();
    assert_eq!(t.len(), 1, "the held prefill ended when the client went away");
    assert!(t[0].client_closed);
    assert_eq!(t[0].max_tokens, Some(0));
    assert!(t[0].end_ms - t[0].start_ms < 2000, "{t:?}");
}

/// SET-051: `stall` sends part of the reply, then is silent, then completes.
#[test]
fn stall_is_silent_mid_stream() {
    let srv = FakeServer::start(
        vec![ScriptTurn {
            fault: Some(Fault::Stall { bytes: 20, ms: 600 }),
            ..ScriptTurn::text("a stalled reply")
        }],
        FakeOpts::default(),
    )
    .unwrap();
    let mut s = connect(&srv.url());
    send(
        &mut s,
        "POST",
        "/v1/chat/completions",
        &[],
        &chat_body("fake-model", &[WireMsg::user("hi")], 64, ""),
    );
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    // Read the head and the first part.
    let start = Instant::now();
    let mut first_gap = None;
    let mut last = Instant::now();
    loop {
        let n = s.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        let gap = last.elapsed();
        if !got.is_empty() && gap >= Duration::from_millis(400) {
            first_gap = Some(gap);
        }
        last = Instant::now();
        got.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&got);
    assert!(
        first_gap.is_some(),
        "a silent gap of about 600 ms mid-stream: {text}"
    );
    assert!(start.elapsed() >= Duration::from_millis(600));
    assert!(
        text.contains("a stalled reply") && text.contains("[DONE]"),
        "{text}"
    );
    assert!(!text.contains(": queued"), "no comment during the stall");
}

/// `cold`: the request finds nothing held (cached 0).
#[test]
fn cold_fault_reports_nothing_cached() {
    let srv = FakeServer::start(
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::fault(Fault::Cold),
            ScriptTurn::text("three"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let h = [("X-Session-Id", "s1")];
    let m1 = vec![WireMsg::user("hi")];
    let (_, b) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &h,
        &chat_body("fake-model", &m1, 64, ""),
    );
    assert_eq!(usage_of(&b)["prompt_tokens_details"]["cached_tokens"], json!(0));
    let mut m2 = m1.clone();
    m2.push(WireMsg::assistant("one"));
    m2.push(WireMsg::user("again"));
    let (_, b) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &h,
        &chat_body("fake-model", &m2, 64, ""),
    );
    assert_eq!(
        usage_of(&b)["prompt_tokens_details"]["cached_tokens"],
        json!(0),
        "served cold"
    );
    let mut m3 = m2.clone();
    m3.push(WireMsg::assistant(""));
    m3.push(WireMsg::user("more"));
    let (_, b) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &h,
        &chat_body("fake-model", &m3, 64, ""),
    );
    let u = usage_of(&b);
    assert!(
        u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap() > 0,
        "warm again: {u}"
    );
}

/// Engine F9 (`chat::chat_inputs`): a generating request ending in an assistant message gets a
/// new turn; only `add_generation_prompt: false` on a generating request is a 400; H6 unchanged.
#[test]
fn f9_trailing_assistant_is_history() {
    let srv = FakeServer::start(vec![ScriptTurn::text("a new turn")], FakeOpts::default()).unwrap();
    let msgs = [WireMsg::user("q"), WireMsg::assistant("earlier answer")];
    let (status, body) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &[],
        &chat_body("fake-model", &msgs, 16, ""),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("a new turn"), "{body}");
    let (status, body) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &[],
        &chat_body("fake-model", &msgs, 16, r#","add_generation_prompt":false"#),
    );
    assert_eq!(status, 400);
    assert!(body.contains(oxa_fake::CONTINUATION_400), "{body}");
    let (status, _) = exchange(
        &srv.url(),
        "POST",
        "/v1/chat/completions",
        &[],
        &chat_body("fake-model", &msgs, 0, r#","add_generation_prompt":false"#),
    );
    assert_eq!(status, 200, "H6 of a finished history");
}

/// `/apply-template` + `return_tokens`: `tokens.len()` is exactly the `prompt_tokens` the same
/// body reports as a chat request; without `return_tokens` only `prompt`.
#[test]
fn apply_template_counts_what_a_request_reports() {
    let srv = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).unwrap();
    let msgs = [
        WireMsg::user("count me"),
        WireMsg::assistant("sure"),
        WireMsg::user("é"),
    ];
    let body = chat_body("fake-model", &msgs, 16, "");
    let mut with = body.clone();
    with.pop();
    with.extend_from_slice(br#","return_tokens":true}"#);
    let (status, t) = exchange(&srv.url(), "POST", "/apply-template", &[], &with);
    assert_eq!(status, 200, "{t}");
    let t = json_body(&t);
    let n = t["tokens"].as_array().unwrap().len();
    assert_eq!(n, cache_bytes("sys", "[]", &msgs).len());
    assert_eq!(t["generation_prompt"], json!(""));
    let (_, reply) = exchange(&srv.url(), "POST", "/v1/chat/completions", &[], &body);
    assert_eq!(usage_of(&reply)["prompt_tokens"], json!(n));

    let plain = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, k) = exchange(&srv.url(), "POST", "/apply-template", &[], plain);
    assert_eq!(status, 200, "{k}");
    let k = json_body(&k);
    assert_eq!(k.as_object().unwrap().keys().collect::<Vec<_>>(), ["prompt"]);
}
