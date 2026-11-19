//! Fit pass (post wave E): the fake engine matches the real engine and controller where the
//! review `FIT-REVIEW-OXA-ENGINE` found it did not (F1, F2, F3, F4, F6, F7, F10, F13, F14, EG5,
//! H4, H6). Each test names its finding. Raw HTTP on 127.0.0.1 only; the scripted engine in
//! process.

use oxa_fake::{
    cache_bytes, parse_script, script_to_text, FakeCaps, FakeModel, FakeOpts, FakeServer, Fault, ScriptTurn,
    ScriptedEngine,
};
use oxa_proto::{
    codes, write_json_str, CancelToken, ChatRequest, Engine, Finish, FrozenPrefix, Hints, StreamEvent,
    ToolCall, TurnKind, WireMsg,
};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// ---------- helpers ----------

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The chunked body ended with its terminating chunk.
    complete: bool,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn exchange(url: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let addr = url.trim_start_matches("http://").to_string();
    let mut s = TcpStream::connect(addr).expect("connect to the fake engine");
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
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
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
    if !chunked {
        return Reply {
            status,
            headers,
            body: rest.to_vec(),
            complete: true,
        };
    }
    let (mut body, mut at, mut complete) = (Vec::new(), 0usize, false);
    while let Some(end) = rest[at..].windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&rest[at..at + end]).trim(), 16).unwrap_or(0);
        let start = at + end + 2;
        if size == 0 {
            complete = true;
            break;
        }
        if rest.len() < start + size + 2 {
            break;
        }
        body.extend_from_slice(&rest[start..start + size]);
        at = start + size + 2;
    }
    Reply {
        status,
        headers,
        body,
        complete,
    }
}

/// The SSE data payloads (`[DONE]` included) and comments (`: ...`), in order.
fn sse_items(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("data: ") {
            out.push(d.to_string());
        } else if line.starts_with(": ") || line.starts_with("event:") {
            out.push(line.to_string());
        }
    }
    out
}

fn data_json(body: &[u8]) -> Vec<Value> {
    sse_items(body)
        .into_iter()
        .filter(|s| !s.starts_with(':') && !s.starts_with("event:") && s != "[DONE]")
        .map(|s| serde_json::from_str(&s).expect("a data event is JSON"))
        .collect()
}

fn usage_of(body: &[u8]) -> Value {
    data_json(body)
        .into_iter()
        .find_map(|e| e.get("usage").cloned())
        .expect("a usage object")
}

fn timings_of(body: &[u8]) -> Value {
    data_json(body)
        .into_iter()
        .find_map(|e| e.get("timings").cloned())
        .expect("a timings object")
}

fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), "You are a test agent.".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    }
}

/// The body in `oxa_engine::build_body`'s layout (no `tools`, `[]` prefix tools).
fn chat_body(p: &FrozenPrefix, msgs: &[WireMsg], max_tokens: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"{\"model\":\"fake-model\",\"messages\":[{\"role\":\"system\",\"content\":");
    write_json_str(&p.system_text(), &mut b);
    b.push(b'}');
    for m in msgs {
        b.push(b',');
        m.write_json(&mut b);
    }
    b.extend_from_slice(
        format!(
            "],\"stream\":true,\"stream_options\":{{\"include_usage\":true}},\"temperature\":0,\"cache_prompt\":true,\"max_tokens\":{max_tokens},\"chat_template_kwargs\":{{\"enable_thinking\":false}}}}"
        )
        .as_bytes(),
    );
    b
}

fn post_chat(url: &str, body: &[u8], session: &str, snapshot: bool) -> Reply {
    let mut h = vec![("X-Oxe-Session", session), ("X-Oxe-Turn-Kind", "chat")];
    if snapshot {
        h.push(("X-Oxe-Snapshot", "keep"));
    }
    exchange(url, "POST", "/v1/chat/completions", &h, body)
}

fn prompt_len(p: &FrozenPrefix, msgs: &[WireMsg]) -> usize {
    cache_bytes(&p.system_text(), &p.tools_json, msgs).len()
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

fn request<'a>(p: &'a FrozenPrefix, msgs: &'a [WireMsg], max_tokens: u32, h: Hints<'a>) -> ChatRequest<'a> {
    ChatRequest {
        output_schema: None,
        prefix: p,
        messages: msgs,
        model: "fake-model",
        max_tokens,
        thinking: false,
        thinking_budget: None,
        thinking_level: None,
        preserve_thinking: false,
        sampling: None,
        hints: h,
    }
}

fn clear(url: &str, path: &str, body: Value) -> Reply {
    exchange(
        url,
        "POST",
        path,
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    )
}

// ---------- F1 ----------

/// F1: the fake holds the prompt plus the reply minus its last token, so the next request's
/// `cached` is `prompt + completion - 1` (the engine's `tokens_cached`), over HTTP and in process.
#[test]
fn f1_cached_is_prompt_plus_completion_minus_one() {
    let p = prefix();
    let server = FakeServer::start(
        vec![ScriptTurn::text("Hello"), ScriptTurn::text("ok")],
        FakeOpts::default(),
    )
    .unwrap();
    let m1 = vec![WireMsg::user("hi")];
    let r1 = post_chat(&server.url(), &chat_body(&p, &m1, 4096), "s", false);
    let u1 = usage_of(&r1.body);
    let (prompt1, completion1) = (
        u1["prompt_tokens"].as_u64().unwrap(),
        u1["completion_tokens"].as_u64().unwrap(),
    );
    assert_eq!(server.held("s").unwrap().len() as u64, prompt1 + completion1 - 1);
    let m2 = vec![
        WireMsg::user("hi"),
        ScriptTurn::text("Hello").message(),
        WireMsg::user("more"),
    ];
    let r2 = post_chat(&server.url(), &chat_body(&p, &m2, 4096), "s", false);
    assert_eq!(
        usage_of(&r2.body)["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap(),
        prompt1 + completion1 - 1
    );

    let eng = ScriptedEngine::new(vec![ScriptTurn::text("Hello"), ScriptTurn::text("ok")]);
    let t1 = eng
        .chat(
            &request(&p, &m1, 4096, hints("s")),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let t2 = eng
        .chat(
            &request(&p, &m2, 4096, hints("s")),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(t2.usage.cached, t1.usage.prompt + t1.usage.completion - 1);
    assert_eq!(
        t1.usage,
        oxa_proto::Usage {
            prompt: prompt1 as u32,
            completion: completion1 as u32,
            cached: 0,
            cached_missing: false,
            unreported: false,
        }
    );
}

// ---------- F2 / F7 / H6 / H4 ----------

/// F7 / H6 / F2: `max_tokens: 0` streams the engine's exact shape: `: prefill N/N 0s`, the role
/// chunk, the finish chunk with `length`, a separate usage chunk (`choices: []`) with timings,
/// `[DONE]`; no content delta; the fake holds exactly the prompt. H4: `snap_pos ==
/// prompt_tokens` with the hint.
#[test]
fn f7_h6_stream_shape_matches_the_engine() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 0), "s", true);
    assert_eq!(r.status, 200);
    let n = prompt_len(&p, &msgs);
    let items = sse_items(&r.body);
    assert_eq!(items[0], format!(": prefill {n}/{n} 0s"));
    let data = data_json(&r.body);
    assert_eq!(data.len(), 3, "{items:?}");
    assert_eq!(data[0]["choices"][0]["delta"]["role"], "assistant");
    assert!(data[0]["choices"][0]["delta"]["content"].is_null());
    assert_eq!(data[1]["choices"][0]["finish_reason"], "length");
    assert!(data[1].get("usage").is_none());
    assert_eq!(data[2]["choices"], json!([]));
    assert_eq!(data[2]["usage"]["completion_tokens"], 0);
    assert_eq!(data[2]["usage"]["prompt_tokens"], n);
    assert_eq!(data[2]["timings"]["predicted_n"], 0);
    assert_eq!(
        data[2]["timings"]["snap_pos"], n,
        "H4 + H6: snap_pos == prompt_tokens"
    );
    assert_eq!(items.last().unwrap(), "[DONE]");
    assert!(data
        .iter()
        .all(|d| d["choices"][0]["delta"].get("content").is_none_or(Value::is_null)));
    assert_eq!(
        server.held("s").unwrap().len(),
        n,
        "nothing beyond the prompt is held"
    );

    // The scripted engine: the same finish.
    let eng = ScriptedEngine::new(vec![]);
    let t = eng
        .chat(
            &request(&p, &msgs, 0, hints("s")),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(t.finish, Finish::Length);
    assert_eq!(t.usage.completion, 0);
    assert_eq!(eng.held("s").unwrap().len(), n);
}

/// H4: `timings.snap_pos` only when `X-Oxe-Snapshot: keep` was sent; on a generating request it
/// is the end of the last message (the prompt end in the fake); `snap_null` makes it null with
/// a `snap_reason`.
#[test]
fn h4_snap_pos_only_with_the_hint_and_scriptable_null() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let script = vec![
        ScriptTurn::text("a"),
        ScriptTurn::text("b"),
        ScriptTurn::fault(Fault::SnapNull {
            reason: "inside_reused_prefix".into(),
        }),
    ];
    let server = FakeServer::start(script.clone(), FakeOpts::default()).unwrap();
    let url = server.url();
    let t = timings_of(&post_chat(&url, &chat_body(&p, &msgs, 4096), "s1", false).body);
    assert!(t.get("snap_pos").is_none(), "no hint, no key: {t}");
    let t = timings_of(&post_chat(&url, &chat_body(&p, &msgs, 4096), "s2", true).body);
    assert_eq!(t["snap_pos"], prompt_len(&p, &msgs));
    let t = timings_of(&post_chat(&url, &chat_body(&p, &msgs, 4096), "s3", true).body);
    assert!(t["snap_pos"].is_null(), "{t}");
    assert_eq!(t["snap_reason"], "inside_reused_prefix");

    let eng = ScriptedEngine::new(script);
    let mut h = hints("s");
    let t = eng
        .chat(&request(&p, &msgs, 4096, h), &CancelToken::new(), &mut |_| {})
        .unwrap();
    let tm = t.timings.unwrap();
    assert!(!tm.snap_asked && tm.snap_pos.is_none());
    h.snapshot_keep = true;
    let tm = eng
        .chat(&request(&p, &msgs, 4096, h), &CancelToken::new(), &mut |_| {})
        .unwrap()
        .timings
        .unwrap();
    assert_eq!(tm.snap_pos, Some(prompt_len(&p, &msgs) as u32));
    let tm = eng
        .chat(&request(&p, &msgs, 4096, h), &CancelToken::new(), &mut |_| {})
        .unwrap()
        .timings
        .unwrap();
    assert!(tm.snap_asked && tm.snap_pos.is_none());
}

// ---------- F6 ----------

/// F6: 400 `exceed_context_size_error` only when `prompt >= n_ctx`; below it the reply is cut
/// when the context is full (`finish_reason: "length"`, `completion = n_ctx - prompt`), and
/// `max_tokens` bounds it the same way.
#[test]
fn f6_overflow_only_when_the_prompt_does_not_fit() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let n = prompt_len(&p, &msgs) as u32;
    let long = "x".repeat(200);

    // prompt == n_ctx: refused.
    let server = FakeServer::start(
        vec![ScriptTurn::text(&long)],
        FakeOpts {
            n_ctx: n,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["type"], "exceed_context_size_error");

    // prompt + max_tokens > n_ctx but prompt < n_ctx: answered, cut where the context fills.
    let server = FakeServer::start(
        vec![ScriptTurn::text(&long), ScriptTurn::text(&long)],
        FakeOpts {
            n_ctx: n + 50,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    assert_eq!(r.status, 200);
    let fin = data_json(&r.body)
        .into_iter()
        .find_map(|e| e["choices"][0]["finish_reason"].as_str().map(str::to_string))
        .unwrap();
    assert_eq!(fin, "length");
    let u = usage_of(&r.body);
    assert_eq!(u["completion_tokens"], 50);
    assert_eq!(u["prompt_tokens"].as_u64().unwrap() + 50, u64::from(n + 50));
    let content: String = data_json(&r.body)
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(content, "x".repeat(50));

    // max_tokens bounds the reply too.
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 7), "t", false);
    assert_eq!(usage_of(&r.body)["completion_tokens"], 7);

    // The scripted engine follows the same rule.
    let eng = ScriptedEngine::new(vec![ScriptTurn::text(&long)]).with_limits(n + 50, 4096);
    let t = eng
        .chat(
            &request(&p, &msgs, 4096, hints("s")),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(t.finish, Finish::Length);
    assert_eq!(t.usage.completion, 50);
    let eng = ScriptedEngine::new(vec![ScriptTurn::text("x")]).with_limits(n, 4096);
    let e = eng
        .chat(
            &request(&p, &msgs, 1, hints("s")),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(e.code, codes::CONTEXT_OVERFLOW);
}

// ---------- F13 ----------

/// F13: a re-sent assistant tool call whose `arguments` is not JSON is a 400, as on the
/// engine; `strict_tool_args: false` turns the check off.
#[test]
fn f13_bad_tool_arguments_are_refused() {
    let p = prefix();
    let call = WireMsg {
        images: Vec::new(),
        role: oxa_proto::Role::Assistant,
        content: None,
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args_raw: "{\"path\": ".into(),
        }],
        provider_history: None,
        tool_call_id: None,
    };
    let msgs = vec![
        WireMsg::user("go"),
        call,
        WireMsg::tool_result("c1", "error: bad JSON"),
    ];
    let server = FakeServer::start(vec![ScriptTurn::text("never")], FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    assert_eq!(r.status, 400);
    assert!(
        r.text().contains("Failed to parse tool call arguments as JSON"),
        "{}",
        r.text()
    );

    let server = FakeServer::start(
        vec![ScriptTurn::text("fine")],
        FakeOpts {
            strict_tool_args: false,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    assert_eq!(r.status, 200);
}

// ---------- S2 / F10 / F3 ----------

/// S2 / F10: `queued` comments in the unified format before the reply: the controller's
/// `pos=<n> wait_s=<s>` (positions count down) or the engine's `wait_s=<s>`; the scripted
/// engine emits the same `Queued` events.
#[test]
fn f10_queued_comments_in_the_unified_format() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let script = vec![
        ScriptTurn {
            fault: Some(Fault::Queued { count: 3, pos: true }),
            ..ScriptTurn::text("ok")
        },
        ScriptTurn {
            fault: Some(Fault::Queued { count: 2, pos: false }),
            ..ScriptTurn::text("ok")
        },
    ];
    let server = FakeServer::start(script.clone(), FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    let items = sse_items(&r.body);
    assert_eq!(
        &items[..3],
        &[
            ": queued pos=3 wait_s=0",
            ": queued pos=2 wait_s=1",
            ": queued pos=1 wait_s=2"
        ]
    );
    assert_eq!(items.last().unwrap(), "[DONE]");
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "t", false);
    let items = sse_items(&r.body);
    assert_eq!(&items[..2], &[": queued wait_s=0", ": queued wait_s=1"]);

    let eng = ScriptedEngine::new(script);
    let mut ev = Vec::new();
    eng.chat(
        &request(&p, &msgs, 4096, hints("s")),
        &CancelToken::new(),
        &mut |e| ev.push(e),
    )
    .unwrap();
    assert_eq!(
        ev[0],
        StreamEvent::Queued {
            pos: Some(3),
            wait_s: 0
        }
    );
    assert_eq!(
        ev[2],
        StreamEvent::Queued {
            pos: Some(1),
            wait_s: 2
        }
    );
    assert_eq!(ev[3], StreamEvent::Text("ok".into()));
}

/// F3: after early headers the controller sends `queued` comments, then `event: error` +
/// `data: {"error":{..,"code":<status>}}` and ends the chunked body without `[DONE]`; the
/// engine's own form is a plain `data: {"error":..}`.
#[test]
fn f3_error_event_after_early_headers() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let script = vec![
        ScriptTurn::fault(Fault::StreamError {
            queued: 2,
            status: 400,
            error_type: "invalid_request_error".into(),
            event: true,
        }),
        ScriptTurn::fault(Fault::StreamError {
            queued: 0,
            status: 500,
            error_type: "server_error".into(),
            event: false,
        }),
    ];
    let server = FakeServer::start(script, FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    assert_eq!(r.status, 200);
    assert!(r.complete, "the chunked body ends cleanly");
    let items = sse_items(&r.body);
    assert_eq!(items[0], ": queued pos=2 wait_s=0");
    assert_eq!(items[2], "event: error");
    let err: Value = serde_json::from_str(&items[3]).unwrap();
    assert_eq!(err["error"]["code"], 400);
    assert_eq!(err["error"]["type"], "invalid_request_error");
    assert!(!items.contains(&"[DONE]".to_string()));

    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    let items = sse_items(&r.body);
    assert_eq!(items.len(), 1, "{items:?}");
    let err: Value = serde_json::from_str(&items[0]).unwrap();
    assert_eq!(err["error"]["code"], 500);
}

/// F3 / S2: the controller's queue-full reply: 503 + `Retry-After: 30`, type
/// `local_backend_busy`; the scripted engine answers `engine_busy` naming `HTTP 503` and the
/// wait.
#[test]
fn f3_busy_503_with_retry_after() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let script = vec![
        ScriptTurn::fault(Fault::Busy {
            retry_after: Some(30),
        }),
        ScriptTurn::fault(Fault::Busy { retry_after: None }),
    ];
    let server = FakeServer::start(script.clone(), FakeOpts::default()).unwrap();
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    assert_eq!(r.status, 503);
    assert_eq!(r.header("retry-after"), Some("30"));
    assert_eq!(r.json()["error"]["type"], "local_backend_busy");
    let r = post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", false);
    assert_eq!(r.status, 503);
    assert_eq!(r.header("retry-after"), None);

    let eng = ScriptedEngine::new(script);
    let e = eng
        .chat(
            &request(&p, &msgs, 4096, hints("s")),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(e.code, codes::ENGINE_BUSY);
    assert!(
        e.message.contains("HTTP 503") && e.message.contains("retry after 30 s"),
        "{e}"
    );
}

/// Script format: the new faults round-trip and have their documented defaults.
#[test]
fn new_faults_roundtrip_through_the_script_format() {
    let turns = vec![
        ScriptTurn::fault(Fault::Queued { count: 2, pos: false }),
        ScriptTurn::fault(Fault::StreamError {
            queued: 1,
            status: 503,
            error_type: "unavailable_error".into(),
            event: true,
        }),
        ScriptTurn::fault(Fault::Busy { retry_after: None }),
        ScriptTurn::fault(Fault::Busy { retry_after: Some(7) }),
        ScriptTurn::fault(Fault::SnapNull {
            reason: "no_slot".into(),
        }),
    ];
    assert_eq!(parse_script(&script_to_text(&turns)).unwrap(), turns);
    let d = parse_script(
        "{\"fault\":{\"kind\":\"queued\"}}\n{\"fault\":{\"kind\":\"stream_error\",\"status\":400}}\n{\"fault\":{\"kind\":\"busy\"}}\n",
    )
    .unwrap();
    assert_eq!(d[0].fault, Some(Fault::Queued { count: 1, pos: true }));
    assert_eq!(
        d[1].fault,
        Some(Fault::StreamError {
            queued: 1,
            status: 400,
            error_type: "invalid_request_error".into(),
            event: true
        })
    );
    assert_eq!(
        d[2].fault,
        Some(Fault::Busy {
            retry_after: Some(30)
        })
    );
}

// ---------- F4 / F14 ----------

/// F4: `/_local/status` publishes `model_limits[id].capabilities` per model before any load
/// (here a Flash row: cap 16384, max_output 2048, `preserve_thinking: false`, `park: "disk"`);
/// `/props` can answer 503 "no model loaded"; the default row carries the Rust MoE block.
#[test]
fn f4_model_limits_capabilities_and_props_503() {
    let opts = FakeOpts {
        models: vec![
            FakeModel::moe("fake-model", 32768),
            FakeModel::flash("flash", 16384, Some(2048)),
        ],
        props_unloaded: true,
        ..FakeOpts::default()
    };
    let server = FakeServer::start(vec![], opts).unwrap();
    let st = exchange(&server.url(), "GET", "/_local/status", &[], b"").json();
    let flash = &st["model_limits"]["flash"];
    assert_eq!(flash["cap"], 16384);
    assert_eq!(flash["max_output"], 2048);
    assert_eq!(flash["capabilities"]["preserve_thinking"], false);
    assert_eq!(flash["capabilities"]["park"], "disk");
    assert_eq!(flash["capabilities"]["snapshot_hint"], 2);
    assert_eq!(
        st["model_limits"]["fake-model"]["capabilities"]["park"],
        "ram+disk"
    );
    let props = exchange(&server.url(), "GET", "/props", &[], b"");
    assert_eq!(props.status, 503);
    server.set_props_unloaded(false);
    let props = exchange(&server.url(), "GET", "/props", &[], b"");
    assert_eq!(props.status, 200);
    assert_eq!(props.json()["oxe_hints"]["hint_clear"], true);

    // The default: one row for the model, the Rust MoE block; without hints no block.
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let st = exchange(&server.url(), "GET", "/_local/status", &[], b"").json();
    assert_eq!(
        st["model_limits"]["fake-model"]["capabilities"]["prefill_only"],
        true
    );
    let server = FakeServer::start(
        vec![],
        FakeOpts {
            hints: false,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let st = exchange(&server.url(), "GET", "/_local/status", &[], b"").json();
    assert!(st["model_limits"]["fake-model"].get("capabilities").is_none());
    assert_eq!(FakeCaps::none().snapshot_hint, 0);
}

/// F14: while the controller loads, `/_local/status` is the short `state: loading` body with no
/// model and no context.
#[test]
fn f14_loading_status_has_no_context() {
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    server.set_loading(true);
    let st = exchange(&server.url(), "GET", "/_local/status", &[], b"").json();
    assert_eq!(st["state"], "loading");
    assert!(st.get("context").is_none() && st.get("model").is_none());
    server.set_loading(false);
    let st = exchange(&server.url(), "GET", "/_local/status", &[], b"").json();
    assert_eq!(st["state"], "ready");
}

// ---------- EG5 ----------

/// EG5: the hint clear on both paths drops only the cleared hint's held state (the parent's
/// survives a child's clear), validates the hint (1-128 of `[A-Za-z0-9._-]`, missing = 400),
/// answers an unknown hint with `sessions: 0`, refuses with 409 while busy and records each
/// accepted clear (`clears()`); a plain engine has only `/state-cache/clear`.
#[test]
fn eg5_hint_clear_drops_only_that_session() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let url = server.url();
    post_chat(&url, &chat_body(&p, &msgs, 0), "parent", false);
    post_chat(&url, &chat_body(&p, &msgs, 0), "parent.s1", false);
    let r = clear(
        &url,
        "/_local/cache/clear",
        json!({"scope": "hint", "hint": "parent.s1"}),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["sessions"], 1);
    assert!(server.held("parent.s1").is_none());
    assert!(server.held("parent").is_some(), "the parent's state survives");
    let r = clear(
        &url,
        "/state-cache/clear",
        json!({"scope": "hint", "hint": "nobody"}),
    );
    assert_eq!((r.status, r.json()["sessions"].clone()), (200, json!(0)));
    for bad in [
        json!({"scope": "hint"}),
        json!({"scope": "hint", "hint": ""}),
        json!({"scope": "hint", "hint": "a/b"}),
        json!({"scope": "hint", "hint": "x".repeat(129)}),
    ] {
        assert_eq!(
            clear(&url, "/_local/cache/clear", bad.clone()).status,
            400,
            "{bad}"
        );
    }
    server.set_clear_busy(true);
    assert_eq!(
        clear(
            &url,
            "/_local/cache/clear",
            json!({"scope": "hint", "hint": "parent"})
        )
        .status,
        409
    );
    server.set_clear_busy(false);
    assert_eq!(
        server.clears(),
        vec!["parent.s1".to_string(), "nobody".to_string()]
    );

    let plain = FakeServer::start(
        vec![],
        FakeOpts {
            controller: false,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    assert_eq!(
        clear(
            &plain.url(),
            "/_local/cache/clear",
            json!({"scope": "hint", "hint": "a"})
        )
        .status,
        404
    );
    assert_eq!(
        clear(
            &plain.url(),
            "/state-cache/clear",
            json!({"scope": "hint", "hint": "a"})
        )
        .status,
        200
    );
    assert_eq!(
        exchange(&plain.url(), "GET", "/_local/status", &[], b"").status,
        404
    );

    // The scripted engine records and drops the same way.
    let eng = ScriptedEngine::new(vec![]);
    eng.chat(
        &request(&p, &msgs, 0, hints("parent")),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap();
    eng.chat(
        &request(&p, &msgs, 0, hints("parent.s1")),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap();
    eng.clear_hint("parent.s1").unwrap();
    assert!(eng.held("parent.s1").is_none() && eng.held("parent").is_some());
    assert_eq!(eng.clears(), vec!["parent.s1".to_string()]);
}

/// F8 / records: each request records `preserve_thinking` as sent (absent = `None`) next to the
/// hint headers.
#[test]
fn records_preserve_thinking_and_hint_headers() {
    let p = prefix();
    let msgs = vec![WireMsg::user("hi")];
    let server = FakeServer::start(vec![ScriptTurn::text("a")], FakeOpts::default()).unwrap();
    post_chat(&server.url(), &chat_body(&p, &msgs, 4096), "s", true);
    let rec = &server.requests()[0];
    assert_eq!(rec.preserve_thinking, None);
    assert_eq!(rec.session, "s");
    assert!(rec.snapshot_keep);
    assert_eq!(rec.kind, "chat");

    let eng = ScriptedEngine::new(vec![ScriptTurn::text("a")]);
    let mut r = request(&p, &msgs, 4096, hints("s"));
    r.preserve_thinking = true;
    eng.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(eng.requests()[0].preserve_thinking, Some(true));
}
