//! A14 acceptance tests: Fake engine over HTTP.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::{
    cache_bytes, parse_recorded, parse_script, script_to_text, FakeOpts, FakeServer, Fault, ScriptTurn,
    ScriptedEngine, TestEnv,
};
use oxa_proto::{
    write_json_str, CancelToken, ChatRequest, Engine, Finish, FrozenPrefix, Hints, ToolCall, TurnKind,
    WireMsg,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------- helpers ----------

/// A parsed HTTP answer. `marks` = (time since the request was sent, bytes received so far)
/// for every read, so tests can time particular parts of the stream.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The body ended cleanly (terminating chunk or full Content-Length).
    complete: bool,
    marks: Vec<(Duration, usize)>,
    raw: Vec<u8>,
}

fn exchange(url: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let addr = url.trim_start_matches("http://").to_string();
    let mut s = TcpStream::connect(addr).expect("connect to fake engine");
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
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
    let start = Instant::now();
    let mut raw = Vec::new();
    let mut marks = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&tmp[..n]);
                marks.push((start.elapsed(), raw.len()));
            }
            Err(_) => break,
        }
    }
    parse_reply(raw, marks)
}

fn parse_reply(raw: Vec<u8>, marks: Vec<(Duration, usize)>) -> Reply {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let rest = &raw[split + 4..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"));
    let (body, complete) = if chunked {
        let mut body = Vec::new();
        let mut at = 0usize;
        let mut complete = false;
        while let Some(line_end) = rest[at..].windows(2).position(|w| w == b"\r\n") {
            let size_hex = String::from_utf8_lossy(&rest[at..at + line_end]).to_string();
            let size = usize::from_str_radix(size_hex.trim(), 16).unwrap_or(0);
            let data_start = at + line_end + 2;
            if size == 0 {
                complete = true;
                break;
            }
            if rest.len() < data_start + size + 2 {
                break;
            }
            body.extend_from_slice(&rest[data_start..data_start + size]);
            at = data_start + size + 2;
        }
        (body, complete)
    } else {
        let len: Option<usize> = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse().ok());
        let body = rest.to_vec();
        let complete = len.map(|l| l == body.len()).unwrap_or(true);
        (body, complete)
    };
    Reply {
        status,
        headers,
        body,
        complete,
        marks,
        raw,
    }
}

/// Time at which the bytes of `pat` had all arrived, if they did.
fn time_of(r: &Reply, pat: &str) -> Option<Duration> {
    let at = r.raw.windows(pat.len()).position(|w| w == pat.as_bytes())? + pat.len();
    r.marks.iter().find(|(_, n)| *n >= at).map(|(t, _)| *t)
}

/// The SSE data payloads (`[DONE]` included) and comments of a stream body, in order.
fn sse_items(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        for line in block.lines() {
            if let Some(d) = line.strip_prefix("data: ") {
                out.push(d.to_string());
            } else if let Some(c) = line.strip_prefix(": ") {
                out.push(format!(": {c}"));
            }
        }
    }
    out
}

fn data_json(body: &[u8]) -> Vec<Value> {
    sse_items(body)
        .into_iter()
        .filter(|s| !s.starts_with(": ") && s != "[DONE]")
        .map(|s| serde_json::from_str(&s).expect("data event is JSON"))
        .collect()
}

fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![
            ("preamble".to_string(), "You are a test agent.".to_string()),
            ("cwd".to_string(), "/work".to_string()),
        ],
        tools_json: "[{\"type\":\"function\",\"function\":{\"name\":\"read\"}}]".to_string(),
        sha256: [0u8; 32],
    }
}

/// The request body of `/v1/chat/completions` in the exact layout of `oxa_engine::build_body`.
fn chat_body(p: &FrozenPrefix, msgs: &[WireMsg], max_tokens: u32, thinking: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"{\"model\":\"fake-model\",\"messages\":[{\"role\":\"system\",\"content\":");
    write_json_str(&p.system_text(), &mut b);
    b.push(b'}');
    for m in msgs {
        b.push(b',');
        m.write_json(&mut b);
    }
    b.push(b']');
    b.extend_from_slice(b",\"tools\":");
    b.extend_from_slice(p.tools_json.as_bytes());
    b.extend_from_slice(
        format!(
            ",\"stream\":true,\"stream_options\":{{\"include_usage\":true}},\"temperature\":0,\"cache_prompt\":true,\"max_tokens\":{max_tokens},\"chat_template_kwargs\":{{\"preserve_thinking\":true,\"enable_thinking\":{thinking}}}}}"
        )
        .as_bytes(),
    );
    b
}

fn chat_headers<'a>(session: &'a str, key: &'a str) -> Vec<(&'static str, &'a str)> {
    let mut h = vec![("X-Oxe-Session", session), ("X-Oxe-Turn-Kind", "chat")];
    if !key.is_empty() {
        h.push(("Idempotency-Key", key));
    }
    h
}

fn post_chat(url: &str, body: &[u8], session: &str, key: &str) -> Reply {
    let h = chat_headers(session, key);
    exchange(url, "POST", "/v1/chat/completions", &h, body)
}

fn expected_prompt_len(p: &FrozenPrefix, msgs: &[WireMsg]) -> usize {
    cache_bytes(&p.system_text(), &p.tools_json, msgs).len()
}

fn usage_of(body: &[u8]) -> Value {
    data_json(body)
        .into_iter()
        .find_map(|e| e.get("usage").cloned())
        .expect("a usage object in the stream")
}

// ---------- acceptance tests ----------

/// chat SSE, usage with cached_tokens, timings, /tokenize, /v1/models, /_local/status
#[test]
fn implements_engine_contract_m1_m2_m3() {
    let server = FakeServer::start(
        vec![ScriptTurn::text("Hello"), ScriptTurn::text("Again")],
        FakeOpts::default(),
    )
    .unwrap();
    let url = server.url();

    let models = exchange(&url, "GET", "/v1/models", &[], b"");
    assert_eq!(models.status, 200);
    let m: Value = serde_json::from_slice(&models.body).unwrap();
    assert_eq!(m["data"][0]["id"], "fake-model");

    let status = exchange(&url, "GET", "/_local/status", &[], b"");
    let s: Value = serde_json::from_slice(&status.body).unwrap();
    // The real controller's shape (X1): context_size / context.cap, queued_requests, no n_ctx.
    assert_eq!(s["context_size"], 1 << 20);
    assert_eq!(s["context"]["cap"], 1 << 20);
    assert_eq!(s["model"], "fake-model");
    assert_eq!(s["queued_requests"], 0);
    assert!(s.get("n_ctx").is_none() && s.get("queue").is_none() && s.get("max_output").is_none());
    let props = exchange(&url, "GET", "/props", &[], b"");
    let p: Value = serde_json::from_slice(&props.body).unwrap();
    assert_eq!(p["oxe_hints"]["prefill_only"], true);

    let tok = exchange(&url, "POST", "/tokenize", &[], br#"{"content":"abc"}"#);
    assert_eq!(tok.status, 200);
    let t: Value = serde_json::from_slice(&tok.body).unwrap();
    assert_eq!(t["tokens"].as_array().unwrap().len(), 3);

    let p = prefix();
    let msgs1 = vec![WireMsg::user("hi")];
    let r1 = post_chat(&url, &chat_body(&p, &msgs1, 100, false), "s1", "");
    assert_eq!(r1.status, 200);
    assert!(r1
        .headers
        .iter()
        .any(|(k, v)| k == "content-type" && v.starts_with("text/event-stream")));
    assert!(r1.complete);
    let items = sse_items(&r1.body);
    assert_eq!(items.last().unwrap(), "[DONE]");
    let content: String = data_json(&r1.body)
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(content, "Hello");
    let u1 = usage_of(&r1.body);
    let prompt1 = expected_prompt_len(&p, &msgs1);
    assert_eq!(u1["prompt_tokens"], prompt1);
    assert_eq!(u1["prompt_tokens_details"]["cached_tokens"], 0);
    let finish = data_json(&r1.body).last().unwrap()["choices"][0]["finish_reason"].clone();
    assert_eq!(finish, "stop");
    let timings = data_json(&r1.body)
        .iter()
        .find_map(|e| e.get("timings").cloned())
        .unwrap();
    assert_eq!(timings["prompt_n"], prompt1);
    assert_eq!(timings["predicted_n"], u1["completion_tokens"]);

    // M1: the echo of the reply is cached: cached == previous prompt + previous completion - 1
    // (F1: the engine never feeds the last generated token).
    let reply1 = ScriptTurn::text("Hello").message();
    let msgs2 = vec![WireMsg::user("hi"), reply1, WireMsg::user("again")];
    let r2 = post_chat(&url, &chat_body(&p, &msgs2, 100, false), "s1", "");
    let u2 = usage_of(&r2.body);
    let cached2 = u2["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap();
    assert_eq!(
        cached2,
        (prompt1 as u64) + u1["completion_tokens"].as_u64().unwrap() - 1
    );
}

/// same script and requests -> same usage numbers
#[test]
fn cache_model_matches_scripted_engine() {
    // Three chat requests consume a turn; the prefill (max_tokens 0) does not.
    let turns = vec![
        ScriptTurn::text("Hello there.\n"),
        ScriptTurn::text("  padded reply  "),
        ScriptTurn::text("x"),
    ];
    let p = prefix();
    let server = FakeServer::start(turns.clone(), FakeOpts::default()).unwrap();
    let scripted = ScriptedEngine::new(turns);
    let reply1 = ScriptTurn::text("Hello there.\n").message();
    let msgs1 = vec![WireMsg::user("hi")];
    let msgs2 = vec![WireMsg::user("hi"), reply1, WireMsg::user("again")];
    let requests: Vec<(Vec<WireMsg>, u32)> = vec![
        (msgs1.clone(), 64),
        (msgs2.clone(), 64),
        (msgs2.clone(), 0),
        (msgs2.clone(), 64),
    ];
    for (msgs, max_tokens) in requests {
        let cr = ChatRequest {
            output_schema: None,
            prefix: &p,
            messages: &msgs,
            model: "fake-model",
            max_tokens,
            thinking: false,
            thinking_budget: None,
            thinking_level: None,
            preserve_thinking: true,
            sampling: None,
            hints: Hints {
                session: "s",
                idem_key: "",
                kind: TurnKind::Chat,
                background: false,
                snapshot_keep: false,
            },
        };
        let want = scripted
            .chat(&cr, &CancelToken::new(), &mut |_| {})
            .expect("scripted reply");
        let got = post_chat(&server.url(), &chat_body(&p, &msgs, max_tokens, false), "s", "");
        assert_eq!(got.status, 200);
        let u = usage_of(&got.body);
        assert_eq!(
            u["prompt_tokens"], want.usage.prompt,
            "prompt, max_tokens {max_tokens}"
        );
        assert_eq!(
            u["completion_tokens"], want.usage.completion,
            "completion, max_tokens {max_tokens}"
        );
        assert_eq!(
            u["prompt_tokens_details"]["cached_tokens"], want.usage.cached,
            "cached, max_tokens {max_tokens}"
        );
        let t = want.timings.unwrap();
        let wire_t = data_json(&got.body)
            .iter()
            .find_map(|e| e.get("timings").cloned())
            .unwrap();
        assert_eq!(wire_t["cache_n"], t.cache_n);
        assert_eq!(wire_t["prompt_n"], t.prompt_n);
        assert_eq!(wire_t["predicted_n"], t.predicted_n);
    }
}

/// `: prefill done/total secs` (F2: the engine's exact form)
#[test]
fn prefill_comments_emitted() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 0, false), "s", "");
    assert_eq!(r.status, 200);
    let n = expected_prompt_len(&p, &msgs);
    let want = format!(": prefill {n}/{n} 0s");
    assert!(
        sse_items(&r.body).contains(&want),
        "items: {:?}",
        sse_items(&r.body)
    );
}

/// same key + same body replays without consuming a turn
#[test]
fn h2_idempotent_replay() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let server = FakeServer::start(vec![ScriptTurn::text("once")], FakeOpts::default()).unwrap();
    let url = server.url();
    let body = chat_body(&p, &msgs, 64, false);

    let first = post_chat(&url, &body, "s", "key-1");
    let second = post_chat(&url, &body, "s", "key-1");
    assert_eq!(first.status, 200);
    assert_eq!(second.status, 200);
    assert_eq!(first.body, second.body, "the replay is the same bytes");
    assert_eq!(server.requests().len(), 1, "the replay is not a new request");

    // Same key, other body: refused, nothing consumed.
    let other = chat_body(&p, &[WireMsg::user("different")], 64, false);
    let conflict = post_chat(&url, &other, "s", "key-1");
    assert_eq!(conflict.status, 409);

    // A new key with the same body is a new request; the script is exhausted.
    let fresh = post_chat(&url, &body, "s", "key-2");
    assert_eq!(fresh.status, 422);
    assert!(String::from_utf8_lossy(&fresh.body).contains("script exhausted"));
}

/// `max_tokens: 0` = prefill only: usage with completion 0, no record reply, no turn consumed
#[test]
fn h6_max_tokens_zero_no_turn() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let server = FakeServer::start(vec![ScriptTurn::text("after prefill")], FakeOpts::default()).unwrap();
    let url = server.url();

    let r = post_chat(&url, &chat_body(&p, &msgs, 0, false), "s", "");
    assert_eq!(r.status, 200);
    let u = usage_of(&r.body);
    assert_eq!(u["completion_tokens"], 0);
    assert!(!String::from_utf8_lossy(&r.body).contains("after prefill"));

    let recs = server.requests();
    assert_eq!(recs.len(), 1);
    assert!(recs[0].reply.is_none());
    assert!(recs[0].usage.is_some());

    // The one scripted turn is still there for the next real request.
    let next = post_chat(&url, &chat_body(&p, &msgs, 64, false), "s", "");
    let text: String = data_json(&next.body)
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, "after prefill");
}

/// pause before [DONE]
#[test]
fn done_gap_ms() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let opts = FakeOpts {
        done_gap_ms: 400,
        ..FakeOpts::default()
    };
    let server = FakeServer::start(vec![ScriptTurn::text("gap")], opts).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 64, false), "s", "");
    assert!(r.complete);
    let t_finish = time_of(&r, "\"finish_reason\":\"stop\"").expect("finish chunk");
    let t_done = time_of(&r, "data: [DONE]").expect("[DONE]");
    assert!(
        t_done >= t_finish + Duration::from_millis(300),
        "gap was {:?}",
        t_done.checked_sub(t_finish)
    );
}

/// delayed headers
#[test]
fn hold_headers() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let opts = FakeOpts {
        hold_headers_ms: 400,
        ..FakeOpts::default()
    };
    let server = FakeServer::start(vec![ScriptTurn::text("late")], opts).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 64, false), "s", "");
    assert_eq!(r.status, 200);
    let first = r.marks.first().expect("some bytes").0;
    assert!(first >= Duration::from_millis(350), "first byte after {first:?}");
}

/// comment-only period then reply
#[test]
fn comments_only() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let opts = FakeOpts {
        comments_only_ms: 1500,
        ..FakeOpts::default()
    };
    let server = FakeServer::start(vec![ScriptTurn::text("Hi")], opts).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 64, false), "s", "");
    assert_eq!(r.status, 200);
    let items = sse_items(&r.body);
    // F10: the engine's unified line, `: queued wait_s=<s>`.
    let queued = items.iter().filter(|s| s.starts_with(": queued wait_s=")).count();
    assert!(queued >= 2, "queued comments: {queued}");
    let first_content = items
        .iter()
        .position(|s| s.contains("\"content\":\"Hi\""))
        .expect("reply");
    let last_queued = items.iter().rposition(|s| s.starts_with(": queued")).unwrap();
    assert!(last_queued < first_content, "comments come before the reply");
    assert!(time_of(&r, "\"content\":\"Hi\"").unwrap() >= Duration::from_millis(1400));
}

/// connection closed mid-stream
#[test]
fn drop_at() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let cut = ScriptTurn {
        fault: Some(Fault::DropAt { bytes: 40 }),
        ..ScriptTurn::text("this reply is cut and must never reach the wire")
    };
    let server = FakeServer::start(vec![cut], FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 64, false), "s", "");
    assert_eq!(r.status, 200);
    assert!(!r.complete, "no terminating chunk");
    assert_eq!(r.body.len(), 40, "exactly the cut bytes of SSE body");
    assert!(!String::from_utf8_lossy(&r.body).contains("[DONE]"));
    let recs = server.requests();
    assert_eq!(recs.len(), 1);
    assert!(
        recs[0].reply.is_none(),
        "a cut reply is never recorded as a reply"
    );
}

/// exceed_context_size_error body
#[test]
fn overflow_400() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let opts = FakeOpts {
        n_ctx: 50,
        ..FakeOpts::default()
    };
    let server = FakeServer::start(vec![ScriptTurn::text("never")], opts).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 10, false), "s", "");
    assert_eq!(r.status, 400);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["error"]["type"], "exceed_context_size_error");
    assert_eq!(server.requests().len(), 1);
}

/// parse(script_to_text(x)) == x; bad line reports its number
#[test]
fn script_roundtrip() {
    let turns = vec![
        ScriptTurn::text("a\nb \"q\" \u{00fc}n\u{00ef} \\ end"),
        ScriptTurn {
            reasoning: Some("think".to_string()),
            text: Some(String::new()),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                args_raw: "{ \"path\" :  \"x\" }".to_string(),
            }],
            finish: Finish::ToolCalls,
            completion_tokens: Some(7),
            chunk: 3,
            fault: None,
        },
        ScriptTurn {
            finish: Finish::Length,
            ..ScriptTurn::text("cut off")
        },
        ScriptTurn::fault(Fault::DropAt { bytes: 40 }),
        ScriptTurn::fault(Fault::Http {
            status: 503,
            body: "busy {}".to_string(),
        }),
        ScriptTurn::fault(Fault::Overflow),
        ScriptTurn::fault(Fault::Refuse),
        ScriptTurn::fault(Fault::HoldHeaders { ms: 5 }),
        ScriptTurn::fault(Fault::CommentsOnly { ms: 6 }),
        ScriptTurn {
            fault: Some(Fault::EarlyClose),
            ..ScriptTurn::text("done")
        },
    ];
    let text = script_to_text(&turns);
    assert_eq!(parse_script(&text).unwrap(), turns);

    let bad = "{\"text\":\"a\"}\n{\"finish\":\"maybe\"}\n";
    let err = parse_script(bad).unwrap_err();
    assert!(err.message.contains("line 2"), "{}", err.message);
}

/// parse_recorded
#[test]
fn record_keeps_raw_tools_and_args() {
    let tools =
        "[{ \"type\": \"function\",  \"function\": {\"name\":\"read\",\"parameters\":{\"b\":1,\"a\":2}}}]";
    let p = FrozenPrefix {
        sections: vec![("preamble".to_string(), "sys".to_string())],
        tools_json: tools.to_string(),
        sha256: [0u8; 32],
    };
    let asst = WireMsg {
        images: Vec::new(),
        role: oxa_proto::Role::Assistant,
        content: Some("  padded  ".to_string()),
        reasoning: Some(String::new()),
        tool_calls: vec![ToolCall {
            id: "c1".to_string(),
            name: "read".to_string(),
            args_raw: "{ \"path\" :  \"a\" }".to_string(),
        }],
        provider_history: None,
        tool_call_id: None,
    };
    let msgs = vec![
        WireMsg::user("u"),
        asst.clone(),
        WireMsg::tool_result("c1", "out"),
    ];
    let body = chat_body(&p, &msgs, 16, true);
    let headers = vec![
        ("X-Oxe-Session".to_string(), "sess".to_string()),
        ("Idempotency-Key".to_string(), "k".to_string()),
        ("X-Oxe-Turn-Kind".to_string(), "compaction".to_string()),
        ("X-Oxe-Priority".to_string(), "background".to_string()),
        ("X-Oxe-Snapshot".to_string(), "keep".to_string()),
    ];
    let rec = parse_recorded(3, &headers, &body).unwrap();
    assert_eq!(rec.seq, 3);
    assert_eq!(rec.tools_json, tools, "tools are the raw substring");
    assert_eq!(rec.system, "sys");
    assert_eq!(rec.messages, msgs, "messages decode to the same wire values");
    assert_eq!(rec.max_tokens, 16);
    assert!(rec.thinking);
    assert_eq!(rec.session, "sess");
    assert_eq!(rec.idem_key, "k");
    assert_eq!(rec.kind, "compaction");
    assert!(rec.background);
    assert!(rec.snapshot_keep);
    // The args string and the padded content come back untouched.
    assert_eq!(rec.messages[1].tool_calls[0].args_raw, "{ \"path\" :  \"a\" }");
    assert_eq!(rec.messages[1].content.as_deref(), Some("  padded  "));
    assert_eq!(rec.messages[1].reasoning.as_deref(), Some(""));
}

// ---------- further tests ----------

/// A `--drop-at` set on the server cuts only the first chat request.
#[test]
fn global_drop_at_hits_first_request_only() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let opts = FakeOpts {
        drop_at: Some(30),
        ..FakeOpts::default()
    };
    let server =
        FakeServer::start(vec![ScriptTurn::text("first"), ScriptTurn::text("second")], opts).unwrap();
    let url = server.url();
    let body = chat_body(&p, &msgs, 64, false);
    let a = post_chat(&url, &body, "s", "");
    assert!(!a.complete);
    assert_eq!(a.body.len(), 30);
    let b = post_chat(&url, &body, "s", "");
    assert!(b.complete);
    assert!(String::from_utf8_lossy(&b.body).contains("[DONE]"));
}

/// `early_close`: the reply is complete through [DONE]; only the terminating chunk is missing.
#[test]
fn early_close_after_done() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let turn = ScriptTurn {
        fault: Some(Fault::EarlyClose),
        ..ScriptTurn::text("bye")
    };
    let server = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 64, false), "s", "");
    assert!(!r.complete);
    assert_eq!(sse_items(&r.body).last().unwrap(), "[DONE]");
}

/// Tool-call deltas and reasoning stream in order and reassemble to the exact strings.
#[test]
fn tool_call_and_reasoning_deltas_reassemble() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let turn = ScriptTurn {
        reasoning: Some("  think  ".to_string()),
        text: None,
        tool_calls: vec![ToolCall {
            id: "call_9".into(),
            name: "read".into(),
            args_raw: "{\"path\": \"a b\"}".into(),
        }],
        finish: Finish::ToolCalls,
        completion_tokens: None,
        chunk: 4,
        fault: None,
    };
    let server = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    // F6: the fake bounds the reply by `max_tokens` (one token = one byte), so leave room.
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096, true), "s", "");
    assert!(r.complete);
    let mut reasoning = String::new();
    let mut args = String::new();
    let mut id = String::new();
    let mut name = String::new();
    for e in data_json(&r.body) {
        let d = &e["choices"][0]["delta"];
        if let Some(s) = d["reasoning_content"].as_str() {
            reasoning.push_str(s);
        }
        if let Some(calls) = d["tool_calls"].as_array() {
            for c in calls {
                if let Some(i) = c["id"].as_str() {
                    id = i.to_string();
                }
                if let Some(n) = c["function"]["name"].as_str() {
                    name = n.to_string();
                }
                if let Some(a) = c["function"]["arguments"].as_str() {
                    args.push_str(a);
                }
            }
        }
    }
    assert_eq!(reasoning, "  think  ");
    assert_eq!(id, "call_9");
    assert_eq!(name, "read");
    assert_eq!(args, "{\"path\": \"a b\"}");
    assert_eq!(
        server.requests()[0].reply.as_ref().unwrap().tool_calls[0].args_raw,
        "{\"path\": \"a b\"}"
    );
}

/// The `oxa-fake` binary serves, records each request to the record file and stops when stdin closes.
#[test]
fn binary_serves_and_records() {
    let env = TestEnv::new();
    let script = env.write("script.jsonl", "{\"text\":\"from binary\"}\n");
    let record = env.work().join("requests.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa-fake"))
        .args([
            "--script",
            script.to_str().unwrap(),
            "--port",
            "0",
            "--record",
            record.to_str().unwrap(),
        ])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxa-fake");
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    let url = line
        .trim()
        .strip_prefix("listening ")
        .expect("listening line")
        .to_string();

    let p = prefix();
    let r = post_chat(&url, &chat_body(&p, &[WireMsg::user("hi")], 64, false), "s", "");
    assert_eq!(r.status, 200);
    assert!(String::from_utf8_lossy(&r.body).contains("from binary"));

    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert!(status.success());
    let log = std::fs::read_to_string(&record).unwrap();
    assert_eq!(log.lines().count(), 1);
    assert!(log.contains("\"seq\":0"));
}
