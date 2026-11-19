//! The stage: the scene director of the activity row (animations design, sections 3.2 and 5).
//!
//! Rules for everything in this module:
//! * Pure functions of their arguments (no process environment, no clock, no I/O).
//! * Randomness comes only from a seed the caller passes in, so a session replays the same picks.

pub mod context;
pub mod pick;
pub mod render;
