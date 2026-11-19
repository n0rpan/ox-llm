//! Scenes as data (design section 4): the types, the JSON loader, the validation rules of 4.2,
//! the glyph allow-list and the ASCII map of 4.3. Pure: no clock, no I/O, no terminal.
//!
//! A pack is read by [`load_pack`]. A file that is not a version 1 pack is refused as a whole;
//! a scene that fails a rule is skipped with one reason, and the others still load.

use oxa_tui::visible_width;
use serde_json::Value;
use std::collections::BTreeMap;

/// The ink table of design 3.1, plus `I` (the current tool's icon, design 4.1).
pub const INKS: &str = "HDGCOB.I";

/// The stage widths a scene may draw at: run and S1 wins 8 and 12, S2 and S3 wins 20 and 32.
pub const SIZES: [usize; 4] = [8, 12, 20, 32];

/// A win scene has at most this many frames at significance 1, 2 and 3.
const WIN_FRAMES: [usize; 3] = [6, 8, 12];

/// No celebration is longer than this (design 4.2).
pub const WIN_MAX_MS: u32 = 1500;

/// Steps are multiples of this (`SPRITE_FRAME_MS`).
pub const STEP_MS: u32 = 125;

/// Caption limits in characters: the short and the long form.
pub const CAPTION_SHORT: usize = 12;
pub const CAPTION_LONG: usize = 32;

/// The glyphs beyond ASCII and the block and box ranges (design 4.3, and `▶` from the owner
/// decision of plan C.1).
const EXTRA_GLYPHS: &[char] = &['·', '▪', '◆', '●', '○', '◐', '♥', '✓', '✗', '↵', '±', '≡', '▶'];

/// Sprites first: a four-cell or three-cell pattern maps to ASCII of the same width.
const SPRITES: &[(&str, &str)] = &[("▟█▛▘", "{ox>"), ("▜█▛▘", "(ox>"), ("▝▜▙", "<o}")];

/// Single cells of the ASCII map (design 4.3).
const CELLS: &[(char, char)] = &[
    ('█', '#'),
    ('▂', '_'),
    ('▁', '.'),
    ('─', '-'),
    ('━', '='),
    ('═', '='),
    ('▶', '>'),
    ('├', '['),
    ('┤', ']'),
    ('▀', '^'),
    ('·', '.'),
    ('▪', 'o'),
    ('◆', 'o'),
    ('●', '#'),
    ('○', 'o'),
    ('◐', ')'),
    ('♥', 'v'),
    ('✓', 'v'),
    ('✗', 'x'),
    ('↵', '<'),
    ('±', '~'),
    ('≡', '='),
    ('▐', '['),
    ('▌', ']'),
];

/// Where a scene may play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Beside the activity badge, 8 or 12 cells.
    Run,
    /// After a run, a success moment.
    Win,
    /// One still frame.
    Poster,
}

/// How the frames advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Looping {
    /// Repeats for as long as the activity lasts.
    Loop,
    /// Plays once, then is gone.
    Once,
    /// Plays once and the last frame stays until the line is taken away.
    Hold,
}

/// The motion level a scene needs (`ui.animations.intensity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Intensity {
    Calm,
    Normal,
    Party,
}

/// One frame: a text row and its ink row, both exactly the size key wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub text: String,
    pub inks: String,
}

/// A validated scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scene {
    pub id: String,
    pub title: String,
    pub note: String,
    /// Event keys the scene may play for.
    pub on: Vec<String>,
    pub tier: Tier,
    pub looping: Looping,
    pub step_ms: u32,
    /// 0 for run scenes, 1 to 3 for wins.
    pub significance: u8,
    pub min_intensity: Intensity,
    pub weight: u32,
    /// Frames per stage width.
    pub sizes: BTreeMap<usize, Vec<Frame>>,
    /// The ASCII skin's frames: the scene's own `ascii`, or the auto-map of 4.3. `None` when a
    /// glyph has no ASCII form, so the scene is left out of ASCII pools.
    pub ascii: Option<BTreeMap<usize, Vec<Frame>>>,
    /// `(short, long)` caption pairs.
    pub captions: Vec<(String, String)>,
}

/// A scene that did not load, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub id: String,
    pub reason: String,
}

/// The scenes that loaded, and the ones that did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub scenes: Vec<Scene>,
    pub skipped: Vec<Skip>,
}

/// Reads a pack (`{"version": 1, "scenes": [...]}`). `Err` only when the whole file is refused.
pub fn load_pack(text: &str) -> Result<Loaded, String> {
    let root: Value = serde_json::from_str(text).map_err(|e| format!("the pack is not JSON: {e}"))?;
    if root.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("the pack version is not 1".into());
    }
    let items = root
        .get("scenes")
        .and_then(Value::as_array)
        .ok_or("the pack has no scenes list")?;
    let mut out = Loaded::default();
    for item in items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("?").to_string();
        match parse_scene(item) {
            Ok(scene) if out.scenes.iter().any(|s| s.id == scene.id) => {
                out.skipped.push(Skip {
                    id,
                    reason: "the id is used twice in this pack".into(),
                });
            }
            Ok(scene) => out.scenes.push(scene),
            Err(reason) => out.skipped.push(Skip { id, reason }),
        }
    }
    Ok(out)
}

/// True for a glyph of the allow-list (design 4.3): printable ASCII, the block elements, the
/// box drawing and the listed marks.
pub fn glyph_allowed(c: char) -> bool {
    is_printable_ascii(c)
        || ('\u{2580}'..='\u{259F}').contains(&c)
        || ('\u{2500}'..='\u{257F}').contains(&c)
        || EXTRA_GLYPHS.contains(&c)
}

/// The ASCII form of a text row, or `None` when a glyph has no ASCII equivalent. The result
/// has the same visible width as the input.
pub fn ascii_text(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let sprite = SPRITES.iter().find(|&&(glyph, _)| rest.starts_with(glyph));
        if let Some(&(glyph, ascii)) = sprite {
            out.push_str(ascii);
            rest = &rest[glyph.len()..];
            continue;
        }
        let mapped = if is_printable_ascii(c) {
            c
        } else {
            CELLS.iter().find(|&&(from, _)| from == c).map(|&(_, to)| to)?
        };
        out.push(mapped);
        rest = &rest[c.len_utf8()..];
    }
    Some(out)
}

fn is_printable_ascii(c: char) -> bool {
    (' '..='~').contains(&c)
}

fn parse_scene(v: &Value) -> Result<Scene, String> {
    let id = text(v, "id")?;
    check_id(&id)?;
    let title = text(v, "title")?;
    if title.is_empty() {
        return Err("the title is empty".into());
    }
    let on = parse_on(v)?;
    let tier = match text(v, "tier")?.as_str() {
        "run" => Tier::Run,
        "win" => Tier::Win,
        "poster" => Tier::Poster,
        other => return Err(format!("unknown tier \"{other}\"")),
    };
    let looping = match text(v, "loop")?.as_str() {
        "loop" => Looping::Loop,
        "once" => Looping::Once,
        "hold" => Looping::Hold,
        other => return Err(format!("unknown loop \"{other}\"")),
    };
    let step_ms = uint(v, "step_ms")?;
    if step_ms == 0 || !step_ms.is_multiple_of(STEP_MS) {
        return Err(format!("step_ms must be a multiple of {STEP_MS}"));
    }
    let significance = match uint(v, "significance")? {
        s @ 0..=3 => s as u8,
        _ => return Err("significance must be 0 to 3".into()),
    };
    match (tier, significance) {
        (Tier::Run, 0) | (Tier::Poster, _) | (Tier::Win, 1..=3) => {}
        (Tier::Run, _) => return Err("a run scene has significance 0".into()),
        (Tier::Win, _) => return Err("a win scene has significance 1 to 3".into()),
    }
    let min_intensity = match opt_text(v, "min_intensity")?.as_deref().unwrap_or("normal") {
        "calm" => Intensity::Calm,
        "normal" => Intensity::Normal,
        "party" => Intensity::Party,
        other => return Err(format!("unknown min_intensity \"{other}\"")),
    };
    let weight = match v.get("weight") {
        None => 1,
        Some(_) => uint(v, "weight")?,
    };
    if weight == 0 {
        return Err("weight must be at least 1".into());
    }
    let sizes = parse_frames(v, "sizes", false)?;
    check_shape(tier, significance, step_ms, &sizes)?;
    let ascii = if v.get("ascii").is_some() {
        let explicit = parse_frames(v, "ascii", true)?;
        let same = explicit.len() == sizes.len()
            && sizes
                .iter()
                .all(|(k, f)| explicit.get(k).map(Vec::len) == Some(f.len()));
        if !same {
            return Err("the ascii frames do not match the sizes".into());
        }
        Some(explicit)
    } else {
        ascii_sizes(&sizes)
    };
    Ok(Scene {
        id,
        title,
        note: opt_text(v, "note")?.unwrap_or_default(),
        on,
        tier,
        looping,
        step_ms,
        significance,
        min_intensity,
        weight,
        sizes,
        ascii,
        captions: parse_captions(v)?,
    })
}

/// The sizes a tier needs, and the frame counts of design 4.2.
fn check_shape(
    tier: Tier,
    significance: u8,
    step_ms: u32,
    sizes: &BTreeMap<usize, Vec<Frame>>,
) -> Result<(), String> {
    let need: &[usize] = match (tier, significance) {
        (Tier::Run, _) | (Tier::Win, 1) => &[8, 12],
        (Tier::Win, _) => &[20, 32],
        (Tier::Poster, _) => &[],
    };
    if let Some(missing) = need.iter().find(|s| !sizes.contains_key(*s)) {
        return Err(format!("the scene has no frames at size {missing}"));
    }
    if sizes.is_empty() {
        return Err("the scene has no frames".into());
    }
    for (size, frames) in sizes {
        match tier {
            Tier::Poster if frames.len() != 1 => {
                return Err(format!(
                    "a poster has one frame, size {size} has {}",
                    frames.len()
                ));
            }
            Tier::Win => {
                let max = WIN_FRAMES[usize::from(significance) - 1];
                if frames.len() > max {
                    return Err(format!(
                        "a significance {significance} win has at most {max} frames"
                    ));
                }
                if frames.len() as u32 * step_ms > WIN_MAX_MS {
                    return Err(format!("a win lasts longer than {WIN_MAX_MS} ms"));
                }
            }
            _ => {}
        }
        if frames.is_empty() {
            return Err(format!("size {size} has no frames"));
        }
    }
    Ok(())
}

fn parse_frames(v: &Value, key: &str, ascii_only: bool) -> Result<BTreeMap<usize, Vec<Frame>>, String> {
    let obj = field(v, key)?
        .as_object()
        .ok_or_else(|| format!("\"{key}\" must be an object of sizes"))?;
    let mut out = BTreeMap::new();
    for (k, list) in obj {
        let size: usize = k.parse().map_err(|_| format!("size \"{k}\" is not a number"))?;
        if !SIZES.contains(&size) {
            return Err(format!("size {size} is not one of 8, 12, 20, 32"));
        }
        let list = list
            .as_array()
            .ok_or_else(|| format!("size {size} must be a list of frames"))?;
        let mut frames = Vec::with_capacity(list.len());
        for item in list {
            let frame = parse_frame(item)?;
            check_frame(&frame, size, ascii_only)?;
            frames.push(frame);
        }
        out.insert(size, frames);
    }
    Ok(out)
}

fn parse_frame(v: &Value) -> Result<Frame, String> {
    let pair = v
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or("a frame is [text, inks]")?;
    let text = pair[0].as_str().ok_or("a frame text is not a string")?;
    let inks = pair[1].as_str().ok_or("a frame ink row is not a string")?;
    Ok(Frame {
        text: text.to_string(),
        inks: inks.to_string(),
    })
}

fn check_frame(frame: &Frame, size: usize, ascii_only: bool) -> Result<(), String> {
    let width = visible_width(&frame.text);
    if width != size {
        return Err(format!("a frame is {width} cells wide at size {size}"));
    }
    if frame.inks.chars().count() != size {
        return Err(format!("the ink row of a size {size} frame is not {size} cells"));
    }
    if let Some(bad) = frame.inks.chars().find(|c| !INKS.contains(*c)) {
        return Err(format!("unknown ink {bad:?}"));
    }
    let glyph_ok = |c: char| {
        if ascii_only {
            is_printable_ascii(c)
        } else {
            glyph_allowed(c)
        }
    };
    if let Some(bad) = frame.text.chars().find(|&c| !glyph_ok(c)) {
        return Err(format!("glyph {bad:?} is outside the allow-list"));
    }
    Ok(())
}

fn ascii_sizes(sizes: &BTreeMap<usize, Vec<Frame>>) -> Option<BTreeMap<usize, Vec<Frame>>> {
    sizes
        .iter()
        .map(|(size, frames)| {
            let mapped = frames
                .iter()
                .map(|f| {
                    Some(Frame {
                        text: ascii_text(&f.text)?,
                        inks: f.inks.clone(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some((*size, mapped))
        })
        .collect()
}

fn parse_on(v: &Value) -> Result<Vec<String>, String> {
    let keys = field(v, "on")?
        .as_array()
        .ok_or("\"on\" must be a list of event keys")?;
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        match key.as_str() {
            Some(k) if !k.is_empty() && k.chars().all(event_char) => out.push(k.to_string()),
            _ => return Err(format!("\"{key}\" is not an event key")),
        }
    }
    if out.is_empty() {
        return Err("\"on\" is empty".into());
    }
    Ok(out)
}

fn event_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '_' | '.' | '-')
}

fn check_id(id: &str) -> Result<(), String> {
    let ok = id.contains('.') && id.chars().all(|c| event_char(c) && c != ':');
    if ok {
        Ok(())
    } else {
        Err(format!("\"{id}\" is not a class.name id"))
    }
}

fn parse_captions(v: &Value) -> Result<Vec<(String, String)>, String> {
    let Some(list) = v.get("captions") else {
        return Ok(Vec::new());
    };
    let list = list.as_array().ok_or("\"captions\" must be a list of pairs")?;
    let mut out = Vec::with_capacity(list.len());
    for pair in list {
        let pair = pair
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or("a caption is [short, long]")?;
        let short = pair[0].as_str().ok_or("a caption is not a string")?;
        let long = pair[1].as_str().ok_or("a caption is not a string")?;
        check_caption(short, CAPTION_SHORT, "short")?;
        check_caption(long, CAPTION_LONG, "long")?;
        out.push((short.to_string(), long.to_string()));
    }
    Ok(out)
}

fn check_caption(text: &str, max: usize, which: &str) -> Result<(), String> {
    if text.chars().count() > max {
        return Err(format!("the {which} caption is longer than {max} characters"));
    }
    if !text.chars().all(is_printable_ascii) {
        return Err(format!("the {which} caption is not ASCII"));
    }
    Ok(())
}

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value, String> {
    v.get(key).ok_or_else(|| format!("\"{key}\" is missing"))
}

fn text(v: &Value, key: &str) -> Result<String, String> {
    field(v, key)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("\"{key}\" must be a string"))
}

fn opt_text(v: &Value, key: &str) -> Result<Option<String>, String> {
    match v.get(key) {
        None => Ok(None),
        Some(_) => text(v, key).map(Some),
    }
}

fn uint(v: &Value, key: &str) -> Result<u32, String> {
    let n = field(v, key)?
        .as_u64()
        .ok_or_else(|| format!("\"{key}\" must be a whole number"))?;
    u32::try_from(n).map_err(|_| format!("\"{key}\" is too large"))
}
