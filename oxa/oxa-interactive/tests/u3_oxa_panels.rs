//! U3 acceptance tests: the OXA transcript panels, the selector frame and the pi parity guard.
//! Spec: OXA-UI-DESIGN.md sec. 5 and 6.
//! Pure functions only: no environment, no clock, no files (the image case uses a missing file,
//! which falls back to text).

use oxa_interactive::skin::{frame_box, render_block_oxa};
use oxa_interactive::views::{render_block, render_tool};
use oxa_interactive::{Block, ImageRef, Skin, ToolBlock, ToolState, ViewCx, ViewSettings};
use oxa_proto::{
    shown_why, HookPoint, HookReply, Hooks, Level, RenderKind, StopReason, Tool, ToolOutput, Ui, Value,
};
use oxa_tui::{builtin_theme, is_image_line, strip_ansi, visible_width, ColorMode, TermCaps, Theme};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// Owns the theme, settings and hooks a `ViewCx` borrows.
struct Fx {
    theme: Theme,
    settings: ViewSettings,
    hooks: Vec<Arc<dyn Hooks>>,
}

impl Fx {
    fn new(theme: Theme, skin: Skin) -> Fx {
        Fx {
            theme,
            settings: ViewSettings {
                skin,
                ..ViewSettings::default()
            },
            hooks: Vec::new(),
        }
    }
    fn plain(skin: Skin) -> Fx {
        Fx::new(Theme::plain(), skin)
    }
    fn dark(skin: Skin) -> Fx {
        let theme = builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme");
        Fx::new(theme, skin)
    }
    fn cx(&self, width: u16) -> ViewCx<'_> {
        ViewCx {
            width,
            theme: &self.theme,
            settings: &self.settings,
            caps: TermCaps::BASIC,
            hooks: &self.hooks,
            cwd: "/work/proj",
            home: "/home/u",
            keymap: None,
        }
    }
}

/// The widths of the fit test: the smallest OXA width, a phone-ish one, a normal one, a wide one.
const WIDTHS: [u16; 4] = [12, 40, 80, 120];

fn tool(name: &str, args: Value, state: ToolState) -> ToolBlock {
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

fn done_with(mut t: ToolBlock, text: &str, is_error: bool, ui: Value, ms: Option<u64>) -> ToolBlock {
    t.result = Some(ToolOutput {
        text: text.to_string(),
        is_error,
        ui,
    });
    t.duration_ms = ms;
    t
}

/// `read src/a.rs`, two lines of result, 12 ms (the duration is hidden under 100 ms).
fn read_done() -> ToolBlock {
    let t = tool("read", json!({"path": "/work/proj/src/a.rs"}), ToolState::Done);
    done_with(t, "fn a() {}\nfn b() {}", false, json!(null), Some(12))
}

/// `$ cargo test` failed with exit 101 after 3.0 s.
fn bash_failed() -> ToolBlock {
    let t = tool("bash", json!({"command": "cargo test"}), ToolState::Error);
    done_with(
        t,
        "error[E0308]: mismatched",
        true,
        json!({"exit": 101, "timedOut": false}),
        Some(3000),
    )
}

/// `$ ./run.sh` still running, with live output.
fn bash_running() -> ToolBlock {
    let mut t = tool("bash", json!({"command": "./run.sh"}), ToolState::Running);
    t.partial = "Compiling x\n".to_string();
    t
}

fn subagent(args: Value, text: &str) -> ToolBlock {
    done_with(
        tool("subagent", args, ToolState::Done),
        text,
        false,
        json!(null),
        Some(250),
    )
}

/// One block of every kind, with content that exercises the wrapping and the states.
fn all_blocks() -> Vec<Block> {
    let mut edit = tool("edit", json!({"path": "/work/proj/x.rs"}), ToolState::Pending);
    edit.args_raw = String::new();
    let mut write = tool("write", json!({}), ToolState::Streaming);
    write.args_raw = "{\"path\": \"/work/pro".to_string();
    vec![
        Block::User {
            text: "Fix the off-by-one in src/range.rs and run the tests".to_string(),
            images: Vec::new(),
        },
        Block::User {
            text: String::new(),
            images: vec![ImageRef {
                path: PathBuf::from("/nonexistent/oxa-u3.png"),
                mime: "image/png".to_string(),
                w: 10,
                h: 10,
            }],
        },
        Block::Assistant {
            text: "Done. The span helper is fixed.".to_string(),
            thinking: "Let me check the span helper first.".to_string(),
            streaming: false,
            stop: Some(StopReason::Stop),
            error: None,
            model: None,
        },
        Block::Assistant {
            text: String::new(),
            thinking: String::new(),
            streaming: true,
            stop: None,
            error: None,
            model: None,
        },
        Block::Assistant {
            text: "partial answer".to_string(),
            thinking: String::new(),
            streaming: false,
            stop: Some(StopReason::Error),
            error: Some("the engine answered HTTP 400".to_string()),
            model: None,
        },
        Block::Assistant {
            text: "cut off here".to_string(),
            thinking: String::new(),
            streaming: false,
            stop: Some(StopReason::Length),
            error: None,
            model: None,
        },
        Block::Tool(read_done()),
        Block::Tool(bash_failed()),
        Block::Tool(bash_running()),
        Block::Tool(edit),
        Block::Tool(write),
        Block::Tool(subagent(
            json!({"agent": "scout", "task": "find the parser"}),
            "found 3 files",
        )),
        Block::Tool(tool("mcp__x__y", json!({"a": 1}), ToolState::Done)),
        Block::Compaction {
            summary: "Earlier work: the range helpers were reviewed.".to_string(),
            tokens_before: 15260,
            tokens_after: None,
            expanded: false,
        },
        Block::Compaction {
            summary: "Earlier work: the range helpers were reviewed.".to_string(),
            tokens_before: 15260,
            tokens_after: None,
            expanded: true,
        },
        Block::BranchSummary {
            summary: "Tried the other branch.".to_string(),
            expanded: false,
        },
        Block::BranchSummary {
            summary: "Tried the other branch.".to_string(),
            expanded: true,
        },
        Block::Custom {
            custom_type: "note".to_string(),
            content: "hello from an extension".to_string(),
            details: Value::Null,
            expanded: false,
        },
        Block::Bash {
            command: "ls".to_string(),
            output: "a\nb".to_string(),
            exit: Some(0),
            excluded: false,
            running: false,
            expanded: false,
        },
        Block::Shown {
            text: "HTTP 400 from the engine".to_string(),
            why: shown_why::FAILED.to_string(),
        },
        Block::Shown {
            text: "half a sentence".to_string(),
            why: shown_why::ABORTED.to_string(),
        },
        Block::Shown {
            text: "the stream stopped".to_string(),
            why: shown_why::CUT.to_string(),
        },
        Block::Shown {
            text: "epoch cost: 0.12".to_string(),
            why: shown_why::NOTICE.to_string(),
        },
        Block::Notice {
            level: Level::Info,
            text: "one two three four five six seven eight nine ten eleven twelve".to_string(),
        },
        Block::Notice {
            level: Level::Warning,
            text: "cache miss".to_string(),
        },
        Block::Notice {
            level: Level::Error,
            text: "the model is not loaded".to_string(),
        },
    ]
}

fn no_newline(lines: &[String]) -> bool {
    lines.iter().all(|l| !l.contains('\n'))
}

/// The stripped lines (no escape sequences), for layout checks.
fn plain_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| strip_ansi(l)).collect()
}

fn first_line_widths_ok(lines: &[String], width: u16) {
    for l in lines {
        if is_image_line(l) {
            continue;
        }
        assert!(
            visible_width(l) <= width as usize,
            "line wider than {width}: {:?}",
            strip_ansi(l)
        );
    }
}

// ---- every block, every width -----------------------------------------------------------------

#[test]
fn every_block_fits_and_is_self_contained() {
    for fx in [
        Fx::dark(Skin::Oxa),
        Fx::plain(Skin::Oxa),
        Fx::plain(Skin::OxaAscii),
    ] {
        let pi = Fx::new(fx.theme.clone(), Skin::Pi);
        for block in all_blocks() {
            for w in WIDTHS {
                let lines = render_block(&block, &fx.cx(w));
                assert!(no_newline(&lines), "newline in {block:?}");
                first_line_widths_ok(&lines, w);
                let pi_lines = render_block(&block, &pi.cx(w));
                if !pi_lines.is_empty() {
                    assert!(!lines.is_empty(), "empty OXA panel for {block:?} at {w}");
                }
            }
        }
    }
}

#[test]
fn render_block_dispatches_to_the_oxa_panels_only_when_asked() {
    let fx = Fx::plain(Skin::Oxa);
    let block = Block::Tool(read_done());
    let direct = render_block_oxa(&block, &fx.cx(60));
    assert_eq!(direct, render_block(&block, &fx.cx(60)));
    assert!(direct[0].starts_with("┌─ "));
}

#[test]
fn narrow_widths_do_not_panic() {
    for fx in [Fx::dark(Skin::Oxa), Fx::plain(Skin::OxaAscii)] {
        for block in all_blocks() {
            for w in 0..12u16 {
                let lines = render_block(&block, &fx.cx(w));
                assert!(no_newline(&lines));
            }
        }
        for w in 0..12u16 {
            let body = vec!["abc".to_string()];
            let _ = frame_box(&body, w, &fx.theme, fx.settings.skin, "borderAccent");
        }
    }
}

// ---- tool panel -------------------------------------------------------------------------------

#[test]
fn tool_rules_are_exactly_the_width() {
    let fx = Fx::dark(Skin::Oxa);
    let tools = [
        read_done(),
        bash_failed(),
        bash_running(),
        subagent(
            json!({"agent": "scout", "task": "find the parser"}),
            "found 3 files",
        ),
    ];
    for t in &tools {
        for w in [12u16, 20, 40, 60, 80, 120] {
            let lines = render_block(&Block::Tool(t.clone()), &fx.cx(w));
            let top = strip_ansi(&lines[0]);
            let bottom = strip_ansi(lines.last().expect("a tool panel has lines"));
            assert_eq!(visible_width(&top), w as usize, "top rule at {w}: {top:?}");
            assert_eq!(
                visible_width(&bottom),
                w as usize,
                "bottom rule at {w}: {bottom:?}"
            );
            assert!(top.starts_with("┌─ "), "{top:?}");
            assert!(bottom.starts_with('└'), "{bottom:?}");
            for l in &lines[1..lines.len() - 1] {
                assert!(
                    strip_ansi(l).starts_with("│ "),
                    "body rail missing: {:?}",
                    strip_ansi(l)
                );
            }
        }
    }
}

#[test]
fn tool_marks_follow_the_state() {
    let fx = Fx::plain(Skin::Oxa);
    let head = |t: ToolBlock| plain_lines(&render_block(&Block::Tool(t), &fx.cx(60)))[0].clone();
    assert!(head(read_done()).starts_with("┌─ ✓ read"));
    assert!(head(bash_failed()).starts_with("┌─ ✗ $ cargo test"));
    assert!(head(bash_running()).starts_with("┌─ ● $ ./run.sh"));
    assert!(head(tool("edit", json!({"path": "a"}), ToolState::Pending)).starts_with("┌─ ● edit"));
    assert!(head(tool("write", json!({}), ToolState::Streaming)).starts_with("┌─ ● write"));
    let mut failed_done = read_done();
    failed_done.result.as_mut().expect("result").is_error = true;
    assert!(
        head(failed_done).starts_with("┌─ ✗ read"),
        "an error result is a fail mark"
    );
}

#[test]
fn tool_status_text() {
    let fx = Fx::plain(Skin::Oxa);
    let bottom = |t: ToolBlock| {
        let lines = plain_lines(&render_block(&Block::Tool(t), &fx.cx(80)));
        lines.last().expect("bottom rule").clone()
    };
    let b = bottom(bash_running());
    assert!(b.contains(" running "), "{b:?}");
    assert!(!b.contains("took"));
    let b = bottom(bash_failed());
    assert!(b.contains("exit 101"), "{b:?}");
    assert!(b.contains("3.0s"), "{b:?}");
    assert!(
        b.find("exit 101") < b.find("3.0s"),
        "exit before the duration: {b:?}"
    );
    assert!(!b.contains("took"));
    // Under 100 ms the duration is hidden; from 100 ms on it is shown.
    let b = bottom(read_done());
    assert!(!b.contains("12ms"), "{b:?}");
    assert!(!b.contains('·') && !b.contains("took"), "{b:?}");
    let mut t = read_done();
    t.duration_ms = Some(100);
    assert!(bottom(t).contains("100ms"));
    // A timed-out bash shows the timeout; exit 0 shows no code.
    let t = done_with(
        tool("bash", json!({"command": "sleep 9"}), ToolState::Error),
        "",
        true,
        json!({"exit": 0, "timedOut": true}),
        None,
    );
    let b = bottom(t);
    assert!(b.contains("timed out"), "{b:?}");
    assert!(!b.contains("exit 0"), "{b:?}");
    // A finished tool without a status is a plain rule.
    let b = bottom(read_done());
    assert_eq!(b.chars().filter(|c| *c != '─' && *c != '└').count(), 0, "{b:?}");
}

#[test]
fn read_done_snapshot_at_60_columns() {
    let fx = Fx::plain(Skin::Oxa);
    let lines = plain_lines(&render_block(&Block::Tool(read_done()), &fx.cx(60)));
    let expected = vec![
        format!("┌─ ✓ read src/a.rs {}", "─".repeat(41)),
        "│ fn a() {}".to_string(),
        "│ fn b() {}".to_string(),
        format!("└{}", "─".repeat(59)),
    ];
    assert_eq!(lines, expected);
}

#[test]
fn failed_bash_snapshot_at_60_columns() {
    let fx = Fx::plain(Skin::Oxa);
    let lines = plain_lines(&render_block(&Block::Tool(bash_failed()), &fx.cx(60)));
    let expected = vec![
        format!("┌─ ✗ $ cargo test {}", "─".repeat(42)),
        "│ error[E0308]: mismatched".to_string(),
        format!("└{} exit 101 · 3.0s ─", "─".repeat(41)),
    ];
    assert_eq!(lines, expected);
}

#[test]
fn long_header_is_truncated_so_a_dash_remains() {
    let fx = Fx::plain(Skin::Oxa);
    let t = tool("bash", json!({"command": "x".repeat(200)}), ToolState::Running);
    let lines = render_block(&Block::Tool(t), &fx.cx(30));
    assert_eq!(visible_width(&lines[0]), 30);
    assert!(lines[0].contains("..."));
    assert!(lines[0].ends_with('─'));
}

#[test]
fn subagent_headers() {
    let fx = Fx::plain(Skin::Oxa);
    let single = plain_lines(&render_block(
        &Block::Tool(subagent(
            json!({"agent": "scout", "task": "find the parser"}),
            "found 3 files",
        )),
        &fx.cx(60),
    ));
    assert!(single[0].starts_with("┌─ ✓ subagent scout "), "{single:?}");
    assert_eq!(visible_width(&single[0]), 60);
    assert_eq!(single[1], "│ task: find the parser");
    assert_eq!(single[2], "│ found 3 files");

    let parallel = plain_lines(&render_block(
        &Block::Tool(subagent(
            json!({"tasks": [{"agent": "a", "task": "x"}, {"agent": "b", "task": "y"}]}),
            "",
        )),
        &fx.cx(60),
    ));
    assert!(
        parallel[0].starts_with("┌─ ✓ subagent parallel (2) "),
        "{parallel:?}"
    );
    assert_eq!(parallel[1], "│ a: x");
    assert_eq!(parallel[2], "│ b: y");

    let chain = plain_lines(&render_block(
        &Block::Tool(subagent(json!({"chain": [{"agent": "a", "task": "x"}]}), "")),
        &fx.cx(60),
    ));
    assert!(chain[0].starts_with("┌─ ✓ subagent chain (1) "), "{chain:?}");
    assert_eq!(chain[1], "│ a: x");

    // An unknown shape falls back to the generic view: the name and the arguments as JSON.
    let unknown = plain_lines(&render_block(
        &Block::Tool(subagent(json!({"foo": 1}), "")),
        &fx.cx(60),
    ));
    assert!(unknown[0].starts_with("┌─ ✓ subagent"), "{unknown:?}");
    assert!(unknown.iter().any(|l| l.contains("\"foo\": 1")), "{unknown:?}");
}

#[test]
fn subagent_while_streaming_falls_back_without_panic() {
    let fx = Fx::dark(Skin::Oxa);
    let mut t = tool("subagent", Value::Null, ToolState::Streaming);
    t.args_raw = "{\"agent\": \"sc".to_string();
    let lines = render_block(&Block::Tool(t), &fx.cx(50));
    assert!(!lines.is_empty());
    first_line_widths_ok(&lines, 50);
}

// ---- user, assistant, error panels --------------------------------------------------------------

#[test]
fn user_rail_on_every_text_line() {
    let fx = Fx::dark(Skin::Oxa);
    let block = Block::User {
        text: "one two three four five six seven eight nine ten\n\nsecond paragraph".to_string(),
        images: Vec::new(),
    };
    let lines = render_block(&block, &fx.cx(24));
    assert!(lines.len() > 3);
    for l in &lines {
        assert!(
            strip_ansi(l).starts_with('▌'),
            "rail missing: {:?}",
            strip_ansi(l)
        );
        assert!(visible_width(l) <= 24);
    }
}

#[test]
fn thinking_rail_and_hidden_thinking() {
    let fx = Fx::dark(Skin::Oxa);
    let block = Block::Assistant {
        text: String::new(),
        thinking: "Let me check the span helper first.".to_string(),
        streaming: false,
        stop: Some(StopReason::Stop),
        error: None,
        model: None,
    };
    let lines = render_block(&block, &fx.cx(30));
    assert!(!lines.is_empty());
    for l in &lines {
        assert!(
            strip_ansi(l).starts_with("┊ "),
            "rail missing: {:?}",
            strip_ansi(l)
        );
        assert!(visible_width(l) <= 30);
    }

    let mut hidden = Fx::dark(Skin::Oxa);
    hidden.settings.hide_thinking = true;
    let lines = plain_lines(&render_block(&block, &hidden.cx(30)));
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], "┊  Thinking...", "the pi indent follows the rail");
}

#[test]
fn assistant_error_ending_is_an_error_panel() {
    let fx = Fx::plain(Skin::Oxa);
    let block = Block::Assistant {
        text: "partial answer".to_string(),
        thinking: String::new(),
        streaming: false,
        stop: Some(StopReason::Error),
        error: Some("engine down".to_string()),
        model: None,
    };
    let lines = plain_lines(&render_block(&block, &fx.cx(40)));
    assert_eq!(lines[0], " partial answer");
    assert!(lines.iter().any(|l| l.starts_with("┌─ ✗ ERROR ")), "{lines:?}");
    assert!(lines.iter().any(|l| l == "│ engine down"), "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("Error:")),
        "the pi notice is replaced: {lines:?}"
    );
    let last = lines.last().expect("panel bottom");
    assert!(last.starts_with('└') && visible_width(last) == 40);

    // No error text: "Unknown error".
    let block = Block::Assistant {
        text: String::new(),
        thinking: String::new(),
        streaming: false,
        stop: Some(StopReason::Error),
        error: None,
        model: None,
    };
    let lines = plain_lines(&render_block(&block, &fx.cx(40)));
    assert!(lines.iter().any(|l| l == "│ Unknown error"), "{lines:?}");
}

// ---- shown replies ------------------------------------------------------------------------------

fn shown(text: &str, why: &str) -> Block {
    Block::Shown {
        text: text.to_string(),
        why: why.to_string(),
    }
}

#[test]
fn shown_failed_aborted_and_cut_panels() {
    let fx = Fx::plain(Skin::Oxa);
    let failed = plain_lines(&render_block(
        &shown("HTTP 400 from the engine", shown_why::FAILED),
        &fx.cx(40),
    ));
    assert!(failed[0].starts_with("┌─ ✗ ERROR "), "{failed:?}");
    assert!(
        failed.iter().any(|l| l == "│ HTTP 400 from the engine"),
        "{failed:?}"
    );
    assert!(
        failed.iter().any(|l| l == "│ not sent to the model; ask again"),
        "{failed:?}"
    );

    let aborted = plain_lines(&render_block(&shown("half", shown_why::ABORTED), &fx.cx(40)));
    assert!(aborted[0].starts_with("┌─ ● ABORTED "), "{aborted:?}");
    let cut = plain_lines(&render_block(&shown("stopped", shown_why::CUT), &fx.cx(40)));
    assert!(cut[0].starts_with("┌─ ● CUT "), "{cut:?}");

    let other = plain_lines(&render_block(&shown("x", "weird"), &fx.cx(40)));
    assert!(other[0].starts_with("┌─ ● WEIRD "), "{other:?}");
}

#[test]
fn shown_notice_is_a_notice() {
    let fx = Fx::plain(Skin::Oxa);
    let lines = plain_lines(&render_block(
        &shown("epoch cost: 0.12", shown_why::NOTICE),
        &fx.cx(40),
    ));
    assert_eq!(lines, vec![" · epoch cost: 0.12".to_string()]);
}

// ---- compaction and branch summary ----------------------------------------------------------------

#[test]
fn compaction_collapsed_divider() {
    let fx = Fx::plain(Skin::Oxa);
    let block = Block::Compaction {
        summary: "Earlier work.".to_string(),
        tokens_before: 15260,
        tokens_after: None,
        expanded: false,
    };
    let lines = plain_lines(&render_block(&block, &fx.cx(80)));
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("── ◆ COMPACTED from 15260 tokens · ctrl+o to expand "),
        "{lines:?}"
    );
    assert!(lines[0].ends_with('─'));
    assert_eq!(visible_width(&lines[0]), 80);
}

#[test]
fn compaction_expanded_has_no_hint_and_a_rail() {
    let fx = Fx::plain(Skin::Oxa);
    let block = Block::Compaction {
        summary: "Earlier work.".to_string(),
        tokens_before: 15260,
        tokens_after: None,
        expanded: true,
    };
    let lines = plain_lines(&render_block(&block, &fx.cx(60)));
    assert!(
        lines[0].starts_with("── ◆ COMPACTED from 15260 tokens "),
        "{lines:?}"
    );
    assert!(!lines[0].contains("to expand"));
    assert_eq!(visible_width(&lines[0]), 60);
    assert!(lines.len() > 2, "{lines:?}");
    assert!(lines[1..].iter().all(|l| l.starts_with('│')), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("Earlier work.")), "{lines:?}");
}

#[test]
fn branch_summary_divider() {
    let fx = Fx::plain(Skin::Oxa);
    let lines = plain_lines(&render_block(
        &Block::BranchSummary {
            summary: "x".to_string(),
            expanded: false,
        },
        &fx.cx(50),
    ));
    assert!(lines[0].starts_with("── ◆ BRANCH SUMMARY "), "{lines:?}");
    assert_eq!(visible_width(&lines[0]), 50);
}

// ---- notices ---------------------------------------------------------------------------------------

#[test]
fn notice_levels_and_wrapping_with_indent() {
    let fx = Fx::plain(Skin::Oxa);
    let info = plain_lines(&render_block(
        &Block::Notice {
            level: Level::Info,
            text: "one two three four five six seven eight nine ten eleven twelve".to_string(),
        },
        &fx.cx(24),
    ));
    assert!(info.len() > 2, "{info:?}");
    assert!(info[0].starts_with(" · "), "{info:?}");
    for l in &info[1..] {
        assert!(l.starts_with("   "), "continuation not indented: {l:?}");
    }
    for l in &info {
        assert!(visible_width(l) <= 24);
    }

    let warn = plain_lines(&render_block(
        &Block::Notice {
            level: Level::Warning,
            text: "cache miss".to_string(),
        },
        &fx.cx(40),
    ));
    assert_eq!(warn, vec![" ! cache miss".to_string()]);

    let err = plain_lines(&render_block(
        &Block::Notice {
            level: Level::Error,
            text: "not loaded".to_string(),
        },
        &fx.cx(40),
    ));
    assert_eq!(err, vec![" ✗ not loaded".to_string()]);
}

// ---- frame_box -------------------------------------------------------------------------------------

#[test]
fn frame_lines_are_exactly_the_width() {
    let body: Vec<String> = vec![
        "Select model".to_string(),
        "search:".to_string(),
        "    fake-model       oxe · not loaded".to_string(),
        "x".repeat(200),
        String::new(),
    ];
    for theme in [
        Theme::plain(),
        builtin_theme("dark", ColorMode::TrueColor, None).expect("dark"),
    ] {
        for w in [8u16, 20, 80] {
            let inner = oxa_interactive::frame_inner_width(w) as usize;
            let rendered: Vec<String> = body
                .iter()
                .map(|l| oxa_tui::truncate_to_width(l, inner, ""))
                .collect();
            let lines = frame_box(&rendered, w, &theme, Skin::Oxa, "borderAccent");
            assert_eq!(lines.len(), rendered.len() + 2);
            for l in &lines {
                assert_eq!(visible_width(l), w as usize, "{:?}", strip_ansi(l));
            }
            let plain = plain_lines(&lines);
            assert!(plain[0].starts_with('╔') && plain[0].ends_with('╗'));
            assert!(plain.last().expect("bottom").starts_with('╚'));
            for l in &plain[1..plain.len() - 1] {
                assert!(l.starts_with("║ ") && l.ends_with(" ║"), "{l:?}");
            }
        }
    }
}

#[test]
fn frame_pads_short_lines_and_truncates_long_ones() {
    let plain = Theme::plain();
    let body = vec!["ab".to_string(), "0123456789012345678901234".to_string()];
    let lines = frame_box(&body, 20, &plain, Skin::Oxa, "borderAccent");
    assert_eq!(lines[1], format!("║ ab{} ║", " ".repeat(14)));
    assert_eq!(lines[2], "║ 0123456789012345 ║");
}

#[test]
fn frame_narrower_than_eight_is_unframed() {
    let plain = Theme::plain();
    let body = vec!["abcdefgh".to_string(), "xy".to_string()];
    let lines = frame_box(&body, 5, &plain, Skin::Oxa, "borderAccent");
    assert_eq!(lines, vec!["abcde".to_string(), "xy".to_string()]);
}

#[test]
fn frame_uses_the_ascii_glyphs_on_the_ascii_skin() {
    let plain = Theme::plain();
    let body = vec!["Pick".to_string()];
    let lines = frame_box(&body, 12, &plain, Skin::OxaAscii, "borderAccent");
    assert_eq!(lines[0], "+==========+");
    assert_eq!(lines[1], "| Pick     |");
    assert_eq!(lines[2], "+==========+");
    assert!(lines.iter().all(|l| l.is_ascii()));
}

#[test]
fn frame_on_a_colour_theme_closes_styles_before_the_border() {
    let dark = builtin_theme("dark", ColorMode::TrueColor, None).expect("dark");
    let body = vec!["\x1b[1mbold without reset".to_string()];
    let lines = frame_box(&body, 30, &dark, Skin::Oxa, "borderAccent");
    assert!(lines[1].contains("\x1b[0m"), "{:?}", lines[1]);
    assert_eq!(visible_width(&lines[1]), 30);
}

// ---- plain theme and ASCII skin --------------------------------------------------------------------

#[test]
fn plain_theme_adds_no_escape_sequence() {
    let fx = Fx::plain(Skin::Oxa);
    let blocks: Vec<Block> = all_blocks()
        .into_iter()
        .filter(|b| {
            matches!(
                b,
                Block::User { .. }
                    | Block::Tool(_)
                    | Block::Notice { .. }
                    | Block::Compaction { .. }
                    | Block::Shown { .. }
            )
        })
        .collect();
    for block in &blocks {
        for w in WIDTHS {
            let lines = render_block(block, &fx.cx(w));
            assert!(
                lines.iter().all(|l| !l.contains('\x1b')),
                "escape in {block:?}: {lines:?}"
            );
        }
    }
    let body = vec!["plain".to_string()];
    let framed = frame_box(&body, 20, &fx.theme, Skin::Oxa, "borderAccent");
    assert!(framed.iter().all(|l| !l.contains('\x1b')));
}

#[test]
fn ascii_skin_with_ascii_input_is_pure_ascii() {
    let texts = [
        Block::User {
            text: "Fix it".to_string(),
            images: Vec::new(),
        },
        Block::Assistant {
            text: "ok".to_string(),
            thinking: "hmm".to_string(),
            streaming: false,
            stop: Some(StopReason::Error),
            error: Some("boom".to_string()),
            model: None,
        },
        Block::Tool(read_done()),
        Block::Tool(bash_failed()),
        Block::Tool(bash_running()),
        Block::Tool(subagent(json!({"agent": "scout", "task": "find"}), "ok")),
        Block::Compaction {
            summary: "Summary text.".to_string(),
            tokens_before: 99,
            tokens_after: None,
            expanded: false,
        },
        Block::Notice {
            level: Level::Info,
            text: "note".to_string(),
        },
        shown("failed reply", shown_why::FAILED),
    ];
    for fx in [Fx::plain(Skin::OxaAscii), Fx::dark(Skin::OxaAscii)] {
        for block in &texts {
            for w in WIDTHS {
                let lines = render_block(block, &fx.cx(w));
                for l in &lines {
                    assert!(l.is_ascii(), "non-ASCII glyph in {block:?}: {l:?}");
                }
            }
        }
    }
}

// ---- pi parity ------------------------------------------------------------------------------------

/// The four blocks of the parity guard (the same content as the captured pi output).
fn parity_blocks() -> Vec<Block> {
    vec![
        Block::Tool(read_done()),
        Block::Tool(bash_failed()),
        Block::Tool(bash_running()),
        Block::User {
            text: "Fix it".to_string(),
            images: Vec::new(),
        },
    ]
}

#[test]
fn pi_skin_output_is_pinned() {
    let plain = Fx::plain(Skin::Pi);
    let dark = Fx::dark(Skin::Pi);
    let names = ["tool done", "tool error", "tool running", "user"];
    let plain_expected: [&[&str]; 4] = [
        PI_PLAIN_READ_DONE,
        PI_PLAIN_BASH_FAILED,
        PI_PLAIN_BASH_RUNNING,
        PI_PLAIN_USER,
    ];
    let dark_expected: [&[&str]; 4] = [
        PI_DARK_READ_DONE,
        PI_DARK_BASH_FAILED,
        PI_DARK_BASH_RUNNING,
        PI_DARK_USER,
    ];
    for (i, block) in parity_blocks().iter().enumerate() {
        let got = render_block(block, &plain.cx(60));
        assert_eq!(got, plain_expected[i], "plain {}", names[i]);
        let got = render_block(block, &dark.cx(60));
        assert_eq!(got, dark_expected[i], "dark {}", names[i]);
    }
}

#[test]
fn render_tool_under_pi_is_pinned() {
    let fx = Fx::plain(Skin::Pi);
    assert_eq!(render_tool(&read_done(), &fx.cx(60)), PI_PLAIN_READ_DONE.to_vec());
}

/// A hook set that renders one extension tool; the header is the tool's own, under OXA too.
struct DemoHooks;

impl Hooks for DemoHooks {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<oxa_proto::CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), oxa_proto::OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn render(&self, kind: RenderKind, _payload: &Value, _width: u16) -> Option<Vec<String>> {
        match kind {
            RenderKind::ToolCall => Some(vec!["stat (extension view)".to_string()]),
            _ => None,
        }
    }
}

#[test]
fn extension_call_renderer_is_the_header() {
    let mut fx = Fx::plain(Skin::Oxa);
    fx.hooks = vec![Arc::new(DemoHooks) as Arc<dyn Hooks>];
    let t = tool("mcp__demo__stat", json!({}), ToolState::Done);
    let lines = plain_lines(&render_block(&Block::Tool(t), &fx.cx(40)));
    assert!(lines[0].starts_with("┌─ ✓ stat (extension view) "), "{lines:?}");
    assert_eq!(visible_width(&lines[0]), 40);
}

const PI_PLAIN_READ_DONE: &[&str] = &[" read src/a.rs", " fn a() {}", " fn b() {}", " took 12ms"];
const PI_PLAIN_BASH_FAILED: &[&str] = &[
    " $ cargo test",
    " error[E0308]: mismatched",
    " took 3.0s",
    " exit 101",
];
const PI_PLAIN_BASH_RUNNING: &[&str] = &[" $ ./run.sh", " Compiling x"];
const PI_PLAIN_USER: &[&str] = &["", " Fix it", ""];
const PI_DARK_READ_DONE: &[&str] = &["\u{1b}[48;2;41;64;50m \u{1b}[38;2;223;224;225m\u{1b}[1mread\u{1b}[22m\u{1b}[39m \u{1b}[38;2;167;153;210msrc/a.rs\u{1b}[39m\u{1b}[0m\u{1b}[48;2;41;64;50m                                              \u{1b}[49m", "\u{1b}[48;2;41;64;50m \u{1b}[38;2;111;172;204mfn\u{1b}[39m\u{1b}[38;2;107;182;141m \u{1b}[39m\u{1b}[38;2;202;155;50ma\u{1b}[39m\u{1b}[38;2;158;164;168m(\u{1b}[39m\u{1b}[38;2;158;164;168m)\u{1b}[39m\u{1b}[38;2;107;182;141m \u{1b}[39m\u{1b}[38;2;158;164;168m{\u{1b}[39m\u{1b}[38;2;158;164;168m}\u{1b}[39m\u{1b}[0m\u{1b}[48;2;41;64;50m                                                  \u{1b}[49m", "\u{1b}[48;2;41;64;50m \u{1b}[38;2;111;172;204mfn\u{1b}[39m\u{1b}[38;2;107;182;141m \u{1b}[39m\u{1b}[38;2;202;155;50mb\u{1b}[39m\u{1b}[38;2;158;164;168m(\u{1b}[39m\u{1b}[38;2;158;164;168m)\u{1b}[39m\u{1b}[38;2;107;182;141m \u{1b}[39m\u{1b}[38;2;158;164;168m{\u{1b}[39m\u{1b}[38;2;158;164;168m}\u{1b}[39m\u{1b}[0m\u{1b}[48;2;41;64;50m                                                  \u{1b}[49m", "\u{1b}[48;2;41;64;50m \u{1b}[2mtook 12ms\u{1b}[22m\u{1b}[0m\u{1b}[48;2;41;64;50m                                                  \u{1b}[49m"];
const PI_DARK_BASH_FAILED: &[&str] = &["\u{1b}[48;2;91;40;42m \u{1b}[38;2;223;224;225m\u{1b}[1m$\u{1b}[22m\u{1b}[39m cargo test\u{1b}[0m\u{1b}[48;2;91;40;42m                                               \u{1b}[49m", "\u{1b}[48;2;91;40;42m \u{1b}[38;2;232;128;130merror[E0308]: mismatched\u{1b}[39m\u{1b}[0m\u{1b}[48;2;91;40;42m                                   \u{1b}[49m", "\u{1b}[48;2;91;40;42m \u{1b}[2mtook 3.0s\u{1b}[22m\u{1b}[0m\u{1b}[48;2;91;40;42m                                                  \u{1b}[49m", "\u{1b}[48;2;91;40;42m \u{1b}[38;2;232;128;130mexit 101\u{1b}[39m\u{1b}[0m\u{1b}[48;2;91;40;42m                                                   \u{1b}[49m"];
const PI_DARK_BASH_RUNNING: &[&str] = &["\u{1b}[48;2;54;56;57m \u{1b}[38;2;223;224;225m\u{1b}[1m$\u{1b}[22m\u{1b}[39m ./run.sh\u{1b}[0m\u{1b}[48;2;54;56;57m                                                 \u{1b}[49m", "\u{1b}[48;2;54;56;57m \u{1b}[38;2;158;164;168mCompiling x\u{1b}[39m\u{1b}[0m\u{1b}[48;2;54;56;57m                                                \u{1b}[49m"];
const PI_DARK_USER: &[&str] = &["\u{1b}[48;2;41;58;68m                                                            \u{1b}[0m", "\u{1b}[48;2;41;58;68m \u{1b}[38;2;223;224;225mFix it\u{1b}[39m\u{1b}[0m\u{1b}[48;2;41;58;68m                                                     \u{1b}[0m", "\u{1b}[48;2;41;58;68m                                                            \u{1b}[0m"];

#[test]
fn ascii_skin_expanded_compaction_has_only_ascii() {
    let fx = Fx::plain(Skin::OxaAscii);
    let block = Block::Compaction {
        summary: "Earlier work.".to_string(),
        tokens_before: 15260,
        tokens_after: Some(4000),
        expanded: true,
    };
    let lines = plain_lines(&render_block(&block, &fx.cx(60)));
    assert!(lines.iter().any(|l| l.contains("15260 -> 4000")), "{lines:?}");
    assert!(lines.iter().all(|l| l.is_ascii()), "{lines:?}");
}
