//! #289: the real core and read tool using the socket-free Engine adapter.
//! The injected peer replaces provider I/O; it never reads a key or opens a port.
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::{EmbeddedEngine, EmbeddedGateway, Flow, GatewayContext, GatewayDiscovery, GatewayEvent};
use oxa_fake::TestEnv;
use oxa_perm::ModePolicy;
use oxa_proto::{codes, AgentEvent, CancelToken, EntryBody, HeadlessUi, Mode, OxaError, Role, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use oxa_tools::ReadTool;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

enum Reply {
    Tool,
    Text(&'static str),
    Budget,
    FailedSettlement,
}
#[derive(Default)]
struct Calls {
    bodies: Vec<Vec<oxa_proto::WireMsg>>,
    models: Vec<String>,
    contexts: Vec<(String, String, String)>,
}
struct Gateway {
    calls: Arc<Mutex<Calls>>,
    replies: Mutex<VecDeque<Reply>>,
}
impl EmbeddedGateway for Gateway {
    fn discovery(&self) -> Result<GatewayDiscovery, OxaError> {
        Ok(GatewayDiscovery {
            models: json!({"data":[{"id":"cloud-model", "remote":true,
                "context_length":64000, "max_output_tokens":4096}]}),
            status: json!({"engine_mode":"none", "model_limits":{"cloud-model":{
                "cap":64000, "max_output":4096, "capabilities":{
                    "remote":true,"remote_provider":"openrouter","context_window":64000,
                    "cancel":true,"echo":false,"cache_hints":false,"prefill":false,
                    "thinking":false,"vision":false
                }
            }}}),
        })
    }
    fn chat(
        &self,
        request: &oxa_proto::ChatRequest<'_>,
        context: GatewayContext<'_>,
        cancel: &CancelToken,
        on: &mut dyn FnMut(GatewayEvent) -> Flow,
    ) -> Result<(), OxaError> {
        {
            let mut calls = self.calls.lock().unwrap();
            calls.bodies.push(request.messages.to_vec());
            calls.models.push(request.model.into());
            calls.contexts.push((
                context.origin.into(),
                context.session.into(),
                context.idempotency_key.into(),
            ));
        }
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected gateway replay");
        if matches!(reply, Reply::Budget) {
            return Err(OxaError::new(codes::BUDGET_EXHAUSTED, "synthetic daily limit"));
        }
        let mut events = match reply {
            Reply::Tool => vec![
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-read",
                    "function":{"name":"read","arguments":"{ \"path\": "}}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,
                    "function":{"arguments":"\"sample.txt\" }"}}]}}]}),
            ],
            Reply::Text(text) => vec![json!({"choices":[{"delta":{"content":text}}]})],
            Reply::FailedSettlement => vec![json!({"choices":[{"delta":{"content":"unsettled answer"}}]})],
            Reply::Budget => unreachable!(),
        };
        events.push(json!({"choices":[{"delta":{},"finish_reason":
            if matches!(reply, Reply::Tool) {"tool_calls"} else {"stop"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}}));
        for event in events
            .into_iter()
            .map(GatewayEvent::Chunk)
            .chain([GatewayEvent::Done])
        {
            if cancel.is_cancelled() || on(event) == Flow::Stop {
                return Err(OxaError::new(codes::ABORTED, "request cancelled"));
            }
        }
        if matches!(reply, Reply::FailedSettlement) {
            return Err(OxaError::new(
                "cloud_ledger_unavailable",
                "synthetic settlement failure",
            ));
        }
        Ok(())
    }
}

fn open(env: &TestEnv, replies: Vec<Reply>) -> (Session, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let gateway = Arc::new(Gateway {
        calls: calls.clone(),
        replies: Mutex::new(replies.into()),
    });
    let deps = Deps {
        engine: Arc::new(EmbeddedEngine::new(gateway)),
        store: Arc::new(MemStore::new()),
        tools: vec![Arc::new(ReadTool::new())],
        hooks: vec![],
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(json!({"oxa":{"envNote":false}}), json!({})),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: Some("cloud-model".into()),
        thinking: None,
        mode: Some(Mode::Workspace),
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    (Session::open(deps, Open::New).unwrap(), calls)
}

#[test]
fn a_real_read_tool_loop_replays_raw_calls_and_remote_metadata_in_process() {
    let env = TestEnv::new();
    env.write("work/sample.txt", "synthetic file contents\n");
    let (mut session, calls) = open(&env, vec![Reply::Tool, Reply::Text("Done.\n")]);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    session
        .prompt("read sample.txt".into(), vec![], &mut sink, &mut ui)
        .unwrap();
    let wire = session.inner().transcript.wire();
    assert_eq!(
        wire.iter().map(|m| m.role).collect::<Vec<_>>(),
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(wire[1].tool_calls[0].args_raw, "{ \"path\": \"sample.txt\" }");
    assert!(wire[2]
        .content
        .as_deref()
        .unwrap()
        .contains("synthetic file contents"));
    assert_eq!(wire[3].content.as_deref(), Some("Done.\n"));
    assert!(session.state().model_remote);
    assert_eq!(session.state().model_provider, Some("openrouter"));
    assert!(sink
        .0
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. })));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.bodies.len(), 2, "no local prefill or extra engine request");
    let replay = &calls.bodies[1];
    assert!(replay
        .iter()
        .any(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("call-read")));
    assert!(calls.models.iter().all(|model| model == "cloud-model"));

    assert!(calls
        .contexts
        .iter()
        .all(|(origin, session, key)| origin == "oxa" && !session.is_empty() && !key.is_empty()));
    assert_eq!(calls.contexts[0].1, calls.contexts[1].1);
    assert_ne!(calls.contexts[0].2, calls.contexts[1].2);
}

#[test]
fn a_budget_refusal_does_not_retry_or_commit_an_assistant() {
    let env = TestEnv::new();
    let (mut session, calls) = open(&env, vec![Reply::Budget]);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    session
        .prompt("hello".into(), vec![], &mut sink, &mut ui)
        .unwrap();
    assert_eq!(calls.lock().unwrap().bodies.len(), 1);
    assert!(session
        .inner()
        .transcript
        .wire()
        .iter()
        .all(|m| m.role != Role::Assistant));
    assert!(session.inner().tree.entries().iter().any(|entry| matches!(
        &entry.body, EntryBody::Shown {text,..} if text.contains("synthetic daily limit")
    )));
    assert!(!sink
        .0
        .iter()
        .any(|event| matches!(event, AgentEvent::AutoRetryStart { .. })));
}

#[test]
fn unsettled_stream_text_stays_out_of_history_and_the_next_turn_is_healthy() {
    let env = TestEnv::new();
    let (mut session, calls) = open(&env, vec![Reply::FailedSettlement, Reply::Text("healthy answer")]);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    session
        .prompt("first".into(), vec![], &mut sink, &mut ui)
        .unwrap();
    assert_eq!(
        calls.lock().unwrap().bodies.len(),
        1,
        "a visible failed stream cannot retry"
    );
    assert!(session
        .inner()
        .transcript
        .wire()
        .iter()
        .all(|m| m.role != Role::Assistant));
    assert!(session.inner().tree.entries().iter().any(|entry| matches!(
        &entry.body, EntryBody::Shown {text,..} if text == "unsettled answer"
    )));
    session
        .prompt("again".into(), vec![], &mut sink, &mut ui)
        .unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.bodies.len(), 2);
    assert!(!calls.bodies[1].iter().any(|message| message
        .content
        .as_deref()
        .is_some_and(|text| text.contains("unsettled answer"))));
    assert_eq!(
        session
            .inner()
            .transcript
            .wire()
            .last()
            .unwrap()
            .content
            .as_deref(),
        Some("healthy answer")
    );
}
