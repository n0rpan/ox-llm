//! U2: the chrome of the OXA skin: splash, HUD line, hint bar, activity line, window title.
//! Spec: OXA-UI-DESIGN.md sec. 4.
use crate::{Activity, BudgetMeter, BudgetScope, BudgetUnit, HeaderInfo, Skin, StatusModel, ViewModel};
use oxa_config::Keymap;
use oxa_tui::{truncate_to_width, visible_width, Theme};

use super::{badge, glyphs, left_right, meter, sprite_frame, Glyphs, Sprite};

/// What the hint bar should teach right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintContext {
    /// Nothing runs. `has_text`: the editor is not empty; `slash`: its text starts with `/`.
    Idle {
        has_text: bool,
        slash: bool,
        /// A Ctrl+C was just pressed on an empty editor: the next one quits.
        armed: bool,
    },
    /// A run is in progress (thinking, streaming, tool, compaction, retry, queue).
    Running,
    /// A dialog waits for an answer.
    Dialog,
    /// A selector is open (model, theme, settings, sessions, ...).
    Overlay,
    /// The transcript search bar is open.
    Search,
}

/// Below this width the splash has no frame and no logo.
const MIN_FRAME_WIDTH: usize = 44;

/// The startup splash (sec. 4.1): a double frame with the OXA logo and three info lines,
/// then the loaded resources. `theme_label` is e.g. `oxa-omarchy (tokyo-night)`.
/// `details == false` (`quietStartup: "header"`) draws the frame without the resource list.
pub fn oxa_splash(
    info: &HeaderInfo,
    theme_label: &str,
    details: bool,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Vec<String> {
    let g = glyphs(skin);
    let w = width as usize;

    // The three info lines (line 1 has a right part only inside the frame).
    let head = theme.bold(&theme.fg("text", &format!("OXA v{}", clean(&info.version))));
    let model = clean(&info.model);
    let engine = clean(&info.engine_url);
    let mut second = String::new();
    if info.model_remote {
        second.push_str(&theme.fg(
            "accent",
            &format!(
                "cloud {} {}",
                g.mid,
                clean(info.model_provider.unwrap_or("remote"))
            ),
        ));
        second.push_str(&theme.fg("muted", &format!(" {} ", g.mid)));
    } else if !model.is_empty() {
        second.push_str(&theme.fg("success", "local"));
        second.push_str(&theme.fg("muted", &format!(" {} ", g.mid)));
    }
    if !model.is_empty() {
        second.push_str(&theme.fg("text", &model));
    }
    if !engine.is_empty() {
        if !model.is_empty() {
            second.push_str(&theme.fg("muted", &format!(" {} ", g.mid)));
        }
        second.push_str(&theme.fg("muted", &engine));
    }
    let label = clean(theme_label);
    let third = if label.is_empty() {
        String::new()
    } else {
        theme.fg("muted", &format!("theme {label}"))
    };
    let info_lines = [head, second, third];

    if w < MIN_FRAME_WIDTH {
        return info_lines
            .iter()
            .filter(|line| !line.is_empty())
            .map(|line| truncate_to_width(line, w, "..."))
            .collect();
    }

    let content = w - 6;
    let logo_w = g.logo.iter().map(|row| visible_width(row)).max().unwrap_or(0);
    let v = theme.fg("borderAccent", g.dv);
    let edge =
        |left: &str, right: &str| theme.fg("borderAccent", &format!("{left}{}{right}", g.dh.repeat(w - 2)));
    let mut out = vec![edge(g.dtl, g.dtr)];
    for (logo_row, info_line) in g.logo.iter().zip(info_lines.iter()) {
        let logo = theme.bold(&theme.fg("accent", &pad_right(logo_row, logo_w)));
        // Line 1 carries the right-aligned "part of ox-llm", line 2 the author credit; each
        // is dropped when it does not fit.
        let right = match out.len() {
            1 => theme.fg("dim", "part of ox-llm"),
            2 => theme.fg("dim", "written by Oscar"),
            _ => String::new(),
        };
        let body = left_right(&format!("{logo}   {info_line}"), &right, content);
        out.push(format!("{v}  {}  {v}", pad_right(&body, content)));
    }
    out.push(edge(g.dbl, g.dbr));

    if details {
        let kinds: Vec<&(String, Vec<String>)> = info
            .resources
            .iter()
            .filter(|(_, names)| !names.is_empty())
            .collect();
        for (kind, names) in &kinds {
            let names: Vec<String> = names.iter().map(|n| clean(n)).filter(|n| !n.is_empty()).collect();
            let kind_cell = pad_right(&theme.fg("accent", &clean(kind).to_lowercase()), 10);
            let list = theme.fg("muted", &names.join(", "));
            out.push(truncate_to_width(&format!("  {kind_cell}{list}"), w, "..."));
        }
        if !kinds.is_empty() {
            out.push(String::new());
        }
    }
    out
}

/// The HUD line (sec. 4.2): `OXA | model | CTX meter | CACHE meter | BUDGET | tok/s | state |
/// mode`, exactly one line that fits `width`; segments are dropped by priority when narrow.
pub fn oxa_hud(st: &StatusModel, width: u16, theme: &Theme, skin: Skin) -> String {
    let g = glyphs(skin);
    let w = width as usize;
    let parts = HudParts::new(st, g);
    for drops in 0..10 {
        let line = hud_build(&parts, drops, &parts.model, g, theme);
        if visible_width(&line) <= w {
            return line;
        }
    }
    // Still too wide: shorten the model name (down to a readable stub) before the mode and the
    // location go, since those say what the session may do and where it runs.
    const MIN_MODEL: usize = 10;
    for drops in 10..HUD_DROPS {
        let base = visible_width(&hud_build(&parts, drops, "", g, theme));
        let room = w.saturating_sub(base);
        if room >= MIN_MODEL.min(visible_width(&parts.model)) {
            let short = truncate_to_width(&parts.model, room, "...");
            let line = hud_build(&parts, drops, &short, g, theme);
            if visible_width(&line) <= w {
                return line;
            }
        }
    }
    // Still too wide: the mode is never dropped (#202) and the location is kept (#343) as long
    // as it fits with the model name cut below the readable stub; only then does it go.
    let keep = HUD_DROPS - 1;
    let drops = if visible_width(&hud_build(&parts, keep, "", g, theme)) <= w {
        keep
    } else {
        HUD_DROPS
    };
    let base = visible_width(&hud_build(&parts, drops, "", g, theme));
    let room = w.saturating_sub(base);
    let short = truncate_to_width(&parts.model, room, "...");
    truncate_to_width(&hud_build(&parts, drops, &short, g, theme), w, "")
}

/// Number of HUD segments that can be dropped (sec. 4.2 drop order, 1st .. 12th).
const HUD_DROPS: usize = 12;

/// The HUD's data, cleaned and ready to lay out. Built once per call.
struct HudParts {
    speed: Option<String>,
    /// Cache hit: percent and fraction; `None` when unknown.
    cache: Option<(u64, f64)>,
    /// Context: percent, fraction and `used/n_ctx`; `None` when the window is unknown.
    ctx: Option<(u64, f64, String)>,
    repairs: u32,
    degraded: bool,
    model: String,
    level: Option<&'static str>,
    /// What the level reads as (`adaptive • low` for Claude 5.5); the token colours it.
    level_text: Option<String>,
    note: Option<String>,
    mode: Option<(&'static str, &'static str)>,
    /// Badge word and colour token of the activity; `None` when idle.
    state: Option<(&'static str, &'static str)>,
    spinner: &'static str,
    /// O-182: YOLO is on, with the count of auto-approved questions.
    yolo: Option<u64>,
    /// D4b: where the model runs: `local`, or `cloud · <provider>`.
    remote: Option<&'static str>,
    is_remote: bool,
    /// #873: the authoritative budget, when a front end projected one.
    budget: Option<BudgetMeter>,
}

impl HudParts {
    fn new(st: &StatusModel, g: &Glyphs) -> HudParts {
        let cache = match (st.last_cached, st.last_prompt) {
            (Some(cached), Some(prompt)) if prompt > 0 => {
                let cached = cached.min(prompt);
                let pct = (u64::from(cached) * 100 + u64::from(prompt) / 2) / u64::from(prompt);
                Some((pct, f64::from(cached) / f64::from(prompt)))
            }
            _ => None,
        };
        let ctx = if st.n_ctx == 0 {
            None
        } else {
            let used = u64::from(st.context_tokens);
            let n = u64::from(st.n_ctx);
            let pct = (used * 100 + n / 2) / n;
            let detail = format!("{}/{}", format_tokens(used), format_tokens(n));
            Some((pct, f64::from(st.context_tokens) / f64::from(st.n_ctx), detail))
        };
        let model = clean(&st.model);
        HudParts {
            speed: st
                .tok_per_s
                .filter(|t| t.is_finite())
                .map(|t| format!("{t:.1} tok/s")),
            cache,
            ctx,
            repairs: st.tool_repairs,
            degraded: st.degraded_context,
            model: if model.is_empty() {
                "no-model".to_string()
            } else {
                model
            },
            level: st.thinking.map(|level| level.as_str()),
            level_text: st.thinking_label_with(g.mid),
            note: clean_opt(st.thinking_note.as_deref()),
            mode: st.mode_label(),
            state: activity_style(&st.activity),
            // The HUD does not animate: the activity line holds the one moving sprite, so a
            // frame changes a handful of cells and nothing else on the screen.
            spinner: g.dot,
            yolo: st.yolo.then_some(st.yolo_approved),
            remote: st.model_provider,
            is_remote: st.model_remote,
            budget: st.budget,
        }
    }
}

/// The HUD with the first `drops` drops applied. A segment with drop number `n` is shown
/// while fewer than `n` drops were applied.
fn hud_build(p: &HudParts, drops: usize, model: &str, g: &Glyphs, theme: &Theme) -> String {
    let shown = |n: usize| drops < n;
    let sep = theme.fg("borderMuted", &format!(" {} ", g.sep));
    let mut segs: Vec<String> = vec![theme.bold(&theme.fg("accent", "OXA"))];

    // YOLO (O-182): never dropped, right after the title: the flame, the badge and the count
    // of auto-approved questions.
    if let Some(approved) = p.yolo {
        segs.push(format!(
            "{} {} {}",
            g.flame,
            badge("YOLO full access", "error", theme),
            theme.fg("warning", &approved.to_string())
        ));
    }

    // 2: the model, with the thinking note (7th) and the level (9th).
    let mut model_seg = theme.fg("text", model);
    if shown(9) {
        if let (Some(level), Some(label)) = (p.level, &p.level_text) {
            let text = format!(" {} {label}", g.mid);
            model_seg.push_str(&theme.fg(Theme::thinking_token(level), &text));
        }
    }
    if shown(7) {
        if let Some(note) = &p.note {
            model_seg.push_str(&theme.fg("muted", &format!(" {} {note}", g.mid)));
        }
    }
    segs.push(model_seg);

    // 3: context. Unknown window: `CTX ?`.
    match &p.ctx {
        None => segs.push(theme.fg("muted", "CTX ?")),
        Some((pct, fraction, detail)) => {
            let token = ctx_token(*pct);
            let cells = if shown(6) { 10 } else { 5 };
            let mut s = if shown(10) {
                format!(
                    "{} {} {}",
                    theme.fg("muted", "CTX"),
                    theme.fg(token, &meter(*fraction, cells, g)),
                    theme.fg(token, &format!("{pct}%"))
                )
            } else {
                // Narrow: the bar goes before the mode and the location do.
                format!(
                    "{} {}",
                    theme.fg("muted", "CTX"),
                    theme.fg(token, &format!("{pct}%"))
                )
            };
            if shown(4) {
                s.push_str(&format!(" {}", theme.fg("muted", detail)));
            }
            segs.push(s);
        }
    }

    // 4: cache hit, absent when unknown.
    if shown(8) {
        if let Some((pct, fraction)) = p.cache {
            let token = cache_token(pct);
            let mut s = theme.fg("muted", "CACHE");
            if shown(3) {
                s.push_str(&format!(" {}", theme.fg(token, &meter(fraction, 5, g))));
            }
            s.push_str(&format!(" {}", theme.fg(token, &format!("{pct}%"))));
            segs.push(s);
        }
    }

    // 4b: the budget (#873), beside CTX and CACHE. Dropped with the jobs count and before the
    // location, so a narrow terminal keeps the model, the state and where it runs.
    if shown(11) {
        if let Some(budget) = &p.budget {
            segs.push(budget_segment(budget, theme));
        }
    }

    // 5: decode speed.
    if shown(2) {
        if let Some(speed) = &p.speed {
            segs.push(theme.fg("text", speed));
        }
    }

    // 6: state. Never dropped.
    segs.push(match p.state {
        None => theme.fg("success", &format!("{} READY", g.dot)),
        Some((word, token)) => theme.fg(token, &format!("{} {word}", p.spinner)),
    });

    // 7: repairs, only when there are some.
    if p.repairs > 0 && shown(5) {
        segs.push(theme.fg("warning", &format!("repairs {}", p.repairs)));
    }

    // 7b: the last compaction had no model summary.
    if p.degraded && shown(5) {
        segs.push(theme.fg("warning", "no summary"));
    }

    // 8: permission mode (#202: never dropped).
    if let Some((label, token)) = p.mode {
        segs.push(theme.fg(token, label));
    }

    // 9: where the model runs (D4b); goes with the mode.
    if shown(12) {
        segs.push(if p.is_remote {
            theme.fg(
                "accent",
                &format!("cloud {} {}", g.mid, clean(p.remote.unwrap_or("remote"))),
            )
        } else {
            theme.fg("success", "local")
        });
    }

    format!(" {}", segs.join(&sep))
}

/// The hint bar (sec. 4.3): the keys that matter in `ctx`, from the live keymap, with the
/// place (`~/dir (branch) • session`) right-aligned when it fits. One line that fits `width`.
pub fn oxa_hint_bar(
    ctx: HintContext,
    st: &StatusModel,
    keymap: &Keymap,
    fullscreen: bool,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> String {
    let g = glyphs(skin);
    let w = width as usize;
    let items = hint_items(ctx, keymap, fullscreen, theme);
    let sep = theme.fg("borderMuted", &format!(" {} ", g.mid));
    let place = place_text(st, skin, w, theme);
    let place_w = visible_width(&place);
    let (plain, _) = take_items(&items, &sep, w.saturating_sub(1));
    let line = if place_w > 0 {
        // The place only shows beside at least the first two hints.
        let (left, shown) = take_items(&items, &sep, w.saturating_sub(1 + 2 + place_w));
        if shown >= items.len().min(2) {
            let pad = w.saturating_sub(1 + visible_width(&left) + place_w);
            format!(" {left}{}{place}", " ".repeat(pad))
        } else {
            format!(" {plain}")
        }
    } else {
        format!(" {plain}")
    };
    truncate_to_width(&line, w, "")
}

/// The hints of a context, in priority order; unbound actions are left out.
fn hint_items(ctx: HintContext, keymap: &Keymap, fullscreen: bool, theme: &Theme) -> Vec<String> {
    let act = |id: &str, label: &str| action_hint(keymap, id, label, "text", theme);
    let items: Vec<Option<String>> = match ctx {
        HintContext::Idle {
            has_text: false,
            armed,
            ..
        } => vec![
            if armed {
                action_hint(
                    keymap,
                    "app.clear",
                    "exit (press again within 2s)",
                    "warning",
                    theme,
                )
            } else {
                None
            },
            act("tui.input.submit", "send"),
            Some(hint("/", "commands", "text", theme)),
            act("app.model.select", "models"),
            act("app.thinking.cycle", "thinking"),
            act("app.mode.cycle", "mode"),
            act("app.tools.expand", "expand"),
            if fullscreen {
                act("tui.altScreen.search", "search")
            } else {
                None
            },
            if armed { None } else { act("app.exit", "exit") },
        ],
        HintContext::Idle {
            has_text: true,
            slash: false,
            ..
        } => vec![
            act("tui.input.submit", "send"),
            act("tui.input.newLine", "newline"),
            act("app.clear", "clear"),
            act("app.editor.external", "editor"),
        ],
        HintContext::Idle {
            has_text: true,
            slash: true,
            ..
        } => vec![
            act("tui.input.tab", "complete"),
            act("tui.input.submit", "run"),
            act("app.clear", "clear"),
        ],
        HintContext::Running => vec![
            action_hint(keymap, "app.interrupt", "cancel", "warning", theme),
            act("tui.input.submit", "steer"),
            act("app.message.followUp", "queue"),
            act("app.tools.expand", "expand"),
        ],
        HintContext::Dialog => vec![
            act("tui.select.confirm", "confirm"),
            act("tui.select.cancel", "cancel"),
        ],
        HintContext::Overlay => {
            let up = keymap.keys_for("tui.select.up").into_iter().next();
            let down = keymap.keys_for("tui.select.down").into_iter().next();
            let move_hint = match (up, down) {
                (Some(up), Some(down)) => Some(hint(&format!("{up}/{down}"), "move", "text", theme)),
                _ => None,
            };
            vec![
                move_hint,
                act("tui.select.confirm", "select"),
                act("tui.select.cancel", "close"),
                Some(hint("type", "to filter", "text", theme)),
            ]
        }
        HintContext::Search => vec![
            act("tui.altScreen.searchNext", "next"),
            act("tui.altScreen.searchPrevious", "previous"),
            act("tui.altScreen.searchClose", "close"),
        ],
    };
    items.into_iter().flatten().collect()
}

/// One hint: the key in `key_token`, the label in `muted`.
fn hint(key: &str, label: &str, key_token: &str, theme: &Theme) -> String {
    format!("{} {}", theme.fg(key_token, key), theme.fg("muted", label))
}

/// A hint for an action: its first key from the keymap; `None` when unbound.
fn action_hint(keymap: &Keymap, id: &str, label: &str, key_token: &str, theme: &Theme) -> Option<String> {
    let key = keymap.keys_for(id).into_iter().next()?;
    Some(hint(&key.to_string(), label, key_token, theme))
}

/// Joins as many `items` as fit `budget` columns (in order); returns the text and the count.
fn take_items(items: &[String], sep: &str, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut count = 0;
    for item in items {
        let candidate = if count == 0 {
            item.clone()
        } else {
            format!("{out}{sep}{item}")
        };
        if visible_width(&candidate) > budget {
            break;
        }
        out = candidate;
        count += 1;
    }
    (out, count)
}

/// The place: cwd (home shortened to `~`, cut from the left past a third of the width),
/// ` (branch)` and ` • session`. Empty when there is nothing to show.
fn place_text(st: &StatusModel, skin: Skin, width: usize, theme: &Theme) -> String {
    let cwd = clean(&short_home(&st.cwd, &st.home));
    let mut parts: Vec<String> = Vec::new();
    let cut = cut_left(&cwd, width / 3, ellipsis(skin));
    if !cut.is_empty() {
        parts.push(cut);
    }
    if let Some(branch) = clean_opt(st.git_branch.as_deref()) {
        parts.push(format!("({branch})"));
    }
    if let Some(name) = clean_opt(st.session_name.as_deref()) {
        parts.push(format!("{} {name}", bullet(skin)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        theme.fg("muted", &parts.join(" "))
    }
}

/// `home` replaced by `~` when `cwd` lies inside it; `home` empty = no shortening.
fn short_home(cwd: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return cwd.to_string();
    }
    if cwd == home {
        return "~".to_string();
    }
    match cwd.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => cwd.to_string(),
    }
}

/// The last columns of `text` that fit `max` (with `ell` in front when cut).
fn cut_left(text: &str, max: usize, ell: &str) -> String {
    if visible_width(text) <= max {
        return text.to_string();
    }
    let ell_w = visible_width(ell);
    if ell_w >= max {
        return truncate_to_width(ell, max, "");
    }
    let budget = max - ell_w;
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let cw = visible_width(&c.to_string());
        if used + cw > budget {
            break;
        }
        used += cw;
        kept.push(c);
    }
    kept.reverse();
    format!("{ell}{}", kept.into_iter().collect::<String>())
}

/// The activity line above the editor (sec. 4.4): spinner, state badge, detail, elapsed time
/// and the cancel key; `None` when idle. `st.activity_ms` is the time in the current state.
pub fn oxa_activity_line(
    st: &StatusModel,
    keymap: &Keymap,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Option<String> {
    oxa_activity_line_with(st, keymap, true, width, theme, skin)
}

/// The sprite of an activity (O-184): flames while YOLO is on, else by the kind of work.
fn activity_sprite(st: &StatusModel) -> Option<Sprite> {
    if st.yolo {
        return Some(Sprite::Fire);
    }
    match st.activity {
        Activity::Idle | Activity::Blocked => None,
        Activity::Thinking => Some(Sprite::Think),
        Activity::Streaming => Some(Sprite::Write),
        _ => Some(Sprite::Work),
    }
}

/// [`oxa_activity_line`] with the motion switch (O-184). With `motion` the line starts with
/// the activity's pixel sprite (thinking, writing, working, YOLO flames), one frame per
/// `SPRITE_FRAME_MS` of `st.activity_ms`. Without it (reduced motion), with a plain theme
/// (`NO_COLOR`, `TERM=dumb`) or the ASCII glyph set, there is no sprite: the line is the
/// static text badge, the detail and the elapsed time. Waiting for the user's answer never
/// animates. Idle: `None`, so nothing moves and nothing is redrawn.
pub fn oxa_activity_line_with(
    st: &StatusModel,
    keymap: &Keymap,
    motion: bool,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Option<String> {
    let lead = if motion && skin == Skin::Oxa && !theme.is_plain() {
        activity_sprite(st).map(|sprite| {
            let (_, token) = activity_style(&st.activity).unwrap_or(("", "muted"));
            let hot = if sprite == Sprite::Fire { "error" } else { token };
            theme.fg(hot, sprite_frame(sprite, st.activity_ms))
        })
    } else {
        None
    };
    activity_line_lead(st, keymap, lead, width, theme, skin)
}

/// The activity line behind `lead` (a sprite or a scene, already styled; `None` = the badge
/// comes first): badge, detail, elapsed time and the cancel key. `None` when idle.
pub(super) fn activity_line_lead(
    st: &StatusModel,
    keymap: &Keymap,
    lead: Option<String>,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Option<String> {
    let (word, token) = activity_style(&st.activity)?;
    let g = glyphs(skin);
    let mut tail: Vec<String> = Vec::new();
    if let Some(detail) = activity_detail(&st.activity, st.working_message.as_deref(), g) {
        tail.push(theme.fg("text", &detail));
    }
    if let Some(elapsed) = elapsed_text(st.activity_ms) {
        tail.push(theme.fg("muted", &elapsed));
    }
    if !matches!(st.activity, Activity::Blocked) {
        if let Some(key) = keymap.keys_for("app.interrupt").into_iter().next() {
            tail.push(format!(
                "{} {}",
                theme.fg("warning", &key.to_string()),
                theme.fg("muted", "to cancel")
            ));
        }
    }
    let mut line = match lead {
        Some(lead) => format!(" {lead} {}", badge(word, token, theme)),
        None => format!(" {}", badge(word, token, theme)),
    };
    if !tail.is_empty() {
        let sep = theme.fg("muted", &format!(" {} ", g.mid));
        line = format!("{line}  {}", tail.join(&sep));
    }
    Some(truncate_to_width(&line, usize::from(width), "..."))
}

/// The badge word and colour token of an activity (sec. 4.4); `None` when idle.
pub(super) fn activity_style(activity: &Activity) -> Option<(&'static str, &'static str)> {
    Some(match activity {
        Activity::Idle => return None,
        Activity::Prefill { .. } => ("PREFILL", "accent"),
        Activity::Thinking => ("THINKING", "thinkingHigh"),
        Activity::Streaming => ("WRITING", "accent"),
        Activity::Tool(_) => ("TOOL", "warning"),
        Activity::Compacting => ("COMPACTING", "accent"),
        Activity::Retrying { .. } => ("RETRY", "warning"),
        Activity::Blocked => ("WAITING", "warning"),
        Activity::Queued { .. } => ("QUEUED", "muted"),
    })
}

/// The detail text of an activity; `working` (an extension's text) replaces it for the
/// thinking, streaming and tool states.
fn activity_detail(activity: &Activity, working: Option<&str>, g: &Glyphs) -> Option<String> {
    let working = clean_opt(working);
    match activity {
        Activity::Idle | Activity::Compacting => None,
        Activity::Prefill { done, total, .. } => {
            let fraction = if *total > 0 {
                f64::from((*done).min(*total)) / f64::from(*total)
            } else {
                0.0
            };
            Some(format!("{done}/{total} {}", meter(fraction, 10, g)))
        }
        Activity::Thinking | Activity::Streaming => working,
        Activity::Tool(name) => working.or_else(|| clean_opt(Some(name))),
        Activity::Retrying {
            attempt,
            max_attempts,
            delay_ms,
        } => Some(format!(
            "{attempt}/{max_attempts} in {}s",
            delay_ms.div_ceil(1000)
        )),
        Activity::Blocked => Some("for your answer".to_string()),
        Activity::Queued { pos: Some(pos), .. } => Some(format!("#{pos}")),
        Activity::Queued { pos: None, .. } => None,
    }
}

/// `<n>s` from 1 s, `<m>m<ss>s` from 60 s; `None` under 1 s.
fn elapsed_text(ms: u64) -> Option<String> {
    let secs = ms / 1000;
    if secs == 0 {
        None
    } else if secs < 60 {
        Some(format!("{secs}s"))
    } else {
        Some(format!("{}m{:02}s", secs / 60, secs % 60))
    }
}

/// The footer of the skin: the HUD line, the hint bar and (when any) one line of extension
/// statuses. `vm.footer_override` replaces everything, as in the pi look.
#[allow(clippy::too_many_arguments)]
pub fn oxa_footer(
    vm: &ViewModel,
    ctx: HintContext,
    keymap: &Keymap,
    fullscreen: bool,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Vec<String> {
    if let Some(lines) = &vm.footer_override {
        return lines.clone();
    }
    let st = &vm.status;
    let mut out = vec![
        oxa_hud(st, width, theme, skin),
        oxa_hint_bar(ctx, st, keymap, fullscreen, width, theme, skin),
    ];
    let statuses: Vec<String> = st
        .ext_status
        .values()
        .map(|text| clean(text))
        .filter(|text| !text.is_empty())
        .collect();
    if !statuses.is_empty() {
        let line = format!(" {}", statuses.join(" "));
        out.push(truncate_to_width(
            &theme.fg("muted", &line),
            usize::from(width),
            "...",
        ));
    }
    out
}

/// The banner shown for a moment when YOLO is switched on (O-182): flames around `YOLO MODE`
/// and what it means, one line that fits `width`. `ms` is the time since it was switched on
/// (the flame frame); `motion == false` (reduced motion, a plain theme) draws no flames, only
/// the badge and the text.
pub fn oxa_yolo_banner(ms: u64, motion: bool, width: u16, theme: &Theme, skin: Skin) -> String {
    let g = glyphs(skin);
    let what = "no prompts, no sandbox, no deny-list or protected paths";
    let label = badge("YOLO MODE", "error", theme);
    let line = if motion && !theme.is_plain() && !g.flames.is_empty() {
        let frame = g.flames[(ms / 120) as usize % g.flames.len()];
        // Each flame cell takes the next of three hot colours, shifted with the frame.
        let shift = (ms / 120) as usize;
        let fire = |offset: usize| -> String {
            frame
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    let token = ["error", "thinkingXhigh", "warning"][(i + shift + offset) % 3];
                    theme.fg(token, &c.to_string())
                })
                .collect()
        };
        format!(" {} {label} {}  {}", fire(0), fire(1), theme.fg("warning", what))
    } else {
        format!(" {} {label}  {what}", g.flame)
    };
    truncate_to_width(&line, width as usize, "...")
}

/// The window title of the skin: `OXA - <session name or cwd basename>`;
/// `vm.title_override` wins. Control characters are dropped.
pub fn oxa_title_text(vm: &ViewModel) -> String {
    if let Some(title) = &vm.title_override {
        return strip_controls(title);
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
    format!("OXA - {}", strip_controls(&name))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The context colour: `success` below 70 %, `warning` from 70 %, `error` from 90 %.
fn ctx_token(pct: u64) -> &'static str {
    if pct >= 90 {
        "error"
    } else if pct >= 70 {
        "warning"
    } else {
        "success"
    }
}

/// The cache colour: `success` from 90 %, `warning` from 50 %, `error` below.
fn cache_token(pct: u64) -> &'static str {
    if pct >= 90 {
        "success"
    } else if pct >= 50 {
        "warning"
    } else {
        "error"
    }
}

/// #873: the budget part: `BUDGET day $ 8.10/10.00 81% near cap`. The unit and the scope are
/// always spelled out, so a number is never read as another kind of budget. An unavailable
/// settlement shows `?` instead of a zero, and an incomplete one marks the used value as a
/// lower bound (`8.10+`). The warning is a steady word and colour at 80 %, never a flash, a
/// forecast or a number the HUD computed itself.
fn budget_segment(b: &BudgetMeter, theme: &Theme) -> String {
    let scope = match b.scope {
        BudgetScope::Day => "day",
        BudgetScope::Run => "run",
        BudgetScope::Goal => "goal",
    };
    let unit = match b.unit {
        BudgetUnit::Usd => "$",
        BudgetUnit::Tokens => "tok",
    };
    let used = match b.used {
        Some(used) => format!(
            "{}{}",
            budget_number(used, b.unit),
            if b.incomplete { "+" } else { "" }
        ),
        None => "?".to_string(),
    };
    let body = match b.cap {
        Some(cap) => format!("{used}/{}", budget_number(cap, b.unit)),
        None => used,
    };
    let pct = budget_percent(b.used, b.cap);
    let token = budget_token(pct);
    let mut s = format!(
        "{} {} {} {}",
        theme.fg("muted", "BUDGET"),
        theme.fg("muted", scope),
        theme.fg("muted", unit),
        theme.fg(token, &body)
    );
    if let Some(pct) = pct {
        let displayed_pct = pct.round().max(0.0) as u64;
        s.push_str(&format!(" {}", theme.fg(token, &format!("{displayed_pct}%"))));
    }
    if let Some(word) = budget_warning(pct) {
        s.push_str(&format!(" {}", theme.fg(token, word)));
    }
    s
}

/// The unrounded used percentage of a budget, so warnings never trigger early. `None`
/// without a positive cap or a settled used value (a zero cap has no percentage).
fn budget_percent(used: Option<f64>, cap: Option<f64>) -> Option<f64> {
    let (used, cap) = (used?, cap?);
    if !used.is_finite() || !cap.is_finite() || cap <= 0.0 {
        return None;
    }
    Some(used / cap * 100.0)
}

/// The steady warning word of a budget: `near cap` from 80 %, `over cap` at or above the cap.
fn budget_warning(pct: Option<f64>) -> Option<&'static str> {
    match pct? {
        pct if pct >= 100.0 => Some("over cap"),
        pct if pct >= 80.0 => Some("near cap"),
        _ => None,
    }
}

/// The colour of a budget: `error` at or above the cap, `warning` from 80 %, else plain text.
fn budget_token(pct: Option<f64>) -> &'static str {
    match pct {
        Some(pct) if pct >= 100.0 => "error",
        Some(pct) if pct >= 80.0 => "warning",
        _ => "text",
    }
}

/// One budget number in its unit: USD with two decimals, tokens compact (`12k`, `1.5M`).
fn budget_number(value: f64, unit: BudgetUnit) -> String {
    match unit {
        BudgetUnit::Usd => format!("{value:.2}"),
        BudgetUnit::Tokens => format_tokens(value.max(0.0).round() as u64),
    }
}

/// The ellipsis of the skin (`...` in ASCII).
fn ellipsis(skin: Skin) -> &'static str {
    if skin == Skin::OxaAscii {
        "..."
    } else {
        "…"
    }
}

/// The bullet before the session name (`*` in ASCII).
fn bullet(skin: Skin) -> &'static str {
    if skin == Skin::OxaAscii {
        "*"
    } else {
        "•"
    }
}

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

/// One line of text: control characters become spaces and runs of spaces collapse.
fn clean(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `clean` of an optional text; `None` when the result is empty.
fn clean_opt(text: Option<&str>) -> Option<String> {
    text.map(clean).filter(|t| !t.is_empty())
}

/// Drops control characters only (window titles keep their spaces).
fn strip_controls(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// `text` padded with spaces to `width` columns (unchanged when already wider).
fn pad_right(text: &str, width: usize) -> String {
    let w = visible_width(text);
    if w >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - w))
    }
}
