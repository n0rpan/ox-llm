//! V020-OPTIMIZE #13 (`oxa.prefill.paste`), the interactive trigger: the worker carries out
//! `SessionCmd::PrefillDraft` invisibly (no `Done`), skips one whose draft generation is stale,
//! and the prompt sent with the same draft finds it cached.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_interactive::{worker_loop_with, ReopenFn, SessionCmd, WorkerMsg, WorkerStop};
use oxa_proto::{AgentEvent, OxaError, WireMsg};
use oxa_resources::Resources;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

struct AllowAll;

impl oxa_proto::Policy for AllowAll {
    fn decide(
        &self,
        _m: oxa_proto::Mode,
        _a: &oxa_proto::Access,
        _cwd: &std::path::Path,
    ) -> oxa_proto::Decision {
        oxa_proto::Decision::Allow
    }
    fn remember(&mut self, _a: &oxa_proto::Access) {}
}

struct NoBash;

impl oxa_proto::Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &oxa_proto::ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(
        &self,
        _s: &oxa_proto::ExecSpec,
        _c: &oxa_proto::CancelToken,
        _live: &mut dyn FnMut(&[u8]),
    ) -> oxa_proto::ExecResult {
        oxa_proto::ExecResult::default()
    }
}

#[test]
fn worker_prefills_the_current_draft_only() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let session = Session::open(
        Deps {
            engine: eng.clone(),
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(AllowAll),
            sandbox: Arc::new(NoBash),
            settings: Settings::from_values(
                json!({"oxa": {"envNote": false, "prefill": {"paste": true}}}),
                json!({}),
            ),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
        },
        Open::New,
    )
    .expect("open");
    let stop = WorkerStop::new();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (ui_tx, _ui_rx) = mpsc::channel();
    let reopen: ReopenFn = Box::new(|_, _| Err(OxaError::new("invalid", "no reopen in this test")));
    let worker_stop = stop.clone();
    let worker =
        std::thread::spawn(move || worker_loop_with(session, reopen, cmd_rx, msg_tx, ui_tx, worker_stop));

    let draft = "fn main() { println!(\"pasted\"); }\n".repeat(80);
    // Taken at an old generation: the user typed since, so it is skipped.
    let old = stop.draft_gen();
    assert!(!stop.draft_changed(), "no prefill runs yet");
    cmd_tx
        .send(SessionCmd::PrefillDraft {
            text: draft.clone(),
            parts: Vec::new(),
            gen: old,
        })
        .unwrap();
    // The current generation: prefilled.
    cmd_tx
        .send(SessionCmd::PrefillDraft {
            text: draft.clone(),
            parts: Vec::new(),
            gen: stop.draft_gen(),
        })
        .unwrap();
    cmd_tx
        .send(SessionCmd::Prompt {
            text: draft.clone(),
            parts: Vec::new(),
        })
        .unwrap();
    // The first `Done` is the prompt's: the draft prefills answer nothing.
    let mut misses = 0;
    loop {
        match msg_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(WorkerMsg::Done(r)) => {
                r.expect("the prompt runs");
                break;
            }
            Ok(WorkerMsg::Event(AgentEvent::CacheMiss { .. })) => misses += 1,
            Ok(WorkerMsg::Event(AgentEvent::Prefill { .. })) => panic!("draft prefill progress is not shown"),
            Ok(_) => {}
            Err(e) => panic!("the worker answered nothing: {e}"),
        }
    }
    cmd_tx.send(SessionCmd::Shutdown).unwrap();
    worker.join().expect("worker");

    let reqs = eng.requests();
    let user = WireMsg::user(draft.clone());
    let drafts: Vec<_> = reqs
        .iter()
        .filter(|r| r.max_tokens == 0 && r.messages.last() == Some(&user))
        .collect();
    assert_eq!(drafts.len(), 1, "one prefill: the stale one was skipped");
    let chat = reqs
        .iter()
        .find(|r| r.kind == "chat")
        .expect("the prompt's request");
    assert_eq!(chat.messages, drafts[0].messages);
    assert!(chat.usage.unwrap().cached >= drafts[0].usage.unwrap().prompt);
    assert_eq!(misses, 0);
}
