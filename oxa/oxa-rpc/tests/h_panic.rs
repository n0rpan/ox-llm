//! Wave H lane `harden` (pre-flight A1/A2/B9): a panic inside an RPC command (injected in the
//! engine during `compact`, i.e. outside a prompt run) answers that command with an error; the
//! session thread and the session go on, and the next prompt runs. Passes in debug and release.
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, AssistantTurn, CancelToken, ChatRequest, Decision, Engine, EngineInfo, ExecResult,
    ExecSpec, Mode, OxaError, Policy, Sandbox, StreamEvent,
};
use oxa_rpc::{serve, Out};
use oxa_session::{FsStore, SessionStore};
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

/// No tools run in this test.
struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Deny("no tools in this test")
    }
    fn remember(&mut self, _a: &Access) {}
}

struct NoBwrap;

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

struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_panicking_command_is_an_error_response_and_the_session_goes_on() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let engine = Arc::new(PanicEngine {
        inner: ScriptedEngine::new(vec![ScriptTurn::text("first"), ScriptTurn::text("second")]),
        calls: AtomicUsize::new(0),
        panic_on: 2,
    });
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.agent_dir().join("sessions")));
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        settings: Settings::from_values(json!({"compaction": {"keepRecentTokens": 1}}), json!({})),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    };
    let session = Session::open(deps, Open::New).expect("open");
    let (reader, mut writer) = std::io::pipe().expect("a pipe");
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let out: Out = Arc::new(Mutex::new(Box::new(SharedBuf(stdout.clone()))));
    let dirs = Dirs::under(env.root());
    let server = std::thread::spawn(move || serve(session, dirs, Box::new(BufReader::new(reader)), out));
    let records = |stdout: &Arc<Mutex<Vec<u8>>>| -> Vec<Value> {
        let bytes = stdout.lock().unwrap().clone();
        bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).expect("one JSON record per line"))
            .collect()
    };
    let wait = |what: &str, pred: &dyn Fn(&Value) -> bool| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !records(&stdout).iter().any(pred) {
            assert!(
                std::time::Instant::now() < end,
                "no {what}: {:?}",
                records(&stdout)
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let mut send = |cmd: Value| writeln!(writer, "{cmd}").expect("write a command");
    send(json!({"id": "p1", "type": "prompt", "message": "one"}));
    wait("agent_settled", &|r| r["type"] == "agent_settled");
    send(json!({"id": "c1", "type": "compact"}));
    wait("c1", &|r| r["type"] == "response" && r["id"] == "c1");
    send(json!({"id": "s1", "type": "get_state"}));
    wait("s1", &|r| r["type"] == "response" && r["id"] == "s1");
    send(json!({"id": "p2", "type": "prompt", "message": "two"}));
    wait("the second answer", &|r| {
        r["type"] == "message_end" && r.to_string().contains("second")
    });
    drop(send);
    drop(writer);
    let code = server.join().expect("the server thread").expect("serve returns");
    assert_eq!(code, 0);
    let records = records(&stdout);
    let response = |id: &str| {
        records
            .iter()
            .find(|r| r["type"] == "response" && r["id"] == id)
            .unwrap_or_else(|| panic!("no response to {id}: {records:?}"))
            .clone()
    };
    let c1 = response("c1");
    assert_eq!(c1["success"], false, "{c1}");
    let why = c1["error"].to_string();
    assert!(why.contains("injected engine panic"), "{c1}");
    assert_eq!(response("s1")["data"]["isStreaming"], false);
    let p2 = response("p2");
    assert_eq!(p2["success"], true, "{p2}");
    let second = records
        .iter()
        .any(|r| r["type"] == "message_end" && r.to_string().contains("second"));
    assert!(second, "the prompt after the panic ran: {records:?}");
}
