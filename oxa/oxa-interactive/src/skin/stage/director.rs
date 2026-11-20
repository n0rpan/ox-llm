//! The director (animations design 3.2 and 3.3, slice 4): it picks the scene of the activity row,
//! keeps the picks, and says when the row must be drawn next. Nothing here reads a clock or a
//! terminal: the caller passes `now` (milliseconds) with every call.
//!
//! Rules:
//! * Idle: no scene, no deadline, no redraw. Waiting for the user: one still frame (no scene).
//! * A slow scene (an activity key starting with `wait`) steps once a second.
//! * After [`SETTLE_MS`] of one unchanged activity, any scene steps once a second.
//! * A pinned scene plays whatever the activity says and bypasses the picks.

use super::data::{Frame, Looping, Scene, Tier};
use super::pick::{first_with_members, Bag, Sticky};
use super::render::{frame_index, render_frame, LoopMode, RowFrame};
use crate::skin::tool_icon;
use crate::Activity;
use oxa_tui::Theme;
use std::collections::HashMap;

/// One activity may run this long before its scene settles to one step a second (design 3.3).
pub const SETTLE_MS: u64 = 20_000;

/// The step of a slow scene and of a settled one.
pub const SLOW_STEP_MS: u64 = 1_000;

/// A pick is kept for this long for one (run, key) (design 3.2, step 4).
pub const STICKY_MS: u64 = 60_000;

/// What the activity asks of the stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Nothing is going on: no scene.
    Idle,
    /// A dialog waits for the user: one still frame, no scene and no timer.
    Waiting,
    /// Working on an event key. `icon` is the tool's icon (the `I` ink of a scene).
    Working { key: String, icon: char },
}

/// The stage's view of an activity: the event key and the tool icon (design 3.2, step 1).
/// `ascii` picks the glyphs of the tool icon, as the arcade does.
pub fn pending_of(activity: &Activity, ascii: bool) -> Pending {
    let working = |key: &str, icon: char| Pending::Working {
        key: key.to_string(),
        icon,
    };
    match activity {
        Activity::Idle => Pending::Idle,
        Activity::Blocked => Pending::Waiting,
        Activity::Tool(name) => {
            let lower = name.to_ascii_lowercase();
            let shell = ["bash", "shell", "exec"].iter().any(|w| lower.contains(w));
            working(if shell { "tool:bash" } else { "tool" }, tool_icon(name, ascii))
        }
        Activity::Streaming => working("write", '*'),
        Activity::Queued { .. } | Activity::Retrying { .. } => working("wait", '*'),
        Activity::Compacting => working("compact", '*'),
        Activity::Prefill { .. } | Activity::Thinking => working("think", '*'),
    }
}

/// What the director remembers of the current activity.
#[derive(Debug, Clone)]
struct Current {
    pending: Pending,
    /// The run: a new run starts when the activity leaves idle.
    run: u64,
    /// The scene picked for it; `None` while waiting or when no scene fits.
    scene: Option<usize>,
    icon: char,
    /// When the scene started (its frame 0).
    scene_since: u64,
    /// When this activity started (the settle clock).
    activity_since: u64,
}

/// `(scene, frame)` and the activity start, as last drawn: a change means a redraw.
type Drawn = (Option<(usize, usize)>, Option<u64>);

/// The scene director of one session.
#[derive(Debug)]
pub struct Director {
    session: String,
    scenes: Vec<Scene>,
    bags: HashMap<String, Bag<usize>>,
    sticky: Sticky<Option<(usize, u64)>>,
    pinned: Option<usize>,
    run: u64,
    cells: usize,
    current: Option<Current>,
    drawn: Drawn,
}

impl Director {
    /// A director over `scenes` for `session`. Nothing is shown until [`Director::update`].
    pub fn new(session: &str, scenes: Vec<Scene>) -> Director {
        Director {
            session: session.to_string(),
            scenes,
            bags: HashMap::new(),
            sticky: Sticky::default(),
            pinned: None,
            run: 0,
            cells: 0,
            current: None,
            drawn: (None, None),
        }
    }

    /// Starts the picks of another session (new bags, no kept picks). The same session is a no-op.
    pub fn reseed(&mut self, session: &str) {
        if self.session != session {
            self.session = session.to_string();
            self.bags.clear();
            self.sticky = Sticky::default();
        }
    }

    /// Plays the scene with this id whatever the activity says. `None` releases the pin; an
    /// unknown id pins nothing.
    pub fn pin(&mut self, id: Option<&str>) {
        self.pinned = id.and_then(|id| self.scenes.iter().position(|s| s.id == id));
    }

    /// Tells the director what the activity is now, in a row of `cells` cells, at `now`.
    /// A change of activity restarts the settle clock; a change of event key picks a scene.
    pub fn update(&mut self, pending: Pending, cells: usize, now: u64) {
        self.cells = cells;
        if pending == Pending::Idle {
            self.current = None;
            return;
        }
        if self.current.as_ref().is_some_and(|c| c.pending == pending) {
            return;
        }
        let was = self.current.take();
        let run = match &was {
            Some(c) => c.run,
            None => {
                self.run += 1;
                self.run
            }
        };
        let (scene, icon) = match &pending {
            Pending::Working { key, icon } => (self.pick(key, run, now), *icon),
            _ => (None, '*'),
        };
        let scene_since = match &was {
            Some(c) if scene.is_some() && c.scene == scene => c.scene_since,
            _ => now,
        };
        self.current = Some(Current {
            pending,
            run,
            scene,
            icon,
            scene_since,
            activity_since: now,
        });
    }

    /// The scene id and frame index shown at `now`; `None` when no scene shows.
    pub fn shown(&self, now: u64) -> Option<(&str, usize)> {
        let (scene, frame) = self.slot(now)?;
        Some((self.scenes.get(scene)?.id.as_str(), frame))
    }

    /// `true` when the row changed since the last call (a new frame, a new activity, or the
    /// scene taken away). The loop uses it where it used to mark the screen dirty.
    pub fn redraw_due(&mut self, now: u64) -> bool {
        let key: Drawn = (self.slot(now), self.current.as_ref().map(|c| c.activity_since));
        if key == self.drawn {
            false
        } else {
            self.drawn = key;
            true
        }
    }

    /// When the row must be drawn next (ms), or `None` when nothing changes without input: idle,
    /// waiting for the user, a finished scene, a held last frame.
    pub fn next_deadline_ms(&self, now: u64) -> Option<u64> {
        let c = self.current.as_ref()?;
        let scene_idx = c.scene?;
        let scene = self.scenes.get(scene_idx)?;
        let len = self.frames(scene_idx)?.len();
        let mode = loop_mode(scene.looping);
        let elapsed = now.saturating_sub(c.scene_since);
        let step = Self::step_of(c, scene, now);
        let index = frame_index(len, elapsed, step, mode)?;
        if mode == LoopMode::Hold && index + 1 == len {
            return None;
        }
        let mut next = c.scene_since + (elapsed / step + 1) * step;
        let settle_at = c.activity_since + SETTLE_MS;
        if settle_at > now && settle_at < next {
            next = settle_at;
        }
        Some(next)
    }

    /// The row of the shown frame, styled like the built-in scenes (see `render_frame`). `hero`
    /// is the activity's theme token, `hot` the YOLO burn, `ascii` the ASCII skin's frames.
    /// `None` when nothing shows.
    pub fn row(&self, now: u64, hero: &str, hot: bool, theme: &Theme, ascii: bool) -> Option<String> {
        let (scene_idx, frame_idx) = self.slot(now)?;
        let scene = self.scenes.get(scene_idx)?;
        let frames = if ascii {
            scene.ascii.as_ref()?.get(&self.cells)?
        } else {
            scene.sizes.get(&self.cells)?
        };
        let frame: &Frame = frames.get(frame_idx)?;
        let icon = self.current.as_ref().map_or('*', |c| c.icon);
        render_frame(
            RowFrame {
                text: &frame.text,
                inks: &frame.inks,
            },
            icon,
            hero,
            hot,
            theme,
        )
    }

    /// The scene and frame index at `now`, from the scene's own clock and step.
    fn slot(&self, now: u64) -> Option<(usize, usize)> {
        let c = self.current.as_ref()?;
        let scene_idx = c.scene?;
        let scene = self.scenes.get(scene_idx)?;
        let len = self.frames(scene_idx)?.len();
        let step = Self::step_of(c, scene, now);
        let elapsed = now.saturating_sub(c.scene_since);
        frame_index(len, elapsed, step, loop_mode(scene.looping)).map(|f| (scene_idx, f))
    }

    /// The frames of a scene at the stage's width (the size key), if it has any.
    fn frames(&self, scene: usize) -> Option<&[Frame]> {
        self.scenes.get(scene)?.sizes.get(&self.cells).map(Vec::as_slice)
    }

    /// The step of the scene now: its own, or one second when slow or settled.
    fn step_of(c: &Current, scene: &Scene, now: u64) -> u64 {
        let slow = matches!(&c.pending, Pending::Working { key, .. } if key.starts_with("wait"));
        let settled = now.saturating_sub(c.activity_since) >= SETTLE_MS;
        if slow || settled {
            SLOW_STEP_MS
        } else {
            u64::from(scene.step_ms).max(1)
        }
    }

    /// The scene for `key` in `run`: the kept pick, or a fresh one once it has been kept for
    /// [`STICKY_MS`].
    fn pick(&mut self, key: &str, run: u64, now: u64) -> Option<usize> {
        let pinned = self.pinned;
        let scenes = &self.scenes;
        let session = self.session.as_str();
        let bags = &mut self.bags;
        let kept = self
            .sticky
            .hold(run, key, false, || draw(scenes, bags, session, pinned, key, now));
        let spent = matches!(kept, Some((_, at)) if now.saturating_sub(at) >= STICKY_MS);
        let picked = if spent {
            self.sticky
                .hold(run, key, true, || draw(scenes, bags, session, pinned, key, now))
        } else {
            kept
        };
        picked.map(|(index, _)| index)
    }
}

/// A fresh pick for `key`: the pin, or the next scene of the key's shuffle bag (with the key
/// fallbacks of design 3.2). The pick is stamped with `now`.
fn draw(
    scenes: &[Scene],
    bags: &mut HashMap<String, Bag<usize>>,
    session: &str,
    pinned: Option<usize>,
    key: &str,
    now: u64,
) -> Option<(usize, u64)> {
    if let Some(index) = pinned {
        return Some((index, now));
    }
    let chosen = first_with_members(key, |k| has_members(scenes, k))?;
    let members: Vec<(usize, u32)> = scenes
        .iter()
        .enumerate()
        .filter(|(_, s)| s.tier == Tier::Run && s.on.contains(&chosen))
        .map(|(i, s)| (i, s.weight))
        .collect();
    let bag = bags
        .entry(chosen.clone())
        .or_insert_with(|| Bag::new(session, &chosen));
    bag.next(&members).map(|index| (index, now))
}

fn has_members(scenes: &[Scene], key: &str) -> bool {
    scenes
        .iter()
        .any(|s| s.tier == Tier::Run && s.on.iter().any(|o| o == key))
}

fn loop_mode(looping: Looping) -> LoopMode {
    match looping {
        Looping::Loop => LoopMode::Loop,
        Looping::Once => LoopMode::Once,
        Looping::Hold => LoopMode::Hold,
    }
}
