//! D5 acceptance tests: Tool views.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_interactive::views::{collapse_lines, render_diff, render_tool, tool_header, tool_result_lines};
use oxa_interactive::{ToolBlock, ToolState, ViewCx, ViewSettings};
use oxa_proto::{HookPoint, HookReply, Hooks, RenderKind, Tool, ToolOutput, Value};
use oxa_tui::{builtin_theme, ColorMode, TermCaps, Theme};
use serde_json::json;
use std::sync::Arc;

/// A plain-theme context at 80 columns with the cwd `/work/proj`.
fn cx<'a>(theme: &'a Theme, settings: &'a ViewSettings, hooks: &'a [Arc<dyn Hooks>]) -> ViewCx<'a> {
    ViewCx {
        width: 80,
        theme,
        settings,
        caps: TermCaps::default(),
        hooks,
        cwd: "/work/proj",
        home: "/home/u",
        keymap: None,
    }
}

fn block(name: &str, args: Value, state: ToolState) -> ToolBlock {
    ToolBlock {
        call_id: "c1".to_string(),
        name: name.to_string(),
        args,
        args_raw: String::new(),
        state,
        partial: String::new(),
        result: None,
        duration_ms: None,
        expanded: false,
    }
}

fn colored_theme() -> Theme {
    builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme")
}

/// A hook set with one extension renderer for the `mcp__demo__stat` tool.
struct DemoHooks;

impl Hooks for DemoHooks {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn oxa_proto::Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<oxa_proto::CommandSpec> {
        Vec::new()
    }
    fn run_command(
        &self,
        _name: &str,
        _args: &str,
        _ui: &mut dyn oxa_proto::Ui,
    ) -> Result<(), oxa_proto::OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn render(&self, kind: RenderKind, payload: &Value, _width: u16) -> Option<Vec<String>> {
        match kind {
            RenderKind::ToolCall if payload["toolName"] == "mcp__demo__stat" => {
                Some(vec!["stat (extension view)".to_string()])
            }
            RenderKind::ToolResult if payload["toolName"] == "mcp__demo__stat" => Some(vec![format!(
                "extension saw {}",
                payload["result"].as_str().unwrap_or("")
            )]),
            _ => None,
        }
    }
}

/// read, bash, edit, write, grep, find, ls headers
#[test]
fn per_tool_call_renderers() {
    let theme = Theme::plain();
    let settings = ViewSettings::default();
    let hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let cx = cx(&theme, &settings, &hooks);

    let h = |name: &str, args: Value| tool_header(name, &args, &cx);
    assert_eq!(
        h(
            "read",
            json!({"path": "/work/proj/src/a.rs", "offset": 10, "limit": 5})
        )
        .as_deref(),
        Some("read src/a.rs:10-14")
    );
    assert_eq!(h("read", json!({"path": "b.txt"})).as_deref(), Some("read b.txt"));
    // The home directory comes from the context (never the process environment) and shows as `~`.
    assert_eq!(
        h("read", json!({"path": "/home/u/notes.md"})).as_deref(),
        Some("read ~/notes.md")
    );
    assert_eq!(
        h("bash", json!({"command": "ls -la"})).as_deref(),
        Some("$ ls -la")
    );
    assert_eq!(
        h("edit", json!({"path": "/work/proj/x.txt"})).as_deref(),
        Some("edit x.txt")
    );
    assert_eq!(
        h("write", json!({"path": "/work/proj/y.txt", "content": "z"})).as_deref(),
        Some("write y.txt")
    );
    assert_eq!(
        h("grep", json!({"pattern": "TODO", "path": "/work/proj/src"})).as_deref(),
        Some("grep TODO in src")
    );
    assert_eq!(
        h("find", json!({"pattern": "*.rs"})).as_deref(),
        Some("find *.rs")
    );
    assert_eq!(h("ls", json!({})).as_deref(), Some("ls ."));
    assert_eq!(
        h("mcp__demo__stat", json!({})),
        None,
        "unknown tools have no built-in header"
    );

    // A whole bash block: header, output, no footer for exit 0.
    let mut t = block("bash", json!({"command": "echo hi"}), ToolState::Done);
    t.result = Some(ToolOutput {
        text: "hi".into(),
        is_error: false,
        ui: json!({"exit": 0, "timedOut": false}),
    });
    let lines = render_tool(&t, &cx);
    assert_eq!(lines, vec![" $ echo hi".to_string(), " hi".to_string()]);
}

/// edit result
#[test]
fn diff_view() {
    let theme = Theme::plain();
    let settings = ViewSettings::default();
    let hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let cx = cx(&theme, &settings, &hooks);

    let diff = "-1 alpha\n+1 beta\n 2 same\n";
    assert_eq!(render_diff(diff, 80, &cx), vec!["-1 alpha", "+1 beta", " 2 same"]);

    // Tabs become three spaces.
    assert_eq!(render_diff("+1 \tx\n", 80, &cx), vec!["+1    x"]);

    // An edit result renders its ui diff, not the message text.
    let lines = tool_result_lines(
        "edit",
        &json!({"path": "f.txt"}),
        "Successfully edited",
        false,
        &json!({"diff": diff}),
        &cx,
    );
    assert_eq!(lines, vec!["-1 alpha", "+1 beta", " 2 same"]);

    // With colour: added and removed use their diff tokens, the changed word is inverse.
    let colored = colored_theme();
    let ccx = self::cx(&colored, &settings, &hooks);
    let out = render_diff(diff, 80, &ccx);
    assert_eq!(out.len(), 3);
    assert!(
        out[0].contains(&colored.fg_code("toolDiffRemoved")),
        "removed colour: {:?}",
        out[0]
    );
    assert!(
        out[1].contains(&colored.fg_code("toolDiffAdded")),
        "added colour: {:?}",
        out[1]
    );
    assert!(
        out[0].contains("\x1b[7m"),
        "intra-line emphasis on the changed words: {:?}",
        out[0]
    );
    assert!(
        out[2].contains(&colored.fg_code("toolDiffContext")),
        "context colour: {:?}",
        out[2]
    );
}

/// ctrl+o
#[test]
fn expand_collapse() {
    let theme = Theme::plain();
    let hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let settings = ViewSettings {
        collapsed_tool_lines: 10,
        ..ViewSettings::default()
    };
    let cx = cx(&theme, &settings, &hooks);

    let text: String = (1..=30).map(|i| format!("row{i}\n")).collect();
    let mut t = block("grep", json!({"pattern": "row"}), ToolState::Done);
    t.result = Some(ToolOutput::ok(text.clone()));

    // Collapsed: header, ten lines and the hint.
    let lines = render_tool(&t, &cx);
    assert_eq!(lines.len(), 1 + 10 + 1, "{lines:?}");
    assert_eq!(
        lines.last().map(String::as_str),
        Some(" ... (20 more lines, ctrl+o to expand)")
    );

    // The block toggle expands it.
    t.expanded = true;
    let lines = render_tool(&t, &cx);
    assert_eq!(lines.len(), 1 + 30, "{lines:?}");
    assert_eq!(lines.last().map(String::as_str), Some(" row30"));

    // The global toggle expands every block.
    t.expanded = false;
    let mut all = settings.clone();
    all.tools_expanded = true;
    let acx = self::cx(&theme, &all, &hooks);
    assert_eq!(render_tool(&t, &acx).len(), 1 + 30);

    // collapse_lines: first lines or last lines, and whether it cut.
    let v: Vec<String> = (1..=5).map(|i| i.to_string()).collect();
    let (first, cut) = collapse_lines(v.clone(), 2, false, "k", &cx);
    assert!(cut);
    assert_eq!(first, vec!["1", "2", "... (3 more lines, k to expand)"]);
    let (last, cut) = collapse_lines(v.clone(), 2, true, "k", &cx);
    assert!(cut);
    assert_eq!(last, vec!["... (3 more lines, k to expand)", "4", "5"]);
    let (same, cut) = collapse_lines(v.clone(), 5, true, "k", &cx);
    assert!(!cut);
    assert_eq!(same, v);
}

/// partial updates
#[test]
fn live_output() {
    let theme = Theme::plain();
    let hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let settings = ViewSettings::default();
    let cx = cx(&theme, &settings, &hooks);

    // A running bash shows the last lines of its partial output.
    let mut t = block("bash", json!({"command": "seq 30"}), ToolState::Running);
    t.partial = (1..=30).map(|i| format!("line{i}\n")).collect();
    let lines = render_tool(&t, &cx);
    assert_eq!(lines[1], " ... (20 more lines, ctrl+o to expand)");
    assert_eq!(lines.last().map(String::as_str), Some(" line30"));
    assert!(!lines.iter().any(|l| l == " line1"));

    // A later partial replaces the earlier view.
    t.partial.push_str("line31\n");
    let lines = render_tool(&t, &cx);
    assert_eq!(lines.last().map(String::as_str), Some(" line31"));

    // Nothing yet while running: a placeholder.
    let t = block("bash", json!({"command": "x"}), ToolState::Running);
    assert_eq!(
        render_tool(&t, &cx).last().map(String::as_str),
        Some(" running...")
    );

    // Streaming: the header is drawn from the partial arguments, never a panic.
    let mut s = block("read", Value::Null, ToolState::Streaming);
    s.args_raw = r#"{"path": "/work/proj/src/mai"#.to_string();
    assert_eq!(render_tool(&s, &cx), vec![" read src/mai".to_string()]);
    s.args_raw = r#"{"pa"#.to_string();
    assert_eq!(render_tool(&s, &cx), vec![" read ...".to_string()]);
}

/// is_error styling
#[test]
fn error_state() {
    let theme = colored_theme();
    let hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let settings = ViewSettings::default();
    let cx = cx(&theme, &settings, &hooks);
    let err_bg = theme.bg_code("toolErrorBg");
    let ok_bg = theme.bg_code("toolSuccessBg");
    assert_ne!(err_bg, ok_bg, "the theme must tell error and success apart");

    // An error result: error background and error text.
    let mut t = block("edit", json!({"path": "f.txt"}), ToolState::Error);
    t.result = Some(ToolOutput::error("file not found"));
    let lines = render_tool(&t, &cx);
    assert!(lines.iter().all(|l| l.contains(&err_bg)), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains(&theme.fg_code("error"))),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("file not found")));

    // A tool that reports `is_error` while Done is styled as an error too.
    let mut d = block("ls", json!({}), ToolState::Done);
    d.result = Some(ToolOutput::error("denied"));
    assert!(render_tool(&d, &cx).iter().all(|l| l.contains(&err_bg)));

    // A successful Done block is on the success background.
    let mut ok = block("ls", json!({}), ToolState::Done);
    ok.result = Some(ToolOutput::ok("a.txt"));
    assert!(render_tool(&ok, &cx)
        .iter()
        .all(|l| l.contains(&ok_bg) && !l.contains(&err_bg)));

    // A pending call is on the pending background.
    let pending = block("ls", json!({}), ToolState::Pending);
    let pending_bg = theme.bg_code("toolPendingBg");
    assert!(render_tool(&pending, &cx).iter().all(|l| l.contains(&pending_bg)));

    // A failed bash exit is shown.
    let mut b = block("bash", json!({"command": "false"}), ToolState::Done);
    b.result = Some(ToolOutput {
        text: String::new(),
        is_error: false,
        ui: json!({"exit": 3, "timedOut": false}),
    });
    let plain = Theme::plain();
    let pcx = self::cx(&plain, &settings, &hooks);
    assert!(render_tool(&b, &pcx).iter().any(|l| l == " exit 3"));
}

/// MCP / extension tools
#[test]
fn unknown_tool_generic_view() {
    let theme = Theme::plain();
    let settings = ViewSettings::default();
    let no_hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let cx = cx(&theme, &settings, &no_hooks);

    // No renderer: name, pretty JSON arguments, then the text result.
    let mut t = block("mcp__files__stat", json!({"path": "x"}), ToolState::Done);
    t.result = Some(ToolOutput::ok("size 10"));
    assert_eq!(
        render_tool(&t, &cx),
        vec![
            " mcp__files__stat".to_string(),
            " {".to_string(),
            "   \"path\": \"x\"".to_string(),
            " }".to_string(),
            " size 10".to_string(),
        ]
    );

    // An extension renderer wins for both the call and the result.
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(DemoHooks)];
    let hcx = self::cx(&theme, &settings, &hooks);
    let mut h = block("mcp__demo__stat", json!({"path": "x"}), ToolState::Done);
    h.result = Some(ToolOutput::ok("10 bytes"));
    assert_eq!(
        render_tool(&h, &hcx),
        vec![
            " stat (extension view)".to_string(),
            " extension saw 10 bytes".to_string()
        ]
    );
}

#[test]
fn write_result_shows_summary_and_added_lines() {
    let theme = Theme::plain();
    let settings = ViewSettings::default();
    let hooks: Vec<Arc<dyn Hooks>> = Vec::new();
    let cx = cx(&theme, &settings, &hooks);
    let lines = tool_result_lines(
        "write",
        &json!({"path": "a.txt", "content": "hello\n"}),
        "Successfully wrote to a.txt",
        false,
        &json!({}),
        &cx,
    );
    assert_eq!(lines, vec!["1 line, 6 bytes", "+hello"]);
}
