//! U1: the OXA themes. `oxa` is a fixed built-in palette; `oxa-omarchy` follows the desktop:
//! it reads the CURRENT Omarchy theme palette (`colors.toml`, read-only) and maps it to oxa's
//! theme tokens.
//!
//! Nothing here reads the process environment (P56) or writes anything: the caller passes the
//! home and XDG directories. No Omarchy file is ever modified.
use crate::{Appearance, Color, ColorMode, Theme, THEME_TOKENS};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// An RGB triple.
pub type Rgb = (u8, u8, u8);

/// The names of the OXA themes, in selector order (listed after the pi built-ins).
pub const OXA_THEME_NAMES: [&str; 2] = ["oxa", "oxa-omarchy"];

/// The largest `colors.toml` that is read (bytes); a larger file is ignored.
pub const OMARCHY_COLORS_MAX_BYTES: u64 = 64 * 1024;

/// A desktop palette: the colours an OXA theme is built from. Every field is always set
/// (the parser derives what a file leaves out, see [`parse_omarchy_colors`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmarchyPalette {
    /// The Omarchy theme name (`tokyo-night`), when known. Display only.
    pub name: Option<String>,
    /// `true` for a light theme (`mode = "light"`, else the luminance of `background`).
    pub light: bool,
    pub background: Rgb,
    pub lighter_background: Rgb,
    pub foreground: Rgb,
    pub dark_foreground: Rgb,
    pub bright_foreground: Rgb,
    pub accent: Rgb,
    pub selection: Rgb,
    pub muted: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub orange: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
}

/// Parse an Omarchy `colors.toml`. See the design doc (OXA-UI-DESIGN.md sec. 3.1) for the
/// accepted keys, both file generations and the derivation of missing colours.
/// `None` when `background` or `foreground` is missing or not `#RRGGBB`.
pub fn parse_omarchy_colors(text: &str) -> Option<OmarchyPalette> {
    let mut colors: HashMap<&str, Rgb> = HashMap::new();
    let mut mode: Option<&str> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let Some(value) = quoted_value(rest.trim()) else {
            continue;
        };
        let key = key.trim();
        if key == "mode" {
            mode = Some(value);
        } else if let Some(rgb) = parse_hex_color(value.trim()) {
            colors.insert(key, rgb);
        }
    }

    let bg = *colors.get("background")?;
    let fg = *colors.get("foreground")?;
    let builtin = oxa_builtin_palette();
    let get = |keys: &[&str]| keys.iter().find_map(|k| colors.get(k).copied());
    let named = |name: &str, old: &str, fallback: Rgb| get(&[name, old]).unwrap_or(fallback);

    let red = named("red", "color1", builtin.red);
    let green = named("green", "color2", builtin.green);
    let yellow = named("yellow", "color3", builtin.yellow);
    let blue = named("blue", "color4", builtin.blue);
    let magenta = named("magenta", "color5", builtin.magenta);
    let cyan = named("cyan", "color6", builtin.cyan);
    let light = match mode {
        Some(m) if m.eq_ignore_ascii_case("light") => true,
        Some(m) if m.eq_ignore_ascii_case("dark") => false,
        _ => luminance(bg) > 0.5,
    };

    Some(OmarchyPalette {
        name: None,
        light,
        background: bg,
        lighter_background: get(&["lighter_background"]).unwrap_or_else(|| mix_rgb(bg, fg, 0.07)),
        foreground: fg,
        dark_foreground: get(&["dark_foreground"]).unwrap_or_else(|| mix_rgb(bg, fg, 0.50)),
        bright_foreground: get(&["bright_foreground", "color15"]).unwrap_or(fg),
        accent: get(&["accent"]).unwrap_or(blue),
        // The older generation's selection colour is a full accent (made for dark text on
        // it); as a row background under normal text it is toned down toward the background.
        selection: get(&["selection"])
            .or_else(|| get(&["selection_background"]).map(|c| mix_rgb(bg, c, 0.30)))
            .unwrap_or_else(|| mix_rgb(bg, fg, 0.15)),
        muted: get(&["muted", "color8"]).unwrap_or_else(|| mix_rgb(bg, fg, 0.25)),
        red,
        green,
        yellow,
        orange: get(&["orange", "bright_yellow"]).unwrap_or(yellow),
        blue,
        magenta,
        cyan,
    })
}

/// The text between the quotes of a value that starts with `"` or `'`. Anything after the
/// closing quote (a trailing comment) is ignored; an unquoted or unclosed value is `None`.
fn quoted_value(rest: &str) -> Option<&str> {
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body = rest.get(1..)?;
    body.get(..body.find(quote)?)
}

/// `#RRGGBB` or `#RGB` (either case). `None` for anything else.
fn parse_hex_color(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Every byte is an ASCII hex digit, so the slices below fall on character boundaries.
    let channel = |start: usize, len: usize| -> Option<u8> {
        let v = u8::from_str_radix(hex.get(start..start + len)?, 16).ok()?;
        Some(if len == 1 { v * 17 } else { v })
    };
    match hex.len() {
        3 => Some((channel(0, 1)?, channel(1, 1)?, channel(2, 1)?)),
        6 => Some((channel(0, 2)?, channel(2, 2)?, channel(4, 2)?)),
        _ => None,
    }
}

/// Relative luminance on 0..1 channel values (no gamma), as in the design doc.
fn luminance((r, g, b): Rgb) -> f64 {
    let ch = |v: u8| f64::from(v) / 255.0;
    0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
}

/// Mix two colours: per channel `round(a + (b - a) * t)`.
pub fn mix_rgb(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let f = |x: u8, y: u8| {
        (f64::from(x) + (f64::from(y) - f64::from(x)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// `#rrggbb` (lowercase).
fn css_hex((r, g, b): Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Where the current Omarchy palette may live, most specific first:
/// `<xdg_state_home or home/.local/state>/omarchy/current/theme/colors.toml`, then
/// `<xdg_config_home or home/.config>/omarchy/current/theme/colors.toml`.
pub fn omarchy_colors_paths(
    home: &Path,
    xdg_state_home: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Vec<PathBuf> {
    const REL: &str = "omarchy/current/theme/colors.toml";
    let state = xdg_state_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local").join("state"));
    let config = xdg_config_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    vec![state.join(REL), config.join(REL)]
}

/// Read the first candidate that exists, is a regular file of at most
/// [`OMARCHY_COLORS_MAX_BYTES`] and parses. The palette `name` is filled from the sibling
/// `../theme.name` file (first line, trimmed) when present, else from the file name of the
/// resolved (`canonicalize`d) theme directory when that is not `theme`. Read-only.
pub fn load_omarchy_palette(candidates: &[PathBuf]) -> Option<(OmarchyPalette, PathBuf)> {
    for path in candidates {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > OMARCHY_COLORS_MAX_BYTES {
            continue;
        }
        // One byte more than the cap: a file that grew since the metadata call is rejected.
        let Some(bytes) = read_prefix(path, OMARCHY_COLORS_MAX_BYTES + 1) else {
            continue;
        };
        if bytes.len() as u64 > OMARCHY_COLORS_MAX_BYTES {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let Some(mut palette) = parse_omarchy_colors(&text) else {
            continue;
        };
        palette.name = theme_name(path);
        return Some((palette, path.clone()));
    }
    None
}

/// At most `max` bytes from the start of `path`.
fn read_prefix(path: &Path, max: u64) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// The theme name for the palette read from `path` (`<theme dir>/colors.toml`).
fn theme_name(path: &Path) -> Option<String> {
    // Lexical `..` of the theme directory: `<state>/omarchy/current/theme.name`.
    let sibling = path
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("theme.name"));
    sibling
        .and_then(|file| name_from_file(&file))
        .or_else(|| name_from_dir(path))
}

/// The first line of a theme name file, cleaned; `None` when empty, too long or unreadable.
fn name_from_file(file: &Path) -> Option<String> {
    let bytes = read_prefix(file, 4096)?;
    let text = String::from_utf8_lossy(&bytes);
    clean_name(text.lines().next()?)
}

/// The file name of the resolved theme directory, unless it is called `theme`.
fn name_from_dir(path: &Path) -> Option<String> {
    let dir = path.parent()?.canonicalize().ok()?;
    let base = dir.file_name()?.to_str()?;
    if base == "theme" {
        return None;
    }
    clean_name(base)
}

/// Control characters removed, then trimmed; `None` when empty or longer than 64 characters.
fn clean_name(line: &str) -> Option<String> {
    let cleaned: String = line.chars().filter(|c| !c.is_control()).collect();
    let name = cleaned.trim().to_string();
    (!name.is_empty() && name.chars().count() <= 64).then_some(name)
}

/// The fixed palette of the built-in `oxa` theme (dark).
pub fn oxa_builtin_palette() -> OmarchyPalette {
    OmarchyPalette {
        name: None,
        light: false,
        background: (0x14, 0x16, 0x1b),
        lighter_background: (0x1f, 0x23, 0x2b),
        foreground: (0xc8, 0xcc, 0xd4),
        dark_foreground: (0x6b, 0x72, 0x80),
        bright_foreground: (0xee, 0xf0, 0xf4),
        accent: (0xf2, 0xb8, 0x4b),
        selection: (0x2b, 0x31, 0x3d),
        muted: (0x3d, 0x44, 0x52),
        red: (0xef, 0x6b, 0x73),
        green: (0x8f, 0xcf, 0x6a),
        yellow: (0xe5, 0xc7, 0x6b),
        orange: (0xf0, 0x94, 0x5a),
        blue: (0x6c, 0xa8, 0xf0),
        magenta: (0xb4, 0x8e, 0xf2),
        cyan: (0x5e, 0xc4, 0xc9),
    }
}

/// Build the theme named `name` from `palette`: every token of [`crate::THEME_TOKENS`] is set
/// (mapping: OXA-UI-DESIGN.md sec. 3.2), `appearance` follows `palette.light`, `export` holds
/// `pageBg` / `cardBg` / `infoBg`, `source` is `None` (the caller sets it for hot reload).
pub fn oxa_theme(palette: &OmarchyPalette, name: &str, mode: ColorMode) -> Theme {
    let mut theme = Theme::plain();
    theme.name = name.to_string();
    theme.mode = mode;
    theme.appearance = if palette.light {
        Appearance::Light
    } else {
        Appearance::Dark
    };
    for (token, _, _) in THEME_TOKENS {
        let (r, g, b) = token_rgb(token, palette);
        theme.colors.insert(token.to_string(), Color::Rgb(r, g, b));
    }
    theme
        .export
        .insert("pageBg".to_string(), css_hex(palette.background));
    theme
        .export
        .insert("cardBg".to_string(), css_hex(palette.lighter_background));
    theme.export.insert(
        "infoBg".to_string(),
        css_hex(mix_rgb(palette.background, palette.accent, 0.12)),
    );
    theme
}

/// The colour of one theme token (sec. 3.2). Tokens not listed there use the foreground.
fn token_rgb(token: &str, p: &OmarchyPalette) -> Rgb {
    let bg = p.background;
    let mix = |t: f64| mix_rgb(bg, p.foreground, t);
    match token {
        "text" | "customMessageText" | "toolOutput" | "syntaxVariable" => p.foreground,
        "userMessageText" | "toolTitle" | "searchMatchText" | "scrollbarThumb" => p.bright_foreground,
        "muted" | "mdQuote" | "toolDiffContext" | "syntaxPunctuation" => mix(0.70),
        "dim" | "mdLinkUrl" | "syntaxComment" | "thinkingMinimal" => mix(0.55),
        "thinkingText" => mix(0.62),
        "border" => mix(0.40),
        "borderMuted" | "mdHr" | "mdQuoteBorder" | "mdCodeBlockBorder" | "scrollbarTrack" | "thinkingOff" => {
            mix(0.25)
        }
        "accent" | "borderAccent" | "customMessageLabel" | "mdListBullet" | "modeAsk" => p.accent,
        "success" | "toolDiffAdded" | "syntaxString" | "bashMode" | "mdCodeBlock" | "modeWorkspace" => {
            p.green
        }
        "error" | "toolDiffRemoved" | "thinkingMax" => p.red,
        "warning" | "mdHeading" | "syntaxType" | "modePlan" => p.yellow,
        "mdLink" | "syntaxFunction" | "thinkingLow" => p.blue,
        "mdCode" | "syntaxOperator" | "thinkingMedium" => p.cyan,
        "syntaxKeyword" | "thinkingHigh" => p.magenta,
        "syntaxNumber" | "thinkingXhigh" => p.orange,
        "selectedBg" => p.selection,
        "userMessageBg" => p.lighter_background,
        "customMessageBg" => mix_rgb(bg, p.magenta, 0.12),
        "toolPendingBg" => mix_rgb(bg, p.blue, 0.10),
        "toolSuccessBg" => mix_rgb(bg, p.green, 0.08),
        "toolErrorBg" => mix_rgb(bg, p.red, 0.12),
        "searchMatchBg" => mix_rgb(bg, p.yellow, 0.35),
        _ => p.foreground,
    }
}

/// What the palette files looked like at one moment: for every candidate the resolved path,
/// modification time and size of `colors.toml`, and the desktop's generation stamp and theme
/// name beside it (`../theme.generation`, `../theme.name`; Omarchy rewrites the stamp on
/// every theme change). Two equal marks = nothing changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PaletteMark(Vec<Option<FileMark>>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMark {
    resolved: Option<PathBuf>,
    modified: Option<std::time::SystemTime>,
    len: u64,
    stamp: Option<Vec<u8>>,
    name: Option<Vec<u8>>,
}

fn palette_mark(candidates: &[PathBuf]) -> PaletteMark {
    PaletteMark(
        candidates
            .iter()
            .map(|path| {
                let meta = std::fs::metadata(path).ok()?;
                let beside = |file: &str| {
                    path.parent()
                        .and_then(Path::parent)
                        .and_then(|dir| read_prefix(&dir.join(file), 256))
                };
                Some(FileMark {
                    resolved: path.canonicalize().ok(),
                    modified: meta.modified().ok(),
                    len: meta.len(),
                    stamp: beside("theme.generation"),
                    name: beside("theme.name"),
                })
            })
            .collect(),
    )
}

/// Follows the desktop palette while the program runs (O-185), so `oxa-omarchy` changes with
/// an Omarchy theme switch.
///
/// It is a cheap poll, not a thread: the owner calls [`PaletteWatch::poll`] from its own loop
/// (a few `stat` calls and two tiny reads every [`PaletteWatch::interval_ms`]); there is
/// nothing to stop on exit and nothing runs between calls. Read-only and bounded like
/// [`load_omarchy_palette`].
///
/// * **Debounce:** a change is acted on only when two polls in a row see the same files (a
///   theme switch swaps several files; while they move, the poll interval drops to
///   [`PaletteWatch::SETTLE_MS`]).
/// * **Half-written files:** when the settled files do not parse, nothing is reported and
///   the caller keeps its palette; the next change is looked at again.
/// * A palette that appears later (none at start) or disappears is a change like any other;
///   after a disappearance nothing is reported until one parses again.
#[derive(Debug, Clone)]
pub struct PaletteWatch {
    candidates: Vec<PathBuf>,
    /// The files as they were when the caller's palette was read.
    seen: PaletteMark,
    /// A change that has not settled yet.
    moving: Option<PaletteMark>,
}

impl PaletteWatch {
    /// The poll interval while nothing changes.
    pub const IDLE_MS: u64 = 2000;
    /// The poll interval while a change settles.
    pub const SETTLE_MS: u64 = 300;

    /// Start watching `candidates` (see [`omarchy_colors_paths`]) from their state now.
    pub fn new(candidates: Vec<PathBuf>) -> PaletteWatch {
        let seen = palette_mark(&candidates);
        PaletteWatch {
            candidates,
            seen,
            moving: None,
        }
    }

    /// How long to wait before the next [`PaletteWatch::poll`].
    pub fn interval_ms(&self) -> u64 {
        if self.moving.is_some() {
            PaletteWatch::SETTLE_MS
        } else {
            PaletteWatch::IDLE_MS
        }
    }

    /// Look at the files once. `Some` = they changed, have settled and parse: the new palette
    /// and the file it was read from. `None` = nothing new (also: still moving, or the new
    /// files do not parse, in which case the caller keeps what it has).
    pub fn poll(&mut self) -> Option<(OmarchyPalette, PathBuf)> {
        let now = palette_mark(&self.candidates);
        if now == self.seen {
            self.moving = None;
            return None;
        }
        if self.moving.as_ref() != Some(&now) {
            // First sight of this state: wait for it to hold still.
            self.moving = Some(now);
            return None;
        }
        // Settled. Whatever the outcome, this state has been dealt with.
        self.moving = None;
        self.seen = now;
        load_omarchy_palette(&self.candidates)
    }
}
