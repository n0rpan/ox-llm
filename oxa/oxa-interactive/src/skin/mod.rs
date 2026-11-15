//! The OXA skin: OXA's own look for the interactive UI (OXA-UI-DESIGN.md). Architect-owned:
//! the skin choice, the glyph sets and the shared helpers (complete). The chrome is task U2
//! (`chrome.rs`), the transcript panels are task U3 (`panels.rs`).
//!
//! Rules for everything in this module:
//! * Pure functions of their arguments (no process environment, no clock, no I/O).
//! * Every returned line fits the given width (`oxa_tui::visible_width(line) <= width`) and
//!   is self-contained (styles opened on a line are closed on it).
//! * Colour only through `Theme` tokens. With `Theme::plain()` (NO_COLOR) the output carries
//!   no escape sequence at all and every state is still readable from text and glyphs.
//! * Glyphs only through [`Glyphs`] (so `Skin::OxaAscii` never emits a non-ASCII glyph of the
//!   skin; text from the model or the user is passed through as it is).
//! * The pi look is never touched: nothing here is called under `Skin::Pi`.
use crate::Skin;
use oxa_tui::{truncate_to_width, visible_width, Theme};

mod arcade;
mod batch;
mod chrome;
mod panels;
mod pixel;

pub use arcade::*;
pub use batch::*;
pub use chrome::*;
pub use panels::*;
pub use pixel::*;

/// The glyphs of the skin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Light frame (transcript panels): horizontal, vertical, top-left, bottom-left.
    pub h: &'static str,
    pub v: &'static str,
    pub tl: &'static str,
    pub bl: &'static str,
    /// Double frame (splash, selectors, dialogs): horizontal, vertical and the four corners.
    pub dh: &'static str,
    pub dv: &'static str,
    pub dtl: &'static str,
    pub dtr: &'static str,
    pub dbl: &'static str,
    pub dbr: &'static str,
    /// Meter cells: filled and empty.
    pub full: &'static str,
    pub empty: &'static str,
    /// The rail of a user message and of a thinking block.
    pub user_rail: &'static str,
    pub think_rail: &'static str,
    /// HUD segment separator.
    pub sep: &'static str,
    /// Status marks.
    pub ok: &'static str,
    pub fail: &'static str,
    pub dot: &'static str,
    pub info: &'static str,
    pub warn: &'static str,
    pub diamond: &'static str,
    /// Hint separator.
    pub mid: &'static str,
    /// The YOLO mark of the HUD and the flame frames of its banner (one per 120 ms).
    pub flame: &'static str,
    pub flames: &'static [&'static str],
    /// Spinner frames (one per 100 ms).
    pub spinner: &'static [&'static str],
    /// The three rows of the OXA logo.
    pub logo: [&'static str; 3],
}

const UNICODE: Glyphs = Glyphs {
    h: "─",
    v: "│",
    tl: "┌",
    bl: "└",
    dh: "═",
    dv: "║",
    dtl: "╔",
    dtr: "╗",
    dbl: "╚",
    dbr: "╝",
    full: "█",
    empty: "░",
    user_rail: "▌",
    think_rail: "┊",
    sep: "│",
    ok: "✓",
    fail: "✗",
    dot: "●",
    info: "·",
    warn: "!",
    diamond: "◆",
    mid: "·",
    flame: "🔥",
    flames: &["▁▃▅▇▅▃", "▃▅▇▅▃▁", "▅▇▅▃▁▃", "▇▅▃▁▃▅", "▅▃▁▃▅▇", "▃▁▃▅▇▅"],
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    logo: ["▄▀▀▄ ▀▄ ▄▀ ▄▀▀▄", "█  █   █   █▀▀█", "▀▄▄▀ ▄▀ ▀▄ █  █"],
};

const ASCII: Glyphs = Glyphs {
    h: "-",
    v: "|",
    tl: "+",
    bl: "+",
    dh: "=",
    dv: "|",
    dtl: "+",
    dtr: "+",
    dbl: "+",
    dbr: "+",
    full: "#",
    empty: ".",
    user_rail: ">",
    think_rail: ":",
    sep: "|",
    ok: "ok",
    fail: "x",
    dot: "*",
    info: "-",
    warn: "!",
    diamond: "*",
    mid: "-",
    flame: "^",
    flames: &["^^^^^^"],
    spinner: &["|", "/", "-", "\\"],
    logo: [" __  _  _  __ ", "|  |  \\/  |__|", "|__| _/\\_ |  |"],
};

/// The glyph set of `skin` (`Skin::Pi` gets the Unicode set; it never draws with it).
pub fn glyphs(skin: Skin) -> &'static Glyphs {
    match skin {
        Skin::OxaAscii => &ASCII,
        Skin::Pi | Skin::Oxa => &UNICODE,
    }
}

/// The skin of a session: the `ui.skin` setting (`"pi"`, `"oxa"`, anything else = `"auto"`),
/// where `auto` is OXA's look. `utf8 == false` (a locale without UTF-8) turns `Oxa` into
/// `OxaAscii`.
pub fn resolve_skin(setting: Option<&str>, utf8: bool) -> Skin {
    if setting.map(str::trim) == Some("ascii") {
        return Skin::OxaAscii;
    }
    let oxa = !matches!(setting.map(str::trim), Some("pi"));
    match (oxa, utf8) {
        (false, _) => Skin::Pi,
        (true, true) => Skin::Oxa,
        (true, false) => Skin::OxaAscii,
    }
}

/// `true` when the environment's locale is UTF-8: the first set of `LC_ALL`, `LC_CTYPE`,
/// `LANG` names `UTF-8` / `utf8` (any case). `TERM` `linux` or `dumb` is never UTF-8 for this purpose; no locale counts as UTF-8 (pure: `env` is the complete input).
pub fn env_is_utf8(env: &[(String, String)]) -> bool {
    let get = |k: &str| {
        env.iter()
            .find(|(n, v)| n == k && !v.is_empty())
            .map(|(_, v)| v.to_ascii_lowercase())
    };
    if matches!(get("TERM").as_deref(), Some("linux") | Some("dumb")) {
        return false;
    }
    match get("LC_ALL").or_else(|| get("LC_CTYPE")).or_else(|| get("LANG")) {
        Some(v) => v.contains("utf-8") || v.contains("utf8"),
        None => true,
    }
}

/// `true` when colour is off for this run: `NO_COLOR` is set and not empty, or `TERM=dumb`.
pub fn env_no_color(env: &[(String, String)]) -> bool {
    env.iter()
        .any(|(k, v)| (k == "NO_COLOR" && !v.is_empty()) || (k == "TERM" && v == "dumb"))
}

/// The small pixel animations of the activity line (O-184): one per kind of work. Every frame
/// of a sprite is [`SPRITE_CELLS`] columns wide, drawn from block elements only, and shown
/// for [`SPRITE_FRAME_MS`] (8 frames per second). No sound, ever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sprite {
    /// Thinking: a pixel bouncing between two walls.
    Think,
    /// Writing the reply: ink running along the line.
    Write,
    /// A tool runs (also prefill, compaction, retry, queue): the ox walks.
    Work,
    /// YOLO is on: flames.
    Fire,
}

/// Columns of a sprite frame.
pub const SPRITE_CELLS: usize = 6;
/// How long one sprite frame is shown.
pub const SPRITE_FRAME_MS: u64 = 125;

const THINK_FRAMES: [&str; 10] = [
    "▪     ", " ▪    ", "  ▪   ", "   ▪  ", "    ▪ ", "     ▪", "    ▪ ", "   ▪  ", "  ▪   ", " ▪    ",
];
const WRITE_FRAMES: [&str; 6] = ["█▒░   ", " █▒░  ", "  █▒░ ", "   █▒░", "░   █▒", "▒░   █"];
const WORK_FRAMES: [&str; 8] = [
    "▜█▙   ",
    " ▟█▛  ",
    "  ▜█▙ ",
    "   ▟█▛",
    "  ▜█▙ ",
    " ▟█▛  ",
    "▜█▙   ",
    "▟█▛   ",
];
const FIRE_FRAMES: [&str; 6] = ["▁▃▅▇▅▃", "▃▅▇▅▃▁", "▅▇▅▃▁▃", "▇▅▃▁▃▅", "▅▃▁▃▅▇", "▃▁▃▅▇▅"];

/// The frame of `sprite` after `ms` milliseconds of the current activity.
pub fn sprite_frame(sprite: Sprite, ms: u64) -> &'static str {
    let frames: &[&'static str] = match sprite {
        Sprite::Think => &THINK_FRAMES,
        Sprite::Write => &WRITE_FRAMES,
        Sprite::Work => &WORK_FRAMES,
        Sprite::Fire => &FIRE_FRAMES,
    };
    let index = (ms / SPRITE_FRAME_MS) as usize % frames.len().max(1);
    frames.get(index).copied().unwrap_or("      ")
}

/// `true` when the environment asks for no animation: `OXA_REDUCED_MOTION` set to anything
/// but empty or `0` (the `ui.reducedMotion` setting is the caller's other input).
pub fn env_reduced_motion(env: &[(String, String)]) -> bool {
    env.iter()
        .any(|(k, v)| k == "OXA_REDUCED_MOTION" && !v.is_empty() && v != "0")
}

/// The theme while YOLO is on (O-182): the accents and the frames turn hot (the theme's
/// orange and red: `thinkingXhigh` and `error`), everything else stays. A plain theme stays
/// plain. Switching YOLO off restores the theme this was derived from.
pub fn hot_theme(theme: &Theme) -> Theme {
    let mut hot = theme.clone();
    if hot.is_plain() {
        return hot;
    }
    let (orange, red) = (theme.color("thinkingXhigh"), theme.color("error"));
    for token in ["accent", "border", "modeAsk", "modeWorkspace"] {
        hot.colors.insert(token.to_string(), orange);
    }
    for token in ["borderAccent", "borderMuted"] {
        hot.colors.insert(token.to_string(), red);
    }
    hot
}

/// A meter of `cells` cells with `fraction` (clamped to 0..=1) of them filled, rounded to the
/// nearest cell; a fraction above 0 shows at least one filled cell. Unstyled.
pub fn meter(fraction: f64, cells: usize, g: &Glyphs) -> String {
    let f = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut filled = (f * cells as f64).round() as usize;
    if f > 0.0 && filled == 0 && cells > 0 {
        filled = 1;
    }
    let filled = filled.min(cells);
    format!("{}{}", g.full.repeat(filled), g.empty.repeat(cells - filled))
}

/// A state badge: bold inverse ` LABEL ` in `token`; `[LABEL]` with a plain theme.
pub fn badge(label: &str, token: &str, theme: &Theme) -> String {
    if theme.is_plain() {
        format!("[{label}]")
    } else {
        theme.fg(token, &theme.bold(&theme.inverse(&format!(" {label} "))))
    }
}

/// `left` and `right` on one line of exactly `width` columns when both fit with a gap of two;
/// else `left` alone, truncated with `...`. Never wider than `width`.
pub fn left_right(left: &str, right: &str, width: usize) -> String {
    let (lw, rw) = (visible_width(left), visible_width(right));
    if rw > 0 && lw + 2 + rw <= width {
        format!("{left}{}{right}", " ".repeat(width - lw - rw))
    } else {
        truncate_to_width(left, width, "...")
    }
}

/// Float `popup` over `frame` so that it ends right above row `base` (the editor's first
/// row): the rows it covers are replaced, nothing else moves and the frame keeps its height.
/// A popup taller than the room above `base` loses its top rows.
pub fn float_above(frame: &mut [String], base: usize, popup: &[String]) {
    let base = base.min(frame.len());
    let shown = popup.len().min(base);
    let (first, skip) = (base - shown, popup.len() - shown);
    for (row, line) in frame[first..base].iter_mut().zip(&popup[skip..]) {
        row.clone_from(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skin_defaults_to_oxa_and_the_setting_wins() {
        assert_eq!(resolve_skin(None, true), Skin::Oxa);
        assert_eq!(resolve_skin(Some("auto"), false), Skin::OxaAscii);
        assert_eq!(resolve_skin(Some("pi"), true), Skin::Pi);
        assert_eq!(resolve_skin(Some("oxa"), false), Skin::OxaAscii);
        assert_eq!(resolve_skin(Some("ascii"), true), Skin::OxaAscii);
    }

    #[test]
    fn ascii_glyphs_are_ascii() {
        let g = glyphs(Skin::OxaAscii);
        let all = [
            g.h,
            g.v,
            g.tl,
            g.bl,
            g.dh,
            g.dv,
            g.dtl,
            g.dtr,
            g.dbl,
            g.dbr,
            g.full,
            g.empty,
            g.user_rail,
            g.think_rail,
            g.sep,
            g.ok,
            g.fail,
            g.dot,
            g.info,
            g.warn,
            g.diamond,
            g.mid,
        ];
        assert!(all.iter().all(|s| s.is_ascii()));
        assert!(g.spinner.iter().all(|s| s.is_ascii()));
        assert!(g.logo.iter().all(|s| s.is_ascii()));
        let w = visible_width(g.logo[0]);
        assert!(g.logo.iter().all(|l| visible_width(l) == w));
        let u = glyphs(Skin::Oxa);
        let w = visible_width(u.logo[0]);
        assert!(u.logo.iter().all(|l| visible_width(l) == w));
    }

    #[test]
    fn sprites_keep_their_width_and_loop() {
        for sprite in [Sprite::Think, Sprite::Write, Sprite::Work, Sprite::Fire] {
            let mut seen = std::collections::BTreeSet::new();
            for step in 0..40u64 {
                let frame = sprite_frame(sprite, step * SPRITE_FRAME_MS);
                assert_eq!(visible_width(frame), SPRITE_CELLS, "{sprite:?} frame {step}");
                seen.insert(frame);
            }
            assert!(seen.len() >= 4, "{sprite:?} moves");
            assert_eq!(sprite_frame(sprite, 0), sprite_frame(sprite, SPRITE_FRAME_MS - 1));
            assert_eq!(sprite_frame(sprite, u64::MAX).chars().count(), SPRITE_CELLS);
        }
    }

    #[test]
    fn a_floating_popup_covers_rows_and_moves_none() {
        let mut frame: Vec<String> = (0..6).map(|i| format!("row{i}")).collect();
        let popup = vec!["A".to_string(), "B".to_string()];
        float_above(&mut frame, 4, &popup);
        assert_eq!(frame, ["row0", "row1", "A", "B", "row4", "row5"]);
        float_above(&mut frame, 1, &popup);
        assert_eq!(frame, ["B", "row1", "A", "B", "row4", "row5"]);
        float_above(&mut frame, 99, &[]);
        assert_eq!(frame.len(), 6);
    }

    #[test]
    fn reduced_motion_env() {
        let e = |v: &str| vec![("OXA_REDUCED_MOTION".to_string(), v.to_string())];
        assert!(env_reduced_motion(&e("1")) && env_reduced_motion(&e("true")));
        assert!(!env_reduced_motion(&e("")) && !env_reduced_motion(&e("0")) && !env_reduced_motion(&[]));
    }

    #[test]
    fn meter_rounds_and_clamps() {
        let g = glyphs(Skin::OxaAscii);
        assert_eq!(meter(0.0, 5, g), ".....");
        assert_eq!(meter(0.01, 5, g), "#....");
        assert_eq!(meter(0.5, 4, g), "##..");
        assert_eq!(meter(7.0, 3, g), "###");
        assert_eq!(meter(f64::NAN, 2, g), "..");
    }

    #[test]
    fn locale_and_no_color() {
        let e = |p: &[(&str, &str)]| -> Vec<(String, String)> {
            p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        assert!(env_is_utf8(&e(&[("LANG", "nb_NO.UTF-8")])));
        assert!(!env_is_utf8(&e(&[("LANG", "C")])));
        assert!(env_is_utf8(&e(&[("LANG", "C"), ("LC_ALL", "C.utf8")])));
        assert!(env_is_utf8(&e(&[("TERM", "xterm-256color")])));
        assert!(!env_is_utf8(&e(&[("TERM", "linux")])));
        assert!(!env_is_utf8(&e(&[("TERM", "dumb"), ("LANG", "en_US.UTF-8")])));
        assert!(env_no_color(&e(&[("NO_COLOR", "1")])));
        assert!(!env_no_color(&e(&[("NO_COLOR", "")])));
        assert!(env_no_color(&e(&[("TERM", "dumb")])));
    }
}
