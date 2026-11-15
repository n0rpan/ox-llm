//! Wave H lane `harden` (pre-flight A1/A2/B9): a panic inside a worker command (injected in the
//! engine during `/compact`, outside a prompt run) is reported as that command's error; the
//! worker thread and the session go on, and the next prompt runs. Passes in debug and release.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_interactive::{worker_loop_with, ReopenFn, SessionCmd, WorkerMsg, WorkerStop};
use oxa_proto::{codes, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent};
use oxa_resources::Resources;
use oxa_session::{FsStore, SessionStore};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// The scripted engine, except that the `panic_on`-th chat request (1-based) panics.
struct PanicEngine {
    inner: ScriptedEngine,
    calls: AtomicUsize,
    panic_on: usize,
}

impl Engine for PanicEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.panic_on {
            panic!("injected engine panic");
        }
        self.inner.chat(r, cancel, on)
    }
}

/// The result of the next `Done`.
fn done(msgs: &mpsc::Receiver<WorkerMsg>) -> Result<(), OxaError> {
    loop {
        match msgs.recv_timeout(Duration::from_secs(20)) {
            Ok(WorkerMsg::Done(r)) => return r,
            Ok(_) => {}
            Err(e) => panic!("the worker answered nothing: {e}"),
        }
    }
}

#[test]
fn a_panicking_command_is_its_error_and_the_worker_goes_on() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let engine = Arc::new(PanicEngine {
        inner: ScriptedEngine::new(vec![ScriptTurn::text("first"), ScriptTurn::text("second")]),
        calls: AtomicUsize::new(0),
        panic_on: 2,
    });
    let session = Session::open(
        Deps {
            engine,
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(AllowAll),
            sandbox: Arc::new(NoBash),
            settings: Settings::from_values(json!({"compaction": {"keepRecentTokens": 1}}), json!({})),
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
    let handle = session.handle();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (ui_tx, _ui_rx) = mpsc::channel();
    let reopen: ReopenFn = Box::new(|_, _| Err(OxaError::new("invalid", "no reopen in this test")));
    let worker = std::thread::spawn(move || {
        worker_loop_with(session, reopen, cmd_rx, msg_tx, ui_tx, WorkerStop::new())
    });

    let prompt = |text: &str| SessionCmd::Prompt {
        text: text.to_string(),
        parts: Vec::new(),
    };
    cmd_tx.send(prompt("one")).unwrap();
    done(&msg_rx).expect("the first prompt runs");
    cmd_tx.send(SessionCmd::Compact { instructions: None }).unwrap();
    let e = done(&msg_rx).expect_err("the compaction reports the panic");
    assert_eq!(e.code, codes::INTERNAL);
    assert!(e.message.contains("injected engine panic"), "{e}");
    assert!(!handle.is_busy(), "the session is not left busy");
    cmd_tx.send(prompt("two")).unwrap();
    done(&msg_rx).expect("the prompt after the panic runs");
    cmd_tx.send(SessionCmd::Shutdown).unwrap();
    assert!(worker.join().expect("the worker thread did not die").is_some());
}
