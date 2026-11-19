//! Socket-free Engine contracts for #289's in-process adapter.
use oxa_engine::{EmbeddedEngine, EmbeddedGateway, Flow, GatewayContext, GatewayDiscovery, GatewayEvent};
use oxa_proto::{
    codes, CancelToken, ChatRequest, Engine, Finish, FrozenPrefix, Hints, OxaError, StreamEvent,
    ThinkingLevel, TurnKind, WireMsg,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// The process-wide network policy is shared only within this test binary.
static POLICY: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Capture {
    discovery: usize,
    bodies: Vec<Vec<WireMsg>>,
    identity: Vec<(usize, usize, String, u32, ThinkingLevel)>,
    contexts: Vec<(String, String, String)>,
    stopped: bool,
}
struct Script {
    capture: Arc<Mutex<Capture>>,
    events: Vec<GatewayEvent>,
    error: Option<&'static str>,
}
impl EmbeddedGateway for Script {
    fn discovery(&self) -> Result<GatewayDiscovery, OxaError> {
        self.capture.lock().unwrap().discovery += 1;
        Ok(GatewayDiscovery {
            models: json!({"data":[{
                "id":"cloud-model", "remote":true, "context_length":64000,
                "max_output_tokens":4096, "remote_provider":"openrouter"
            }]}),
            status: json!({"engine_mode":"none", "model":"local-model"}),
        })
    }
    fn chat(
        &self,
        request: &ChatRequest<'_>,
        context: GatewayContext<'_>,
        _cancel: &CancelToken,
        on: &mut dyn FnMut(GatewayEvent) -> Flow,
    ) -> Result<(), OxaError> {
        {
            let mut capture = self.capture.lock().unwrap();
            capture.bodies.push(request.messages.to_vec());
            capture.identity.push((
                request.prefix as *const _ as usize,
                request.messages.as_ptr() as usize,
                request.model.into(),
                request.max_tokens,
                request.effective_level(),
            ));
            capture.contexts.push((
                context.origin.to_string(),
                context.session.to_string(),
                context.idempotency_key.to_string(),
            ));
        }
        for event in &self.events {
            if on(event.clone()) == Flow::Stop {
                self.capture.lock().unwrap().stopped = true;
                return Ok(());
            }
        }
        self.error.map_or(Ok(()), |code| {
            Err(OxaError::new(code, "synthetic gateway refusal"))
        })
    }
}
fn engine(events: Vec<GatewayEvent>, error: Option<&'static str>) -> (EmbeddedEngine, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let gateway = Script {
        capture: capture.clone(),
        events,
        error,
    };
    (EmbeddedEngine::new(Arc::new(gateway)), capture)
}
fn data(value: Value) -> GatewayEvent {
    GatewayEvent::Chunk(value)
}
fn complete() -> Vec<GatewayEvent> {
    vec![
        data(json!({"choices":[{"delta":{"content":"answer"}}]})),
        data(json!({"choices":[{"finish_reason":"stop","delta":{}}],
            "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}})),
        GatewayEvent::Done,
    ]
}
fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".into(), "SYS".into())],
        tools_json: "[]".into(),
        sha256: [0; 32],
    }
}
fn request<'a>(prefix: &'a FrozenPrefix, messages: &'a [WireMsg]) -> ChatRequest<'a> {
    ChatRequest {
        max_tokens: 64,
        thinking: true,
        preserve_thinking: true,
        hints: Hints {
            session: "synthetic-session",
            idem_key: "synthetic-request-id",
            kind: TurnKind::Chat,
            background: false,
            snapshot_keep: true,
        },
        ..ChatRequest::new(prefix, messages, "cloud-model")
    }
}

#[test]
fn discovery_and_chat_borrow_the_same_typed_request() {
    let _policy = POLICY.lock().unwrap();
    let (engine, capture) = engine(complete(), None);
    let info = engine.info(None).unwrap();
    assert_eq!(info.base_url, "in-process");
    assert_eq!(info.model, "cloud-model");
    assert_eq!(info.n_ctx, 64000);
    assert_eq!(info.max_output, 4096);
    assert!(info.caps.remote);
    assert!(!info.caps.echo);
    assert!(!info.caps.cache_hints);
    let prefix = prefix();
    let messages = [WireMsg::user("hello")];
    let request = request(&prefix, &messages);
    let mut events = Vec::new();
    let turn = engine
        .chat(&request, &CancelToken::new(), &mut |e| events.push(e))
        .unwrap();
    assert_eq!(turn.model, "cloud-model", "model defaults to the requested alias");
    assert_eq!(turn.msg.content.as_deref(), Some("answer"));
    assert_eq!(turn.usage.prompt, 7);
    assert_eq!(turn.usage.completion, 3);
    assert_eq!(turn.level, ThinkingLevel::Medium);
    assert_eq!(turn.finish, Finish::Stop);
    assert_eq!(events, vec![StreamEvent::Text("answer".into())]);
    let capture = capture.lock().unwrap();
    assert_eq!(capture.bodies, vec![messages.to_vec()]);
    assert_eq!(
        capture.identity,
        vec![(
            &prefix as *const _ as usize,
            messages.as_ptr() as usize,
            "cloud-model".into(),
            64,
            ThinkingLevel::Medium
        )]
    );
    assert_eq!(
        capture.contexts,
        vec![(
            "oxa".into(),
            "synthetic-session".into(),
            "synthetic-request-id".into()
        )]
    );
}

#[test]
fn tool_calls_keep_raw_arguments_and_replay_with_the_tool_result() {
    let _policy = POLICY.lock().unwrap();
    let events = vec![
        data(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1",
            "function":{"name":"lookup","arguments":"{ \"name\": "}}]}}]}),
        ),
        data(json!({"choices":[{"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":"\"sample\" }"}}]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}})),
        GatewayEvent::Done,
    ];
    let (engine, capture) = engine(events, None);
    let prefix = prefix();
    let mut messages = vec![WireMsg::user("lookup sample")];
    let turn = engine
        .chat(&request(&prefix, &messages), &CancelToken::new(), &mut |_| {})
        .unwrap();
    assert_eq!(turn.finish, Finish::ToolCalls);
    assert_eq!(turn.msg.tool_calls[0].args_raw, "{ \"name\": \"sample\" }");
    messages.push(turn.msg);
    messages.push(WireMsg::tool_result("call-1", "found"));
    engine
        .chat(&request(&prefix, &messages), &CancelToken::new(), &mut |_| {})
        .unwrap();
    let capture = capture.lock().unwrap();
    let replay = &capture.bodies[1];
    assert_eq!(replay[1].tool_calls[0].args_raw, "{ \"name\": \"sample\" }");
    assert_eq!(replay[2].tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(replay[2].content.as_deref(), Some("found"));
}

#[test]
fn incomplete_or_malformed_streams_never_return_an_assistant_turn() {
    let _policy = POLICY.lock().unwrap();
    let prefix = prefix();
    for events in [
        vec![],
        complete()[..2].to_vec(),
        vec![GatewayEvent::Done],
        vec![GatewayEvent::Chunk(json!("invalid object")), GatewayEvent::Done],
        vec![
            data(json!({"usage":{"prompt_tokens":1,"completion_tokens":1}})),
            GatewayEvent::Done,
        ],
        [complete(), vec![GatewayEvent::Done]].concat(),
    ] {
        let (engine, _) = engine(events, None);
        let error = engine
            .chat(&request(&prefix, &[]), &CancelToken::new(), &mut |_| {})
            .unwrap_err();
        assert_eq!(error.code, codes::STREAM_BROKEN);
    }
}

#[test]
fn finish_and_done_without_usage_completes_with_unknown_usage() {
    let _policy = POLICY.lock().unwrap();
    let prefix = prefix();
    let events = vec![
        data(json!({"choices":[{"delta":{"content":"answer"}}]})),
        data(json!({"choices":[{"finish_reason":"stop","delta":{}}]})),
        GatewayEvent::Done,
    ];
    let (engine, _) = engine(events, None);
    let turn = engine
        .chat(&request(&prefix, &[]), &CancelToken::new(), &mut |_| {})
        .unwrap();
    assert_eq!((turn.usage.prompt, turn.usage.completion), (0, 0));
    assert!(turn.usage.cached_missing);
}

#[test]
fn gateway_refusals_are_preserved_before_data_and_cut_after_data() {
    let _policy = POLICY.lock().unwrap();
    let prefix = prefix();
    let request = request(&prefix, &[]);
    for code in [codes::BUDGET_EXHAUSTED, codes::DENIED, codes::ENGINE_DOWN] {
        let (engine, capture) = engine(vec![], Some(code));
        assert_eq!(
            engine
                .chat(&request, &CancelToken::new(), &mut |_| {})
                .unwrap_err()
                .code,
            code
        );
        assert_eq!(
            capture.lock().unwrap().bodies.len(),
            1,
            "the adapter never retries"
        );
        let (engine, _) = self::engine(complete(), Some(code));
        assert_eq!(
            engine
                .chat(&request, &CancelToken::new(), &mut |_| {})
                .unwrap_err()
                .code,
            codes::STREAM_BROKEN,
            "even Done does not commit success before ledger settlement"
        );
    }
    // In-stream JSON errors keep ReplyAssembler's existing F3 classification.
    // Before-stream budget refusals use the typed gateway error tested above.
    let (engine, capture) = engine(
        vec![
            data(json!({"error":{
        "code":"budget_exceeded","message":"synthetic daily limit"}})),
            GatewayEvent::Done,
        ],
        None,
    );
    assert_eq!(
        engine
            .chat(&request, &CancelToken::new(), &mut |_| {})
            .unwrap_err()
            .code,
        codes::STREAM_BROKEN
    );
    assert!(capture.lock().unwrap().stopped);
}

#[test]
fn cancellation_stops_delivery_and_cannot_commit_a_partial_reply() {
    let _policy = POLICY.lock().unwrap();
    let prefix = prefix();
    let request = request(&prefix, &[]);
    let (engine, capture) = engine(complete(), None);
    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        engine.chat(&request, &cancel, &mut |_| {}).unwrap_err().code,
        codes::ABORTED
    );
    assert!(capture.lock().unwrap().bodies.is_empty());
    let cancel = CancelToken::new();
    let mut events = Vec::new();
    let error = engine
        .chat(&request, &cancel, &mut |event| {
            events.push(event);
            cancel.cancel();
        })
        .unwrap_err();
    assert_eq!(error.code, codes::ABORTED);
    assert_eq!(events.len(), 1);
    assert!(capture.lock().unwrap().stopped);
}

#[test]
fn cancellation_during_the_final_buffer_flush_keeps_the_settled_turn() {
    let _policy = POLICY.lock().unwrap();
    let prefix = prefix();
    let events = vec![
        data(json!({"choices":[{"delta":{"content":"<thi"}}]})),
        data(json!({"choices":[{"finish_reason":"stop","delta":{}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1}})),
        GatewayEvent::Done,
    ];
    let (engine, _) = engine(events, None);
    let cancel = CancelToken::new();
    let mut delivered = Vec::new();
    let turn = engine
        .chat(&request(&prefix, &[]), &cancel, &mut |event| {
            delivered.push(event);
            cancel.cancel();
        })
        .expect("the reply was already settled at [DONE]");
    assert_eq!(delivered, vec![StreamEvent::Text("<thi".into())]);
    assert_eq!(turn.msg.content.as_deref(), Some("<thi"));
}

/// A gateway that settles the reply and only then sees the user's cancel.
struct CancelAfterDone;
impl EmbeddedGateway for CancelAfterDone {
    fn discovery(&self) -> Result<GatewayDiscovery, OxaError> {
        Script {
            capture: Arc::default(),
            events: Vec::new(),
            error: None,
        }
        .discovery()
    }
    fn chat(
        &self,
        _request: &ChatRequest<'_>,
        _context: GatewayContext<'_>,
        cancel: &CancelToken,
        on: &mut dyn FnMut(GatewayEvent) -> Flow,
    ) -> Result<(), OxaError> {
        for event in complete() {
            on(event);
        }
        cancel.cancel();
        Ok(())
    }
}

#[test]
fn cancel_after_done_keeps_the_settled_reply() {
    let _policy = POLICY.lock().unwrap();
    let prefix = prefix();
    let engine = EmbeddedEngine::new(Arc::new(CancelAfterDone));
    let turn = engine
        .chat(&request(&prefix, &[]), &CancelToken::new(), &mut |_| {})
        .expect("a reply finished and settled before the cancel is returned");
    assert_eq!(turn.msg.content.as_deref(), Some("answer"));
}

#[test]
fn offline_refuses_before_gateway_calls_or_credential_access() {
    let _policy = POLICY.lock().unwrap();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            oxa_proto::set_net_policy(false, false);
        }
    }
    let _reset = Reset;
    oxa_proto::set_net_policy(true, false);
    let (engine, capture) = engine(complete(), None);
    let prefix = prefix();
    assert_eq!(engine.info(None).unwrap_err().code, codes::OFFLINE);
    assert_eq!(engine.count_tokens("text").unwrap_err().code, codes::OFFLINE);
    assert_eq!(
        engine
            .chat(&request(&prefix, &[]), &CancelToken::new(), &mut |_| {})
            .unwrap_err()
            .code,
        codes::OFFLINE
    );
    let capture = capture.lock().unwrap();
    assert_eq!(capture.discovery, 0);
    assert!(capture.bodies.is_empty());
}

#[test]
fn missing_idempotency_keys_are_generated_once_for_the_gateway_call() {
    let _policy = POLICY.lock().unwrap();
    let (engine, capture) = engine(complete(), None);
    let prefix = prefix();
    let mut request = request(&prefix, &[]);
    request.hints.idem_key = "";
    engine.chat(&request, &CancelToken::new(), &mut |_| {}).unwrap();
    let capture = capture.lock().unwrap();
    assert_eq!(capture.contexts.len(), 1);
    assert!(!capture.contexts[0].2.is_empty());
    assert_eq!(engine.count_prompt(&request).unwrap(), None);
    assert_eq!(
        engine.count_tokens("text").unwrap_err().code,
        codes::UNSUPPORTED_IN_OXA
    );
}

/// A malformed chunk after visible reply data is a cut reply that must not be replayed.
#[test]
fn malformed_chunk_after_data_is_marked_cut() {
    let _policy = POLICY.lock().unwrap();
    let prefix = prefix();
    let (engine, _) = engine(
        vec![
            data(json!({"choices":[{"delta":{"content":"partial"}}]})),
            GatewayEvent::Chunk(json!("invalid object")),
            GatewayEvent::Done,
        ],
        None,
    );
    let error = engine
        .chat(&request(&prefix, &[]), &CancelToken::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(error.code, codes::STREAM_BROKEN);
    assert!(
        error.message.starts_with(oxa_proto::CUT_BY_REFUSAL),
        "{}",
        error.message
    );
}
