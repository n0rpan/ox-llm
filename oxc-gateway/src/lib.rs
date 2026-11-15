//! Opt-in, engine-independent cloud gateway building blocks.
//! No daemon or installer is enabled by default.
#![forbid(unsafe_code)]
#[cfg(feature = "policy")]
pub mod policy;
