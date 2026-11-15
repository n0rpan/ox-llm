//! D6: footer, header and title. Owner: task D6. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/components/{footer.ts,status-indicator.ts,keybinding-hints.ts}
use crate::{Activity, StatusModel, ViewModel};
use oxa_config::Keymap;
use oxa_proto::Mode;
use oxa_tui::{truncate_to_width, visible_width, Theme};

/// pi's `formatTokens`: compact token counts (`999`, `8.0k`, `12k`, `1.5M`).
fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        format!("{}k", (n as f64 / 1_000.0).round() as u64)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}M", (n as f64 / 1_000_000.0).round() as u64)
    }
}

/// One status line: control characters become spaces, runs of spaces collapse.
fn sanitize_status(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The theme token of a permission mode.
fn mode_token(mode: Mode) -> &'static str {
    match mode {
        Mode::Plan => "modePlan",
        Mode::Workspace => "modeWorkspace",
        Mode::Ask => "modeAsk",
    }
}

/// The left half of line 2: tokens, cache, prefill, context, speed and mode.
fn stats_left(st: &StatusModel, theme: &Theme) -> String {
    let mut parts: Vec<String> = Vec::new();
    if st.total_in > 0 {
        parts.push(theme.dim(&format!("↑{}", format_tokens(st.total_in))));
    }
    if st.total_out > 0 {
        parts.push(theme.dim(&format!("↓{}", format_tokens(st.total_out))));
    }
    if let (Some(cached), Some(prompt)) = (st.last_cached, st.last_prompt) {
        if prompt > 0 {
            parts.push(theme.dim(&format!("cache {cached}/{prompt}")));
        }
    }
    if let Activity::Prefill { done, total, .. } = &st.activity {
        parts.push(theme.dim(&format!("prefill {done}/{total}")));
    }
    if let Some(text) = queued_text(&st.activity) {
        parts.push(theme.dim(&text));
    }
    let ctx = if st.n_ctx > 0 {
        let pct = st.context_tokens as f64 * 100.0 / st.n_ctx as f64;
        let text = format!("{pct:.1}%/{}", format_tokens(st.n_ctx as u64));
        if pct > 90.0 {
            theme.fg("error", &text)
        } else if pct > 70.0 {
            theme.fg("warning", &text)
        } else {
            theme.dim(&text)
        }
    } else {
        theme.dim(&format!("?/{}", format_tokens(0)))
    };
    let auto = if st.auto_compact { " (auto)" } else { "" };
    parts.push(format!("{ctx}{}", theme.dim(auto)));
    if let Some(tps) = st.tok_per_s {
        parts.push(theme.dim(&format!("{tps:.1} tok/s")));
    }
    if st.tool_repairs > 0 {
        parts.push(theme.fg("warning", &format!("repairs {}", st.tool_repairs)));
    }
    if st.degraded_context {
        parts.push(theme.fg("warning", "no summary"));
    }
    if let Some(mode) = st.mode {
        parts.push(theme.fg(mode_token(mode), mode.as_str()));
    }
    parts.join(" ")
}

/// The right half of line 2: model, thinking level and the thinking note.
fn stats_right(st: &StatusModel) -> String {
    let mut text = if st.model.is_empty() {
        "no-model".to_string()
    } else {
        st.model.clone()
    };
    if let Some(level) = st.thinking_label() {
        text.push_str(" • ");
        text.push_str(&level);
    }
    if let Some(note) = &st.thinking_note {
        text.push_str(" · ");
        text.push_str(&sanitize_status(note));
    }
    text
}

/// Join left and right parts on one line of `width` columns; the left part is cut first.
fn join_line(left: &str, right: &str, width: usize, theme: &Theme) -> String {
    const MIN_GAP: usize = 2;
    let right_w = visible_width(right);
    if right_w + MIN_GAP >= width {
        // Not even room for the left part: show what fits of the right part.
        return truncate_to_width(&theme.dim(right), width, "");
    }
    let avail = width - right_w - MIN_GAP;
    let left = truncate_to_width(left, avail, "...");
    let pad = width.saturating_sub(visible_width(&left) + right_w);
    format!("{left}{}{}", " ".repeat(pad), theme.dim(right))
}

/// The footer: two lines as pi (line 1: cwd, git branch, session name; line 2: token stats on
/// the left, model + thinking level on the right) plus one line of extension statuses when any
/// are set. oxa additions on line 2: context `%` of `n_ctx`, cache hit `cached/prompt` of the
/// last request, prefill progress while prefilling, `tok/s`, and the permission mode
/// (`modePlan` / `modeWorkspace` / `modeAsk` tokens). Each line fits `width`; the left part is
/// truncated first. `vm.footer_override` replaces everything.
pub fn render_footer(vm: &ViewModel, width: u16, theme: &Theme) -> Vec<String> {
    if let Some(lines) = &vm.footer_override {
        return lines.clone();
    }
    let st = &vm.status;
    let w = width as usize;
    let mut out = Vec::new();

    // Line 1: where we are.
    let mut pwd = sanitize_status(&st.cwd);
    if let Some(branch) = st
        .git_branch
        .as_deref()
        .map(sanitize_status)
        .filter(|b| !b.is_empty())
    {
        pwd = format!("{pwd} ({branch})");
    }
    if let Some(name) = st
        .session_name
        .as_deref()
        .map(sanitize_status)
        .filter(|n| !n.is_empty())
    {
        pwd = format!("{pwd} • {name}");
    }
    out.push(truncate_to_width(&theme.dim(&pwd), w, &theme.dim("...")));

    // Line 2: stats and model.
    out.push(join_line(&stats_left(st, theme), &stats_right(st), w, theme));

    // Extension statuses, one line, keys in sorted order (BTreeMap).
    let statuses: Vec<String> = st
        .ext_status
        .values()
        .map(|t| sanitize_status(t))
        .filter(|t| !t.is_empty())
        .collect();
    if !statuses.is_empty() {
        out.push(truncate_to_width(&statuses.join(" "), w, &theme.dim("...")));
    }
    out
}

/// What the startup header shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeaderInfo {
    pub version: String,
    /// Loaded context files, skills, templates, extensions, themes: `(kind, names)`.
    pub resources: Vec<(String, Vec<String>)>,
    pub engine_url: String,
    pub model: String,
}

/// The startup header: logo line, key hints from the keymap, loaded resources. With
/// `quiet == true` (`quietStartup`) it is empty. `vm.header_override` replaces it.
pub fn render_header(
    info: &HeaderInfo,
    vm: &ViewModel,
    keymap: &Keymap,
    quiet: bool,
    width: u16,
    theme: &Theme,
) -> Vec<String> {
    if quiet {
        return Vec::new();
    }
    let w = width as usize;
    if let Some(lines) = &vm.header_override {
        return lines.clone();
    }
    let mut out = Vec::new();
    out.push(truncate_to_width(
        &theme.bold(&format!("oxa v{}", info.version)),
        w,
        "...",
    ));
    let hints = key_hints(
        &[
            ("app.interrupt", "interrupt"),
            ("app.clear", "clear"),
            ("app.exit", "exit"),
            ("app.model.select", "models"),
            ("app.session.new", "new session"),
            ("app.session.resume", "resume"),
            ("app.thinking.cycle", "thinking"),
            ("app.mode.cycle", "mode"),
        ],
        keymap,
        width,
        theme,
    );
    if !hints.is_empty() {
        out.push(hints);
    }
    let model = if info.model.is_empty() {
        String::new()
    } else {
        format!(" · {}", info.model)
    };
    out.push(truncate_to_width(
        &theme.dim(&format!("{}{model}", info.engine_url)),
        w,
        "...",
    ));
    // pi `showLoadedResources`: a blank line, then per section `[Name]` (mdHeading) and the
    // compact list (dim, indented), each followed by a blank line.
    let sections: Vec<&(String, Vec<String>)> = info
        .resources
        .iter()
        .filter(|(_, names)| !names.is_empty())
        .collect();
    if !sections.is_empty() {
        out.push(String::new());
    }
    for (kind, names) in sections {
        out.push(truncate_to_width(
            &theme.fg("mdHeading", &format!("[{kind}]")),
            w,
            "...",
        ));
        out.push(truncate_to_width(
            &theme.dim(&format!("  {}", names.join(", "))),
            w,
            "...",
        ));
        out.push(String::new());
    }
    out
}

/// The loader line text for the current activity (`Working...`, `Prefill 1200/8000 (3s)`,
/// `Compacting...`, `Retrying in 2s (1/3)`), `None` when idle. `working_message` overrides the
/// default text of the working states.
pub fn activity_text(status: &StatusModel) -> Option<String> {
    let working = |default: String| status.working_message.clone().unwrap_or(default);
    match &status.activity {
        Activity::Idle => None,
        Activity::Prefill { done, total, secs } => Some(format!("Prefill {done}/{total} ({secs}s)")),
        Activity::Thinking | Activity::Streaming => Some(working("Working...".to_string())),
        Activity::Tool(name) => Some(working(format!("Running {name}..."))),
        Activity::Compacting => Some("Compacting...".to_string()),
        Activity::Retrying {
            attempt,
            max_attempts,
            delay_ms,
        } => {
            let secs = delay_ms.div_ceil(1000);
            Some(format!("Retrying in {secs}s ({attempt}/{max_attempts})"))
        }
        Activity::Blocked => Some("Waiting for input...".to_string()),
        Activity::Queued { .. } => queued_text(&status.activity),
    }
}

/// `queued #<pos>` (the controller reports the place) or `queued <wait_s>s` while a request
/// waits for the engine's one slot (S2); `None` for any other activity.
pub fn queued_text(activity: &Activity) -> Option<String> {
    match activity {
        Activity::Queued { pos: Some(pos), .. } => Some(format!("queued #{pos}")),
        Activity::Queued { pos: None, wait_s } => Some(format!("queued {wait_s}s")),
        _ => None,
    }
}

/// The window title: `oxa - <session name or cwd basename>`; `vm.title_override` wins.
pub fn title_text(vm: &ViewModel) -> String {
    if let Some(title) = &vm.title_override {
        return title.clone();
    }
    let st = &vm.status;
    let name = st
        .session_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| {
            std::path::Path::new(&st.cwd)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| st.cwd.clone());
    // A title is written into an escape sequence by the caller: drop control characters.
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    format!("oxa - {clean}")
}

/// The theme token of the editor border: the thinking-level token, or `bashMode` when the
/// editor text starts with `!`.
pub fn editor_border_token(status: &StatusModel, editor_text: &str) -> &'static str {
    if editor_text.starts_with('!') {
        return "bashMode";
    }
    match status.thinking {
        Some(level) => Theme::thinking_token(level.as_str()),
        None => "border",
    }
}

/// A dim one-line hint list built from the keymap: `(action id, label)` pairs become
/// `key label · key label`, truncated to `width`.
pub fn key_hints(pairs: &[(&str, &str)], keymap: &Keymap, width: u16, theme: &Theme) -> String {
    let items: Vec<String> = pairs
        .iter()
        .filter_map(|(action, label)| {
            let keys = keymap.keys_for(action);
            if keys.is_empty() {
                return None;
            }
            let key_text = keys.iter().map(|k| k.to_string()).collect::<Vec<_>>().join("/");
            Some(theme.dim(&format!("{key_text} {label}")))
        })
        .collect();
    let line = items.join(&theme.dim(" · "));
    truncate_to_width(&line, width as usize, &theme.dim("..."))
}
