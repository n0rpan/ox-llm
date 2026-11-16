//! F9 acceptance tests: Example ports 1.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! Each test loads one port from `examples/extensions/<name>` on a real `ExtensionHost` (the
//! QuickJS runtime, the bus, the W-guard), attaches a session double (answers the reads and
//! records every write the port asks of the session), and plays a scripted session: commands,
//! shortcuts, tool calls, lifecycle events, with a scripted UI that answers the dialogs. A port
//! passes when its replies, its session writes and its UI calls are the ones pi's example makes.
//! The full session with the fake engine (the W1-W6 invariants over real requests) is G12's
//! `oxa-parity/tests/extensions.rs`: `oxa-ext` cannot depend on `oxa-core`.

use oxa_ext::{ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{
    Access, Answer, CancelToken, ComponentAction, CustomOpts, ExecResult, ExecSpec, HookPoint, HookReply,
    Hooks, Host, HostAction, KeyId, Level, Mode, OxaError, Placement, Sandbox, Tool, ToolCtx, ToolOutput, Ui,
    UiComponent, Value,
};
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

const EXAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/extensions");

/// A sandbox that is never asked to run anything (the ports here run no process).
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

/// The session as the ports see it: reads answer from fields the script sets, every action is
/// recorded in order.
struct Double {
    actions: Mutex<Vec<HostAction>>,
    active: Mutex<Vec<String>>,
    entries: Mutex<Value>,
    branch: Mutex<Value>,
}

impl Double {
    fn new() -> Arc<Double> {
        Arc::new(Double {
            actions: Mutex::new(Vec::new()),
            active: Mutex::new(Vec::new()),
            entries: Mutex::new(json!([])),
            branch: Mutex::new(json!([])),
        })
    }
    fn set_active(&self, names: &[&str]) {
        *self.active.lock().unwrap() = names.iter().map(|s| s.to_string()).collect();
    }
    fn set_branch(&self, v: Value) {
        *self.branch.lock().unwrap() = v;
    }
    /// Every write the port made (queries excluded), in order.
    fn effects(&self) -> Vec<HostAction> {
        self.actions
            .lock()
            .unwrap()
            .iter()
            .filter(|a| !is_query(a))
            .cloned()
            .collect()
    }
}

fn is_query(a: &HostAction) -> bool {
    matches!(
        a,
        HostAction::GetActiveTools
            | HostAction::GetAllTools
            | HostAction::GetEntries
            | HostAction::GetBranch
            | HostAction::GetState
            | HostAction::IsIdle
            | HostAction::ContextUsage
    )
}

impl Host for Double {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        let reply = match &action {
            HostAction::GetActiveTools => json!(*self.active.lock().unwrap()),
            HostAction::GetEntries => self.entries.lock().unwrap().clone(),
            HostAction::GetBranch => self.branch.lock().unwrap().clone(),
            HostAction::IsIdle => Value::Bool(true),
            _ => Value::Null,
        };
        self.actions.lock().unwrap().push(action);
        Ok(reply)
    }
}

/// A UI with scripted answers. `selects` / `editors` are consumed in order; an empty queue
/// dismisses the dialog (`None`).
#[derive(Default)]
struct TestUi {
    interactive: bool,
    selects: VecDeque<Option<usize>>,
    editors: VecDeque<Option<String>>,
    /// Titles of the selects asked, in order.
    asked: Vec<String>,
    notes: Vec<(Level, String)>,
    /// Every `setStatus` call in order: key and text (`None` removes).
    statuses: Vec<(String, Option<String>)>,
    /// Every `setWidget` call in order: key and lines (`None` removes).
    widgets: Vec<(String, Option<Vec<String>>)>,
    /// The lines of the last custom component rendered (`/todos`).
    custom_lines: Option<Vec<String>>,
}

impl Ui for TestUi {
    fn select(&mut self, title: &str, _options: &[String], _timeout_ms: Option<u32>) -> Option<usize> {
        self.asked.push(title.to_string());
        self.selects.pop_front().unwrap_or(None)
    }
    fn confirm(&mut self, _title: &str, _body: &str, _timeout_ms: Option<u32>) -> Option<bool> {
        None
    }
    fn input(&mut self, _title: &str, _placeholder: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, title: &str, _prefill: &str) -> Option<String> {
        self.asked.push(title.to_string());
        self.editors.pop_front().unwrap_or(None)
    }
    fn notify(&mut self, text: &str, level: Level) {
        self.notes.push((level, text.to_string()));
    }
    fn set_status(&mut self, key: &str, text: Option<&str>) {
        self.statuses.push((key.to_string(), text.map(str::to_string)));
    }
    fn set_widget(&mut self, key: &str, lines: Option<&[String]>, _at: Placement) {
        self.widgets.push((key.to_string(), lines.map(|l| l.to_vec())));
    }
    fn set_title(&mut self, _t: &str) {}
    fn set_editor_text(&mut self, _t: &str) {}
    fn ask_access(&mut self, _a: &Access) -> Answer {
        Answer::Deny
    }
    fn interactive(&self) -> bool {
        self.interactive
    }
    /// Renders the component once, then sends Escape: the component's `done` ends it.
    fn custom(&mut self, c: Box<dyn UiComponent>, _opts: CustomOpts) -> Option<Value> {
        let mut c = c;
        self.custom_lines = Some(c.render(60));
        match c.input(&KeyId::parse("escape").expect("escape is a key"), None) {
            ComponentAction::Done(v) => Some(v),
            _ => None,
        }
    }
}

/// A loaded port and its session double.
struct Rig {
    env: TestEnv,
    host: ExtensionHost,
    double: Arc<Double>,
}

/// Load `examples/extensions/<name>` (a directory with `index.js`) and attach a session double.
fn rig(name: &str) -> Rig {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("the extension runtime starts");
    let loaded = host.load(&[PathBuf::from(EXAMPLES).join(name)], &LoadOpts::default());
    assert_eq!(loaded.len(), 1, "{name}: one entry");
    assert!(
        loaded[0].error.is_none(),
        "{name} failed to load: {:?}",
        loaded[0].error
    );
    let double = Double::new();
    Hooks::attach(&host, double.clone() as Arc<dyn Host>);
    Rig { env, host, double }
}

impl Rig {
    fn fire(&self, p: HookPoint, payload: Value, ui: &mut TestUi) -> HookReply {
        Hooks::fire(&self.host, p, &payload, ui)
    }
    fn command(&self, name: &str, args: &str, ui: &mut TestUi) {
        Hooks::run_command(&self.host, name, args, ui).unwrap_or_else(|e| panic!("/{name}: {e}"));
    }
    fn shortcut(&self, key: &str, ui: &mut TestUi) {
        let key = KeyId::parse(key).expect("valid key");
        Hooks::run_shortcut(&self.host, &key, ui).expect("shortcut runs");
    }
    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        Hooks::tools(&self.host)
            .into_iter()
            .find(|t| t.spec().name == name)
            .unwrap_or_else(|| panic!("tool {name} is registered"))
    }
    fn run_tool(&self, name: &str, args: Value) -> ToolOutput {
        let tool = self.tool(name);
        let sandbox = NoSandbox;
        let cancel = CancelToken::new();
        let cwd = self.env.work();
        let spill = self.env.tmp();
        let cx = ToolCtx {
            cwd: &cwd,
            spill_dir: &spill,
            sandbox: &sandbox,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &[],
            tune: Default::default(),
            hidden: None,
        };
        tool.run(&args, &cx, &mut |_: &str| {})
    }
    /// No extension error was recorded, and no error note reached the UI.
    fn assert_clean(&self, ui: &TestUi) {
        let errors = self.host.state().take_errors();
        assert!(errors.is_empty(), "extension errors: {errors:?}");
        let bad: Vec<_> = ui.notes.iter().filter(|(l, _)| *l == Level::Error).collect();
        assert!(bad.is_empty(), "error notes: {bad:?}");
    }
}

fn tool_call(name: &str, input: Value) -> Value {
    json!({ "toolName": name, "toolCallId": "call-1", "input": input })
}

fn assistant(text: &str) -> Value {
    json!({ "type": "message", "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] } })
}

fn status(key: &str, text: Option<&str>) -> (String, Option<String>) {
    (key.to_string(), text.map(str::to_string))
}

fn blocked(reply: &HookReply) -> Option<&str> {
    match reply {
        HookReply::Block(reason) => Some(reason.as_str()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Acceptance tests (one per WAVES.toml bullet)
// ---------------------------------------------------------------------------------------------

/// port
#[test]
fn plan_mode_loads_and_passes_scripted_session() {
    let r = rig("plan-mode");
    let mut ui = TestUi {
        interactive: true,
        ..TestUi::default()
    };

    // Registrations: the flag, both commands, the shortcut.
    let commands: Vec<String> = Hooks::commands(&r.host).into_iter().map(|c| c.name).collect();
    assert!(commands.contains(&"plan".to_string()) && commands.contains(&"todos".to_string()));
    let shortcuts: Vec<String> = Hooks::shortcuts(&r.host)
        .into_iter()
        .map(|s| s.key.as_str().to_string())
        .collect();
    assert_eq!(shortcuts, vec!["ctrl+alt+p".to_string()]);
    let flags: Vec<String> = Hooks::flags(&r.host).into_iter().map(|f| f.name).collect();
    assert_eq!(flags, vec!["plan".to_string()]);

    // Session start, plan mode off: the status and widget are cleared.
    assert_eq!(
        r.fire(HookPoint::SessionStart, Value::Null, &mut ui),
        HookReply::Continue
    );
    assert_eq!(ui.statuses.last(), Some(&status("plan-mode", None)));

    // /plan: the write tools go, the read tools and the rest stay; the state is persisted.
    r.double.set_active(&["read", "bash", "edit", "write"]);
    r.command("plan", "", &mut ui);
    let effects = r.double.effects();
    assert!(effects.contains(&HostAction::SetActiveTools(
        ["read", "bash", "grep", "find", "ls", "questionnaire"]
            .map(str::to_string)
            .to_vec()
    )));
    assert!(effects
        .iter()
        .any(|a| matches!(a, HostAction::AppendEntry { custom_type, .. } if custom_type == "plan-mode")));
    assert!(ui
        .notes
        .iter()
        .any(|(_, t)| t == "Plan mode enabled. Built-in write tools disabled."));

    // Plan mode blocks a bash command that is not allowlisted, and only that.
    let reply = r.fire(
        HookPoint::ToolCall,
        tool_call("bash", json!({"command": "rm -rf build"})),
        &mut ui,
    );
    assert!(
        blocked(&reply).is_some_and(|reason| reason.starts_with("Plan mode: command blocked")),
        "{reply:?}"
    );
    assert_eq!(
        r.fire(
            HookPoint::ToolCall,
            tool_call("bash", json!({"command": "git status"})),
            &mut ui
        ),
        HookReply::Continue
    );
    assert_eq!(
        r.fire(
            HookPoint::ToolCall,
            tool_call("bash", json!({"command": "ls -la src"})),
            &mut ui
        ),
        HookReply::Continue
    );
    assert!(blocked(&r.fire(
        HookPoint::ToolCall,
        tool_call("bash", json!({"command": "git commit -m x"})),
        &mut ui
    ))
    .is_some());
    assert_eq!(
        r.fire(
            HookPoint::ToolCall,
            tool_call("edit", json!({"path": "a"})),
            &mut ui
        ),
        HookReply::Continue
    );

    // Each prompt gets the plan-mode instructions, appended after the user message (append-only).
    match r.fire(
        HookPoint::BeforeAgentStart,
        json!({"prompt": "look at the parser"}),
        &mut ui,
    ) {
        HookReply::Append(msgs) => {
            assert_eq!(msgs.len(), 1);
            assert!(msgs[0]
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with("[PLAN MODE ACTIVE]")));
        }
        other => panic!("expected the plan context to be appended, got {other:?}"),
    }

    // The run ends with a numbered plan: it is extracted and the user chooses to execute it.
    r.double.set_branch(json!([assistant(
        "Plan:\n1. Read the parser module\n2. Update the **config** loader\n3. Run the tests\n"
    )]));
    ui.selects.push_back(Some(0));
    r.fire(HookPoint::AgentEnd, Value::Null, &mut ui);
    assert_eq!(ui.asked, vec!["Plan mode - what next?".to_string()]);
    let effects = r.double.effects();
    let execute = effects
        .iter()
        .find_map(|a| match a {
            HostAction::SendMessage {
                custom_type,
                content,
                trigger_turn,
                deliver,
                ..
            } if custom_type == "plan-mode-execute" => Some((content.clone(), *trigger_turn, *deliver)),
            _ => None,
        })
        .expect("the execute message is sent");
    assert!(execute.0.contains("Start with: Parser module"), "{}", execute.0);
    assert!(execute.1, "the execute message starts a turn");
    assert_eq!(execute.2, oxa_proto::Delivery::FollowUp);
    // Leaving plan mode gives back the tools it took.
    assert_eq!(
        effects.iter().rev().find_map(|a| match a {
            HostAction::SetActiveTools(names) => Some(names.clone()),
            _ => None,
        }),
        Some(vec![
            "read".to_string(),
            "bash".to_string(),
            "edit".to_string(),
            "write".to_string()
        ])
    );

    // Progress: [DONE:n] markers tick the footer status, one step per marker.
    r.double.set_branch(json!([assistant("[DONE:1] parser read")]));
    r.fire(HookPoint::TurnEnd, Value::Null, &mut ui);
    assert_eq!(ui.statuses.last(), Some(&status("plan-mode", Some("📋 1/3"))));
    assert_eq!(
        ui.widgets.last().and_then(|(_, lines)| lines.clone()),
        Some(vec![
            "☑ Parser module".to_string(),
            "☐ Config loader".to_string(),
            "☐ Tests".to_string()
        ])
    );

    // The last steps are done; the run ends and the completion card waits for the next prompt.
    r.double
        .set_branch(json!([assistant("[DONE:2] [DONE:3] all three")]));
    r.fire(HookPoint::TurnEnd, Value::Null, &mut ui);
    r.fire(HookPoint::AgentEnd, Value::Null, &mut ui);
    assert!(r.double.effects().iter().any(|a| matches!(a,
        HostAction::SendMessage { custom_type, deliver: oxa_proto::Delivery::NextTurn, trigger_turn: false, .. }
            if custom_type == "plan-complete")));
    assert_eq!(ui.statuses.last(), Some(&status("plan-mode", None)));

    // /todos with an empty plan, then Ctrl+Alt+P turns plan mode on again.
    r.command("todos", "", &mut ui);
    assert!(ui.notes.iter().any(|(_, t)| t.starts_with("No todos.")));
    r.shortcut("ctrl+alt+p", &mut ui);
    assert!(ui
        .notes
        .iter()
        .any(|(_, t)| t == "Plan mode enabled. Built-in write tools disabled."));

    r.assert_clean(&ui);
}

/// port
#[test]
fn permission_gate_loads_and_passes_scripted_session() {
    let r = rig("permission-gate");
    let mut ui = TestUi {
        interactive: true,
        ..TestUi::default()
    };

    // A dangerous command asks; "No" blocks it.
    ui.selects.push_back(Some(1));
    let reply = r.fire(
        HookPoint::ToolCall,
        tool_call("bash", json!({"command": "sudo rm /etc/hosts"})),
        &mut ui,
    );
    assert_eq!(blocked(&reply), Some("Blocked by user"));
    assert_eq!(ui.asked.len(), 1);
    assert!(ui.asked[0].starts_with("⚠️ Dangerous command:"));

    // "Yes" lets it run.
    ui.selects.push_back(Some(0));
    assert_eq!(
        r.fire(
            HookPoint::ToolCall,
            tool_call("bash", json!({"command": "rm -rf build"})),
            &mut ui
        ),
        HookReply::Continue
    );
    assert_eq!(ui.asked.len(), 2);

    // A dismissed prompt is not consent.
    ui.selects.push_back(None);
    assert_eq!(
        blocked(&r.fire(
            HookPoint::ToolCall,
            tool_call("bash", json!({"command": "chmod 777 run.sh"})),
            &mut ui
        )),
        Some("Blocked by user")
    );

    // Ordinary commands and other tools are not asked about.
    assert_eq!(
        r.fire(
            HookPoint::ToolCall,
            tool_call("bash", json!({"command": "ls -la"})),
            &mut ui
        ),
        HookReply::Continue
    );
    assert_eq!(
        r.fire(
            HookPoint::ToolCall,
            tool_call("read", json!({"command": "sudo"})),
            &mut ui
        ),
        HookReply::Continue
    );
    assert_eq!(ui.asked.len(), 3);

    // Without a UI a dangerous command is blocked without asking.
    ui.interactive = false;
    assert_eq!(
        blocked(&r.fire(
            HookPoint::ToolCall,
            tool_call("bash", json!({"command": "rm -rf x"})),
            &mut ui
        )),
        Some("Dangerous command blocked (no UI for confirmation)")
    );
    assert_eq!(ui.asked.len(), 3);

    assert!(
        r.double.effects().is_empty(),
        "the gate writes nothing to the session"
    );
    r.assert_clean(&ui);
}

/// summary lands as the W5 head
#[test]
fn custom_compaction_loads_and_passes_scripted_session() {
    let r = rig("custom-compaction");
    let mut ui = TestUi {
        interactive: true,
        ..TestUi::default()
    };
    let commands: Vec<String> = Hooks::commands(&r.host).into_iter().map(|c| c.name).collect();
    assert_eq!(commands, vec!["compact-all".to_string()]);

    // An ordinary compaction is the core's: the port observes it and answers Continue.
    let reply = r.fire(
        HookPoint::SessionBeforeCompact,
        json!({"reason": "threshold", "instructions": null, "tokensBefore": 120000, "firstKeptEntryId": "e9"}),
        &mut ui,
    );
    assert_eq!(reply, HookReply::Continue);
    assert!(ui.notes.is_empty());
    assert!(r.double.effects().is_empty());

    // /compact-all asks the core to compact with the port's instructions. The port itself sends
    // no model request: the only session write is the compaction request.
    r.command("compact-all", "", &mut ui);
    let effects = r.double.effects();
    assert_eq!(effects.len(), 1, "{effects:?}");
    let instructions = match &effects[0] {
        HostAction::Compact {
            instructions: Some(text),
        } => text.clone(),
        other => panic!("expected a compaction with instructions, got {other:?}"),
    };
    assert!(instructions.contains("Create a comprehensive summary"));

    // The core then fires session_before_compact with those instructions. The port observes it
    // and answers Continue, not Summary: the summary is the core's own W5 request, so it lands as
    // the head through the core's path and no second model is called.
    let reply = r.fire(
        HookPoint::SessionBeforeCompact,
        json!({"reason": "manual", "instructions": instructions, "tokensBefore": 98765, "firstKeptEntryId": "e40"}),
        &mut ui,
    );
    assert_eq!(reply, HookReply::Continue);
    assert!(ui
        .notes
        .iter()
        .any(|(l, t)| *l == Level::Info && t.contains("98765 tokens")));
    assert_eq!(
        r.double.effects().len(),
        1,
        "no request of the port's own: {:?}",
        r.double.effects()
    );

    r.assert_clean(&ui);
}

/// port
#[test]
fn todo_loads_and_passes_scripted_session() {
    let r = rig("todo");
    let mut ui = TestUi {
        interactive: true,
        ..TestUi::default()
    };
    assert!(Hooks::commands(&r.host).iter().any(|c| c.name == "todos"));

    // The state comes back from the branch: the last todo tool result carries the list.
    r.double.set_branch(json!([{
        "type": "message",
        "message": {
            "role": "toolResult",
            "toolName": "todo",
            "details": {
                "action": "add",
                "todos": [{"id": 1, "text": "write spec", "done": false}, {"id": 2, "text": "review", "done": true}],
                "nextId": 3
            }
        }
    }]));
    assert_eq!(
        r.fire(HookPoint::SessionStart, Value::Null, &mut ui),
        HookReply::Continue
    );

    let out = r.run_tool("todo", json!({"action": "list"}));
    assert!(!out.is_error);
    assert_eq!(out.text, "[ ] #1: write spec\n[x] #2: review");

    let out = r.run_tool("todo", json!({"action": "add", "text": "ship it"}));
    assert_eq!(out.text, "Added todo #3: ship it");

    // /todos shows the list (1 of 3 done) and closes on Escape.
    r.command("todos", "", &mut ui);
    let lines = ui.custom_lines.clone().expect("the component was shown");
    assert!(lines.iter().any(|l| l == "  1/3 completed"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "  ○ #1 write spec"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "  ✓ #2 review"), "{lines:?}");

    assert_eq!(
        r.run_tool("todo", json!({"action": "toggle", "id": 1})).text,
        "Todo #1 completed"
    );
    assert_eq!(
        r.run_tool("todo", json!({"action": "toggle", "id": 9})).text,
        "Todo #9 not found"
    );
    assert_eq!(
        r.run_tool("todo", json!({"action": "add"})).text,
        "Error: text required for add"
    );
    assert_eq!(
        r.run_tool("todo", json!({"action": "list"})).text,
        "[x] #1: write spec\n[x] #2: review\n[ ] #3: ship it"
    );
    assert_eq!(
        r.run_tool("todo", json!({"action": "clear"})).text,
        "Cleared 3 todos"
    );
    assert_eq!(r.run_tool("todo", json!({"action": "list"})).text, "No todos");

    // A tree move rebuilds the state from the new branch.
    r.double.set_branch(json!([]));
    assert_eq!(
        r.fire(HookPoint::SessionTree, Value::Null, &mut ui),
        HookReply::Continue
    );
    assert_eq!(r.run_tool("todo", json!({"action": "list"})).text, "No todos");

    // The session writes nothing: the todo state lives in the tool results.
    assert!(r.double.effects().is_empty());
    r.assert_clean(&ui);
}

/// port
#[test]
fn status_line_loads_and_passes_scripted_session() {
    let r = rig("status-line");
    let mut ui = TestUi::default();

    assert_eq!(
        r.fire(HookPoint::SessionStart, Value::Null, &mut ui),
        HookReply::Continue
    );
    assert_eq!(
        r.fire(HookPoint::TurnStart, Value::Null, &mut ui),
        HookReply::Continue
    );
    assert_eq!(
        r.fire(HookPoint::TurnEnd, Value::Null, &mut ui),
        HookReply::Continue
    );
    assert_eq!(
        r.fire(HookPoint::TurnStart, Value::Null, &mut ui),
        HookReply::Continue
    );
    assert_eq!(
        r.fire(HookPoint::TurnEnd, Value::Null, &mut ui),
        HookReply::Continue
    );

    assert_eq!(
        ui.statuses,
        vec![
            status("status-demo", Some("Ready")),
            status("status-demo", Some("● Turn 1...")),
            status("status-demo", Some("✓ Turn 1 complete")),
            status("status-demo", Some("● Turn 2...")),
            status("status-demo", Some("✓ Turn 2 complete")),
        ]
    );
    assert!(r.double.effects().is_empty());
    r.assert_clean(&ui);
}
