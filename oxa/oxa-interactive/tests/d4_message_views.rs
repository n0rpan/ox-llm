//! D4 acceptance tests: Message views. Owner: task D4.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_interactive::views::{
    render_assistant, render_bash, render_block, render_branch_summary, render_compaction, render_custom,
    render_notice, render_shown, render_user,
};
use oxa_interactive::{Block, ImageRef, ViewCx, ViewSettings};
use oxa_proto::{
    shown_why, CommandSpec, HookPoint, HookReply, Hooks, Level, OxaError, RenderKind, StopReason, Tool, Ui,
    Value,
};
use oxa_tui::{builtin_theme, visible_width, ColorMode, TermCaps, Theme};
use std::sync::Arc;

/// Owns the theme, settings and hooks a `ViewCx` borrows.
struct Fixture {
    theme: Theme,
    settings: ViewSettings,
    hooks: Vec<Arc<dyn Hooks>>,
}

impl Fixture {
    fn plain() -> Fixture {
        Fixture {
            theme: Theme::plain(),
            settings: ViewSettings::default(),
            hooks: Vec::new(),
        }
    }
    fn dark() -> Fixture {
        let theme = builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme");
        Fixture {
            theme,
            settings: ViewSettings::default(),
            hooks: Vec::new(),
        }
    }
    fn cx(&self, width: u16) -> ViewCx<'_> {
        ViewCx {
            width,
            theme: &self.theme,
            settings: &self.settings,
            caps: TermCaps::BASIC,
            hooks: &self.hooks,
            cwd: "/work",
            home: "",
            keymap: None,
        }
    }
}

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

fn assert_fits(lines: &[String], width: u16) {
    for line in lines {
        assert!(
            visible_width(line) <= width as usize,
            "line wider than {width}: {line:?}"
        );
    }
}

/// A hook set whose renderer draws one fixed line for message payloads.
struct MessageRenderer;

impl Hooks for MessageRenderer {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn render(&self, kind: RenderKind, payload: &Value, _width: u16) -> Option<Vec<String>> {
        match kind {
            RenderKind::Message if payload["customType"] == "fancy" => {
                Some(vec!["EXTENSION RENDER".to_string()])
            }
            _ => None,
        }
    }
}

/// A PNG header with the given size (enough for the dimension reader).
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    v
}

/// user message
#[test]
fn user_message_view() {
    let fx = Fixture::plain();
    let cx = fx.cx(80);
    let lines = render_user("hello **world**", &[], &cx);
    // One blank line above and below the block, Markdown inside.
    assert_eq!(lines.first().map(String::as_str), Some(""));
    assert_eq!(lines.last().map(String::as_str), Some(""));
    assert_eq!(lines[1].trim(), "hello world");
    assert_fits(&lines, 80);

    let wide = fx.cx(120);
    let lines120 = render_user("hello **world**", &[], &wide);
    assert_eq!(lines120[1].trim(), "hello world");
    assert_fits(&lines120, 120);

    assert!(render_user("", &[], &cx).is_empty());
}

/// markdown
#[test]
fn assistant_message_view() {
    let fx = Fixture::plain();
    let cx = fx.cx(80);
    let md = "# Title\n\n- alpha\n- beta\n\n`code` and a [link](https://example.com)";
    let lines = render_assistant(md, "", false, None, None, &cx);
    let text = joined(&lines);
    assert!(text.contains("Title"), "heading text: {text}");
    assert!(!text.contains("# Title"), "heading marker must not show: {text}");
    assert!(
        text.contains("alpha") && text.contains("beta"),
        "list items: {text}"
    );
    assert!(!text.contains("**"), "no raw markup: {text}");
    assert_fits(&lines, 80);

    // Streaming: a half-written fence and a lone emphasis marker render without panicking.
    let partial = render_assistant("```rust\nfn main() {\n*half", "", true, None, None, &cx);
    assert!(joined(&partial).contains("fn main()"));

    // Empty text and empty thinking, no error: no lines.
    assert!(render_assistant("", "", false, None, None, &cx).is_empty());
    assert!(render_assistant("   ", "", false, Some(StopReason::Stop), None, &cx).is_empty());
}

/// hideThinkingBlock and ctrl+t
#[test]
fn thinking_hide_and_collapse() {
    let mut fx = Fixture::plain();
    fx.settings.hide_thinking = false;
    let shown = render_assistant("answer", "pondering the question", false, None, None, &fx.cx(80));
    assert!(joined(&shown).contains("pondering the question"));
    assert!(joined(&shown).contains("answer"));

    // ctrl+t collapses thinking blocks: the app flips hide_thinking and the view follows.
    fx.settings.hide_thinking = true;
    let hidden = render_assistant("answer", "pondering the question", false, None, None, &fx.cx(80));
    let text = joined(&hidden);
    assert!(text.contains("Thinking..."), "hidden label: {text}");
    assert!(
        !text.contains("pondering"),
        "hidden thinking must not show: {text}"
    );
    assert!(text.contains("answer"));
    assert_eq!(hidden.iter().filter(|l| l.contains("Thinking...")).count(), 1);

    // Hidden thinking with no text is one line.
    let only_thinking = render_assistant("", "deep", false, None, None, &fx.cx(80));
    assert_eq!(only_thinking.len(), 1);

    fx.settings.hide_thinking = false;
    assert!(joined(&render_assistant(
        "answer",
        "pondering",
        false,
        None,
        None,
        &fx.cx(80)
    ))
    .contains("pondering"));
}

/// collapsed and expanded
#[test]
fn compaction_and_branch_summary_views() {
    let fx = Fixture::plain();
    let cx = fx.cx(80);

    let collapsed = render_compaction("the summary body", 1234, None, false, &cx);
    let text = joined(&collapsed);
    assert!(text.contains("[compaction]"), "{text}");
    assert!(text.contains("Compacted from 1234 tokens"), "{text}");
    assert!(text.contains("to expand"), "{text}");
    assert!(
        !text.contains("the summary body"),
        "collapsed hides the summary: {text}"
    );
    assert_fits(&collapsed, 80);

    let expanded = render_compaction("the summary body", 1234, None, true, &cx);
    let text = joined(&expanded);
    assert!(text.contains("[compaction]"));
    assert!(text.contains("the summary body"), "{text}");
    assert_fits(&expanded, 80);

    let branch = render_branch_summary("branch body", false, &cx);
    let text = joined(&branch);
    assert!(
        text.contains("[branch]") && text.contains("Branch summary ("),
        "{text}"
    );
    assert!(!text.contains("branch body"));

    let branch_open = render_branch_summary("branch body", true, &cx);
    let text = joined(&branch_open);
    assert!(
        text.contains("branch body") && text.contains("Branch Summary"),
        "{text}"
    );

    // The expand hint names the key of app.tools.expand (default ctrl+o).
    assert!(joined(&collapsed).contains("ctrl+o"));
}

/// display flag
#[test]
fn custom_message_view() {
    let mut fx = Fixture::plain();
    let details = serde_json::json!({});

    // No extension renderer: the default block with the custom type as label.
    let default = render_custom("note", "custom *content*", &details, false, &fx.cx(80));
    let text = joined(&default);
    assert!(text.contains("[note]"), "{text}");
    assert!(text.contains("custom content"), "{text}");
    assert_fits(&default, 80);

    // An extension renderer for the same payload wins.
    let block = Block::Custom {
        custom_type: "fancy".to_string(),
        content: "x".to_string(),
        details: details.clone(),
        expanded: false,
    };
    fx.hooks = vec![Arc::new(MessageRenderer)];
    let rendered = render_block(&block, &fx.cx(80));
    assert_eq!(rendered, vec!["EXTENSION RENDER".to_string()]);

    // Another custom type falls through to the default block.
    let other = render_custom("note", "hi", &details, false, &fx.cx(80));
    assert!(joined(&other).contains("[note]"));
}

/// command, output, exit code, excluded marker
#[test]
fn bash_execution_view() {
    let fx = Fixture::plain();
    let cx = fx.cx(80);

    let ok = render_bash("ls", "a\nb\n", Some(0), false, false, false, &cx);
    let text = joined(&ok);
    assert!(text.contains("$ ls"), "{text}");
    assert!(text.contains("a") && text.contains("b"), "{text}");
    assert!(text.contains("exit 0"), "{text}");
    assert!(!text.contains("not sent"), "{text}");

    let failed = render_bash("false", "", Some(2), false, false, false, &cx);
    assert!(joined(&failed).contains("exit 2"));

    let running = render_bash("sleep 9", "tick", None, false, true, false, &cx);
    assert!(joined(&running).contains("Running..."));

    let cancelled = render_bash("sleep 9", "", None, false, false, false, &cx);
    assert!(joined(&cancelled).contains("(cancelled)"));

    let excluded = render_bash("secret", "x", Some(0), true, false, false, &cx);
    assert!(joined(&excluded).contains("not sent to the model"));

    // Collapsed: only the last collapsed_tool_lines lines, with the count of earlier ones.
    let mut fx2 = Fixture::plain();
    fx2.settings.collapsed_tool_lines = 5;
    let output: String = (0..30).map(|i| format!("line {i}\n")).collect();
    let collapsed = render_bash("seq 30", &output, Some(0), false, false, false, &fx2.cx(80));
    let text = joined(&collapsed);
    assert!(text.contains("25 earlier lines"), "{text}");
    assert!(text.contains("line 29") && !text.contains("line 0\n"), "{text}");
    assert!(!text.lines().any(|l| l.trim() == "line 0"), "{text}");
    let expanded = render_bash("seq 30", &output, Some(0), false, false, true, &fx2.cx(80));
    assert!(joined(&expanded).lines().any(|l| l.trim() == "line 0"));
    assert_fits(&expanded, 80);
}

/// aborted / failed / cut notices
#[test]
fn shown_entry_view() {
    let fx = Fixture::plain();
    let cx = fx.cx(80);

    let aborted = render_shown("half a reply", shown_why::ABORTED, &cx);
    let text = joined(&aborted);
    assert!(text.contains("half a reply"), "{text}");
    assert!(
        text.contains("aborted: partial reply dropped; your prompt stays"),
        "{text}"
    );

    let failed = render_shown("partial", shown_why::FAILED, &cx);
    assert!(joined(&failed).contains("failed: not sent to the model; ask again"));

    let cut = render_shown("partial", shown_why::CUT, &cx);
    assert!(joined(&cut).contains("cut: not sent to the model; ask again"));

    // A notice shown as an entry is a plain notice (no "not sent" reason).
    let notice = render_shown("epoch cost 0.02", shown_why::NOTICE, &cx);
    let text = joined(&notice);
    assert!(text.contains("epoch cost 0.02"));
    assert!(!text.contains("not sent"));

    // Assistant stop reasons: aborted, error, and a length cut.
    let stopped = render_assistant("", "", false, Some(StopReason::Aborted), None, &cx);
    assert!(joined(&stopped).contains("Operation aborted"));
    let errored = render_assistant("", "", false, Some(StopReason::Error), Some("boom"), &cx);
    assert!(joined(&errored).contains("Error: boom"));
    let cut_reply = render_assistant("partial answer", "", false, Some(StopReason::Length), None, &cx);
    let text = joined(&cut_reply);
    assert!(
        text.contains("partial answer") && text.contains("truncated before completion"),
        "{text}"
    );

    let note = render_notice(Level::Warning, "careful", &cx);
    assert!(joined(&note).contains("careful"));
}

/// Lines never exceed the width, at 80 and 120 columns, for long wrapping text.
#[test]
fn views_fit_the_width() {
    let fx = Fixture::dark();
    let long = "word ".repeat(80);
    for width in [80u16, 120] {
        let cx = fx.cx(width);
        assert_fits(&render_user(&long, &[], &cx), width);
        assert_fits(
            &render_assistant(&long, &long, false, Some(StopReason::Error), Some(&long), &cx),
            width,
        );
        assert_fits(&render_compaction(&long, 99, None, true, &cx), width);
        assert_fits(&render_bash(&long, &long, Some(1), true, false, true, &cx), width);
        assert_fits(&render_shown(&long, shown_why::FAILED, &cx), width);
        assert_fits(&render_notice(Level::Error, &long, &cx), width);
    }
}

/// A coloured user block is filled to the full width on its background.
#[test]
fn user_block_fills_background() {
    let fx = Fixture::dark();
    let cx = fx.cx(60);
    let lines = render_user("short", &[], &cx);
    assert!(lines.len() >= 3);
    for line in &lines {
        assert_eq!(visible_width(line), 60, "{line:?}");
    }
    assert!(lines[0].contains("\x1b["), "background escape expected");
}

/// An image attachment is a fallback line when images are blocked, and inline text otherwise
/// falls back too when the terminal has no image protocol.
#[test]
fn user_images_fallback() {
    let env = TestEnv::new();
    let path = env.work().join("pic.png");
    std::fs::write(&path, png_bytes(40, 20)).expect("write png");
    let image = ImageRef {
        path,
        mime: "image/png".to_string(),
        w: 40,
        h: 20,
    };

    let mut fx = Fixture::plain();
    // No protocol (BASIC caps): fallback text with name and size.
    let lines = render_user("look", std::slice::from_ref(&image), &fx.cx(80));
    let text = joined(&lines);
    assert!(text.contains("[image pic.png 40x20]"), "{text}");

    // Protocol available but blocked by settings: still the fallback.
    fx.settings.block_images = true;
    let mut cx = fx.cx(80);
    cx.caps = TermCaps {
        images: Some(oxa_tui::ImageProtocol::Kitty),
        ..TermCaps::BASIC
    };
    let blocked = render_user("", std::slice::from_ref(&image), &cx);
    assert!(joined(&blocked).contains("[image pic.png 40x20]"));
}
