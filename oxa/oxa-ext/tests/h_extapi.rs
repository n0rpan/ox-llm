//! Wave H (lane extapi): the extension API that pi has and oxa lacked, the d.ts / glue
//! agreements, dialog results, error codes, tool argument validation and the oxa:tui pieces.
//! Each test loads a real extension on a real runtime with a session double behind `Host`.

use oxa_ext::{check_imports, validate_tool_args, ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{
    codes, Access, Answer, CancelToken, ComponentAction, CustomOpts, ExecResult, ExecSpec, HookPoint, Hooks,
    Host, HostAction, KeyId, Level, Mode, OxaError, Placement, Sandbox, ToolCtx, Ui, UiComponent, Value,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// A session double: records every action, answers queries from a fixed table.
#[derive(Default)]
struct Double {
    seen: Mutex<Vec<HostAction>>,
}

impl Host for Double {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        let out = match &action {
            HostAction::GetSessionName => json!("named"),
            HostAction::GetThinkingLevel => json!("high"),
            HostAction::GetSettings => json!({"theme": "dark", "marker": 7}),
            HostAction::GetSystemPrompt => json!("You are oxa."),
            HostAction::GetCommands => json!([{"name": "x", "source": "extension",
                "sourceInfo": {"path": "/e/x.js", "source": "local", "scope": "user", "origin": "top-level"}}]),
            HostAction::GetState => {
                json!({"model": "m1", "models": ["m1", "m2"], "sessionId": "s1", "pendingMessageCount": 0})
            }
            HostAction::GetBranch => json!([{"id": "a", "type": "message"}, {"id": "b", "type": "custom"}]),
            HostAction::GetEntries => json!([{"id": "a"}, {"id": "b"}]),
            HostAction::Exec(_) => json!({"output": "out\n", "exit": 0, "timedOut": false}),
            HostAction::CallTool { name, .. } if name == "ls" => json!({"text": "a.txt", "isError": false}),
            HostAction::CallTool { .. } => return Err(OxaError::new(codes::NOT_FOUND, "no tool named nope")),
            _ => Value::Null,
        };
        self.seen.lock().unwrap().push(action);
        Ok(out)
    }
}

/// A UI that records notes and answers dialogs from queues.
#[derive(Default)]
struct TestUi {
    interactive: bool,
    selects: VecDeque<Option<usize>>,
    confirms: VecDeque<Option<bool>>,
    inputs: VecDeque<Option<String>>,
    notes: Vec<String>,
    keys: VecDeque<(&'static str, Option<&'static str>)>,
    renders: Vec<Vec<String>>,
    actions: Vec<ComponentAction>,
}

impl Ui for TestUi {
    fn select(&mut self, _t: &str, _o: &[String], _ms: Option<u32>) -> Option<usize> {
        self.selects.pop_front().flatten()
    }
    fn confirm(&mut self, _t: &str, _b: &str, _ms: Option<u32>) -> Option<bool> {
        self.confirms.pop_front().flatten()
    }
    fn input(&mut self, _t: &str, _p: &str) -> Option<String> {
        self.inputs.pop_front().flatten()
    }
    fn editor(&mut self, _t: &str, _p: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, text: &str, _level: Level) {
        self.notes.push(text.to_string());
    }
    fn set_status(&mut self, _k: &str, _t: Option<&str>) {}
    fn set_widget(&mut self, _k: &str, _l: Option<&[String]>, _a: Placement) {}
    fn set_title(&mut self, _t: &str) {}
    fn set_editor_text(&mut self, _t: &str) {}
    fn ask_access(&mut self, _a: &Access) -> Answer {
        Answer::Deny
    }
    fn interactive(&self) -> bool {
        self.interactive
    }
    fn custom(&mut self, mut c: Box<dyn UiComponent>, _o: CustomOpts) -> Option<Value> {
        self.renders.push(c.render(40));
        while let Some((k, text)) = self.keys.pop_front() {
            let a = c.input(&KeyId::parse(k).expect("key"), text);
            self.actions.push(a.clone());
            self.renders.push(c.render(40));
            if let ComponentAction::Done(v) = a {
                return Some(v);
            }
        }
        None
    }
}

fn ext(body: &str) -> String {
    format!("import {{ SettingsList, Editor, wrapTextWithAnsi }} from \"oxa:tui\";\nexport default function (oxa) {{\n{body}\n}}\n")
}

fn load(body: &str) -> (TestEnv, ExtensionHost, Arc<Double>) {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("runtime");
    let loaded = host.load_source("t", &ext(body), &LoadOpts::default());
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let double = Arc::new(Double::default());
    Hooks::attach(&host, double.clone() as Arc<dyn Host>);
    (env, host, double)
}

/// Run the command `/t` registered by `body` and return the notes.
fn run(body: &str, ui: &mut TestUi) -> (ExtensionHost, Arc<Double>) {
    let (_env, host, double) = load(body);
    host.run_command("t", "", ui).expect("the command runs");
    (host, double)
}

fn command(handler: &str) -> String {
    format!("oxa.registerCommand('t', {{ handler: async (args, ctx) => {{ {handler} }} }});")
}

#[test]
fn getters_answer_from_the_session() {
    let mut ui = TestUi::default();
    run(
        &command(
            "ctx.ui.notify('name=' + oxa.getSessionName());
             ctx.ui.notify('level=' + oxa.getThinkingLevel());
             ctx.ui.notify('marker=' + oxa.getSettings().marker);
             ctx.ui.notify('cmds=' + oxa.getCommands().map((c) => c.name + ':' + c.source + ':' + c.sourceInfo.scope).join(','));
             ctx.ui.notify('sys=' + ctx.getSystemPrompt());
             ctx.ui.notify('tl=' + ctx.thinkingLevel);
             ctx.ui.notify('leaf=' + ctx.sessionManager.getLeafEntry().id + '/' + ctx.sessionManager.getLeafId());
             ctx.ui.notify('entry=' + ctx.sessionManager.getEntry('b').id + '/' + ctx.sessionManager.getEntry('zz'));
             ctx.ui.notify('sid=' + ctx.sessionManager.getSessionId());
             ctx.ui.notify('pending=' + ctx.hasPendingMessages());",
        ),
        &mut ui,
    );
    assert_eq!(
        ui.notes,
        vec![
            "name=named",
            "level=high",
            "marker=7",
            "cmds=x:extension:user",
            "sys=You are oxa.",
            "tl=high",
            "leaf=b/b",
            "entry=b/undefined",
            "sid=s1",
            "pending=false",
        ]
    );
}

#[test]
fn mode_and_can_prompt() {
    // Without a mode set by the embedder, the mode follows the UI.
    let mut ui = TestUi {
        interactive: true,
        ..TestUi::default()
    };
    let body = command("ctx.ui.notify(ctx.mode + ':' + ctx.canPrompt + ':' + ctx.hasUI);");
    run(&body, &mut ui);
    let mut headless = TestUi::default();
    run(&body, &mut headless);
    assert_eq!(ui.notes, vec!["tui:true:true"]);
    assert_eq!(headless.notes, vec!["print:false:false"]);
    // The embedder names the mode.
    let (_env, host, _d) = load(&body);
    host.set_mode("rpc");
    let mut ui = TestUi {
        interactive: true,
        ..TestUi::default()
    };
    host.run_command("t", "", &mut ui).unwrap();
    assert_eq!(ui.notes, vec!["rpc:true:true"]);
}

#[test]
fn dialogs_answer_like_pi() {
    let mut ui = TestUi {
        interactive: true,
        selects: VecDeque::from([Some(1), None]),
        confirms: VecDeque::from([None, Some(true)]),
        inputs: VecDeque::from([None]),
        ..TestUi::default()
    };
    run(
        &command(
            "const show = (v) => typeof v + ':' + String(v);
             ctx.ui.notify(show(await ctx.ui.select('t', ['a', 'b'])));
             ctx.ui.notify(show(ctx.ui.select('t', ['a', 'b'], {timeout: 5})));
             ctx.ui.notify(show(await ctx.ui.confirm('t', 'b')));
             ctx.ui.notify(show(ctx.ui.confirm('t', 'b')));
             ctx.ui.notify(show(await ctx.ui.input('t', 'p')));",
        ),
        &mut ui,
    );
    assert_eq!(
        ui.notes,
        vec![
            "string:b",
            "undefined:undefined",
            "boolean:false",
            "boolean:true",
            "undefined:undefined"
        ]
    );
}

#[test]
fn exec_answers_pi_and_core_fields() {
    let mut ui = TestUi::default();
    run(
        &command(
            "const r = await oxa.exec('echo', ['out']);
             ctx.ui.notify([r.stdout.trim(), r.stderr, r.code, r.killed, r.output.trim(), r.exit, r.timedOut].join('|'));",
        ),
        &mut ui,
    );
    assert_eq!(ui.notes, vec!["out||0|false|out|0|false"]);
}

#[test]
fn set_model_refuses_an_unknown_model() {
    let mut ui = TestUi::default();
    let (_h, double) = run(
        &command(
            "try { oxa.setModel('nope'); ctx.ui.notify('set'); } catch (e) { ctx.ui.notify('refused:' + e.code); }
             ctx.ui.notify('ok:' + oxa.setModel('m2') + ':' + oxa.setModel({id: 'm1'}));",
        ),
        &mut ui,
    );
    assert_eq!(ui.notes, vec!["refused:not_found", "ok:true:true"]);
    let seen = double.seen.lock().unwrap();
    assert!(!seen.contains(&HostAction::SetModel("nope".into())));
    assert!(seen.contains(&HostAction::SetModel("m2".into())));
}

#[test]
fn omitted_apis_carry_the_unsupported_code() {
    for call in [
        "oxa.registerProvider('p', {})",
        "oxa.registerVirtualModel({id: 'v'})",
        "oxa.on('before_provider_headers', () => {})",
    ] {
        let mut ui = TestUi::default();
        run(
            &command(&format!(
                "try {{ {call}; }} catch (e) {{ ctx.ui.notify(e.code + ':' + e.message); }}"
            )),
            &mut ui,
        );
        assert!(
            ui.notes[0].starts_with("unsupported_in_oxa:"),
            "{call}: {:?}",
            ui.notes
        );
        // Uncaught at load time, the load error names the code too.
        let env = TestEnv::new();
        let host = ExtensionHost::new(env.work()).expect("runtime");
        let loaded = host.load_source("t", &ext(&format!("{call};")), &LoadOpts::default());
        let error = loaded.error.expect("the load fails");
        assert!(error.contains("unsupported_in_oxa"), "{call}: {error}");
    }
}

#[test]
fn imports_and_require_fail_with_unsupported_in_oxa() {
    let cases = [
        (
            "import fs from 'node:fs';\nexport default function (oxa) {}",
            "import fs from 'node:fs'",
        ),
        (
            "import pi from '@earendil-works/pi-ai';\nexport default function () {}",
            "@earendil-works/pi-ai",
        ),
        (
            "export default function (oxa) {\n  import fs from 'node:fs';\n}",
            "import fs from 'node:fs'",
        ),
        (
            "const fs = require('fs');\nexport default function () {}",
            "require('fs')",
        ),
        (
            "export default async function () { await import('lodash'); }",
            "import('lodash')",
        ),
    ];
    for (src, named) in cases {
        let e = check_imports("index.js", src).expect_err(src);
        assert_eq!(e.code, codes::UNSUPPORTED_IN_OXA, "{src}");
        assert!(e.message.contains(named), "{src}: {}", e.message);
        assert!(e.hint.as_deref().unwrap_or("").contains("oxa:"), "{src}");
        // Loading it gives the same error, never a QuickJS SyntaxError.
        let env = TestEnv::new();
        let host = ExtensionHost::new(env.work()).expect("runtime");
        let loaded = host.load_source("t", src, &LoadOpts::default());
        let error = loaded.error.expect("the load fails");
        assert!(
            error.contains("unsupported_in_oxa") && !error.contains("SyntaxError"),
            "{error}"
        );
    }
    // Supported module statements pass, as do words inside strings, comments and regexes.
    let fine = r#"import { Text } from "oxa:tui";
import helper from "./helper.js";
// import x from 'node:fs'
const s = "require('fs')";
const r = /import fs from 'x'/;
const t = `import y from "z"`;
const meta = import.meta;
export default function (oxa) { obj.require("x"); }
"#;
    check_imports("index.js", fine).expect("supported statements pass");
    // A relative import inside a function is not node, but still not at the top level.
    let e = check_imports("index.js", "function f() {\n import a from './a.js';\n}").unwrap_err();
    assert_eq!(e.code, codes::INVALID);
}

#[test]
fn tool_arguments_are_validated_with_pis_text() {
    let schema = json!({
        "type": "object",
        "properties": {
            "x": {"type": "string"},
            "n": {"type": "integer", "minimum": 1},
            "mode": {"type": "string", "enum": ["a", "b"]},
            "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
            "opt": {"type": "string"}
        },
        "required": ["x"],
        "additionalProperties": false
    });
    let e = validate_tool_args("need", &schema, &json!({})).unwrap_err();
    assert_eq!(
        e,
        "Validation failed for tool \"need\":\n  - x: missing\n\nReceived arguments:\n{}"
    );
    let e = validate_tool_args(
        "need",
        &schema,
        &json!({"x": 1, "n": 0, "mode": "c", "extra": true}),
    )
    .unwrap_err();
    assert!(e.contains("  - x: expected string, got integer"), "{e}");
    assert!(e.contains("  - n: expected >= 1"), "{e}");
    assert!(e.contains("  - mode: expected one of [\"a\",\"b\"]"), "{e}");
    assert!(e.contains("  - extra: unknown field"), "{e}");
    let e = validate_tool_args("need", &schema, &json!({"x": "a", "tags": ["a", 2, "c"]})).unwrap_err();
    assert!(e.contains("  - tags.1: expected string, got integer"), "{e}");
    assert!(e.contains("  - tags: expected at most 2 items"), "{e}");
    // pi coerces: a numeric string, a single value for an array, null for an optional field.
    let ok = validate_tool_args(
        "need",
        &schema,
        &json!({"x": "a", "n": "3", "tags": "t", "opt": null}),
    )
    .unwrap();
    assert_eq!(ok, json!({"x": "a", "n": 3, "tags": ["t"]}));
}

struct NoSandbox;
impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _l: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

fn run_tool(host: &ExtensionHost, name: &str, args: Value, call_id: &str) -> oxa_proto::ToolOutput {
    let env = TestEnv::new();
    let tool = Hooks::tools(host)
        .into_iter()
        .find(|t| t.spec().name == name)
        .expect("tool");
    let (cancel, cwd, spill) = (CancelToken::new(), env.work(), env.tmp());
    let vars = vec![(
        oxa_proto::names::ENV_TOOL_CALL_ID.to_string(),
        call_id.to_string(),
    )];
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &NoSandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &vars,
        tune: Default::default(),
        hidden: None,
    };
    tool.run(&args, &cx, &mut |_: &str| {})
}

#[test]
fn a_tool_never_runs_with_invalid_arguments_and_keeps_its_details() {
    let (_env, host, double) = load(
        "oxa.registerTool({name: 'need', description: 'd', executionMode: 'sequential',
            parameters: {type: 'object', properties: {x: {type: 'string'}}, required: ['x']},
            execute: async (id, p) => ({content: [{type: 'text', text: 'got ' + p.x}], details: {seen: p.x}})});",
    );
    let out = run_tool(&host, "need", json!({}), "c1");
    assert!(out.is_error);
    assert!(
        out.text
            .starts_with("Validation failed for tool \"need\":\n  - x: missing"),
        "{}",
        out.text
    );
    let out = run_tool(&host, "need", json!({"x": "v"}), "c2");
    assert_eq!((out.text.as_str(), out.is_error), ("got v", false));
    // The details are kept for the session (pi stores them on the toolResult message).
    let seen = double.seen.lock().unwrap();
    assert!(seen.contains(&HostAction::AppendEntry {
        custom_type: "oxa.toolDetails".into(),
        data: json!({"toolCallId": "c2", "toolName": "need", "details": {"seen": "v"}}),
    }));
}

/// O-175: MCP servers come from mcp.json only; the pi calls throw `unsupported_in_oxa`.
#[test]
fn mcp_server_registration_is_unsupported() {
    let (_env, host, _d) = load(
        "oxa.registerCommand('t', { handler: (a, ctx) => {
           const codes = [];
           for (const f of [() => oxa.registerMcpServer('jira', {command: 'x'}),
                            () => oxa.unregisterMcpServer('jira'),
                            () => oxa.getMcpServers()]) {
             try { f(); codes.push('accepted'); } catch (e) { codes.push(e.code); }
           }
           ctx.ui.notify(codes.join(','));
         }});",
    );
    let mut ui = TestUi::default();
    host.run_command("t", "", &mut ui).expect("runs");
    assert_eq!(
        ui.notes,
        vec!["unsupported_in_oxa,unsupported_in_oxa,unsupported_in_oxa"]
    );
}

#[test]
fn compact_callbacks_run_at_the_outcome() {
    let (_env, host, double) = load(
        "oxa.registerCommand('t', { handler: (a, ctx) => {
           ctx.compact({ customInstructions: 'keep it short',
             onComplete: (r) => ctx.ui.notify('done:' + r.summary + ':' + r.tokensBefore),
             onError: (e) => ctx.ui.notify('failed:' + e.message) });
         }});",
    );
    let mut ui = TestUi::default();
    host.run_command("t", "", &mut ui).unwrap();
    assert!(double.seen.lock().unwrap().contains(&HostAction::Compact {
        instructions: Some("keep it short".into())
    }));
    host.fire(
        HookPoint::SessionCompact,
        &json!({"summary": "S", "firstKeptEntryId": "e1", "tokensBefore": 12}),
        &mut ui,
    );
    // Each callback runs once.
    host.fire(HookPoint::SessionCompact, &json!({"summary": "again"}), &mut ui);
    host.run_command("t", "", &mut ui).unwrap();
    host.fire(
        HookPoint::SessionCompactFailed,
        &json!({"error": "no room"}),
        &mut ui,
    );
    assert_eq!(ui.notes, vec!["done:S:12", "failed:no room"]);
    assert!(host.state().take_errors().is_empty());
}

#[test]
fn a_throwing_autocomplete_provider_is_reported_once() {
    let (_env, host, _d) = load(
        "oxa.on('session_start', (e, ctx) => {
           ctx.ui.addAutocompleteProvider((p) => { if (p === 'ok') return ['fine']; throw new Error('broken'); });
         });",
    );
    let mut ui = TestUi::default();
    host.fire(HookPoint::SessionStart, &Value::Null, &mut ui);
    for prefix in ["a", "ab", "abc"] {
        assert!(host.complete(prefix).is_empty());
    }
    assert_eq!(
        host.state().take_errors().len(),
        1,
        "one error for three keystrokes"
    );
    assert_eq!(host.complete("ok"), vec!["fine"]);
    assert!(host.complete("x").is_empty());
    assert_eq!(
        host.state().take_errors().len(),
        1,
        "a new failure after a success is reported"
    );
}

#[test]
fn custom_takes_pis_factory_signature() {
    let (_env, host, _d) = load(
        "oxa.registerCommand('t', { handler: async (a, ctx) => {
           const v = await ctx.ui.custom((tui, theme, kb, done) => ({
             render: (w) => [theme.fg('accent', theme.bold('title')), 'keys ' + kb.getKeys('tui.select.confirm').join(',')],
             handleInput: (data) => {
               if (kb.matches(data, 'tui.select.confirm')) done('picked');
               else tui.requestRender();
             },
           }));
           ctx.ui.notify('custom=' + v);
         }});",
    );
    let mut ui = TestUi {
        interactive: true,
        keys: VecDeque::from([("down", None), ("enter", None)]),
        ..TestUi::default()
    };
    // Each call captures fresh functions; after the call they are released.
    host.run_command("t", "", &mut ui).unwrap();
    assert_eq!(ui.renders[0], vec!["title", "keys enter"]);
    assert_eq!(
        ui.actions,
        vec![ComponentAction::Redraw, ComponentAction::Done(json!("picked"))],
        "requestRender redraws"
    );
    assert_eq!(ui.notes, vec!["custom=picked"]);
    let before = host.state().take_errors();
    assert!(before.is_empty(), "{before:?}");
}

#[test]
fn tui_shim_has_settings_list_editor_and_wrap() {
    let (_env, host, _d) = load(
        "oxa.registerCommand('t', { handler: async (a, ctx) => {
           const changes = [];
           const list = new SettingsList(
             [{id: 'a', label: 'alpha', currentValue: 'on', values: ['on', 'off'], description: 'the first'},
              {id: 'b', label: 'b', currentValue: 'x', values: ['x', 'y']}],
             5, undefined, (id, v) => changes.push(id + '=' + v), () => changes.push('cancel'));
           const first = list.render(60);
           list.handleInput('enter', null);
           list.handleInput('down', null);
           list.handleInput('space', ' ');
           list.handleInput('escape', null);
           const ed = new Editor();
           let submitted = null;
           ed.onSubmit = (t) => { submitted = t; };
           for (const ch of 'hi') ed.handleInput(ch, ch);
           ed.handleInput('shift+enter', null);
           ed.handleInput('x', 'x');
           ed.handleInput('backspace', null);
           ed.handleInput('y', 'y');
           ed.handleInput('enter', null);
           ctx.ui.notify(JSON.stringify({first, changes, submitted,
             wrap: wrapTextWithAnsi('one two three\\nfour', 7)}));
         }});",
    );
    let mut ui = TestUi::default();
    host.run_command("t", "", &mut ui).unwrap();
    let got: Value = serde_json::from_str(&ui.notes[0]).unwrap();
    assert_eq!(
        got["first"],
        json!([
            "→ alpha  on",
            "  b      x",
            "",
            "  the first",
            "",
            "  Enter/Space to change · Esc to cancel"
        ])
    );
    assert_eq!(got["changes"], json!(["a=off", "b=y", "cancel"]));
    assert_eq!(got["submitted"], json!("hi\ny"));
    assert_eq!(got["wrap"], json!(["one two", "three", "four"]));
}

#[test]
fn model_and_execute_tool() {
    let mut ui = TestUi::default();
    run(
        &command(
            "ctx.ui.notify(ctx.model.id + '/' + ctx.model.provider);
             const ok = ctx.executeTool('ls', {});
             const bad = ctx.executeTool('nope');
             ctx.ui.notify(ok.content[0].text + ':' + ok.isError + '|' + bad.isError + ':' + bad.content[0].text);",
        ),
        &mut ui,
    );
    assert_eq!(ui.notes, vec!["m1/oxe", "a.txt:false|true:no tool named nope"]);
}
