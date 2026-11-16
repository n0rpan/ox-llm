//! F2 acceptance tests: Custom UI components.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_ext::{ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{
    Access, Answer, ComponentAction, CustomOpts, HeadlessUi, HookPoint, HookReply, Hooks, KeyId, Level,
    Placement, Ui, UiComponent, Value,
};
use serde_json::json;

/// Module-level imports of the shim test (imports cannot sit inside the factory).
const IMPORTS: &str =
    r#"import { Text, Box, Container, Spacer, SelectList, Input, matchesKey } from "oxa:tui";"#;

/// Wrap a body as an extension module whose factory registers things.
fn ext(body: &str) -> String {
    format!("export default function (oxa) {{ {body} }}")
}

/// A host with a working directory under a fresh test environment.
fn host() -> (TestEnv, ExtensionHost) {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("start the extension runtime");
    (env, host)
}

/// A UI that feeds scripted keys to the components it is asked to show. Records what it saw.
#[derive(Default)]
struct FakeUi {
    /// Keys fed to each shown component, in order: (key id, typed text).
    keys: Vec<(&'static str, Option<&'static str>)>,
    /// The width the component is rendered at.
    width: u16,
    renders: Vec<Vec<String>>,
    actions: Vec<ComponentAction>,
    opts: Vec<CustomOpts>,
    /// What each `custom` call returned: `Some(value)` when the component finished, `None` when
    /// it was dismissed (no key finished it).
    closed_with: Vec<Option<Value>>,
    notes: Vec<(Level, String)>,
}

impl Ui for FakeUi {
    fn select(&mut self, _title: &str, _options: &[String], _timeout_ms: Option<u32>) -> Option<usize> {
        None
    }
    fn confirm(&mut self, _title: &str, _body: &str, _timeout_ms: Option<u32>) -> Option<bool> {
        None
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
        true
    }
    fn custom(&mut self, mut c: Box<dyn UiComponent>, opts: CustomOpts) -> Option<Value> {
        self.opts.push(opts);
        self.renders.push(c.render(self.width));
        for (key, text) in self.keys.clone() {
            let key = KeyId::parse(key).expect("valid key id");
            let action = c.input(&key, text);
            self.actions.push(action.clone());
            if let ComponentAction::Done(v) = action {
                self.closed_with.push(Some(v.clone()));
                return Some(v);
            }
        }
        self.closed_with.push(None);
        None
    }
}

/// Visible columns of a line: SGR sequences cost nothing, East Asian wide characters two.
fn cols(line: &str) -> usize {
    let mut n = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for x in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&x) {
                        break;
                    }
                }
            }
            continue;
        }
        let cp = c as u32;
        n += if (0x2e80..=0xa4cf).contains(&cp) { 2 } else { 1 };
    }
    n
}

/// lines fit the width
#[test]
fn js_component_render_width() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "width",
        &ext(r#"
            oxa.registerCommand("show", { description: "show", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({
                    render: (w) => [
                        "x".repeat(500),
                        "short",
                        "\u001b[31m" + "漢".repeat(30) + "\u001b[0m",
                        "é".repeat(40),
                    ],
                }));
            } });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = FakeUi {
        width: 20,
        ..FakeUi::default()
    };
    host.run_command("show", "", &mut ui).expect("the command runs");
    let lines = &ui.renders[0];
    assert_eq!(lines.len(), 4);
    for line in lines {
        assert!(cols(line) <= 20, "{line:?} is {} columns", cols(line));
    }
    assert_eq!(lines[1], "short");
    assert!(lines[2].starts_with("\u{1b}[31m"), "colour kept: {:?}", lines[2]);
    assert!(
        lines[2].ends_with("\u{1b}[0m"),
        "a cut coloured line is reset: {:?}",
        lines[2]
    );
    assert_eq!(cols(&lines[2]), 20);
    assert_eq!(cols(&lines[3]), 20);
}

/// A component whose render throws shows one line with the error and stays open.
#[test]
fn render_throw_keeps_component_open() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "render_throw",
        &ext(r#"
            oxa.registerCommand("show", { description: "show", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({ render: () => { throw new Error("boom"); } }));
            } });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = FakeUi {
        width: 40,
        ..FakeUi::default()
    };
    host.run_command("show", "", &mut ui).expect("the command runs");
    assert_eq!(ui.renders.len(), 1);
    assert_eq!(ui.renders[0].len(), 1);
    assert!(ui.renders[0][0].contains("boom"), "{:?}", ui.renders[0]);
    assert_eq!(ui.closed_with, vec![None], "dismissed, not closed by an error");
}

/// ComponentAction mapping
#[test]
fn handle_input_and_done() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "keys",
        &ext(r#"
            oxa.registerCommand("keys", { description: "keys", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({
                    render: () => ["keys"],
                    handleInput: (key, text) => {
                        if (key === "escape") return { done: "cancelled" };
                        if (key === "enter") return true;
                        if (text === "a") return undefined;
                        return 42;
                    },
                }));
            } });
            oxa.registerCommand("via_done", { description: "done callback", handler: (args, ctx) => {
                return ctx.ui.custom((tui, theme, keybindings, done) => ({
                    render: () => ["via done"],
                    handleInput: () => { done("via-done"); },
                }));
            } });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);

    let mut ui = FakeUi {
        width: 20,
        keys: vec![
            ("enter", None),
            ("a", Some("a")),
            ("up", None),
            ("escape", None),
            ("enter", None),
        ],
        ..FakeUi::default()
    };
    host.run_command("keys", "", &mut ui).expect("the command runs");
    assert_eq!(
        ui.actions,
        vec![
            ComponentAction::Redraw,
            ComponentAction::Ignored,
            ComponentAction::Ignored,
            ComponentAction::Done(json!("cancelled")),
        ],
        "the last key is never delivered once the component is done"
    );
    assert_eq!(ui.closed_with, vec![Some(json!("cancelled"))]);

    // `done(value)` called from a key handler closes the component with that value.
    let mut ui = FakeUi {
        width: 20,
        keys: vec![("enter", None)],
        ..FakeUi::default()
    };
    host.run_command("via_done", "", &mut ui)
        .expect("the command runs");
    assert_eq!(ui.actions, vec![ComponentAction::Done(json!("via-done"))]);
}

/// CustomOpts.overlay
#[test]
fn overlay() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "overlay",
        &ext(r#"
            oxa.registerCommand("over", { description: "over", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({ render: () => ["o"] }), { overlay: true });
            } });
            oxa.registerCommand("plain", { description: "plain", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({ render: () => ["p"] }));
            } });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = FakeUi {
        width: 10,
        ..FakeUi::default()
    };
    host.run_command("over", "", &mut ui).expect("over runs");
    host.run_command("plain", "", &mut ui).expect("plain runs");
    assert_eq!(
        ui.opts,
        vec![
            CustomOpts {
                overlay: true,
                as_editor: false
            },
            CustomOpts::default(),
        ]
    );
    assert_eq!(ui.renders, vec![vec!["o".to_string()], vec!["p".to_string()]]);
}

/// as_editor
#[test]
fn custom_editor() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "editor",
        &ext(r#"
            oxa.registerCommand("edit", { description: "edit", handler: (args, ctx) => {
                ctx.ui.setEditorComponent(() => ({
                    render: () => ["> editor"],
                    handleInput: () => true,
                }));
            } });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = FakeUi {
        width: 20,
        ..FakeUi::default()
    };
    host.run_command("edit", "", &mut ui).expect("the command runs");
    assert_eq!(
        ui.opts,
        vec![CustomOpts {
            overlay: false,
            as_editor: true
        }]
    );
    assert_eq!(ui.renders, vec![vec!["> editor".to_string()]]);
}

/// Text, Box, Container, SelectList usable from JS
#[test]
fn pi_tui_class_shim() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "shim",
        &format!("{IMPORTS}\n{}", ext(r#"
            oxa.registerCommand("shim", { description: "shim", handler: (args, ctx) => {
                return ctx.ui.custom(() => {
                    const root = new Container();
                    const box = new Box(1, 0);
                    box.addChild(new Text("hello world from the shim", 0, 0));
                    root.addChild(box);
                    const list = new SelectList([
                        { value: "a", label: "Alpha", description: "first" },
                        { value: "b", label: "Beta" },
                    ], 5);
                    const input = new Input();
                    root.addChild(list);
                    root.addChild(new Spacer(1));
                    root.addChild(input);
                    return {
                        render: (w) => root.render(w),
                        handleInput: (key, text) => {
                            if (matchesKey(key, "down")) { list.handleInput(key); return true; }
                            if (matchesKey(key, "enter")) {
                                return { done: { selected: list.getSelectedItem().value, typed: input.getValue() } };
                            }
                            input.handleInput(key, text);
                            return true;
                        },
                    };
                });
            } });
        "#)),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = FakeUi {
        width: 30,
        keys: vec![
            ("down", None),
            ("x", Some("x")),
            ("y", Some("y")),
            ("enter", None),
        ],
        ..FakeUi::default()
    };
    host.run_command("shim", "", &mut ui).expect("the command runs");
    let first = &ui.renders[0];
    assert!(
        first.iter().any(|l| l == " hello world from the shim"),
        "{first:?}"
    );
    assert!(first.iter().any(|l| l == "→ Alpha  first"), "{first:?}");
    assert!(first.iter().any(|l| l == "  Beta"), "{first:?}");
    assert!(first.iter().any(|l| l == "> |"), "{first:?}");
    assert_eq!(
        ui.closed_with,
        vec![Some(json!({"selected": "b", "typed": "xy"}))],
        "down moves the selection, typing goes to the Input, enter finishes"
    );
}

/// ExtensionError
#[test]
fn component_throw_closes_cleanly() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "boom",
        &ext(r#"
            oxa.registerCommand("boom", { description: "boom", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({
                    render: () => ["x"],
                    handleInput: () => { throw new Error("boom"); },
                }));
            } });
            oxa.on("input", (e, ctx) => {
                return ctx.ui.custom(() => ({
                    render: () => ["x"],
                    handleInput: () => { throw new Error("boom"); },
                }));
            });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);

    // A command: the throw closes the component and fails the command.
    let mut ui = FakeUi {
        width: 20,
        keys: vec![("enter", None)],
        ..FakeUi::default()
    };
    let err = host
        .run_command("boom", "", &mut ui)
        .expect_err("the component's error reaches the command");
    assert!(err.message.contains("boom"), "{err:?}");
    assert_eq!(ui.closed_with.len(), 1, "the component was closed, not left open");
    let closed = ui.closed_with[0].clone().expect("closed with an error value");
    assert_eq!(closed["$error"]["name"], json!("ExtensionError"));
    assert!(closed["$error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("boom"));

    // An event handler: the error is an extension error, reported and the chain goes on.
    let mut ui = FakeUi {
        width: 20,
        keys: vec![("enter", None)],
        ..FakeUi::default()
    };
    let reply = host.fire(HookPoint::Input, &json!({"text": "x"}), &mut ui);
    assert_eq!(reply, HookReply::Continue);
    assert_eq!(ui.closed_with.len(), 1);
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].error.contains("boom"), "{errors:?}");
    // Reported as an `extension_error` event by the core (it drains `Hooks::take_errors`),
    // not as a UI notification (pi: rpc-mode `onError` writes `extension_error`).
    assert!(
        !ui.notes.iter().any(|(_, text)| text.contains("boom")),
        "{:?}",
        ui.notes
    );
}

/// Without a UI that can show components (print mode), `ctx.ui.custom` resolves to null.
#[test]
fn no_component_ui_answers_null() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "headless",
        &ext(r#"
            oxa.registerCommand("show", { description: "show", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({ render: () => ["x"] })).then((v) => {
                    if (v !== null) throw new Error("expected null, got " + v);
                });
            } });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = HeadlessUi::default();
    host.run_command("show", "", &mut ui).expect("the command runs");
}

/// A factory without render(width) is refused as an extension error.
#[test]
fn component_without_render_is_refused() {
    let (_env, host) = host();
    let loaded = host.load_source(
        "norender",
        &ext(r#"
            oxa.registerCommand("show", { description: "show", handler: (args, ctx) => {
                return ctx.ui.custom(() => ({}));
            } });
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = FakeUi {
        width: 20,
        ..FakeUi::default()
    };
    let err = host
        .run_command("show", "", &mut ui)
        .expect_err("a component needs render(width)");
    assert!(err.message.contains("render(width)"), "{err:?}");
    assert!(ui.opts.is_empty(), "nothing was shown");
}
