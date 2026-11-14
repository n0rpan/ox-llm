//! Wave D D4: per-model cloud capabilities, the offline refusal and the remote request body.
use oxa_engine::{build_body, build_body_for, parse_info_for};
use oxa_proto::{codes, ChatRequest, FrozenPrefix, Hints, TurnKind, WireMsg};
use serde_json::json;
use std::sync::Mutex;

static NET_POLICY: Mutex<()> = Mutex::new(());

fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), "SYS".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0; 32],
    }
}

fn request<'a>(prefix: &'a FrozenPrefix, messages: &'a [WireMsg], model: &'a str) -> ChatRequest<'a> {
    ChatRequest {
        prefix,
        messages,
        model,
        max_tokens: 64,
        thinking: true,
        preserve_thinking: true,
        sampling: None,
        hints: Hints {
            session: "session",
            idem_key: "idem",
            kind: TurnKind::Chat,
            background: false,
            snapshot_keep: true,
        },
    }
}

fn cloud_status(cap: u32, context_window: u32) -> serde_json::Value {
    json!({
        "state": "ready", "model": "local-model", "context": {"cap": 8192},
        "model_limits": {
            "cloud-model": {
                "cap": cap, "context_window": context_window, "max_output": 4096,
                "capabilities": {
                    "remote": true, "remote_provider": "openrouter", "echo": false,
                    "cache_hints": false, "cancel": true, "prefill": false,
                    "context_window": context_window
                }
            },
            "local-model": {
                "cap": 8192, "max_output": 1024,
                "capabilities": {"remote": false, "echo": true, "cache_hints": true,
                                 "cancel": true, "prefill": true, "context_window": 8192}
            }
        }
    })
}

#[test]
fn a_remote_row_publishes_its_capabilities_and_context_window() {
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let models = json!({"data": [{"id": "local-model"}, {"id": "cloud-model"}]});
    let info = parse_info_for(
        "http://127.0.0.1:8092",
        &models,
        &cloud_status(32000, 64000),
        Some("cloud-model"),
    )
    .expect("parsed");
    assert_eq!(info.limits_for, "cloud-model");
    assert_eq!(info.n_ctx, 64000, "context_window from the capability row wins");
    assert_eq!(info.max_output, 4096);
    assert!(info.caps.remote);
    assert_eq!(info.caps.remote_provider, Some("openrouter"));
    assert!(info.caps.cancel);
    assert!(!info.caps.prefill);
    assert!(!info.caps.echo);
    assert!(!info.caps.cache_hints);
    assert_eq!(info.caps.context_window, Some(64000));
    assert!(!info.prefill_only);
}

#[test]
fn offline_refuses_a_remote_model_before_any_connect() {
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let models = json!({"data": [{"id": "local-model"}, {"id": "cloud-model"}]});
    oxa_proto::set_net_policy(true, false);
    let error = parse_info_for(
        "http://127.0.0.1:8092",
        &models,
        &cloud_status(64000, 64000),
        Some("cloud-model"),
    )
    .expect_err("offline");
    oxa_proto::set_net_policy(false, false);
    assert_eq!(error.code, codes::OFFLINE);
    assert!(error.message.contains("cloud-model"), "{}", error.message);
    assert!(error.message.contains("offline mode is on"), "{}", error.message);
}

#[test]
fn a_remote_body_skips_local_only_fields_and_uses_reasoning_effort() {
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let p = prefix();
    let messages = vec![WireMsg::user("hi")];
    let r = request(&p, &messages, "cloud-model");
    let mut local = Vec::new();
    build_body(&r, &mut local);
    let local = String::from_utf8(local).unwrap();
    assert!(local.contains("\"cache_prompt\":true"));
    assert!(local.contains("\"chat_template_kwargs\":{\"preserve_thinking\":true,"));

    let mut remote = Vec::new();
    build_body_for(&r, true, &mut remote);
    let remote = String::from_utf8(remote).unwrap();
    assert!(!remote.contains("cache_prompt"), "{remote}");
    assert!(!remote.contains("chat_template_kwargs"), "{remote}");
    assert!(!remote.contains("preserve_thinking"), "{remote}");
    assert!(remote.contains("\"reasoning_effort\":\"high\""), "{remote}");
    assert!(
        remote.contains("\"stream_options\":{\"include_usage\":true}"),
        "{remote}"
    );
}
