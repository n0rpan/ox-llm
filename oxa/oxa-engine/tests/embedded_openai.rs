#![cfg(feature = "in-process-gateway")]
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
#[allow(dead_code)]
#[allow(unexpected_cfgs)] // Shared gateway fixture tests its transport feature.
#[path = "../../oxc-gateway/tests/common/openai.rs"]
mod mock;
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/skeleton.rs"]
mod skeleton;
use oxa_engine::{EmbeddedEngine, Gateway, GatewayOptions, GatewayRuntime};
use oxa_proto::{
    codes, CancelToken, ChatRequest, Engine, FrozenPrefix, ProviderHistory, StreamEvent, WireMsg,
};
use oxc_gateway::{
    billing::SubscriptionLimits,
    catalog::{Access, Model, Provider},
    pricing::Price,
    runtime::{Billing, Subscription},
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};

fn install(fixture: &skeleton::Fixture, subscription: bool) {
    if subscription {
        fs::write(&fixture.key_path, "invalid synthetic API-key JSON").unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.key_path.parent().unwrap().join("chatgpt-auth.json"))
            .unwrap();
        write!(
            file,
            "{}",
            json!({"access_token":"synthetic-access","refresh_token":"synthetic-refresh",
            "account_id":"synthetic-account","expires_at":2_000_000_000})
        )
        .unwrap();
    } else {
        fs::write(
            &fixture.key_path,
            json!({"openai":anthropic::CREDENTIAL}).to_string(),
        )
        .unwrap();
    }
}
fn engine(fixture: &skeleton::Fixture, base: &str, subscription: bool) -> EmbeddedEngine {
    let endpoint = if subscription {
        base.trim_end_matches("/v1").to_owned() + "/backend-api/codex"
    } else {
        base.into()
    };
    let model = Model::new(
        skeleton::MODEL,
        if subscription {
            Provider::ChatGpt
        } else {
            Provider::OpenAi
        },
        "synthetic-openai",
        &endpoint,
        1000,
        100,
    )
    .unwrap();
    let billing = if subscription {
        Billing::Subscription
    } else {
        Billing::Metered(Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]).unwrap())
    };
    let runtime = GatewayRuntime::new(
        [(model, Some(billing))],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        GatewayOptions {
            access: Access::Enabled,
            daily_budget_usd: Some(1.0),
            subscription_limits: subscription.then(|| SubscriptionLimits::new(10, 20000).unwrap()),
            subscription: subscription.then(|| Subscription {
                refresh_endpoint: base.trim_end_matches("/v1").to_owned() + "/token",
                client_id: "synthetic-client".into(),
                timeout: Duration::from_secs(15),
            }),
            ..GatewayOptions::default()
        },
    )
    .unwrap();
    EmbeddedEngine::from_gateway(Gateway::new(runtime).unwrap())
}
fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".into(), "Synthetic instructions".into())],
        tools_json: json!([{"type":"function","function":{"name":"read","parameters":{"type":"object"}}},
            {"type":"function","function":{"name":"bash","parameters":{"type":"object"}}}])
        .to_string(),
        sha256: [0; 32],
    }
}
fn final_reply() -> mock::Reply {
    mock::Reply::events(vec![
        json!({"type":"response.completed","response":{"status":"completed",
        "output":[{"type":"message","id":"msg_done","role":"assistant","content":[{"type":"output_text","text":"Done"}]}],
        "usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}),
    ])
}
fn rows(fixture: &skeleton::Fixture) -> Vec<Value> {
    fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|ext| ext == "jsonl")
                .then(|| fs::read_to_string(path).unwrap())
        })
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn both_modes_done_only_and_partial_tool_arguments_survive_restart_and_private_roundtrip() {
    let replies: [fn() -> mock::Reply; 3] = [
        mock::Reply::multi_tool_no_deltas,
        mock::Reply::multi_tool_partial_deltas,
        mock::Reply::multi_tool_full_deltas,
    ];
    for subscription in [false, true] {
        for reply in replies {
            let peer = mock::MockOpenAi::start(vec![reply(), final_reply()]);
            let fixture = skeleton::Fixture::for_endpoint(&peer.base_url());
            install(&fixture, subscription);
            let first = engine(&fixture, &peer.base_url(), subscription);
            let prefix = prefix();
            let mut messages = vec![WireMsg::user("Synthetic task")];
            let mut turn = first
                .chat(
                    &ChatRequest::new(&prefix, &messages, skeleton::MODEL),
                    &CancelToken::new(),
                    &mut |_| {},
                )
                .unwrap();
            assert_eq!(turn.msg.tool_calls.len(), 3);
            assert_eq!((turn.usage.prompt, turn.usage.completion), (12, 9));
            let history = turn.msg.provider_history.as_ref().unwrap();
            for (index, (call, item)) in turn.msg.tool_calls.iter().zip(history.items()).enumerate() {
                assert_eq!(call.id, format!("call_synthetic_{index}"));
                assert_eq!(item["call_id"], call.id);
                assert_eq!(item["arguments"], call.args_raw);
                assert!(serde_json::from_str::<Value>(&call.args_raw).unwrap().is_object());
            }
            let saved = history.to_session_value();
            turn.msg.provider_history = Some(ProviderHistory::from_session_value(&saved).unwrap());
            let calls = turn.msg.tool_calls.clone();
            messages.push(turn.msg);
            for call in calls {
                messages.push(WireMsg::tool_result(call.id, "Synthetic result"));
            }
            drop(first);
            let restarted = engine(&fixture, &peer.base_url(), subscription);
            let turn = restarted
                .chat(
                    &ChatRequest::new(&prefix, &messages, skeleton::MODEL),
                    &CancelToken::new(),
                    &mut |_| {},
                )
                .unwrap();
            assert_eq!(turn.msg.content.as_deref(), Some("Done"));
            let requests = peer.requests();
            assert_eq!(requests.len(), 2);
            let native = requests[1].body["input"].as_array().unwrap();
            assert_eq!(
                native
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .count(),
                3
            );
            assert_eq!(
                native
                    .iter()
                    .filter(|item| item["type"] == "function_call_output")
                    .count(),
                3
            );
            let recorded = rows(&fixture);
            assert_eq!(recorded.len(), 2);
            assert!(recorded
                .iter()
                .all(|row| row["status"] == 200 && row["settled"] == "usage"));
            if subscription {
                assert!(recorded
                    .iter()
                    .all(|row| row["cost_usd"] == 0.0 && row["subscription_requests"] == 1));
                assert_eq!(recorded[0]["subscription_tokens"], 21);
                assert_eq!(recorded[1]["subscription_tokens"], 9);
            } else {
                assert_eq!(recorded[0]["cost_usd"], json!(0.000051));
            }
            messages[0].content = Some("Changed task".into());
            let error = restarted
                .chat(
                    &ChatRequest::new(&prefix, &messages, skeleton::MODEL),
                    &CancelToken::new(),
                    &mut |_| {},
                )
                .unwrap_err();
            assert_eq!(error.code, codes::ENGINE_HTTP);
            assert_eq!(peer.requests().len(), 2);
            let refused = rows(&fixture).pop().unwrap();
            assert_eq!(refused["status"], 400);
            assert_eq!(refused["error"], "provider_history_mismatch");
            assert_eq!(refused["cost_usd"], 0.0);
            peer.assert_healthy();
        }
    }
}

#[test]
fn terminal_tool_delivery_cancellation_settles_private_usage_in_both_modes() {
    for subscription in [false, true] {
        let peer = mock::MockOpenAi::start(vec![mock::Reply::multi_tool_no_deltas()]);
        let fixture = skeleton::Fixture::for_endpoint(&peer.base_url());
        install(&fixture, subscription);
        let engine = engine(&fixture, &peer.base_url(), subscription);
        let prefix = prefix();
        let messages = [WireMsg::user("Synthetic task")];
        let cancel = CancelToken::new();
        let error = engine
            .chat(
                &ChatRequest::new(&prefix, &messages, skeleton::MODEL),
                &cancel,
                &mut |event| {
                    if matches!(event, StreamEvent::ToolArgs { .. }) {
                        cancel.cancel();
                    }
                },
            )
            .unwrap_err();
        assert_eq!(error.code, codes::ABORTED);
        let row = rows(&fixture).pop().unwrap();
        assert_eq!(row["status"], 499);
        assert_eq!(row["error"], "request_cancelled");
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["prompt_tokens"], 12);
        assert_eq!(row["completion_tokens"], 9);
        if subscription {
            assert_eq!(row["cost_usd"], 0.0);
            assert_eq!(row["subscription_tokens"], 21);
        } else {
            assert_eq!(row["cost_usd"], json!(0.000051));
        }
    }
}

#[test]
fn unsafe_native_identity_is_permanent_and_settles_known_usage_without_tool_delivery() {
    for subscription in [false, true] {
        let secret = if subscription {
            "synthetic-access"
        } else {
            anthropic::CREDENTIAL
        };
        let reply = mock::Reply::events(vec![json!({"type":"response.completed","response":{
            "status":"completed","output":[{"type":"function_call","id":"fc_refused",
                "call_id":"call_refused","name":format!("read{secret}"),"arguments":"{}","status":"completed"}],
            "usage":{"input_tokens":12,"output_tokens":9,"total_tokens":21}}})]);
        let peer = mock::MockOpenAi::start(vec![reply]);
        let fixture = skeleton::Fixture::for_endpoint(&peer.base_url());
        install(&fixture, subscription);
        let engine = engine(&fixture, &peer.base_url(), subscription);
        let prefix = prefix();
        let messages = [WireMsg::user("Synthetic task")];
        let mut tools = 0;
        let error = engine
            .chat(
                &ChatRequest::new(&prefix, &messages, skeleton::MODEL),
                &CancelToken::new(),
                &mut |event| {
                    if matches!(event, StreamEvent::ToolArgs { .. }) {
                        tools += 1;
                    }
                },
            )
            .unwrap_err();
        assert_eq!(error.code, codes::STREAM_BROKEN);
        assert!(error.message.starts_with(oxa_proto::CUT_BY_REFUSAL));
        assert!(!error.message.contains(secret));
        assert_eq!(tools, 0);
        let row = rows(&fixture).pop().unwrap();
        assert_eq!(row["status"], 502);
        assert_eq!(row["error"], "cloud_upstream_stream_broken");
        assert_eq!(row["settled"], "usage");
        if subscription {
            assert_eq!(row["cost_usd"], 0.0);
            assert_eq!(row["subscription_tokens"], 21);
        } else {
            assert_eq!(row["cost_usd"], json!(0.000051));
        }
        assert_eq!(peer.requests().len(), 1);
    }
}
