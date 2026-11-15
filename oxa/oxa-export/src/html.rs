//! E12: fill pi's HTML template from a session.
// ported from pi packages/coding-agent/src/core/export-html/index.ts and tool-renderer.ts
use crate::template::{
    HIGHLIGHT_JS, MARKED_CJK_FRIENDLY_JS, MARKED_JS, TEMPLATE_CSS, TEMPLATE_HTML, TEMPLATE_JS,
};
use oxa_proto::{codes, Entry, EntryBody, OxaError, Value};
use oxa_session::{encode_entry, encode_header, SessionHeader};
use std::collections::{HashMap, HashSet};
use std::fs::{OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Colours of the export. The caller (interactive mode, CLI) resolves them from the active
/// theme; this crate does not depend on `oxa-tui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTheme {
    /// Theme name, shown nowhere, used only for the light/dark derivation fallback.
    pub name: String,
    /// `(token, css colour)` for every theme colour token, e.g. `("accent", "#8abeb7")`.
    /// Emitted as `--<token>: <colour>;` lines in token order as given (`{{THEME_VARS}}`).
    pub vars: Vec<(String, String)>,
    /// Explicit export colours of the theme's `export` section; `None` = derive from
    /// `userMessageBg` with pi's luminance rule (dark base -> darker page, light -> lighter).
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

impl ExportTheme {
    /// pi's built-in `dark` export colours; the default when the caller has no theme (RPC,
    /// `--export` without a terminal). Fully deterministic.
    pub fn dark() -> ExportTheme {
        // pi's dark theme resolved to hex (okhsl colours converted with pi's tui/src/oklab.ts).
        let vars = [
            ("accent", "#a798d7"),
            ("border", "#5fa8cc"),
            ("borderAccent", "#a08ed5"),
            ("borderMuted", "#768186"),
            ("success", "#68b78d"),
            ("error", "#ea7f81"),
            ("warning", "#cd9a22"),
            ("muted", "#9da5a9"),
            ("dim", "#7e888e"),
            ("text", "#dee0e1"),
            ("thinkingText", "#96a0a4"),
            ("selectedBg", "#213b49"),
            ("scrollbarTrack", "#484e52"),
            ("scrollbarThumb", "#97a0a5"),
            ("searchMatchBg", "#4e2f1b"),
            ("searchMatchText", "#9da5a9"),
            ("userMessageBg", "#213b49"),
            ("userMessageText", "#dee0e1"),
            ("customMessageBg", "#3a3055"),
            ("customMessageText", "#9da5a9"),
            ("customMessageLabel", "#a798d7"),
            ("toolPendingBg", "#34383a"),
            ("toolSuccessBg", "#254131"),
            ("toolErrorBg", "#5b282a"),
            ("toolTitle", "#dee0e1"),
            ("toolOutput", "#9da5a9"),
            ("mdHeading", "#cd9a22"),
            ("mdLink", "#69add0"),
            ("mdLinkUrl", "#9da5a9"),
            ("mdCode", "#a798d7"),
            ("mdCodeBlock", "#68b78d"),
            ("mdCodeBlockBorder", "#9da5a9"),
            ("mdQuote", "#9da5a9"),
            ("mdQuoteBorder", "#9da5a9"),
            ("mdHr", "#9da5a9"),
            ("mdListBullet", "#a798d7"),
            ("toolDiffAdded", "#68b78d"),
            ("toolDiffRemoved", "#ea7f81"),
            ("toolDiffContext", "#9da5a9"),
            ("syntaxComment", "#9da5a9"),
            ("syntaxKeyword", "#69add0"),
            ("syntaxFunction", "#cd9a22"),
            ("syntaxVariable", "#5db3ba"),
            ("syntaxString", "#de8d5a"),
            ("syntaxNumber", "#68b78d"),
            ("syntaxType", "#a798d7"),
            ("syntaxOperator", "#9da5a9"),
            ("syntaxPunctuation", "#9da5a9"),
            ("thinkingOff", "#6c767b"),
            ("thinkingMinimal", "#68808d"),
            ("thinkingLow", "#5489a4"),
            ("thinkingMedium", "#6185cc"),
            ("thinkingHigh", "#9776e5"),
            ("thinkingXhigh", "#de54c1"),
            ("thinkingMax", "#fe5462"),
            ("bashMode", "#5eb286"),
        ];
        ExportTheme {
            name: "dark".to_string(),
            vars: vars.iter().map(|(t, c)| (t.to_string(), c.to_string())).collect(),
            page_bg: Some("#21252c".to_string()),
            card_bg: Some("#282c34".to_string()),
            info_bg: Some("#4e2f1b".to_string()),
        }
    }
}

/// Pre-rendered HTML for one tool call of a custom (extension / MCP) tool, produced by the
/// caller from `Hooks::render` output passed through [`crate::ansi_lines_to_html`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTool {
    pub tool_call_id: String,
    /// HTML of the call header; `None` = template default.
    pub call_html: Option<String>,
    /// HTML of the collapsed result; `None` = template default.
    pub result_collapsed_html: Option<String>,
    /// HTML of the expanded result; `None` = template default.
    pub result_expanded_html: Option<String>,
}

/// Options of [`export_html`].
#[derive(Debug, Clone, PartialEq)]
pub struct ExportOpts {
    pub theme: ExportTheme,
    /// Display name of the session (`session_info`), if any.
    pub session_name: Option<String>,
    /// The frozen system prompt text and the tool list (`[{name, description, parameters}]`) of
    /// the session's latest epoch, shown in the header like pi's `systemPrompt` / `tools`.
    pub system_prompt: Option<String>,
    pub tools: Value,
    /// Custom-tool renderings, keyed by tool call id. Built-in tools are drawn by the template.
    pub rendered_tools: Vec<RenderedTool>,
    /// Id of the active leaf (the template opens on this branch). `None` = last entry.
    pub leaf: Option<String>,
}

impl Default for ExportOpts {
    fn default() -> Self {
        ExportOpts {
            theme: ExportTheme {
                name: String::new(),
                vars: Vec::new(),
                page_bg: None,
                card_bg: None,
                info_bg: None,
            },
            session_name: None,
            system_prompt: None,
            tools: Value::Null,
            rendered_tools: Vec::new(),
            leaf: None,
        }
    }
}

/// Build the complete HTML document.
///
/// * Input: the session header and **all** entries of the file in file order.
/// * Output: [`crate::TEMPLATE_HTML`] with every placeholder replaced once (order documented in
///   `template.rs`). `{{SESSION_DATA}}` is standard base64 (with padding) of the UTF-8 JSON
///   object `{"header":H,"entries":[E..],"leafId":L,"systemPrompt":S,"tools":T,"renderedTools":R}`
///   where `H` and `E` are pi v3 JSON (same bytes as the session file lines).
/// * An empty `opts.theme.vars` means [`ExportTheme::dark`].
/// * Errors: `invalid` when `entries` is empty or `opts.leaf` names no entry. Never panics on
///   odd content: text is embedded as data, never as markup (no `</script>` break-out: the
///   payload is base64).
/// * Deterministic: no timestamps other than those in the entries.
///
/// Acceptance (plan E12): pi's template filled from a fixture session; tool renderers; ANSI to
/// HTML; derived output name.
pub fn export_html(header: &SessionHeader, entries: &[Entry], opts: &ExportOpts) -> Result<String, OxaError> {
    let data = session_data_json(header, entries, opts)?;
    let (vars, body, container, info) = theme_css(&opts.theme);
    // Single pass per template: inserted values are never scanned for placeholders.
    let css = fill(
        TEMPLATE_CSS,
        &[
            ("{{THEME_VARS}}", vars.as_str()),
            ("{{BODY_BG}}", body.as_str()),
            ("{{CONTAINER_BG}}", container.as_str()),
            ("{{INFO_BG}}", info.as_str()),
        ],
    );
    let session_data = base64_encode(&data);
    Ok(fill(
        TEMPLATE_HTML,
        &[
            ("{{CSS}}", css.as_str()),
            ("{{JS}}", TEMPLATE_JS),
            ("{{SESSION_DATA}}", session_data.as_str()),
            ("{{MARKED_JS}}", MARKED_JS),
            ("{{MARKED_CJK_FRIENDLY_JS}}", MARKED_CJK_FRIENDLY_JS),
            ("{{HIGHLIGHT_JS}}", HIGHLIGHT_JS),
        ],
    ))
}

/// The session data JSON embedded by [`export_html`], before base64 (exposed for tests).
///
/// Oxa-only entries (`shown`, `mode_change`) are left out. A kept entry whose parent was left out
/// is re-parented to its nearest kept ancestor, so the template's tree stays connected; every
/// other entry is written with the bytes `encode_entry` gives for the session file.
pub fn session_data_json(
    header: &SessionHeader,
    entries: &[Entry],
    opts: &ExportOpts,
) -> Result<Vec<u8>, OxaError> {
    let last = entries.last().ok_or_else(|| {
        OxaError::new(codes::INVALID, "the session has no entries to export")
            .with_hint("send at least one message first")
    })?;
    let index: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let kept: HashSet<&str> = entries
        .iter()
        .filter(|e| is_exported(e))
        .map(|e| e.id.as_str())
        .collect();

    // The entry itself when kept, else its nearest kept ancestor (`None` at the root).
    let nearest_kept = |start: Option<&str>| -> Option<String> {
        let mut cur = start;
        for _ in 0..=entries.len() {
            let id = cur?;
            if kept.contains(id) {
                return Some(id.to_string());
            }
            match index.get(id) {
                Some(e) => cur = e.parent.as_deref(),
                // A parent that is not in the file: keep the reference as it is.
                None => return Some(id.to_string()),
            }
        }
        None
    };

    let leaf_start = match &opts.leaf {
        Some(id) => {
            if !index.contains_key(id.as_str()) {
                return Err(
                    OxaError::new(codes::INVALID, format!("no entry {id} to open the export on"))
                        .with_hint("use an entry id of the session"),
                );
            }
            id.as_str()
        }
        None => last.id.as_str(),
    };
    let leaf_id = nearest_kept(Some(leaf_start));

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"{\"header\":");
    encode_header(header, &mut out);
    out.extend_from_slice(b",\"entries\":[");
    let mut first = true;
    for e in entries.iter().filter(|e| is_exported(e)) {
        if !first {
            out.push(b',');
        }
        first = false;
        let parent = nearest_kept(e.parent.as_deref());
        if parent.as_deref() == e.parent.as_deref() {
            encode_entry(e, &mut out);
        } else {
            let mut moved = (*e).clone();
            moved.parent = parent;
            encode_entry(&moved, &mut out);
        }
    }
    out.extend_from_slice(b"],\"leafId\":");
    write_json_value(&leaf_id.map_or(Value::Null, Value::from), &mut out);
    if let Some(prompt) = &opts.system_prompt {
        out.extend_from_slice(b",\"systemPrompt\":");
        write_json_value(&Value::from(prompt.as_str()), &mut out);
    }
    if !opts.tools.is_null() {
        out.extend_from_slice(b",\"tools\":");
        write_json_value(&opts.tools, &mut out);
    }
    if !opts.rendered_tools.is_empty() {
        out.extend_from_slice(b",\"renderedTools\":");
        write_json_value(&rendered_tools_value(&opts.rendered_tools), &mut out);
    }
    out.push(b'}');
    Ok(out)
}

/// The `{{THEME_VARS}}` block and the three background colours `(vars, body, container, info)`.
/// Colour parsing accepts `#RRGGBB` and `rgb(r,g,b)`; anything else falls back to pi's dark
/// defaults.
pub fn theme_css(theme: &ExportTheme) -> (String, String, String, String) {
    let theme = if theme.vars.is_empty() {
        ExportTheme::dark()
    } else {
        theme.clone()
    };
    let base = theme
        .vars
        .iter()
        .find(|(token, _)| token == "userMessageBg")
        .map_or("#343541", |(_, colour)| colour.as_str());
    let (derived_body, derived_container, derived_info) = derive_export_colors(base);
    let body = theme.page_bg.unwrap_or(derived_body);
    let container = theme.card_bg.unwrap_or(derived_container);
    let info = theme.info_bg.unwrap_or(derived_info);

    let mut lines: Vec<String> = theme
        .vars
        .iter()
        .map(|(token, colour)| format!("--{token}: {colour};"))
        .collect();
    lines.push(format!("--exportPageBg: {body};"));
    lines.push(format!("--exportCardBg: {container};"));
    lines.push(format!("--exportInfoBg: {info};"));
    (lines.join("\n      "), body, container, info)
}

/// Standard base64 with padding (this crate has no base64 dependency; ~20 lines).
pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(TABLE[(n >> 18) as usize & 63]));
        out.push(char::from(TABLE[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

/// Write `html` to `out` (create or truncate, mode 0644, parent directories are not created).
/// Returns the absolute path written. Errors: `io`.
pub fn write_export(out: &Path, html: &str) -> Result<PathBuf, OxaError> {
    let path = if out.is_absolute() {
        out.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| OxaError::io("cannot read the working directory", &e))?
            .join(out)
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&path)
        .map_err(|e| OxaError::io(&format!("cannot write {}", path.display()), &e))?;
    file.set_permissions(Permissions::from_mode(0o644))
        .map_err(|e| OxaError::io(&format!("cannot set the mode of {}", path.display()), &e))?;
    file.write_all(html.as_bytes())
        .map_err(|e| OxaError::io(&format!("cannot write {}", path.display()), &e))?;
    Ok(path)
}

/// Entries the template shows: everything except the oxa-only `shown` and `mode_change` bodies.
fn is_exported(e: &Entry) -> bool {
    !matches!(e.body, EntryBody::Shown { .. } | EntryBody::ModeChange(_))
}

/// Replace each `{{NAME}}` of `template` by its value. One left-to-right pass: a value is never
/// scanned for placeholders, and unknown `{{...}}` text is kept.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match values.iter().find(|(name, _)| after.starts_with(name)) {
            Some((name, value)) => {
                out.push_str(value);
                rest = &after[name.len()..];
            }
            None => {
                out.push_str("{{");
                rest = &after[2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The `renderedTools` object: tool call id -> the three HTML fields that are set.
fn rendered_tools_value(tools: &[RenderedTool]) -> Value {
    let mut map = serde_json::Map::new();
    for t in tools {
        let mut m = serde_json::Map::new();
        if let Some(h) = &t.call_html {
            m.insert("callHtml".to_string(), Value::from(h.as_str()));
        }
        if let Some(h) = &t.result_collapsed_html {
            m.insert("resultHtmlCollapsed".to_string(), Value::from(h.as_str()));
        }
        if let Some(h) = &t.result_expanded_html {
            m.insert("resultHtmlExpanded".to_string(), Value::from(h.as_str()));
        }
        if !m.is_empty() {
            map.insert(t.tool_call_id.clone(), Value::Object(m));
        }
    }
    Value::Object(map)
}

fn write_json_value(v: &Value, out: &mut Vec<u8>) {
    serde_json::to_writer(out, v).expect("writing JSON to a Vec cannot fail");
}

/// `#RRGGBB` or `rgb(r,g,b)` as integers; `None` for anything else.
fn parse_rgb(s: &str) -> Option<(i64, i64, i64)> {
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let part = |i: usize| i64::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some((part(0)?, part(2)?, part(4)?));
    }
    let inner = s
        .strip_prefix("rgb")?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    let mut nums = inner.split(',').map(|p| {
        let p = p.trim();
        (!p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
            .then(|| p.parse::<i64>().ok())
            .flatten()
    });
    let (r, g, b) = (nums.next()??, nums.next()??, nums.next()??);
    nums.next().is_none().then_some((r, g, b))
}

/// pi's `adjustBrightness`: scale each channel, round, clamp to 0-255.
fn scaled(rgb: (i64, i64, i64), factor: f64) -> String {
    let f = |c: i64| js_round(c as f64 * factor).clamp(0, 255);
    format!("rgb({}, {}, {})", f(rgb.0), f(rgb.1), f(rgb.2))
}

/// JavaScript `Math.round` for the values used here (round half up).
fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// pi's `deriveExportColors`: `(page, card, info)` from a base colour. Light bases get a darker
/// page, dark bases a lighter one; unparseable bases get pi's dark defaults.
fn derive_export_colors(base: &str) -> (String, String, String) {
    let Some((r, g, b)) = parse_rgb(base) else {
        return (
            "rgb(24, 24, 30)".to_string(),
            "rgb(30, 30, 36)".to_string(),
            "rgb(60, 55, 40)".to_string(),
        );
    };
    let lin = |c: i64| {
        let s = c as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
    if luminance > 0.5 {
        (
            scaled((r, g, b), 0.96),
            base.to_string(),
            format!(
                "rgb({}, {}, {})",
                (r + 10).min(255),
                (g + 5).min(255),
                (b - 20).max(0)
            ),
        )
    } else {
        (
            scaled((r, g, b), 0.7),
            scaled((r, g, b), 0.85),
            format!("rgb({}, {}, {})", (r + 20).min(255), (g + 15).min(255), b),
        )
    }
}
