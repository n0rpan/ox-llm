//! U3: the transcript panels of the OXA skin and the frame of selectors and dialogs.
//! Spec: OXA-UI-DESIGN.md sec. 5.
//!
//! Every panel is built from the pi views (`views::*`), so the content is the same; only the
//! frame, the rails and the headers are the skin's. Widths below [`MIN_WIDTH`] fall back to the
//! pi view of the block.
use super::{glyphs, Glyphs};
use crate::views::{self, tool_parts};
use crate::{Block, ImageRef, Skin, ToolBlock, ToolState, ViewCx, ViewSettings};
use oxa_proto::{shown_why, Level, StopReason, Value};
use oxa_tui::{is_image_line, truncate_to_width, visible_width, wrap_ansi, Theme};

/// The SGR full reset that closes every style of a line.
const RESET: &str = "\x1b[0m";
/// The key action whose label is shown in the compaction hint (as the pi views).
const EXPAND_ACTION: &str = "app.tools.expand";
/// Narrower blocks fall back to the pi view.
const MIN_WIDTH: usize = 12;

/// Lines of one block in the OXA look at `cx.width` (the counterpart of
/// `views::render_block`, which calls this when `cx.settings.skin.is_oxa()`).
pub fn render_block_oxa(block: &Block, cx: &ViewCx) -> Vec<String> {
    let w = cx.width as usize;
    if w < MIN_WIDTH {
        return pi_fallback(block, cx);
    }
    match block {
        Block::User { text, images } => user_panel(text, images, cx),
        Block::Assistant {
            text,
            thinking,
            streaming,
            stop,
            error,
            ..
        } => assistant_panel(text, thinking, *streaming, *stop, error.as_deref(), cx),
        Block::Tool(t) => tool_panel(t, cx),
        Block::Compaction {
            summary,
            tokens_before,
            tokens_after,
            expanded,
        } => compaction_panel(summary, *tokens_before, *tokens_after, *expanded, cx),
        Block::BranchSummary { summary, expanded } => branch_panel(summary, *expanded, cx),
        Block::Custom {
            custom_type,
            content,
            details,
            expanded,
        } => views::render_custom(custom_type, content, details, *expanded, cx),
        Block::Bash {
            command,
            output,
            exit,
            excluded,
            running,
            expanded,
        } => views::render_bash(command, output, *exit, *excluded, *running, *expanded, cx),
        Block::Shown { text, why } => shown_panel(text, why, cx),
        Block::Notice { level, text } => notice_lines(*level, text, cx),
    }
}

/// A closed double frame of exactly `width` columns around `body` (sec. 5.8). The body lines
/// must have been rendered for `frame_inner_width(width)` columns; shorter lines are padded,
/// longer ones truncated. The frame is drawn in the theme token `token`. A `width` below 8
/// returns the body unframed (truncated to `width`).
pub fn frame_box(body: &[String], width: u16, theme: &Theme, skin: Skin, token: &str) -> Vec<String> {
    let w = width as usize;
    if w < 8 {
        return body.iter().map(|l| truncate_to_width(l, w, "")).collect();
    }
    let g = glyphs(skin);
    let inner = w - 4;
    let edge = |left: &str, right: &str| theme.fg(token, &format!("{left}{}{right}", g.dh.repeat(w - 2)));
    let mut out = vec![edge(g.dtl, g.dtr)];
    let side = theme.fg(token, g.dv);
    // A style left open by the content must not reach the padding or the right border.
    let reset = if theme.is_plain() { "" } else { RESET };
    for line in body {
        let cut = truncate_to_width(line, inner, "");
        let fill = " ".repeat(inner.saturating_sub(visible_width(&cut)));
        out.push(format!("{side} {cut}{reset}{fill} {side}"));
    }
    out.push(edge(g.dbl, g.dbr));
    out
}

/// The width a body must be rendered at to fit inside [`frame_box`] of `width` columns
/// (`width - 4`: a border and one column of padding each side); `width` itself below 8.
pub fn frame_inner_width(width: u16) -> u16 {
    if width < 8 {
        width
    } else {
        width - 4
    }
}

// ---- helpers ----------------------------------------------------------------------------------

/// A copy of `cx` at another width.
fn at_width<'a>(cx: &ViewCx<'a>, width: usize) -> ViewCx<'a> {
    ViewCx {
        width: u16::try_from(width).unwrap_or(u16::MAX),
        theme: cx.theme,
        settings: cx.settings,
        caps: cx.caps,
        hooks: cx.hooks,
        cwd: cx.cwd,
        home: cx.home,
        keymap: cx.keymap,
    }
}

/// The block in the pi look (for widths the OXA panels do not fit).
fn pi_fallback(block: &Block, cx: &ViewCx) -> Vec<String> {
    let settings = ViewSettings {
        skin: Skin::Pi,
        ..cx.settings.clone()
    };
    let pcx = ViewCx {
        settings: &settings,
        ..at_width(cx, cx.width as usize)
    };
    views::render_block(block, &pcx)
}

/// The output columns of the padding (`outputPad`).
fn opad(cx: &ViewCx) -> usize {
    cx.settings.output_pad as usize
}

/// Each line with a rail: `glyph` in `token`, then `gap`, then the line.
fn rail(lines: Vec<String>, glyph: &str, token: &str, gap: &str, theme: &Theme) -> Vec<String> {
    let mark = theme.fg(token, glyph);
    lines.into_iter().map(|l| format!("{mark}{gap}{l}")).collect()
}

/// Plain text, one styled line per source line.
fn styled_lines(text: &str, token: &str, theme: &Theme) -> Vec<String> {
    text.lines().map(|l| theme.fg(token, l)).collect()
}

/// The top rule `<lead><head> <dashes>` of exactly `w` columns; `lead` is three columns wide
/// (`┌─ ` or `── `). The head is truncated so one dash remains.
fn head_rule(lead: &str, head: &str, w: usize, token: &str, g: &Glyphs, theme: &Theme) -> String {
    let head = truncate_to_width(head, w.saturating_sub(5), "...");
    let (sep, hw) = if head.is_empty() {
        ("", 0)
    } else {
        (" ", visible_width(&head))
    };
    let dashes = w.saturating_sub(3 + hw + sep.len());
    format!(
        "{}{head}{}",
        theme.fg(token, lead),
        theme.fg(token, &format!("{sep}{}", g.h.repeat(dashes)))
    )
}

/// The bottom rule `└<dashes> <status> ─` of exactly `w` columns, or a plain rule without a
/// status. `status` is already styled.
fn foot_rule(status: &str, w: usize, token: &str, g: &Glyphs, theme: &Theme) -> String {
    if status.is_empty() {
        return theme.fg(token, &format!("{}{}", g.bl, g.h.repeat(w.saturating_sub(1))));
    }
    let status = truncate_to_width(status, w.saturating_sub(5), "...");
    let sw = visible_width(&status);
    let dashes = w.saturating_sub(4 + sw);
    format!(
        "{}{status}{}",
        theme.fg(token, &format!("{}{} ", g.bl, g.h.repeat(dashes))),
        theme.fg(token, &format!(" {}", g.h))
    )
}

/// A panel: top rule with `head`, body lines on the left rail (wrapped to the width inside
/// the rail), bottom rule with `status`. Frame in `token`.
fn framed(head: &str, status: &str, body: &[String], token: &str, cx: &ViewCx) -> Vec<String> {
    let g = glyphs(cx.settings.skin);
    let theme = cx.theme;
    let w = cx.width as usize;
    let inner = w.saturating_sub(2).max(1);
    let mut out = vec![head_rule(&format!("{}{} ", g.tl, g.h), head, w, token, g, theme)];
    let lines: Vec<String> = body.iter().flat_map(|l| wrap_ansi(l, inner)).collect();
    out.extend(rail(lines, g.v, token, " ", theme));
    out.push(foot_rule(status, w, token, g, theme));
    out
}

// ---- panels -----------------------------------------------------------------------------------

/// 5.3: the pi user view at `W - 1` with the rail in `accent`; images pass through.
fn user_panel(text: &str, images: &[ImageRef], cx: &ViewCx) -> Vec<String> {
    let g = glyphs(cx.settings.skin);
    let inner = at_width(cx, (cx.width as usize).saturating_sub(1));
    views::render_user(text, images, &inner)
        .into_iter()
        .map(|l| {
            if is_image_line(&l) {
                l
            } else {
                format!("{}{l}", cx.theme.fg("accent", g.user_rail))
            }
        })
        .collect()
}

/// 5.4: the thinking on the `┊ ` rail at `W - 2`, the text as the pi view, and an error ending
/// as an error panel (5.5).
fn assistant_panel(
    text: &str,
    thinking: &str,
    streaming: bool,
    stop: Option<StopReason>,
    error: Option<&str>,
    cx: &ViewCx,
) -> Vec<String> {
    let g = glyphs(cx.settings.skin);
    let theme = cx.theme;
    let mut out: Vec<String> = Vec::new();

    let think_cx = at_width(cx, (cx.width as usize).saturating_sub(2));
    let think = views::render_assistant("", thinking, streaming, None, None, &think_cx);
    out.extend(rail(think, g.think_rail, "borderMuted", " ", theme));

    let failed = stop == Some(StopReason::Error);
    let body_stop = if failed { None } else { stop };
    let body_error = if failed { None } else { error };
    let body = views::render_assistant(text, "", streaming, body_stop, body_error, cx);
    if !out.is_empty() && !body.is_empty() {
        out.push(String::new());
    }
    out.extend(body);

    if failed {
        let msg = error.filter(|e| !e.is_empty()).unwrap_or("Unknown error");
        let head = theme.fg("error", &theme.bold(&format!("{} ERROR", g.fail)));
        out.extend(framed(&head, "", &styled_lines(msg, "text", theme), "error", cx));
    }
    out
}

/// 5.1 + 5.2: the tool call in a frame. The header is the title, further header lines and the
/// result body are the body; `subagent` calls get their readable form.
fn tool_panel(t: &ToolBlock, cx: &ViewCx) -> Vec<String> {
    let g = glyphs(cx.settings.skin);
    let theme = cx.theme;
    let inner = (cx.width as usize).saturating_sub(2).max(1);
    let icx = at_width(cx, inner);
    let parts = tool_parts(t, &icx, inner);

    let subagent = if t.name == "subagent" && !t.args.is_null() {
        subagent_view(&t.args, theme)
    } else {
        None
    };
    let (title, mut body) = match subagent {
        Some((title, lines)) => (title, lines),
        None => {
            let lines: Vec<String> = parts
                .header
                .iter()
                .flat_map(|l| l.split('\n'))
                .map(str::to_string)
                .collect();
            match lines.split_first() {
                Some((first, rest)) => (first.clone(), rest.to_vec()),
                None => (String::new(), Vec::new()),
            }
        }
    };
    body.extend(parts.body.iter().cloned());

    let failed = parts.is_error || t.state == ToolState::Error;
    let (mark, mark_token) = if failed {
        (g.fail, "error")
    } else if t.state == ToolState::Done {
        (g.ok, "success")
    } else {
        (g.dot, "warning")
    };
    let frame = if failed {
        "error"
    } else if t.state == ToolState::Running {
        "border"
    } else {
        "borderMuted"
    };
    let head = if title.is_empty() {
        theme.fg(mark_token, mark)
    } else {
        format!("{} {title}", theme.fg(mark_token, mark))
    };
    framed(&head, &tool_status(t, &parts, cx), &body, frame, cx)
}

/// The status of a tool panel's bottom rule: `running` / `pending` / `streaming` while not
/// finished; `exit <code>` (non-zero), `timed out` and the duration (from 100 ms) when done.
fn tool_status(t: &ToolBlock, parts: &views::ToolParts, cx: &ViewCx) -> String {
    let theme = cx.theme;
    let mut bits: Vec<String> = Vec::new();
    match t.state {
        ToolState::Running => bits.push(theme.fg("muted", "running")),
        ToolState::Pending => bits.push(theme.fg("muted", "pending")),
        ToolState::Streaming => bits.push(theme.fg("muted", "streaming")),
        ToolState::Done | ToolState::Error => {
            if let Some(code) = parts.exit.filter(|c| *c != 0) {
                bits.push(theme.fg("error", &format!("exit {code}")));
            }
            if parts.timed_out {
                bits.push(theme.fg("error", "timed out"));
            }
            if let Some(ms) = parts.duration_ms.filter(|ms| *ms >= 100) {
                bits.push(theme.fg("muted", &views::fmt_duration(ms)));
            }
        }
    }
    let sep = theme.fg("muted", &format!(" {} ", glyphs(cx.settings.skin).mid));
    bits.join(&sep)
}

/// 5.2: the readable header of a `subagent` call: `subagent <agent>` + `task: <task>`
/// (single), `subagent parallel (<n>)` / `subagent chain (<n>)` + `<agent>: <task>` per entry.
/// `None` for any other shape.
fn subagent_view(args: &Value, theme: &Theme) -> Option<(String, Vec<String>)> {
    let name = theme.fg("toolTitle", &theme.bold("subagent"));
    if let Some(agent) = args.get("agent").and_then(Value::as_str) {
        let task = args.get("task").and_then(Value::as_str)?;
        let title = format!("{name} {}", theme.fg("accent", agent));
        return Some((title, vec![format!("task: {task}")]));
    }
    for (key, mode) in [("tasks", "parallel"), ("chain", "chain")] {
        let Some(items) = args.get(key).and_then(Value::as_array) else {
            continue;
        };
        if items.is_empty() {
            return None;
        }
        let mut lines = Vec::with_capacity(items.len());
        for item in items {
            let agent = item.get("agent").and_then(Value::as_str)?;
            let task = item.get("task").and_then(Value::as_str)?;
            lines.push(format!("{agent}: {task}"));
        }
        return Some((format!("{name} {mode} ({})", items.len()), lines));
    }
    None
}

/// 5.6: the checkpoint divider `── ◆ <label>[ · <key> to expand] ───`, exactly `W` columns.
fn checkpoint(label: &str, hint: bool, cx: &ViewCx) -> String {
    let g = glyphs(cx.settings.skin);
    let theme = cx.theme;
    let mut head = theme.fg("accent", &format!("{} {label}", g.diamond));
    if hint {
        let key = cx.key_label(EXPAND_ACTION);
        head.push_str(&theme.fg("muted", &format!(" {} {key} to expand", g.mid)));
    }
    let lead = format!("{}{} ", g.h, g.h);
    head_rule(&lead, &head, cx.width as usize, "borderMuted", g, theme)
}

/// 5.6: the compaction divider; expanded, the summary as the pi view at `W - 2` on the rail.
fn compaction_panel(
    summary: &str,
    tokens_before: u32,
    tokens_after: Option<u32>,
    expanded: bool,
    cx: &ViewCx,
) -> Vec<String> {
    let g = glyphs(cx.settings.skin);
    let label = match tokens_after {
        Some(after) => format!("COMPACTED from {tokens_before} to {after} tokens"),
        None => format!("COMPACTED from {tokens_before} tokens"),
    };
    let mut out = vec![checkpoint(&label, !expanded, cx)];
    if expanded {
        let inner = at_width(cx, (cx.width as usize).saturating_sub(2));
        let md = views::render_compaction_arrow(
            summary,
            tokens_before,
            tokens_after,
            true,
            if cx.settings.skin == Skin::OxaAscii {
                "->"
            } else {
                "→"
            },
            &inner,
        );
        out.extend(rail(md, g.v, "borderMuted", " ", cx.theme));
    }
    out
}

/// 5.6: the branch summary divider; expanded, the summary on the rail.
fn branch_panel(summary: &str, expanded: bool, cx: &ViewCx) -> Vec<String> {
    let g = glyphs(cx.settings.skin);
    let mut out = vec![checkpoint("BRANCH SUMMARY", !expanded, cx)];
    if expanded {
        let inner = at_width(cx, (cx.width as usize).saturating_sub(2));
        let md = views::render_branch_summary(summary, true, &inner);
        out.extend(rail(md, g.v, "borderMuted", " ", cx.theme));
    }
    out
}

/// 5.5: a reply shown but not sent. `notice` is a notice (5.7); other reasons get a panel with
/// the title by reason, the text and the line "not sent to the model; ask again".
fn shown_panel(text: &str, why: &str, cx: &ViewCx) -> Vec<String> {
    if why == shown_why::NOTICE {
        return notice_lines(Level::Info, text, cx);
    }
    let g = glyphs(cx.settings.skin);
    let theme = cx.theme;
    let (label, token, mark): (String, &str, &str) = match why {
        shown_why::FAILED => ("ERROR".to_string(), "error", g.fail),
        shown_why::ABORTED => ("ABORTED".to_string(), "warning", g.dot),
        shown_why::CUT => ("CUT".to_string(), "warning", g.dot),
        other => (other.to_uppercase(), "muted", g.dot),
    };
    let head = theme.fg(token, &theme.bold(&format!("{mark} {label}")));
    let mut body = styled_lines(text.trim(), "text", theme);
    let reason = if why == shown_why::ABORTED {
        "partial reply dropped; your prompt stays"
    } else {
        "not sent to the model; ask again"
    };
    body.push(theme.fg("muted", reason));
    framed(&head, "", &body, token, cx)
}

/// 5.7: a notice: mark and text, wrapped at `W - pad - 2`, continuation lines indented by two.
fn notice_lines(level: Level, text: &str, cx: &ViewCx) -> Vec<String> {
    let g = glyphs(cx.settings.skin);
    let (mark, token) = match level {
        Level::Info => (g.info, "muted"),
        Level::Warning => (g.warn, "warning"),
        Level::Error => (g.fail, "error"),
    };
    let pad = " ".repeat(opad(cx));
    let width = (cx.width as usize).saturating_sub(opad(cx) + 2).max(1);
    let mut out = Vec::new();
    for (i, line) in wrap_ansi(text, width).iter().enumerate() {
        let lead = if i == 0 {
            format!("{} ", cx.theme.fg(token, mark))
        } else {
            "  ".to_string()
        };
        out.push(format!("{pad}{lead}{}", cx.theme.fg(token, line)));
    }
    out
}
