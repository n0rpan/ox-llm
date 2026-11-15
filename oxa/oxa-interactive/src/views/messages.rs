//! D4: message views. Owner: task D4. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/components/{user-message.ts,assistant-message.ts,compaction-summary-message.ts,branch-summary-message.ts,custom-message.ts,bash-execution.ts}
//
// All functions are pure, return lines that fit `cx.width`, and never panic on any text.
// Image lines are the one exception to "fits the width": an inline image sequence is opaque
// (see `oxa_tui::is_image_line`) and is passed through untouched.
use crate::{ImageRef, ViewCx};
use oxa_proto::{shown_why, Level, RenderKind, StopReason, Value};
use oxa_tui::{
    image_fallback, render_image, render_markdown, strip_ansi, truncate_to_width, visible_width, wrap_ansi,
    ImageDims, MarkdownOpts,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The SGR full reset that closes every style of a line.
const RESET: &str = "\x1b[0m";
/// Most terminal rows an inline image may take.
const IMAGE_MAX_ROWS: u16 = 24;

/// A user message: Markdown on `userMessageBg` in `userMessageText`, full-width block with
/// one line of vertical padding. Attached images are drawn inline (kitty / iTerm2) or as the
/// fallback text when `cx.settings.block_images` or the terminal has no image protocol.
pub fn render_user(text: &str, images: &[ImageRef], cx: &ViewCx) -> Vec<String> {
    let mut out = Vec::new();
    let text = text.trim();
    if !text.is_empty() {
        let body = markdown(text, "userMessageText", false, cx.settings.output_pad, None, cx);
        out.extend(boxed(body, "userMessageBg", cx));
    }
    // Images come after the block. Their lines are opaque and are not fitted (cutting an image
    // sequence would break it); only the text lines above are fitted by `boxed`.
    for (i, img) in images.iter().enumerate() {
        out.extend(image_lines(img, i as u32 + 1, cx));
    }
    out
}

/// An assistant message: the thinking block first (italic, `thinkingText`; when
/// `cx.settings.hide_thinking` a single dim `Thinking...` line instead; nothing when
/// `thinking` is empty), then the text as Markdown. While `streaming`, partial Markdown must
/// render. `stop == Aborted` appends pi's `Aborted` line, `Error` appends `Error: <error>` in
/// `error`. Tool calls are separate blocks and are not drawn here. Empty text, empty
/// thinking and no error = no lines.
pub fn render_assistant(
    text: &str,
    thinking: &str,
    streaming: bool,
    stop: Option<StopReason>,
    error: Option<&str>,
    cx: &ViewCx,
) -> Vec<String> {
    // Partial Markdown already renders as it is; nothing else depends on the streaming flag.
    let _ = streaming;
    let mut out: Vec<String> = Vec::new();

    let thinking = thinking.trim();
    if !thinking.is_empty() {
        if cx.settings.hide_thinking {
            let label = cx.theme.italic(&cx.theme.fg("dim", "Thinking..."));
            out.extend(indent(vec![label], opad(cx)));
        } else {
            out.extend(markdown(
                thinking,
                "thinkingText",
                true,
                cx.settings.output_pad,
                None,
                cx,
            ));
        }
    }

    let text = text.trim();
    if !text.is_empty() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.extend(markdown(text, "text", false, cx.settings.output_pad, None, cx));
    }

    let notice: Option<String> = match stop {
        Some(StopReason::Length) => Some("Response was truncated before completion.".to_string()),
        Some(StopReason::Aborted) => Some(
            match error.filter(|e| !e.is_empty() && *e != "Request was aborted") {
                Some(e) => e.to_string(),
                None => "Operation aborted".to_string(),
            },
        ),
        Some(StopReason::Error) => {
            let msg = error.filter(|e| !e.is_empty()).unwrap_or("Unknown error");
            Some(format!("Error: {msg}"))
        }
        _ => None,
    };
    if let Some(msg) = notice {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.extend(styled_text(&msg, "error", opad(cx), cx));
    }
    fit(out, cx.width)
}

/// A compaction summary: collapsed = one `customMessageBg` block with the label
/// `[compaction]`, `Compacted from <n> tokens` and the expand key hint; expanded = the label
/// and the summary as Markdown. When `tokens_after` is known it is shown as the destination.
pub fn render_compaction(
    summary: &str,
    tokens_before: u32,
    tokens_after: Option<u32>,
    expanded: bool,
    cx: &ViewCx,
) -> Vec<String> {
    render_compaction_arrow(summary, tokens_before, tokens_after, expanded, "→", cx)
}

/// [`render_compaction`] with the arrow glyph chosen by the caller (`->` for the ASCII skin).
pub fn render_compaction_arrow(
    summary: &str,
    tokens_before: u32,
    tokens_after: Option<u32>,
    expanded: bool,
    arrow: &str,
    cx: &ViewCx,
) -> Vec<String> {
    let (collapsed, header) = match tokens_after {
        Some(after) => (
            format!("Compacted {tokens_before} {arrow} {after} tokens ("),
            format!("**Compacted {tokens_before} {arrow} {after} tokens**"),
        ),
        None => (
            format!("Compacted from {tokens_before} tokens ("),
            format!("**Compacted from {tokens_before} tokens**"),
        ),
    };
    summary_view("compaction", &collapsed, &header, summary, expanded, cx)
}

/// A branch summary: like [`render_compaction`] with the label `[branch]`.
pub fn render_branch_summary(summary: &str, expanded: bool, cx: &ViewCx) -> Vec<String> {
    summary_view(
        "branch",
        "Branch summary (",
        "**Branch Summary**",
        summary,
        expanded,
        cx,
    )
}

/// An extension message: first an extension renderer (`Hooks::render` with
/// `RenderKind::Message`, payload `{customType, content, details, expanded}`) from `cx.hooks`
/// in order; without one, the default block: label `[<custom_type>]` in `customMessageLabel`,
/// content as Markdown on `customMessageBg`.
pub fn render_custom(
    custom_type: &str,
    content: &str,
    details: &Value,
    expanded: bool,
    cx: &ViewCx,
) -> Vec<String> {
    let payload = serde_json::json!({
        "customType": custom_type,
        "content": content,
        "details": details,
        "expanded": expanded,
    });
    for hooks in cx.hooks {
        if let Some(lines) = hooks.render(RenderKind::Message, &payload, cx.width) {
            return fit(lines, cx.width);
        }
    }
    let label = cx
        .theme
        .fg("customMessageLabel", &cx.theme.bold(&format!("[{custom_type}]")));
    let mut lines = indent(vec![label, String::new()], opad(cx));
    lines.extend(markdown(
        content.trim(),
        "customMessageText",
        false,
        cx.settings.output_pad,
        None,
        cx,
    ));
    boxed(lines, "customMessageBg", cx)
}

/// A `!cmd` / `!!cmd` execution: `$ <command>` header in `bashMode`, the output (collapsed to
/// the last `cx.settings.collapsed_tool_lines` lines unless `expanded`, with a
/// `... (<n> earlier lines)` marker), then a status line: running, `exit <code>`, or
/// cancelled (`exit == None` and not running). `excluded` adds pi's "not sent to the model"
/// marker.
pub fn render_bash(
    command: &str,
    output: &str,
    exit: Option<i32>,
    excluded: bool,
    running: bool,
    expanded: bool,
    cx: &ViewCx,
) -> Vec<String> {
    let theme = cx.theme;
    let key_token = if excluded { "dim" } else { "bashMode" };
    let width = avail(cx, opad(cx));
    let lead = " ".repeat(opad(cx));
    let mut out = Vec::new();

    let header = format!("$ {}", command.replace(['\n', '\r'], " "));
    for line in wrap_ansi(&header, width) {
        out.push(format!("{lead}{}", theme.fg(key_token, &theme.bold(&line))));
    }

    let text = strip_ansi(output)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ");
    let lines: Vec<&str> = if text.is_empty() {
        Vec::new()
    } else {
        let mut v: Vec<&str> = text.split('\n').collect();
        if text.ends_with('\n') {
            v.pop();
        }
        v
    };
    let max = cx.settings.collapsed_tool_lines.max(1);
    let hidden = if !expanded && lines.len() > max {
        lines.len() - max
    } else {
        0
    };
    if hidden > 0 {
        out.push(format!(
            "{lead}{}",
            theme.fg("muted", &format!("... ({hidden} earlier lines)"))
        ));
    }
    for line in lines.iter().skip(hidden) {
        for wrapped in wrap_ansi(line, width) {
            out.push(format!("{lead}{}", theme.fg("muted", &wrapped)));
        }
    }

    let mut status = if running {
        theme.fg("muted", "Running...")
    } else if let Some(code) = exit {
        theme.fg(if code == 0 { "muted" } else { "error" }, &format!("exit {code}"))
    } else {
        theme.fg("warning", "(cancelled)")
    };
    if excluded {
        status.push_str("  ");
        status.push_str(&theme.fg("dim", "not sent to the model"));
    }
    out.push(format!("{lead}{status}"));
    fit(out, cx.width)
}

/// A reply that was shown but never sent (W2): the text as Markdown, dimmed, with a one-line
/// reason (`aborted`, `failed`, `cut`: "not sent to the model; ask again"). `why == notice`
/// renders as a plain notice.
pub fn render_shown(text: &str, why: &str, cx: &ViewCx) -> Vec<String> {
    if why == shown_why::NOTICE {
        return render_notice(Level::Info, text, cx);
    }
    let mut out = Vec::new();
    let text = text.trim();
    if !text.is_empty() {
        out.extend(markdown(text, "dim", false, cx.settings.output_pad, None, cx));
        out.push(String::new());
    }
    let (label, token) = match why {
        shown_why::ABORTED => ("aborted", "dim"),
        shown_why::FAILED => ("failed", "error"),
        shown_why::CUT => ("cut", "dim"),
        other => (other, "dim"),
    };
    let reason = if why == shown_why::ABORTED {
        format!("{label}: already sent; ask again")
    } else {
        format!("{label}: not sent to the model; ask again")
    };
    out.extend(styled_text(&reason, token, opad(cx), cx));
    fit(out, cx.width)
}

/// A one-line notice, wrapped: info in `muted`, warning in `warning`, error in `error`.
pub fn render_notice(level: Level, text: &str, cx: &ViewCx) -> Vec<String> {
    let token = match level {
        Level::Info => "muted",
        Level::Warning => "warning",
        Level::Error => "error",
    };
    fit(styled_text(text, token, opad(cx), cx), cx.width)
}

// ---- helpers ---------------------------------------------------------------------------------

/// Shared body of the compaction and branch summary views.
fn summary_view(
    kind: &str,
    collapsed: &str,
    header: &str,
    summary: &str,
    expanded: bool,
    cx: &ViewCx,
) -> Vec<String> {
    let theme = cx.theme;
    let label = theme.fg("customMessageLabel", &theme.bold(&format!("[{kind}]")));
    let mut lines = indent(vec![label, String::new()], opad(cx));
    if expanded {
        let md = format!("{header}\n\n{}", summary.trim());
        lines.extend(markdown(
            &md,
            "customMessageText",
            false,
            cx.settings.output_pad,
            None,
            cx,
        ));
    } else {
        let hint = format!(
            "{}{}",
            theme.fg("customMessageText", collapsed),
            theme.fg("dim", &cx.key_label("app.tools.expand")),
        );
        let line = format!("{hint}{}", theme.fg("customMessageText", " to expand)"));
        lines.extend(indent(vec![line], opad(cx)));
    }
    boxed(lines, "customMessageBg", cx)
}

/// `outputPad`: columns of padding on each side of a transcript block.
fn opad(cx: &ViewCx) -> usize {
    cx.settings.output_pad as usize
}

/// Markdown lines at `cx.width` with `pad` columns on each side.
fn markdown(src: &str, token: &str, italic: bool, pad: u16, bg: Option<&str>, cx: &ViewCx) -> Vec<String> {
    let opts = MarkdownOpts {
        code_block_indent: cx.settings.code_block_indent,
        code_block_prefix: cx.settings.code_block_prefix.clone(),
        hyperlinks: cx.settings.hyperlinks && cx.caps.hyperlinks,
        padding_x: pad,
        text_token: token.to_string(),
        bg_token: bg.map(str::to_string),
        italic,
    };
    render_markdown(src, cx.width, cx.theme, &opts)
}

/// Plain text wrapped to the width after `pad` columns on each side, styled with `token`.
fn styled_text(text: &str, token: &str, pad: usize, cx: &ViewCx) -> Vec<String> {
    let p = " ".repeat(pad);
    wrap_ansi(text, avail(cx, pad))
        .into_iter()
        .map(|l| format!("{p}{}", cx.theme.fg(token, &l)))
        .collect()
}

/// Columns left for content after `pad` columns on each side (at least one).
fn avail(cx: &ViewCx, pad: usize) -> usize {
    (cx.width as usize).saturating_sub(2 * pad).max(1)
}

fn indent(lines: Vec<String>, pad: usize) -> Vec<String> {
    let p = " ".repeat(pad);
    lines.into_iter().map(|l| format!("{p}{l}")).collect()
}

/// Cut every line to the width (text lines only).
fn fit(lines: Vec<String>, width: u16) -> Vec<String> {
    let w = width as usize;
    lines.into_iter().map(|l| truncate_to_width(&l, w, "")).collect()
}

/// A full-width block on background `bg`: one blank line above and below the content.
fn boxed(lines: Vec<String>, bg: &str, cx: &ViewCx) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(fill_line("", bg, cx));
    out.extend(lines.iter().map(|l| fill_line(l, bg, cx)));
    out.push(fill_line("", bg, cx));
    fit(out, cx.width)
}

/// Extend `line` with the background `bg` to the full width. Every full reset inside the line
/// is followed by the background again, so an inner style never leaves a gap in the fill.
fn fill_line(line: &str, bg: &str, cx: &ViewCx) -> String {
    let theme = cx.theme;
    if theme.is_plain() {
        return line.to_string();
    }
    let open = theme.bg_code(bg);
    let body = line.replace(RESET, &format!("{RESET}{open}"));
    let fill = (cx.width as usize).saturating_sub(visible_width(&body));
    format!("{open}{body}{}{RESET}", " ".repeat(fill))
}

/// One inline image, or its fallback text. Inline images are read from `img.path` when the
/// terminal has an image protocol and `images.blockImages` is off.
fn image_lines(img: &ImageRef, id: u32, cx: &ViewCx) -> Vec<String> {
    let dims = if img.w > 0 && img.h > 0 {
        Some(ImageDims { w: img.w, h: img.h })
    } else {
        None
    };
    let name = img.path.file_name().and_then(|n| n.to_str());
    let fallback = || {
        fit(
            vec![cx.theme.fg("dim", &image_fallback(&img.mime, dims, name))],
            cx.width,
        )
    };
    let Some(protocol) = cx.caps.images else {
        return fallback();
    };
    if cx.settings.block_images {
        return fallback();
    }
    match image_bytes(&img.path) {
        Some(data) => {
            let lines = render_image(
                &data,
                &img.mime,
                protocol,
                cx.width.min(cx.settings.image_width_cells.max(1)),
                IMAGE_MAX_ROWS,
                cx.caps.cell_px,
                id,
            );
            if lines.is_empty() {
                fallback()
            } else {
                lines
            }
        }
        None => fallback(),
    }
}

/// The bytes of an attached image, read once per UI thread and kept for the last few images:
/// a view runs on every frame and must not read the disk each time. An attachment file is
/// written once and never changes, so the lines stay a function of the block.
fn image_bytes(path: &Path) -> Option<Arc<Vec<u8>>> {
    const KEEP: usize = 8;
    thread_local! {
        static RECENT: RefCell<Vec<(PathBuf, Arc<Vec<u8>>)>> = const { RefCell::new(Vec::new()) };
    }
    RECENT.with(|cell| {
        let mut recent = cell.borrow_mut();
        if let Some(i) = recent.iter().position(|(p, _)| p == path) {
            let hit = recent.remove(i);
            let data = hit.1.clone();
            recent.push(hit);
            return Some(data);
        }
        let data = Arc::new(std::fs::read(path).ok()?);
        if recent.len() >= KEEP {
            recent.remove(0);
        }
        recent.push((path.to_path_buf(), data.clone()));
        Some(data)
    })
}
