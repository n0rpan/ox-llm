#![cfg(all(feature = "upstream-openai", feature = "anthropic-client", feature = "ledger"))]
#[path = "common/openai.rs"]
mod mock;
use mock::{MockOpenAi, Reply};
use oxc_gateway::openai::{
    chatgpt_tokens,
    client::{self, Auth, Cancellation, Chat, Client, Control, Event},
    request,
};
use oxc_gateway::{rate_limits, secrets};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
const KEY: &str = "synthetic-openai-credential";
struct Fixture {
    root: PathBuf,
    key: secrets::Key,
}
impl Fixture {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "openai-client-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("keys.json");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        write!(f, "{}", json!({"openai":{"api_key":KEY}})).unwrap();
        let key = secrets::read(&path, "openai").unwrap();
        Self { root, key }
    }
    fn token_path(&self, fresh: bool) -> PathBuf {
        let path = self.root.join("chatgpt-auth.json");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        write!(f,"{}",json!({"access_token":KEY,"refresh_token":"synthetic-refresh","account_id":"synthetic-account","expires_at":if fresh{now+3600}else{1}})).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn chat<'a>(base: &'a str, payload: &'a Value, replay: &'a [Option<request::ReplayItems>]) -> Chat<'a> {
    Chat {
        base_url: base,
        upstream_model: "synthetic-model",
        alias: "synthetic-alias",
        max_output: Some(128),
        payload,
        replay,
        created: 7,
    }
}
fn payload() -> Value {
    json!({"messages":[{"role":"system","content":"Synthetic instructions"},{"role":"user","content":"Synthetic task"}],"stream":true,"stream_options":{"include_usage":false}})
}
#[tokio::test]
async fn both_modes_three_tools_done_only_partial_and_empty_completed_output() {
    for subscription in [false, true] {
        for reply in [
            Reply::multi_tool_no_deltas(),
            Reply::multi_tool_partial_deltas(),
            Reply::multi_tool_full_deltas(),
        ] {
            let mock = MockOpenAi::start(vec![reply]);
            assert!(mock.address().ip().is_loopback());
            let fixture = Fixture::new();
            let base = if subscription {
                mock.base_url().trim_end_matches("/v1").to_owned() + "/backend-api/codex"
            } else {
                mock.base_url()
            };
            let path = fixture.token_path(true);
            let auth = if subscription {
                Auth::Subscription {
                    expected_account_id: "synthetic-account",
                    token_path: &path,
                    refresh: client::Refresh {
                        endpoint: &base,
                        client_id: "synthetic-client",
                        timeout: std::time::Duration::from_secs(1),
                    },
                }
            } else {
                Auth::ApiKey(&fixture.key)
            };
            let mut chunks = vec![];
            let complete = Client::new()
                .unwrap()
                .stream(
                    chat(&base, &payload(), &[]),
                    auth,
                    &Cancellation::default(),
                    &mut |e| {
                        if let Event::Chunk(c) = e {
                            chunks.push(c)
                        }
                        Control::Continue
                    },
                )
                .await
                .unwrap();
            let calls = complete.assistant["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 3);
            for (i, call) in calls.iter().enumerate() {
                assert_eq!(call["id"], format!("call_synthetic_{i}"));
                assert!(!call["function"]["arguments"].as_str().unwrap().is_empty());
                let _: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
            }
            let mut assembler = oxa_engine::ReplyAssembler::new();
            for chunk in &chunks {
                assembler.chunk_value(chunk).unwrap();
            }
            // The request opts out of public usage; provide the trusted private
            // terminal counts as the gateway owner does before assembly finishes.
            assembler.chunk_value(&json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":9,"total_tokens":21}})).unwrap();
            let turn = assembler.finish(oxa_proto::ThinkingLevel::Off).unwrap();
            assert_eq!(turn.msg.tool_calls.len(), calls.len());
            for (streamed, call) in turn.msg.tool_calls.iter().zip(calls) {
                assert_eq!(streamed.id, call["id"].as_str().unwrap());
                assert_eq!(streamed.name, call["function"]["name"].as_str().unwrap());
                assert_eq!(streamed.args_raw, call["function"]["arguments"].as_str().unwrap());
            }
            let usage = complete.usage.unwrap();
            assert_eq!(usage.prompt, Some(12));
            assert_eq!(usage.output, Some(9));
            assert_eq!(usage.cache_write, Some(0));
            assert!(chunks.iter().all(|c| c.get("usage").is_none()));
            let requests = mock.wait_for_requests(1);
            let r = &requests[0];
            assert_eq!(r.header("authorization"), Some(format!("Bearer {KEY}").as_str()));
            assert_eq!(r.body["store"], false);
            assert!(r.body.get("previous_response_id").is_none());
            if subscription {
                assert_eq!(r.target, "/backend-api/codex/responses");
                assert_eq!(r.body["instructions"], "Synthetic instructions");
                assert_eq!(r.header("chatgpt-account-id"), Some("synthetic-account"));
                assert!(r.body.get("max_output_tokens").is_none())
            } else {
                assert_eq!(r.target, "/v1/responses");
                assert_eq!(r.body["max_output_tokens"], 128);
                assert!(r.header("chatgpt-account-id").is_none())
            }
            mock.assert_healthy();
        }
    }
}

fn answer() -> Reply {
    Reply::events(vec![
        json!({"type":"response.completed","response":{"status":"completed","output":[{"id":"msg_synthetic","type":"message","role":"assistant","content":[{"type":"output_text","text":"Synthetic answer"}]}],"usage":{"input_tokens":20,"output_tokens":7,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2}}}}),
    ])
}
fn tool_turn() -> Reply {
    let mut events = vec![
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_synthetic","encrypted_content":"opaque-synthetic-reasoning","summary":[]}}),
    ];
    for i in 0..3 {
        events.push(json!({"type":"response.output_item.done","output_index":i+1,"item":{"type":"function_call","id":format!("fc_{i}"),"call_id":format!("call_{i}"),"name":"read","arguments":format!("{{\"path\":\"file-{i}\"}}"),"status":"completed"}}));
    }
    events.push(json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":12,"output_tokens":9}}}));
    Reply::events(events)
}
#[tokio::test]
async fn persisted_three_tool_history_continues_identically_in_new_process_both_modes() {
    for subscription in [false, true] {
        let mock = MockOpenAi::start(vec![tool_turn(), answer(), answer()]);
        let fixture = Fixture::new();
        let base = mock.base_url();
        let path = fixture.token_path(true);
        let auth = || {
            if subscription {
                Auth::Subscription {
                    expected_account_id: "synthetic-account",
                    token_path: &path,
                    refresh: client::Refresh {
                        endpoint: &base,
                        client_id: "synthetic-client",
                        timeout: std::time::Duration::from_secs(1),
                    },
                }
            } else {
                Auth::ApiKey(&fixture.key)
            }
        };
        let client = Client::new().unwrap();
        let completed = client
            .stream(
                chat(&base, &payload(), &[]),
                auth(),
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .unwrap();
        assert_eq!(completed.replay_items.len(), 4);
        let mut next = payload();
        let messages = next["messages"].as_array_mut().unwrap();
        messages.push(completed.assistant);
        for i in 0..3 {
            messages.push(json!({"role":"tool","tool_call_id":format!("call_{i}"),"content":if i==1{"failed: synthetic permission denied"}else{"synthetic result"},"is_error":i==1}));
        }
        let replay = vec![Some(request::ReplayItems::new(completed.replay_items.to_vec()))];
        let refused = client
            .stream(
                chat(&base, &next, &[]),
                auth(),
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(refused, client::Error::Request("messages"));
        assert_eq!(mock.requests().len(), 1);
        client
            .stream(
                chat(&base, &next, &replay),
                auth(),
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .unwrap();
        let history = fixture.root.join("history.json");
        fs::write(&history,json!({"payload":next,"items":completed.replay_items.as_ref(),"subscription":subscription,"base":base,"token_path":path}).to_string()).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "new_process_replay_child", "--nocapture"])
            .env_clear()
            .env("HOME", &fixture.root)
            .env("PATH", "/usr/bin:/bin")
            .env("OPENAI_REPLAY_FIXTURE", &history)
            .output()
            .unwrap();
        assert!(result.status.success(), "new-process mock continuation failed");
        let requests = mock.wait_for_requests(3);
        assert_eq!(
            requests[1].body_bytes, requests[2].body_bytes,
            "new-process serialized replay must be byte-identical"
        );
        let input = requests[2].body["input"].as_array().unwrap();
        let opaque = input.iter().position(|v| v["type"] == "reasoning").unwrap();
        assert_eq!(input[opaque]["encrypted_content"], "opaque-synthetic-reasoning");
        assert_eq!(
            input
                .iter()
                .filter(|v| v["type"] == "function_call_output")
                .count(),
            3
        );
        assert!(input
            .iter()
            .any(|v| v["output"] == "failed: synthetic permission denied"));
        mock.assert_healthy();
    }
}
#[test]
fn new_process_replay_child() {
    let Some(path) = std::env::var_os("OPENAI_REPLAY_FIXTURE") else {
        return;
    };
    let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let fixture = Fixture::new();
    let replay = [Some(request::ReplayItems::new(
        saved["items"].as_array().unwrap().clone(),
    ))];
    let base = saved["base"].as_str().unwrap();
    let path = PathBuf::from(saved["token_path"].as_str().unwrap());
    let auth = if saved["subscription"] == true {
        Auth::Subscription {
            expected_account_id: "synthetic-account",
            token_path: &path,
            refresh: client::Refresh {
                endpoint: base,
                client_id: "synthetic-client",
                timeout: std::time::Duration::from_secs(1),
            },
        }
    } else {
        Auth::ApiKey(&fixture.key)
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let result = Client::new()
                .unwrap()
                .stream(
                    chat(base, &saved["payload"], &replay),
                    auth,
                    &Cancellation::default(),
                    &mut |_| Control::Continue,
                )
                .await
                .unwrap();
            assert_eq!(result.assistant["content"], "Synthetic answer");
            assert_eq!(result.usage.unwrap().cache_read, Some(4));
        });
}
#[tokio::test]
async fn expired_token_refreshes_once_under_lock_and_uses_new_bearer() {
    let refresh = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"synthetic-renewed-access","refresh_token":"synthetic-renewed-refresh","expires_in":3600}),
    )]);
    let upstream = MockOpenAi::start(vec![answer(), answer()]);
    let fixture = Fixture::new();
    let path = fixture.token_path(false);
    let endpoint = refresh.base_url() + "/oauth/token";
    let base = upstream.base_url();
    let client = Client::new().unwrap();
    for _ in 0..2 {
        client
            .stream(
                chat(&base, &payload(), &[]),
                Auth::Subscription {
                    expected_account_id: "synthetic-account",
                    token_path: &path,
                    refresh: client::Refresh {
                        endpoint: &endpoint,
                        client_id: "synthetic-client",
                        timeout: std::time::Duration::from_secs(2),
                    },
                },
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .unwrap();
    }
    assert_eq!(refresh.requests().len(), 1);
    let r = &refresh.requests()[0];
    assert_eq!(r.body["grant_type"], "refresh_token");
    assert_eq!(r.body["refresh_token"], "synthetic-refresh");
    assert_eq!(r.body["client_id"], "synthetic-client");
    for r in upstream.requests() {
        assert_eq!(r.header("authorization"), Some("Bearer synthetic-renewed-access"));
        assert_eq!(r.header("chatgpt-account-id"), Some("synthetic-account"));
    }
    let record = chatgpt_tokens::read(&path).unwrap();
    assert_eq!(record.refresh_token(), "synthetic-renewed-refresh");
    assert!(!format!("{record:?}").contains("synthetic-renewed"));
    refresh.assert_healthy();
    upstream.assert_healthy();
}

#[tokio::test]
async fn refresh_timeout_releases_lock_without_forwarding_or_replacing_tokens() {
    let (reply, release) = Reply::json(json!({"access_token":"synthetic-new","expires_in":3600})).gated();
    let refresh = MockOpenAi::start(vec![
        reply,
        Reply::json(json!({"access_token":"synthetic-new","expires_in":3600})),
    ]);
    let fixture = Fixture::new();
    let path = fixture.token_path(false);
    let before = fs::read(&path).unwrap();
    let base = refresh.base_url();
    let cancel = Cancellation::default();
    let client = Client::with_read_timeout(None).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.stream(
            chat(&base, &payload(), &[]),
            Auth::Subscription {
                expected_account_id: "synthetic-account",
                token_path: &path,
                refresh: client::Refresh {
                    endpoint: &base,
                    client_id: "synthetic-client",
                    timeout: std::time::Duration::from_millis(50),
                },
            },
            &cancel,
            &mut |_| Control::Continue,
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(client::Error::SignedOut)));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(refresh.requests().len(), 1);
    // A second real request must acquire the same lock and refresh successfully.
    let upstream = MockOpenAi::start(vec![answer()]);
    let upstream_base = upstream.base_url();
    client
        .stream(
            chat(&upstream_base, &payload(), &[]),
            Auth::Subscription {
                expected_account_id: "synthetic-account",
                token_path: &path,
                refresh: client::Refresh {
                    endpoint: &base,
                    client_id: "synthetic-client",
                    timeout: std::time::Duration::from_secs(1),
                },
            },
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .unwrap();
    assert_eq!(refresh.requests().len(), 2);
    let _ = release.send(());
}
#[tokio::test]
async fn refusals_use_typed_classifiers_and_never_expose_credentials() {
    for subscription in [false, true] {
        let reply = if subscription {
            Reply::raw(
                429,
                "application/json",
                json!({"error":{"type":"usage_limit_reached","plan_type":KEY,"message":KEY,"resets_at":123}})
                    .to_string()
                    .into_bytes(),
                vec![("Retry-After", "2".into())],
            )
        } else {
            Reply::insufficient_quota(2)
        };
        let mock = MockOpenAi::start(vec![reply]);
        let fixture = Fixture::new();
        let path = fixture.token_path(true);
        let base = mock.base_url();
        let auth = if subscription {
            Auth::Subscription {
                expected_account_id: "synthetic-account",
                token_path: &path,
                refresh: client::Refresh {
                    endpoint: &base,
                    client_id: "synthetic-client",
                    timeout: std::time::Duration::from_secs(1),
                },
            }
        } else {
            Auth::ApiKey(&fixture.key)
        };
        let error = Client::new()
            .unwrap()
            .stream(
                chat(&base, &payload(), &[]),
                auth,
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:?} {error}").contains(KEY));
        if subscription {
            assert!(matches!(
                error,
                client::Error::Http {
                    status: 429,
                    refusal: Some(client::Refusal::Subscription(
                        rate_limits::chatgpt::Refusal::UsageLimitReached { .. }
                    ))
                }
            ))
        } else {
            assert!(matches!(
                error,
                client::Error::Http {
                    status: 429,
                    refusal: Some(client::Refusal::Api(rate_limits::Refusal::OutOfCredit))
                }
            ))
        }
    }
}
#[tokio::test]
async fn cancellation_before_send_and_during_quiet_stream_is_sticky() {
    let (reply, release) = answer().gated();
    let mock = MockOpenAi::start(vec![reply]);
    let fixture = Fixture::new();
    let base = mock.base_url();
    let cancel = Cancellation::default();
    cancel.cancel();
    let client = Client::new().unwrap();
    assert!(matches!(
        client
            .stream(
                chat(&base, &payload(), &[]),
                Auth::ApiKey(&fixture.key),
                &cancel,
                &mut |_| Control::Continue
            )
            .await,
        Err(client::Error::Cancelled)
    ));
    assert!(mock.requests().is_empty());
    let cancel = Cancellation::default();
    let other = cancel.clone();
    let funded = payload();
    let mut on = |_| Control::Continue;
    let inference = client.stream(
        chat(&base, &funded, &[]),
        Auth::ApiKey(&fixture.key),
        &cancel,
        &mut on,
    );
    let stop = async {
        while mock.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await
        }
        other.cancel();
    };
    let (result, ()) = tokio::join!(inference, stop);
    assert!(matches!(result, Err(client::Error::Cancelled)));
    assert_eq!(mock.requests().len(), 1);
    let _ = release.send(());
}
#[tokio::test]
async fn body_bounds_content_type_eof_and_redirect_refusal() {
    let fixture = Fixture::new();
    for reply in [
        Reply::raw(400, "application/json", vec![b'x'; 65537], vec![]),
        Reply::raw(200, "text/plain", b"not SSE".to_vec(), vec![]),
        Reply::events(vec![
            json!({"type":"response.created","response":{"id":"synthetic"}}),
        ]),
    ] {
        let mock = MockOpenAi::start(vec![reply]);
        let base = mock.base_url();
        let error = Client::new()
            .unwrap()
            .stream(
                chat(&base, &payload(), &[]),
                Auth::ApiKey(&fixture.key),
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(error, client::Error::TooLarge | client::Error::Protocol));
    }
    let target = MockOpenAi::start(vec![answer()]);
    let redirect = MockOpenAi::start(vec![Reply::raw(
        307,
        "application/json",
        b"{}".to_vec(),
        vec![("Location", target.base_url())],
    )]);
    let base = redirect.base_url();
    let error = Client::new()
        .unwrap()
        .stream(
            chat(&base, &payload(), &[]),
            Auth::ApiKey(&fixture.key),
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, client::Error::Http { status: 307, .. }));
    assert!(target.requests().is_empty());
}
#[tokio::test]
async fn final_private_usage_survives_cancelled_finish_delivery() {
    let mock = MockOpenAi::start(vec![answer()]);
    let fixture = Fixture::new();
    let base = mock.base_url();
    let mut usage = None;
    let result=Client::new().unwrap().stream_with_usage(chat(&base,&payload(),&[]),Auth::ApiKey(&fixture.key),&Cancellation::default(),&mut |event|if matches!(event,Event::Chunk(ref c) if c.pointer("/choices/0/finish_reason").is_some_and(|v|!v.is_null())){Control::Stop}else{Control::Continue},&mut usage).await;
    assert!(matches!(result, Err(client::Error::Cancelled)));
    let usage = usage.unwrap();
    assert_eq!(usage.prompt, Some(20));
    assert_eq!(usage.cache_read, Some(4));
    assert_eq!(usage.output, Some(7));
}

#[tokio::test]
async fn refreshed_and_retired_credentials_stream_masked_but_refuse_replay_and_tools() {
    let refresh = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"synthetic-renewed-access","refresh_token":"synthetic-renewed-refresh","expires_in":3600}),
    )]);
    let fixture = Fixture::new();
    let path = fixture.token_path(false);
    let endpoint = refresh.base_url();
    let text = format!(
        "{KEY} synthetic-refresh synthetic-renewed-access synthetic-renewed-refresh synthetic-account"
    );
    let events = vec![
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_synthetic","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":&text[..10]}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":&text[10..]}),
        json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
    ];
    let upstream = MockOpenAi::start(vec![Reply::events(events)]);
    let base = upstream.base_url();
    let mut chunks = vec![];
    let result = Client::new()
        .unwrap()
        .stream(
            chat(&base, &payload(), &[]),
            Auth::Subscription {
                expected_account_id: "synthetic-account",
                token_path: &path,
                refresh: client::Refresh {
                    endpoint: &endpoint,
                    client_id: "synthetic-client",
                    timeout: std::time::Duration::from_secs(1),
                },
            },
            &Cancellation::default(),
            &mut |event| {
                if let Event::Chunk(c) = event {
                    chunks.push(c)
                }
                Control::Continue
            },
        )
        .await;
    assert!(matches!(result, Err(client::Error::Protocol)));
    assert!(!chunks.is_empty());
    assert!(!probe_known_value(
        &json!(chunks),
        &[
            KEY,
            "synthetic-refresh",
            "synthetic-renewed-access",
            "synthetic-renewed-refresh",
            "synthetic-account"
        ]
    ));
    assert!(chunks
        .iter()
        .all(|c| c.pointer("/choices/0/delta/tool_calls").is_none()));
    assert_eq!(refresh.requests().len(), 1);
}

#[tokio::test]
async fn malformed_refresh_never_replaces_file_and_errors_are_fixed() {
    for reply in [
        json!({"access_token":KEY,"expires_in":0,"error":"synthetic-refresh"}),
        json!({"access_token":"","refresh_token":"synthetic-refresh","expires_in":3600}),
        json!({"access_token":KEY,"expires_in":"synthetic-refresh"}),
        json!({"access_token":"e30.eyJleHAiOjEwMDB9.synthetic","refresh_token":"synthetic-refresh","expires_in":3600}),
    ] {
        let refresh = MockOpenAi::start(vec![Reply::json(reply)]);
        let base = refresh.base_url();
        let fixture = Fixture::new();
        let path = fixture.token_path(false);
        let before = fs::read(&path).unwrap();
        let error = Client::new()
            .unwrap()
            .stream(
                chat(&base, &payload(), &[]),
                Auth::Subscription {
                    expected_account_id: "synthetic-account",
                    token_path: &path,
                    refresh: client::Refresh {
                        endpoint: &base,
                        client_id: "synthetic-client",
                        timeout: std::time::Duration::from_secs(1),
                    },
                },
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error, client::Error::SignedOut);
        let status = oxc_gateway::status::ProviderStatus {
            remaining: None,
            source: oxc_gateway::status::Source::NotReported,
            state: error.subscription_state().unwrap(),
            checked_at: UNIX_EPOCH,
        };
        assert!(status.render_card().contains("State: signed out"));
        assert_eq!(fs::read(&path).unwrap(), before);
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(KEY) && !diagnostic.contains("synthetic-refresh"));
        assert_eq!(refresh.requests().len(), 1);
    }
}

#[tokio::test]
async fn missing_replay_refuses_before_any_token_file_access() {
    let payload = json!({"messages":[{"role":"user","content":"synthetic"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_synthetic","type":"function","function":{"name":"read","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_synthetic","content":"error: synthetic missing file","is_error":true}]});
    let mock = MockOpenAi::start(vec![]);
    let base = mock.base_url();
    let fixture = Fixture::new();
    let absent = fixture.root.join("not-created.json");
    let error = Client::new()
        .unwrap()
        .stream(
            chat(&base, &payload, &[]),
            Auth::Subscription {
                expected_account_id: "synthetic-account",
                token_path: &absent,
                refresh: client::Refresh {
                    endpoint: &base,
                    client_id: "synthetic-client",
                    timeout: std::time::Duration::from_secs(1),
                },
            },
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .unwrap_err();
    assert_eq!(error, client::Error::Request("messages"));
    assert!(mock.requests().is_empty());
    assert!(!absent.exists());
}
#[test]
fn completed_and_auth_debug_hide_private_values() {
    let fixture = Fixture::new();
    let auth = Auth::ApiKey(&fixture.key);
    assert!(!format!("{auth:?}").contains(KEY));
    let completed = client::Completed {
        assistant: json!({"content":KEY}),
        output_items: vec![json!({"encrypted_content":"synthetic-refresh"})].into(),
        replay_items: vec![json!({"encrypted_content":"synthetic-refresh"})].into(),
        usage: None,
        finish_reason: "stop",
    };
    let debug = format!("{completed:?}");
    assert!(!debug.contains(KEY) && !debug.contains("synthetic-refresh"));
}

#[tokio::test]
async fn dropping_refresh_future_releases_owned_http_work_and_lock() {
    let (reply, release) = Reply::json(json!({"access_token":"synthetic-first","expires_in":3600})).gated();
    let refresh = MockOpenAi::start(vec![
        reply,
        Reply::json(json!({"access_token":"synthetic-second","expires_in":3600})),
    ]);
    let fixture = Fixture::new();
    let path = fixture.token_path(false);
    let before = fs::read(&path).unwrap();
    let endpoint = refresh.base_url();
    let client = Client::new().unwrap();
    let payload = payload();
    let cancel = Cancellation::default();
    let mut on = |_| Control::Continue;
    let mut first = Box::pin(client.stream(
        chat(&endpoint, &payload, &[]),
        Auth::Subscription {
            expected_account_id: "synthetic-account",
            token_path: &path,
            refresh: client::Refresh {
                endpoint: &endpoint,
                client_id: "synthetic-client",
                timeout: std::time::Duration::from_secs(30),
            },
        },
        &cancel,
        &mut on,
    ));
    tokio::select! { result=&mut first=>panic!("refresh gate unexpectedly completed: {}",result.is_ok()), ()=async{while refresh.requests().is_empty(){tokio::time::sleep(std::time::Duration::from_millis(5)).await}}=>{} }
    drop(first);
    assert_eq!(fs::read(&path).unwrap(), before);
    let upstream = MockOpenAi::start(vec![answer()]);
    let base = upstream.base_url();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.stream(
            chat(&base, &payload, &[]),
            Auth::Subscription {
                expected_account_id: "synthetic-account",
                token_path: &path,
                refresh: client::Refresh {
                    endpoint: &endpoint,
                    client_id: "synthetic-client",
                    timeout: std::time::Duration::from_secs(1),
                },
            },
            &Cancellation::default(),
            &mut |_| Control::Continue,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(refresh.requests().len(), 2);
    assert_eq!(
        upstream.requests()[0].header("authorization"),
        Some("Bearer synthetic-second")
    );
    let _ = release.send(());
}

#[test]
fn lock_waiters_exit_new_process_on_cancel_and_deadline_before_unlock() {
    use nix::fcntl::{Flock, FlockArg};
    for deadline_only in [false, true] {
        let fixture = Fixture::new();
        let tokens = fixture.token_path(false);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.root.join("chatgpt-auth.lock"))
            .unwrap();
        let held = Flock::lock(lock, FlockArg::LockExclusive).unwrap();
        let mock = MockOpenAi::start(vec![]);
        let record = fixture.root.join("lock-wait.json");
        fs::write(
            &record,
            json!({"tokens":tokens,"base":mock.base_url(),"deadline_only":deadline_only}).to_string(),
        )
        .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cancel_lock_wait_child"])
            .env_clear()
            .env("HOME", &fixture.root)
            .env("PATH", "/usr/bin:/bin")
            .env("OPENAI_LOCK_FIXTURE", &record)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(held);
        assert!(child.wait().unwrap().success());
        assert!(mock.requests().is_empty());
        assert!(
            exited,
            "cancelled or expired client runtime waited for an external token-file lock at shutdown"
        );
    }
}
#[test]
fn cancel_lock_wait_child() {
    let Some(path) = std::env::var_os("OPENAI_LOCK_FIXTURE") else {
        return;
    };
    let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let tokens = PathBuf::from(saved["tokens"].as_str().unwrap());
    let base = saved["base"].as_str().unwrap();
    let deadline_only = saved["deadline_only"] == true;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let cancel = Cancellation::default();
            let other = cancel.clone();
            if !deadline_only {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    other.cancel();
                });
            }
            let timeout = if deadline_only {
                std::time::Duration::from_millis(50)
            } else {
                std::time::Duration::from_secs(30)
            };
            let result = Client::new()
                .unwrap()
                .stream(
                    chat(base, &payload(), &[]),
                    Auth::Subscription {
                        expected_account_id: "synthetic-account",
                        token_path: &tokens,
                        refresh: client::Refresh {
                            endpoint: base,
                            client_id: "synthetic-client",
                            timeout,
                        },
                    },
                    &cancel,
                    &mut |_| Control::Continue,
                )
                .await;
            if deadline_only {
                assert!(matches!(result, Err(client::Error::Credentials)))
            } else {
                assert!(matches!(result, Err(client::Error::Cancelled)))
            }
        });
}

#[tokio::test]
async fn incomplete_finish_keeps_length_and_invalid_usage_stays_unknown() {
    for usage in [
        Value::Null,
        json!({"input_tokens":10,"output_tokens":"invalid"}),
        json!({"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}),
    ] {
        let known = usage["output_tokens"].as_u64().is_some();
        let mock = MockOpenAi::start(vec![Reply::events(vec![
            json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":usage}}),
        ])]);
        let fixture = Fixture::new();
        let base = mock.base_url();
        let mut chunks = vec![];
        let complete = Client::new()
            .unwrap()
            .stream(
                chat(&base, &payload(), &[]),
                Auth::ApiKey(&fixture.key),
                &Cancellation::default(),
                &mut |event| {
                    if let Event::Chunk(c) = event {
                        chunks.push(c)
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        assert_eq!(complete.finish_reason, "length");
        assert!(chunks
            .iter()
            .any(|v| v.pointer("/choices/0/finish_reason") == Some(&json!("length"))));
        assert_eq!(complete.usage.is_some(), known);
        if let Some(usage) = complete.usage {
            assert_eq!(usage.output, Some(4));
            assert_eq!(usage.prompt, Some(10));
            assert_eq!(usage.cache_read, Some(3));
            assert_eq!(usage.cache_write, Some(0));
        }
    }
}

#[tokio::test]
async fn incomplete_unexecuted_call_does_not_poison_a_new_user_turn() {
    let mut failures = Vec::new();
    for subscription in [false, true] {
        for mixed in [false, true] {
            let mut items = vec![
                json!({"type":"reasoning","id":"rs_partial","encrypted_content":"opaque-synthetic-partial","summary":[]}),
            ];
            if mixed {
                items.push(json!({"type":"function_call","id":"fc_valid","call_id":"call_valid","name":"read","arguments":"{}","status":"completed"}));
            }
            items.push(json!({"type":"function_call","id":"fc_partial","call_id":"call_partial","name":"read","arguments":"{","status":"incomplete"}));
            items.push(json!({"type":"message","id":"msg_partial","role":"assistant","content":[{"type":"output_text","text":"Visible partial answer"}],"status":"incomplete"}));
            let mut events = Vec::new();
            if mixed {
                events.push(json!({"type":"response.output_item.done","output_index":1,"item":items[1]}));
            }
            events.push(json!({"type":"response.incomplete","response":{"status":"incomplete","output":items.clone(),"usage":{"input_tokens":10,"output_tokens":4}}}));
            let mock = MockOpenAi::start(vec![Reply::events(events), answer()]);
            let fixture = Fixture::new();
            let base = mock.base_url();
            let client = Client::new().unwrap();
            let path = fixture.token_path(true);
            let auth = || {
                if subscription {
                    Auth::Subscription {
                        expected_account_id: "synthetic-account",
                        token_path: &path,
                        refresh: client::Refresh {
                            endpoint: &base,
                            client_id: "synthetic-client",
                            timeout: std::time::Duration::from_secs(1),
                        },
                    }
                } else {
                    Auth::ApiKey(&fixture.key)
                }
            };
            let mut chunks = Vec::new();
            let complete = client
                .stream(
                    chat(&base, &payload(), &[]),
                    auth(),
                    &Cancellation::default(),
                    &mut |event| {
                        if let Event::Chunk(chunk) = event {
                            chunks.push(chunk);
                        }
                        Control::Continue
                    },
                )
                .await
                .unwrap();
            assert_eq!(complete.finish_reason, "length");
            assert_eq!(complete.usage.as_ref().unwrap().output, Some(4));
            assert_eq!(complete.output_items.as_ref(), items.as_slice());
            assert!(complete.replay_items.is_empty());
            assert!(complete.assistant.get("tool_calls").is_none());
            assert_eq!(complete.assistant["content"], "Visible partial answer");
            assert!(chunks
                .iter()
                .all(|chunk| chunk.pointer("/choices/0/delta/tool_calls").is_none()));
            let mut assembler = oxa_engine::ReplyAssembler::new();
            for chunk in &chunks {
                assembler.chunk_value(chunk).unwrap();
            }
            assembler.chunk_value(&json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}})).unwrap();
            let delivered = assembler.finish(oxa_proto::ThinkingLevel::Off).unwrap();
            assert!(delivered.msg.tool_calls.is_empty());
            assert_eq!(delivered.msg.content.as_deref(), Some("Visible partial answer"));
            assert!(!format!("{complete:?}").contains("opaque-synthetic-partial"));
            let mut next = payload();
            let messages = next["messages"].as_array_mut().unwrap();
            messages.push(complete.assistant);
            messages.push(json!({"role":"user","content":"Start a new turn after the interrupted response"}));
            let replay = [if complete.replay_items.is_empty() {
                None
            } else {
                Some(request::ReplayItems::new(complete.replay_items.to_vec()))
            }];
            match client
                .stream(
                    chat(&base, &next, &replay),
                    auth(),
                    &Cancellation::default(),
                    &mut |_| Control::Continue,
                )
                .await
            {
                Ok(continued) => assert_eq!(continued.assistant["content"], "Synthetic answer"),
                Err(error) => failures.push((subscription, mixed, error)),
            }
            let requests = mock.wait_for_requests(2);
            let input = requests[1].body["input"].as_array().unwrap();
            assert!(input
                .iter()
                .any(|item| item["role"] == "assistant" && item["content"] == "Visible partial answer"));
            assert!(input.iter().all(|item| !matches!(
                item["type"].as_str(),
                Some("function_call" | "function_call_output" | "reasoning")
            )));
        }
    }
    assert!(
        failures.is_empty(),
        "interrupted continuation failures: {failures:?}"
    );
}

#[tokio::test]
async fn subscription_account_switch_refuses_before_refresh_or_paid_sending() {
    for fresh in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.token_path(fresh);
        // Represents an account switch after the caller scoped its admission.
        let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record["account_id"] = json!("synthetic-other-account");
        fs::write(&path, record.to_string()).unwrap();
        let before = fs::read(&path).unwrap();
        let mock = MockOpenAi::start(vec![]);
        let base = mock.base_url();
        let mut sending = 0;
        let auth = Auth::Subscription {
            token_path: &path,
            expected_account_id: "synthetic-account",
            refresh: client::Refresh {
                endpoint: &base,
                client_id: "synthetic-client",
                timeout: std::time::Duration::from_secs(1),
            },
        };
        assert!(!format!("{auth:?}").contains("synthetic-account"));
        let error = Client::new()
            .unwrap()
            .stream(
                chat(&base, &payload(), &[]),
                auth,
                &Cancellation::default(),
                &mut |event| {
                    if matches!(event, Event::Sending) {
                        sending += 1;
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, client::Error::Credentials);
        assert_eq!(sending, 0);
        assert!(mock.requests().is_empty());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!format!("{error:?} {error}").contains("synthetic-other-account"));
    }
}

// Exercise the production consumer: IDs replace atomically and repeated names
// are idempotent, unlike a test-only concatenating approximation.
#[tokio::test]
async fn atomic_tool_identity_survives_credential_prefix_redaction_in_real_assembler() {
    for subscription in [false, true] {
        let fixture = Fixture::new();
        let items = vec![
            json!({"id":"fc_prefix","type":"function_call","call_id":"call_s","name":"ss","arguments":"{}","status":"completed"}),
            json!({"id":"fc_other","type":"function_call","call_id":"other_sy","name":"read_s","arguments":"{}","status":"completed"}),
        ];
        let response = Reply::events(vec![json!({"type":"response.completed","response":{
            "id":"resp_synthetic","status":"completed","output":items,
            "usage":{"input_tokens":12,"output_tokens":9}
        }})]);
        let mock = MockOpenAi::start(vec![response, Reply::multi_tool_no_deltas()]);
        let base = mock.base_url();
        let path = fixture.token_path(true);
        let auth = || {
            if subscription {
                Auth::Subscription {
                    token_path: &path,
                    expected_account_id: "synthetic-account",
                    refresh: client::Refresh {
                        endpoint: &base,
                        client_id: "synthetic-client",
                        timeout: std::time::Duration::from_secs(1),
                    },
                }
            } else {
                Auth::ApiKey(&fixture.key)
            }
        };
        let mut assembler = oxa_engine::ReplyAssembler::new();
        let mut chunks = vec![];
        let mut body = payload();
        body["stream_options"]["include_usage"] = json!(true);
        let completed = Client::new()
            .unwrap()
            .stream(
                chat(&base, &body, &[]),
                auth(),
                &Cancellation::default(),
                &mut |e| {
                    if let Event::Chunk(c) = e {
                        assembler.chunk_value(&c).unwrap();
                        chunks.push(c);
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        let turn = assembler.finish(oxa_proto::ThinkingLevel::Off).unwrap();
        assert_eq!(turn.msg.tool_calls.len(), 2);
        for (i, call) in turn.msg.tool_calls.iter().enumerate() {
            assert_eq!(
                call.id,
                items[i]["call_id"].as_str().unwrap(),
                "mode subscription={subscription}"
            );
            assert_eq!(call.name, items[i]["name"].as_str().unwrap());
            assert_eq!(
                call.id,
                completed.assistant["tool_calls"][i]["id"].as_str().unwrap()
            );
            assert_eq!(
                call.name,
                completed.assistant["tool_calls"][i]["function"]["name"]
                    .as_str()
                    .unwrap()
            );
            assert_eq!(call.id, completed.replay_items[i]["call_id"].as_str().unwrap());
            assert_eq!(call.name, completed.replay_items[i]["name"].as_str().unwrap());
            let ids: Vec<_> = chunks
                .iter()
                .flat_map(|c| {
                    c["choices"][0]["delta"]["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                })
                .filter(|c| c["index"] == json!(i))
                .filter_map(|c| c["id"].as_str())
                .filter(|id| !id.is_empty())
                .collect();
            assert_eq!(ids, vec![call.id.as_str()]);
        }
        assert_eq!(completed.output_items.as_ref(), items.as_slice());
        let mut next = payload();
        let messages = next["messages"].as_array_mut().unwrap();
        messages.push(completed.assistant.clone());
        for call in &turn.msg.tool_calls {
            messages.push(
                json!({"role":"tool","tool_call_id":call.id,"content":"synthetic error","is_error":true}),
            );
        }
        messages.push(json!({"role":"user","content":"Continue"}));
        let replay = [Some(request::ReplayItems::new(completed.replay_items.to_vec()))];
        Client::new()
            .unwrap()
            .stream(
                chat(&base, &next, &replay),
                auth(),
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .unwrap();
        let requests = mock.wait_for_requests(2);
        let input = requests[1].body["input"].as_array().unwrap();
        for call in &turn.msg.tool_calls {
            assert!(input
                .iter()
                .any(|v| v["type"] == "function_call" && v["call_id"] == call.id));
            assert!(input
                .iter()
                .any(|v| v["type"] == "function_call_output" && v["call_id"] == call.id));
        }
        mock.assert_healthy();
    }
}

#[tokio::test]
async fn credential_in_tool_identity_refuses_before_execution_without_rewriting_replay() {
    for subscription in [false, true] {
        for field in ["call_id", "name"] {
            let fixture = Fixture::new();
            let mut item = json!({"type":"function_call","id":"fc_synthetic","call_id":"call_s","name":"ss","arguments":"{}","status":"completed"});
            item[field] = json!(format!("prefix_{KEY}_suffix"));
            let mock = MockOpenAi::start(vec![Reply::events(vec![
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":12,"output_tokens":9}}}),
            ])]);
            let base = mock.base_url();
            let path = fixture.token_path(true);
            let auth = if subscription {
                Auth::Subscription {
                    token_path: &path,
                    expected_account_id: "synthetic-account",
                    refresh: client::Refresh {
                        endpoint: &base,
                        client_id: "synthetic-client",
                        timeout: std::time::Duration::from_secs(1),
                    },
                }
            } else {
                Auth::ApiKey(&fixture.key)
            };
            let mut usage = None;
            let mut assembler = oxa_engine::ReplyAssembler::new();
            let mut chunks = vec![];
            let error = Client::new()
                .unwrap()
                .stream_with_usage(
                    chat(&base, &payload(), &[]),
                    auth,
                    &Cancellation::default(),
                    &mut |e| {
                        if let Event::Chunk(c) = e {
                            assembler.chunk_value(&c).unwrap();
                            chunks.push(c);
                        }
                        Control::Continue
                    },
                    &mut usage,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, client::Error::Protocol));
            assert!(!format!("{error:?}").contains(KEY));
            assert_eq!(usage.unwrap().output, Some(9));
            assert!(chunks
                .iter()
                .all(|c| c.pointer("/choices/0/delta/tool_calls").is_none()));
            mock.assert_healthy();
        }
    }
}

// A test-local peer with independently controlled headers and SSE reads. No
// changes to the shared fixture, production server, or HTTP client's policy.
struct HeldResponsesPeer {
    base: String,
    connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    send: Option<std::sync::mpsc::Sender<Value>>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl HeldResponsesPeer {
    fn start() -> Self {
        use std::io::{BufRead, Read};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (send, receive) = std::sync::mpsc::channel::<Value>();
        let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peer_connected = connected.clone();
        let join = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    _ => return,
                }
            };
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            socket
                .set_write_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut request = std::io::BufReader::new(&mut socket);
            let mut length = 0;
            let mut header_bytes = 0;
            loop {
                let mut line = String::new();
                request.read_line(&mut line).unwrap();
                header_bytes += line.len();
                assert!(header_bytes <= 32 * 1024 && !line.is_empty());
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            assert!(length <= 64 * 1024);
            request.read_exact(&mut vec![0; length]).unwrap();
            drop(request);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .unwrap();
            socket.flush().unwrap();
            peer_connected.store(true, Ordering::Release);
            while let Ok(event) = receive.recv_timeout(std::time::Duration::from_secs(3)) {
                // A peer closed by cancellation/timeout is an expected control.
                if write!(socket, "data: {event}\n\n")
                    .and_then(|_| socket.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            base,
            connected,
            send: Some(send),
            join: Some(join),
        }
    }
    fn send(&self, event: Value) {
        self.send.as_ref().unwrap().send(event).unwrap();
    }
}
impl Drop for HeldResponsesPeer {
    fn drop(&mut self) {
        self.send.take();
        self.join.take().unwrap().join().unwrap();
    }
}
fn held_text() -> Value {
    json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Visible output."})
}
fn held_complete() -> Value {
    json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":12,"output_tokens":9}}})
}

#[tokio::test]
async fn selected_read_timeout_expires_on_held_headers_and_held_sse_in_both_modes() {
    for subscription in [false, true] {
        for after_headers in [false, true] {
            let fixture = Fixture::new();
            let (reply, _release) = answer().gated();
            let headers = (!after_headers).then(|| MockOpenAi::start(vec![reply]));
            let body = after_headers.then(HeldResponsesPeer::start);
            let base = if let Some(body) = &body {
                body.send(held_text());
                body.base.clone()
            } else {
                headers.as_ref().unwrap().base_url()
            };
            let path = fixture.token_path(true);
            let auth = if subscription {
                Auth::Subscription {
                    token_path: &path,
                    expected_account_id: "synthetic-account",
                    refresh: client::Refresh {
                        endpoint: &base,
                        client_id: "synthetic-client",
                        timeout: std::time::Duration::from_secs(1),
                    },
                }
            } else {
                Auth::ApiKey(&fixture.key)
            };
            let client = Client::with_read_timeout(Some(std::time::Duration::from_millis(150))).unwrap();
            let mut saw_text = false;
            let mut usage = None;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.stream_with_usage(
                    chat(&base, &payload(), &[]),
                    auth,
                    &Cancellation::default(),
                    &mut |e| {
                        if let Event::Chunk(c) = e {
                            saw_text |= c["choices"][0]["delta"]["content"] == "Visible output.";
                        }
                        Control::Continue
                    },
                    &mut usage,
                ),
            )
            .await;
            assert!(
                result.is_ok(),
                "selected silence timeout did not expire while peer was held"
            );
            assert!(matches!(result.unwrap(), Err(client::Error::Transport)));
            assert_eq!(saw_text, after_headers);
            assert!(usage.is_none());
        }
    }
}

#[tokio::test]
async fn read_timeout_resets_on_progress_and_none_keeps_quiet_cancellation() {
    for subscription in [false, true] {
        for selected in [Some(std::time::Duration::from_millis(500)), None] {
            let fixture = Fixture::new();
            let peer = HeldResponsesPeer::start();
            peer.send(held_text());
            let path = fixture.token_path(true);
            let base = &peer.base;
            let auth = || {
                if subscription {
                    Auth::Subscription {
                        token_path: &path,
                        expected_account_id: "synthetic-account",
                        refresh: client::Refresh {
                            endpoint: base,
                            client_id: "synthetic-client",
                            timeout: std::time::Duration::from_secs(1),
                        },
                    }
                } else {
                    Auth::ApiKey(&fixture.key)
                }
            };
            let client = Client::with_read_timeout(selected).unwrap();
            let started = std::time::Instant::now();
            let body = payload();
            let cancel = Cancellation::default();
            let mut callback = |_| Control::Continue;
            let inference = client.stream(chat(base, &body, &[]), auth(), &cancel, &mut callback);
            let progress = async {
                for _ in 0..8 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if selected.is_some() {
                        peer.send(held_text());
                    }
                }
                peer.send(held_complete());
            };
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(4), async {
                tokio::join!(inference, progress)
            })
            .await
            .unwrap();
            let completed = result.unwrap();
            assert_eq!(completed.usage.unwrap().output, Some(9));
            assert!(
                started.elapsed() > std::time::Duration::from_millis(500),
                "control must outlive a total-generation deadline"
            );
            if selected.is_none() {
                let quiet = HeldResponsesPeer::start();
                quiet.send(held_text());
                let stop = Cancellation::default();
                let mut callback = |_| Control::Continue;
                let inference = client.stream(chat(&quiet.base, &body, &[]), auth(), &stop, &mut callback);
                let cancel_quiet = async {
                    while !quiet.connected.load(Ordering::Acquire) {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    stop.cancel();
                };
                let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    tokio::join!(inference, cancel_quiet)
                })
                .await
                .unwrap();
                assert!(matches!(result, Err(client::Error::Cancelled)));
            }
        }
    }
}

#[test]
fn unrepresentable_read_timeout_is_a_fixed_setup_error() {
    assert!(matches!(
        Client::with_read_timeout(Some(std::time::Duration::MAX)),
        Err(client::Error::Setup)
    ));
}

fn subscription_auth<'a>(path: &'a std::path::Path, refresh: &'a str) -> Auth<'a> {
    Auth::Subscription {
        token_path: path,
        expected_account_id: "synthetic-account",
        refresh: client::Refresh {
            endpoint: refresh,
            client_id: "synthetic-client",
            timeout: std::time::Duration::from_secs(2),
        },
    }
}

#[tokio::test]
async fn codex_shaped_refresh_without_expires_in_is_stored_before_forwarding() {
    let fixture = Fixture::new();
    let path = fixture.token_path(false);
    let refresh = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"e30.eyJleHAiOjQxMDI0NDQ4MDB9.synthetic", "refresh_token":"synthetic-rotated-refresh"}),
    )]);
    let upstream = MockOpenAi::start(vec![answer()]);
    let base = upstream.base_url();
    let endpoint = refresh.base_url();
    let mut forwarded = false;
    let result = Client::new()
        .unwrap()
        .stream(
            chat(&base, &payload(), &[]),
            subscription_auth(&path, &endpoint),
            &Cancellation::default(),
            &mut |e| {
                if matches!(e, Event::Sending) {
                    let record = chatgpt_tokens::read(&path).unwrap();
                    assert_eq!(record.access_token(), "e30.eyJleHAiOjQxMDI0NDQ4MDB9.synthetic");
                    assert_eq!(record.refresh_token(), "synthetic-rotated-refresh");
                    forwarded = true;
                }
                Control::Continue
            },
        )
        .await;
    assert!(result.is_ok(), "Codex-shaped token-only refresh must succeed");
    assert!(forwarded);
    assert_eq!(refresh.requests().len(), 1);
    assert_eq!(upstream.requests().len(), 1);
}

#[tokio::test]
async fn token_expired_401_refreshes_and_retries_once_in_the_same_session() {
    let fixture = Fixture::new();
    let path = fixture.token_path(true);
    let refresh = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"synthetic-recovered-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}),
    )]);
    let upstream = MockOpenAi::start(vec![
        answer(),
        Reply::error(401, "token_expired", "synthetic expiry"),
        answer(),
    ]);
    let base = upstream.base_url();
    let endpoint = refresh.base_url();
    let client = Client::new().unwrap();
    for _ in 0..2 {
        let result = client
            .stream(
                chat(&base, &payload(), &[]),
                subscription_auth(&path, &endpoint),
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await;
        assert!(result.is_ok(), "token expiry in a running session must recover");
    }
    assert_eq!(refresh.requests().len(), 1);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].body_bytes, requests[2].body_bytes);
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer synthetic-openai-credential")
    );
    assert_eq!(
        requests[2].header("authorization"),
        Some("Bearer synthetic-recovered-access")
    );
    assert_eq!(
        chatgpt_tokens::read(&path).unwrap().refresh_token(),
        "synthetic-rotated-refresh"
    );
}

#[tokio::test]
async fn expired_recovery_is_bounded_and_excludes_api_keys_and_other_refusals() {
    for (subscription, code, expected_refreshes, expected_requests) in [
        (true, "token_expired", 1, 2),
        (true, "invalid_api_key", 0, 1),
        (false, "token_expired", 0, 1),
    ] {
        let fixture = Fixture::new();
        let path = fixture.token_path(true);
        let before = fs::read(&path).unwrap();
        let refresh = MockOpenAi::start(vec![Reply::json(
            json!({"access_token":"synthetic-recovered-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}),
        )]);
        let upstream = MockOpenAi::start(vec![
            Reply::error(401, code, "synthetic refusal"),
            Reply::error(401, code, "synthetic repeated refusal"),
        ]);
        let endpoint = refresh.base_url();
        let base = upstream.base_url();
        let auth = if subscription {
            subscription_auth(&path, &endpoint)
        } else {
            Auth::ApiKey(&fixture.key)
        };
        let mut delivered = false;
        let error = Client::new()
            .unwrap()
            .stream(
                chat(&base, &payload(), &[]),
                auth,
                &Cancellation::default(),
                &mut |e| {
                    delivered |= matches!(e, Event::Chunk(_));
                    Control::Continue
                },
            )
            .await
            .unwrap_err();
        if subscription {
            assert_eq!(error, client::Error::SignedOut);
            assert_eq!(
                error.subscription_state(),
                Some(oxc_gateway::status::State::SignedOut)
            );
        } else {
            assert!(matches!(error, client::Error::Http { status: 401, .. }));
        }
        assert!(!delivered);
        assert_eq!(refresh.requests().len(), expected_refreshes);
        assert_eq!(upstream.requests().len(), expected_requests);
        if expected_refreshes == 0 {
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        assert!(!format!("{error:?} {error}").contains(KEY));
    }
}

#[tokio::test]
async fn running_client_rechecks_proactive_expiry_without_a_restart() {
    let fixture = Fixture::new();
    let path = fixture.token_path(true);
    let refresh = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"synthetic-recovered-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}),
    )]);
    let upstream = MockOpenAi::start(vec![answer(), answer()]);
    let base = upstream.base_url();
    let endpoint = refresh.base_url();
    let client = Client::new().unwrap();
    client
        .stream(
            chat(&base, &payload(), &[]),
            subscription_auth(&path, &endpoint),
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .unwrap();
    assert!(refresh.requests().is_empty());
    // Advance the owned synthetic record into the proactive window; same client,
    // process and route. No wall-clock sleeps or real credential files.
    fixture.token_path(false);
    client
        .stream(
            chat(&base, &payload(), &[]),
            subscription_auth(&path, &endpoint),
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .unwrap();
    assert_eq!(refresh.requests().len(), 1);
    assert_eq!(
        upstream.requests()[1].header("authorization"),
        Some("Bearer synthetic-recovered-access")
    );
}

#[test]
fn forced_refresh_child() {
    let Some(path) = std::env::var_os("OPENAI_FORCE_REFRESH_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    let base = std::env::var("OPENAI_FORCE_UPSTREAM").unwrap();
    let endpoint = std::env::var("OPENAI_FORCE_ENDPOINT").unwrap();
    let mut sends = 0;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                Client::new().unwrap().stream(
                    chat(&base, &payload(), &[]),
                    subscription_auth(&path, &endpoint),
                    &Cancellation::default(),
                    &mut |e| {
                        if matches!(e, Event::Sending) {
                            sends += 1;
                            if sends == 2 {
                                assert_eq!(
                                    chatgpt_tokens::read(&path).unwrap().refresh_token(),
                                    "synthetic-rotated-refresh"
                                );
                            }
                        }
                        Control::Continue
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap();
        });
    assert_eq!(sends, 2);
}

#[tokio::test]
async fn concurrent_processes_refresh_a_rejected_generation_once() {
    let fixture = Fixture::new();
    let path = fixture.token_path(true);
    let refresh = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"synthetic-recovered-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}),
    )]);
    let (first, release_first) = Reply::error(401, "token_expired", "synthetic expiry").gated();
    let (second, release_second) = Reply::error(401, "token_expired", "synthetic expiry").gated();
    let upstream = MockOpenAi::start(vec![first, second, answer(), answer()]);
    let children: Vec<_> = (0..2)
        .map(|_| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "forced_refresh_child"])
                .env_clear()
                .env("HOME", &fixture.root)
                .env("PATH", "/usr/bin:/bin")
                .env("OPENAI_FORCE_REFRESH_FIXTURE", &path)
                .env("OPENAI_FORCE_UPSTREAM", upstream.base_url())
                .env("OPENAI_FORCE_ENDPOINT", refresh.base_url())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    assert_eq!(upstream.wait_for_requests(2).len(), 2);
    release_first.send(()).unwrap();
    release_second.send(()).unwrap();
    for child in children {
        assert!(
            child.wait_with_output().unwrap().status.success(),
            "synthetic refresh child failed"
        );
    }
    assert_eq!(refresh.requests().len(), 1);
    let requests = upstream.wait_for_requests(4);
    assert_eq!(requests.len(), 4);
    assert!(requests[..2]
        .iter()
        .all(|r| r.header("authorization") == Some("Bearer synthetic-openai-credential")));
    assert!(requests[2..]
        .iter()
        .all(|r| r.header("authorization") == Some("Bearer synthetic-recovered-access")));
    assert_eq!(
        chatgpt_tokens::read(&path).unwrap().refresh_token(),
        "synthetic-rotated-refresh"
    );
    refresh.assert_healthy();
    upstream.assert_healthy();
}

#[tokio::test]
async fn rejected_nonterminal_event_fails_before_a_held_peer_is_released() {
    for subscription in [false, true] {
        for read_timeout in [Some(std::time::Duration::from_secs(5)), None] {
            let fixture = Fixture::new();
            let path = fixture.token_path(true);
            let peer = HeldResponsesPeer::start();
            peer.send(held_text());
            peer.send(json!({"type":"response.output_item.done","output_index":1,"item":{
                "type":"function_call","id":"fc_pending","call_id":"call_pending","name":"read","arguments":"{}","status":"completed"
            }}));
            // No item99 exists: the real translator rejects this nonterminal
            // argument delta. The peer then deliberately holds its socket open.
            peer.send(
                json!({"type":"response.function_call_arguments.delta","output_index":99,"delta":"{}"}),
            );
            let auth = if subscription {
                subscription_auth(&path, &peer.base)
            } else {
                Auth::ApiKey(&fixture.key)
            };
            let client = Client::with_read_timeout(read_timeout).unwrap();
            let body = payload();
            let cancel = Cancellation::default();
            let mut chunks = Vec::new();
            let mut usage = None;
            let mut callback = |event| {
                if let Event::Chunk(chunk) = event {
                    chunks.push(chunk);
                }
                Control::Continue
            };
            let mut inference = Box::pin(client.stream_with_usage(
                chat(&peer.base, &body, &[]),
                auth,
                &cancel,
                &mut callback,
                &mut usage,
            ));
            let prompt =
                tokio::time::timeout(std::time::Duration::from_millis(500), inference.as_mut()).await;
            let before_release = prompt.is_ok();
            let result = match prompt {
                Ok(result) => result,
                Err(_) => {
                    // Negative control is bounded: release the peer and await
                    // the old path's terminal check, rather than hanging forever.
                    peer.send(held_complete());
                    tokio::time::timeout(std::time::Duration::from_secs(2), inference.as_mut())
                        .await
                        .unwrap()
                }
            };
            drop(inference);
            assert!(before_release,"translator rejection waited for the held peer; subscription={subscription}, reads_disabled={}",read_timeout.is_none());
            assert_eq!(result.unwrap_err(), client::Error::Protocol);
            assert!(usage.is_none());
            // Prior benign text was live, but no executable calls or error
            // payloads escape the prompt translator refusal.
            assert!(chunks
                .iter()
                .any(|c| c["choices"][0]["delta"]["content"] == "Visible output."));
            assert!(chunks
                .iter()
                .all(|c| c.get("error").is_none() && c.pointer("/choices/0/delta/tool_calls").is_none()));
        }
    }
}

#[tokio::test]
async fn responses_framing_keeps_json_only_and_unfinished_eof_policy_in_both_modes() {
    let invalid_frames: &[&[u8]] = &[
        b"data: [DONE]\r\n\r\n",
        b"data\n\n",
        b"data: {not-json}\n\n",
        b"data: {\"type\":\"response.created\",\"text\":\"\xff\"}\n\n",
        // Valid JSON without the blank event boundary must not be flushed at EOF.
        b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n",
    ];
    for subscription in [false, true] {
        for bytes in invalid_frames {
            let fixture = Fixture::new();
            let path = fixture.token_path(true);
            let mock = MockOpenAi::start(vec![Reply::raw(200, "text/event-stream", bytes.to_vec(), vec![])]);
            let base = mock.base_url();
            let auth = if subscription {
                subscription_auth(&path, &base)
            } else {
                Auth::ApiKey(&fixture.key)
            };
            let mut chunks = Vec::new();
            let mut usage = None;
            let error = Client::new()
                .unwrap()
                .stream_with_usage(
                    chat(&base, &payload(), &[]),
                    auth,
                    &Cancellation::default(),
                    &mut |event| {
                        if let Event::Chunk(chunk) = event {
                            chunks.push(chunk);
                        }
                        Control::Continue
                    },
                    &mut usage,
                )
                .await
                .unwrap_err();
            assert_eq!(error, client::Error::Protocol);
            assert!(usage.is_none());
            assert!(chunks.is_empty());
            mock.assert_healthy();
        }
    }
}

// Immutable-source probe/regression for #756. Reports only category labels and
// booleans, never fixture values, native payloads, headers or file locations.
fn probe_known_value(value: &Value, known: &[&str]) -> bool {
    let text_has = |text: &str| known.iter().any(|secret| text.contains(secret));
    match value {
        Value::String(text) => text_has(text),
        Value::Array(items) => items.iter().any(|v| probe_known_value(v, known)),
        Value::Object(object) => object
            .iter()
            .any(|(key, v)| text_has(key) || probe_known_value(v, known)),
        _ => false,
    }
}
fn probe_native_semantics(items: &[Value], known: &[&str]) -> bool {
    items.iter().any(|item| {
        probe_known_value(item, known)
            || item["type"] == "function_call"
                && item["arguments"]
                    .as_str()
                    .and_then(|args| serde_json::from_str::<Value>(args).ok())
                    .is_some_and(|args| probe_known_value(&args, known))
    })
}

#[tokio::test]
async fn known_secret_turns_have_no_tool_execution_or_replay_authority() {
    let mut failures = Vec::new();
    let mut rows = Vec::new();
    for rotated in [false, true] {
        let secrets: Vec<(&str, &str)> = if rotated {
            vec![
                ("retired_access", KEY),
                ("retired_refresh", "synthetic-refresh"),
                ("current_access", "synthetic-renewed-access"),
                ("current_refresh", "synthetic-renewed-refresh"),
                ("account_id", "synthetic-account"),
            ]
        } else {
            vec![("api_key", KEY)]
        };
        let known: Vec<_> = secrets.iter().map(|(_, value)| *value).collect();
        let mut cases: Vec<_> = secrets
            .iter()
            .flat_map(|(label, secret)| {
                [
                    "text",
                    "reasoning_text",
                    "opaque_reasoning",
                    "arguments",
                    "escaped_arguments",
                ]
                .into_iter()
                .map(move |place| (*label, *secret, place))
            })
            .collect();
        cases.push(("negative", "", "clean"));
        cases.push(("identity", secrets[0].1, "identity"));
        for (label, secret, place) in cases {
            let fixture = Fixture::new();
            let path = fixture.token_path(false);
            let refresh = MockOpenAi::start(vec![Reply::json(
                json!({"access_token":"synthetic-renewed-access","refresh_token":"synthetic-renewed-refresh","expires_in":3600}),
            )]);
            let prefix = "Visible prefix. ";
            let text = if place == "text" {
                format!("{prefix}{secret}")
            } else {
                prefix.into()
            };
            let mut args=json!({"outer":{"value":if matches!(place,"arguments"|"escaped_arguments"){secret}else{"ordinary synthetic value"}}}).to_string();
            if place == "escaped_arguments" {
                args = args.replacen(
                    secret,
                    &format!("\\u{:04x}{}", secret.as_bytes()[0], &secret[1..]),
                    1,
                );
            }
            let id = if place == "identity" {
                format!("call_{secret}")
            } else {
                "call_probe".into()
            };
            let reasoning_text = if place == "reasoning_text" {
                secret
            } else {
                "ordinary reasoning"
            };
            let output = vec![
                json!({"type":"message","id":"msg_probe","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}),
                json!({"type":"reasoning","id":"rs_probe","encrypted_content":if place=="opaque_reasoning"{secret}else{"opaque ordinary data"},"content":[{"type":"reasoning_text","text":reasoning_text}],"summary":[]}),
                json!({"type":"function_call","id":"fc_probe","call_id":id,"name":"read","arguments":args,"status":"completed"}),
            ];
            let mut events = vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_probe","role":"assistant","content":[]}}),
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":prefix}),
                json!({"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_probe","content":[],"summary":[]}}),
            ];
            if place == "text" {
                let mid = secret.len() / 2;
                for part in [&secret[..mid], &secret[mid..]] {
                    events.push(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":part}));
                }
            }
            let mid = reasoning_text.len() / 2;
            for part in [&reasoning_text[..mid], &reasoning_text[mid..]] {
                events.push(json!({"type":"response.reasoning_text.delta","output_index":1,"content_index":0,"delta":part}));
            }
            let mut opening = output[2].clone();
            opening["arguments"] = json!("");
            opening["status"] = json!("in_progress");
            events.push(json!({"type":"response.output_item.added","output_index":2,"item":opening}));
            let mid = args.len() / 2;
            for part in [&args[..mid], &args[mid..]] {
                events.push(
                    json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":part}),
                );
            }
            events.push(json!({"type":"response.output_item.done","output_index":2,"item":output[2]}));
            events.push(json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":9}}}));
            let upstream = MockOpenAi::start(vec![Reply::events(events), answer()]);
            let base = upstream.base_url();
            let endpoint = refresh.base_url();
            let auth = || {
                if rotated {
                    subscription_auth(&path, &endpoint)
                } else {
                    Auth::ApiKey(&fixture.key)
                }
            };
            let mut chunks = Vec::new();
            let mut usage = None;
            let result = Client::new()
                .unwrap()
                .stream_with_usage(
                    chat(&base, &payload(), &[]),
                    auth(),
                    &Cancellation::default(),
                    &mut |event| {
                        if let Event::Chunk(chunk) = event {
                            chunks.push(chunk);
                        }
                        Control::Continue
                    },
                    &mut usage,
                )
                .await;
            let mut retained = false;
            let mut replay_exact = false;
            let mut args_equal = true;
            let mut tool_semantic = false;
            let mut forwarded = false;
            if let Ok(completed) = &result {
                retained = probe_native_semantics(completed.replay_items.as_ref(), &known);
                replay_exact = completed.replay_items.as_ref() == output.as_slice();
                let mut assembler = oxa_engine::ReplyAssembler::new();
                for chunk in &chunks {
                    assembler.chunk_value(chunk).unwrap();
                }
                assembler.chunk_value(&json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":9,"total_tokens":21}})).unwrap();
                let turn = assembler.finish(oxa_proto::ThinkingLevel::Off).unwrap();
                args_equal = turn.msg.tool_calls.iter().all(|call| call.args_raw == args);
                tool_semantic = turn.msg.tool_calls.iter().any(|call| {
                    probe_known_value(&serde_json::from_str::<Value>(&call.args_raw).unwrap(), &known)
                });
                // Exercise the existing serialized-replay input path, not a new
                // carrier implementation. Both mock requests stay on loopback.
                let saved = serde_json::to_vec(completed.replay_items.as_ref()).unwrap();
                let restored: Vec<Value> = serde_json::from_slice(&saved).unwrap();
                let mut next = payload();
                let messages = next["messages"].as_array_mut().unwrap();
                messages.push(completed.assistant.clone());
                for call in &turn.msg.tool_calls {
                    messages.push(
                        json!({"role":"tool","tool_call_id":call.id,"content":"ordinary synthetic result"}),
                    );
                }
                let replay = [Some(request::ReplayItems::new(restored))];
                let continuation = Client::new()
                    .unwrap()
                    .stream(
                        chat(&base, &next, &replay),
                        auth(),
                        &Cancellation::default(),
                        &mut |_| Control::Continue,
                    )
                    .await;
                assert!(
                    continuation.is_ok(),
                    "probe continuation failed without payload diagnostics"
                );
                let captured = upstream.wait_for_requests(2);
                forwarded = probe_native_semantics(captured[1].body["input"].as_array().unwrap(), &known);
            }
            let refused = matches!(result, Err(client::Error::Protocol));
            let clean = place == "clean";
            let accepted_ok = result.is_ok();
            let pass = if clean {
                accepted_ok && replay_exact && !retained && args_equal && !tool_semantic && !forwarded
            } else {
                refused
                    && chunks
                        .iter()
                        .all(|chunk| chunk.pointer("/choices/0/delta/tool_calls").is_none())
                    && !probe_known_value(&json!(chunks), &known)
                    && usage.as_ref().is_some_and(|u| u.output == Some(9))
            };
            rows.push(json!({"profile":if rotated{"rotated_subscription"}else{"api"},"known_class":label,"placement":place,
                "accepted":accepted_ok,"chunks":chunks.len(),"replay_retains_known_value":retained,"replay_exact":replay_exact,
                "delivered_arguments_match_native":args_equal,"tool_decoded_known_value":tool_semantic,
                "continuation_retains_known_value":forwarded,"guard_contract_satisfied":pass}));
            if !pass {
                failures.push((rotated, label, place));
            }
        }
    }
    for row in &rows {
        println!("SECRET_BOUNDARY_PROBE {row}");
    }
    assert!(
        failures.is_empty(),
        "known-secret execution/replay contract failed in {} cases",
        failures.len()
    );
}

#[tokio::test]
async fn live_output_preserves_content_and_reasoning_order_in_real_assembler() {
    for subscription in [false, true] {
        let output = json!([
            {"type":"message","id":"msg_order","role":"assistant","content":[{"type":"output_text","text":"<think>A</think>B"}]},
            {"type":"reasoning","id":"rs_order","content":[{"type":"reasoning_text","text":"C"}],"summary":[]}
        ]);
        let upstream = MockOpenAi::start(vec![Reply::events(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_order","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"<think>A</think>B"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_order","content":[],"summary":[]}}),
            json!({"type":"response.reasoning_text.delta","output_index":1,"content_index":0,"delta":"C"}),
            json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":9}}}),
        ])]);
        let fixture = Fixture::new();
        let path = fixture.token_path(true);
        let base = upstream.base_url();
        let auth = if subscription {
            subscription_auth(&path, &base)
        } else {
            Auth::ApiKey(&fixture.key)
        };
        let mut assembler = oxa_engine::ReplyAssembler::new();
        let completed = Client::new()
            .unwrap()
            .stream(
                chat(&base, &payload(), &[]),
                auth,
                &Cancellation::default(),
                &mut |event| {
                    if let Event::Chunk(chunk) = event {
                        assembler.chunk_value(&chunk).unwrap();
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        assembler
            .chunk_value(
                &json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":9,"total_tokens":21}}),
            )
            .unwrap();
        let turn = assembler.finish(oxa_proto::ThinkingLevel::Off).unwrap();
        assert_eq!(turn.msg.content.as_deref(), Some("B"));
        assert_eq!(turn.msg.reasoning.as_deref(), Some("AC"));
        assert_eq!(completed.output_items.as_ref(), output.as_array().unwrap());
    }
}

#[tokio::test]
async fn live_redacted_text_and_reasoning_precede_the_held_terminal_but_tools_do_not() {
    for subscription in [false, true] {
        for secret in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.token_path(true);
            let peer = HeldResponsesPeer::start();
            let text = if secret {
                format!("Visible live {KEY} text.")
            } else {
                "Visible live ordinary text.".into()
            };
            let reasoning = "Ordinary live reasoning.";
            let tool = json!({"type":"function_call","id":"fc_live","call_id":"call_live","name":"read","arguments":"{\"path\":\"ordinary.txt\"}","status":"completed"});
            peer.send(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_live","role":"assistant","content":[]}}));
            // Split inside the exact held value; the existing field redactor
            // must mask it while still making ordinary text visible live.
            let middle = "Visible live ".len() + if secret { KEY.len() / 2 } else { 3 };
            for part in [&text[..middle], &text[middle..]] {
                peer.send(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":part}));
            }
            peer.send(json!({"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_live","content":[],"summary":[]}}));
            peer.send(json!({"type":"response.reasoning_text.delta","output_index":1,"content_index":0,"delta":reasoning}));
            peer.send(json!({"type":"response.output_item.added","output_index":2,"item":tool}));
            peer.send(json!({"type":"response.output_item.done","output_index":2,"item":tool}));
            let base = &peer.base;
            let auth = if subscription {
                subscription_auth(&path, base)
            } else {
                Auth::ApiKey(&fixture.key)
            };
            let (seen, received) = tokio::sync::oneshot::channel();
            let mut seen = Some(seen);
            let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let in_callback = released.clone();
            let mut visible = false;
            let mut thought = false;
            let mut chunks = Vec::new();
            let mut callback = |event| {
                if let Event::Chunk(chunk) = event {
                    if chunk.pointer("/choices/0/delta/tool_calls").is_some() {
                        assert!(
                            in_callback.load(Ordering::Acquire),
                            "tool escaped before terminal authority"
                        );
                    }
                    visible |= chunk["choices"][0]["delta"]["content"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty());
                    thought |= chunk["choices"][0]["delta"]["reasoning_content"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty());
                    if visible && thought && !in_callback.load(Ordering::Acquire) {
                        if let Some(seen) = seen.take() {
                            let _ = seen.send(());
                        }
                    }
                    chunks.push(chunk);
                }
                Control::Continue
            };
            let body = payload();
            let cancel = Cancellation::default();
            let client = Client::new().unwrap();
            let mut usage = None;
            let inference =
                client.stream_with_usage(chat(base, &body, &[]), auth, &cancel, &mut callback, &mut usage);
            let release = async {
                received.await.expect("no visible text/reasoning before terminal");
                released.store(true, Ordering::Release);
                peer.send(json!({"type":"response.completed","response":{"status":"completed","output":[
                    {"type":"message","id":"msg_live","role":"assistant","content":[{"type":"output_text","text":text}]},
                    {"type":"reasoning","id":"rs_live","content":[{"type":"reasoning_text","text":reasoning}],"summary":[]},tool
                ],"usage":{"input_tokens":12,"output_tokens":9}}}));
            };
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(inference, release)
            })
            .await
            .expect("terminal release depended on buffered output");
            assert!(released.load(Ordering::Acquire));
            assert!(
                !probe_known_value(&json!(chunks), &[KEY]),
                "live redactor exposed a held value"
            );
            let mut assembler = oxa_engine::ReplyAssembler::new();
            for chunk in &chunks {
                assembler.chunk_value(chunk).unwrap();
            }
            if secret {
                assert!(matches!(result, Err(client::Error::Protocol)));
                assert!(chunks
                    .iter()
                    .all(|c| c.pointer("/choices/0/delta/tool_calls").is_none()));
                assert!(usage.as_ref().is_some_and(|u| u.output == Some(9)));
            } else {
                let completed = result.unwrap();
                assert!(!completed.replay_items.is_empty());
                assembler.chunk_value(&json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":9,"total_tokens":21}})).unwrap();
                let turn = assembler.finish(oxa_proto::ThinkingLevel::Off).unwrap();
                assert_eq!(turn.msg.content.as_deref(), Some(text.as_str()));
                assert_eq!(turn.msg.reasoning.as_deref(), Some(reasoning));
                assert_eq!(turn.msg.tool_calls[0].id, "call_live");
                assert_eq!(
                    turn.msg.tool_calls[0].args_raw,
                    tool["arguments"].as_str().unwrap()
                );
            }
        }
    }
}

#[tokio::test]
async fn stopping_live_text_delivery_cancels_a_held_peer_without_terminal_usage() {
    for subscription in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.token_path(true);
        let peer = HeldResponsesPeer::start();
        peer.send(held_text());
        let auth = if subscription {
            subscription_auth(&path, &peer.base)
        } else {
            Auth::ApiKey(&fixture.key)
        };
        let mut saw_text = false;
        let mut usage = None;
        let result=tokio::time::timeout(std::time::Duration::from_secs(2),Client::with_read_timeout(None).unwrap().stream_with_usage(
            chat(&peer.base,&payload(),&[]),auth,&Cancellation::default(),&mut |event| {
                if matches!(event,Event::Chunk(ref chunk) if chunk["choices"][0]["delta"]["content"].as_str().is_some_and(|s|!s.is_empty())) { saw_text=true;return Control::Stop; }
                Control::Continue
            },&mut usage)).await.expect("live cancellation waited for terminal event");
        assert!(saw_text);
        assert!(matches!(result, Err(client::Error::Cancelled)));
        assert!(usage.is_none());
    }
}
