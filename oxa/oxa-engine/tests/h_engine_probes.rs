//! Wave H, lane cache: what the HTTP engine asks the server, and how often.
//! A4 (tiny mechanics A/B): a plain engine (no controller) answers 404 on `/_local/status`;
//! oxa probed it twice per start. It is asked once per endpoint and process.

use oxa_engine::HttpEngine;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
use oxa_proto::Engine;

#[test]
fn a_plain_engine_is_probed_for_the_controller_status_once() {
    let srv = FakeServer::start(
        vec![ScriptTurn::text("ok")],
        FakeOpts {
            controller: false,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let a = HttpEngine::new(&srv.url(), None).unwrap();
    let info = a.info(None).expect("info from /props");
    assert_eq!(info.model, "fake-model");
    a.info(Some("fake-model")).expect("info again");
    // A second client of the same endpoint in the same process (dispatch builds more than one).
    let b = HttpEngine::new(&srv.url(), None).unwrap();
    b.info(None).expect("info from another client");
    assert_eq!(srv.hits("GET", "/_local/status"), 1, "the 404 is remembered");
    assert_eq!(
        srv.hits("GET", "/props"),
        3,
        "the engine's own document is read each time"
    );

    // The hint clear goes straight to the engine's own endpoint after the first 404.
    a.clear_hint("x.s1").expect("clear");
    a.clear_hint("x.s2").expect("clear");
    assert_eq!(srv.hits("POST", "/_local/cache/clear"), 1);
    assert_eq!(srv.hits("POST", "/state-cache/clear"), 2);
}

#[test]
fn a_controller_status_is_read_every_time() {
    let srv = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).unwrap();
    let a = HttpEngine::new(&srv.url(), None).unwrap();
    a.info(None).unwrap();
    a.info(None).unwrap();
    assert_eq!(
        srv.hits("GET", "/_local/status"),
        2,
        "live data (queue, loaded model)"
    );
}

/// OXA-EXACT-COUNTS: `count_prompt` asks `/apply-template` with the request's own body and
/// `return_tokens`; the count equals the `prompt_tokens` the same request then reports.
#[test]
fn count_prompt_equals_the_reported_prompt_tokens() {
    use oxa_proto::{CancelToken, ChatRequest, FrozenPrefix, Hints, TurnKind, WireMsg};
    let srv = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).unwrap();
    let eng = HttpEngine::new(&srv.url(), None).unwrap();
    let prefix = FrozenPrefix {
        sections: vec![("preamble".to_string(), "You are a test agent.".to_string())],
        tools_json: r#"[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]"#
            .to_string(),
        sha256: [0u8; 32],
    };
    let msgs = vec![
        WireMsg::user("count me"),
        WireMsg::assistant("sure"),
        WireMsg::user("é and more"),
    ];
    let req = ChatRequest {
        output_schema: None,
        prefix: &prefix,
        messages: &msgs,
        model: "fake-model",
        max_tokens: 64,
        thinking: false,
        thinking_budget: None,
        thinking_level: None,
        preserve_thinking: true,
        sampling: None,
        hints: Hints {
            session: "s1",
            idem_key: "",
            kind: TurnKind::Chat,
            background: false,
            snapshot_keep: false,
        },
    };
    let n = eng
        .count_prompt(&req)
        .expect("count")
        .expect("the fake counts exactly");
    let turn = eng.chat(&req, &CancelToken::new(), &mut |_| {}).expect("chat");
    assert_eq!(n, u64::from(turn.usage.prompt));
    assert_eq!(srv.hits("POST", "/apply-template"), 1);
}

/// A server without `/apply-template` (404): `Ok(None)`, asked once per endpoint.
#[test]
fn count_prompt_without_the_endpoint_is_none_once() {
    use oxa_proto::{ChatRequest, FrozenPrefix, Hints, TurnKind, WireMsg};
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { break };
            let mut buf = [0u8; 65536];
            let _ = s.read(&mut buf);
            h.fetch_add(1, Ordering::SeqCst);
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    let eng = HttpEngine::new(&url, None).unwrap();
    let prefix = FrozenPrefix {
        sections: vec![("p".to_string(), "s".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    };
    let msgs = vec![WireMsg::user("x")];
    let req = ChatRequest {
        output_schema: None,
        prefix: &prefix,
        messages: &msgs,
        model: "m",
        max_tokens: 8,
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
    assert_eq!(eng.count_prompt(&req).unwrap(), None);
    assert_eq!(eng.count_prompt(&req).unwrap(), None);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the 404 is remembered");
}
