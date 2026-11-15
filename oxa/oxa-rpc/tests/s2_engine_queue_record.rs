//! S2 (ENG-003): a request waiting in the engine's queue is a JSON / RPC record
//! `{"type": "engine_queue", "pos": <n>, "waitS": <s>}`, never an error, and does not change
//! the reply. Isolation: `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{AgentEvent, HeadlessUi};
use oxa_rpc::{event_to_json, JsonSink};
use oxa_session::{MemStore, SessionStore};
use serde_json::{json, Value};
use std::sync::Arc;

#[test]
fn engine_queue_record_shape() {
    assert_eq!(
        event_to_json(&AgentEvent::EngineQueue {
            pos: Some(3),
            wait_s: 15
        }),
        json!({"type": "engine_queue", "pos": 3, "waitS": 15})
    );
    // The engine's own line has no position: the key is left out.
    assert_eq!(
        event_to_json(&AgentEvent::EngineQueue { pos: None, wait_s: 2 }),
        json!({"type": "engine_queue", "waitS": 2})
    );
}

#[test]
fn queued_request_streams_engine_queue_records() {
    let env = TestEnv::new();
    let mut turn = ScriptTurn::text("the reply");
    turn.fault = Some(Fault::Queued { count: 2, pos: true });
    let dirs = Dirs::under(env.root());
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut s = Session::open(
        Deps {
            engine: Arc::new(ScriptedEngine::new(vec![turn])),
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(policy::Deny),
            sandbox: Arc::new(sandbox::NoBwrap),
            settings: Settings::default(),
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Default::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    let mut sink = JsonSink {
        out: Vec::new(),
        error: None,
    };
    s.prompt(
        "hi".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    let records: Vec<Value> = String::from_utf8(sink.out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let queue: Vec<&Value> = records.iter().filter(|r| r["type"] == "engine_queue").collect();
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0]["pos"], 2);
    assert_eq!(queue[1]["pos"], 1);
    assert!(queue.iter().all(|r| r["waitS"].is_u64()));
    let end = records
        .iter()
        .find(|r| r["type"] == "message_end" && r["message"]["role"] == "assistant")
        .expect("the assistant's message_end");
    assert_eq!(end["message"]["content"][0]["text"], "the reply");
}

/// Stand-ins for the policy and sandbox crates, which `oxa-rpc` does not depend on: this
/// session runs no tools.
mod policy {
    use oxa_proto::{Access, Decision, Mode, Policy};
    pub struct Deny;
    impl Policy for Deny {
        fn decide(&self, _mode: Mode, _a: &Access, _cwd: &std::path::Path) -> Decision {
            Decision::Deny("no tools in this test")
        }
        fn remember(&mut self, _a: &Access) {}
    }
}

mod sandbox {
    use oxa_proto::{codes, CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
    pub struct NoBwrap;
    impl Sandbox for NoBwrap {
        fn check(&self) -> Result<(), OxaError> {
            Err(OxaError::new(codes::NO_SANDBOX, "no sandbox in this test"))
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
            ExecResult::default()
        }
    }
}
