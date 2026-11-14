//! F1 acceptance tests: Extension UI setters. Owner: task F1.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_ext::{api_ui, ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{Access, Answer, HeadlessUi, HookPoint, HookReply, Hooks, Level, Placement, Ui, Value};
use serde_json::json;

/// A module whose default export installs `body` (the factory gets `oxa`).
fn ext(body: &str) -> String {
    format!("export default function (oxa) {{ {body} }}")
}

/// A host with a working directory under a fresh test environment.
fn host() -> (TestEnv, ExtensionHost) {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("start the extension runtime");
    (env, host)
}

/// A UI that records every widget call as one line of text.
#[derive(Default)]
struct RecUi {
    calls: Vec<String>,
    editor: String,
    notes: Vec<(Level, String)>,
}

impl Ui for RecUi {
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
    fn set_status(&mut self, key: &str, text: Option<&str>) {
        self.calls
            .push(format!("status {key}={}", text.unwrap_or("<none>")));
    }
    fn set_widget(&mut self, key: &str, lines: Option<&[String]>, at: Placement) {
        self.calls.push(format!("widget {key} {lines:?} {at:?}"));
    }
    fn set_title(&mut self, t: &str) {
        self.calls.push(format!("title {t}"));
    }
    fn set_editor_text(&mut self, t: &str) {
        self.editor = t.to_string();
    }
    fn ask_access(&mut self, _a: &Access) -> Answer {
        Answer::Deny
    }
    fn editor_text(&mut self) -> String {
        self.editor.clone()
    }
    fn set_footer(&mut self, lines: Option<&[String]>) {
        self.calls.push(format!("footer {lines:?}"));
    }
    fn set_header(&mut self, lines: Option<&[String]>) {
        self.calls.push(format!("header {lines:?}"));
    }
    fn set_working_message(&mut self, text: Option<&str>) {
        self.calls.push(format!("working {text:?}"));
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// Load one extension whose `input` handler runs `body` (with `ctx` in scope), then fire
/// `input` once. Returns the reply.
fn run_input(host: &ExtensionHost, name: &str, body: &str, ui: &mut dyn Ui) -> HookReply {
    let src = ext(&format!(r#"oxa.on("input", (e, ctx) => {{ {body} }});"#));
    let loaded = host.load_source(name, &src, &LoadOpts::default());
    assert!(loaded.error.is_none(), "{name} must load: {:?}", loaded.error);
    host.fire(HookPoint::Input, &json!({ "text": "x" }), ui)
}

/// Ui calls
#[test]
fn set_status_widget_footer_header_title() {
    let (_env, host) = host();
    let mut ui = RecUi::default();
    let reply = run_input(
        &host,
        "widgets",
        r#"
        ctx.ui.setStatus("git", "main");
        ctx.ui.setWidget("todo", ["one", "two"], { placement: "belowEditor" });
        ctx.ui.setWidget("tip", ["x"]);
        ctx.ui.setFooter(["footer"]);
        ctx.ui.setHeader(["header"]);
        ctx.ui.setTitle("oxa: project");
        ctx.ui.setStatus("git", undefined);
        ctx.ui.setWidget("todo", undefined);
        ctx.ui.setFooter(null);
        "#,
        &mut ui,
    );
    assert_eq!(reply, HookReply::Continue);
    assert!(host.state().take_errors().is_empty());
    assert!(ui.notes.is_empty(), "no notes expected: {:?}", ui.notes);
    assert_eq!(
        ui.calls,
        vec![
            "status git=main".to_string(),
            r#"widget todo Some(["one", "two"]) BelowEditor"#.to_string(),
            r#"widget tip Some(["x"]) AboveEditor"#.to_string(),
            r#"footer Some(["footer"])"#.to_string(),
            r#"header Some(["header"])"#.to_string(),
            "title oxa: project".to_string(),
            "status git=<none>".to_string(),
            "widget todo None AboveEditor".to_string(),
            "footer None".to_string(),
        ]
    );
}

/// Hooks::complete
#[test]
fn autocomplete_provider() {
    let (_env, host) = host();
    let opts = LoadOpts::default();
    // Providers are registered while the factory runs, through the ctx of the glue.
    let a = host.load_source(
        "a",
        &ext(r#"__oxa_make_ctx().ui.addAutocompleteProvider((p) => ["alpha", "alps"].filter((x) => x.startsWith(p)));"#),
        &opts,
    );
    let b = host.load_source(
        "b",
        &ext(r#"__oxa_make_ctx().ui.addAutocompleteProvider((p) => { throw new Error("boom " + p); });"#),
        &opts,
    );
    let c = host.load_source(
        "c",
        &ext(r#"__oxa_make_ctx().ui.addAutocompleteProvider(async (p) => ["async-" + p]);"#),
        &opts,
    );
    let d = host.load_source(
        "d",
        &ext(r#"__oxa_make_ctx().ui.addAutocompleteProvider((p) => [1, "ok"]);"#),
        &opts,
    );
    for loaded in [&a, &b, &c, &d] {
        assert!(
            loaded.error.is_none(),
            "{} must load: {:?}",
            loaded.path,
            loaded.error
        );
    }

    // Registration order, concatenated; the throwing provider contributes nothing.
    assert_eq!(host.complete("al"), vec!["alpha", "alps", "async-al", "ok"]);
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "one failing provider, one error: {errors:?}");
    assert_eq!(errors[0].extension_path, "builtin:b");
    assert_eq!(errors[0].event, "autocomplete");
    assert!(errors[0].error.contains("boom al"), "{}", errors[0].error);

    // A non-function provider is refused at load time.
    let bad = host.load_source(
        "bad",
        &ext("__oxa_make_ctx().ui.addAutocompleteProvider(5);"),
        &opts,
    );
    assert!(bad.error.is_some(), "a non-function provider must fail the load");
    // The failed extension registers nothing.
    assert_eq!(host.complete("al"), vec!["alpha", "alps", "async-al", "ok"]);
}

/// set and restore
#[test]
fn working_message() {
    let (_env, host) = host();
    let mut ui = RecUi::default();
    run_input(
        &host,
        "working",
        r#"
        ctx.ui.setWorkingMessage("Reviewing");
        ctx.ui.setWorkingMessage(undefined);
        ctx.ui.setWorkingMessage("Thinking");
        ctx.ui.setWorkingMessage(null);
        "#,
        &mut ui,
    );
    assert!(host.state().take_errors().is_empty());
    assert_eq!(
        ui.calls,
        vec![
            r#"working Some("Reviewing")"#.to_string(),
            "working None".to_string(),
            r#"working Some("Thinking")"#.to_string(),
            "working None".to_string(),
        ]
    );
}

/// no throw
#[test]
fn headless_ui_calls_are_noops() {
    let (_env, host) = host();
    let mut ui = HeadlessUi::default();
    let reply = run_input(
        &host,
        "headless",
        r#"
        ctx.ui.setStatus("k", "v");
        ctx.ui.setWidget("w", ["a"], { placement: "belowEditor" });
        ctx.ui.setWidget("w");
        ctx.ui.setFooter(["f"]);
        ctx.ui.setHeader(["h"]);
        ctx.ui.setTitle("t");
        ctx.ui.setWorkingMessage("busy");
        ctx.ui.setWorkingMessage(undefined);
        ctx.ui.setEditorText("typed");
        if (typeof ctx.ui.getEditorText() !== "string") { throw new Error("no editor text"); }
        ctx.ui.addAutocompleteProvider(() => []);
        "#,
        &mut ui,
    );
    assert_eq!(reply, HookReply::Continue);
    assert!(host.state().take_errors().is_empty());
    assert!(
        ui.notes.is_empty(),
        "headless setters must not notify: {:?}",
        ui.notes
    );
}

/// The editor text is read back through the caller's UI.
#[test]
fn editor_text_round_trip() {
    let (_env, host) = host();
    let mut ui = RecUi::default();
    let reply = run_input(
        &host,
        "editor",
        r#"
        ctx.ui.setEditorText("hello");
        return { action: "transform", text: ctx.ui.getEditorText() + "!" };
        "#,
        &mut ui,
    );
    assert_eq!(reply, HookReply::ReplaceInput("hello!".to_string()));
    assert!(host.state().take_errors().is_empty());
}

/// A widget placement other than `belowEditor` is above the editor; a missing options object is
/// above too.
#[test]
fn widget_placement_defaults_above() {
    let (_env, host) = host();
    let mut ui = RecUi::default();
    run_input(
        &host,
        "placement",
        r#"
        ctx.ui.setWidget("p1", ["a"], { placement: "aboveEditor" });
        ctx.ui.setWidget("p2", ["b"], { placement: "somewhere" });
        ctx.ui.setWidget("p3", "single line");
        "#,
        &mut ui,
    );
    assert!(host.state().take_errors().is_empty());
    assert_eq!(
        ui.calls,
        vec![
            r#"widget p1 Some(["a"]) AboveEditor"#.to_string(),
            r#"widget p2 Some(["b"]) AboveEditor"#.to_string(),
            r#"widget p3 Some(["single line"]) AboveEditor"#.to_string(),
        ]
    );
}

/// `serve` answers the widget upcalls only; anything else is not ours.
#[test]
fn serve_answers_only_widget_ops() {
    let mut ui = RecUi::default();
    assert_eq!(
        api_ui::serve(&json!({"op": "set_status", "key": "k", "text": null}), &mut ui),
        Some(Value::Null)
    );
    assert_eq!(ui.calls, vec!["status k=<none>".to_string()]);
    assert_eq!(
        api_ui::serve(&json!({"op": "get_editor_text"}), &mut ui),
        Some(json!(""))
    );
    assert_eq!(
        api_ui::serve(&json!({"op": "select", "options": []}), &mut ui),
        None
    );
    assert_eq!(api_ui::serve(&json!({"op": "unknown"}), &mut ui), None);
    assert_eq!(api_ui::serve(&json!({"no": "op"}), &mut ui), None);
}
