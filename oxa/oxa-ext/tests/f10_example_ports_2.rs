//! F10 acceptance tests: Example ports 2.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! Each test boots the port from `examples/extensions/<name>/` (the real file, through
//! `ExtensionHost::load`) on an in-process session double, drives it with the events and commands
//! of a scripted session, and checks what reached the session and the UI. The port's
//! `session.fake.json` must parse as an `oxa-fake` script. The full `oxa --mode json -e` run against
//! the fake engine belongs to the parity suite (G12), not to this crate.

use oxa_ext::{ExtensionHost, LoadOpts, LoadedExtension};
use oxa_fake::{parse_script, TestEnv};
use oxa_proto::{
    Access, Answer, CancelToken, ComponentAction, CustomOpts, ExecResult, ExecSpec, HookPoint, HookReply,
    Hooks, Host, HostAction, KeyId, Level, Mode, OxaError, Placement, Sandbox, ToolCtx, Ui, UiComponent,
    Value,
};
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// `examples/extensions` of this repository.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/extensions")
        .canonicalize()
        .expect("examples/extensions exists")
}

/// The port's scripted session parses as an oxa-fake script with at least one turn.
fn assert_script(name: &str) {
    let path = examples_dir().join(name).join("session.fake.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let turns = parse_script(&text).unwrap_or_else(|e| panic!("{name} script: {}", e.message));
    assert!(!turns.is_empty(), "{name}: the script has no turns");
}

/// Load the port `name` on a fresh runtime attached to `session`. The load must succeed.
fn boot(name: &str, session: Arc<Session>) -> (TestEnv, ExtensionHost, LoadedExtension) {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("start the extension runtime");
    host.attach(session);
    let loaded = host.load(&[examples_dir().join(name)], &LoadOpts::default());
    assert_eq!(loaded.len(), 1, "{name}: one extension expected: {loaded:?}");
    let ext = loaded[0].clone();
    assert!(ext.error.is_none(), "{name} must load: {:?}", ext.error);
    assert_script(name);
    (env, host, ext)
}

/// A tool entry as `getAllTools` answers it.
fn tool_entry(name: &str) -> Value {
    json!({"name": name, "description": format!("the {name} tool"), "source": "builtin", "exposure": "direct"})
}

/// The session double: branch entries, active tools, and a record of every action the port took.
struct Session {
    branch: Mutex<Vec<Value>>,
    all_tools: Vec<Value>,
    active: Mutex<Vec<String>>,
    set_active_calls: Mutex<Vec<Vec<String>>>,
    appended: Mutex<Vec<(String, Value)>>,
    execs: Mutex<Vec<String>>,
    exec_outputs: Mutex<VecDeque<String>>,
    usage: Mutex<Option<u64>>,
    compacts: Mutex<Vec<Option<String>>>,
    /// What `oxa.getCommands()` answers.
    commands: Mutex<Value>,
    idle: bool,
}

impl Session {
    fn new(all: &[&str], active: &[&str]) -> Arc<Session> {
        Arc::new(Session {
            branch: Mutex::new(Vec::new()),
            all_tools: all.iter().map(|n| tool_entry(n)).collect(),
            active: Mutex::new(active.iter().map(|s| s.to_string()).collect()),
            set_active_calls: Mutex::new(Vec::new()),
            appended: Mutex::new(Vec::new()),
            execs: Mutex::new(Vec::new()),
            exec_outputs: Mutex::new(VecDeque::new()),
            usage: Mutex::new(None),
            compacts: Mutex::new(Vec::new()),
            commands: Mutex::new(json!([])),
            idle: true,
        })
    }

    /// A session with a branch of one entry per id, the last one being the leaf.
    fn with_branch(ids: &[&str]) -> Arc<Session> {
        let s = Session::new(&[], &[]);
        *s.branch.lock().unwrap() = ids
            .iter()
            .map(|id| json!({"id": id, "type": "message"}))
            .collect();
        s
    }
}

impl Host for Session {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        Ok(match action {
            HostAction::GetBranch => Value::Array(self.branch.lock().unwrap().clone()),
            HostAction::GetAllTools => Value::Array(self.all_tools.clone()),
            HostAction::GetActiveTools => json!(self.active.lock().unwrap().clone()),
            HostAction::SetActiveTools(names) => {
                self.set_active_calls.lock().unwrap().push(names.clone());
                *self.active.lock().unwrap() = names;
                Value::Null
            }
            HostAction::AppendEntry { custom_type, data } => {
                // The core puts the entry on the branch at once; the port reads it back on a tree move.
                let mut branch = self.branch.lock().unwrap();
                let id = format!("entry{}", branch.len() + 1);
                branch.push(json!({"id": id, "type": "custom", "customType": custom_type, "data": data}));
                self.appended.lock().unwrap().push((custom_type, data));
                Value::Null
            }
            HostAction::Exec(ExecSpec { command, .. }) => {
                self.execs.lock().unwrap().push(command);
                let output = self.exec_outputs.lock().unwrap().pop_front().unwrap_or_default();
                json!({"output": output, "exit": 0, "timedOut": false})
            }
            HostAction::ContextUsage => match *self.usage.lock().unwrap() {
                Some(tokens) => json!({"tokens": tokens, "contextWindow": 200000, "percent": 0}),
                None => Value::Null,
            },
            HostAction::Compact { instructions } => {
                self.compacts.lock().unwrap().push(instructions);
                Value::Null
            }
            HostAction::IsIdle => Value::Bool(self.idle),
            HostAction::GetCommands => self.commands.lock().unwrap().clone(),
            _ => Value::Null,
        })
    }
}

/// A UI that answers from queues and records what the port showed.
#[derive(Default)]
struct ScriptUi {
    selects: VecDeque<Option<usize>>,
    confirms: VecDeque<Option<bool>>,
    /// Keys fed to a custom component, one `handleInput` each (single characters carry their text).
    keys: VecDeque<String>,
    interactive: bool,
    notes: Vec<(Level, String)>,
    select_log: Vec<(String, Vec<String>)>,
    custom_first_render: Vec<Vec<String>>,
}

impl ScriptUi {
    fn person() -> ScriptUi {
        ScriptUi {
            interactive: true,
            ..ScriptUi::default()
        }
    }

    fn queue_keys(&mut self, keys: &[&str]) {
        self.keys.extend(keys.iter().map(|k| k.to_string()));
    }

    fn noted(&self, level: Level, needle: &str) -> bool {
        self.notes.iter().any(|(l, t)| *l == level && t.contains(needle))
    }
}

impl Ui for ScriptUi {
    fn select(&mut self, title: &str, options: &[String], _timeout_ms: Option<u32>) -> Option<usize> {
        self.select_log.push((title.to_string(), options.to_vec()));
        self.selects.pop_front().unwrap_or(None)
    }
    fn confirm(&mut self, _title: &str, _body: &str, _timeout_ms: Option<u32>) -> Option<bool> {
        self.confirms.pop_front().unwrap_or(None)
    }
    fn input(&mut self, _title: &str, _placeholder: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _title: &str, _prefill: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, text: &str, level: Level) {
        self.notes.push((level, text.to_string()));
    }
    fn set_status(&mut self, _key: &str, _text: Option<&str>) {}
    fn set_widget(&mut self, _key: &str, _lines: Option<&[String]>, _at: Placement) {}
    fn set_title(&mut self, _t: &str) {}
    fn set_editor_text(&mut self, _t: &str) {}
    fn ask_access(&mut self, _a: &Access) -> Answer {
        Answer::Deny
    }
    fn interactive(&self) -> bool {
        self.interactive
    }
    /// Renders once, then feeds the queued keys until the component closes.
    fn custom(&mut self, mut c: Box<dyn UiComponent>, _opts: CustomOpts) -> Option<Value> {
        self.custom_first_render.push(c.render(80));
        while let Some(k) = self.keys.pop_front() {
            let key = KeyId::parse(&k).expect("a valid key id");
            let text = if k.chars().count() == 1 {
                Some(k.as_str())
            } else {
                None
            };
            if let ComponentAction::Done(v) = c.input(&key, text) {
                return Some(v);
            }
        }
        None
    }
}

/// The access-free tool context of the tests.
struct NoSandbox;

impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

fn no_errors(host: &ExtensionHost) {
    let errors = host.state().take_errors();
    assert!(errors.is_empty(), "no extension error expected: {errors:?}");
}

// ---------------------------------------------------------------------------------------------
// tools: /tools toggles the active tools; the selection follows the session branch
// ---------------------------------------------------------------------------------------------

/// port
#[test]
fn tools_loads_and_passes_scripted_session() {
    let session = Session::new(&["read", "write", "edit", "bash"], &["read", "write", "edit"]);
    let (_env, host, _ext) = boot("tools", session.clone());
    let mut ui = ScriptUi::person();

    // Session start: nothing saved yet, so the selection is the active tools.
    host.fire(HookPoint::SessionStart, &Value::Null, &mut ui);
    assert!(session.set_active_calls.lock().unwrap().is_empty());

    // /tools: write off, bash on, close.
    ui.queue_keys(&["down", "enter", "down", "down", "enter", "escape"]);
    host.run_command("tools", "", &mut ui)
        .expect("the /tools command runs");
    assert_eq!(
        *session.set_active_calls.lock().unwrap(),
        vec![
            vec!["read".to_string(), "edit".to_string()],
            vec!["read".to_string(), "edit".to_string(), "bash".to_string()],
        ]
    );
    let shown = &ui.custom_first_render[0];
    assert!(
        shown.iter().any(|l| l.contains("Tool Configuration")),
        "{shown:?}"
    );
    // pi's SettingsList row: the cursor, the padded label, the value.
    assert!(
        shown
            .iter()
            .any(|l| l.starts_with("→ read") && l.ends_with("enabled")),
        "{shown:?}"
    );
    let appended = session.appended.lock().unwrap().clone();
    assert_eq!(
        appended.len(),
        2,
        "one tools-config entry per change: {appended:?}"
    );
    assert_eq!(appended[1].0, "tools-config");
    assert_eq!(appended[1].1, json!({"enabledTools": ["read", "edit", "bash"]}));

    // A tree move back to a branch whose saved selection names a tool that no longer exists:
    // the saved names that exist are restored, the rest is dropped.
    session.branch.lock().unwrap().clear();
    session
        .branch
        .lock()
        .unwrap()
        .push(json!({"id": "t1", "type": "message"}));
    session.branch.lock().unwrap().push(json!({
        "id": "t2", "type": "custom", "customType": "tools-config",
        "data": {"enabledTools": ["read", "write", "gone"]}
    }));
    host.fire(HookPoint::SessionTree, &Value::Null, &mut ui);
    assert_eq!(
        session.set_active_calls.lock().unwrap().last().cloned(),
        Some(vec!["read".to_string(), "write".to_string()])
    );
    no_errors(&host);
}

// ---------------------------------------------------------------------------------------------
// commands: /commands lists the slash commands
// ---------------------------------------------------------------------------------------------

/// port
#[test]
fn commands_loads_and_passes_scripted_session() {
    let session = Session::new(&[], &[]);
    *session.commands.lock().unwrap() = json!([
        {"name": "commands", "description": "List available slash commands", "source": "extension",
         "sourceInfo": {"path": "/x/commands/index.js", "source": "local", "scope": "user", "origin": "top-level"}},
        {"name": "review", "description": "Review a diff", "source": "prompt",
         "sourceInfo": {"path": "/x/prompts/review.md", "source": "local", "scope": "user", "origin": "top-level"}},
        {"name": "skill:pdf", "source": "skill",
         "sourceInfo": {"path": "/x/skills/pdf/SKILL.md", "source": "local", "scope": "user", "origin": "top-level", "baseDir": "/x/skills/pdf"}}
    ]);
    let (_env, host, _ext) = boot("commands", session.clone());
    let mut ui = ScriptUi::person();

    assert!(
        host.commands().iter().any(|c| c.name == "commands"),
        "the /commands command is registered"
    );

    // /commands: the list grouped by source; picking the prompt and confirming shows its path.
    ui.selects.push_back(Some(3));
    ui.confirms.push_back(Some(true));
    host.run_command("commands", "", &mut ui)
        .expect("the command runs");
    assert_eq!(
        ui.select_log[0].1,
        vec![
            "--- Extensions ---",
            "/commands - List available slash commands",
            "--- Prompts ---",
            "/review - Review a diff",
            "--- Skills ---",
            "/skill:pdf",
        ]
    );
    assert!(ui.noted(Level::Info, "/x/prompts/review.md"), "{:?}", ui.notes);

    // A filter by source; a header picked shows nothing more.
    ui.selects.push_back(Some(0));
    host.run_command("commands", "skill", &mut ui)
        .expect("the command runs with a filter");
    assert_eq!(ui.select_log[1].1, vec!["--- Skills ---", "/skill:pdf"]);
    assert_eq!(ui.notes.len(), 1, "{:?}", ui.notes);

    // No match.
    host.run_command("commands", "nothing", &mut ui).expect("runs");
    assert!(
        ui.noted(Level::Info, "No nothing commands found"),
        "{:?}",
        ui.notes
    );
    no_errors(&host);
}

// ---------------------------------------------------------------------------------------------
// input-transform: ?quick, ping and time are handled on the `input` event
// ---------------------------------------------------------------------------------------------

fn input(host: &ExtensionHost, ui: &mut ScriptUi, text: &str, source: &str) -> HookReply {
    host.fire(HookPoint::Input, &json!({"text": text, "source": source}), ui)
}

/// port
#[test]
fn input_transform_loads_and_passes_scripted_session() {
    let session = Session::new(&[], &[]);
    let (_env, host, _ext) = boot("input-transform", session);
    let mut ui = ScriptUi::person();

    assert_eq!(
        input(&host, &mut ui, "?quick What is TypeScript?", "interactive"),
        HookReply::ReplaceInput("Respond briefly in 1-2 sentences: What is TypeScript?".into())
    );
    assert_eq!(input(&host, &mut ui, "ping", "interactive"), HookReply::Handled);
    assert!(ui.noted(Level::Info, "pong"), "{:?}", ui.notes);
    assert_eq!(input(&host, &mut ui, "TIME", "interactive"), HookReply::Handled);
    let stamp = ui.notes.last().map(|(_, t)| t.clone()).unwrap_or_default();
    assert!(stamp.len() >= 19 && stamp.contains('T'), "a timestamp: {stamp}");
    assert_eq!(
        input(&host, &mut ui, "?quick   ", "interactive"),
        HookReply::Handled
    );
    assert!(ui.noted(Level::Warning, "Usage: ?quick"), "{:?}", ui.notes);

    // Extension-injected messages pass through untouched, and so does everything else.
    let before = ui.notes.len();
    assert_eq!(input(&host, &mut ui, "ping", "extension"), HookReply::Continue);
    assert_eq!(ui.notes.len(), before);
    assert_eq!(
        input(&host, &mut ui, "write a haiku", "interactive"),
        HookReply::Continue
    );
    no_errors(&host);
}

// ---------------------------------------------------------------------------------------------
// question: a tool that asks the user one question with options
// ---------------------------------------------------------------------------------------------

/// Run the `question` tool of the port with `args`, UI `ui`.
fn ask(host: &ExtensionHost, env: &TestEnv, ui: &mut ScriptUi, args: Value) -> (String, Value) {
    let tools = host.tools();
    let tool = tools
        .iter()
        .find(|t| t.spec().name == "question")
        .expect("the question tool is registered");
    let cancel = CancelToken::new();
    let sandbox = NoSandbox;
    let cwd = env.work();
    let spill = env.tmp();
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    let out = tool.run_with_ui(&args, &cx, &mut |_: &str| {}, ui);
    (out.text, out.ui)
}

fn question_args() -> Value {
    json!({
        "question": "Which database?",
        "options": [
            {"label": "SQLite"},
            {"label": "Postgres", "description": "needs a server"}
        ]
    })
}

/// port
#[test]
fn question_loads_and_passes_scripted_session() {
    let session = Session::new(&[], &[]);
    let (env, host, _ext) = boot("question", session);

    // The option list is a tool with a JSON Schema, not a typebox value.
    let tools = host.tools();
    let schema_json = tools
        .iter()
        .find(|t| t.spec().name == "question")
        .expect("registered")
        .spec()
        .schema_json
        .clone();
    assert!(schema_json.contains("\"options\""), "{schema_json}");

    // Pick option 1.
    let mut ui = ScriptUi::person();
    ui.queue_keys(&["enter"]);
    let (text, details) = ask(&host, &env, &mut ui, question_args());
    assert_eq!(text, "User selected: 1. SQLite");
    assert_eq!(details["answer"], json!("SQLite"));
    assert_eq!(details["wasCustom"], json!(false));
    assert!(ui.custom_first_render[0]
        .iter()
        .any(|l| l.contains("Which database?")));

    // Type something: down to the last row, enter, type "xy", enter.
    let mut ui = ScriptUi::person();
    ui.queue_keys(&["down", "down", "enter", "x", "y", "enter"]);
    let (text, details) = ask(&host, &env, &mut ui, question_args());
    assert_eq!(text, "User wrote: xy");
    assert_eq!(details["wasCustom"], json!(true));

    // Escape in the options cancels.
    let mut ui = ScriptUi::person();
    ui.queue_keys(&["escape"]);
    let (text, details) = ask(&host, &env, &mut ui, question_args());
    assert_eq!(text, "User cancelled the selection");
    assert_eq!(details["answer"], Value::Null);

    // Without a person to answer, the tool says so and asks nothing.
    let mut ui = ScriptUi::default();
    let (text, _) = ask(&host, &env, &mut ui, question_args());
    assert_eq!(text, "Error: UI not available (running in non-interactive mode)");
    assert!(ui.custom_first_render.is_empty());

    // No options is an error answer.
    let mut ui = ScriptUi::person();
    let (text, _) = ask(&host, &env, &mut ui, json!({"question": "Empty?", "options": []}));
    assert_eq!(text, "Error: No options provided");
    no_errors(&host);
}

// ---------------------------------------------------------------------------------------------
// git-checkpoint: a stash snapshot per turn; a fork offers to restore it
// ---------------------------------------------------------------------------------------------

/// port
#[test]
fn git_checkpoint_loads_and_passes_scripted_session() {
    let session = Session::with_branch(&["e0", "e1"]);
    let (_env, host, _ext) = boot("git-checkpoint", session.clone());
    let mut ui = ScriptUi::person();

    // A tool result sets the leaf (e1); the next turn snapshots the tree under it.
    let tool_result =
        json!({"toolCallId": "c1", "toolName": "write", "input": {}, "content": "ok", "isError": false});
    host.fire(HookPoint::ToolResult, &tool_result, &mut ui);
    session.exec_outputs.lock().unwrap().push_back("abc123\n".into());
    host.fire(HookPoint::TurnStart, &Value::Null, &mut ui);
    assert_eq!(
        *session.execs.lock().unwrap(),
        vec!["git 'stash' 'create'".to_string()]
    );

    // A fork from e1 asks; yes applies the snapshot.
    ui.selects.push_back(Some(0));
    host.fire(HookPoint::SessionBeforeFork, &json!({"entryId": "e1"}), &mut ui);
    assert_eq!(ui.select_log[0].0, "Restore code state?");
    assert_eq!(
        session.execs.lock().unwrap().last().cloned(),
        Some("git 'stash' 'apply' 'abc123'".to_string())
    );
    assert!(
        ui.noted(Level::Info, "Code restored to checkpoint"),
        "{:?}",
        ui.notes
    );

    // No: the code stays as it is.
    ui.selects.push_back(Some(1));
    let runs = session.execs.lock().unwrap().len();
    host.fire(HookPoint::SessionBeforeFork, &json!({"entryId": "e1"}), &mut ui);
    assert_eq!(session.execs.lock().unwrap().len(), runs);

    // A fork from an entry with no checkpoint asks nothing.
    let asked = ui.select_log.len();
    host.fire(HookPoint::SessionBeforeFork, &json!({"entryId": "e0"}), &mut ui);
    assert_eq!(ui.select_log.len(), asked);

    // Without a UI the fork is never restored automatically.
    let mut headless = ScriptUi::default();
    host.fire(
        HookPoint::SessionBeforeFork,
        &json!({"entryId": "e1"}),
        &mut headless,
    );
    assert!(headless.select_log.is_empty());

    // The checkpoints belong to one agent run: after it settles, nothing is offered.
    host.fire(HookPoint::AgentSettled, &Value::Null, &mut ui);
    let asked = ui.select_log.len();
    host.fire(HookPoint::SessionBeforeFork, &json!({"entryId": "e1"}), &mut ui);
    assert_eq!(ui.select_log.len(), asked);
    no_errors(&host);
}

// ---------------------------------------------------------------------------------------------
// trigger-compact: compacts when the context crosses 100k tokens; /trigger-compact on demand
// ---------------------------------------------------------------------------------------------

/// port
#[test]
fn trigger_compact_loads_and_passes_scripted_session() {
    let session = Session::new(&[], &[]);
    let (_env, host, _ext) = boot("trigger-compact", session.clone());
    let mut ui = ScriptUi::person();

    // No usage yet: nothing happens.
    host.fire(HookPoint::TurnEnd, &Value::Null, &mut ui);
    assert!(session.compacts.lock().unwrap().is_empty());

    // Below the threshold, then across it: one compaction, announced.
    *session.usage.lock().unwrap() = Some(90_000);
    host.fire(HookPoint::TurnEnd, &Value::Null, &mut ui);
    *session.usage.lock().unwrap() = Some(120_000);
    host.fire(HookPoint::TurnEnd, &Value::Null, &mut ui);
    assert_eq!(*session.compacts.lock().unwrap(), vec![None]);
    assert!(ui.noted(Level::Info, "Compaction started"), "{:?}", ui.notes);

    // Still above the threshold: no second compaction.
    *session.usage.lock().unwrap() = Some(130_000);
    host.fire(HookPoint::TurnEnd, &Value::Null, &mut ui);
    assert_eq!(session.compacts.lock().unwrap().len(), 1);

    // The command compacts at once, with the instructions it was given.
    host.run_command("trigger-compact", "keep the test plan", &mut ui)
        .expect("the command runs");
    assert_eq!(
        session.compacts.lock().unwrap().last().cloned(),
        Some(Some("keep the test plan".to_string()))
    );
    no_errors(&host);
}

// ---------------------------------------------------------------------------------------------
// protected-paths: write and edit to .env, .git/ and node_modules/ are blocked
// ---------------------------------------------------------------------------------------------

fn tool_call(host: &ExtensionHost, ui: &mut ScriptUi, tool: &str, input: Value) -> HookReply {
    host.fire(
        HookPoint::ToolCall,
        &json!({"toolCallId": "c1", "toolName": tool, "input": input}),
        ui,
    )
}

/// port
#[test]
fn protected_paths_loads_and_passes_scripted_session() {
    let session = Session::new(&[], &[]);
    let (_env, host, _ext) = boot("protected-paths", session);
    let mut ui = ScriptUi::person();

    assert_eq!(
        tool_call(
            &host,
            &mut ui,
            "write",
            json!({"path": ".env", "content": "TOKEN=1\n"})
        ),
        HookReply::Block("Path \".env\" is protected".into())
    );
    assert!(
        ui.noted(Level::Warning, "Blocked write to protected path: .env"),
        "{:?}",
        ui.notes
    );
    assert_eq!(
        tool_call(
            &host,
            &mut ui,
            "edit",
            json!({"path": ".git/config", "oldText": "a", "newText": "b"})
        ),
        HookReply::Block("Path \".git/config\" is protected".into())
    );
    assert!(matches!(
        tool_call(
            &host,
            &mut ui,
            "write",
            json!({"path": "node_modules/x/index.js", "content": ""})
        ),
        HookReply::Block(_)
    ));

    // Other paths, other tools, and a call without a path are not blocked.
    assert_eq!(
        tool_call(
            &host,
            &mut ui,
            "write",
            json!({"path": "src/main.rs", "content": ""})
        ),
        HookReply::Continue
    );
    assert_eq!(
        tool_call(&host, &mut ui, "read", json!({"path": ".env"})),
        HookReply::Continue
    );
    assert_eq!(tool_call(&host, &mut ui, "write", json!({})), HookReply::Continue);
    no_errors(&host);
}
