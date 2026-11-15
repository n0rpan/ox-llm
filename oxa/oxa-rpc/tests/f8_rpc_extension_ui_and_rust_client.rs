//! F8 acceptance tests: RPC extension UI and Rust client. Owner: task F8.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The client tests drive a real `oxa --mode rpc` process. This binary is that process: a test
//! named `rpc_child_entry` serves an RPC session over its own stdin/stdout when the environment
//! names a test root (`OXA_F8_CHILD_ROOT`), and does nothing in a normal run. The client starts it
//! through a small shell script, because `--mode rpc` is not a libtest argument.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, Answer, CancelToken, Decision, Engine, ExecResult, ExecSpec, Level, Mode, OxaError,
    Placement, Policy, Sandbox, Ui, Value,
};
use oxa_rpc::{parse_command, serve, Out, RpcClient, RpcUi, UiRouter};
use oxa_session::{FsStore, SessionStore};
use serde_json::json;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CHILD_ROOT_ENV: &str = "OXA_F8_CHILD_ROOT";

/// A policy for the child session: no tools, so every access is refused.
struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Deny("no tools in this test")
    }
    fn remember(&mut self, _a: &Access) {}
}

/// A sandbox that is never reached: the child session runs no tools.
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

/// A `Write` that appends to a shared buffer.
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

/// An RPC UI writing into a buffer, with the router its answers arrive through.
struct Harness {
    ui: RpcUi,
    router: Arc<UiRouter>,
    stdout: Arc<Mutex<Vec<u8>>>,
}

fn harness() -> Harness {
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let out: Out = Arc::new(Mutex::new(Box::new(SharedBuf(stdout.clone()))));
    let router = Arc::new(UiRouter::new());
    Harness {
        ui: RpcUi {
            out,
            router: router.clone(),
        },
        router,
        stdout,
    }
}

impl Harness {
    /// Every record written so far.
    fn records(&self) -> Vec<Value> {
        let bytes = self.stdout.lock().unwrap().clone();
        bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("every line is one JSON record"))
            .collect()
    }

    /// The first `extension_ui_request` with `method`, waiting up to 10 seconds.
    fn request(&self, method: &str) -> Value {
        let start = Instant::now();
        loop {
            let found = self
                .records()
                .into_iter()
                .find(|r| r["type"] == "extension_ui_request" && r["method"] == method);
            if let Some(r) = found {
                return r;
            }
            if start.elapsed() > Duration::from_secs(10) {
                panic!("no {method} request; stdout so far: {:?}", self.records());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Answer `request` the way a client does: the response line goes through the command
    /// reader (`parse_command` + `deliver`), so the test covers the real routing.
    fn answer(&self, request: &Value, fields: Value) {
        let mut line = json!({"type": "extension_ui_response", "id": request["id"]});
        if let (Value::Object(m), Value::Object(extra)) = (&mut line, fields) {
            m.extend(extra);
        }
        let cmd = parse_command(&line.to_string()).expect("a command line");
        assert!(
            self.router.deliver(&cmd),
            "the dialog waits for {}",
            request["id"]
        );
    }
}

fn keys(v: &Value) -> BTreeSet<&str> {
    v.as_object()
        .expect("a record is an object")
        .keys()
        .map(String::as_str)
        .collect()
}

fn key_set(names: &[&'static str]) -> BTreeSet<&'static str> {
    names.iter().copied().collect()
}

/// Runs a dialog on its own thread (it blocks until answered).
fn dialog<R: Send + 'static>(
    mut ui: RpcUi,
    f: impl FnOnce(&mut RpcUi) -> R + Send + 'static,
) -> JoinHandle<R> {
    thread::spawn(move || f(&mut ui))
}

/// The child session: a scripted engine that answers "Hello from the child".
fn child_session(root: &Path) -> Session {
    let agent = root.join("home/.oxa/agent");
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(agent.join("sessions")));
    let engine: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text(
        "Hello from the child",
    )]));
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        settings: Settings::default(),
        dirs: Dirs::under(root),
        cwd: root.join("work"),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, Open::New).expect("the child session opens over the engine")
}

/// Child mode: serves `oxa --mode rpc` over this process's stdin and stdout. Does nothing in a
/// normal test run.
#[test]
fn rpc_child_entry() {
    let Ok(root) = std::env::var(CHILD_ROOT_ENV) else {
        return;
    };
    let root = PathBuf::from(root);
    // libtest prints "test rpc_child_entry ... " without a newline before this test runs; start
    // the first record on a line of its own.
    {
        let mut out = std::io::stdout();
        out.write_all(b"\n").expect("stdout");
        out.flush().expect("stdout");
    }
    let session = child_session(&root);
    let out: Out = Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let input: Box<dyn BufRead + Send> = Box::new(BufReader::new(std::io::stdin()));
    let code = serve(session, Dirs::under(&root), input, out).expect("serve returns Ok");
    std::process::exit(code);
}

/// Starts this test binary as the `rpc` child of a client. The script and the spawn happen under
/// one lock, so no concurrently forked child keeps the script open for writing (ETXTBSY).
fn spawn_child(env: &TestEnv) -> RpcClient {
    static SPAWN: Mutex<()> = Mutex::new(());
    let _guard = SPAWN.lock().unwrap_or_else(|p| p.into_inner());
    let exe = std::env::current_exe().expect("the test binary");
    let script = env.tmp().join("oxa-rpc-child.sh");
    let body = format!(
        "#!/bin/sh\nexec '{}' --exact rpc_child_entry --test-threads=1\n",
        exe.display()
    );
    std::fs::write(&script, body).expect("write the child script");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let root = env.root().display().to_string();
    let envs = vec![(CHILD_ROOT_ENV.to_string(), root)];
    RpcClient::spawn(&script, &[], &envs, env.work().as_path()).expect("the child starts")
}

/// request/response shapes
#[test]
fn every_record_of_rpc_extension_ui_md() {
    let mut h = harness();

    // select: request shape, timeout included, the answer is an option's index.
    let opts = vec!["Allow".to_string(), "Block".to_string()];
    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let o = opts.clone();
    let t = dialog(ui, move |ui| {
        ui.select("Allow dangerous command?", &o, Some(10000))
    });
    let rec = h.request("select");
    assert_eq!(
        keys(&rec),
        key_set(&["type", "id", "method", "title", "options", "timeout"])
    );
    assert_eq!(rec["title"], "Allow dangerous command?");
    assert_eq!(rec["options"], json!(["Allow", "Block"]));
    assert_eq!(rec["timeout"], 10000);
    h.answer(&rec, json!({"value": "Block"}));
    assert_eq!(t.join().unwrap(), Some(1));

    // confirm: `message` carries the body; the answer is `confirmed`.
    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let t = dialog(ui, |ui| {
        ui.confirm("Clear session?", "All messages will be lost.", Some(5000))
    });
    let rec = h.request("confirm");
    assert_eq!(
        keys(&rec),
        key_set(&["type", "id", "method", "title", "message", "timeout"])
    );
    assert_eq!(rec["message"], "All messages will be lost.");
    h.answer(&rec, json!({"confirmed": true}));
    assert_eq!(t.join().unwrap(), Some(true));

    // input: no timeout field (the trait has none); the answer is `value`.
    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let t = dialog(ui, |ui| ui.input("Enter a value", "type something..."));
    let rec = h.request("input");
    assert_eq!(
        keys(&rec),
        key_set(&["type", "id", "method", "title", "placeholder"])
    );
    assert_eq!(rec["placeholder"], "type something...");
    h.answer(&rec, json!({"value": "hello"}));
    assert_eq!(t.join().unwrap(), Some("hello".to_string()));

    // editor: `prefill`, answered with `value`.
    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let t = dialog(ui, |ui| ui.editor("Edit some text", "Line 1\nLine 2\nLine 3"));
    let rec = h.request("editor");
    assert_eq!(keys(&rec), key_set(&["type", "id", "method", "title", "prefill"]));
    assert_eq!(rec["prefill"], "Line 1\nLine 2\nLine 3");
    h.answer(&rec, json!({"value": "Line 1 edited"}));
    assert_eq!(t.join().unwrap(), Some("Line 1 edited".to_string()));

    // The fire-and-forget records: `notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`.
    h.ui.notify("Command blocked by user", Level::Warning);
    let rec = h.request("notify");
    assert_eq!(
        keys(&rec),
        key_set(&["type", "id", "method", "message", "notifyType"])
    );
    assert_eq!(rec["notifyType"], "warning");

    h.ui.set_status("my-ext", Some("Turn 3 running..."));
    let rec = h.request("setStatus");
    assert_eq!(
        keys(&rec),
        key_set(&["type", "id", "method", "statusKey", "statusText"])
    );
    assert_eq!(rec["statusText"], "Turn 3 running...");

    let lines = vec!["--- My Widget ---".to_string(), "Line 1".to_string()];
    h.ui.set_widget("my-ext", Some(&lines), Placement::AboveEditor);
    let rec = h
        .records()
        .into_iter()
        .find(|r| r["method"] == "setWidget")
        .unwrap();
    assert_eq!(
        keys(&rec),
        key_set(&[
            "type",
            "id",
            "method",
            "widgetKey",
            "widgetLines",
            "widgetPlacement"
        ])
    );
    assert_eq!(rec["widgetLines"], json!(lines));
    assert_eq!(rec["widgetPlacement"], "aboveEditor");

    h.ui.set_title("pi - my project");
    let rec = h.request("setTitle");
    assert_eq!(keys(&rec), key_set(&["type", "id", "method", "title"]));

    h.ui.set_editor_text("prefilled text for the user");
    let rec = h.request("set_editor_text");
    assert_eq!(keys(&rec), key_set(&["type", "id", "method", "text"]));
    assert_eq!(rec["text"], "prefilled text for the user");

    // Every request record carries a unique id.
    let ids: BTreeSet<String> = h
        .records()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), h.records().len());
}

/// Answer mapping
#[test]
fn permission_ask_relayed_as_confirm() {
    let h = harness();
    let access = Access::Write(PathBuf::from("/work/notes.md"));
    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let t = dialog(ui, move |ui| ui.ask_access(&access));
    let rec = h.request("confirm");
    let title = rec["title"].as_str().unwrap();
    assert!(
        title.contains("write /work/notes.md"),
        "the title names the access: {title}"
    );
    h.answer(&rec, json!({"confirmed": true}));
    assert!(matches!(t.join().unwrap(), Answer::Once));

    // "no" and "cancelled" both deny.
    for reply in [json!({"confirmed": false}), json!({"cancelled": true})] {
        let ui = RpcUi {
            out: h.ui.out.clone(),
            router: h.router.clone(),
        };
        // Count before the dialog thread starts: its record may be written at once.
        let before = h.records().len();
        let t = dialog(ui, |ui| {
            ui.ask_access(&Access::Net {
                host: "example.org".into(),
            })
        });
        let rec = loop {
            let all = h.records();
            if let Some(r) = all[before..].iter().find(|r| r["method"] == "confirm") {
                break r.clone();
            }
            thread::sleep(Duration::from_millis(2));
        };
        assert!(rec["title"].as_str().unwrap().contains("example.org"));
        h.answer(&rec, reply);
        assert!(matches!(t.join().unwrap(), Answer::Deny));
    }
}

/// no hang
#[test]
fn dialog_timeout_returns_none() {
    let h = harness();
    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let start = Instant::now();
    let t = dialog(ui, |ui| {
        ui.confirm("Clear session?", "All messages will be lost.", Some(50))
    });
    let rec = h.request("confirm");
    assert_eq!(rec["timeout"], 50);
    assert_eq!(t.join().unwrap(), None);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "the dialog gave up on its own"
    );

    // A late answer is not delivered to anything, and a timed-out ask denies.
    let late = parse_command(
        &json!({"type": "extension_ui_response", "id": rec["id"], "confirmed": true}).to_string(),
    )
    .expect("a command line");
    assert!(!h.router.deliver(&late));
}

/// spawn, prompt, events, close
#[test]
fn client_drives_rpc_client_example_flow() {
    let env = TestEnv::new();
    let mut client = spawn_child(&env);

    let state = client
        .request(json!({"type": "get_state"}), 20_000)
        .expect("get_state answers");
    assert_eq!(state["success"], true);
    assert_eq!(state["data"]["isStreaming"], false);

    let resp = client
        .request(json!({"type": "prompt", "message": "Hi"}), 20_000)
        .expect("prompt is accepted");
    assert_eq!(resp["data"], json!({"disposition": "started"}));

    let events = client.wait_for("agent_settled", 20_000).expect("the run settles");
    assert!(events.iter().any(|e| e["type"] == "agent_start"), "a run starts");
    assert_eq!(events.last().unwrap()["type"], "agent_settled");

    let code = client.close().expect("close waits for the child");
    assert_eq!(code, 0);
}

/// success false
#[test]
fn client_error_response_is_err() {
    let env = TestEnv::new();
    let mut client = spawn_child(&env);

    let err = client
        .request(json!({"type": "no_such_command"}), 20_000)
        .expect_err("an unknown command is an error response");
    assert!(
        err.message.contains("Unknown command: no_such_command"),
        "{}",
        err.message
    );

    // The client is still usable after an error response.
    let ok = client
        .request(json!({"type": "get_state"}), 20_000)
        .expect("get_state answers");
    assert_eq!(ok["success"], true);
    assert_eq!(client.close().expect("close"), 0);
}

/// notify, setStatus, setWidget, setTitle
#[test]
fn fire_and_forget_methods() {
    let h = harness();
    // Each call returns at once: none of them waits for an answer. A call that waited would
    // never return here, so the whole sequence runs on a thread with a deadline.
    let (done_tx, done_rx) = mpsc::channel();
    let mut ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    thread::spawn(move || {
        ui.notify("Command blocked by user", Level::Error);
        ui.set_status("my-ext", Some("working"));
        ui.set_status("my-ext", None);
        ui.set_widget("my-ext", Some(&["a".to_string()]), Placement::BelowEditor);
        ui.set_widget("my-ext", None, Placement::AboveEditor);
        ui.set_title("pi - my project");
        ui.set_editor_text("text");
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("fire-and-forget calls do not wait for an answer");

    let recs = h.records();
    let methods: Vec<&str> = recs.iter().map(|r| r["method"].as_str().unwrap()).collect();
    assert_eq!(
        methods,
        vec![
            "notify",
            "setStatus",
            "setStatus",
            "setWidget",
            "setWidget",
            "setTitle",
            "set_editor_text"
        ]
    );
    assert_eq!(recs[0]["notifyType"], "error");
    assert!(
        recs[2].get("statusText").is_none(),
        "a cleared status has no statusText"
    );
    assert_eq!(recs[3]["widgetPlacement"], "belowEditor");
    assert!(
        recs[4].get("widgetLines").is_none(),
        "a cleared widget has no widgetLines"
    );
    assert_eq!(recs[4]["widgetPlacement"], "aboveEditor");
}

/// Extra: answers that are not an option or a confirmation (cancel, unknown option, wrong kind).
#[test]
fn cancelled_and_unmatched_answers_are_no_answer() {
    let h = harness();
    let opts = vec!["Allow".to_string()];
    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let o = opts.clone();
    let t = dialog(ui, move |ui| ui.select("Pick", &o, None));
    let rec = h.request("select");
    h.answer(&rec, json!({"value": "Not an option"}));
    assert_eq!(t.join().unwrap(), None);

    let ui = RpcUi {
        out: h.ui.out.clone(),
        router: h.router.clone(),
    };
    let t = dialog(ui, |ui| ui.input("Name", ""));
    let rec = h.request("input");
    h.answer(&rec, json!({"cancelled": true}));
    assert_eq!(t.join().unwrap(), None);

    // Only extension_ui_response commands are answers, and only for a waiting id.
    let not_an_answer = parse_command(r#"{"type":"prompt","id":"x","message":"hi"}"#).expect("a command");
    assert!(!h.router.deliver(&not_an_answer));
    let unknown = parse_command(r#"{"type":"extension_ui_response","id":"ui-unknown","value":"x"}"#)
        .expect("a command");
    assert!(!h.router.deliver(&unknown));
}

/// Extra: respond_ui and a second request against a real child.
#[test]
fn client_respond_ui_and_timeouts_against_a_child() {
    let env = TestEnv::new();
    let mut client = spawn_child(&env);
    // An answer for a dialog that does not exist is written and ignored by the child.
    client
        .respond_ui("ui-nobody", json!({"cancelled": true}))
        .expect("the answer is written");
    // The next request is answered; the child's responses are matched by id.
    let ok = client
        .request(json!({"type": "get_state"}), 20_000)
        .expect("get_state answers");
    assert_eq!(ok["success"], true);
    assert_eq!(client.close().expect("close"), 0);
}
