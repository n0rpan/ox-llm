//! B13: theme loading (pi theme JSON), colour conversion and terminal-colour detection.
// ported from pi packages/coding-agent/src/modes/interactive/theme/{theme.ts,theme-schema.ts,system-theme.ts} and packages/tui/src/terminal-colors.ts
//
// The `Theme` value itself (tokens, SGR emission) is architect-owned in `theme_core.rs`.
//
// Deliberate simplifications against pi (documented in the task report): the `system` theme
// uses the terminal's ANSI palette indices for accents and tints its panels from the terminal
// background; pi's contrast-solved OKLCH generator is not ported (no palette is reported by
// OSC 10/11, so the palette tier is out of reach here anyway).
use crate::{
    Appearance, Color, ColorMode, Theme, BUILTIN_THEME_NAMES, THEME_DARK_JSON, THEME_LIGHT_JSON, THEME_TOKENS,
};
use oxa_proto::{codes, OxaError};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

fn invalid(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, message)
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Parse a pi theme file (`{"name", "appearance"?, "vars"?, "colors", "export"?}`).
///
/// * pi's `dark.json` / `light.json` load unchanged ([`crate::THEME_DARK_JSON`]).
/// * A colour value is `""` (terminal default), `"#RRGGBB"`, an integer 0-255 (palette index)
///   or the name of an entry in `vars` (resolved recursively; a cycle is an error).
/// * Every required token of [`crate::THEME_TOKENS`] (those without a fallback) must be
///   present; a missing one, an unknown colour reference or malformed JSON is
///   `Err(invalid)` naming the token. Unknown extra tokens are kept and ignored.
/// * `appearance` defaults to the luminance of `userMessageBg` (dark when unset).
/// * `export` colours are copied as strings.
pub fn parse_theme(json: &str, mode: ColorMode) -> Result<Theme, OxaError> {
    parse_document(json, mode, "custom")
}

fn parse_document(json: &str, mode: ColorMode, default_name: &str) -> Result<Theme, OxaError> {
    let doc: Value =
        serde_json::from_str(strip_bom(json)).map_err(|e| invalid(format!("malformed theme JSON: {e}")))?;
    let obj = doc
        .as_object()
        .ok_or_else(|| invalid("a theme must be a JSON object"))?;
    let raw_colors = obj
        .get("colors")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("a theme needs a \"colors\" object"))?;
    let empty = Map::new();
    let vars = obj.get("vars").and_then(Value::as_object).unwrap_or(&empty);

    let mut colors = BTreeMap::new();
    for (token, value) in raw_colors {
        let mut visiting = Vec::new();
        colors.insert(token.clone(), resolve_value(value, vars, token, &mut visiting)?);
    }
    for (name, _, fallback) in THEME_TOKENS {
        if fallback.is_none() && !colors.contains_key(name) {
            return Err(invalid(format!(
                "theme token \"{name}\" is missing and has no fallback"
            )));
        }
    }

    let export: BTreeMap<String, String> = obj
        .get("export")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some((k.clone(), s.clone())),
                    Value::Number(n) => Some((k.clone(), n.to_string())),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(default_name)
        .to_string();
    let mut theme = Theme {
        name,
        appearance: Appearance::Dark,
        mode,
        colors,
        export,
        source: None,
    };
    theme.appearance = match obj.get("appearance") {
        None | Some(Value::Null) => {
            if color_luminance(theme.color("userMessageBg")).is_some_and(|l| l > 0.5) {
                Appearance::Light
            } else {
                Appearance::Dark
            }
        }
        Some(Value::String(s)) if s == "dark" => Appearance::Dark,
        Some(Value::String(s)) if s == "light" => Appearance::Light,
        Some(other) => {
            return Err(invalid(format!(
                "theme \"appearance\" must be \"dark\" or \"light\", not {other}"
            )))
        }
    };
    Ok(theme)
}

fn resolve_value(
    value: &Value,
    vars: &Map<String, Value>,
    token: &str,
    visiting: &mut Vec<String>,
) -> Result<Color, OxaError> {
    match value {
        Value::Number(n) => match n.as_u64() {
            Some(i) if i <= 255 => Ok(Color::Indexed(i as u8)),
            _ => Err(invalid(format!(
                "theme token \"{token}\": palette index {n} is not 0-255"
            ))),
        },
        Value::String(raw) => {
            let s = raw.trim();
            if s.is_empty() {
                return Ok(Color::Default);
            }
            if let Some(c) = parse_hex(s) {
                return Ok(c);
            }
            if let Some(c) = parse_color_function(s) {
                return Ok(c);
            }
            if s.starts_with('#') || s.contains('(') {
                return Err(invalid(format!(
                    "theme token \"{token}\": malformed colour \"{s}\""
                )));
            }
            if visiting.iter().any(|v| v.as_str() == s) {
                return Err(invalid(format!(
                    "theme token \"{token}\": circular variable reference \"{s}\""
                )));
            }
            match vars.get(s) {
                Some(next) => {
                    visiting.push(s.to_string());
                    let resolved = resolve_value(next, vars, token, visiting);
                    visiting.pop();
                    resolved
                }
                None => Err(invalid(format!(
                    "theme token \"{token}\": unknown colour or variable \"{s}\""
                ))),
            }
        }
        _ => Err(invalid(format!(
            "theme token \"{token}\": a colour must be a string or a number"
        ))),
    }
}

fn parse_hex(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// `okhsl(H S% L%)` and `oklch(L C H)` (pi's perceptual colour functions). Case-insensitive;
/// arguments separated by spaces or commas; a `%` scales by 1/100, a bare number is a fraction.
fn parse_color_function(value: &str) -> Option<Color> {
    let lower = value.trim().to_ascii_lowercase();
    let (name, rest) = lower.split_once('(')?;
    let inner = rest.strip_suffix(')')?;
    let parts: Vec<&str> = inner
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() != 3 {
        return None;
    }
    let fraction = |p: &str| -> Option<f64> {
        let (body, scale) = match p.strip_suffix('%') {
            Some(b) => (b, 0.01),
            None => (p, 1.0),
        };
        body.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v * scale)
    };
    let degrees = |p: &str| -> Option<f64> {
        p.trim_end_matches("deg")
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    };
    match name {
        "okhsl" => {
            let h = degrees(parts[0])?;
            let s = fraction(parts[1])?;
            let l = fraction(parts[2])?;
            let (r, g, b) = okhsl_to_rgb(h, s, l);
            Some(Color::Rgb(r, g, b))
        }
        "oklch" => {
            let l = fraction(parts[0])?;
            let c = parts[1].parse::<f64>().ok().filter(|v| v.is_finite())?;
            let h = degrees(parts[2])?;
            let (r, g, b) = oklch_to_rgb(l, c, h);
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

// ----------------------------------------------------------------------------
// OKLab / OKHSL / OKLCH to sRGB (Bjorn Ottosson's definitions)
// ----------------------------------------------------------------------------

const TOE_K1: f64 = 0.206;
const TOE_K2: f64 = 0.03;
const TOE_K3: f64 = (1.0 + TOE_K1) / (1.0 + TOE_K2);

fn toe_inv(x: f64) -> f64 {
    (x * x + TOE_K1 * x) / (TOE_K3 * (x + TOE_K2))
}

/// OKLab to linear sRGB.
fn oklab_to_linear_srgb(l: f64, a: f64, b: f64) -> [f64; 3] {
    let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = l - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3,
        -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701_0 * s3,
    ]
}

fn in_gamut(lin: [f64; 3]) -> bool {
    lin.iter().all(|c| (-1e-9..=1.0 + 1e-9).contains(c))
}

/// The largest OKLab chroma at lightness `l` in the direction `(cos_h, sin_h)`: the sRGB gamut
/// boundary, found by bisection (the gamut is convex along a ray from the neutral axis).
fn max_chroma(l: f64, cos_h: f64, sin_h: f64) -> f64 {
    let (mut lo, mut hi) = (0.0_f64, 0.5_f64);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if in_gamut(oklab_to_linear_srgb(l, mid * cos_h, mid * sin_h)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

fn srgb_encode(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

fn linear_to_rgb8(lin: [f64; 3]) -> (u8, u8, u8) {
    let q = |x: f64| (srgb_encode(x) * 255.0).round().clamp(0.0, 255.0) as u8;
    (q(lin[0]), q(lin[1]), q(lin[2]))
}

/// OKHSL to 8-bit sRGB. Saturation is relative to the gamut boundary at the same lightness.
fn okhsl_to_rgb(hue_deg: f64, saturation: f64, lightness: f64) -> (u8, u8, u8) {
    let l = toe_inv(lightness.clamp(0.0, 1.0));
    let s = saturation.clamp(0.0, 1.0);
    if s <= 0.0 || l <= 0.0 || l >= 1.0 {
        return linear_to_rgb8(oklab_to_linear_srgb(l, 0.0, 0.0));
    }
    let h = hue_deg.rem_euclid(360.0).to_radians();
    let (cos_h, sin_h) = (h.cos(), h.sin());
    let c = s * max_chroma(l, cos_h, sin_h);
    linear_to_rgb8(oklab_to_linear_srgb(l, c * cos_h, c * sin_h))
}

/// OKLCH to 8-bit sRGB (out-of-gamut values are clamped).
fn oklch_to_rgb(l: f64, chroma: f64, hue_deg: f64) -> (u8, u8, u8) {
    let h = hue_deg.rem_euclid(360.0).to_radians();
    linear_to_rgb8(oklab_to_linear_srgb(l, chroma * h.cos(), chroma * h.sin()))
}

// ----------------------------------------------------------------------------
// Palette and luminance helpers
// ----------------------------------------------------------------------------

/// The xterm 256-colour palette entry `n` as RGB.
pub(crate) fn indexed_rgb(n: u8) -> (u8, u8, u8) {
    const BASE16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match n {
        0..=15 => BASE16[n as usize],
        16..=231 => {
            let i = n - 16;
            (
                CUBE[(i / 36) as usize],
                CUBE[((i / 6) % 6) as usize],
                CUBE[(i % 6) as usize],
            )
        }
        232..=255 => {
            let v = 8 + 10 * (n - 232);
            (v, v, v)
        }
    }
}

/// WCAG 2 relative luminance of an sRGB colour, 0-1.
/// WCAG 2 contrast ratio of two relative luminances, 1-21.
fn contrast(a: f64, b: f64) -> f64 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let lin = |c: u8| {
        let v = c as f64 / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn color_luminance(c: Color) -> Option<f64> {
    match c {
        Color::Rgb(r, g, b) => Some(relative_luminance((r, g, b))),
        Color::Indexed(n) => Some(relative_luminance(indexed_rgb(n))),
        Color::Default => None,
    }
}

fn css_hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round().clamp(0.0, 255.0) as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

fn is_hex_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One OSC channel of 1-4 hex digits, scaled to 8 bits.
fn osc_channel(s: &str) -> Option<u8> {
    if s.len() > 4 || !is_hex_digits(s) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    let max = 16u32.pow(s.len() as u32) - 1;
    Some((v as f64 * 255.0 / max as f64).round() as u8)
}

// ----------------------------------------------------------------------------
// Built-in and loaded themes
// ----------------------------------------------------------------------------

/// [`parse_theme`] of a file; sets `Theme::source`. Errors: `io`, `invalid`.
pub fn load_theme_file(path: &Path, mode: ColorMode) -> Result<Theme, OxaError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| OxaError::io(&format!("reading theme {}", path.display()), &e))?;
    let default_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("theme");
    let mut theme = parse_document(&content, mode, default_name).map_err(|e| OxaError {
        message: format!("theme {}: {}", path.display(), e.message),
        ..e
    })?;
    theme.source = Some(path.to_path_buf());
    Ok(theme)
}

/// A built-in theme: `dark`, `light`, or `system` (see [`system_theme`]; needs `terminal`).
/// `None` for any other name.
pub fn builtin_theme(name: &str, mode: ColorMode, terminal: Option<&TerminalColors>) -> Option<Theme> {
    match name {
        "dark" => parse_document(THEME_DARK_JSON, mode, "dark").ok(),
        "light" => parse_document(THEME_LIGHT_JSON, mode, "light").ok(),
        // Without a reported terminal the system theme assumes a dark terminal.
        "system" => Some(system_theme(&terminal.copied().unwrap_or_default(), mode)),
        _ => None,
    }
}

/// The terminal's own colours as answered to OSC 10 (foreground) and OSC 11 (background).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalColors {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
}

impl TerminalColors {
    /// pi's `terminalAppearance` (system-theme.ts): with a foreground clearly lighter or darker
    /// than the background (and text of that polarity readable, WCAG >= 4.5) that polarity;
    /// otherwise `Dark` when white text contrasts at least as well as black on the background.
    /// `None` when the background is unknown. (pi compares OKLab lightness; the cube root of
    /// the relative luminance stands in for it here.)
    pub fn appearance(&self) -> Option<Appearance> {
        let bg = self.bg?;
        let white = contrast(relative_luminance((255, 255, 255)), relative_luminance(bg));
        let black = contrast(relative_luminance((0, 0, 0)), relative_luminance(bg));
        if let Some(fg) = self.fg {
            let (lf, lb) = (relative_luminance(fg).cbrt(), relative_luminance(bg).cbrt());
            if (lf - lb).abs() > 0.05 {
                let (appearance, best) = if lf > lb {
                    (Appearance::Dark, white)
                } else {
                    (Appearance::Light, black)
                };
                if best >= 4.5 {
                    return Some(appearance);
                }
            }
        }
        Some(if white >= black {
            Appearance::Dark
        } else {
            Appearance::Light
        })
    }
}

/// Parse an OSC 10/11 reply payload: `rgb:RRRR/GGGG/BBBB` (1-4 hex digits per channel, scaled
/// to 8 bits), `rgba:..` or `#RRGGBB`. `None` when malformed.
pub fn parse_osc_color(data: &str) -> Option<(u8, u8, u8)> {
    let data = data.trim();
    if let Some(hex) = data.strip_prefix('#') {
        if hex.len() == 6 && is_hex_digits(hex) {
            let v = u32::from_str_radix(hex, 16).ok()?;
            return Some(((v >> 16) as u8, (v >> 8) as u8, v as u8));
        }
        if hex.len() == 12 && is_hex_digits(hex) {
            return Some((
                osc_channel(&hex[0..4])?,
                osc_channel(&hex[4..8])?,
                osc_channel(&hex[8..12])?,
            ));
        }
        return None;
    }
    let lower = data.to_ascii_lowercase();
    let body = lower
        .strip_prefix("rgba:")
        .or_else(|| lower.strip_prefix("rgb:"))?;
    let parts: Vec<&str> = body.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    // A fourth channel (alpha) is accepted and ignored.
    Some((
        osc_channel(parts[0])?,
        osc_channel(parts[1])?,
        osc_channel(parts[2])?,
    ))
}

/// pi's `system` theme: text colours are the terminal's defaults, accents use the 16 ANSI
/// palette colours, backgrounds are derived from the terminal background (slightly lighter on
/// dark, darker on light). Without a known background it assumes dark.
pub fn system_theme(terminal: &TerminalColors, mode: ColorMode) -> Theme {
    let appearance = terminal.appearance().unwrap_or(Appearance::Dark);
    let bg = terminal.bg;
    let mut colors = BTreeMap::new();
    for (token, _, fallback) in THEME_TOKENS {
        match system_token_color(token, bg, appearance) {
            Some(c) => {
                colors.insert(token.to_string(), c);
            }
            // Tokens with a fallback that the system theme does not set keep resolving through it.
            None if fallback.is_none() => {
                colors.insert(token.to_string(), Color::Default);
            }
            None => {}
        }
    }
    let mut export = BTreeMap::new();
    if let Some(bg) = bg {
        export.insert("pageBg".to_string(), css_hex(bg));
        export.insert("cardBg".to_string(), css_hex(panel_rgb(bg, appearance, 0.04)));
        export.insert("infoBg".to_string(), css_hex(panel_rgb(bg, appearance, 0.08)));
    }
    Theme {
        name: "system".to_string(),
        appearance,
        mode,
        colors,
        export,
        source: None,
    }
}

/// A panel colour `t` of the way from the terminal background toward white (dark terminals) or
/// black (light terminals).
fn panel_rgb(bg: (u8, u8, u8), appearance: Appearance, t: f64) -> (u8, u8, u8) {
    let target = match appearance {
        Appearance::Dark => (255, 255, 255),
        Appearance::Light => (0, 0, 0),
    };
    mix(bg, target, t)
}

fn system_token_color(token: &str, bg: Option<(u8, u8, u8)>, appearance: Appearance) -> Option<Color> {
    let panel = |t: f64| match bg {
        Some(b) => {
            let (r, g, c) = panel_rgb(b, appearance, t);
            Color::Rgb(r, g, c)
        }
        None => Color::Default,
    };
    let hued = |t: f64, hue: u8| match bg {
        Some(b) => {
            let (r, g, c) = mix(panel_rgb(b, appearance, t), indexed_rgb(hue), 0.15);
            Color::Rgb(r, g, c)
        }
        None => Color::Default,
    };
    Some(match token {
        "accent" | "borderAccent" | "customMessageLabel" | "mdCode" | "mdListBullet" | "syntaxType"
        | "syntaxNumber" => Color::Indexed(5),
        "border" | "mdLink" | "syntaxKeyword" | "thinkingLow" => Color::Indexed(4),
        "success" | "mdCodeBlock" | "toolDiffAdded" | "bashMode" | "syntaxString" => Color::Indexed(2),
        "error" | "toolDiffRemoved" | "thinkingMax" => Color::Indexed(1),
        "warning" | "mdHeading" | "syntaxFunction" => Color::Indexed(3),
        "syntaxVariable" | "thinkingMedium" => Color::Indexed(6),
        "thinkingHigh" => Color::Indexed(5),
        "thinkingXhigh" => Color::Indexed(13),
        "muted" | "dim" | "borderMuted" | "mdLinkUrl" | "mdQuote" | "mdQuoteBorder" | "mdHr"
        | "mdCodeBlockBorder" | "toolDiffContext" | "syntaxComment" | "scrollbarTrack" | "thinkingOff"
        | "thinkingMinimal" => Color::Indexed(8),
        "text" | "thinkingText" | "userMessageText" | "customMessageText" | "toolTitle" | "toolOutput"
        | "scrollbarThumb" | "searchMatchText" | "syntaxOperator" | "syntaxPunctuation" => Color::Default,
        "selectedBg" => panel(0.10),
        "userMessageBg" | "customMessageBg" => panel(0.08),
        "toolPendingBg" => panel(0.06),
        "toolSuccessBg" => hued(0.06, 2),
        "toolErrorBg" => hued(0.06, 1),
        "searchMatchBg" => hued(0.10, 3),
        _ => return None,
    })
}

/// Appearance guess from the `COLORFGBG` environment value (`"15;0"` = dark), pi's rule.
pub fn appearance_from_colorfgbg(value: Option<&str>) -> Option<Appearance> {
    let last = value?.split(';').next_back()?.trim();
    if last.is_empty() || last.len() > 2 || !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: u32 = last.parse().ok()?;
    if index > 15 {
        return None;
    }
    // Indices 0-6 and 8 are the dark ANSI backgrounds (Vim's rule); 7 and 9-15 are light.
    Some(if index <= 6 || index == 8 {
        Appearance::Dark
    } else {
        Appearance::Light
    })
}

/// The nearest 256-colour palette index for an RGB colour: the 6x6x6 cube or the 24-step grey
/// ramp, whichever is closer (pi's algorithm; fixture table in the task's tests).
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |c: u8| -> usize { (0..6).min_by_key(|&i| (LEVELS[i] - c as i32).abs()).unwrap_or(0) };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = (16 + 36 * ri + 6 * gi + bi) as u8;
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let sq = |x: i32| x * x;
    let cube_d = sq(LEVELS[ri] - r) + sq(LEVELS[gi] - g) + sq(LEVELS[bi] - b);
    // Grey ramp: 232 + k is 8 + 10k, k in 0-23; pick the step nearest the mean.
    let avg = (r + g + b) / 3;
    let step = (avg - 8 + 5).div_euclid(10).clamp(0, 23);
    let v = 8 + 10 * step;
    let grey_d = sq(v - r) + sq(v - g) + sq(v - b);
    if grey_d < cube_d {
        (232 + step) as u8
    } else {
        cube
    }
}

/// The CSS colour string of a resolved colour (`#rrggbb`; palette indices through the xterm
/// table; `Default` = `None`). Used to build `oxa_export::ExportTheme`.
pub fn css_color(c: Color) -> Option<String> {
    match c {
        Color::Default => None,
        Color::Indexed(n) => Some(css_hex(indexed_rgb(n))),
        Color::Rgb(r, g, b) => Some(css_hex((r, g, b))),
    }
}

/// A theme found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    /// `None` for built-ins.
    pub path: Option<PathBuf>,
}

/// Built-ins plus JSON files and every `*.json` in `dirs` (later paths win on equal names), sorted:
/// built-ins first in [`crate::BUILTIN_THEME_NAMES`] order, then by name.
///
/// A custom theme cannot shadow a built-in name (built-ins are listed first, as in pi). Files
/// that fail to load are skipped here; loading them by path reports the error.
pub fn list_themes(dirs: &[PathBuf]) -> Vec<ThemeInfo> {
    let mut custom: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in dirs {
        let mut files: Vec<PathBuf> = if dir.is_file() {
            vec![dir.clone()]
        } else {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json") && p.is_file())
                .collect()
        };
        files.sort();
        for path in files {
            if let Ok(theme) = load_theme_file(&path, ColorMode::TrueColor) {
                if !BUILTIN_THEME_NAMES.contains(&theme.name.as_str()) {
                    custom.insert(theme.name, path);
                }
            }
        }
    }
    let mut out: Vec<ThemeInfo> = BUILTIN_THEME_NAMES
        .iter()
        .map(|n| ThemeInfo {
            name: n.to_string(),
            path: None,
        })
        .collect();
    out.extend(custom.into_iter().map(|(name, path)| ThemeInfo {
        name,
        path: Some(path),
    }));
    out
}

/// Resolve a theme setting: a built-in name, a name found in `dirs`, or a path. pi's
/// `"auto"`-style value `light-name/dark-name` picks by the terminal appearance. Errors:
/// `not_found` (the caller falls back to `dark` with a warning), `invalid`.
pub fn resolve_theme(
    setting: &str,
    dirs: &[PathBuf],
    mode: ColorMode,
    terminal: Option<&TerminalColors>,
) -> Result<Theme, OxaError> {
    let setting = setting.trim();

    // `light/dark`: exactly one slash, both sides named, and not a path to a .json file.
    if !setting.ends_with(".json") && setting.matches('/').count() == 1 {
        if let Some((light, dark)) = setting.split_once('/') {
            let (light, dark) = (light.trim(), dark.trim());
            if !light.is_empty() && !dark.is_empty() {
                let appearance = terminal.and_then(|t| t.appearance()).unwrap_or(Appearance::Dark);
                let chosen = if appearance == Appearance::Light {
                    light
                } else {
                    dark
                };
                return resolve_theme(chosen, dirs, mode, terminal);
            }
        }
    }

    if let Some(theme) = builtin_theme(setting, mode, terminal) {
        return Ok(theme);
    }
    // Resolve each source in precedence order. Prefer the name shown in the picker
    // within that source, with the filename retained as an alias for theme directories.
    for source in dirs.iter().rev() {
        let named = list_themes(std::slice::from_ref(source))
            .into_iter()
            .find(|theme| theme.name == setting)
            .and_then(|theme| theme.path);
        if let Some(path) = named {
            return load_theme_file(&path, mode);
        }
        let candidate = source.join(format!("{setting}.json"));
        if candidate.is_file() {
            return load_theme_file(&candidate, mode);
        }
    }
    // A path.
    let path = Path::new(setting);
    if path.is_file() {
        return load_theme_file(path, mode);
    }
    Err(
        OxaError::new(codes::NOT_FOUND, format!("theme not found: {setting}"))
            .with_hint("use dark, light or system, or the name or path of a theme file"),
    )
}

/// Hot reload support: `true` when the theme's source file changed since `since_mtime_ms`
/// (the caller polls; there is no watcher thread). Built-ins and unreadable files report
/// `false`.
pub fn theme_file_changed(theme: &Theme, since_mtime_ms: u64) -> bool {
    let Some(path) = &theme.source else {
        return false;
    };
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) else {
        return false;
    };
    since_epoch.as_millis() as u64 > since_mtime_ms
}
