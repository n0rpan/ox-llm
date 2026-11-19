//! The stage: data-driven scenes for the activity line (design `handoff/animations/design.md`).
//!
//! Slices 1 and 3 together: scenes as JSON, loaded and validated at start, with the built-in pack
//! compiled in, and the renderer that draws them. Nothing here reads a clock or touches the
//! terminal directly.

pub mod data;
pub mod render;

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
