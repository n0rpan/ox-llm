//! The stage: the scene director of the activity row (animations design, sections 3.2 and 5).
//!
//! Rules for everything in this module:
//! * Pure functions of their arguments (no process environment, no clock, no I/O), except `probe`,
//!   which reads the session's facts once through an injected `ProbeFs` and never in the draw path.
//! * Randomness comes only from a seed the caller passes in, so a session replays the same picks.

pub mod context;
pub mod pick;
pub mod probe;
pub mod render;

pub mod data;

pub use data::{
    ascii_text, glyph_allowed, load_pack, Frame, Intensity, Loaded, Looping, Scene, Skip, Tier, CAPTION_LONG,
    CAPTION_SHORT, INKS, SIZES, STEP_MS, WIN_MAX_MS,
};

/// The built-in pack. Compiled in: no file and no network at start.
const BASE_PACK: &str = include_str!("pack/base.json");

/// The built-in scenes. `built_in_pack_passes_validation` keeps this pack loadable, so the
/// empty fallback is never reached in a released build.
pub fn built_in() -> Loaded {
    load_pack(BASE_PACK).unwrap_or_default()
}
