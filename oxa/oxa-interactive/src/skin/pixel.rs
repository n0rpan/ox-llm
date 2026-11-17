//! The graphics tier of the arcade animation (ARCADE-ANIMATION.md): on a terminal with the
//! kitty graphics protocol the scene of the activity line is real pixel art, one text row
//! high, drawn procedurally in the colours of the theme.
//!
//! * [`pixel_key`] names the frame for a status (pure); [`pixel_frame`] paints it (pure): a
//!   16 pixel high strip, 8 pixels per cell, transparent where nothing is.
//! * [`PixelStage`] is the only state: which frames the terminal already holds. A frame is
//!   transmitted ONCE (on first use); after that a tick costs one short text row whose colour
//!   names the image (`oxa_tui::gfx_placeholder_row`). Everything is deleted again on exit,
//!   resize, suspend and a theme change.
//! * All art is OXA's own: the ox, its crate, coins and sparks. No sound.
use super::arcade::{finish_line, scene_cells, tool_icon, ArcadeRun, Finish};
use super::chrome::{activity_line_lead, activity_style};
use super::{glyphs, SPRITE_FRAME_MS};
use crate::{Activity, Skin, StatusModel};
use oxa_config::Keymap;
use oxa_tui::{color_rgb, gfx_delete, gfx_placeholder_row, gfx_transmit, Theme};
use std::collections::BTreeMap;

/// Logical pixels per cell (width) and of the strip (height).
pub const PIXEL_CELL_W: usize = 8;
pub const PIXEL_H: usize = 16;

/// What a frame shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PixelState {
    Think,
    Write,
    /// A tool runs; the index of its crate icon.
    Tool(u8),
    /// A `subagent` call runs (#871); how many children it reports (1..=3).
    Agents(u8),
    Compact,
    /// Prefill progress in sixteenths.
    Prefill(u8),
    Retry,
    Queued,
    Clear,
    Fail,
}

/// One frame of the scene: everything its picture depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PixelKey {
    pub state: PixelState,
    pub frame: u8,
    /// Width in cells.
    pub cells: u8,
    /// The theme token of the ox (the state's colour; `error` while YOLO is on).
    pub hero: &'static str,
}

/// The crate icons, in the order of [`PixelState::Tool`]'s index.
const ICON_CHARS: &str = "$≡±+?@&%*";

/// The frame for a status, or `None` when nothing animates (idle without a closing scene,
/// waiting for the user, no room). `st.activity_ms` picks the frame; 8 frames per second.
pub fn pixel_key(st: &StatusModel, run: &ArcadeRun, cells: usize) -> Option<PixelKey> {
    if cells < 8 {
        return None;
    }
    let f = st.activity_ms / SPRITE_FRAME_MS;
    let (state, frame, token) = match &st.activity {
        Activity::Blocked => return None,
        Activity::Idle => {
            let (how, ms) = run.finish?;
            let f = ms / SPRITE_FRAME_MS;
            match how {
                Finish::Clear => (PixelState::Clear, f.min(11) as u8, "success"),
                Finish::Fail => (PixelState::Fail, (f % 4) as u8, "error"),
            }
        }
        activity => {
            let (_, token) = activity_style(activity)?;
            let (state, frame) = match activity {
                Activity::Thinking => (PixelState::Think, f % 16),
                Activity::Streaming => (PixelState::Write, f % 16),
                Activity::Tool(name) => {
                    let icon = tool_icon(name, false);
                    let index = ICON_CHARS.chars().position(|c| c == icon).unwrap_or(8);
                    (PixelState::Tool(index as u8), f % 8)
                }
                Activity::Subagent(s) => (
                    PixelState::Agents(s.children.unwrap_or(1).clamp(1, 3) as u8),
                    f % 8,
                ),
                Activity::Compacting => (PixelState::Compact, f % 8),
                Activity::Prefill { done, total, .. } => {
                    let step = if *total > 0 {
                        u64::from((*done).min(*total)) * 15 / u64::from(*total)
                    } else {
                        0
                    };
                    (PixelState::Prefill(step as u8), f / 2 % 4)
                }
                Activity::Retrying { .. } => (PixelState::Retry, f % 4),
                Activity::Queued { .. } => (PixelState::Queued, f / 4 % 2),
                Activity::Idle | Activity::Blocked => return None,
            };
            (state, frame as u8, token)
        }
    };
    Some(PixelKey {
        state,
        frame,
        cells: cells.min(32) as u8,
        hero: if st.yolo { "error" } else { token },
    })
}

// ---- painting ----------------------------------------------------------------------------

type Rgb = (u8, u8, u8);

/// The colours of a frame, all from the theme.
struct Palette {
    hero: Rgb,
    shade: Rgb,
    horn: Rgb,
    dim: Rgb,
    faint: Rgb,
    gold: Rgb,
    good: Rgb,
    bad: Rgb,
    accent: Rgb,
    ink: Rgb,
}

fn scaled(c: Rgb, by: f32) -> Rgb {
    let s = |v: u8| (f32::from(v) * by).round().clamp(0.0, 255.0) as u8;
    (s(c.0), s(c.1), s(c.2))
}

impl Palette {
    fn of(theme: &Theme, hero: &str) -> Palette {
        let get = |token: &str, fallback: Rgb| color_rgb(theme.color(token)).unwrap_or(fallback);
        let hero = get(hero, (202, 155, 50));
        let dim = get("muted", (130, 136, 140));
        Palette {
            hero,
            shade: scaled(hero, 0.55),
            horn: get("text", (225, 226, 228)),
            dim,
            faint: scaled(dim, 0.6),
            gold: get("warning", (202, 155, 50)),
            good: get("success", (107, 182, 141)),
            bad: get("error", (220, 90, 90)),
            accent: get("accent", (167, 153, 210)),
            ink: (14, 14, 20),
        }
    }
}

/// A painted frame: `w` x `h` pixels, 4 bytes (RGBA) each, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelImage {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

struct Canvas {
    w: i32,
    px: Vec<[u8; 4]>,
}

impl Canvas {
    fn new(w: usize) -> Canvas {
        Canvas {
            w: w as i32,
            px: vec![[0; 4]; w * PIXEL_H],
        }
    }

    fn set(&mut self, x: i32, y: i32, c: Rgb) {
        if x >= 0 && x < self.w && y >= 0 && y < PIXEL_H as i32 {
            self.px[(y * self.w + x) as usize] = [c.0, c.1, c.2, 255];
        }
    }

    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set(xx, yy, c);
            }
        }
    }

    /// Rows of characters at `(x, y)`; `ink` maps a character to its colour (`None` = clear).
    fn sprite(&mut self, x: i32, y: i32, rows: &[&str], ink: impl Fn(char) -> Option<Rgb>) {
        for (dy, row) in rows.iter().enumerate() {
            for (dx, ch) in row.chars().enumerate() {
                if let Some(c) = ink(ch) {
                    self.set(x + dx as i32, y + dy as i32, c);
                }
            }
        }
    }

    /// The image at `scale`: every pixel a square; odd rows a little darker (scanlines).
    fn image(&self, scale: u32) -> PixelImage {
        let scale = scale.clamp(1, 8);
        let (w, h) = (self.w as u32 * scale, PIXEL_H as u32 * scale);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..PIXEL_H {
            let mut row = Vec::with_capacity((w * 4) as usize);
            for x in 0..self.w as usize {
                let mut p = self.px[y * self.w as usize + x];
                if y % 2 == 1 && p[3] != 0 {
                    p[3] = 190;
                }
                for _ in 0..scale {
                    row.extend_from_slice(&p);
                }
            }
            for _ in 0..scale {
                rgba.extend_from_slice(&row);
            }
        }
        PixelImage { w, h, rgba }
    }
}

/// The ox, facing right: head and body (8 rows), then one of four leg rows (3 rows).
const OX_BODY: [&str; 8] = [
    "...........H..H.",
    "...........HBBH.",
    "..........BBBBBB",
    ".BBBBBBBBBBBEBBB",
    "TBBBBBBBBBBBBBNN",
    "TBBBBBBBBBBBBBNN",
    ".BBBBBBBBBBBBB..",
    ".SSBBBBBBBSS....",
];
const OX_LEGS: [[&str; 3]; 4] = [
    [".LL......LL.....", ".LL......LL.....", ".LL......LL....."],
    ["LL........LL....", "L..........LL...", "L...........L..."],
    [".LL......LL.....", "..L......L......", "................"],
    ["..LL....LL......", "...L....L.......", "...L....L......."],
];
const OX_W: i32 = 16;
/// The row the ox stands on.
const GROUND: i32 = 14;

/// The ox with its feet on the ground at `x`, `lift` pixels up, legs of `gait` (0 = standing).
fn ox(c: &mut Canvas, p: &Palette, x: i32, lift: i32, gait: usize, dazed: bool) {
    let y = GROUND - 11 - lift;
    let ink = |ch: char| match ch {
        'B' => Some(p.hero),
        'S' | 'N' | 'L' | 'T' => Some(p.shade),
        'H' => Some(p.horn),
        'E' => Some(if dazed { p.bad } else { p.ink }),
        _ => None,
    };
    c.sprite(x, y, &OX_BODY, ink);
    c.sprite(x, y + 8, &OX_LEGS[gait % 4], ink);
}

/// Far ridges (32 pixels long), heights in pixels.
const RIDGE: [u8; 32] = [
    1, 1, 2, 3, 4, 5, 5, 4, 3, 2, 2, 1, 1, 0, 0, 1, 2, 2, 3, 3, 2, 1, 0, 0, 1, 2, 4, 6, 4, 3, 2, 1,
];

/// Sky, ridges and ground. `travel` = how far the world has scrolled, in frames.
fn backdrop(c: &mut Canvas, p: &Palette, travel: i32, twinkle: i32) {
    // Stars: fixed places, twinkling.
    for x in 0..c.w {
        let h = (x * 37 + 11) % 23;
        if h < 2 {
            let y = (x * 7) % 6;
            let lit = (x + twinkle / 4) % 3 != 0;
            c.set(x, y, if lit { p.dim } else { p.faint });
        }
    }
    // Ridges: two pixels a frame.
    for x in 0..c.w {
        let h = i32::from(RIDGE[((x + travel * 2).rem_euclid(32)) as usize]);
        for y in GROUND - h..GROUND {
            c.set(x, y, p.faint);
        }
    }
    // Ground: the line, and marks that rush by.
    for x in 0..c.w {
        c.set(x, GROUND, p.dim);
        if (x + travel * 4).rem_euclid(8) < 3 {
            c.set(x, GROUND + 1, p.faint);
        }
    }
}

/// Crate icons, 5x5.
const ICONS: [[&str; 5]; 9] = [
    [".###.", "##...", ".###.", "...##", ".###."],
    ["#####", ".....", "#####", ".....", "#####"],
    ["..#..", ".###.", "..#..", ".....", ".###."],
    ["..#..", "..#..", "#####", "..#..", "..#.."],
    [".###.", "...#.", "..#..", ".....", "..#.."],
    [".###.", "#.#.#", "#.###", "#....", ".###."],
    [".#...", "#.#..", ".#.#.", "#.#..", ".#.#."],
    ["#...#", "...#.", "..#..", ".#...", "#...#"],
    ["#.#.#", ".###.", "#####", ".###.", "#.#.#"],
];

/// A tool's crate (10x10) standing on the ground at `x`.
fn tool_crate(c: &mut Canvas, p: &Palette, x: i32, icon: usize) {
    let y = GROUND - 10;
    c.rect(x, y, 10, 10, p.accent);
    c.rect(x + 1, y + 1, 8, 8, scaled(p.accent, 0.35));
    c.sprite(x + 2, y + 2, &ICONS[icon % 9], |ch| (ch == '#').then_some(p.horn));
}

/// A coin (5x5); `spin` 0..4 turns it.
fn coin(c: &mut Canvas, p: &Palette, x: i32, y: i32, spin: i32) {
    const FACES: [[&str; 5]; 3] = [
        [".###.", "#o###", "#####", "#####", ".###."],
        ["..#..", ".#o#.", ".###.", ".###.", "..#.."],
        ["..#..", "..#..", "..#..", "..#..", "..#.."],
    ];
    let face = [0, 1, 2, 1][spin.rem_euclid(4) as usize];
    c.sprite(x, y, &FACES[face], |ch| match ch {
        '#' => Some(p.gold),
        'o' => Some(p.horn),
        _ => None,
    });
}

/// A four-pointed spark, `size` 1 or 2.
fn spark(c: &mut Canvas, x: i32, y: i32, size: i32, colour: Rgb) {
    c.set(x, y, colour);
    for d in 1..=size {
        c.set(x - d, y, colour);
        c.set(x + d, y, colour);
        c.set(x, y - d, colour);
        c.set(x, y + d, colour);
    }
}

/// Paint `key` at `scale` (device pixels per logical pixel) in the colours of `theme`.
pub fn pixel_frame(key: PixelKey, theme: &Theme, scale: u32) -> PixelImage {
    let p = Palette::of(theme, key.hero);
    let w = usize::from(key.cells.max(1)) * PIXEL_CELL_W;
    let mut c = Canvas::new(w);
    let f = i32::from(key.frame);
    let wi = w as i32;
    match key.state {
        // Thinking: the ox gallops through the night; a thought gathers over its horns.
        PixelState::Think => {
            backdrop(&mut c, &p, f, f);
            ox(
                &mut c,
                &p,
                6,
                i32::from(f / 2 % 2 == 1),
                1 + (f as usize / 2) % 3,
                false,
            );
            for k in 0..(f / 4 % 4) {
                c.rect(26 + k * 4, 7 - k * 2, 2, 2, p.horn);
            }
        }
        // Writing: the ox runs ahead and pulls a ribbon of ink behind it.
        PixelState::Write => {
            backdrop(&mut c, &p, f, f);
            let x = wi - OX_W - 6;
            for rx in 0..x {
                let wave = [0, 1, 2, 1, 0, -1, -2, -1][((rx + f * 2).rem_euclid(8)) as usize];
                c.set(rx, 8 + wave, p.accent);
                c.set(rx, 9 + wave, scaled(p.accent, 0.6));
            }
            ox(
                &mut c,
                &p,
                x,
                i32::from(f / 2 % 2 == 1),
                1 + (f as usize / 2) % 3,
                false,
            );
        }
        // A tool: the ox butts its crate; a coin jumps out at the hit.
        PixelState::Tool(icon) => {
            backdrop(&mut c, &p, 0, f);
            let step = f / 2 % 4;
            let (lunge, knock) = [(0, 0), (5, 0), (9, 2), (4, 1)][step as usize];
            let crate_x = 6 + OX_W + 9;
            tool_crate(&mut c, &p, crate_x + knock, usize::from(icon));
            ox(
                &mut c,
                &p,
                6 + lunge,
                0,
                if step == 0 { 0 } else { 1 + step as usize % 3 },
                false,
            );
            if step >= 2 {
                spark(&mut c, crate_x - 1, GROUND - 6, 3 - step, p.gold);
                coin(&mut c, &p, crate_x + 14, GROUND - 9 - (step - 2) * 4, f);
            }
        }
        // A subagent: the ox butts one crate per child it reports (#871).
        PixelState::Agents(children) => {
            backdrop(&mut c, &p, 0, f);
            let step = f / 2 % 4;
            let lunge = [0, 5, 9, 4][step as usize];
            for k in 0..i32::from(children.clamp(1, 3)) {
                let x = 6 + OX_W + 9 + k * 16;
                if x + 14 < wi {
                    tool_crate(&mut c, &p, x, 6);
                }
            }
            ox(
                &mut c,
                &p,
                6 + lunge,
                0,
                if step == 0 { 0 } else { 1 + step as usize % 3 },
                false,
            );
        }
        // Compacting: blocks are pushed into the save chip; its light answers.
        PixelState::Compact => {
            backdrop(&mut c, &p, 0, f);
            let chip = wi - 22;
            let blocks = (6 - f).max(0);
            ox(&mut c, &p, 4 + f.min(6), 0, 1 + (f as usize) % 3, false);
            for b in 0..blocks {
                let x = chip - 5 - b * 5;
                if x > 26 + f.min(6) {
                    c.rect(x, GROUND - 4, 4, 4, p.accent);
                }
            }
            c.rect(chip, GROUND - 11, 14, 11, p.dim);
            c.rect(chip + 2, GROUND - 11, 8, 4, p.faint);
            c.rect(chip + 3, GROUND - 5, 8, 4, p.horn);
            c.rect(chip + 11, GROUND - 3, 2, 2, if f >= 6 { p.good } else { p.faint });
        }
        // Prefill: the ox pushes the prompt block; where it is, is how far the prefill is.
        PixelState::Prefill(step) => {
            backdrop(&mut c, &p, 0, f);
            let span = (wi - OX_W - 12).max(0);
            let x = 2 + span * i32::from(step.min(15)) / 15;
            for dot in (x + OX_W + 10..wi - 1).step_by(4) {
                c.set(dot, GROUND - 2, p.dim);
            }
            ox(&mut c, &p, x, 0, 1 + (f as usize) % 3, false);
            c.rect(x + OX_W, GROUND - 7, 7, 7, p.gold);
            c.rect(x + OX_W + 1, GROUND - 6, 5, 5, scaled(p.gold, 0.5));
        }
        // A retry: the ox is dazed; stars circle its head.
        PixelState::Retry => {
            backdrop(&mut c, &p, 0, f);
            ox(&mut c, &p, 6, 0, 0, true);
            let orbit = [(0, 0), (5, -1), (9, 0), (5, 1)];
            for k in 0..2 {
                let (dx, dy) = orbit[((f + k * 2) % 4) as usize];
                spark(&mut c, 14 + dx, 2 + dy, 1, p.bad);
            }
        }
        // Queued: the ox waits its turn.
        PixelState::Queued => {
            backdrop(&mut c, &p, 0, f * 4);
            ox(&mut c, &p, 6, 0, 0, false);
            let z = ["###", ".#.", "###"];
            c.sprite(26 + f * 3, 6 - f * 3, &z, |ch| (ch == '#').then_some(p.dim));
        }
        // Stage clear: the ox jumps, coins and sparks fly.
        PixelState::Clear => {
            backdrop(&mut c, &p, 0, f);
            let hop = [0, 2, 3, 4, 4, 3, 2, 0, 0, 1, 2, 0][(f.clamp(0, 11)) as usize];
            ox(&mut c, &p, 6, hop, if hop > 0 { 2 } else { 0 }, false);
            for k in 0..3 {
                let x = 28 + k * 14 + f;
                let y = GROUND - 6 - ((f * 3 + k * 5) % 9);
                if x + 5 < wi {
                    coin(&mut c, &p, x, y, f + k);
                }
            }
            for k in 0..4 {
                let x = (31 + k * 23 + f * 5) % (wi - 26).max(1) + 24;
                let y = 1 + (k * 5 + f * 2) % 9;
                spark(&mut c, x, y, 1 + (f + k) % 2, p.good);
            }
        }
        // Failed: the ox stopped at a cross.
        PixelState::Fail => {
            backdrop(&mut c, &p, 0, 0);
            ox(&mut c, &p, 6, 0, 0, true);
            if f != 3 {
                for d in 0..7 {
                    c.rect(28 + d, GROUND - 9 + d, 2, 1, p.bad);
                    c.rect(34 - d, GROUND - 9 + d, 2, 1, p.bad);
                }
            }
        }
    }
    c.image(scale)
}

// ---- the stage ---------------------------------------------------------------------------

/// The first image id of the stage (ids are 24 bits: they travel as a colour).
pub const PIXEL_ID_BASE: u32 = 0x00F0_A000;
/// More frames than this are never held: the stage starts over.
const MAX_IMAGES: usize = 384;

/// Which frames the terminal holds. Owned by the UI loop.
#[derive(Debug, Clone)]
pub struct PixelStage {
    ids: BTreeMap<PixelKey, u32>,
    scale: u32,
    /// The theme the held frames were painted for (the caller's counter).
    epoch: u64,
    /// Bytes of image data sent so far (for the cost report).
    pub sent_bytes: u64,
}

impl PixelStage {
    /// A stage for cells of `cell_px` pixels (`None` = unknown: two device pixels per pixel).
    pub fn new(cell_px: Option<(u16, u16)>) -> PixelStage {
        let scale = match cell_px {
            // Rounded up: the terminal then scales the strip down a little, which stays sharp.
            Some((_, h)) if h > 0 => u32::from(h).div_ceil(PIXEL_H as u32).clamp(1, 4),
            _ => 2,
        };
        PixelStage {
            ids: BTreeMap::new(),
            scale,
            epoch: 0,
            sent_bytes: 0,
        }
    }

    /// Frames the terminal holds.
    pub fn images(&self) -> usize {
        self.ids.len()
    }

    /// The row that shows `key`. A frame the terminal does not hold yet is painted and its
    /// transmission appended to `out` (once per frame; `out` must reach the terminal before
    /// the row is drawn). A new `epoch` (the theme changed) deletes the held frames first.
    pub fn show(&mut self, key: PixelKey, theme: &Theme, epoch: u64, out: &mut String) -> String {
        if epoch != self.epoch || self.ids.len() >= MAX_IMAGES {
            self.clear(out);
            self.epoch = epoch;
        }
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = PIXEL_ID_BASE + self.ids.len() as u32;
                let image = pixel_frame(key, theme, self.scale);
                let sent = gfx_transmit(id, image.w, image.h, &image.rgba, u16::from(key.cells), 1);
                self.sent_bytes += sent.len() as u64;
                out.push_str(&sent);
                self.ids.insert(key, id);
                id
            }
        };
        gfx_placeholder_row(id, u16::from(key.cells))
    }

    /// Delete every held frame (exit, resize, suspend, theme change): the sequences are
    /// appended to `out`. Nothing of the stage is left in the terminal afterwards.
    pub fn clear(&mut self, out: &mut String) {
        for id in self.ids.values() {
            out.push_str(&gfx_delete(*id));
        }
        self.ids.clear();
    }
}

/// The activity line with the pixel scene in front (the graphics tier's counterpart of
/// `oxa_arcade_line`): the image row, the coins of the run as text, then the badge and the
/// rest. `None` when there is nothing to animate (the caller then draws the text line).
/// New frames are transmitted through `out`.
pub fn oxa_pixel_line(
    st: &StatusModel,
    keymap: &Keymap,
    run: &ArcadeRun,
    width: u16,
    theme: &Theme,
    skin: Skin,
    stage: &mut PixelStage,
    epoch: u64,
    out: &mut String,
) -> Option<String> {
    if skin != Skin::Oxa || theme.is_plain() {
        return None;
    }
    let key = pixel_key(st, run, scene_cells(width))?;
    let mut lead = stage.show(key, theme, epoch, out);
    if matches!(key.state, PixelState::Clear | PixelState::Fail) {
        let (how, _) = run.finish?;
        return Some(finish_line(lead, how, run, width, theme, skin));
    }
    if run.coins > 0 && matches!(key.state, PixelState::Tool(_) | PixelState::Agents(_)) {
        let coins = format!("{}{}", glyphs(skin).diamond, run.coins.min(999));
        lead.push_str(&format!(" {}", theme.fg("warning", &coins)));
    }
    activity_line_lead(st, keymap, Some(lead), width, theme, skin)
}
