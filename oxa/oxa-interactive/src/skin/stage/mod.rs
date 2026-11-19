//! The stage: the picture beside the activity line (design `handoff/animations/design.md`).
//!
//! Slice 1 holds the data side only: scenes as JSON, loaded and validated at start, with the
//! built-in pack compiled in. Nothing here draws, reads a clock or touches the terminal.

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
