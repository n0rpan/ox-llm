//! E3 acceptance tests: Registration API.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The tests boot an extension the way `oxa_ext::loader` does (a runtime slot, the API installed on
//! it, then the module), on their own `RuntimeThread`, with the registry entry's slot set before the
//! API is installed. That is the contract `api_register::install` reads.

use oxa_ext::api_register::{self, tool_spec_from_def, validate_tool_schema};
use oxa_ext::{ExtId, ExtState, ExtTool, ExtensionHost, LoadedExtension};
use oxa_fake::TestEnv;
use oxa_proto::{
    Access, CancelToken, CommandSpec, Delivery, ExecResult, ExecSpec, Exposure, HookPoint, Hooks, Host,
    HostAction, KeyId, Mode, OxaError, Sandbox, Source, Tool, ToolClass, ToolCtx, Value,
};
use oxa_script::{CallbackPort, Limits, RuntimeThread, SlotId, Slots};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A sandbox that is never asked to run anything: extension tools reach it only through
/// `ctx.exec`, which these tests do not call.
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

/// A session stand-in that records every action and answers `GetActiveTools` with `active`.
struct Recorder {
    actions: Mutex<Vec<HostAction>>,
    active: Value,
}

impl Recorder {
    fn new(active: Value) -> Arc<Recorder> {
        Arc::new(Recorder {
            actions: Mutex::new(Vec::new()),
            active,
        })
    }
    fn actions(&self) -> Vec<HostAction> {
        self.actions.lock().unwrap().clone()
    }
}

impl Host for Recorder {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        let reply = match &action {
            HostAction::GetActiveTools => self.active.clone(),
            _ => Value::Null,
        };
        self.actions.lock().unwrap().push(action);
        Ok(reply)
    }
}

fn ext_path(env: &TestEnv) -> String {
    env.work().join("ext.js").display().to_string()
}

fn new_state(env: &TestEnv) -> Arc<ExtState> {
    Arc::new(ExtState::new(env.work()))
}

/// Add the registry entry the loader creates before a factory runs.
fn register_ext(st: &Arc<ExtState>, ext: ExtId, path: &str) {
    st.registry.lock().unwrap().extensions.push(LoadedExtension {
        id: ext,
        path: path.to_string(),
        name: "ext".to_string(),
        slot: None,
        declared_rebase: false,
        error: None,
    });
}

/// Boot `src` as extension `ext` on a fresh runtime of `th`: slot, then the API, then the module.
/// The slot is recorded in the registry first, as the loader must do for `install` to find it.
fn boot(st: &Arc<ExtState>, th: &RuntimeThread, ext: ExtId, src: &str) -> Result<SlotId, OxaError> {
    let st = st.clone();
    let src = src.to_string();
    th.run(
        move |slots: &mut Slots| -> Result<SlotId, OxaError> {
            let slot = slots.create(Limits::EXTENSION)?;
            {
                let mut reg = st.registry.lock().unwrap();
                reg.extensions
                    .iter_mut()
                    .find(|e| e.id == ext)
                    .expect("the extension is registered")
                    .slot = Some(slot);
            }
            let rt = slots.get(slot).expect("the slot was just created");
            api_register::install(rt, &st, ext);
            rt.eval_script("ext.js", &src)?;
            Ok(slot)
        },
        &mut |_: Value, _: &CallbackPort| Value::Null,
    )
    .and_then(|r| r)
}

/// A booted extension and the runtime it lives on.
struct Rig {
    st: Arc<ExtState>,
    th: Arc<RuntimeThread>,
    slot: SlotId,
    ext: ExtId,
}

fn rig(env: &TestEnv, src: &str) -> Result<Rig, OxaError> {
    let st = new_state(env);
    let th = Arc::new(RuntimeThread::spawn("e3-test")?);
    let ext = ExtId(0);
    register_ext(&st, ext, &ext_path(env));
    let slot = boot(&st, &th, ext, src)?;
    Ok(Rig { st, th, slot, ext })
}

impl Rig {
    fn attach(&self, host: Arc<dyn Host>) {
        *self.st.host.lock().unwrap() = Some(host);
    }

    /// Evaluate `src` on the extension's runtime (its `oxa` global is the one it booted with).
    fn eval(&self, src: &str) -> Result<Value, OxaError> {
        eval_on(&self.th, self.slot, src)
    }

    fn tool(&self, name: &str) -> oxa_ext::RegisteredTool {
        let reg = self.st.registry.lock().unwrap();
        reg.tools
            .iter()
            .find(|t| t.spec.name == name)
            .cloned()
            .expect("the tool is registered")
    }
}

fn eval_on(th: &RuntimeThread, slot: SlotId, src: &str) -> Result<Value, OxaError> {
    let src = src.to_string();
    th.run(
        move |slots: &mut Slots| match slots.get(slot) {
            Some(rt) => rt.eval_script("test.js", &src),
            None => Err(OxaError::new(
                oxa_proto::codes::SCRIPT,
                "the extension is not loaded",
            )),
        },
        &mut |_: Value, _: &CallbackPort| Value::Null,
    )
    .and_then(|r| r)
}

/// The access-free tool context used by the tests.
fn with_ctx<R>(env: &TestEnv, cancel: &CancelToken, f: impl FnOnce(&ToolCtx) -> R) -> R {
    let sandbox = NoSandbox;
    let cwd: PathBuf = env.work();
    let spill: PathBuf = env.tmp();
    let cx = ToolCtx {
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel,
        env: &[],
        tune: Default::default(),
    };
    f(&cx)
}

// ---------------------------------------------------------------------------------------------
// Acceptance tests (one per WAVES.toml bullet)
// ---------------------------------------------------------------------------------------------

/// bad schema rejected at load
#[test]
fn register_tool_schema_validation() {
    let env = TestEnv::new();

    // The schema is canonical JSON in the definition's key order, with no whitespace.
    let good = json!({"type": "object", "properties": {"b": {"type": "string"}, "a": {"type": "number"}}, "required": ["a"]});
    assert_eq!(
        validate_tool_schema(&good).unwrap(),
        r#"{"type":"object","properties":{"b":{"type":"string"},"a":{"type":"number"}},"required":["a"]}"#
    );

    for (bad, path) in [
        (json!({"type": "array"}), "parameters.type"),
        (json!({"properties": {}}), "parameters.type"),
        (
            json!({"type": "object", "properties": []}),
            "parameters.properties",
        ),
        (
            json!({"type": "object", "properties": {"x": 3}}),
            "parameters.properties.x",
        ),
        (json!({"type": "object", "required": "a"}), "parameters.required"),
        (json!("object"), "parameters"),
    ] {
        let err = validate_tool_schema(&bad).expect_err("a bad schema is refused");
        assert_eq!(err.code, oxa_proto::codes::INVALID);
        assert!(err.message.contains(path), "{bad} -> {err}");
    }

    // A tool definition with a bad name, class or schema is refused by tool_spec_from_def.
    let def = |name: &str, class: Value, parameters: Value| json!({"name": name, "description": "d", "parameters": parameters, "class": class});
    let ok_params = json!({"type": "object"});
    assert!(tool_spec_from_def(&def("mcp__x", Value::Null, ok_params.clone()), "p").is_err());
    assert!(tool_spec_from_def(&def("has space", Value::Null, ok_params.clone()), "p").is_err());
    assert!(tool_spec_from_def(&def(&"x".repeat(65), Value::Null, ok_params.clone()), "p").is_err());
    assert!(tool_spec_from_def(&def("ok-name_1", json!("admin"), ok_params.clone()), "p").is_err());
    assert!(tool_spec_from_def(&def("ok-name_1", Value::Null, json!({"type": "array"})), "p").is_err());

    let spec = tool_spec_from_def(&def("ok-name_1", Value::Null, ok_params), "/x/ext.js").unwrap();
    assert_eq!(
        spec.class,
        ToolClass::Exec,
        "an extension tool asks unless it says otherwise"
    );
    assert_eq!(spec.exposure, Exposure::Direct);
    assert_eq!(spec.source, Source::Extension("/x/ext.js".to_string()));

    // Rejected at load: the factory throws, the extension fails to load, and nothing it
    // registered before the throw is kept.
    let st = new_state(&env);
    let th = RuntimeThread::spawn("e3-load").unwrap();
    let ext = ExtId(0);
    register_ext(&st, ext, &ext_path(&env));
    let bad_load = r#"oxa.registerTool({name: "bad", description: "b", parameters: {type: "array"}, execute() { return {content: "x"}; }});"#;
    let err = boot(&st, &th, ext, bad_load).expect_err("the factory throws on the bad schema");
    assert!(format!("{err}").contains("parameters.type"), "{err}");
    assert!(st.registry.lock().unwrap().tools.is_empty());
}

/// access declared
#[test]
fn registered_tool_runs_through_policy_and_shape() {
    let env = TestEnv::new();
    let r = rig(
        &env,
        r#"
        oxa.registerTool({name: "echo", description: "echo", class: "write",
            parameters: {type: "object", properties: {x: {type: "string"}}},
            execute: async (id, params, signal, onUpdate) => {
                onUpdate({content: [{type: "text", text: "partial"}]});
                return {content: [{type: "text", text: "got " + params.x}], details: {n: 1}};
            }});
        oxa.registerTool({name: "boom", description: "b", parameters: {type: "object"},
            execute: () => { throw new Error("nope"); }});
        oxa.registerTool({name: "flagged", description: "f", class: "read", parameters: {type: "object"},
            execute: () => ({content: "bad", isError: true})});
        oxa.registerTool({name: "fetcher", description: "n", class: "net", parameters: {type: "object"},
            execute: () => ({content: "ok"})});
        "#,
    )
    .expect("the extension loads");

    let cancel = CancelToken::new();
    let cwd = env.work();
    with_ctx(&env, &cancel, |cx| {
        let echo = ExtTool::new(r.tool("echo"), r.st.clone(), r.th.clone(), None);
        assert_eq!(echo.spec().class, ToolClass::Write);
        assert_eq!(echo.spec().source, Source::Extension(ext_path(&env)));
        assert_eq!(
            echo.access(&json!({}), cx).unwrap(),
            vec![Access::Write(cwd.clone())]
        );

        let boom = ExtTool::new(r.tool("boom"), r.st.clone(), r.th.clone(), None);
        assert_eq!(
            boom.access(&json!({}), cx).unwrap(),
            vec![Access::Exec {
                command: "extension tool boom".to_string(),
                net: false
            }]
        );
        let flagged = ExtTool::new(r.tool("flagged"), r.st.clone(), r.th.clone(), None);
        assert!(flagged.access(&json!({}), cx).unwrap().is_empty());
        let fetcher = ExtTool::new(r.tool("fetcher"), r.st.clone(), r.th.clone(), None);
        assert_eq!(
            fetcher.access(&json!({}), cx).unwrap(),
            vec![Access::Net {
                host: "*".to_string()
            }]
        );

        // Shape: the text reaches the caller, partial output goes to `live`, details stay in `ui`.
        let mut partials: Vec<String> = Vec::new();
        let out = echo.run(&json!({"x": "hi"}), cx, &mut |t: &str| {
            partials.push(t.to_string())
        });
        assert_eq!(out.text, "got hi");
        assert!(!out.is_error);
        assert_eq!(out.ui, json!({"n": 1}));
        assert_eq!(partials, vec!["partial".to_string()]);

        let out = boom.run(&json!({}), cx, &mut |_: &str| {});
        assert!(out.is_error && out.text.contains("nope"), "{out:?}");

        let out = flagged.run(&json!({}), cx, &mut |_: &str| {});
        assert!(out.is_error);
        assert_eq!(out.text, "bad");
    });
}

/// listed through Hooks
#[test]
fn register_command_shortcut_flag() {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("the extension host");
    let st = host.state().clone();
    let th = RuntimeThread::spawn("e3-listed").unwrap();
    let ext = ExtId(0);
    register_ext(&st, ext, &ext_path(&env));
    boot(
        &st,
        &th,
        ext,
        r#"
        oxa.registerCommand("greet", {description: "Say hi", argumentHint: "<name>",
            handler: (args) => args, getArgumentCompletions: () => []});
        oxa.registerShortcut("ctrl+k", {description: "Open it", handler: () => {}});
        oxa.registerFlag("verbose", {type: "boolean", default: false, description: "Chatty"});
        oxa.registerFlag("mode", {type: "string", default: "fast"});
        "#,
    )
    .expect("the extension loads");

    let cmds: Vec<CommandSpec> = host.commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].name, "greet");
    assert_eq!(cmds[0].description, "Say hi");
    assert_eq!(cmds[0].argument_hint.as_deref(), Some("<name>"));
    assert_eq!(cmds[0].source, Source::Extension(ext_path(&env)));

    let shortcuts = host.shortcuts();
    assert_eq!(shortcuts.len(), 1);
    assert_eq!(shortcuts[0].key, KeyId::parse("CTRL+K").unwrap());
    assert_eq!(shortcuts[0].description, "Open it");

    let flags = host.flags();
    assert_eq!(flags.len(), 2);
    let verbose = flags.iter().find(|f| f.name == "verbose").expect("listed");
    assert!(verbose.boolean);
    assert_eq!(verbose.default, Value::Bool(false));
    assert_eq!(verbose.description, "Chatty");

    // A value given through Hooks::set_flag is what the script reads back with oxa.getFlag.
    host.set_flag("verbose", &Value::Bool(true));
    host.set_flag("mode", &Value::String("slow".into()));
    // Read the slot first: holding the registry lock across the call would deadlock with getFlag.
    let slot = st.registry.lock().unwrap().extensions[0].slot.unwrap();
    let got = eval_on(
        &th,
        slot,
        r#"[oxa.getFlag("verbose"), oxa.getFlag("mode"), oxa.getFlag("missing")]"#,
    )
    .unwrap();
    assert_eq!(got, json!([true, "slow", null]));
}

/// steer / followUp / nextTurn
#[test]
fn send_message_delivery_modes() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    let rec = Recorder::new(Value::Null);
    r.attach(rec.clone());
    r.eval(
        r#"
        oxa.sendMessage({customType: "note", content: "a", display: true, details: {k: 1}}, {deliverAs: "steer"});
        oxa.sendMessage({customType: "note", content: "b", display: false}, {deliverAs: "followUp", triggerTurn: true});
        oxa.sendMessage({customType: "note", content: [{type: "text", text: "c"}]}, {deliverAs: "nextTurn"});
        oxa.sendMessage({customType: "note", content: "d"});
        "#,
    )
    .expect("the messages are queued");

    assert_eq!(
        rec.actions(),
        vec![
            HostAction::SendMessage {
                custom_type: "note".into(),
                content: "a".into(),
                display: true,
                details: json!({"k": 1}),
                deliver: Delivery::Steer,
                trigger_turn: false,
            },
            HostAction::SendMessage {
                custom_type: "note".into(),
                content: "b".into(),
                display: false,
                details: Value::Null,
                deliver: Delivery::FollowUp,
                trigger_turn: true,
            },
            HostAction::SendMessage {
                custom_type: "note".into(),
                content: "c".into(),
                display: true,
                details: Value::Null,
                deliver: Delivery::NextTurn,
                trigger_turn: false,
            },
            HostAction::SendMessage {
                custom_type: "note".into(),
                content: "d".into(),
                display: true,
                details: Value::Null,
                deliver: Delivery::Steer,
                trigger_turn: false,
            },
        ]
    );

    // An unknown mode is refused; with no session attached the call fails with `busy`.
    let got = r
        .eval(r#"try { oxa.sendMessage({customType: "x", content: "y"}, {deliverAs: "later"}); "no" } catch (e) { e.code }"#)
        .unwrap();
    assert_eq!(got, json!("invalid"));
    let bare = rig(&env, "").expect("loads");
    let got = bare
        .eval(r#"try { oxa.sendMessage({customType: "x", content: "y"}); "no" } catch (e) { e.code }"#)
        .unwrap();
    assert_eq!(got, json!("busy"));
}

/// delivery
#[test]
fn send_user_message() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    let rec = Recorder::new(Value::Null);
    r.attach(rec.clone());
    r.eval(
        r#"
        oxa.sendUserMessage("hi");
        oxa.sendUserMessage([{type: "text", text: "a"}, {type: "text", text: "b"}], {deliverAs: "followUp"});
        "#,
    )
    .unwrap();
    assert_eq!(
        rec.actions(),
        vec![
            HostAction::SendUserMessage {
                text: "hi".into(),
                deliver: Delivery::Steer,
            },
            HostAction::SendUserMessage {
                text: "a\nb".into(),
                deliver: Delivery::FollowUp,
            },
        ]
    );
    // pi's sendUserMessage has no nextTurn.
    let got = r
        .eval(r#"try { oxa.sendUserMessage("x", {deliverAs: "nextTurn"}); "no" } catch (e) { e.code }"#)
        .unwrap();
    assert_eq!(got, json!("invalid"));
}

/// Custom entry + EntryAppended
#[test]
fn append_entry() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    let rec = Recorder::new(Value::Null);
    r.attach(rec.clone());
    r.eval(
        r#"
        oxa.appendEntry("tag", {x: 1});
        oxa.appendEntry("bare");
        "#,
    )
    .unwrap();
    assert_eq!(
        rec.actions(),
        vec![
            HostAction::AppendEntry {
                custom_type: "tag".into(),
                data: json!({"x": 1}),
            },
            HostAction::AppendEntry {
                custom_type: "bare".into(),
                data: Value::Null,
            },
        ]
    );
}

/// applied at settle (epoch)
#[test]
fn active_tools_get_set() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    let rec = Recorder::new(json!(["read", "bash"]));
    r.attach(rec.clone());
    // The request is sent now; the core applies it when the run settles (HostAction's timing rule).
    let got = r
        .eval(
            r#"
            oxa.setActiveTools(["read", "grep"]);
            oxa.getActiveTools();
            "#,
        )
        .unwrap();
    assert_eq!(got, json!(["read", "bash"]));
    assert_eq!(
        rec.actions(),
        vec![
            HostAction::SetActiveTools(vec!["read".into(), "grep".into()]),
            HostAction::GetActiveTools,
        ]
    );
    let got = r
        .eval(r#"try { oxa.setActiveTools("read"); "no" } catch (e) { e.code }"#)
        .unwrap();
    assert_eq!(got, json!("invalid"));
}

/// registerProvider
#[test]
fn omitted_registrations_fail_loudly() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    let got = r
        .eval(
            r#"
            const out = [];
            for (const name of ["registerProvider", "unregisterProvider", "registerVirtualModel", "unregisterVirtualModel"]) {
                try { oxa[name]("x", {}); out.push("no-throw"); } catch (e) { out.push(e.code); }
            }
            try { oxa.on("before_provider_headers", () => {}); out.push("no-throw"); } catch (e) { out.push(e.code); }
            try { oxa.on("no_such_event", () => {}); out.push("no-throw"); } catch (e) { out.push(e.code); }
            out;
            "#,
        )
        .unwrap();
    assert_eq!(
        got,
        json!([
            "unsupported_in_oxa",
            "unsupported_in_oxa",
            "unsupported_in_oxa",
            "unsupported_in_oxa",
            "unsupported_in_oxa",
            "invalid"
        ])
    );
}

// ---------------------------------------------------------------------------------------------
// Further tests
// ---------------------------------------------------------------------------------------------

#[test]
fn on_handler_is_registered_and_its_disposable_removes_it() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    let count = |r: &Rig| {
        r.st.registry
            .lock()
            .unwrap()
            .handlers
            .get(&HookPoint::Input)
            .map_or(0, Vec::len)
    };
    r.eval(r#"globalThis.off = oxa.on("input", () => ({action: "handled"}));"#)
        .unwrap();
    assert_eq!(count(&r), 1);
    let handler = r.st.registry.lock().unwrap().handlers[&HookPoint::Input][0];
    assert_eq!(handler.ext, r.ext);
    assert_eq!(handler.slot, r.slot);
    r.eval("off();").unwrap();
    assert_eq!(count(&r), 0);
}

#[test]
fn bus_listeners_register_and_unregister() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    r.eval(r#"globalThis.off = oxa.events.on("ping", () => {});"#)
        .unwrap();
    assert_eq!(r.st.registry.lock().unwrap().bus_listeners.len(), 1);
    assert_eq!(r.st.registry.lock().unwrap().bus_listeners[0].0, "ping");
    r.eval("off();").unwrap();
    assert!(r.st.registry.lock().unwrap().bus_listeners.is_empty());
    // E-INT 5: `emit` queues the event; the bus delivers it from the session side after the
    // emitting handler returned.
    let got = r
        .eval(r#"try { oxa.events.emit("ping", {n: 1}); "queued" } catch (e) { e.code }"#)
        .unwrap();
    assert_eq!(got, json!("queued"));
    let queued = r.st.bus_queue.lock().unwrap().clone();
    assert_eq!(queued, vec![("ping".to_string(), json!({"n": 1}))]);
}

#[test]
fn registrations_replace_by_name_and_prompt_sections_too() {
    let env = TestEnv::new();
    let r = rig(
        &env,
        r#"
        oxa.registerTool({name: "t", description: "one", parameters: {type: "object"}, execute: () => ({content: "1"})});
        oxa.registerTool({name: "t", description: "two", parameters: {type: "object"}, execute: () => ({content: "2"})});
        oxa.registerCommand("c", {description: "one", handler: () => {}});
        oxa.registerCommand("c", {description: "two", handler: () => {}});
        oxa.addPromptSection("s", "first");
        oxa.addPromptSection("s", "second");
        "#,
    )
    .expect("loads");
    let reg = r.st.registry.lock().unwrap();
    assert_eq!(reg.tools.len(), 1);
    assert_eq!(reg.tools[0].spec.description, "two");
    assert_eq!(reg.commands.len(), 1);
    assert_eq!(reg.commands[0].spec.description, "two");
    assert_eq!(reg.prompt_sections, vec![("s".to_string(), "second".to_string())]);
}

#[test]
fn session_actions_are_forwarded() {
    let env = TestEnv::new();
    let r = rig(&env, "").expect("loads");
    let rec = Recorder::new(Value::Null);
    r.attach(rec.clone());
    let got = r
        .eval(
            r#"
            oxa.setSessionName("work");
            oxa.setLabel("e42", "milestone");
            oxa.setLabel("e43", undefined);
            oxa.setThinkingLevel("high");
            oxa.setModel("qwen");
            oxa.getAllTools();
            "#,
        )
        .unwrap();
    // The recorder answers only GetActiveTools, so getAllTools returns null here.
    assert_eq!(got, Value::Null);
    assert_eq!(
        rec.actions(),
        vec![
            HostAction::SetSessionName("work".into()),
            HostAction::SetLabel {
                target: "e42".into(),
                label: Some("milestone".into()),
            },
            HostAction::SetLabel {
                target: "e43".into(),
                label: None,
            },
            HostAction::SetThinking(oxa_proto::ThinkingLevel::High),
            // setModel checks the engine's models first (EXT-075).
            HostAction::GetState,
            HostAction::SetModel("qwen".into()),
            HostAction::GetAllTools,
        ]
    );
}

#[test]
fn tool_update_and_cancel_interrupt_the_script() {
    let env = TestEnv::new();
    let r = rig(
        &env,
        r#"
        oxa.registerTool({name: "spin", description: "s", class: "read", parameters: {type: "object"},
            execute: () => { while (true) {} }});
        "#,
    )
    .expect("loads");
    let cancel = CancelToken::new();
    let stopper = cancel.clone();
    let tripwire = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        stopper.cancel();
    });
    let started = Instant::now();
    with_ctx(&env, &cancel, |cx| {
        let spin = ExtTool::new(r.tool("spin"), r.st.clone(), r.th.clone(), None);
        let out = spin.run(&json!({}), cx, &mut |_: &str| {});
        assert!(out.is_error, "{out:?}");
    });
    tripwire.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the cancel stops the loop"
    );
}

#[test]
fn text_blocks_are_joined_and_images_dropped() {
    let env = TestEnv::new();
    let r = rig(
        &env,
        r#"
        oxa.registerTool({name: "plain", description: "p", class: "read", parameters: {type: "object"},
            execute: () => ({content: [{type: "text", text: "x"}, {type: "image", data: "", mimeType: "image/png"}, {type: "text", text: "y"}]})});
        "#,
    )
    .expect("loads");
    with_ctx(&env, &CancelToken::new(), |cx| {
        let t = ExtTool::new(r.tool("plain"), r.st.clone(), r.th.clone(), None);
        let out = t.run(&json!({}), cx, &mut |_: &str| {});
        assert_eq!(out.text, "x\ny");
        assert_eq!(out.ui, Value::Null);
    });
}
