//! The arcade animations of the activity line (O-184, ARCADE-ANIMATION.md): a one-row pixel
//! scene in front of the state badge while a run is in progress, and a short "stage clear"
//! after it. All designs are OXA's own (the charging ox, its crates and coins).
//!
//! Rules (on top of the skin's):
//! * One row, a fixed number of cells ([`scene_cells`]): the scene never changes the height or
//!   the layout of anything, and never touches the transcript.
//! * A scene is a pure function of the status, the run's counters and the activity's elapsed
//!   time; one frame per [`SPRITE_FRAME_MS`] (8 frames per second). No clock, no state here
//!   except [`ArcadeState`], which the UI loop owns and feeds.
//! * Colour only through theme tokens (so it follows the live Omarchy palette); block
//!   elements with `Skin::Oxa`, plain ASCII with `Skin::OxaAscii`; a plain theme (`NO_COLOR`,
//!   `TERM=dumb`) and `ui.animation = "off"` give the static badge and no scene at all.
//! * No sound, ever.
use super::chrome::{activity_line_lead, activity_style};
use super::stage::Director;
use super::{badge, glyphs, oxa_activity_line_with, SPRITE_FRAME_MS};
use crate::{Activity, Block, Skin, StatusModel};
use oxa_config::Keymap;
use oxa_proto::Level;
use oxa_tui::{truncate_to_width, Theme};

/// The `ui.animation` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Animation {
    /// The arcade scenes of this module.
    #[default]
    Arcade,
    /// The small six-cell sprites of O-184 (the bouncing pixel).
    Minimal,
    /// Nothing moves: the static text badge.
    Off,
}

impl Animation {
    /// The accepted `ui.animation` values, in help order (also `--animation`).
    pub const ALL: [&'static str; 3] = ["arcade", "minimal", "off"];

    /// `"minimal"` and `"off"`; anything else (unset, `"arcade"`) is the arcade.
    pub fn from_setting(setting: Option<&str>) -> Animation {
        match setting.map(str::trim) {
            Some("minimal") => Animation::Minimal,
            Some("off") => Animation::Off,
            _ => Animation::Arcade,
        }
    }
}

/// The `ui.animationScene` setting: which arcade scene plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArcadeScene {
    /// The ox charges through blocks, butts tool crates open and collects a coin per tool.
    Charge,
    /// A small ship in a two-layer starfield; warp streaks while the reply streams.
    Stars,
    /// Columns that stack up and flash when the row is full. The default on a text terminal.
    #[default]
    Stack,
}

impl ArcadeScene {
    /// `"charge"` and `"stars"`; anything else (unset, `"stack"`, `"graphics"` on a terminal
    /// without graphics) is the stack.
    pub fn from_setting(setting: Option<&str>) -> ArcadeScene {
        match setting.map(str::trim) {
            Some("charge") => ArcadeScene::Charge,
            Some("stars") => ArcadeScene::Stars,
            _ => ArcadeScene::Stack,
        }
    }
}

/// Whether the pixel scene of the graphics tier plays (`skin/pixel.rs`), from the settings
/// `ui.animationScene` and `ui.animationGraphics` and what the terminal can do:
/// `answered` = it answered the kitty graphics query, `placeholders` = it is known to draw
/// Unicode placeholders (kitty, Ghostty).
///
/// * `ui.animationGraphics = "off"`: never.
/// * `ui.animationScene` unset: when the terminal answered and draws placeholders.
/// * `ui.animationScene = "graphics"`: whenever the terminal answered (the user's override
///   for a terminal this build does not know).
/// * a text scene by name (`stack`, `stars`, `charge`): never.
pub fn graphics_wanted(
    scene: Option<&str>,
    graphics: Option<&str>,
    answered: bool,
    placeholders: bool,
) -> bool {
    if graphics.map(str::trim) == Some("off") || !answered {
        return false;
    }
    match scene.map(str::trim) {
        Some("graphics") => true,
        Some("stack") | Some("stars") | Some("charge") => false,
        _ => placeholders,
    }
}

/// How a run ended, for the short closing scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// The run completed: "stage clear".
    Clear,
    /// The run failed or its reply was cut.
    Fail,
}

/// What a scene shows of the run besides the status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArcadeRun {
    /// Tool calls of the run so far (one coin each).
    pub coins: u32,
    /// The closing scene and the milliseconds it has been showing; `None` while a run is in
    /// progress and once the closing scene is over.
    pub finish: Option<(Finish, u64)>,
}

/// How long the closing scene shows.
pub const FINISH_MS: u64 = 1500;

/// The run's counters, kept by the UI loop: fed with what the screen shows, read as an
/// [`ArcadeRun`]. No clock of its own (the caller passes its `now`).
#[derive(Debug, Clone, Default)]
pub struct ArcadeState {
    coins: u32,
    was_running: bool,
    finish: Option<(Finish, u64)>,
}

impl ArcadeState {
    /// Note the state of the run. `running` is "the activity line shows a state"; `blocks` is the
    /// transcript: the coins are the tool calls since the user's last message, and how the
    /// run ended is read from the last block when the run stops.
    pub fn observe(&mut self, running: bool, blocks: &[Block], now_ms: u64) {
        if running && !self.was_running {
            self.finish = None;
        }
        if running || self.was_running {
            let calls = blocks
                .iter()
                .rev()
                .take_while(|b| !b.is_user())
                .filter(|b| matches!(b, Block::Tool(_)))
                .count();
            self.coins = u32::try_from(calls).unwrap_or(u32::MAX);
        }
        if !running && self.was_running {
            self.finish = finish_of(blocks.last()).map(|f| (f, now_ms));
        }
        self.was_running = running;
    }

    /// The counters at `now_ms`.
    pub fn run(&self, now_ms: u64) -> ArcadeRun {
        let finish = self
            .finish
            .map(|(f, since)| (f, now_ms.saturating_sub(since)))
            .filter(|(_, ms)| *ms < FINISH_MS);
        ArcadeRun {
            coins: self.coins,
            finish,
        }
    }

    /// `true` while the closing scene shows (the loop keeps stepping the animation).
    pub fn finishing(&self, now_ms: u64) -> bool {
        self.run(now_ms).finish.is_some()
    }

    /// `true` once, when the closing scene has just ended (the loop redraws to take it away).
    pub fn take_expired(&mut self, now_ms: u64) -> bool {
        match self.finish {
            Some((_, since)) if now_ms.saturating_sub(since) >= FINISH_MS => {
                self.finish = None;
                true
            }
            _ => false,
        }
    }
}

/// The closing scene of a run whose transcript ends with `last`: none after an abort (the
/// user stopped it; nothing to celebrate, nothing failed).
fn finish_of(last: Option<&Block>) -> Option<Finish> {
    match last {
        Some(Block::Shown { why, .. }) if why == oxa_proto::shown_why::ABORTED => None,
        Some(Block::Shown { why, .. }) if why == oxa_proto::shown_why::NOTICE => Some(Finish::Clear),
        Some(Block::Shown { .. }) => Some(Finish::Fail),
        Some(Block::Assistant { error: Some(_), .. }) => Some(Finish::Fail),
        Some(Block::Notice {
            level: Level::Error, ..
        }) => Some(Finish::Fail),
        _ => Some(Finish::Clear),
    }
}

/// Cells of a scene at a terminal width: 12, 8 under 72 columns, none under 40.
pub fn scene_cells(width: u16) -> usize {
    match width {
        0..=39 => 0,
        40..=71 => 8,
        _ => 12,
    }
}

/// The most cells a scene has.
pub(super) const MAX_CELLS: usize = 12;

/// The label on a tool's crate: one character per kind of tool (ASCII when `ascii`).
pub fn tool_icon(name: &str, ascii: bool) -> char {
    let n = name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    if has(&["web", "fetch", "http", "url"]) {
        '@'
    } else if has(&["bash", "shell", "exec", "run", "script"]) {
        '$'
    } else if has(&["edit", "patch", "replace"]) {
        if ascii {
            '~'
        } else {
            '±'
        }
    } else if has(&["write", "create"]) {
        '+'
    } else if has(&["grep", "search", "find", "glob", "ls", "list"]) {
        '?'
    } else if has(&["read", "cat", "view", "open"]) {
        if ascii {
            '='
        } else {
            '≡'
        }
    } else if has(&["task", "agent"]) {
        '&'
    } else if has(&["mcp", "__"]) {
        '%'
    } else {
        '*'
    }
}

// ---- the cell buffer ---------------------------------------------------------------------

/// Colour roles of a scene's cells; mapped to theme tokens by [`Canvas::paint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ink {
    /// The hero (the ox, the ship, the stack): the activity's own colour.
    Hero,
    /// Ground, dust, far stars.
    Dim,
    /// Coins, sparks, debris.
    Gold,
    /// A tool's crate.
    Crate,
    /// "Clear".
    Good,
    /// Lives, failure.
    Bad,
    /// The current tool's icon (the `I` ink of a data scene): the tool colour.
    Icon,
}

pub(super) struct Canvas {
    cells: [(char, Ink); MAX_CELLS],
    len: usize,
}

impl Canvas {
    pub(super) fn new(len: usize) -> Canvas {
        Canvas {
            cells: [(' ', Ink::Dim); MAX_CELLS],
            len: len.min(MAX_CELLS),
        }
    }

    pub(super) fn put(&mut self, x: usize, ch: char, ink: Ink) {
        if x < self.len {
            self.cells[x] = (ch, ink);
        }
    }

    fn text(&mut self, x: usize, s: &str, ink: Ink) {
        for (i, ch) in s.chars().enumerate() {
            self.put(x + i, ch, ink);
        }
    }

    /// The row as styled text: runs of one colour share one escape pair; blanks carry none.
    pub(super) fn paint(&self, hero: &str, hot: bool, theme: &Theme) -> String {
        let mut out = String::with_capacity(self.len * 4 + 48);
        let mut i = 0;
        while i < self.len {
            let (ch, ink) = self.cells[i];
            if ch == ' ' {
                out.push(' ');
                i += 1;
                continue;
            }
            let mut run = String::new();
            let mut j = i;
            while j < self.len && self.cells[j].1 == ink && self.cells[j].0 != ' ' {
                run.push(self.cells[j].0);
                j += 1;
            }
            let token = match ink {
                // O-182: the hero burns while YOLO is on.
                Ink::Hero if hot => "error",
                Ink::Hero => hero,
                Ink::Dim => "muted",
                Ink::Gold => "warning",
                Ink::Crate => "accent",
                Ink::Good => "success",
                Ink::Bad => "error",
                Ink::Icon => "warning",
            };
            out.push_str(&theme.fg(token, &run));
            i = j;
        }
        out
    }
}

/// The glyphs of the scenes.
struct Art {
    /// The ox, two gallop frames of four cells.
    ox: [&'static str; 2],
    /// Blocks on the track (low, high, small), the debris frames and the dust.
    blocks: [char; 3],
    debris: [char; 2],
    dust: char,
    /// The written line behind the ox and the wet ink right behind it.
    line: char,
    ink: char,
    /// A crate's sides.
    crate_l: char,
    crate_r: char,
    coin: char,
    life: char,
    /// The save bar: left end, fill, right end.
    bar: [char; 3],
    /// The ship (two cells), stars (far, near) and warp streaks (far, near).
    ship: &'static str,
    stars: [char; 2],
    streaks: [char; 2],
    /// Column heights 1..=8.
    levels: [char; 8],
    spark: [char; 3],
    fail: char,
}

const ART: Art = Art {
    ox: ["▟█▛▘", "▜█▛▘"],
    blocks: ['▄', '▆', '▖'],
    debris: ['▚', '▞'],
    dust: '·',
    line: '▂',
    ink: '░',
    crate_l: '▐',
    crate_r: '▌',
    coin: '◆',
    life: '♥',
    bar: ['▐', '█', '▌'],
    ship: "═▶",
    stars: ['·', '*'],
    streaks: ['─', '━'],
    levels: ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'],
    spark: ['+', '·', '*'],
    fail: '✗',
};

const ART_ASCII: Art = Art {
    ox: ["{ox>", "(ox>"],
    blocks: ['n', 'H', 'o'],
    debris: ['*', '%'],
    dust: '.',
    line: '_',
    ink: '~',
    crate_l: '[',
    crate_r: ']',
    coin: 'o',
    life: 'v',
    bar: ['[', '#', ']'],
    ship: "=>",
    stars: ['.', '*'],
    streaks: ['-', '='],
    levels: ['.', '.', ':', ':', '=', '=', '#', '#'],
    spark: ['+', '.', '*'],
    fail: 'x',
};

// ---- the scenes --------------------------------------------------------------------------

/// The track of the charge scene: where blocks stand (index into `Art::blocks` + 1).
const TRACK: [u8; 16] = [0, 0, 1, 0, 0, 3, 0, 0, 0, 2, 0, 0, 1, 0, 3, 0];

/// The ox at `x`, galloping.
fn ox(c: &mut Canvas, x: usize, frame: u64, art: &Art) {
    c.text(x, art.ox[(frame / 2 % 2) as usize], Ink::Hero);
}

/// The coin counter in the last three cells (`◆7`, `◆42`); nothing without coins or room.
fn coins(c: &mut Canvas, coins: u32, art: &Art) {
    if coins == 0 || c.len < 12 {
        return;
    }
    let n = coins.min(99).to_string();
    let x = c.len - 1 - n.len();
    c.put(x, art.coin, Ink::Gold);
    c.text(x + 1, &n, Ink::Gold);
}

/// A tool's crate (three cells) at `x`.
fn tool_crate(c: &mut Canvas, x: usize, name: &str, ascii: bool, art: &Art) {
    c.put(x, art.crate_l, Ink::Crate);
    c.put(x + 1, tool_icon(name, ascii), Ink::Crate);
    c.put(x + 2, art.crate_r, Ink::Crate);
}

/// The charging ox, by activity.
fn charge(c: &mut Canvas, st: &StatusModel, run: &ArcadeRun, f: u64, ascii: bool, art: &Art) {
    let w = c.len;
    match &st.activity {
        Activity::Idle | Activity::Blocked => {}
        // Thinking: blocks come down the track; the ox breaks every one of them.
        Activity::Thinking => {
            let scroll = (f / 2) as usize;
            for x in 4..w {
                let kind = TRACK[(x - 4 + scroll) % TRACK.len()];
                if kind == 0 {
                    continue;
                }
                if x == 4 {
                    c.put(x, art.debris[(f % 2) as usize], Ink::Gold);
                } else {
                    c.put(x, art.blocks[usize::from(kind) - 1], Ink::Dim);
                }
            }
            ox(c, 0, f, art);
        }
        // Writing: the ox pulls the line across the row, wet ink right behind it.
        Activity::Streaming => {
            let span = w.saturating_sub(4);
            let x = (f / 2) as usize % (span + 1);
            for i in 0..x {
                let (ch, ink) = if i + 1 == x {
                    (art.ink, Ink::Hero)
                } else {
                    (art.line, Ink::Dim)
                };
                c.put(i, ch, ink);
            }
            ox(c, x, f, art);
        }
        // A tool: the ox butts the tool's crate; one coin per tool call of the run.
        Activity::Tool(name) => {
            let step = f % 4;
            let lunge = usize::from(step == 1 || step == 2);
            let knocked = usize::from(step == 2 && w >= 12);
            tool_crate(c, 5 + knocked, name, ascii, art);
            ox(c, lunge, f, art);
            coins(c, run.coins, art);
        }
        // Compacting: the context is pressed into a save block.
        Activity::Compacting => {
            let step = (f % 12) as usize;
            let shortest = 3;
            let bar = w.saturating_sub(step).max(shortest);
            c.put(0, art.bar[0], Ink::Hero);
            for x in 1..bar - 1 {
                c.put(x, art.bar[1], Ink::Hero);
            }
            c.put(bar - 1, art.bar[2], Ink::Hero);
            if bar + 5 <= w && step.is_multiple_of(2) {
                c.text(bar + 1, "SAVE", Ink::Gold);
            }
        }
        // Prefill: the ox pushes the prompt block down the track, as far as the prefill is.
        Activity::Prefill { done, total, .. } => {
            let span = w.saturating_sub(5);
            let x = if *total > 0 {
                (u64::from((*done).min(*total)) * span as u64 / u64::from(*total)) as usize
            } else {
                0
            };
            for i in x + 5..w {
                c.put(i, art.dust, Ink::Dim);
            }
            ox(c, x, f, art);
            c.put(x + 4, art.bar[1], Ink::Gold);
        }
        // A retry: the ox blinks; one life per attempt that is left.
        Activity::Retrying {
            attempt,
            max_attempts,
            ..
        } => {
            if f % 4 != 3 {
                ox(c, 0, 0, art);
            }
            let left = max_attempts.saturating_sub(*attempt).min(5) as usize;
            let spent = (*attempt).min(*max_attempts).min(5 - left as u32) as usize;
            for i in 0..left {
                c.put(5 + i, art.life, Ink::Bad);
            }
            for i in 0..spent {
                c.put(5 + left + i, art.dust, Ink::Dim);
            }
        }
        // Queued: the ox waits its turn.
        Activity::Queued { .. } => {
            ox(c, 0, 0, art);
            let step = (f / 4 % 3) as usize;
            c.put(5 + step, if step == 2 { 'Z' } else { 'z' }, Ink::Dim);
        }
    }
}

/// The ship in the starfield.
fn stars(c: &mut Canvas, st: &StatusModel, run: &ArcadeRun, f: u64, ascii: bool, art: &Art) {
    let w = c.len;
    let warp = matches!(st.activity, Activity::Streaming);
    let (far, near) = if warp {
        (f as usize, f as usize * 2)
    } else {
        ((f / 4) as usize, (f / 2) as usize)
    };
    let glyph = if warp { art.streaks } else { art.stars };
    for x in 0..w {
        if (x + far) % 5 == 2 {
            c.put(x, glyph[0], Ink::Dim);
        }
        if (x + near) % 7 == 0 {
            c.put(x, glyph[1], if warp { Ink::Hero } else { Ink::Dim });
        }
    }
    match &st.activity {
        Activity::Tool(name) => {
            // Docked at the tool's station.
            let dock = usize::from(f % 4 < 2);
            c.text(2 + dock, art.ship, Ink::Hero);
            tool_crate(c, 5, name, ascii, art);
            coins(c, run.coins, art);
        }
        _ => {
            let bob = usize::from(f / 4 % 2 == 1);
            c.put(0, ' ', Ink::Dim);
            c.text(1 + bob, art.ship, Ink::Hero);
        }
    }
}

/// The stacking columns.
fn stack(c: &mut Canvas, st: &StatusModel, run: &ArcadeRun, f: u64, ascii: bool, art: &Art) {
    let tool = match &st.activity {
        Activity::Tool(name) => Some(name),
        _ => None,
    };
    let cols = if tool.is_some() {
        c.len
            .saturating_sub(if run.coins > 0 && c.len >= 12 { 7 } else { 4 })
    } else {
        c.len
    };
    let speed: u64 = if matches!(st.activity, Activity::Streaming) {
        4
    } else {
        2
    };
    let full = cols as u64 * 8;
    let flash = 8;
    // The first frame already shows a block: a run never starts on an empty row.
    let units = ((f + 1) * speed) % (full + flash * speed).max(1);
    for x in 0..cols {
        if units >= full {
            // The row is full: it flashes, then the stack starts again.
            let on = f.is_multiple_of(2);
            c.put(x, art.levels[if on { 7 } else { 5 }], Ink::Good);
        } else {
            let h = units.saturating_sub(x as u64 * 8).min(8) as usize;
            if h > 0 {
                c.put(x, art.levels[h - 1], Ink::Hero);
            }
        }
    }
    if let Some(name) = tool {
        tool_crate(c, cols + 1, name, ascii, art);
        coins(c, run.coins, art);
    }
}

/// The closing scene: the ox among sparks ("stage clear"), or stopped at a cross.
fn finish(c: &mut Canvas, how: Finish, f: u64, art: &Art) {
    c.text(0, art.ox[0], Ink::Hero);
    match how {
        Finish::Clear => {
            for x in 5..c.len {
                let k = (x * 5 + (f / 2) as usize) % 6;
                if k.is_multiple_of(2) {
                    c.put(x, art.spark[k / 2], if k == 0 { Ink::Good } else { Ink::Gold });
                }
            }
        }
        Finish::Fail => {
            if f % 4 != 3 {
                c.put(5, art.fail, Ink::Bad);
            }
        }
    }
}

/// One frame of `scene`: exactly `cells` columns (styled), or `None` when there is nothing to
/// animate (idle, waiting for the user, no room). `st.activity_ms` picks the frame.
pub fn arcade_scene(
    scene: ArcadeScene,
    st: &StatusModel,
    run: &ArcadeRun,
    cells: usize,
    theme: &Theme,
    skin: Skin,
) -> Option<String> {
    if cells < 8 || matches!(st.activity, Activity::Idle | Activity::Blocked) {
        return None;
    }
    let (_, token) = activity_style(&st.activity)?;
    let ascii = skin != Skin::Oxa;
    let art = if ascii { &ART_ASCII } else { &ART };
    let f = st.activity_ms / SPRITE_FRAME_MS;
    let mut c = Canvas::new(cells);
    // The states only the ox has a picture for play as the charge scene everywhere.
    let own = matches!(
        st.activity,
        Activity::Thinking | Activity::Streaming | Activity::Tool(_)
    );
    match scene {
        ArcadeScene::Stars if own => stars(&mut c, st, run, f, ascii, art),
        ArcadeScene::Stack if own => stack(&mut c, st, run, f, ascii, art),
        _ => charge(&mut c, st, run, f, ascii, art),
    }
    Some(c.paint(token, st.yolo, theme))
}

/// The closing line behind `lead` (a scene, already styled): the `CLEAR` / `FAILED` badge and
/// the coins of the run.
pub(super) fn finish_line(
    lead: String,
    how: Finish,
    run: &ArcadeRun,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> String {
    let (word, token) = match how {
        Finish::Clear => ("CLEAR", "success"),
        Finish::Fail => ("FAILED", "error"),
    };
    let mut line = format!(" {lead} {}", badge(word, token, theme));
    if run.coins > 0 {
        let g = glyphs(skin);
        let tools = if run.coins == 1 { "tool" } else { "tools" };
        line.push_str(&format!(
            "  {} {}",
            theme.fg("warning", &format!("{}{}", g.diamond, run.coins)),
            theme.fg("muted", tools)
        ));
    }
    truncate_to_width(&line, usize::from(width), "...")
}

/// The activity line with the animation setting (the UI loop's entry point).
///
/// * `Animation::Arcade`: the line starts with the scene of the activity; for [`FINISH_MS`]
///   after a run, an otherwise idle line shows the closing scene (`CLEAR` / `FAILED`).
/// * `Animation::Minimal`: O-184's small sprites ([`oxa_activity_line_with`], motion on).
/// * `Animation::Off`, a plain theme (`NO_COLOR`, `TERM=dumb`) and waiting for the user's
///   answer: the static badge, no escape sequence that moves anything, no closing scene.
///
/// Idle and no closing scene: `None`. Never wider than `width`; one line, always.
pub fn oxa_arcade_line(
    st: &StatusModel,
    keymap: &Keymap,
    animation: Animation,
    scene: ArcadeScene,
    run: &ArcadeRun,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Option<String> {
    let arcade = animation == Animation::Arcade && skin.is_oxa() && !theme.is_plain();
    if !arcade {
        return oxa_activity_line_with(st, keymap, animation == Animation::Minimal, width, theme, skin);
    }
    let cells = scene_cells(width);
    if matches!(st.activity, Activity::Idle) {
        let (how, ms) = run.finish?;
        if cells == 0 {
            return None;
        }
        let ascii = skin != Skin::Oxa;
        let art = if ascii { &ART_ASCII } else { &ART };
        let mut c = Canvas::new(cells);
        finish(&mut c, how, ms / SPRITE_FRAME_MS, art);
        let token = match how {
            Finish::Clear => "success",
            Finish::Fail => "error",
        };
        return Some(finish_line(
            c.paint(token, st.yolo, theme),
            how,
            run,
            width,
            theme,
            skin,
        ));
    }
    let lead = arcade_scene(scene, st, run, cells, theme, skin);
    activity_line_lead(st, keymap, lead, width, theme, skin)
}

/// The activity line whose scene is the director's frame (`stage`, design 3.2): the same line as
/// [`oxa_arcade_line`] while a run is active, with the scene the director picked for the
/// activity. The caller has already told the director the activity (`Director::update`) at
/// `now_ms`. Waiting for the user and idle show the static badge, as before.
pub fn oxa_stage_line(
    stage: &Director,
    st: &StatusModel,
    keymap: &Keymap,
    now_ms: u64,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Option<String> {
    let lead = match activity_style(&st.activity) {
        Some((_, token)) if !matches!(st.activity, Activity::Blocked) => {
            stage.row(now_ms, token, st.yolo, theme, skin != Skin::Oxa)
        }
        _ => None,
    };
    activity_line_lead(st, keymap, lead, width, theme, skin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_with_the_arcade_as_default() {
        assert_eq!(Animation::from_setting(None), Animation::Arcade);
        assert_eq!(Animation::from_setting(Some("arcade")), Animation::Arcade);
        assert_eq!(Animation::from_setting(Some(" minimal ")), Animation::Minimal);
        assert_eq!(Animation::from_setting(Some("off")), Animation::Off);
        assert_eq!(ArcadeScene::from_setting(Some("stars")), ArcadeScene::Stars);
        assert_eq!(ArcadeScene::from_setting(Some("charge")), ArcadeScene::Charge);
        assert_eq!(ArcadeScene::from_setting(None), ArcadeScene::Stack);
        assert_eq!(ArcadeScene::from_setting(Some("graphics")), ArcadeScene::Stack);
        assert_eq!(ArcadeScene::default(), ArcadeScene::Stack);
    }

    #[test]
    fn tool_icons_by_kind() {
        assert_eq!(tool_icon("bash", false), '$');
        assert_eq!(tool_icon("read", false), '≡');
        assert_eq!(tool_icon("read", true), '=');
        assert_eq!(tool_icon("edit", false), '±');
        assert_eq!(tool_icon("write", false), '+');
        assert_eq!(tool_icon("grep", false), '?');
        assert_eq!(tool_icon("web_search", false), '@');
        assert_eq!(tool_icon("subagent", false), '&');
        assert_eq!(tool_icon("srv__thing", false), '%');
        assert_eq!(tool_icon("todo", false), '*');
        for name in ["bash", "read", "edit", "write", "grep", "web", "task", "mcp", "x"] {
            assert!(tool_icon(name, true).is_ascii());
        }
    }

    fn tool_block(id: &str) -> Block {
        Block::Tool(crate::ToolBlock {
            call_id: id.to_string(),
            name: "read".to_string(),
            args: oxa_proto::Value::Null,
            args_raw: String::new(),
            state: crate::ToolState::Done,
            partial: String::new(),
            result: None,
            duration_ms: None,
            expanded: false,
        })
    }

    fn user_block() -> Block {
        Block::User {
            text: "go".to_string(),
            images: Vec::new(),
        }
    }

    #[test]
    fn state_counts_tools_and_ends_with_a_closing_scene() {
        let mut s = ArcadeState::default();
        // An earlier run's calls are not this run's.
        let mut blocks = vec![user_block(), tool_block("old"), user_block()];
        s.observe(true, &blocks, 0);
        assert_eq!(s.run(0), ArcadeRun::default());
        blocks.push(tool_block("a"));
        s.observe(true, &blocks, 10);
        s.observe(true, &blocks, 20);
        assert_eq!(s.run(20).coins, 1, "one call, however often it is drawn");
        blocks.extend([tool_block("b"), tool_block("c")]);
        s.observe(true, &blocks, 60);
        assert_eq!(s.run(60).coins, 3);
        s.observe(false, &blocks, 1000);
        assert_eq!(s.run(1100).coins, 3);
        assert_eq!(s.run(1100).finish, Some((Finish::Clear, 100)));
        assert!(s.finishing(1100) && !s.take_expired(1100));
        assert!(!s.finishing(1000 + FINISH_MS));
        assert!(s.take_expired(1000 + FINISH_MS) && !s.take_expired(1000 + FINISH_MS));
        // The next run starts from nothing.
        blocks.push(user_block());
        s.observe(true, &blocks, 5000);
        assert_eq!(s.run(5000), ArcadeRun::default());
    }

    #[test]
    fn how_a_run_ended() {
        let shown = |why: &str| Block::Shown {
            text: String::new(),
            why: why.to_string(),
        };
        assert_eq!(finish_of(None), Some(Finish::Clear));
        assert_eq!(finish_of(Some(&shown(oxa_proto::shown_why::ABORTED))), None);
        assert_eq!(
            finish_of(Some(&shown(oxa_proto::shown_why::FAILED))),
            Some(Finish::Fail)
        );
        assert_eq!(
            finish_of(Some(&shown(oxa_proto::shown_why::CUT))),
            Some(Finish::Fail)
        );
        let mut s = ArcadeState::default();
        s.observe(true, &[], 0);
        s.observe(false, &[shown(oxa_proto::shown_why::ABORTED)], 9);
        assert!(!s.finishing(10), "an abort has no closing scene");
    }
}
