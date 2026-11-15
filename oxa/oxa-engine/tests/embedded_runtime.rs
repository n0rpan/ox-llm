#![cfg(feature = "in-process-gateway")]
// Reuse the runner-owned wire peer and private inputs; do not rebuild admission
// or settlement inside the Engine tests.
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
#[path = "../../oxc-gateway/tests/common/skeleton.rs"]
mod skeleton;

use anthropic::{MockAnthropic, Reply};
use oxa_engine::{EmbeddedEngine, Gateway, GatewayOptions, GatewayRuntime};
use oxa_proto::{
    codes, CancelToken, ChatRequest, Engine, FrozenPrefix, Hints, StreamEvent, TurnKind, WireMsg,
};
use serde_json::json;
use std::{sync::Arc, thread, time::Duration};

fn engine(fixture: &skeleton::Fixture) -> EmbeddedEngine {
    let runtime = GatewayRuntime::new(
        fixture.config.clone(),
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        GatewayOptions::default(),
    )
    .unwrap();
    EmbeddedEngine::from_gateway(Gateway::new(runtime).unwrap()).unwrap()
}
fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".into(), "Synthetic system prompt".into())],
        tools_json: "[]".into(),
        sha256: [0; 32],
    }
}
fn request<'a>(prefix: &'a FrozenPrefix, messages: &'a [WireMsg]) -> ChatRequest<'a> {
    ChatRequest {
        prefix,
        messages,
        model: skeleton::MODEL,
        max_tokens: 1000,
        thinking: false,
        preserve_thinking: false,
        sampling: None,
        hints: Hints {
            session: "synthetic-session",
            idem_key: "synthetic-request-id",
            kind: TurnKind::Chat,
            background: false,
            snapshot_keep: false,
        },
    }
}

#[test]
fn real_gateway_returns_a_turn_and_delivers_on_the_calling_thread() {
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic λ reply", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let engine = engine(&fixture);
    let info = engine.info(None).unwrap();
    assert!(info.caps.remote);
    assert_eq!(info.model, skeleton::MODEL);
    assert_eq!(info.n_ctx, 1024);
    assert!(
        mock.requests().is_empty(),
        "discovery cannot contact the provider"
    );
    assert_eq!(std::fs::read_dir(&fixture.ledger_dir).unwrap().count(), 0);
    let caller = thread::current().id();
    let mut text = String::new();
    let prefix = prefix();
    let messages = [WireMsg::user("Synthetic request")];
    let turn = engine
        .chat(&request(&prefix, &messages), &CancelToken::new(), &mut |event| {
            assert_eq!(thread::current().id(), caller);
            if let StreamEvent::Text(delta) = event {
                text.push_str(&delta);
            }
        })
        .unwrap();
    assert_eq!(text, "Synthetic λ reply");
    assert_eq!(turn.msg.content.as_deref(), Some(text.as_str()));
    assert_eq!((turn.usage.prompt, turn.usage.completion), (21, 5));
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].body["max_tokens"], 16);
    mock.assert_healthy();
}

#[test]
fn live_oxa_cancellation_interrupts_a_provider_waiting_for_headers() {
    let (reply, _gate) = Reply::text("Synthetic reply", 11, 7, 3, 5).gated();
    let mock = Arc::new(MockAnthropic::start(vec![reply]));
    let fixture = skeleton::Fixture::new(&mock);
    let engine = engine(&fixture);
    let cancel = CancelToken::new();
    let watcher = thread::spawn({
        let mock = mock.clone();
        let cancel = cancel.clone();
        move || {
            mock.wait_for_requests(1);
            cancel.cancel();
        }
    });
    // A bounded caller wait detects a bridge that only polls between chunks.
    let worker = thread::spawn(move || {
        let prefix = prefix();
        engine.chat(&request(&prefix, &[]), &cancel, &mut |_| {
            panic!("the gated provider cannot emit a chunk")
        })
    });
    let (sent, received) = std::sync::mpsc::channel();
    thread::spawn(move || sent.send(worker.join().unwrap()).unwrap());
    let result = received.recv_timeout(Duration::from_secs(2)).unwrap();
    watcher.join().unwrap();
    assert_eq!(result.unwrap_err().code, codes::ABORTED);
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn budget_and_credential_refusals_keep_the_existing_engine_error_classes() {
    let mock = MockAnthropic::start(vec![]);
    let mut fixture = skeleton::Fixture::new(&mock);
    fixture.config["cloud"]["daily_budget_usd"] = json!(0.000000001);
    let prefix = prefix();
    let result = engine(&fixture).chat(&request(&prefix, &[]), &CancelToken::new(), &mut |_| {
        panic!("a refusal cannot emit reply data")
    });
    assert_eq!(result.unwrap_err().code, codes::BUDGET_EXHAUSTED);
    std::fs::remove_file(&fixture.key_path).unwrap();
    let result = engine(&fixture).chat(&request(&prefix, &[]), &CancelToken::new(), &mut |_| {
        panic!("a refusal cannot emit reply data")
    });
    assert_eq!(result.unwrap_err().code, "cloud_credentials_unavailable");
    assert!(mock.requests().is_empty());
}
