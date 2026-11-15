//! D5: tool call and result views. Owner: task D5. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/components/{tool-execution.ts,diff.ts,visual-truncate.ts} and the per-tool renderers in src/core/tools/*.ts
use crate::{ToolBlock, ToolState, ViewCx};
use oxa_proto::{RenderKind, Value};
use oxa_tui::{highlight_code, language_from_path, strip_ansi, visible_width, wrap_ansi, Theme};
use serde_json::{json, Map};

/// The action whose key label is shown in the collapse hint.
const EXPAND_ACTION: &str = "app.tools.expand";

/// One tool block: a full-width box on `toolPendingBg` (streaming, pending, running),
/// `toolSuccessBg` (done) or `toolErrorBg` (error), holding the call header and the result.
///
/// * An extension renderer wins: `Hooks::render` with `RenderKind::ToolCall` /
///   `RenderKind::ToolResult` from `cx.hooks`, in order.
/// * Built-in tools use [`tool_header`] and [`tool_result_lines`]; unknown tools show the
///   name and the arguments as pretty JSON, and the result text.
/// * While `Streaming`, the header is drawn from `args_raw` best-effort (a partial path is
///   shown as soon as it is readable); never a panic on partial JSON.
/// * Collapsed (`!t.expanded && !cx.settings.tools_expanded`): at most
///   `cx.settings.collapsed_tool_lines` result lines plus a `... (<n> more lines)` hint;
///   live output of a running bash shows the **last** lines.
/// * `duration_ms` and the bash exit code come from the block / `result.ui` and are display
///   only (they are never on the wire, W4).
///
/// Acceptance (plan D5): per-tool call/result renderers; diff view; expand/collapse; live
/// output; error state.
pub fn render_tool(t: &ToolBlock, cx: &ViewCx) -> Vec<String> {
    let theme = cx.theme;
    let width = cx.width as usize;
    let pad = cx.settings.output_pad as usize;
    let inner = width.saturating_sub(2 * pad).max(1);
    let parts = tool_parts(t, cx, inner);
    let bg = if parts.is_error {
        "toolErrorBg"
    } else if t.state == ToolState::Done {
        "toolSuccessBg"
    } else {
        "toolPendingBg"
    };

    // Footer: duration and bash status, all from the block or `result.ui` (display only).
    let mut lines = parts.header;
    lines.extend(parts.body);
    if let Some(ms) = parts.duration_ms {
        lines.push(theme.dim(&format!("took {}", fmt_duration(ms))));
    }
    if let Some(code) = parts.exit {
        if code != 0 {
            lines.push(theme.fg("error", &format!("exit {code}")));
        }
    }
    if parts.timed_out {
        lines.push(theme.fg("error", "timed out"));
    }

    lines
        .iter()
        .flat_map(|l| wrap_ansi(l, inner))
        .map(|l| box_line(theme, bg, &l, width, pad))
        .collect()
}

/// The parts of one tool block laid out at `inner` columns (see [`render_tool`]): the call
/// header (its first line is the title), the wrapped and collapsed result or live body, and
/// the footer facts (display only): duration, bash exit code and timeout.
pub(crate) struct ToolParts {
    pub header: Vec<String>,
    pub body: Vec<String>,
    pub duration_ms: Option<u64>,
    pub exit: Option<i64>,
    pub timed_out: bool,
    pub is_error: bool,
}

/// The parts of a tool block, see [`ToolParts`]. `inner` is the layout width of the content;
/// `cx.width` is still the width the extension renderers get.
pub(crate) fn tool_parts(t: &ToolBlock, cx: &ViewCx, inner: usize) -> ToolParts {
    let theme = cx.theme;
    let inner = inner.max(1);
    // Everything inside the block is laid out at the inner width.
    let inner_cx = ViewCx {
        width: inner as u16,
        theme: cx.theme,
        settings: cx.settings,
        caps: cx.caps,
        hooks: cx.hooks,
        cwd: cx.cwd,
        home: cx.home,
        keymap: cx.keymap,
    };
    let expand_key = cx.key_label(EXPAND_ACTION);
    let expanded = t.expanded || cx.settings.tools_expanded;
    let max = cx.settings.collapsed_tool_lines;
    let args = header_args(t);
    let is_error = t.state == ToolState::Error || t.result.as_ref().is_some_and(|r| r.is_error);

    // Call header: extension renderer, then built-in header, then the generic view.
    let mut header: Vec<String> = Vec::new();
    let hooked_call = cx.hooks.iter().find_map(|h| {
        h.render(
            RenderKind::ToolCall,
            &json!({ "toolName": t.name, "args": args }),
            cx.width,
        )
    });
    match hooked_call {
        Some(l) => header.extend(l),
        None => match tool_header(&t.name, &args, &inner_cx) {
            Some(h) => header.push(h),
            None => {
                header.push(theme.fg("toolTitle", &theme.bold(&sanitize_display(&t.name))));
                let args_text = if t.args.is_null() {
                    sanitize_display(&t.args_raw)
                } else {
                    sanitize_display(&serde_json::to_string_pretty(&t.args).unwrap_or_default())
                };
                let args_lines: Vec<String> = args_text.lines().map(|l| theme.fg("muted", l)).collect();
                let (args_lines, _) = if expanded {
                    (args_lines, false)
                } else {
                    collapse_lines(args_lines, max, false, &expand_key, &inner_cx)
                };
                header.extend(args_lines);
            }
        },
    }

    // Result body: extension renderer, then the built-in view, then live output, then a hint.
    let mut from_end = false;
    let mut body: Vec<String> = Vec::new();
    match &t.result {
        Some(r) => {
            let hooked_result = cx.hooks.iter().find_map(|h| {
                h.render(
                    RenderKind::ToolResult,
                    &json!({
                        "toolName": t.name,
                        "args": args,
                        "result": r.text,
                        "isError": r.is_error,
                        "expanded": expanded,
                        "partial": false,
                    }),
                    cx.width,
                )
            });
            body = match hooked_result {
                Some(l) => l,
                None => tool_result_lines(&t.name, &args, &r.text, r.is_error, &r.ui, &inner_cx),
            };
        }
        None => match t.state {
            ToolState::Running if !t.partial.is_empty() => {
                // Live output: a running bash shows its last lines.
                from_end = t.name == "bash";
                let text = if t.name == "bash" {
                    strip_ansi(&t.partial)
                } else {
                    t.partial.clone()
                };
                body = plain_lines(&text, "toolOutput", theme);
            }
            ToolState::Running => body.push(theme.dim("running...")),
            ToolState::Pending => body.push(theme.dim("pending...")),
            ToolState::Streaming | ToolState::Done | ToolState::Error => {}
        },
    }

    // Wrap first so the collapse counts visual lines.
    let body: Vec<String> = body.iter().flat_map(|l| wrap_ansi(l, inner)).collect();
    let (body, _) = if expanded {
        (body, false)
    } else {
        collapse_lines(body, max, from_end, &expand_key, &inner_cx)
    };

    let (mut exit, mut timed_out) = (None, false);
    if let Some(r) = &t.result {
        if t.name == "bash" {
            exit = r.ui.get("exit").and_then(Value::as_i64);
            timed_out = r.ui.get("timedOut").and_then(Value::as_bool) == Some(true);
        }
    }
    ToolParts {
        header,
        body,
        duration_ms: t.duration_ms,
        exit,
        timed_out,
        is_error,
    }
}

/// The header text of a built-in tool call, styled, unwrapped: `read <path>[:<offset>-<end>]`,
/// `write <path>`, `edit <path>`, `bash` -> `$ <command>`, `grep <pattern> [in <path>]`,
/// `find <pattern> [in <path>]`, `ls <path>`. Paths under `cx.cwd` are shown relative, the
/// home directory as `~`. `None` for a tool that is not built in.
pub fn tool_header(name: &str, args: &Value, cx: &ViewCx) -> Option<String> {
    let theme = cx.theme;
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(|v| sanitize_display(v).replace(['\n', '\t'], " "))
    };
    let n = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
    };
    let title = |t: &str| theme.fg("toolTitle", &theme.bold(t));
    let path_text = |p: Option<String>| match p {
        Some(p) if !p.is_empty() => theme.fg("accent", &display_path(&p, cx)),
        _ => theme.dim("..."),
    };
    let in_path = |p: Option<String>| match p {
        Some(p) if !p.is_empty() => format!(
            " {} {}",
            theme.dim("in"),
            theme.fg("accent", &display_path(&p, cx))
        ),
        _ => String::new(),
    };
    match name {
        "read" => {
            let mut text = format!("{} {}", title("read"), path_text(s("path")));
            let offset = n("offset");
            let limit = n("limit");
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1);
                match limit {
                    Some(l) => text.push_str(
                        &theme.dim(&format!(":{start}-{}", start.saturating_add(l).saturating_sub(1))),
                    ),
                    None => text.push_str(&theme.dim(&format!(":{start}"))),
                }
            }
            Some(text)
        }
        "write" => Some(format!("{} {}", title("write"), path_text(s("path")))),
        "edit" => Some(format!("{} {}", title("edit"), path_text(s("path")))),
        "bash" => {
            let cmd = s("command").unwrap_or_default();
            let shown = if cmd.is_empty() { theme.dim("...") } else { cmd };
            Some(format!("{} {}", title("$"), shown))
        }
        "grep" | "find" => {
            let pattern = s("pattern").unwrap_or_default();
            let shown = if pattern.is_empty() {
                theme.dim("...")
            } else {
                theme.fg("accent", &pattern)
            };
            Some(format!("{} {}{}", title(name), shown, in_path(s("path"))))
        }
        "ls" => {
            let p = s("path")
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| ".".to_string());
            Some(format!(
                "{} {}",
                title("ls"),
                theme.fg("accent", &display_path(&p, cx))
            ))
        }
        _ => None,
    }
}

/// The result body of a built-in tool, unwrapped and untruncated (the caller collapses):
/// `read` = highlighted file content by extension; `edit` = [`render_diff`] of
/// `ui["diff"]`; `write` = highlighted content; `bash` = output with ANSI stripped;
/// `grep` / `find` / `ls` = the text. An error result is the text in `error`.
pub fn tool_result_lines(
    name: &str,
    args: &Value,
    text: &str,
    is_error: bool,
    ui: &Value,
    cx: &ViewCx,
) -> Vec<String> {
    let theme = cx.theme;
    if is_error {
        return plain_lines(text, "error", theme);
    }
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    match name {
        "read" => highlight_code(&sanitize_display(text), language_from_path(path), theme),
        "write" => {
            let content = args.get("content").and_then(Value::as_str).unwrap_or(text);
            // A summary line, then the content as added (`+`) lines, so a write does not
            // look like the output of `cat`.
            let n = content.lines().count();
            let summary = format!(
                "{n} line{}, {} bytes",
                if n == 1 { "" } else { "s" },
                content.len()
            );
            let clean = sanitize_display(content);
            let diff: String = clean.lines().map(|l| format!("+{l}\n")).collect();
            let mut out = vec![theme.dim(&summary)];
            out.extend(render_diff(&diff, cx.width, cx));
            out
        }
        "edit" => match ui.get("diff").and_then(Value::as_str) {
            Some(diff) => render_diff(&sanitize_display(diff), cx.width, cx),
            None => plain_lines(text, "toolOutput", theme),
        },
        "bash" => plain_lines(&strip_ansi(text), "toolOutput", theme),
        // grep, find, ls and every other tool: the text as it is.
        _ => plain_lines(text, "toolOutput", theme),
    }
}

/// A unified-diff-style view of pi's edit diff string (lines prefixed `+`, `-` or space, with
/// line numbers): added in `toolDiffAdded`, removed in `toolDiffRemoved`, context in
/// `toolDiffContext`; a changed line pair gets intra-line emphasis (inverse) on the changed
/// words, as pi. Tabs are shown as 3 spaces. Lines are wrapped to `width`.
pub fn render_diff(diff: &str, width: u16, cx: &ViewCx) -> Vec<String> {
    let theme = cx.theme;
    let w = width as usize;
    let raw: Vec<String> = diff.lines().map(|l| l.replace('\t', "   ")).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let line = &raw[i];
        if line.starts_with('-') && raw.get(i + 1).is_some_and(|n| n.starts_with('+')) {
            let (old_styled, new_styled) = emphasize_pair(line, &raw[i + 1], theme);
            out.extend(wrap_ansi(&old_styled, w));
            out.extend(wrap_ansi(&new_styled, w));
            i += 2;
            continue;
        }
        let token = match line.chars().next() {
            Some('+') => "toolDiffAdded",
            Some('-') => "toolDiffRemoved",
            _ => "toolDiffContext",
        };
        out.extend(wrap_ansi(&theme.fg(token, line), w));
        i += 1;
    }
    out
}

/// Keep at most `max` lines: the first lines (`from_end == false`) or the last (`true`), plus
/// one dim hint line `... (<n> more lines, <key> to expand)` when something was cut. Returns
/// the lines and whether it cut.
pub fn collapse_lines(
    lines: Vec<String>,
    max: usize,
    from_end: bool,
    expand_key: &str,
    cx: &ViewCx,
) -> (Vec<String>, bool) {
    if lines.len() <= max {
        return (lines, false);
    }
    let hidden = lines.len() - max;
    let hint = cx
        .theme
        .dim(&format!("... ({hidden} more lines, {expand_key} to expand)"));
    if from_end {
        let mut out = vec![hint];
        out.extend(lines.into_iter().skip(hidden));
        (out, true)
    } else {
        let mut out: Vec<String> = lines.into_iter().take(max).collect();
        out.push(hint);
        (out, true)
    }
}

// ---- helpers ----------------------------------------------------------------------------------

/// The arguments used for the header: the parsed ones, or while streaming the string and
/// number fields that can already be read from `args_raw`.
fn header_args(t: &ToolBlock) -> Value {
    if !t.args.is_null() {
        return t.args.clone();
    }
    let mut m = Map::new();
    for key in ["path", "command", "pattern", "glob"] {
        if let Some(v) = partial_string(&t.args_raw, key) {
            m.insert(key.to_string(), Value::String(v));
        }
    }
    for key in ["offset", "limit"] {
        if let Some(v) = partial_number(&t.args_raw, key) {
            m.insert(key.to_string(), json!(v));
        }
    }
    Value::Object(m)
}

/// The value of `"key": "..."` in possibly incomplete JSON, as far as it has been streamed.
fn partial_string(raw: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)? + needle.len();
    let rest = raw[start..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    Some(out)
}

/// The digits of `"key": <number>` in possibly incomplete JSON.
fn partial_number(raw: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)? + needle.len();
    let rest = raw[start..].trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// A path for display: relative to the session cwd, the home directory as `~`.
fn display_path(p: &str, cx: &ViewCx) -> String {
    let cwd = cx.cwd.trim_end_matches('/');
    if !cwd.is_empty() {
        if p == cwd {
            return ".".to_string();
        }
        if let Some(rel) = p.strip_prefix(cwd).and_then(|r| r.strip_prefix('/')) {
            return rel.to_string();
        }
    }
    {
        let home = cx.home.trim_end_matches('/');
        if !home.is_empty() {
            if p == home {
                return "~".to_string();
            }
            if let Some(rest) = p.strip_prefix(home).and_then(|r| r.strip_prefix('/')) {
                return format!("~/{rest}");
            }
        }
    }
    p.to_string()
}

/// `text` without control characters except newline and tab, so untrusted tool text (arguments
/// from the model, live output, diffs) cannot write escape sequences to the terminal (#409).
fn sanitize_display(text: &str) -> String {
    text.chars()
        .filter(|&c| !c.is_control() || c == '\n' || c == '\t')
        .collect()
}

/// Lines of `text` in one token, tabs as 3 spaces; control characters are dropped.
fn plain_lines(text: &str, token: &str, theme: &Theme) -> Vec<String> {
    sanitize_display(text)
        .lines()
        .map(|l| theme.fg(token, &l.replace('\t', "   ")))
        .collect()
}

pub(crate) fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// One line of the tool box: one column of padding each side, filled to `width` with the
/// background `token`. Resets inside the line re-apply the background.
fn box_line(theme: &Theme, token: &str, text: &str, width: usize, pad: usize) -> String {
    let s = format!("{}{text}", " ".repeat(pad));
    if theme.is_plain() {
        return s;
    }
    let bg = theme.bg_code(token);
    let body = if bg.is_empty() {
        s
    } else {
        s.replace("\x1b[0m", &format!("\x1b[0m{bg}"))
    };
    let pad = width.saturating_sub(visible_width(&body));
    theme.bg(token, &format!("{body}{}", " ".repeat(pad)))
}

/// Split a diff line into its tag plus padded number and space (`-  9 `) and the content.
fn split_prefix(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut idx = 1.min(bytes.len());
    while idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }
    line.split_at(idx)
}

/// The removed and added line of a changed pair, with the changed middle words inverse.
fn emphasize_pair(old: &str, new: &str, theme: &Theme) -> (String, String) {
    let (old_prefix, old_text) = split_prefix(old);
    let (new_prefix, new_text) = split_prefix(new);
    let oc: Vec<char> = old_text.chars().collect();
    let nc: Vec<char> = new_text.chars().collect();
    let common_head = oc.iter().zip(nc.iter()).take_while(|(a, b)| a == b).count();
    let max_tail = oc.len().min(nc.len()) - common_head;
    let common_tail = oc
        .iter()
        .rev()
        .zip(nc.iter().rev())
        .take(max_tail)
        .take_while(|(a, b)| a == b)
        .count();
    let style = |prefix: &str, chars: &[char], token: &str| {
        let head: String = chars[..common_head].iter().collect();
        let mid: String = chars[common_head..chars.len() - common_tail].iter().collect();
        let tail: String = chars[chars.len() - common_tail..].iter().collect();
        let body = format!("{head}{}{tail}", theme.inverse(&mid));
        theme.fg(token, &format!("{prefix}{body}"))
    };
    (
        style(old_prefix, &oc, "toolDiffRemoved"),
        style(new_prefix, &nc, "toolDiffAdded"),
    )
}
