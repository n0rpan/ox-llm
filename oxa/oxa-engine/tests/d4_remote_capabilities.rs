//! Wave D D4: per-model cloud capabilities, the offline refusal and the remote request body.
use oxa_engine::{build_body, build_body_for, parse_info_for};
use oxa_proto::{codes, ChatRequest, FrozenPrefix, Hints, ThinkingLevel, WireMsg};
use serde_json::json;
use std::sync::Mutex;

static NET_POLICY: Mutex<()> = Mutex::new(());

struct PolicyReset;
impl Drop for PolicyReset {
    fn drop(&mut self) {
        oxa_proto::set_net_policy(false, false);
    }
}

fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), "SYS".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0; 32],
    }
}

fn request<'a>(prefix: &'a FrozenPrefix, messages: &'a [WireMsg], model: &'a str) -> ChatRequest<'a> {
    ChatRequest {
        max_tokens: 64,
        thinking: true,
        thinking_budget: None,
        thinking_level: Some(ThinkingLevel::High),
        preserve_thinking: true,
        hints: Hints {
            session: "session",
            idem_key: "idem",
            snapshot_keep: true,
            background: true,
            ..Hints::default()
        },
        ..ChatRequest::new(prefix, messages, model)
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
    let _reset = PolicyReset;
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

#[test]
fn a_remote_body_sends_the_requested_level_as_reasoning_effort() {
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let p = prefix();
    let messages = vec![WireMsg::user("hi")];
    let cases = [
        (ThinkingLevel::Off, "none"),
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Xhigh, "high"),
        (ThinkingLevel::Max, "high"),
    ];
    for (level, effort) in cases {
        let mut r = request(&p, &messages, "cloud-model");
        r.thinking = level.enabled();
        r.thinking_level = Some(level);
        let mut remote = Vec::new();
        build_body_for(&r, true, &mut remote);
        let body: serde_json::Value = serde_json::from_slice(&remote).unwrap();
        assert_eq!(body["reasoning_effort"], effort, "{level:?}");
        // A local engine only ever sees the boolean.
        let mut local = Vec::new();
        build_body_for(&r, false, &mut local);
        let local = String::from_utf8(local).unwrap();
        let on = level.enabled();
        assert!(local.contains(&format!("\"enable_thinking\":{on}")), "{local}");
        assert!(!local.contains("reasoning_effort"), "{local}");
    }
}

#[test]
fn openai_location_and_offline_policy_come_from_model_capabilities() {
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let models = json!({"data": [{"id": "cloud-model"}]});
    let mut status = cloud_status(32000, 200000);
    let caps = &mut status["model_limits"]["cloud-model"]["capabilities"];
    caps["remote_provider"] = json!("openai");
    caps["cancel"] = json!(false);
    let info = parse_info_for("http://127.0.0.1:8092", &models, &status, Some("cloud-model"))
        .expect("OpenAI capabilities");
    assert_eq!(info.caps.remote_provider, Some("openai"));
    assert_eq!(info.n_ctx, 200000);
    assert!(!info.caps.cancel);
    let _reset = PolicyReset;
    oxa_proto::set_net_policy(true, false);
    let result = parse_info_for("http://127.0.0.1:8092", &models, &status, Some("cloud-model"));
    oxa_proto::set_net_policy(false, false);
    assert_eq!(result.expect_err("offline refusal").code, codes::OFFLINE);
}

#[test]
fn requesting_cloud_info_does_not_classify_the_loaded_local_model_as_remote() {
    use oxa_engine::HttpEngine;
    use oxa_fake::{FakeModel, FakeOpts, FakeServer};
    use oxa_proto::Engine;
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let server = FakeServer::start(
        Vec::new(),
        FakeOpts {
            model: "local-model".to_string(),
            remote: Some("openai"),
            models: vec![FakeModel {
                alias: "cloud-model".to_string(),
                cap: 64000,
                max_output: Some(4096),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let info = engine.info(Some("cloud-model")).unwrap();
    assert_eq!(info.model, "local-model");
    assert_eq!(info.limits_for, "cloud-model");
    assert!(info.caps.remote);
    let p = prefix();
    let messages = vec![WireMsg::user("synthetic prompt")];
    assert_eq!(
        engine
            .count_prompt(&request(&p, &messages, "cloud-model"))
            .unwrap(),
        None
    );
    assert!(
        engine
            .count_prompt(&request(&p, &messages, "local-model"))
            .unwrap()
            .is_some(),
        "the unrelated loaded model must retain its local prompt-count endpoint"
    );
}

#[test]
fn unknown_provider_is_still_remote_and_refused_offline() {
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = PolicyReset;
    let models = json!({"data": [{"id": "cloud-model"}]});
    let mut status = cloud_status(64000, 64000);
    status["model_limits"]["cloud-model"]["capabilities"]["remote_provider"] = json!("future-provider");
    let info = parse_info_for("http://127.0.0.1:8092", &models, &status, Some("cloud-model")).unwrap();
    assert!(info.caps.remote);
    assert_eq!(info.caps.remote_provider, None);
    oxa_proto::set_net_policy(true, false);
    assert_eq!(
        parse_info_for("http://127.0.0.1:8092", &models, &status, Some("cloud-model"))
            .unwrap_err()
            .code,
        codes::OFFLINE
    );
}

#[test]
fn openai_http_chat_omits_local_cache_echo_and_prefill_controls() {
    use oxa_engine::HttpEngine;
    use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
    use oxa_proto::{CancelToken, Engine};
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let server = FakeServer::start(
        vec![ScriptTurn::text("reply")],
        FakeOpts {
            remote: Some("openai"),
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let info = engine.info(None).unwrap();
    assert!(info.caps.remote);
    let p = prefix();
    let messages = [WireMsg::user("hi")];
    engine
        .chat(
            &request(&p, &messages, &info.model),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    for field in [
        "cache_prompt",
        "chat_template_kwargs",
        "echo",
        "prefill_only",
        "snapshot_keep",
    ] {
        assert!(
            body.get(field).is_none(),
            "unexpected local field {field}: {body}"
        );
    }
    assert_eq!(body["reasoning_effort"], "high");
}

#[test]
fn request_budget_is_local_thinking_only_and_discovered_without_model_names() {
    let p = prefix();
    let messages = [WireMsg::user("hi")];
    let mut req = request(&p, &messages, "synthetic");
    req.thinking_budget = Some(0);
    let mut body = Vec::new();
    build_body(&req, &mut body);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["thinking_budget"],
        0
    );
    body.clear();
    build_body_for(&req, true, &mut body);
    assert!(serde_json::from_slice::<serde_json::Value>(&body)
        .unwrap()
        .get("thinking_budget")
        .is_none());
    for (thinking, total) in [(false, 64), (true, 0)] {
        req.thinking = thinking;
        req.max_tokens = total;
        body.clear();
        build_body(&req, &mut body);
        assert!(serde_json::from_slice::<serde_json::Value>(&body)
            .unwrap()
            .get("thinking_budget")
            .is_none());
    }
    let models = json!({"data":[{"id":"synthetic"}]});
    for supported in [false, true] {
        let status =
            json!({"model_alias":"synthetic","n_ctx":8192,"thinking_budget":supported,"oxe_hints":{}});
        assert_eq!(
            parse_info_for("http://127.0.0.1:8092", &models, &status, None)
                .unwrap()
                .caps
                .thinking_budget,
            supported
        );
        let status = json!({"model":"synthetic","context_size":8192,"model_limits":{"synthetic":{"cap":8192,"capabilities":{"thinking_budget":supported}}}});
        assert_eq!(
            parse_info_for("http://127.0.0.1:8092", &models, &status, None)
                .unwrap()
                .caps
                .thinking_budget,
            supported
        );
    }
}

#[test]
fn output_schema_capability_and_remote_only_serialization() {
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let models = json!({"data":[{"id":"cloud-model"}]});
    for remote in [false, true] {
        for advertised in [false, true] {
            let mut status = cloud_status(64000, 64000);
            status["model_limits"]["cloud-model"]["capabilities"]["remote"] = json!(remote);
            status["model_limits"]["cloud-model"]["capabilities"]["response_format"] = json!(advertised);
            let info =
                parse_info_for("http://127.0.0.1:8092", &models, &status, Some("cloud-model")).unwrap();
            assert_eq!(info.caps.response_format, remote && advertised);
        }
    }
    let p = prefix();
    let schema = json!({"type":"object", "required":["answer"], "properties":{"answer":{"type":"number"}}});
    let mut r = request(&p, &[], "cloud-model");
    r.output_schema = Some(&schema);
    for remote in [false, true] {
        let mut body = vec![];
        build_body_for(&r, remote, &mut body);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if remote {
            assert_eq!(body["response_format"]["type"], "json_schema");
            assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
        } else {
            assert!(
                body.get("response_format").is_none(),
                "local engine rejects response_format"
            );
        }
    }
}

/// Issue #574: a cloud model reached through oxc carries the request's own Idempotency-Key and
/// session header (oxc's headers, never forwarded upstream); the local-only hint headers stay off.
#[test]
fn a_remote_request_sends_the_idempotency_key_and_session() {
    use oxa_engine::HttpEngine;
    use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
    use oxa_proto::{CancelToken, Engine};
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let server = FakeServer::start(
        vec![ScriptTurn::text("reply")],
        FakeOpts {
            remote: Some("openrouter"),
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let engine = HttpEngine::new(&server.url(), None).unwrap();
    let info = engine.info(None).unwrap();
    assert!(info.caps.remote);
    let p = prefix();
    let messages = [WireMsg::user("hi")];
    engine
        .chat(
            &request(&p, &messages, &info.model),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].idem_key, "idem");
    assert_eq!(seen[0].session, "session");
    let has = |name: &str| seen[0].headers.iter().any(|(k, _)| k == name);
    for header in ["x-oxe-priority", "x-oxe-snapshot", "x-oxe-turn-kind"] {
        assert!(!has(header), "unexpected {header}: {:?}", seen[0].headers);
    }
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "x-session-id" && value == "session"),
        "{:?}",
        seen[0].headers
    );
}

/// Issue #574: a cut or silent stream before the first data line must not be re-sent by the
/// engine for a cloud model (each send is a new, billed upstream generation); the core's single
/// retry layer decides. A refused connection reached nothing and is still retried.
#[test]
fn a_remote_cut_or_stall_before_data_is_not_resent_by_the_engine() {
    use oxa_engine::{HttpEngine, StreamOpts};
    use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn};
    use oxa_proto::{CancelToken, Engine};
    let _policy = NET_POLICY.lock().unwrap_or_else(|p| p.into_inner());
    for fault in [Fault::DropAt { bytes: 0 }, Fault::Stall { bytes: 0, ms: 500 }] {
        let server = FakeServer::start(
            vec![
                ScriptTurn {
                    fault: Some(fault.clone()),
                    ..ScriptTurn::text("lost")
                },
                ScriptTurn::text("second"),
            ],
            FakeOpts {
                remote: Some("openrouter"),
                ..FakeOpts::default()
            },
        )
        .unwrap();
        let engine = HttpEngine::new(&server.url(), None).unwrap().with_opts(
            StreamOpts {
                silence_limit_ms: 100,
                ..StreamOpts::default()
            },
            2,
        );
        let info = engine.info(None).unwrap();
        let p = prefix();
        let messages = [WireMsg::user("hi")];
        let err = engine
            .chat(
                &request(&p, &messages, &info.model),
                &CancelToken::new(),
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(err.code, codes::STREAM_BROKEN, "{fault:?}");
        let seen = server.requests();
        assert_eq!(
            seen.len(),
            1,
            "one upstream request, no engine-level resend: {fault:?}"
        );
        assert_eq!(seen[0].idem_key, "idem");
    }
}
