//! A2 acceptance tests: Request builder, reply assembly, HttpEngine.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The network tests talk to `oxa_fake::FakeServer` on 127.0.0.1 (an ephemeral port) or to a
//! one-shot raw loopback server started by this file. Nothing outside 127.0.0.1 is contacted.

use oxa_engine::{
    build_body, build_headers, fetch_info, new_idem_key, parse_info, HttpEngine, ReplyAssembler, Url,
};
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn};
use oxa_proto::{
    codes, CancelToken, ChatRequest, Engine, Finish, FrozenPrefix, Hints, StreamEvent, ThinkingLevel,
    TurnKind, Value, WireMsg,
};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn prefix(system: &str, tools_json: &str) -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), system.to_string())],
        tools_json: tools_json.to_string(),
        sha256: [0u8; 32],
    }
}

fn hints<'a>(session: &'a str, key: &'a str) -> Hints<'a> {
    Hints {
        session,
        idem_key: key,
        kind: TurnKind::Chat,
        background: false,
        snapshot_keep: false,
    }
}

fn request<'a>(p: &'a FrozenPrefix, msgs: &'a [WireMsg], max_tokens: u32, h: Hints<'a>) -> ChatRequest<'a> {
    ChatRequest {
        prefix: p,
        messages: msgs,
        model: "fake-model",
        max_tokens,
        thinking: false,
        thinking_budget: None,
        preserve_thinking: true,
        sampling: None,
        hints: h,
    }
}

fn body_of(r: &ChatRequest) -> String {
    let mut out = Vec::new();
    build_body(r, &mut out);
    String::from_utf8(out).expect("the body is UTF-8")
}

/// A one-shot loopback server: reads one request, answers with `response` verbatim, closes.
/// Returns the base URL and the request bytes it received.
fn raw_server(response: Vec<u8>) -> (String, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let handle = thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        let req = read_request(&mut s);
        let _ = s.write_all(&response);
        let _ = s.shutdown(Shutdown::Both);
        req
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn read_request(s: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return buf,
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
    buf
}

/// `data:` SSE response with close-delimited framing (no chunked encoding).
fn sse_response(events: &[&str]) -> Vec<u8> {
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n".to_vec();
    for e in events {
        out.extend_from_slice(format!("data: {e}\n\n").as_bytes());
    }
    out
}

// ---------- acceptance tests ----------

/// tool-call argument deltas concatenate verbatim, incl. odd whitespace
#[test]
fn args_raw_is_concatenated_deltas() {
    let args = "{ \"path\" :\t\"a b\" ,  \"n\": 1 }\n ";
    let server = FakeServer::start(
        vec![ScriptTurn {
            chunk: 3,
            ..ScriptTurn::tool("call_1", "read", args)
        }],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    // F6: the fake bounds the reply by `max_tokens` (one token = one byte), so leave room.
    let r = request(&p, &go, 4096, hints("s1", KEY));
    let mut events = Vec::new();
    let turn = engine
        .chat(&r, &CancelToken::new(), &mut |e| events.push(e))
        .unwrap();

    assert_eq!(turn.msg.tool_calls.len(), 1);
    assert_eq!(turn.msg.tool_calls[0].args_raw, args);
    assert_eq!(turn.msg.tool_calls[0].id, "call_1");
    assert_eq!(turn.msg.tool_calls[0].name, "read");
    assert_eq!(turn.finish, Finish::ToolCalls);
    let streamed: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolArgs { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        streamed, args,
        "the ToolArgs events carry the same bytes in order"
    );
}

/// A name repeated whole (every delta of a gateway that re-sends it) is not doubled.
#[test]
fn repeated_identical_tool_names_are_not_doubled() {
    let mut assembler = ReplyAssembler::new();
    for (i, args) in ["{\"path\":", "\"oxa\"}"].into_iter().enumerate() {
        let mut tc = serde_json::json!({"index": 0, "function": {"name": "list_files", "arguments": args}});
        if i == 0 {
            tc["id"] = "call_directory".into();
        }
        let finish = if i == 1 { "tool_calls" } else { "" };
        let fr = if finish.is_empty() {
            serde_json::Value::Null
        } else {
            finish.into()
        };
        let mut chunk =
            serde_json::json!({"choices": [{"delta": {"tool_calls": [tc]}, "finish_reason": fr}]});
        if i == 1 {
            chunk["usage"] =
                serde_json::json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
        }
        assembler.chunk(&chunk.to_string()).unwrap();
    }
    let turn = assembler.finish(ThinkingLevel::Off).unwrap();
    assert_eq!(turn.msg.tool_calls[0].name, "list_files");
    assert_eq!(turn.msg.tool_calls[0].args_raw, "{\"path\":\"oxa\"}");
}

/// A refusal ends the stream with `content_filter`: the streamed text is kept.
#[test]
fn content_filter_finish_keeps_the_streamed_text() {
    let mut assembler = ReplyAssembler::new();
    assembler
        .chunk(&serde_json::json!({"choices": [{"delta": {"content": "I can't help"}}]}).to_string())
        .unwrap();
    assembler
        .chunk(
            &serde_json::json!({"choices": [{"delta": {}, "finish_reason": "content_filter"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
            .to_string(),
        )
        .unwrap();
    let turn = assembler.finish(ThinkingLevel::Off).unwrap();
    assert_eq!(turn.finish, Finish::ContentFilter);
    assert_eq!(turn.msg.content.as_deref(), Some("I can't help"));
}

/// leading/trailing whitespace survives assembly
#[test]
fn content_and_reasoning_untrimmed() {
    let server = FakeServer::start(
        vec![ScriptTurn {
            reasoning: Some("  think \n".to_string()),
            chunk: 2,
            ..ScriptTurn::text("\n  hi  \n")
        }],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let turn = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap();

    assert_eq!(turn.msg.content.as_deref(), Some("\n  hi  \n"));
    assert_eq!(turn.msg.reasoning.as_deref(), Some("  think \n"));
    assert_eq!(
        String::from_utf8(turn.msg.to_json()).unwrap(),
        "{\"role\":\"assistant\",\"content\":\"\\n  hi  \\n\",\"reasoning_content\":\"  think \\n\"}"
    );
}

/// build_body output equals the golden bytes (key order, W6 fields)
#[test]
fn body_bytes_golden() {
    let p = prefix(
        "SYS",
        "[{\"type\":\"function\",\"function\":{\"name\":\"read\"}}]",
    );
    let msgs = [WireMsg::user("hi"), WireMsg::assistant("ok")];
    let r = ChatRequest {
        thinking: true,
        thinking_budget: None,
        ..request(&p, &msgs, 128, hints("", ""))
    };
    let expected = r#"{"model":"fake-model","messages":[{"role":"system","content":"SYS"},{"role":"user","content":"hi"},{"role":"assistant","content":"ok"}],"tools":[{"type":"function","function":{"name":"read"}}],"stream":true,"stream_options":{"include_usage":true},"temperature":0,"cache_prompt":true,"max_tokens":128,"chat_template_kwargs":{"preserve_thinking":true,"enable_thinking":true}}"#;
    assert_eq!(body_of(&r), expected);
}

/// tools_json `[]` -> no tools key
#[test]
fn body_omits_tools_when_empty() {
    let p = prefix("SYS", "[]");
    let msgs = [WireMsg::user("hi")];
    let r = request(&p, &msgs, 5, hints("", ""));
    let expected = r#"{"model":"fake-model","messages":[{"role":"system","content":"SYS"},{"role":"user","content":"hi"}],"stream":true,"stream_options":{"include_usage":true},"temperature":0,"cache_prompt":true,"max_tokens":5,"chat_template_kwargs":{"preserve_thinking":true,"enable_thinking":false}}"#;
    assert_eq!(body_of(&r), expected);
    assert!(!body_of(&r).contains("\"tools\""));
}

/// H6 body golden
#[test]
fn body_prefill_max_tokens_zero() {
    let p = prefix("SYS", "[]");
    let r = request(&p, &[], 0, hints("s1", ""));
    let expected = r#"{"model":"fake-model","messages":[{"role":"system","content":"SYS"}],"stream":true,"stream_options":{"include_usage":true},"temperature":0,"cache_prompt":true,"max_tokens":0,"chat_template_kwargs":{"preserve_thinking":true,"enable_thinking":false}}"#;
    assert_eq!(body_of(&r), expected);
}

/// Opt-in sampling (`oxa.sampling.mode = model`): the card's keys replace the greedy `0` in place
/// and the result is valid JSON the engine's validator accepts (numbers, top_k an integer).
#[test]
fn body_with_model_sampling() {
    let p = prefix("SYS", "[]");
    let msgs = [WireMsg::user("hi")];
    let mut r = request(&p, &msgs, 5, hints("", ""));
    r.sampling = Some(oxa_proto::Sampling {
        temperature: 0.7,
        top_p: 0.8,
        top_k: 20,
        min_p: 0.0,
        presence_penalty: 1.5,
        seed: Some(7),
    });
    let expected = r#"{"model":"fake-model","messages":[{"role":"system","content":"SYS"},{"role":"user","content":"hi"}],"stream":true,"stream_options":{"include_usage":true},"temperature":0.7,"top_p":0.8,"top_k":20,"min_p":0,"presence_penalty":1.5,"seed":7,"cache_prompt":true,"max_tokens":5,"chat_template_kwargs":{"preserve_thinking":true,"enable_thinking":false}}"#;
    assert_eq!(body_of(&r), expected);
    let v: Value = serde_json::from_str(&body_of(&r)).unwrap();
    assert_eq!(v["top_k"].as_i64(), Some(20));
    r.sampling.as_mut().unwrap().seed = None;
    assert!(!body_of(&r).contains("\"seed\""));
}

/// build_headers per hint combination
#[test]
fn header_bytes_golden_h1_to_h5() {
    let none = Hints {
        session: "",
        idem_key: "",
        kind: TurnKind::Chat,
        background: false,
        snapshot_keep: false,
    };
    assert_eq!(
        build_headers(&none),
        vec![("X-Oxe-Turn-Kind", "chat".to_string())]
    );

    let all = Hints {
        session: "s1",
        idem_key: "k",
        kind: TurnKind::Compaction,
        background: true,
        snapshot_keep: true,
    };
    assert_eq!(
        build_headers(&all),
        vec![
            ("X-Oxe-Session", "s1".to_string()),
            ("X-Session-Id", "s1".to_string()),
            ("Idempotency-Key", "k".to_string()),
            ("X-Oxe-Priority", "background".to_string()),
            ("X-Oxe-Snapshot", "keep".to_string()),
            ("X-Oxe-Turn-Kind", "compaction".to_string()),
        ]
    );

    let some = Hints {
        session: "s2",
        idem_key: "",
        kind: TurnKind::Subagent,
        background: false,
        snapshot_keep: true,
    };
    assert_eq!(
        build_headers(&some),
        vec![
            ("X-Oxe-Session", "s2".to_string()),
            ("X-Session-Id", "s2".to_string()),
            ("X-Oxe-Snapshot", "keep".to_string()),
            ("X-Oxe-Turn-Kind", "subagent".to_string()),
        ]
    );
}

/// refused twice then ok -> one reply, same Idempotency-Key on every attempt
#[test]
fn retry_only_before_first_data_byte() {
    let server = FakeServer::start(
        vec![
            ScriptTurn::fault(Fault::Refuse),
            ScriptTurn::fault(Fault::Refuse),
            ScriptTurn::text("ok"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let turn = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap();

    assert_eq!(turn.msg.content.as_deref(), Some("ok"));
    let seen = server.requests();
    assert_eq!(seen.len(), 3, "two refused attempts and one answered attempt");
    assert!(
        seen.iter().all(|rec| rec.idem_key == KEY),
        "every attempt reuses the key"
    );
    assert!(seen.iter().all(|rec| rec.session == "s1"));
}

/// drop after first data chunk -> stream_broken, exactly one request
#[test]
fn no_retry_after_data() {
    // 20 deltas of about 137 bytes each; the cut at 300 bytes leaves two complete data events.
    let text = "x".repeat(200);
    let server = FakeServer::start(
        vec![ScriptTurn {
            chunk: 10,
            fault: Some(Fault::DropAt { bytes: 300 }),
            ..ScriptTurn::text(&text)
        }],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let mut got_text = 0usize;
    let err = engine
        .chat(&r, &CancelToken::new(), &mut |e| {
            if let StreamEvent::Text(t) = e {
                got_text += t.len();
            }
        })
        .unwrap_err();

    assert_eq!(err.code, codes::STREAM_BROKEN);
    assert_eq!(
        server.requests().len(),
        1,
        "no second request after the first data byte"
    );
    assert!(got_text > 0, "the deltas before the cut were shown");
}

/// 400 exceed_context_size_error -> context_overflow
#[test]
fn overflow_maps_to_context_overflow() {
    // The prompt alone is longer than the engine's context: the fake answers 400 overflow.
    let long = "S".repeat(200);
    let server = FakeServer::start(
        vec![],
        FakeOpts {
            n_ctx: 64,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix(&long, "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 16, hints("s1", KEY));
    let err = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.code, codes::CONTEXT_OVERFLOW);
    assert!(err.hint.is_some());

    // A scripted overflow answer maps the same way.
    let server = FakeServer::start(vec![ScriptTurn::fault(Fault::Overflow)], FakeOpts::default()).unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 16, hints("s1", KEY));
    let err = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.code, codes::CONTEXT_OVERFLOW);
    assert_eq!(server.requests().len(), 1, "an overflow is not retried");
}

/// missing queue = 0
#[test]
fn info_parses_models_and_status() {
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let url = Url::parse(&server.url()).unwrap();
    let info = fetch_info(&url, None).unwrap();
    assert_eq!(info.models, vec!["fake-model".to_string()]);
    assert_eq!(info.model, "fake-model");
    assert_eq!(info.n_ctx, 1 << 20);
    // The fake serves the real controller's status, which publishes no output limit.
    assert_eq!(info.max_output, oxa_engine::DEFAULT_MAX_OUTPUT);
    assert_eq!(info.queue, 0);
    assert!(info.prefill_only, "the fake advertises H6 on /props");

    // Without the queue field (a server without M3's queue length) the queue is 0.
    let server = FakeServer::start(
        vec![],
        FakeOpts {
            hints: false,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let url = Url::parse(&server.url()).unwrap();
    assert_eq!(fetch_info(&url, None).unwrap().queue, 0);

    // The same through the Engine trait.
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    assert_eq!(engine.info(None).unwrap().n_ctx, 1 << 20);

    // parse_info: missing queue, model taken from /v1/models when the status has none.
    let models: Value = serde_json::json!({"data": [{"id": "m-a"}, {"id": "m-b"}]});
    let status: Value = serde_json::json!({"n_ctx": 4096});
    let info = parse_info("http://h:1", &models, &status).unwrap();
    assert_eq!(info.queue, 0);
    assert_eq!(info.model, "m-a");
    assert_eq!(info.models, vec!["m-a".to_string(), "m-b".to_string()]);
    // No output limit published: a quarter of the context, at most DEFAULT_MAX_OUTPUT.
    assert_eq!(info.max_output, 1024);
    assert!(!info.prefill_only);

    // Missing n_ctx is an engine_http error.
    let err = parse_info("http://h:1", &models, &serde_json::json!({})).unwrap_err();
    assert_eq!(err.code, codes::ENGINE_HTTP);
}

/// [DONE] without finish_reason/usage
#[test]
fn finish_without_usage_is_stream_broken() {
    let content = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#;
    let finish_only = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    let usage_only = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;

    // [DONE] after a finish_reason but with no usage.
    let (url, handle) = raw_server(sse_response(&[content, finish_only, "[DONE]"]));
    let engine = HttpEngine::new(&url, None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let err = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.code, codes::STREAM_BROKEN);
    let _ = handle.join();

    // [DONE] after usage but with no finish_reason.
    let (url, handle) = raw_server(sse_response(&[content, usage_only, "[DONE]"]));
    let engine = HttpEngine::new(&url, None).unwrap();
    let err = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.code, codes::STREAM_BROKEN);
    let _ = handle.join();
}

// ---------- further tests ----------

/// H6 over HTTP: prefill comment becomes a Prefill event; usage and timings, no decode.
#[test]
fn prefill_request_returns_usage_and_prefill_event() {
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 0, hints("s1", KEY));
    let mut events = Vec::new();
    let turn = engine
        .chat(&r, &CancelToken::new(), &mut |e| events.push(e))
        .unwrap();

    assert!(turn.msg.content.is_none());
    assert_eq!(turn.usage.completion, 0);
    assert!(turn.usage.prompt > 0);
    assert!(turn.timings.is_some());
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Prefill { .. })));
    assert_eq!(server.requests().len(), 1);
}

/// Reasoning, text and tool deltas come out in arrival order; the request is sent with the
/// session and turn-kind hints and the Authorization header when a key is set.
#[test]
fn events_in_order_and_request_headers() {
    let server = FakeServer::start(
        vec![ScriptTurn {
            reasoning: Some("r1".to_string()),
            text: Some("t1".to_string()),
            tool_calls: ScriptTurn::tool("c", "n", "{}").tool_calls,
            finish: Finish::ToolCalls,
            ..ScriptTurn::text("")
        }],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), Some("sekrit".to_string())).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    // F6: the fake bounds the reply by `max_tokens` (one token = one byte), so leave room.
    let r = request(&p, &go, 4096, hints("sess-9", KEY));
    let mut events = Vec::new();
    let turn = engine
        .chat(&r, &CancelToken::new(), &mut |e| events.push(e))
        .unwrap();

    assert_eq!(turn.level, ThinkingLevel::Off);
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            StreamEvent::Reasoning(_) => "reasoning",
            StreamEvent::Text(_) => "text",
            StreamEvent::ToolArgs { .. } => "tool",
            StreamEvent::Prefill { .. } => "prefill",
            StreamEvent::Queued { .. } => "queued",
        })
        .collect();
    assert_eq!(kinds, vec!["reasoning", "text", "tool"]);

    let rec = &server.requests()[0];
    assert_eq!(rec.session, "sess-9");
    assert_eq!(rec.kind, "chat");
    assert_eq!(rec.idem_key, KEY);
    assert!(rec
        .headers
        .iter()
        .any(|(k, v)| k == "authorization" && v == "Bearer sekrit"));
}

/// Retries stop after `connect_retries`; the last error is returned and nothing is answered.
#[test]
fn retries_are_bounded() {
    let server = FakeServer::start(
        vec![
            ScriptTurn::fault(Fault::Refuse),
            ScriptTurn::fault(Fault::Refuse),
            ScriptTurn::fault(Fault::Refuse),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let err = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.code, codes::STREAM_BROKEN);
    assert_eq!(server.requests().len(), 3, "one attempt plus two retries");
}

/// A refused connection is engine_down with the hint, after the retries.
#[test]
fn refused_port_is_engine_down_with_hint() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let engine = HttpEngine::new(&format!("http://127.0.0.1:{port}"), None)
        .unwrap()
        .with_opts(
            oxa_engine::StreamOpts {
                connect_timeout_ms: 1000,
                silence_limit_ms: 1000,
            },
            0,
        );
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let err = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.code, codes::ENGINE_DOWN);
    assert!(err.hint.is_some());
}

/// A cancelled token ends the request as aborted before anything is sent.
#[test]
fn cancel_before_chat_is_aborted() {
    let server = FakeServer::start(vec![ScriptTurn::text("never")], FakeOpts::default()).unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = engine.chat(&r, &cancel, &mut |_| {}).unwrap_err();
    assert_eq!(err.code, codes::ABORTED);
    assert_eq!(server.requests().len(), 0);
}

/// count_tokens counts the engine's tokens (the fake: one per byte).
#[test]
fn count_tokens_uses_tokenize() {
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    assert_eq!(engine.count_tokens("hello").unwrap(), 5);
    assert_eq!(engine.count_tokens("").unwrap(), 0);
}

/// An empty idempotency key is generated once per call, so retries still share it.
#[test]
fn generated_key_is_shared_by_retries() {
    let server = FakeServer::start(
        vec![ScriptTurn::fault(Fault::Refuse), ScriptTurn::text("ok")],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", ""));
    engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap();
    let seen = server.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].idem_key.len(), 32);
    assert_eq!(seen[0].idem_key, seen[1].idem_key);
}

#[test]
fn idem_keys_are_32_lower_hex_and_fresh() {
    let a = new_idem_key();
    let b = new_idem_key();
    assert_eq!(a.len(), 32);
    assert!(a
        .bytes()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    assert_ne!(a, b);
}

#[test]
fn https_engine_url_is_rejected() {
    let err = HttpEngine::new("https://engine.local:8080", None).unwrap_err();
    assert_eq!(err.code, codes::INVALID);
    assert!(err.hint.is_some());
}

#[test]
fn reply_assembler_prefill_comment() {
    let asm = ReplyAssembler::new();
    assert_eq!(
        asm.comment("prefill 10/200", 3),
        Some(StreamEvent::Prefill {
            done: 10,
            total: 200,
            secs: 3
        })
    );
    // S2 / F10: a bare `queued` is a queued line (wait = the caller's seconds), no longer
    // ignored; any other comment still is.
    assert_eq!(
        asm.comment("queued", 1),
        Some(StreamEvent::Queued { pos: None, wait_s: 1 })
    );
    assert_eq!(asm.comment("keepalive", 1), None);
    assert_eq!(asm.comment("queuedx 3", 1), None);
}

fn run_reasoning_case(turn: ScriptTurn) -> (Vec<StreamEvent>, oxa_proto::AssistantTurn) {
    let server = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 4096, hints("s-r", KEY));
    let mut events = Vec::new();
    let t = engine
        .chat(&r, &CancelToken::new(), &mut |e| events.push(e))
        .unwrap();
    (events, t)
}

fn joined(events: &[StreamEvent]) -> (String, String) {
    let (mut r, mut t) = (String::new(), String::new());
    for e in events {
        match e {
            StreamEvent::Reasoning(d) => r.push_str(d),
            StreamEvent::Text(d) => t.push_str(d),
            _ => {}
        }
    }
    (r, t)
}

/// `reasoning_content` deltas are reasoning events, not answer text.
#[test]
fn reasoning_content_field_is_thinking() {
    let (events, turn) = run_reasoning_case(ScriptTurn {
        reasoning: Some("pondering".to_string()),
        text: Some("answer".to_string()),
        ..ScriptTurn::text("")
    });
    assert_eq!(joined(&events), ("pondering".to_string(), "answer".to_string()));
    assert_eq!(turn.msg.reasoning.as_deref(), Some("pondering"));
    assert_eq!(turn.msg.content.as_deref(), Some("answer"));
}

/// `<think>..</think>` in content text (split across tiny chunks) is reasoning too.
#[test]
fn think_tags_in_content_are_thinking() {
    for chunk in [1usize, 3, 1000] {
        let (events, turn) = run_reasoning_case(ScriptTurn {
            text: Some("<think>\nlook first\n</think>\n\nthe answer".to_string()),
            chunk,
            ..ScriptTurn::text("")
        });
        let (r, t) = joined(&events);
        assert_eq!(r.trim(), "look first", "chunk {chunk}");
        assert_eq!(t.trim(), "the answer", "chunk {chunk}");
        assert_eq!(turn.msg.reasoning.as_deref().map(str::trim), Some("look first"));
        assert_eq!(turn.msg.content.as_deref().map(str::trim), Some("the answer"));
        assert!(
            matches!(events.first(), Some(StreamEvent::Reasoning(_))),
            "chunk {chunk}"
        );
    }
}

/// A think tag mentioned later in the answer is left alone.
#[test]
fn think_tag_after_answer_text_is_text() {
    let (events, _) = run_reasoning_case(ScriptTurn::text("use <think> tags"));
    assert_eq!(joined(&events), (String::new(), "use <think> tags".to_string()));
}

/// A stream that ends on a held prefix (`<`, `<thi`) must still deliver that tail through the
/// callback: the joined live events equal the assembled reply.
#[test]
fn held_prefix_at_stream_end_reaches_the_callback() {
    for text in ["<", "<thi", "ok <"] {
        for chunk in [1usize, 1000] {
            let (events, turn) = run_reasoning_case(ScriptTurn {
                text: Some(text.to_string()),
                chunk,
                ..ScriptTurn::text("")
            });
            let (r, t) = joined(&events);
            assert_eq!(r, "", "{text:?} chunk {chunk}");
            assert_eq!(t, text, "{text:?} chunk {chunk}");
            assert_eq!(turn.msg.content.as_deref(), Some(text), "{text:?} chunk {chunk}");
        }
    }
}

/// Leading whitespace before a think block is routed the same however the stream is chunked.
#[test]
fn leading_whitespace_before_think_is_chunk_independent() {
    let mut seen = Vec::new();
    for chunk in [1usize, 2, 1000] {
        let (events, turn) = run_reasoning_case(ScriptTurn {
            text: Some(" \n<think>x</think>answer".to_string()),
            chunk,
            ..ScriptTurn::text("")
        });
        let (r, t) = joined(&events);
        assert_eq!(r, "x", "chunk {chunk}");
        seen.push((t, turn.msg.content.clone()));
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    assert_eq!(seen[0].0, "answer");
}

/// Once a think block has closed, the answer is literal: a second `<think>` block in the
/// answer (an example of the syntax) is answer text, not more reasoning.
#[test]
fn second_think_block_after_close_is_text() {
    for chunk in [1usize, 1000] {
        let (events, turn) = run_reasoning_case(ScriptTurn {
            text: Some("<think>reasoning</think><think>example</think>".to_string()),
            chunk,
            ..ScriptTurn::text("")
        });
        let (r, t) = joined(&events);
        assert_eq!(r, "reasoning", "chunk {chunk}");
        assert_eq!(t, "<think>example</think>", "chunk {chunk}");
        assert_eq!(
            turn.msg.content.as_deref(),
            Some("<think>example</think>"),
            "chunk {chunk}"
        );
    }
}

#[test]
fn expired_cloud_continuation_is_reported_without_retry() {
    let body =
        r#"{"error":{"message":"OpenAI tool continuation expired","code":"tool_continuation_expired"}}"#;
    let response = format!("HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
    let (url, handle) = raw_server(response.into_bytes());
    let engine = HttpEngine::new(&url, None).unwrap();
    let p = prefix("SYS", "[]");
    let go = [WireMsg::user("go")];
    let r = request(&p, &go, 64, hints("s1", KEY));
    let err = engine.chat(&r, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert!(err.message.contains("continuation expired"), "{}", err.message);
    let received = handle.join().unwrap();
    assert!(received.starts_with(b"POST "));
}
