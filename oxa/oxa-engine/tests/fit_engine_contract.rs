//! Fit pass (post wave E): the oxa engine client against the real engine's and controller's
//! HTTP behaviour (review `FIT-REVIEW-OXA-ENGINE`: F1, F2, F3, F4, F6, F7, F8, F10, F14, EG5,
//! H4, H6). Each test names its finding. Only 127.0.0.1: `oxa_fake::FakeServer` or a one-shot
//! raw loopback server started here.

use oxa_engine::{parse_info, parse_info_for, HttpEngine, ReplyAssembler, SseEvent, SseParser};
use oxa_fake::{FakeModel, FakeOpts, FakeServer, Fault, ScriptTurn};
use oxa_proto::{
    codes, CancelToken, ChatRequest, Engine, EngineCaps, Finish, FrozenPrefix, Hints, StreamEvent, TurnKind,
    Value, WireMsg,
};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;

fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), "SYS".to_string())],
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

fn request<'a>(p: &'a FrozenPrefix, msgs: &'a [WireMsg], max_tokens: u32, h: Hints<'a>) -> ChatRequest<'a> {
    ChatRequest {
        prefix: p,
        messages: msgs,
        model: "fake-model",
        max_tokens,
        thinking: false,
        preserve_thinking: true,
        sampling: None,
        hints: h,
    }
}

fn run(
    engine: &HttpEngine,
    r: &ChatRequest,
) -> (
    Result<oxa_proto::AssistantTurn, oxa_proto::OxaError>,
    Vec<StreamEvent>,
) {
    let mut events = Vec::new();
    let res = engine.chat(r, &CancelToken::new(), &mut |e| events.push(e));
    (res, events)
}

/// A loopback server that answers every connection with `response` and logs each request's
/// first line and body. Returns the base URL and the log.
fn canned(response: &'static [u8]) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&log);
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..h]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        let first = head.lines().next().unwrap_or("").to_string();
                        let body = String::from_utf8_lossy(&buf[h + 4..h + 4 + len]).to_string();
                        seen.lock().unwrap().push(format!("{first} {body}"));
                        break;
                    }
                }
            }
            let _ = s.write_all(response);
            let _ = s.shutdown(Shutdown::Both);
        }
    });
    (format!("http://127.0.0.1:{port}"), log)
}

// ---------- F2 / F10 comments ----------

/// F2: the engine's prefill line `: prefill <done>/<total> <secs>s` is parsed (its own seconds
/// win); the plain form keeps the caller's seconds; later fields are ignored.
#[test]
fn f2_prefill_comment_with_the_engines_seconds() {
    let asm = ReplyAssembler::new();
    assert_eq!(
        asm.comment("prefill 12288/28500 361s", 9),
        Some(StreamEvent::Prefill {
            done: 12288,
            total: 28500,
            secs: 361
        })
    );
    assert_eq!(
        asm.comment("prefill 10/200", 3),
        Some(StreamEvent::Prefill {
            done: 10,
            total: 200,
            secs: 3
        })
    );
    assert_eq!(
        asm.comment("prefill 5/9 extra=1 7s", 0),
        Some(StreamEvent::Prefill {
            done: 5,
            total: 9,
            secs: 7
        })
    );
    assert_eq!(asm.comment("prefill x/9 1s", 0), None);
}

/// S2 / F10: the unified queued line `: queued [pos=<n>] wait_s=<s>` (unknown keys ignored) and
/// the two legacy forms (`queued <n>` controller position, `queued <n>s` engine seconds).
#[test]
fn f10_queued_comment_forms() {
    let asm = ReplyAssembler::new();
    let q = |pos, wait_s| Some(StreamEvent::Queued { pos, wait_s });
    assert_eq!(asm.comment("queued pos=2 wait_s=31", 0), q(Some(2), 31));
    assert_eq!(asm.comment("queued wait_s=12", 0), q(None, 12));
    assert_eq!(asm.comment("queued pos=1 wait_s=0 lane=bg", 5), q(Some(1), 0));
    assert_eq!(asm.comment("queued pos=4", 5), q(Some(4), 5));
    assert_eq!(asm.comment("queued 3", 5), q(Some(3), 5));
    assert_eq!(asm.comment("queued 12s", 5), q(None, 12));
    assert_eq!(asm.comment("waiting 3", 5), None);
}

/// S2 / F10 over HTTP: queued comments become `Queued` events before the reply, the reply is
/// complete, and the comments are not data.
#[test]
fn f10_queued_events_over_http() {
    let server = FakeServer::start(
        vec![ScriptTurn {
            fault: Some(Fault::Queued { count: 2, pos: true }),
            ..ScriptTurn::text("ok")
        }],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let (p, go) = (prefix(), [WireMsg::user("go")]);
    let (res, events) = run(&engine, &request(&p, &go, 4096, hints("s")));
    assert_eq!(res.unwrap().msg.content.as_deref(), Some("ok"));
    assert_eq!(
        events,
        vec![
            StreamEvent::Queued {
                pos: Some(2),
                wait_s: 0
            },
            StreamEvent::Queued {
                pos: Some(1),
                wait_s: 1
            },
            StreamEvent::Text("ok".into()),
        ]
    );
}

// ---------- F3 ----------

/// F3: the SSE parser tolerates the `event:` field line; the error object is the data.
#[test]
fn f3_sse_parser_tolerates_event_lines() {
    let mut p = SseParser::new();
    let ev = p.feed(b": queued pos=1 wait_s=0\n\nevent: error\ndata: {\"error\":{\"code\":400}}\n\n");
    assert_eq!(
        ev,
        vec![
            SseEvent::Comment("queued pos=1 wait_s=0".into()),
            SseEvent::Data("{\"error\":{\"code\":400}}".into()),
        ]
    );
}

/// F3: an in-stream error object is classified like a status reply: overflow, a numeric (or
/// 3-digit string, or `status`) code -> `engine_http` / `engine_busy` naming `HTTP <status>`,
/// `local_backend_busy` -> busy; no usable status stays `stream_broken`.
#[test]
fn f3_reply_assembler_classifies_error_objects() {
    let err = |v: Value| ReplyAssembler::new().chunk(&v.to_string()).unwrap_err();
    let e = err(json!({"error": {"code": 400, "type": "exceed_context_size_error", "message": "too long"}}));
    assert_eq!(e.code, codes::CONTEXT_OVERFLOW);
    let e = err(json!({"error": {"code": "exceed_context_size_error", "message": "too long"}}));
    assert_eq!(e.code, codes::CONTEXT_OVERFLOW);
    let e = err(json!({"error": {"code": 400, "type": "invalid_request_error", "message": "bad"}}));
    assert_eq!(e.code, codes::ENGINE_HTTP);
    assert!(e.message.contains("HTTP 400") && e.message.contains("bad"), "{e}");
    let e = err(json!({"error": {"code": 500, "message": "decode failed"}}));
    assert_eq!(e.code, codes::ENGINE_HTTP);
    assert!(e.message.contains("HTTP 500"), "{e}");
    let e = err(json!({"error": {"code": "429", "message": "slow down"}}));
    assert!(e.message.contains("HTTP 429"), "{e}");
    let e = err(json!({"error": {"status": 502, "message": "gateway"}}));
    assert!(e.message.contains("HTTP 502"), "{e}");
    let e = err(json!({"error": {"code": 503, "type": "unavailable_error", "message": "no model"}}));
    assert_eq!(e.code, codes::ENGINE_BUSY);
    assert!(e.message.contains("HTTP 503"), "{e}");
    let e = err(json!({"error": {"code": "queue_full", "type": "local_backend_busy", "message": "full"}}));
    assert_eq!(e.code, codes::ENGINE_BUSY);
    let e = err(
        json!({"error": {"code": null, "type": "local_backend_error", "message": "Local backend error."}}),
    );
    assert_eq!(e.code, codes::STREAM_BROKEN);
    let e = err(json!({"error": "plain"}));
    assert_eq!(e.code, codes::STREAM_BROKEN);
}

/// F3 over HTTP: after early headers and queued comments, the controller's `event: error` with
/// a 400 is a final `engine_http` (HTTP 400), only the queued events reached `on` (no data
/// event, so the core's got-data probe stays false) and the client did not retry; an
/// in-stream overflow is `context_overflow`; the engine's own plain 500 error is `engine_http`
/// HTTP 500 (the core may retry it).
#[test]
fn f3_in_stream_errors_over_http() {
    let server = FakeServer::start(
        vec![
            ScriptTurn::fault(Fault::StreamError {
                queued: 2,
                status: 400,
                error_type: "invalid_request_error".into(),
                event: true,
            }),
            ScriptTurn::fault(Fault::StreamError {
                queued: 1,
                status: 400,
                error_type: "exceed_context_size_error".into(),
                event: true,
            }),
            ScriptTurn::fault(Fault::StreamError {
                queued: 0,
                status: 500,
                error_type: "server_error".into(),
                event: false,
            }),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let (p, go) = (prefix(), [WireMsg::user("go")]);

    let (res, events) = run(&engine, &request(&p, &go, 4096, hints("s")));
    let e = res.unwrap_err();
    assert_eq!(e.code, codes::ENGINE_HTTP);
    assert!(e.message.contains("HTTP 400"), "{e}");
    assert!(
        events.iter().all(|e| matches!(e, StreamEvent::Queued { .. })),
        "{events:?}"
    );
    assert_eq!(events.len(), 2);
    assert_eq!(server.requests().len(), 1, "a 400 is final: no client retry");

    let (res, _) = run(&engine, &request(&p, &go, 4096, hints("s")));
    assert_eq!(res.unwrap_err().code, codes::CONTEXT_OVERFLOW);

    let (res, events) = run(&engine, &request(&p, &go, 4096, hints("s")));
    let e = res.unwrap_err();
    assert_eq!(e.code, codes::ENGINE_HTTP);
    assert!(e.message.contains("HTTP 500"), "{e}");
    assert!(events.is_empty());
    assert_eq!(server.requests().len(), 3);
}

/// F3: an error object after reply data is a cut reply (`stream_broken`), never a status the
/// core would replay after output.
#[test]
fn f3_error_after_data_is_a_cut_reply() {
    let (url, _) = canned(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"},\"finish_reason\":null}]}\n\n\
data: {\"error\":{\"code\":500,\"message\":\"boom\"}}\n\n",
    );
    let engine = HttpEngine::new(&url, None).unwrap();
    let (p, go) = (prefix(), [WireMsg::user("go")]);
    let (res, events) = run(&engine, &request(&p, &go, 64, hints("s")));
    let e = res.unwrap_err();
    assert_eq!(e.code, codes::STREAM_BROKEN, "{e}");
    assert!(e.message.contains("boom"));
    assert_eq!(events, vec![StreamEvent::Text("par".into())]);
}

/// F3 / S2: the controller's queue-full 503 + `Retry-After: 30` is `engine_busy` naming
/// `HTTP 503` and `retry after 30 s`; the client does not spin on it (one request; the wait is
/// the core's). Without the header the message names no wait.
#[test]
fn f3_busy_503_is_engine_busy_with_its_wait() {
    let server = FakeServer::start(
        vec![
            ScriptTurn::fault(Fault::Busy {
                retry_after: Some(30),
            }),
            ScriptTurn::fault(Fault::Busy { retry_after: None }),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let (p, go) = (prefix(), [WireMsg::user("go")]);
    let (res, events) = run(&engine, &request(&p, &go, 4096, hints("s")));
    let e = res.unwrap_err();
    assert_eq!(e.code, codes::ENGINE_BUSY);
    assert!(
        e.message.contains("HTTP 503") && e.message.contains("retry after 30 s"),
        "{e}"
    );
    assert!(events.is_empty());
    assert_eq!(server.requests().len(), 1, "no client-side spin on engine_busy");
    let (res, _) = run(&engine, &request(&p, &go, 4096, hints("s")));
    let e = res.unwrap_err();
    assert_eq!(e.code, codes::ENGINE_BUSY);
    assert!(!e.message.contains("retry after"), "{e}");
}

// ---------- F1 / F6 / F7 / H4 / H6 ----------

/// F1 over HTTP: a perfect continuation reports `prompt + completion - 1` cached tokens.
#[test]
fn f1_continuation_cache_over_http() {
    let server = FakeServer::start(
        vec![ScriptTurn::text("Hello"), ScriptTurn::text("ok")],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix();
    let m1 = [WireMsg::user("hi")];
    let t1 = run(&engine, &request(&p, &m1, 4096, hints("s"))).0.unwrap();
    let m2 = [WireMsg::user("hi"), t1.msg.clone(), WireMsg::user("more")];
    let t2 = run(&engine, &request(&p, &m2, 4096, hints("s"))).0.unwrap();
    assert_eq!(t2.usage.cached, t1.usage.prompt + t1.usage.completion - 1);
}

/// F6 (item 8, the band): a prompt below `n_ctx` whose reply does not fit is answered and cut
/// with `length` when the context is full (`prompt + completion == n_ctx`), not refused; a
/// prompt of `n_ctx` tokens is `context_overflow`. Treating the band as an overflow is the
/// core's rule (lane L2), not the client's.
#[test]
fn f6_band_reply_is_cut_with_length() {
    let p = prefix();
    let go = [WireMsg::user("go")];
    let n = oxa_fake::cache_bytes(&p.system_text(), &p.tools_json, &go).len() as u32;
    let server = FakeServer::start(
        vec![ScriptTurn::text(&"y".repeat(300))],
        FakeOpts {
            n_ctx: n + 20,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let t = run(&engine, &request(&p, &go, 4096, hints("s"))).0.unwrap();
    assert_eq!(t.finish, Finish::Length);
    assert_eq!(t.usage.prompt + t.usage.completion, n + 20);
    assert_eq!(t.msg.content.as_deref(), Some("y".repeat(20).as_str()));

    let full = FakeServer::start(
        vec![ScriptTurn::text("never")],
        FakeOpts {
            n_ctx: n,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&full.url(), None).unwrap();
    let e = run(&engine, &request(&p, &go, 1, hints("s"))).0.unwrap_err();
    assert_eq!(e.code, codes::CONTEXT_OVERFLOW);
}

/// F7 / H6 / H4: a prefill (`max_tokens: 0`) assembles from the engine's stream (role chunk,
/// finish chunk, separate usage chunk): `length`, completion 0, no content; with H4
/// `snap_pos == prompt`; without the hint the reply has no `snap_pos` key at all; a scripted
/// null snapshot is `snap_asked` without a position.
#[test]
fn f7_h6_and_h4_over_http() {
    let server = FakeServer::start(
        vec![ScriptTurn::fault(Fault::SnapNull {
            reason: "no_slot".into(),
        })],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix();
    let go = [WireMsg::user("go")];
    let mut h = hints("s");
    h.snapshot_keep = true;
    let (res, events) = run(&engine, &request(&p, &go, 0, h));
    let t = res.unwrap();
    assert_eq!(t.finish, Finish::Length);
    assert_eq!(t.usage.completion, 0);
    assert_eq!(t.msg.content, None);
    let tm = t.timings.unwrap();
    assert_eq!(tm.snap_pos, Some(t.usage.prompt));
    assert!(tm.snap_asked);
    assert!(
        matches!(events[..], [StreamEvent::Prefill { done, total, secs: 0 }] if done == total && total == t.usage.prompt),
        "{events:?}"
    );

    let t = run(&engine, &request(&p, &go, 0, hints("t"))).0.unwrap();
    let tm = t.timings.unwrap();
    assert!(!tm.snap_asked && tm.snap_pos.is_none());

    let t = run(&engine, &request(&p, &go, 4096, h)).0.unwrap();
    let tm = t.timings.unwrap();
    assert!(tm.snap_asked && tm.snap_pos.is_none(), "{tm:?}");
}

/// F8: `preserve_thinking: false` on the request omits the key from the body (the controller's
/// own default then applies); the fake records what was sent.
#[test]
fn f8_preserve_thinking_omitted_when_false() {
    let server = FakeServer::start(
        vec![ScriptTurn::text("a"), ScriptTurn::text("b")],
        FakeOpts::default(),
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix();
    let go = [WireMsg::user("go")];
    let mut r = request(&p, &go, 4096, hints("s"));
    r.preserve_thinking = false;
    run(&engine, &r).0.unwrap();
    r.preserve_thinking = true;
    run(&engine, &r).0.unwrap();
    let recs = server.requests();
    assert_eq!(recs[0].preserve_thinking, None);
    assert_eq!(recs[1].preserve_thinking, Some(true));
}

// ---------- F4 / F14 ----------

/// F4: capabilities come from `model_limits[id].capabilities` (before any load) and fall back to
/// `/props` `oxe_hints`; nothing published = the defaults. `prefill_only` follows the same
/// source.
#[test]
fn f4_parse_capabilities_with_fallback() {
    let models = json!({"data": [{"id": "flash"}, {"id": "moe"}]});
    let status = json!({
        "state": "ready", "model": "moe", "context": {"cap": 65536, "mode": null, "modes": []},
        "oxe_hints": {"snapshot_keep": 2, "prefill_only": true, "hint_clear": true},
        "model_limits": {
            "flash": {"cap": 16384, "max_output": 2048, "capabilities": {
                "prefill_only": true, "snapshot_hint": 2, "hint_clear": true,
                "preserve_thinking": false, "park": "disk"}},
            "moe": {"cap": 65536, "max_output": null}
        }
    });
    let info = parse_info_for("http://h:1", &models, &status, Some("flash")).unwrap();
    assert_eq!(
        (info.limits_for.as_str(), info.n_ctx, info.max_output),
        ("flash", 16384, 2048)
    );
    assert!(info.prefill_only);
    assert_eq!(
        info.caps,
        EngineCaps {
            snapshot_keep: 2,
            hint_clear: true,
            preserve_thinking: Some(false),
            park_disk_only: true,
            cache_hints: true,
            prefill: true,
            ..EngineCaps::default()
        }
    );
    // The moe row has no block: the loaded engine's oxe_hints, preserve_thinking unknown.
    let info = parse_info("http://h:1", &models, &status).unwrap();
    assert_eq!(info.limits_for, "moe");
    assert!(info.prefill_only);
    assert_eq!(
        info.caps,
        EngineCaps {
            snapshot_keep: 2,
            hint_clear: true,
            preserve_thinking: None,
            park_disk_only: false,
            cache_hints: true,
            prefill: true,
            ..EngineCaps::default()
        }
    );
    // Neither: nothing advertised.
    let bare = json!({"state": "ready", "model": "moe", "context_size": 8192});
    let info = parse_info("http://h:1", &models, &bare).unwrap();
    assert!(!info.prefill_only);
    assert_eq!(info.caps, EngineCaps::default());
    // A capability block that claims nothing wins over oxe_hints.
    let mut none = status.clone();
    none["model_limits"]["moe"]["capabilities"] = json!({"prefill_only": false, "snapshot_hint": 0, "hint_clear": false, "preserve_thinking": false, "park": "none"});
    let info = parse_info("http://h:1", &models, &none).unwrap();
    assert!(!info.prefill_only);
    assert_eq!(info.caps.preserve_thinking, Some(false));
    assert_eq!(info.caps.snapshot_keep, 0);
}

/// F4 over HTTP: with no model loaded (`/props` 503) the info still carries the target's
/// capabilities and limits from `model_limits` (a Flash row: 16384 / 2048, no
/// `preserve_thinking`, disk park); after the load `/props` adds nothing that changes them.
#[test]
fn f4_capabilities_before_the_model_loads() {
    let server = FakeServer::start(
        vec![],
        FakeOpts {
            models: vec![
                FakeModel::moe("fake-model", 32768),
                FakeModel::flash("flash", 16384, Some(2048)),
            ],
            props_unloaded: true,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let info = engine.info(Some("flash")).expect("a /props 503 is not an error");
    assert_eq!((info.n_ctx, info.max_output), (16384, 2048));
    assert!(info.prefill_only);
    assert_eq!(info.caps.preserve_thinking, Some(false));
    assert!(info.caps.park_disk_only && info.caps.hint_clear);
    assert_eq!(info.caps.snapshot_keep, 2);
    let moe = engine.info(None).unwrap();
    assert_eq!(moe.caps.preserve_thinking, Some(true));
    server.set_props_unloaded(false);
    assert_eq!(engine.info(Some("flash")).unwrap(), info);
}

/// F14: while the controller loads (`state: loading`, no model, no context) the info is the
/// retryable `engine_busy`, not "no context size"; once loaded it works.
#[test]
fn f14_loading_controller_is_engine_busy() {
    let server = FakeServer::start(
        vec![],
        FakeOpts {
            loading: true,
            props_unloaded: true,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let e = engine.info(None).unwrap_err();
    assert_eq!(e.code, codes::ENGINE_BUSY, "{e}");
    server.set_loading(false);
    assert_eq!(engine.info(None).unwrap().model, "fake-model");
}

// ---------- EG5 ----------

/// EG5: `clear_hint` posts `{"scope":"hint","hint":h}` to the controller's
/// `/_local/cache/clear`; on a plain engine (404) it falls back to `/state-cache/clear`; 409 is
/// `busy`; a refused hint (400) is `engine_http`. The parent's state survives a child's clear.
#[test]
fn eg5_clear_hint_paths() {
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let p = prefix();
    let go = [WireMsg::user("go")];
    run(&engine, &request(&p, &go, 0, hints("parent"))).0.unwrap();
    run(&engine, &request(&p, &go, 0, hints("parent.s1"))).0.unwrap();
    engine.clear_hint("parent.s1").unwrap();
    assert_eq!(server.clears(), vec!["parent.s1".to_string()]);
    assert!(server.held("parent.s1").is_none());
    assert!(server.held("parent").is_some());
    engine.clear_hint("parent.s1").expect("idempotent");

    server.set_clear_busy(true);
    assert_eq!(engine.clear_hint("parent").unwrap_err().code, codes::BUSY);
    server.set_clear_busy(false);
    let e = engine.clear_hint("bad/hint").unwrap_err();
    assert_eq!(e.code, codes::ENGINE_HTTP);
    assert!(e.message.contains("HTTP 400"), "{e}");

    let plain = FakeServer::start(
        vec![],
        FakeOpts {
            controller: false,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    HttpEngine::new(&plain.url(), None)
        .unwrap()
        .clear_hint("x.s2")
        .unwrap();
    assert_eq!(plain.clears(), vec!["x.s2".to_string()]);

    // The exact request: path and body.
    let (url, log) = canned(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    HttpEngine::new(&url, None).unwrap().clear_hint("p.s1").unwrap();
    assert_eq!(
        log.lock().unwrap().as_slice(),
        ["POST /_local/cache/clear HTTP/1.1 {\"scope\":\"hint\",\"hint\":\"p.s1\"}".to_string()]
    );
}

/// B6 (wave H lane `harden`) over HTTP: the engine counts reasoning in `completion_tokens` and
/// reports `completion_tokens_details.reasoning_tokens` (the fake models it); a server that
/// leaves out `prompt_tokens_details.cached_tokens` gives `cached_missing`, never "0 cached".
#[test]
fn b6_reasoning_in_completion_and_missing_cached_tokens() {
    let turn = || ScriptTurn {
        reasoning: Some("think".into()),
        ..ScriptTurn::text("ok")
    };
    let server = FakeServer::start(vec![turn()], FakeOpts::default()).unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let (p, go) = (prefix(), [WireMsg::user("go")]);
    let (res, _) = run(&engine, &request(&p, &go, 4096, hints("s")));
    let u = res.unwrap().usage;
    assert!(!u.cached_missing);
    assert!(u.completion as usize > "think".len(), "{u:?}");

    let server = FakeServer::start(
        vec![turn()],
        FakeOpts {
            omit_cached_tokens: true,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let (res, _) = run(&engine, &request(&p, &go, 4096, hints("s")));
    let u = res.unwrap().usage;
    assert!(u.cached_missing, "{u:?}");
    assert_eq!(u.cached, 0);
}
